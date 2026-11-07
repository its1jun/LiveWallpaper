//! 여러 파일이 함께 쓰는 Win32 상수. (컨트롤 전용 상수는 쓰는 파일에 둔다.)
#![cfg(windows)]

use super::ffi as f;

// 창 스타일
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
pub const WS_TABSTOP: u32 = 0x0001_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
pub const GWL_STYLE: i32 = -16;
pub const GWL_EXSTYLE: i32 = -20;

// ShowWindow / SetWindowPos
pub const SW_HIDE: i32 = 0;
pub const SW_SHOW: i32 = 5;
pub const SW_SHOWNA: i32 = 8;
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_SHOWWINDOW: u32 = 0x0040;
pub const HWND_BOTTOM: f::HWND = 1usize as f::HWND;
pub const HWND_TOPMOST: f::HWND = (-1isize) as f::HWND;
pub const HWND_NOTOPMOST: f::HWND = (-2isize) as f::HWND;

// 창 메시지
pub const WM_NULL: u32 = 0x0000;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_SETTINGCHANGE: u32 = 0x001A;
pub const WM_GETMINMAXINFO: u32 = 0x0024;
pub const WM_SETICON: u32 = 0x0080;
pub const WM_DISPLAYCHANGE: u32 = 0x007E;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
/// 트레이 아이콘 콜백의 lparam 값 (WM_TRAY 의 lparam)
pub const WM_LBUTTONUP: usize = 0x0202;
pub const WM_RBUTTONUP: usize = 0x0205;
