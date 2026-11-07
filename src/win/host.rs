//! Desktop host and common Win32 window plumbing.
#![cfg(windows)]

use super::consts::*;
use super::ffi as f;
use super::util::{from_wide, wide};
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use std::ffi::c_void;

pub const CLASS_NAME: &str = "LiveWallpaperWindow";
pub const LWA_ALPHA: u32 = 0x02;

pub type WindowHandler = Arc<dyn Fn(f::HWND, u32, usize, isize) -> Option<isize> + Send + Sync + 'static>;

thread_local! {
    // Clone the Arc before calling it so no RefCell borrow is held across Win32 calls.
    static HANDLERS: RefCell<HashMap<isize, WindowHandler>> = RefCell::new(HashMap::new());
}

static CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();
static UI_CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();
/// Class of the settings window. Unlike the wallpaper host/watcher windows (black background),
/// it paints the normal dialog background (COLOR_BTNFACE) and shows the arrow cursor.
pub const UI_CLASS_NAME: &str = "LiveWallpaperMainWindow";

unsafe extern "system" fn window_proc(hwnd: f::HWND, msg: u32, wp: usize, lp: isize) -> isize {
    let handler = HANDLERS.with(|h| h.try_borrow().ok().and_then(|m| m.get(&(hwnd as isize)).cloned()));
    if let Some(handler) = handler {
        if let Ok(Some(result)) = catch_unwind(AssertUnwindSafe(|| handler(hwnd, msg, wp, lp))) {
            return result;
        }
    }
    if msg == 0x0082 { // WM_NCDESTROY
        HANDLERS.with(|h| {
            if let Ok(mut m) = h.try_borrow_mut() { m.remove(&(hwnd as isize)); }
        });
    }
    f::DefWindowProcW(hwnd, msg, wp, lp)
}

fn ensure_class() -> Result<(), String> {
    let registered = CLASS_REGISTERED.get_or_init(|| unsafe {
        let class_name = wide(CLASS_NAME);
        let instance = f::GetModuleHandleW(std::ptr::null());
        let class = f::WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: f::GetStockObject(4), // BLACK_BRUSH
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        f::RegisterClassW(&class) != 0 || f::GetLastError() == 1410 // ERROR_CLASS_ALREADY_EXISTS
    });
    if *registered { Ok(()) } else { Err(format!("RegisterClassW failed ({})", unsafe { f::GetLastError() })) }
}

fn ensure_ui_class() -> Result<(), String> {
    let registered = UI_CLASS_REGISTERED.get_or_init(|| unsafe {
        let class_name = wide(UI_CLASS_NAME);
        let instance = f::GetModuleHandleW(std::ptr::null());
        let class = f::WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: f::LoadCursorW(std::ptr::null_mut(), 32512usize as *const u16), // IDC_ARROW
            hbrBackground: 16usize as f::HBRUSH, // COLOR_BTNFACE + 1
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        f::RegisterClassW(&class) != 0 || f::GetLastError() == 1410 // ERROR_CLASS_ALREADY_EXISTS
    });
    if *registered { Ok(()) } else { Err(format!("RegisterClassW failed ({})", unsafe { f::GetLastError() })) }
}

pub fn register_handler(hwnd: f::HWND, handler: WindowHandler) {
    HANDLERS.with(|h| {
        if let Ok(mut map) = h.try_borrow_mut() { map.insert(hwnd as isize, handler); }
    });
}

pub fn unregister_handler(hwnd: f::HWND) {
    HANDLERS.with(|h| {
        if let Ok(mut map) = h.try_borrow_mut() { map.remove(&(hwnd as isize)); }
    });
}

pub fn create_window(ex_style: u32, style: u32, parent: f::HWND, width: i32, height: i32,
                     title: &str, handler: Option<WindowHandler>) -> Result<f::HWND, String> {
    ensure_class()?;
    let class_name = wide(CLASS_NAME);
    let title_w = wide(title);
    let instance = unsafe { f::GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        f::CreateWindowExW(ex_style, class_name.as_ptr(), title_w.as_ptr(), style,
            0, 0, width.max(0), height.max(0), parent, std::ptr::null_mut(), instance,
            std::ptr::null_mut::<c_void>())
    };
    if hwnd.is_null() { return Err(format!("CreateWindowExW failed ({})", unsafe { f::GetLastError() })); }
    if let Some(h) = handler { register_handler(hwnd, h); }
    Ok(hwnd)
}

/// Top-level settings window at (x, y) with the given outer size.
pub fn create_ui_window(style: u32, x: i32, y: i32, width: i32, height: i32,
                        title: &str, handler: Option<WindowHandler>) -> Result<f::HWND, String> {
    ensure_ui_class()?;
    let class_name = wide(UI_CLASS_NAME);
    let title_w = wide(title);
    let instance = unsafe { f::GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        f::CreateWindowExW(0, class_name.as_ptr(), title_w.as_ptr(), style,
            x, y, width.max(0), height.max(0), std::ptr::null_mut(), std::ptr::null_mut(), instance,
            std::ptr::null_mut::<c_void>())
    };
    if hwnd.is_null() { return Err(format!("CreateWindowExW failed ({})", unsafe { f::GetLastError() })); }
    if let Some(h) = handler { register_handler(hwnd, h); }
    Ok(hwnd)
}

pub fn create_host_window() -> f::HWND {
    let (w, h) = super::util::screen_size();
    create_window(WS_EX_TOOLWINDOW, WS_POPUP, std::ptr::null_mut(), w, h, "LiveWallpaper", None)
        .unwrap_or(std::ptr::null_mut())
}

pub fn create_hidden_top_level(handler: WindowHandler) -> Result<f::HWND, String> {
    let hwnd = create_window(WS_EX_TOOLWINDOW, WS_POPUP, std::ptr::null_mut(), 0, 0,
        "LiveWallpaper", Some(handler))?;
    Ok(hwnd)
}

pub fn destroy_window(hwnd: f::HWND) {
    if !hwnd.is_null() {
        unregister_handler(hwnd);
        unsafe { f::DestroyWindow(hwnd); }
    }
}

pub fn hide_window(hwnd: f::HWND) { if !hwnd.is_null() { unsafe { f::ShowWindow(hwnd, SW_HIDE); } } }
pub fn show_window(hwnd: f::HWND) { if !hwnd.is_null() { unsafe { f::ShowWindow(hwnd, SW_SHOWNA); } } }
pub fn host_alive(hwnd: f::HWND) -> bool { !hwnd.is_null() && unsafe { f::IsWindow(hwnd) != 0 } }

fn find_progman() -> f::HWND {
    let name = wide("Progman");
    let hwnd = unsafe { f::FindWindowW(name.as_ptr(), std::ptr::null()) };
    if !hwnd.is_null() {
        let mut result = 0usize;
        unsafe { f::SendMessageTimeoutW(hwnd, 0x052C, 0xD, 0x1, 0, 1000, &mut result); }
    }
    hwnd
}

fn make_child_style(hwnd: f::HWND) {
    unsafe {
        let old = f::GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let new = (old & !WS_POPUP) | WS_CHILD;
        f::SetWindowLongPtrW(hwnd, GWL_STYLE, new as isize);
    }
}

fn fit_to_parent(hwnd: f::HWND, parent: f::HWND, after: f::HWND) -> bool {
    let mut rect = f::RECT::default();
    let ok = unsafe { f::GetClientRect(parent, &mut rect) != 0 };
    let mut w = if ok { rect.right - rect.left } else { 0 };
    let mut h = if ok { rect.bottom - rect.top } else { 0 };
    if w <= 0 || h <= 0 { (w, h) = super::util::screen_size(); }
    w > 0 && h > 0 && unsafe {
        f::SetWindowPos(hwnd, after, 0, 0, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW) != 0
    }
}

/// 해상도/배율이 바뀐 뒤 호스트 창을 부모(없으면 화면) 크기에 다시 맞춘다. 크기가 같으면 아무것도 하지 않는다.
/// 순서와 표시 상태는 건드리지 않는다 (정적 전환 중 숨겨 둔 창이 다시 보이면 안 된다).
pub fn refit_host(hwnd: f::HWND) {
    if hwnd.is_null() { return; }
    let parent = unsafe { f::GetParent(hwnd) };
    let mut target = f::RECT::default();
    let have_parent = !parent.is_null() && unsafe { f::GetClientRect(parent, &mut target) != 0 };
    let (mut w, mut h) = if have_parent { (target.right - target.left, target.bottom - target.top) } else { (0, 0) };
    if w <= 0 || h <= 0 { (w, h) = super::util::screen_size(); }
    if w <= 0 || h <= 0 { return; }
    let mut current = f::RECT::default();
    if unsafe { f::GetClientRect(hwnd, &mut current) } != 0
        && current.right - current.left == w && current.bottom - current.top == h { return; }
    unsafe { f::SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOACTIVATE | SWP_NOZORDER); }
}

fn attach_a(hwnd: f::HWND) -> Result<(), String> {
    let progman = find_progman();
    if progman.is_null() { return Err("Progman window not found".into()); }
    let defview_name = wide("SHELLDLL_DefView");
    let defview = unsafe { f::FindWindowExW(progman, std::ptr::null_mut(), defview_name.as_ptr(), std::ptr::null()) };
    if defview.is_null() {
        return Err("No icon layer under Progman (this PC likely uses the legacy layout)".into());
    }
    unsafe {
        let ex = f::GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        f::SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | WS_EX_LAYERED) as isize);
        if f::SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA) == 0 {
            let error = f::GetLastError();
            // Restore the original ex-style so the Method B fallback starts from a clean window.
            f::SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
            return Err(format!("SetLayeredWindowAttributes failed ({error})"));
        }
    }
    make_child_style(hwnd);
    unsafe { f::SetLastError(0); f::SetParent(hwnd, progman); }
    let error = unsafe { f::GetLastError() };
    if unsafe { f::GetParent(hwnd) } != progman {
        return Err(if error != 0 { format!("SetParent failed ({error})") } else { "SetParent failed".into() });
    }
    if !fit_to_parent(hwnd, progman, defview) { return Err("SetWindowPos failed".into()); }
    if unsafe { f::GetParent(hwnd) } != progman || unsafe { f::IsWindowVisible(hwnd) } == 0 {
        return Err("desktop host verification failed".into());
    }
    Ok(())
}

fn attach_b(hwnd: f::HWND) -> Result<(), String> {
    let mut top: f::HWND = std::ptr::null_mut();
    unsafe {
        let context = &mut top as *mut f::HWND as isize;
        // EnumWindows callback uses a local, stack-owned output pointer.
        f::EnumWindows(Some(enum_find_defview), context);
    }
    if top.is_null() { return Err("SHELLDLL_DefView top-level window not found".into()); }
    let worker_class = wide("WorkerW");
    let worker = unsafe { f::FindWindowExW(std::ptr::null_mut(), top, worker_class.as_ptr(), std::ptr::null()) };
    if worker.is_null() { return Err("WorkerW window not found".into()); }
    make_child_style(hwnd);
    unsafe { f::SetLastError(0); f::SetParent(hwnd, worker); }
    let error = unsafe { f::GetLastError() };
    if unsafe { f::GetParent(hwnd) } != worker {
        return Err(if error != 0 { format!("SetParent failed ({error})") } else { "SetParent failed".into() });
    }
    if !fit_to_parent(hwnd, worker, HWND_BOTTOM) { return Err("SetWindowPos failed".into()); }
    if unsafe { f::GetParent(hwnd) } != worker { return Err("desktop host verification failed".into()); }
    Ok(())
}

unsafe extern "system" fn enum_find_defview(hwnd: f::HWND, lparam: isize) -> i32 {
    // No Rust panic may unwind through the Win32 callback boundary.
    catch_unwind(AssertUnwindSafe(|| {
        let class = wide("SHELLDLL_DefView");
        let found = f::FindWindowExW(hwnd, std::ptr::null_mut(), class.as_ptr(), std::ptr::null());
        if !found.is_null() {
            let out = &mut *(lparam as *mut f::HWND);
            *out = hwnd;
            0
        } else { 1 }
    })).unwrap_or(1)
}

/// Attach in modern Progman-child mode first, then fall back to WorkerW sibling mode.
pub fn attach_to_desktop(hwnd: f::HWND) -> (bool, String) {
    if hwnd.is_null() { return (false, "Invalid host window".into()); }
    match attach_a(hwnd) {
        Ok(()) => (true, "Method A (Progman child, layered)".into()),
        Err(a) => match attach_b(hwnd) {
            Ok(()) => (true, "Method B (WorkerW sibling)".into()),
            Err(b) => (false, format!("Method A: {a} / Method B: {b}")),
        },
    }
}

pub fn window_class_name(hwnd: f::HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { f::GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 { String::new() } else { from_wide(&buf[..n as usize]) }
}
