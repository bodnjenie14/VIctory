//! Window-size controls owned by VIctory, in the title window's system menu.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, RECT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;
use windows_sys::core::{PCWSTR, w};

static SETTINGS: OnceLock<PathBuf> = OnceLock::new();
const SIZE_MENU: PCWSTR = w!("victory.window-size");

#[derive(Clone, Copy)]
struct Preset {
    command: usize,
    width: i32,
    height: i32,
    label: PCWSTR,
}

const PRESETS: [Preset; 3] = [
    Preset {
        command: 0xa010,
        width: 1280,
        height: 720,
        label: w!("1280 × 720"),
    },
    Preset {
        command: 0xa020,
        width: 1920,
        height: 1080,
        label: w!("1920 × 1080"),
    },
    Preset {
        command: 0xa030,
        width: 2560,
        height: 1440,
        label: w!("2560 × 1440"),
    },
];

/// Use a file under the title's VIctory data directory, outside its saves.
pub fn use_window_size_file(path: PathBuf) {
    let _ = SETTINGS.set(path);
}

pub(super) fn install(hwnd: HWND) {
    let Some(path) = SETTINGS.get() else {
        return;
    };
    if hwnd.is_null() || !attach_menu(hwnd) {
        return;
    }
    if let Some(preset) = load(path) {
        apply(hwnd, preset);
    }
}

fn attach_menu(hwnd: HWND) -> bool {
    // SAFETY: the hooked window was just created on this thread. Its system
    // menu owns the submenu after AppendMenuW succeeds.
    unsafe {
        let styles = wm::GetWindowLongW(hwnd, wm::GWL_STYLE) as u32;
        if styles & (wm::WS_CHILD | wm::WS_SYSMENU) != wm::WS_SYSMENU
            || !wm::GetWindow(hwnd, wm::GW_OWNER).is_null()
        {
            return false;
        }
        let menu = wm::GetSystemMenu(hwnd, 0);
        if menu.is_null() {
            return false;
        }
        let sizes = wm::CreatePopupMenu();
        if sizes.is_null() {
            return false;
        }
        if !PRESETS
            .iter()
            .all(|preset| wm::AppendMenuW(sizes, wm::MF_STRING, preset.command, preset.label) != 0)
            || wm::SetPropW(hwnd, SIZE_MENU, sizes) == 0
        {
            wm::DestroyMenu(sizes);
            return false;
        }
        if wm::AppendMenuW(
            menu,
            wm::MF_POPUP,
            sizes as usize,
            w!("VIctory window size"),
        ) == 0
        {
            wm::RemovePropW(hwnd, SIZE_MENU);
            wm::DestroyMenu(sizes);
            return false;
        }
    }
    true
}

pub(super) fn handle_message(hwnd: HWND, message: u32, wparam: WPARAM) -> bool {
    if !matches!(
        message,
        wm::WM_SYSCOMMAND | wm::WM_INITMENU | wm::WM_NCDESTROY
    ) {
        return false;
    }
    // SAFETY: a property of the window receiving this message, set in attach_menu.
    let menu = unsafe { wm::GetPropW(hwnd, SIZE_MENU) };
    if menu.is_null() {
        return false;
    }
    if message == wm::WM_NCDESTROY {
        // SAFETY: removes this window's property. Its system menu owns the submenu.
        unsafe { wm::RemovePropW(hwnd, SIZE_MENU) };
        return false;
    }
    if message == wm::WM_INITMENU {
        let current = client_size(hwnd);
        for preset in PRESETS {
            let state = if current == Some((preset.width, preset.height)) {
                wm::MF_CHECKED
            } else {
                wm::MF_UNCHECKED
            };
            // SAFETY: the attached menu is valid while its window receives messages.
            unsafe { wm::CheckMenuItem(menu, preset.command as u32, wm::MF_BYCOMMAND | state) };
        }
        return false;
    }
    let Some(preset) = PRESETS
        .iter()
        .find(|preset| preset.command == wparam & 0xfff0)
    else {
        return false;
    };
    if apply(hwnd, *preset)
        && let Some(path) = SETTINGS.get()
        && let Err(error) = save(path, *preset)
    {
        tracing::warn!(%error, "could not save VIctory window size");
    }
    true
}

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: GetClientRect writes to this local rectangle.
    (unsafe { wm::GetClientRect(hwnd, &raw mut rect) } != 0).then_some((rect.right, rect.bottom))
}

fn apply(hwnd: HWND, preset: Preset) -> bool {
    let mut rect = RECT {
        right: preset.width,
        bottom: preset.height,
        ..Default::default()
    };
    // SAFETY: the window procedure owns this window on its UI thread. Only
    // its outer size is changed to accommodate the requested client area.
    let resized = unsafe {
        if wm::IsZoomed(hwnd) != 0 || wm::IsIconic(hwnd) != 0 {
            wm::ShowWindow(hwnd, wm::SW_RESTORE);
        }
        let style = wm::GetWindowLongW(hwnd, wm::GWL_STYLE) as u32;
        let ex_style = wm::GetWindowLongW(hwnd, wm::GWL_EXSTYLE) as u32;
        let has_menu = !wm::GetMenu(hwnd).is_null();
        AdjustWindowRectExForDpi(
            &raw mut rect,
            style,
            has_menu.into(),
            ex_style,
            GetDpiForWindow(hwnd),
        ) != 0
            && wm::SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                wm::SWP_NOMOVE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
            ) != 0
    };
    let actual = client_size(hwnd);
    if resized && actual == Some((preset.width, preset.height)) {
        tracing::info!(
            width = preset.width,
            height = preset.height,
            "VIctory window resized"
        );
        return true;
    }
    tracing::warn!(
        width = preset.width,
        height = preset.height,
        ?actual,
        "window size unavailable"
    );
    false
}

fn load(path: &Path) -> Option<Preset> {
    let setting = std::fs::read_to_string(path).ok()?;
    let (width, height) = setting.trim().split_once('x')?;
    let (width, height) = (width.parse().ok()?, height.parse().ok()?);
    PRESETS
        .iter()
        .copied()
        .find(|preset| (preset.width, preset.height) == (width, height))
}

fn save(path: &Path, preset: Preset) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing window settings directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(file, "{}x{}", preset.width, preset.height)?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_1080p_resizes_the_client_and_saves_the_preset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window-size.txt");
        use_window_size_file(path.clone());
        let null = std::ptr::null_mut();
        let hwnd = super::super::CreateWindowExW(
            0,
            w!("STATIC"),
            w!("VIctory window size test"),
            wm::WS_OVERLAPPEDWINDOW,
            0,
            0,
            640,
            480,
            null,
            null,
            null,
            null,
        );
        assert!(!hwnd.is_null());
        // SAFETY: the test owns this window and sends it a native menu command.
        unsafe {
            assert!(!wm::GetPropW(hwnd, SIZE_MENU).is_null());
            wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, PRESETS[1].command, 0);
        }
        assert_eq!(client_size(hwnd), Some((1920, 1080)));
        assert_eq!(
            load(&path).map(|preset| (preset.width, preset.height)),
            Some((1920, 1080))
        );
        // SAFETY: this thread created the window.
        unsafe { wm::DestroyWindow(hwnd) };

        let reopened = super::super::CreateWindowExW(
            0,
            w!("STATIC"),
            w!("VIctory window size test"),
            wm::WS_OVERLAPPEDWINDOW,
            0,
            0,
            640,
            480,
            null,
            null,
            null,
            null,
        );
        assert!(!reopened.is_null());
        assert_eq!(client_size(reopened), Some((1920, 1080)));
        // SAFETY: this thread created the window.
        unsafe { wm::DestroyWindow(reopened) };
    }
}
