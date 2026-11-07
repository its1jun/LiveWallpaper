//! Event-driven Windows desktop/system watcher.
#![cfg(windows)]

use crate::sync::LockExt;
use super::consts::*;
use super::{ffi as f, host::{self, WindowHandler}};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const PBT_POWERSETTINGCHANGE: usize = 0x8013;
const PBT_APMSUSPEND: usize = 0x0004;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMPOWERSTATUSCHANGE: usize = 0x000A;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;
const WM_APP_WATCH_CHANGE: u32 = 0x8000 + 0x32;
// GUID_CONSOLE_DISPLAY_STATE = 6FE69556-704A-47A0-8F24-C28D936FDA47.
// GUID's first three fields are stored little-endian in memory (Data2 starts 4A 70).
const DISPLAY_GUID: [u8; 16] = [0x56, 0x95, 0xE6, 0x6F, 0x4A, 0x70, 0xA0, 0x47, 0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47];
const EVENT_RANGES: [(u32, u32); 6] = [
    (0x0003, 0x0003), (0x000B, 0x000B), (0x0016, 0x0017),
    (0x8002, 0x8003), (0x800B, 0x800B), (0x8017, 0x8018),
];

/// True while a WM_APP_WATCH_CHANGE is already queued. WinEvents (especially LOCATIONCHANGE while a
/// window is dragged) can fire hundreds of times per second; only one message may wait in the queue.
static WATCH_POSTED: AtomicBool = AtomicBool::new(false);

fn watcher_windows() -> &'static Mutex<Vec<usize>> {
    static WINDOWS: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
    WINDOWS.get_or_init(|| Mutex::new(Vec::new()))
}

unsafe extern "system" fn on_win_event(_hook: f::HANDLE, _event: u32, hwnd: f::HWND,
    id_object: i32, id_child: i32, _thread: u32, _time: u32) {
    if hwnd.is_null() || id_object != OBJID_WINDOW || id_child != CHILDID_SELF { return; }
    if WATCH_POSTED.swap(true, Ordering::AcqRel) { return; }
    let mut posted_any = false;
    { let windows = watcher_windows().guard();
        for &h in windows.iter() {
            if f::PostMessageW(h as f::HWND, WM_APP_WATCH_CHANGE, 0, 0) != 0 { posted_any = true; }
        }
    }
    // Nothing was queued, so no handler will ever clear the flag: clear it here.
    if !posted_any { WATCH_POSTED.store(false, Ordering::Release); }
}

pub struct SystemWatcher {
    hwnd: f::HWND,
    power: f::HPOWERNOTIFY,
    hooks: Vec<f::HHOOK>,
    wts: bool,
    ok: bool,
}

impl SystemWatcher {
    pub fn new(on_display: impl Fn(bool) + Send + Sync + 'static,
               on_change: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let on_display: Arc<dyn Fn(bool) + Send + Sync> = Arc::new(on_display);
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(on_change);
        let change_for_window = on_change.clone();
        let display_for_window = on_display.clone();
        let taskbar_name = crate::win::util::wide("TaskbarCreated");
        let taskbar_created = unsafe { f::RegisterWindowMessageW(taskbar_name.as_ptr()) };
        let handler: WindowHandler = Arc::new(move |_hwnd, msg, wp, lp| {
            if msg == WM_APP_WATCH_CHANGE {
                // Clear first so an event arriving during the check queues a fresh message.
                WATCH_POSTED.store(false, Ordering::Release);
                change_for_window();
                return Some(0);
            }
            if msg == WM_POWERBROADCAST {
                if wp == PBT_POWERSETTINGCHANGE {
                    if lp != 0 {
                        let ptr = lp as *const u8;
                        unsafe {
                            // POWERBROADCAST_SETTING = GUID(16), DataLength(4), Data(1+).
                            let data_len = std::ptr::read_unaligned(ptr.add(16) as *const u32);
                            if data_len >= 1 {
                                let guid = std::slice::from_raw_parts(ptr, 16);
                                if guid == DISPLAY_GUID {
                                    let value = *ptr.add(20) != 0;
                                    display_for_window(value);
                                }
                            }
                        }
                    }
                    return Some(1);
                }
                if [PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMESUSPEND, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND].contains(&wp) {
                    change_for_window();
                    return Some(1);
                }
                return None;
            }
            if msg == WM_WTSSESSION_CHANGE || msg == WM_DISPLAYCHANGE || msg == WM_SETTINGCHANGE
                || (taskbar_created != 0 && msg == taskbar_created) {
                change_for_window();
            }
            None
        });
        WATCH_POSTED.store(false, Ordering::Release);
        let hwnd = host::create_hidden_top_level(handler)?;
        let guid = DISPLAY_GUID;
        let power = unsafe { f::RegisterPowerSettingNotification(hwnd, guid.as_ptr() as *const c_void, 0) };
        let mut hooks = Vec::with_capacity(EVENT_RANGES.len());
        for (lo, hi) in EVENT_RANGES {
            let h = unsafe { f::SetWinEventHook(lo, hi, std::ptr::null_mut(), Some(on_win_event), 0, 0, WINEVENT_SKIPOWNPROCESS) };
            if !h.is_null() { hooks.push(h); }
        }
        let wts = unsafe { f::WTSRegisterSessionNotification(hwnd, 0) != 0 };
        watcher_windows().with_lock(|list| list.push(hwnd as usize));
        let ok = !power.is_null() && hooks.len() == EVENT_RANGES.len();
        Ok(Self { hwnd, power, hooks, wts, ok })
    }

    pub fn ok(&self) -> bool { self.ok }

    pub fn close(&mut self) {
        watcher_windows().with_lock(|list| list.retain(|&h| h != self.hwnd as usize));
        // A queued message dies with its window; do not leave the flag stuck on.
        WATCH_POSTED.store(false, Ordering::Release);
        for hook in self.hooks.drain(..) { unsafe { f::UnhookWinEvent(hook); } }
        if !self.power.is_null() {
            unsafe { f::UnregisterPowerSettingNotification(self.power); }
            self.power = std::ptr::null_mut();
        }
        if !self.hwnd.is_null() {
            if self.wts { unsafe { f::WTSUnRegisterSessionNotification(self.hwnd); } self.wts = false; }
            host::destroy_window(self.hwnd);
            self.hwnd = std::ptr::null_mut();
        }
    }
}

impl Drop for SystemWatcher { fn drop(&mut self) { self.close(); } }

