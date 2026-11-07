//! UI-thread dispatcher and one-shot timer helpers.
#![cfg(windows)]

use crate::sync::LockExt;
use crate::wallpaper_logic::Armed;
use crate::win::ffi as f;
use std::sync::{Mutex, Weak};

pub const WM_APP_RUN: u32 = 0x8000 + 1;
pub type UiJob = Box<dyn FnOnce() + Send + 'static>;

/// Post work from any thread. Failed PostMessage drops the boxed job (no leak).
pub fn post(hwnd: f::HWND, job: UiJob) -> bool {
    if hwnd.is_null() { return false; }
    let raw = Box::into_raw(Box::new(job));
    let ok = unsafe { f::PostMessageW(hwnd, WM_APP_RUN, 0, raw as isize) != 0 };
    if !ok { unsafe { drop(Box::from_raw(raw)); } }
    ok
}

/// `target` 이 아직 살아 있으면 UI 스레드에서 `action` 을 (락을 잡은 채) 실행하도록 예약한다.
/// HWND 는 포인터라 스레드 사이로 못 넘기므로 정수(`hwnd as usize`)로 받는다.
/// `action` 안에서 같은 `target` 의 락을 다시 잡으면 안 된다. 락 밖에서 해야 하는 일은 `post` 를 직접 쓴다.
/// 모달 창(MessageBox, 파일 선택)도 `action` 안에서 열면 안 된다: 창이 도는 동안 UI 스레드에 도착하는 다른 작업이
/// 같은 락을 기다리며 영구히 멈춘다. 창은 `post` 로 예약해 락이 풀린 뒤에 띄운다.
pub fn post_to<T: Send + 'static>(hwnd: usize, target: Weak<Mutex<T>>, action: impl FnOnce(&mut T) + Send + 'static) -> bool {
    post(hwnd as f::HWND, Box::new(move || {
        if let Some(t) = target.upgrade() { action(&mut *t.guard()); }
    }))
}

/// Must be called from the registered window handler for WM_APP_RUN.
pub fn dispatch_run(lparam: isize) -> isize {
    if lparam == 0 { return 0; }
    let job = unsafe { Box::from_raw(lparam as *mut UiJob) };
    // A panic here is contained by `host::window_proc` (FFI boundary) and reported by the panic hook.
    job();
    0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum TimerId { Eval = 1, Settle = 2, Snapshot = 3, Resume = 4, Poll = 5, Retry = 6, CloseHint = 7 }

impl TimerId {
    pub fn from_usize(v: usize) -> Option<Self> {
        Some(match v { 1 => Self::Eval, 2 => Self::Settle, 3 => Self::Snapshot, 4 => Self::Resume, 5 => Self::Poll, 6 => Self::Retry, 7 => Self::CloseHint, _ => return None })
    }
}

pub struct TimerSet { hwnd: f::HWND }
impl TimerSet {
    pub fn new(hwnd: f::HWND) -> Self { Self { hwnd } }
    /// SetTimer repeats; handlers must call `kill(id)` on the first WM_TIMER for one-shot use.
    pub fn set(&self, id: TimerId, ms: u32) -> bool {
        unsafe { f::SetTimer(self.hwnd, id as usize, ms.max(1), std::ptr::null_mut()) != 0 }
    }
    pub fn kill(&self, id: TimerId) { unsafe { f::KillTimer(self.hwnd, id as usize); } }
    pub fn kill_all(&self) {
        // Retry/CloseHint belong to the app window; a wallpaper restart must not cancel them.
        for id in [TimerId::Eval, TimerId::Settle, TimerId::Snapshot, TimerId::Resume, TimerId::Poll] { self.kill(id); }
    }
}

/// `TimerSet`에 "지금 걸려 있는 타이머"의 기록을 더한 것 (`Wallpaper` 전용, `&mut self`로만 쓴다).
/// `set`이 성공한 타이머만 걸린 것으로 보고, `kill`/`kill_all`이면 기록도 지운다.
/// 이미 걸려 있으면 다시 걸지 않는 디바운스(Eval)처럼 "걸려 있는가"를 물을 때 쓴다.
pub struct TrackedTimers { timers: TimerSet, armed: Armed }
impl TrackedTimers {
    pub fn new(hwnd: f::HWND) -> Self { Self { timers: TimerSet::new(hwnd), armed: Armed::default() } }
    pub fn is_armed(&self, id: TimerId) -> bool { self.armed.is_armed(id as usize) }
    /// 타이머를 건다. 실패하면 걸리지 않은 것으로 기록한다.
    pub fn set(&mut self, id: TimerId, ms: u32) -> bool {
        let ok = self.timers.set(id, ms);
        self.armed.mark(id as usize, ok);
        ok
    }
    pub fn kill(&mut self, id: TimerId) {
        self.timers.kill(id);
        self.armed.mark(id as usize, false);
    }
    /// 걸려 있을 때만 끈다 (걸려 있지 않으면 아무것도 하지 않는다).
    pub fn kill_if_armed(&mut self, id: TimerId) {
        if self.is_armed(id) { self.kill(id); }
    }
    pub fn kill_all(&mut self) {
        self.timers.kill_all();
        self.armed.clear();
    }
}
