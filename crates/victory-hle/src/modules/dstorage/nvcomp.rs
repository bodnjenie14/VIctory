//! Optional NVIDIA nvCOMP 5.3 CUDA backend for Xbox zlib file requests.
//! NVIDIA's raw DEFLATE API requires valid input. The first read always
//! uses the bounded CPU decoder. Only identical, fully consumed streams
//! with exactly the same output size are eligible for the GPU thereafter.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{FARPROC, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use super::{HRESULT, inflate, inflate_with_consumption};

const MIN_CHUNK: usize = 64 << 10;
const MAX_CHUNK: usize = 256 << 10;
const MAX_CACHE_BYTES: usize = 16 << 20;
const MAX_CACHE_ENTRIES: usize = 256;
const HOST_TO_DEVICE: i32 = 1;
const DEVICE_TO_HOST: i32 = 2;

static GPU_DECODED: AtomicU64 = AtomicU64::new(0);
static CPU_FALLBACK: AtomicU64 = AtomicU64::new(0);

pub(super) fn inflate_file(source: &[u8], out: &mut [u8]) -> Result<usize, HRESULT> {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("VICTORY_NVCOMP_ZLIB").is_some_and(|v| v == "1")) {
        return inflate(source, out);
    }
    let Some(chunk) = Chunk::new(source, out.len()) else {
        return inflate(source, out);
    };
    let Some(backend) = backend() else {
        return inflate(source, out);
    };
    if backend.decode_verified(source, chunk, out) {
        record(&GPU_DECODED);
        return Ok(out.len());
    }

    record(&CPU_FALLBACK);
    let (consumed, written) = inflate_with_consumption(source, out)?;
    if consumed == source.len() && written == out.len() {
        backend
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source, written);
    }
    Ok(written)
}

#[cfg(test)]
pub(super) fn gpu_decodes() -> u64 {
    GPU_DECODED.load(Ordering::Relaxed)
}

fn record(counter: &AtomicU64) {
    if (counter.fetch_add(1, Ordering::Relaxed) + 1).is_power_of_two() {
        tracing::info!(
            gpu = GPU_DECODED.load(Ordering::Relaxed),
            cpu = CPU_FALLBACK.load(Ordering::Relaxed),
            "dstorage: experimental nvCOMP zlib chunks",
        );
    }
}

struct Chunk<'a> {
    deflate: &'a [u8],
    adler: u32,
}

impl<'a> Chunk<'a> {
    fn new(input: &'a [u8], output_size: usize) -> Option<Self> {
        if !(7..=MAX_CHUNK).contains(&input.len())
            || !(MIN_CHUNK..=MAX_CHUNK).contains(&output_size)
        {
            return None;
        }
        let [cmf, flg] = input[..2].try_into().ok()?;
        if cmf & 0xf != 8
            || cmf >> 4 > 7
            || flg & 0x20 != 0
            || !u16::from_be_bytes([cmf, flg]).is_multiple_of(31)
        {
            return None;
        }
        let (body, trailer) = input.split_at(input.len() - 4);
        Some(Self {
            deflate: &body[2..],
            adler: u32::from_be_bytes(trailer.try_into().ok()?),
        })
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Box<[u8]>, usize>,
    bytes: usize,
}

impl Cache {
    fn contains(&self, source: &[u8], output: usize) -> bool {
        self.entries.get(source) == Some(&output)
    }

    fn insert(&mut self, source: &[u8], output: usize) {
        if self.entries.contains_key(source) {
            return;
        }
        if self.entries.len() == MAX_CACHE_ENTRIES || self.bytes + source.len() > MAX_CACHE_BYTES {
            *self = Self::default();
        }
        self.entries.insert(source.into(), output);
        self.bytes += source.len();
    }
}

struct Backend {
    cuda: Cuda,
    nvcomp: Nvcomp,
    cache: Mutex<Cache>,
    decoder: Mutex<Option<Decoder>>,
    _nvcomp_lib: Library,
    _cuda_lib: Library,
}

fn backend() -> Option<&'static Backend> {
    static BACKEND: OnceLock<Option<Backend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            let backend = Backend::load();
            if backend.is_some() {
                tracing::info!("dstorage: experimental nvCOMP CUDA zlib decoder enabled");
            } else {
                tracing::warn!(
                    "dstorage: nvCOMP 5.3 or CUDA 13 unavailable, using CPU zlib decoding"
                );
            }
            backend
        })
        .as_ref()
}

impl Backend {
    fn load() -> Option<Self> {
        let cuda_lib = Library::load("cudart64_13.dll")?;
        let nvcomp_lib = Library::load("nvcomp64_5.dll")?;
        let cuda = Cuda::load(&cuda_lib)?;
        let nvcomp = Nvcomp::load(&nvcomp_lib)?;
        let decoder = Decoder::new(&cuda, &nvcomp)?;
        Some(Self {
            cuda,
            nvcomp,
            cache: Mutex::default(),
            decoder: Mutex::new(Some(decoder)),
            _nvcomp_lib: nvcomp_lib,
            _cuda_lib: cuda_lib,
        })
    }

    fn decode_verified(&self, source: &[u8], chunk: Chunk<'_>, out: &mut [u8]) -> bool {
        // Do not queue behind a GPU decode: existing CPU workers can proceed.
        let Ok(mut decoder) = self.decoder.try_lock() else {
            return false;
        };
        let Some(active) = decoder.as_ref() else {
            return false;
        };
        if !self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(source, out.len())
        {
            return false;
        }
        if active
            .decode(&self.cuda, &self.nvcomp, chunk, out)
            .is_some()
        {
            return true;
        }
        tracing::warn!(
            "dstorage: nvCOMP decode failed validation or execution, using CPU zlib decoding"
        );
        *decoder = None;
        false
    }
}

struct Library(HMODULE);

impl Library {
    fn load(name: &str) -> Option<Self> {
        let path = std::env::current_exe().ok()?.with_file_name(name);
        let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: terminated absolute path; dependencies stay in trusted DLL directories.
        let handle = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        (!handle.is_null()).then(|| Self(handle))
    }

    fn symbol(&self, name: &CStr) -> FARPROC {
        // SAFETY: the module stays loaded for the lifetime of its exports.
        unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this reference was obtained by a successful LoadLibraryExW call.
        unsafe { FreeLibrary(self.0) };
    }
}

// SAFETY: a Windows module reference and its exports are process-wide.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

// The declarations below match NVIDIA CUDA 13 and nvCOMP 5.3.0 headers.
type Stream = *mut c_void;
type Memcpy = unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32) -> i32;
type Synchronize = unsafe extern "C" fn(Stream) -> i32;
type Free = unsafe extern "C" fn(*mut c_void) -> i32;
type SetDevice = unsafe extern "C" fn(i32) -> i32;

struct Cuda {
    get_device: unsafe extern "C" fn(*mut i32) -> i32,
    set_device: SetDevice,
    malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> i32,
    free: Free,
    memcpy: Memcpy,
    stream_create: unsafe extern "C" fn(*mut Stream, u32) -> i32,
    stream_synchronize: Synchronize,
    stream_destroy: Synchronize,
}

macro_rules! symbol {
    ($lib:expr, $name:expr) => {
        // SAFETY: the declared function type matches this documented export.
        unsafe { std::mem::transmute($lib.symbol($name)?) }
    };
}

impl Cuda {
    fn load(lib: &Library) -> Option<Self> {
        Some(Self {
            get_device: symbol!(lib, c"cudaGetDevice"),
            set_device: symbol!(lib, c"cudaSetDevice"),
            malloc: symbol!(lib, c"cudaMalloc"),
            free: symbol!(lib, c"cudaFree"),
            memcpy: symbol!(lib, c"cudaMemcpy"),
            stream_create: symbol!(lib, c"cudaStreamCreateWithFlags"),
            stream_synchronize: symbol!(lib, c"cudaStreamSynchronize"),
            stream_destroy: symbol!(lib, c"cudaStreamDestroy"),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Options {
    backend: i32,
    sort_for_hardware: i32,
    reserved: [u8; 56],
}
const CUDA_OPTIONS: Options = Options {
    backend: 2,
    sort_for_hardware: 0,
    reserved: [0; 56],
};

#[repr(C)]
#[derive(Default)]
struct Alignment {
    input: usize,
    output: usize,
    temp: usize,
}

#[repr(C)]
#[derive(Default)]
struct Properties {
    version: u32,
    cudart_version: u32,
}

type Decode = unsafe extern "C" fn(
    *const *const c_void,
    *const usize,
    *const usize,
    *mut usize,
    usize,
    *mut c_void,
    usize,
    *const *mut c_void,
    Options,
    *mut i32,
    Stream,
) -> i32;

struct Nvcomp {
    alignments: unsafe extern "C" fn(Options, *mut Alignment) -> i32,
    scratch_size: unsafe extern "C" fn(usize, usize, Options, *mut usize, usize) -> i32,
    decode: Decode,
}

impl Nvcomp {
    fn load(lib: &Library) -> Option<Self> {
        let get_properties: unsafe extern "C" fn(*mut Properties) -> i32 =
            symbol!(lib, c"nvcompGetProperties");
        let mut properties = Properties::default();
        // SAFETY: valid output storage; pin the ABI to the tested SDK series.
        if unsafe { get_properties(&mut properties) } != 0
            || properties.version != 5300
            || !(13000..14000).contains(&properties.cudart_version)
        {
            return None;
        }
        Some(Self {
            alignments: symbol!(lib, c"nvcompBatchedDeflateDecompressGetRequiredAlignments"),
            scratch_size: symbol!(lib, c"nvcompBatchedDeflateDecompressGetTempSizeAsync"),
            decode: symbol!(lib, c"nvcompBatchedDeflateDecompressAsync"),
        })
    }
}

struct DeviceBuffer {
    pointer: NonNull<c_void>,
    free: Free,
}

impl DeviceBuffer {
    fn new(cuda: &Cuda, size: usize) -> Option<Self> {
        let mut pointer = null_mut();
        // SAFETY: one CUDA allocation on the selected device.
        success(unsafe { (cuda.malloc)(&mut pointer, size) })?;
        Some(Self {
            pointer: NonNull::new(pointer)?,
            free: cuda.free,
        })
    }

    fn raw(&self) -> *mut c_void {
        self.pointer.as_ptr()
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        // SAFETY: this owns a CUDA allocation on the selected device.
        unsafe { (self.free)(self.raw()) };
    }
}

#[repr(C)]
struct Batch {
    input: *const c_void,
    output: *mut c_void,
    compressed: usize,
    capacity: usize,
    written: usize,
    status: i32,
    reserved: u32,
}

struct Decoder {
    device: i32,
    set_device: SetDevice,
    input: DeviceBuffer,
    output: DeviceBuffer,
    batch: DeviceBuffer,
    scratch: DeviceBuffer,
    scratch_size: usize,
    stream: Stream,
    destroy: Synchronize,
}

// SAFETY: the backend's mutex owns all accesses. CUDA streams and device
// allocations are process-wide and the caller selects this device before use.
unsafe impl Send for Decoder {}

impl Decoder {
    fn new(cuda: &Cuda, nvcomp: &Nvcomp) -> Option<Self> {
        let mut device = 0;
        let mut align = Alignment::default();
        let mut scratch_size = 0;
        // SAFETY: SDK APIs write their results into the declared raw outputs.
        unsafe {
            success((cuda.get_device)(&mut device))?;
            success((cuda.set_device)(device))?;
            success((nvcomp.alignments)(CUDA_OPTIONS, &mut align))?;
            success((nvcomp.scratch_size)(
                1,
                MAX_CHUNK,
                CUDA_OPTIONS,
                &mut scratch_size,
                MAX_CHUNK,
            ))?;
        }
        let input = DeviceBuffer::new(cuda, MAX_CHUNK)?;
        let output = DeviceBuffer::new(cuda, MAX_CHUNK)?;
        let batch = DeviceBuffer::new(cuda, size_of::<Batch>())?;
        let scratch = DeviceBuffer::new(cuda, scratch_size.max(1))?;
        for (buffer, alignment) in [
            (&input, align.input),
            (&output, align.output),
            (&scratch, align.temp),
        ] {
            if alignment == 0 || !buffer.raw().addr().is_multiple_of(alignment) {
                return None;
            }
        }
        let mut stream = null_mut();
        // SAFETY: creates a private nonblocking stream on `device`.
        success(unsafe { (cuda.stream_create)(&mut stream, 1) })?;
        Some(Self {
            device,
            set_device: cuda.set_device,
            input,
            output,
            batch,
            scratch,
            scratch_size,
            stream,
            destroy: cuda.stream_destroy,
        })
    }

    fn decode(&self, cuda: &Cuda, nvcomp: &Nvcomp, chunk: Chunk<'_>, out: &mut [u8]) -> Option<()> {
        let mut batch = Batch {
            input: self.input.raw(),
            output: self.output.raw(),
            compressed: chunk.deflate.len(),
            capacity: out.len(),
            written: 0,
            status: -1,
            reserved: 0,
        };
        let device = self.batch.raw().cast::<Batch>();
        // SAFETY: the source previously passed bounded CPU validation byte for
        // byte. `device` holds one Batch, which supplies the singleton arrays.
        // We wait for all GPU operations before reading the results.
        unsafe {
            success((cuda.set_device)(self.device))?;
            success((cuda.memcpy)(
                self.input.raw(),
                chunk.deflate.as_ptr().cast(),
                chunk.deflate.len(),
                HOST_TO_DEVICE,
            ))?;
            success((cuda.memcpy)(
                self.batch.raw(),
                (&raw const batch).cast(),
                size_of::<Batch>(),
                HOST_TO_DEVICE,
            ))?;
            // Pageable host uploads can return once bytes are staged. Wait
            // for the default copy stream before the nonblocking decode.
            success((cuda.stream_synchronize)(null_mut()))?;
            success((nvcomp.decode)(
                &raw const (*device).input,
                &raw const (*device).compressed,
                &raw const (*device).capacity,
                &raw mut (*device).written,
                1,
                self.scratch.raw(),
                self.scratch_size,
                &raw const (*device).output,
                CUDA_OPTIONS,
                &raw mut (*device).status,
                self.stream,
            ))?;
            success((cuda.stream_synchronize)(self.stream))?;
            success((cuda.memcpy)(
                (&raw mut batch).cast(),
                self.batch.raw(),
                size_of::<Batch>(),
                DEVICE_TO_HOST,
            ))?;
            if batch.status != 0 || batch.written != out.len() {
                return None;
            }
            success((cuda.memcpy)(
                out.as_mut_ptr().cast(),
                self.output.raw(),
                out.len(),
                DEVICE_TO_HOST,
            ))?;
        }
        (miniz_oxide::mz_adler32_oxide(miniz_oxide::MZ_ADLER32_INIT, out) == chunk.adler)
            .then_some(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the decoder owns the stream, followed by its allocations.
        unsafe {
            (self.set_device)(self.device);
            (self.destroy)(self.stream);
        }
    }
}

fn success(code: i32) -> Option<()> {
    (code == 0).then_some(())
}
