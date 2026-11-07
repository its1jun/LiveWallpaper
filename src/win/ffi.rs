//! Raw Win32 declarations used by the Windows backend.
//! All handles are opaque pointers; Win32 BOOL is a 32-bit integer.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type HWND = *mut c_void;
pub type HINSTANCE = *mut c_void;
pub type HICON = *mut c_void;
pub type HCURSOR = *mut c_void;
pub type HBRUSH = *mut c_void;
pub type HMENU = *mut c_void;
pub type HHOOK = *mut c_void;
pub type HPOWERNOTIFY = *mut c_void;
pub type HKEY = *mut c_void;
pub type BOOL = i32;
pub type DWORD = u32;
pub type UINT = u32;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type LONG_PTR = isize;
pub type ULONG_PTR = usize;
pub type ATOM = u16;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RECT { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT { pub x: i32, pub y: i32 }

pub type WNDPROC = Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT>;
pub type WNDENUMPROC = Option<unsafe extern "system" fn(HWND, LPARAM) -> BOOL>;
pub type WINEVENTPROC = Option<unsafe extern "system" fn(HANDLE, DWORD, HWND, i32, i32, DWORD, DWORD)>;

#[repr(C)]
pub struct WNDCLASSW {
    pub style: UINT,
    pub lpfnWndProc: WNDPROC,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HINSTANCE,
    pub hIcon: HICON,
    pub hCursor: HCURSOR,
    pub hbrBackground: HBRUSH,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
}

#[repr(C)]
pub struct MINMAXINFO {
    pub ptReserved: POINT,
    pub ptMaxSize: POINT,
    pub ptMaxPosition: POINT,
    pub ptMinTrackSize: POINT,
    pub ptMaxTrackSize: POINT,
}

#[repr(C)]
pub struct SYSTEM_POWER_STATUS {
    pub ACLineStatus: u8,
    pub BatteryFlag: u8,
    pub BatteryLifePercent: u8,
    pub SystemStatusFlag: u8,
    pub BatteryLifeTime: DWORD,
    pub BatteryFullLifeTime: DWORD,
}

#[repr(C)]
pub struct POWERBROADCAST_SETTING {
    pub PowerSetting: [u8; 16],
    pub DataLength: DWORD,
    pub Data: [u8; 1],
}

#[repr(C)]
pub struct OPENFILENAMEW {
    pub lStructSize: DWORD,
    pub hwndOwner: HWND,
    pub hInstance: HINSTANCE,
    pub lpstrFilter: *const u16,
    pub lpstrCustomFilter: *mut u16,
    pub nMaxCustFilter: DWORD,
    pub nFilterIndex: DWORD,
    pub lpstrFile: *mut u16,
    pub nMaxFile: DWORD,
    pub lpstrFileTitle: *mut u16,
    pub nMaxFileTitle: DWORD,
    pub lpstrInitialDir: *const u16,
    pub lpstrTitle: *const u16,
    pub Flags: DWORD,
    pub nFileOffset: u16,
    pub nFileExtension: u16,
    pub lpstrDefExt: *const u16,
    pub lCustData: LPARAM,
    pub lpfnHook: Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> UINT>,
    pub lpTemplateName: *const u16,
    pub pvReserved: *mut c_void,
    pub dwReserved: DWORD,
    pub FlagsEx: DWORD,
}

#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cbSize: DWORD,
    pub hWnd: HWND,
    pub uID: UINT,
    pub uFlags: UINT,
    pub uCallbackMessage: UINT,
    pub hIcon: HICON,
    pub szTip: [u16; 128],
    pub dwState: DWORD,
    pub dwStateMask: DWORD,
    pub szInfo: [u16; 256],
    pub uTimeoutOrVersion: UINT,
    pub szInfoTitle: [u16; 64],
    pub dwInfoFlags: DWORD,
    pub guidItem: [u8; 16],
    pub hBalloonIcon: HICON,
}


#[link(name = "user32")]
extern "system" {
    pub fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, utype: UINT) -> i32;
    pub fn SystemParametersInfoW(action: UINT, uiparam: UINT, pvparam: *mut c_void, winini: UINT) -> BOOL;
    pub fn GetSystemMetrics(index: i32) -> i32;
    pub fn RegisterClassW(class: *const WNDCLASSW) -> ATOM;
    pub fn CreateWindowExW(ex: DWORD, class_name: *const u16, title: *const u16, style: DWORD,
        x: i32, y: i32, width: i32, height: i32, parent: HWND, menu: HMENU,
        instance: HINSTANCE, param: *mut c_void) -> HWND;
    pub fn DefWindowProcW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    pub fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
    pub fn DestroyWindow(hwnd: HWND) -> BOOL;
    pub fn IsWindow(hwnd: HWND) -> BOOL;
    pub fn IsWindowVisible(hwnd: HWND) -> BOOL;
    pub fn GetParent(hwnd: HWND) -> HWND;
    pub fn SetParent(hwnd: HWND, parent: HWND) -> HWND;
    pub fn FindWindowW(class_name: *const u16, title: *const u16) -> HWND;
    pub fn FindWindowExW(parent: HWND, after: HWND, class_name: *const u16, title: *const u16) -> HWND;
    pub fn SendMessageTimeoutW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM,
        flags: UINT, timeout: UINT, result: *mut ULONG_PTR) -> LRESULT;
    pub fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    pub fn GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    pub fn SetWindowPos(hwnd: HWND, after: HWND, x: i32, y: i32, cx: i32, cy: i32, flags: UINT) -> BOOL;
    pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> LONG_PTR;
    pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: LONG_PTR) -> LONG_PTR;
    pub fn SetLayeredWindowAttributes(hwnd: HWND, color: DWORD, alpha: u8, flags: DWORD) -> BOOL;
    pub fn EnumWindows(callback: WNDENUMPROC, lparam: LPARAM) -> BOOL;
    pub fn IsIconic(hwnd: HWND) -> BOOL;
    pub fn IsZoomed(hwnd: HWND) -> BOOL;
    pub fn GetForegroundWindow() -> HWND;
    pub fn GetClassNameW(hwnd: HWND, name: *mut u16, max: i32) -> i32;
    pub fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut DWORD) -> DWORD;
    pub fn GetCursorPos(point: *mut POINT) -> BOOL;
    pub fn SetForegroundWindow(hwnd: HWND) -> BOOL;
    pub fn TrackPopupMenu(menu: HMENU, flags: UINT, x: i32, y: i32, reserved: i32, hwnd: HWND, rect: *const RECT) -> UINT;
    pub fn PostMessageW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> BOOL;
    pub fn SendMessageW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    pub fn SetWindowTextW(hwnd: HWND, text: *const u16) -> BOOL;
    pub fn EnableWindow(hwnd: HWND, enable: BOOL) -> BOOL;
    pub fn MoveWindow(hwnd: HWND, x: i32, y: i32, width: i32, height: i32, repaint: BOOL) -> BOOL;
    pub fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: UINT, max: UINT) -> BOOL;
    pub fn TranslateMessage(msg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(code: i32);
    pub fn SetTimer(hwnd: HWND, id: usize, elapsed: UINT, callback: *mut c_void) -> usize;
    pub fn KillTimer(hwnd: HWND, id: usize) -> BOOL;
    pub fn RegisterWindowMessageW(name: *const u16) -> UINT;
    pub fn LoadImageW(instance: HINSTANCE, name: *const u16, image_type: UINT,
        cx: i32, cy: i32, flags: UINT) -> HANDLE;
    pub fn LoadIconW(instance: HINSTANCE, name: *const u16) -> HICON;
    pub fn DestroyIcon(icon: HICON) -> BOOL;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn AppendMenuW(menu: HMENU, flags: UINT, id: usize, text: *const u16) -> BOOL;
    pub fn DestroyMenu(menu: HMENU) -> BOOL;
}

#[repr(C)]
pub struct MSG { pub hwnd: HWND, pub message: UINT, pub wParam: WPARAM, pub lParam: LPARAM,
    pub time: DWORD, pub pt: POINT, pub lPrivate: DWORD }

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateMutexW(attrs: *const c_void, initial_owner: BOOL, name: *const u16) -> HANDLE;
    pub fn ReleaseMutex(handle: HANDLE) -> BOOL;
    pub fn CloseHandle(handle: HANDLE) -> BOOL;
    pub fn GetLastError() -> DWORD;
    pub fn SetLastError(error: DWORD);
    pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    pub fn GetCurrentProcessId() -> DWORD;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn SetPriorityClass(process: HANDLE, class: DWORD) -> BOOL;
    pub fn SetProcessWorkingSetSize(process: HANDLE, min: usize, max: usize) -> BOOL;
    pub fn GetSystemPowerStatus(status: *mut SYSTEM_POWER_STATUS) -> BOOL;
    pub fn CreateFileW(name: *const u16, access: DWORD, share: DWORD, security: *mut c_void,
        creation: DWORD, flags: DWORD, template: HANDLE) -> HANDLE;
    pub fn CreateNamedPipeW(name: *const u16, open_mode: DWORD, pipe_mode: DWORD,
        max_instances: DWORD, out_size: DWORD, in_size: DWORD, timeout: DWORD,
        security: *mut c_void) -> HANDLE;
    pub fn ConnectNamedPipe(pipe: HANDLE, overlapped: *mut c_void) -> BOOL;
    pub fn DisconnectNamedPipe(pipe: HANDLE) -> BOOL;
    pub fn ReadFile(handle: HANDLE, buffer: *mut c_void, bytes: DWORD, read: *mut DWORD, overlapped: *mut c_void) -> BOOL;
    pub fn WriteFile(handle: HANDLE, buffer: *const c_void, bytes: DWORD, written: *mut DWORD, overlapped: *mut c_void) -> BOOL;
    pub fn FlushFileBuffers(handle: HANDLE) -> BOOL;
    pub fn WaitNamedPipeW(name: *const u16, timeout: DWORD) -> BOOL;
    pub fn Sleep(ms: DWORD);
    pub fn LoadLibraryW(name: *const u16) -> HANDLE;
    pub fn LoadLibraryExW(name: *const u16, file: HANDLE, flags: DWORD) -> HANDLE;
    pub fn GetProcAddress(module: HANDLE, name: *const u8) -> *mut c_void;
    pub fn FreeLibrary(module: HANDLE) -> BOOL;
}

#[link(name = "gdi32")]
extern "system" { pub fn GetStockObject(index: i32) -> HBRUSH; }

#[link(name = "dwmapi")]
extern "system" {
    pub fn DwmGetWindowAttribute(hwnd: HWND, attribute: DWORD, value: *mut c_void, size: DWORD) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    pub fn SHQueryUserNotificationState(state: *mut i32) -> i32;
    pub fn Shell_NotifyIconW(message: DWORD, data: *mut NOTIFYICONDATAW) -> BOOL;
}

#[link(name = "shcore")]
extern "system" { pub fn SetProcessDpiAwareness(value: i32) -> HRESULT; }

pub type HRESULT = i32;

#[link(name = "user32")]
extern "system" {
    pub fn RegisterPowerSettingNotification(recipient: HANDLE, setting: *const c_void, flags: DWORD) -> HPOWERNOTIFY;
    pub fn UnregisterPowerSettingNotification(notification: HPOWERNOTIFY) -> BOOL;
    pub fn SetWinEventHook(event_min: DWORD, event_max: DWORD, module: HANDLE, callback: WINEVENTPROC,
        process: DWORD, thread: DWORD, flags: DWORD) -> HHOOK;
    pub fn UnhookWinEvent(hook: HHOOK) -> BOOL;
}

#[link(name = "wtsapi32")]
extern "system" {
    pub fn WTSRegisterSessionNotification(hwnd: HWND, flags: DWORD) -> BOOL;
    pub fn WTSUnRegisterSessionNotification(hwnd: HWND) -> BOOL;
}

#[link(name = "advapi32")]
extern "system" {
    pub fn RegOpenKeyExW(key: HKEY, subkey: *const u16, options: DWORD, access: DWORD, result: *mut HKEY) -> i32;
    pub fn RegCreateKeyExW(key: HKEY, subkey: *const u16, reserved: DWORD, class: *mut u16,
        options: DWORD, access: DWORD, security: *mut c_void, result: *mut HKEY, disposition: *mut DWORD) -> i32;
    pub fn RegSetValueExW(key: HKEY, name: *const u16, reserved: DWORD, kind: DWORD,
        data: *const u8, size: DWORD) -> i32;
    pub fn RegDeleteValueW(key: HKEY, name: *const u16) -> i32;
    pub fn RegCloseKey(key: HKEY) -> i32;
}

#[link(name = "comdlg32")]
extern "system" { pub fn GetOpenFileNameW(ofn: *mut OPENFILENAMEW) -> BOOL; }

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateFontW(height: i32, width: i32, escapement: i32, orientation: i32, weight: i32, italic: u32,
                       underline: u32, strike_out: u32, charset: u32, out_precision: u32, clip_precision: u32,
                       quality: u32, pitch_and_family: u32, face: *const u16) -> HANDLE;
    pub fn DeleteObject(object: HANDLE) -> i32;
    pub fn GetDeviceCaps(hdc: HANDLE, index: i32) -> i32;
}

#[link(name = "user32")]
extern "system" {
    pub fn GetDC(hwnd: HWND) -> HANDLE;
    pub fn ReleaseDC(hwnd: HWND, hdc: HANDLE) -> i32;
    pub fn AdjustWindowRect(rect: *mut RECT, style: u32, menu: i32) -> i32;
    pub fn LoadCursorW(instance: HINSTANCE, name: *const u16) -> HCURSOR;
}
