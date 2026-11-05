//! Shell notification icon and context menu.
#![cfg(windows)]

use crate::sync::LockExt;
use super::consts::*;
use super::{ffi as f, host::{self, WindowHandler}};
use crate::win::util::wide;
use std::sync::{Arc, Mutex};

const WM_TRAY: u32 = 0x8001;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_INFO: u32 = 0x10;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;
const TPM_NONOTIFY: u32 = 0x0080;

#[derive(Clone)]
pub struct MenuItem {
    pub title: String,
    pub checked: bool,
    pub disabled: bool,
    pub action: Option<Arc<dyn Fn() + Send + Sync>>,
    pub children: Vec<MenuItem>,
    pub separator: bool,
}
impl MenuItem {
    pub fn command(title: impl Into<String>, action: impl Fn() + Send + Sync + 'static) -> Self {
        Self { title: title.into(), checked: false, disabled: false, action: Some(Arc::new(action)), children: vec![], separator: false }
    }
    pub fn separator() -> Self { Self { title: String::new(), checked: false, disabled: false, action: None, children: vec![], separator: true } }
}

struct TrayState { hwnd: f::HWND, icon: f::HICON, owns_icon: bool, ok: bool, tooltip: String }
unsafe impl Send for TrayState {}
struct Callbacks {
    menu: Arc<dyn Fn() -> Vec<MenuItem> + Send + Sync>,
    click: Arc<dyn Fn() + Send + Sync>,
    state: Arc<Mutex<TrayState>>,
    taskbar_msg: u32,
}

fn make_data(hwnd: f::HWND, icon: f::HICON, tooltip: &str, flags: u32) -> f::NOTIFYICONDATAW {
    let mut data: f::NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<f::NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd; data.uID = 1; data.uFlags = flags;
    data.uCallbackMessage = WM_TRAY; data.hIcon = icon;
    let encoded = wide(tooltip);
    let n = encoded.len().saturating_sub(1).min(127);
    data.szTip[..n].copy_from_slice(&encoded[..n]);
    data
}

fn load_icon(path: Option<&str>) -> (f::HICON, bool) {
    if let Some(path) = path {
        let p = wide(path);
        let cx = unsafe { f::GetSystemMetrics(49) };
        let cy = unsafe { f::GetSystemMetrics(50) };
        let h = unsafe { f::LoadImageW(std::ptr::null_mut(), p.as_ptr(), 1, cx, cy, 0x10) };
        if !h.is_null() { return (h as f::HICON, true); }
    }
    (unsafe { f::LoadIconW(std::ptr::null_mut(), 32512usize as *const u16) }, false)
}

fn add_icon(state: &Arc<Mutex<TrayState>>) -> bool {
    state.with_lock(|s| {
        let mut data = make_data(s.hwnd, s.icon, &s.tooltip, NIF_MESSAGE | NIF_ICON | NIF_TIP);
        s.ok = unsafe { f::Shell_NotifyIconW(NIM_ADD, &mut data) != 0 };
        s.ok
    })
}

fn append_menu(menu: f::HMENU, items: &[MenuItem], actions: &mut Vec<(u32, Arc<dyn Fn() + Send + Sync>)>, next: &mut u32) {
    for item in items {
        if item.separator {
            unsafe { f::AppendMenuW(menu, 0x0800, 0, std::ptr::null()); }
            continue;
        }
        let title = wide(&item.title);
        if !item.children.is_empty() {
            let sub = unsafe { f::CreatePopupMenu() };
            if sub.is_null() { continue; }
            append_menu(sub, &item.children, actions, next);
            let added = unsafe { f::AppendMenuW(menu, 0x0010 | if item.disabled { 0x0001 } else { 0 }, sub as usize, title.as_ptr()) };
            if added == 0 { unsafe { f::DestroyMenu(sub); } }
        } else {
            let id = *next; *next = (*next).saturating_add(1);
            let flags = 0x0000 | if item.checked { 0x0008 } else { 0 } | if item.disabled { 0x0001 } else { 0 };
            unsafe { f::AppendMenuW(menu, flags, id as usize, title.as_ptr()); }
            if let Some(action) = item.action.as_ref() { actions.push((id, action.clone())); }
        }
    }
}

fn popup(callbacks: &Callbacks) {
    let items = (callbacks.menu)();
    let menu = unsafe { f::CreatePopupMenu() };
    if menu.is_null() { return; }
    let mut actions = Vec::new(); let mut next = 100u32;
    append_menu(menu, &items, &mut actions, &mut next);
    let mut point = f::POINT { x: 0, y: 0 };
    unsafe {
        f::GetCursorPos(&mut point);
        let hwnd = callbacks.state.with_lock(|s| s.hwnd);
        f::SetForegroundWindow(hwnd);
        let selected = f::TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, point.x, point.y, 0, hwnd, std::ptr::null());
        f::PostMessageW(hwnd, WM_NULL, 0, 0);
        f::DestroyMenu(menu);
        if selected != 0 {
            if let Some((_, action)) = actions.iter().find(|(id, _)| *id == selected) { action(); }
        }
    }
}

pub struct Tray { callbacks: Arc<Callbacks> }
impl Tray {
    pub fn new(tooltip: &str, icon_path: Option<&str>,
        menu_provider: impl Fn() -> Vec<MenuItem> + Send + Sync + 'static,
        on_click: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(TrayState { hwnd: std::ptr::null_mut(), icon: std::ptr::null_mut(), owns_icon: false, ok: false, tooltip: tooltip.to_string() }));
        let menu: Arc<dyn Fn() -> Vec<MenuItem> + Send + Sync> = Arc::new(menu_provider);
        let click: Arc<dyn Fn() + Send + Sync> = Arc::new(on_click);
        let taskbar_name = wide("TaskbarCreated");
        let taskbar_msg = unsafe { f::RegisterWindowMessageW(taskbar_name.as_ptr()) };
        let callbacks = Arc::new(Callbacks { menu, click, state: state.clone(), taskbar_msg });
        let cb = callbacks.clone();
        let handler: WindowHandler = Arc::new(move |_hwnd, msg, _wp, lp| {
            if msg == WM_TRAY {
                if lp as usize == WM_LBUTTONUP { (cb.click)(); }
                else if lp as usize == WM_RBUTTONUP { popup(&cb); }
                return Some(0);
            }
            if cb.taskbar_msg != 0 && msg == cb.taskbar_msg { add_icon(&cb.state); return Some(0); }
            None
        });
        let hwnd = host::create_hidden_top_level(handler)?;
        { let mut s = state.guard();
            s.hwnd = hwnd;
            let (icon, owns_icon) = load_icon(icon_path);
            s.icon = icon;
            s.owns_icon = owns_icon;
        }
        add_icon(&state); // It may fail before Explorer's taskbar is ready; TaskbarCreated retries.
        Ok(Self { callbacks })
    }

    pub fn ok(&self) -> bool { self.callbacks.state.with_lock(|s| s.ok) }
    pub fn set_tooltip(&self, text: &str) {
        { let mut s = self.callbacks.state.guard();
            s.tooltip = text.chars().take(127).collect();
            let mut data = make_data(s.hwnd, s.icon, &s.tooltip, NIF_TIP);
            unsafe { f::Shell_NotifyIconW(NIM_MODIFY, &mut data); }
        }
    }
    pub fn notify(&self, message: &str, title: &str) {
        { let s = self.callbacks.state.guard();
            let mut data = make_data(s.hwnd, s.icon, &s.tooltip, NIF_INFO);
            let body = wide(message); let heading = wide(title);
            let n = body.len().saturating_sub(1).min(255); data.szInfo[..n].copy_from_slice(&body[..n]);
            let n = heading.len().saturating_sub(1).min(63); data.szInfoTitle[..n].copy_from_slice(&heading[..n]);
            data.dwInfoFlags = 1;
            unsafe { f::Shell_NotifyIconW(NIM_MODIFY, &mut data); }
        }
    }
    pub fn close(&mut self) {
        { let mut s = self.callbacks.state.guard();
            if !s.hwnd.is_null() {
                if s.ok { let mut data = make_data(s.hwnd, s.icon, &s.tooltip, 0); unsafe { f::Shell_NotifyIconW(NIM_DELETE, &mut data); } }
                host::destroy_window(s.hwnd); s.hwnd = std::ptr::null_mut(); s.ok = false;
                if s.owns_icon && !s.icon.is_null() { unsafe { f::DestroyIcon(s.icon); } }
                s.icon = std::ptr::null_mut(); s.owns_icon = false;
            }
        }
    }
}
impl Drop for Tray { fn drop(&mut self) { self.close(); } }
