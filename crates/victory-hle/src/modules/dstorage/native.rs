//! Desktop DirectStorage 1.3 file reads to host memory. The Xbox queue owns
//! request ordering, destination mappings and its zlib/BCPack decoding.

use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{NonNull, null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

use super::{Guid, HRESULT, Out};

const FACTORY: Guid = Guid::new(
    0x6924_ea0c,
    0xc3cd,
    0x4826,
    [0xb1, 0x0a, 0xf6, 0x4f, 0x4e, 0xd9, 0x27, 0xc1],
);
const FILE: Guid = Guid::new(
    0x5de9_5e7b,
    0x955a,
    0x4868,
    [0xa7, 0x3c, 0x24, 0x3b, 0x29, 0xf4, 0xb8, 0xda],
);
const STATUS: Guid = Guid::new(
    0x8239_7587,
    0x7cd5,
    0x453b,
    [0xa0, 0x2e, 0x31, 0x37, 0x9b, 0xd6, 0x46, 0x56],
);
const QUEUE1: Guid = Guid::new(
    0xdd2f_482c,
    0x5eff,
    0x41e8,
    [0x9c, 0x9e, 0xd2, 0x37, 0x4b, 0x27, 0x81, 0x28],
);

// ABI from the Windows DirectStorage SDK's dstorage.h. Only file-to-memory
// requests are translated here. Options and unused union bytes stay zero.
#[repr(C)]
struct ReadRequest {
    options: [u64; 2],
    source: *mut c_void,
    offset: u64,
    source_size: u32,
    source_padding: u32,
    destination: *mut u8,
    destination_size: u32,
    destination_padding: u32,
    destination_union: [u64; 3],
    uncompressed_size: u32,
    padding: u32,
    cancellation_tag: u64,
    name: *const c_char,
}

#[repr(C)]
struct QueueDesc {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    device: *mut c_void,
}

const _: () = {
    assert!(size_of::<ReadRequest>() == 104);
    assert!(size_of::<QueueDesc>() == 32);
};

#[repr(C)]
struct Unknown {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct Factory {
    base: Unknown,
    create_queue:
        unsafe extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT,
    open_file: unsafe extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT,
    create_status:
        unsafe extern "system" fn(*mut c_void, u32, *const c_char, *const Guid, Out) -> HRESULT,
}

#[repr(C)]
struct Status {
    base: Unknown,
    is_complete: usize,
    result: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}

#[repr(C)]
struct Queue {
    base: Unknown,
    enqueue_request: unsafe extern "system" fn(*mut c_void, *const ReadRequest),
    enqueue_status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32),
    enqueue_signal: usize,
    submit: unsafe extern "system" fn(*mut c_void),
    cancel_requests: usize,
    close: unsafe extern "system" fn(*mut c_void),
    error_event: usize,
    retrieve_error_record: usize,
    query: usize,
    enqueue_event: unsafe extern "system" fn(*mut c_void, HANDLE),
}

struct Com<T>(NonNull<*const T>);

impl<T> Com<T> {
    /// The SDK returns an owning COM reference for the requested interface.
    fn get(call: impl FnOnce(Out) -> HRESULT) -> Option<Self> {
        let mut out = null_mut();
        if call(&mut out) < 0 {
            return None;
        }
        NonNull::new(out.cast()).map(Self)
    }

    fn raw(&self) -> *mut c_void {
        self.0.as_ptr().cast()
    }

    fn vtable(&self) -> &T {
        // SAFETY: a live instance of the matching interface owns its vtable.
        unsafe { &**self.0.as_ptr() }
    }
}

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        // SAFETY: each SDK interface starts with IUnknown.
        let unknown = unsafe { &*(*self.0.as_ptr()).cast::<Unknown>() };
        unsafe { (unknown.release)(self.raw()) };
    }
}

struct Module(HMODULE);

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: the successful load belongs to this owner.
        unsafe { FreeLibrary(self.0) };
    }
}

struct Runtime {
    factory: Com<Factory>,
    _module: Module,
}

// SAFETY: DirectStorage exposes a process-wide factory. Its calls and the
// loader reference are independent of the caller's thread.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            if std::env::var_os("VICTORY_NATIVE_DIRECTSTORAGE").as_deref()
                != Some(std::ffi::OsStr::new("1"))
            {
                return None;
            }
            let runtime = Runtime::load();
            if runtime.is_some() {
                tracing::info!("dstorage: Windows DirectStorage runtime enabled");
            } else {
                tracing::warn!(
                    "dstorage: Windows DirectStorage unavailable, using Win32 file reads"
                );
            }
            runtime
        })
        .as_ref()
}

impl Runtime {
    fn load() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let dll = exe.with_file_name("dstorage.dll");
        let path: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a terminated absolute path. Dependencies are searched in
        // the executable's directory and System32, away from the game data.
        let handle = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if handle.is_null() {
            return None;
        }
        let module = Module(handle);
        // SAFETY: the loaded SDK export takes a GUID and COM output pointer.
        let symbol = unsafe { GetProcAddress(handle, c"DStorageGetFactory".as_ptr().cast()) }?;
        let get_factory: unsafe extern "system" fn(*const Guid, Out) -> HRESULT =
            unsafe { std::mem::transmute(symbol) };
        let factory = Com::get(|out| unsafe { get_factory(&FACTORY, out) })?;
        Some(Self {
            factory,
            _module: module,
        })
    }
}

pub(super) struct File(Com<Unknown>);

// SAFETY: the native file holds a reference to shared read-only file state.
// Each request passes the file to a worker's own queue with an explicit offset.
unsafe impl Send for File {}
unsafe impl Sync for File {}

pub(super) fn open(file: &std::fs::File) -> Option<File> {
    let runtime = runtime()?;
    let mut path = vec![0u16; 260];
    loop {
        // SAFETY: this is the already-open Win32 file. Its final Windows
        // path accounts for all title drive and case resolution.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                0,
            )
        } as usize;
        if len == 0 {
            return None;
        }
        if len < path.len() {
            break;
        }
        path.resize(len, 0);
    }
    let factory = &runtime.factory;
    // SAFETY: terminated native path and the documented file interface IID.
    Com::get(|out| unsafe {
        (factory.vtable().open_file)(factory.raw(), path.as_ptr(), &FILE, out)
    })
    .map(File)
}

thread_local! {
    // Reuse one status entry, event and native queue per existing I/O worker.
    static WORKER: RefCell<Option<Worker>> = RefCell::new(runtime().and_then(Worker::new));
}

impl File {
    /// True only once the native queue has filled all of `out`. A false
    /// result leaves the caller free to retry the same range through Win32.
    pub(super) fn read(&self, offset: u64, out: &mut [u8]) -> bool {
        if out.is_empty() {
            return true;
        }
        // Larger requests can exceed the desktop runtime's default staging
        // size. They use the existing read path from the start.
        if out.len() > 32 << 20 {
            return false;
        }
        WORKER.with(|worker| {
            worker
                .borrow_mut()
                .as_mut()
                .is_some_and(|w| w.read(self, offset, out))
        })
    }
}

struct Worker {
    queue: Com<Queue>,
    status: Com<Status>,
    event: OwnedHandle,
    closed: bool,
}

impl Worker {
    fn new(runtime: &Runtime) -> Option<Self> {
        let factory = &runtime.factory;
        // SAFETY: the status instance and event are private to the worker.
        let status = Com::get(|out| unsafe {
            (factory.vtable().create_status)(factory.raw(), 1, null(), &STATUS, out)
        })?;
        let event = unsafe { CreateEventW(null(), 0, 0, null()) };
        if event.is_null() {
            return None;
        }
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        let desc = QueueDesc {
            source_type: 0, // DSTORAGE_REQUEST_SOURCE_FILE
            capacity: 0x80,
            priority: 0, // DSTORAGE_PRIORITY_NORMAL
            name: c"VIctory file reads".as_ptr(),
            device: null_mut(),
        };
        let queue = Com::get(|out| unsafe {
            (factory.vtable().create_queue)(factory.raw(), &desc, &QUEUE1, out)
        })?;
        Some(Self {
            queue,
            status,
            event,
            closed: false,
        })
    }

    fn read(&mut self, file: &File, offset: u64, out: &mut [u8]) -> bool {
        if self.closed {
            return false;
        }
        let size = out.len() as u32;
        let request = ReadRequest {
            options: [0; 2], // uncompressed file source and memory destination
            source: file.0.raw(),
            offset,
            source_size: size,
            source_padding: 0,
            destination: out.as_mut_ptr(),
            destination_size: size,
            destination_padding: 0,
            destination_union: [0; 3],
            uncompressed_size: 0,
            padding: 0,
            cancellation_tag: 0,
            name: null(),
        };
        let queue = self.queue.raw();
        let vtable = self.queue.vtable();
        let event = self.event.as_raw_handle();
        // SAFETY: each output and source stays live until the event (after
        // the request and status), or Close ensures no requests can still
        // complete. The event is auto-reset and only this worker waits on it.
        let result = unsafe {
            (vtable.enqueue_request)(queue, &request);
            (vtable.enqueue_status)(queue, self.status.raw(), 0);
            (vtable.enqueue_event)(queue, event);
            (vtable.submit)(queue);
            if WaitForSingleObject(event, INFINITE) != WAIT_OBJECT_0 {
                self.close();
                return false;
            }
            (self.status.vtable().result)(self.status.raw(), 0)
        };
        if result < 0 {
            static FALLBACKS: AtomicU64 = AtomicU64::new(0);
            let requests = FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1;
            if requests.is_power_of_two() {
                tracing::debug!(
                    requests,
                    hr = format_args!("{:#010x}", result as u32),
                    "dstorage: native file request retried through Win32"
                );
            }
            return false;
        }
        static FIRST_READ: std::sync::Once = std::sync::Once::new();
        FIRST_READ.call_once(|| {
            tracing::info!("dstorage: first Windows DirectStorage file read completed")
        });
        true
    }

    fn close(&mut self) {
        if !self.closed {
            // SAFETY: this queue belongs to the current worker.
            unsafe { (self.queue.vtable().close)(self.queue.raw()) };
            self.closed = true;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires the Microsoft DirectStorage DLLs beside the test executable and VICTORY_NATIVE_DIRECTSTORAGE=1"]
    fn native_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.dat");
        let data: Vec<u8> = (0..131_072).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let native = super::open(&file).expect("open file through Windows DirectStorage");
        std::thread::scope(|scope| {
            for start in [3, 65_537] {
                let (native, data) = (&native, &data);
                scope.spawn(move || {
                    for offset in [start, start + 19] {
                        let mut out = [0u8; 4096];
                        assert!(native.read(offset as u64, &mut out));
                        assert_eq!(out, data[offset..offset + out.len()]);
                    }
                });
            }
        });
    }
}
