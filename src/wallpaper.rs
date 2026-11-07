//! Live wallpaper lifecycle and hidden/static/resume state machine.
#![cfg(windows)]

use crate::sync::LockExt;
use crate::config::{self, FitMode, Settings};
use crate::media;
use crate::player::Player;
use crate::snapshot::WallpaperSwapper;
use crate::ui::{self, TimerId, TrackedTimers};
use crate::win::{cover, host, util, watcher::SystemWatcher};
use crate::win::ffi as f;
use crate::wallpaper_logic::{self as logic, HideReason, State};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

const EVAL_DEBOUNCE_MS: u32 = 150;
const EVAL_SETTLE_MS: u32 = 1500;
const POLL_VISIBLE_MS: u32 = 1000;
const POLL_HIDDEN_MS: u32 = 2000;
const RESUME_TIMEOUT_MS: u32 = 3000;

/// UI 쪽으로 문자열을 알리는 콜백 (오류 문구 / 상태 문구 / 사라진 영상 경로).
type Callback = Arc<dyn Fn(String) + Send + Sync>;
const RESTORE_FAILED: &str = "Unable to restore the original Windows wallpaper (will retry next launch)";
pub type DoneCallback = Box<dyn FnOnce() + Send + 'static>;

pub struct Wallpaper {
    ui_hwnd: f::HWND,
    timers: TrackedTimers,
    settings: Arc<Mutex<Settings>>,
    swapper: WallpaperSwapper,
    self_weak: Weak<Mutex<Wallpaper>>,
    hwnd: f::HWND,
    path: Option<String>,
    player: Option<Player>,
    stopping: bool,
    after_stop: Vec<DoneCallback>,
    start_gen: u64,
    watcher: Option<SystemWatcher>,
    event_driven: bool,
    display_on: bool,
    /// 재생/멈춤/정적/재개 상태 (표는 `wallpaper_logic::State` 참고).
    state: State,
    /// 정적 전환이 불가능한 이유 (있으면 다시 시도하지 않는다)
    snapshot_off: Option<String>,
    /// 워커 스레드에서 첫 프레임을 만드는 중인 작업 번호 (없으면 None). 오래된 결과를 걸러내는 데 쓴다.
    frame_job: Option<u64>,
    frame_seq: u64,
    check_posted: AtomicBool,
    on_error: Callback,
    /// 바탕화면 창에 붙이지 못했을 때 (부팅 직후 Progman 이 아직 없을 수 있다). 재생 오류가 아니므로 따로 알린다.
    on_attach_failed: Callback,
    on_status: Callback,
    on_lost: Callback,
}
// State is only mutated while its Mutex is held on the UI thread. Native callbacks only post jobs.
unsafe impl Send for Wallpaper {}

pub type WallpaperHandle = Arc<Mutex<Wallpaper>>;

impl Wallpaper {
    pub fn new(ui_hwnd: f::HWND, settings: Arc<Mutex<Settings>>,
               on_error: impl Fn(String) + Send + Sync + 'static,
               on_attach_failed: impl Fn(String) + Send + Sync + 'static,
               on_status: impl Fn(String) + Send + Sync + 'static,
               on_lost: impl Fn(String) + Send + Sync + 'static) -> WallpaperHandle {
        let swapper = WallpaperSwapper::new(config::wallpaper_state_path(),
            || util::get_desktop_wallpaper(), |p| util::set_desktop_wallpaper(p));
        let wall = Arc::new(Mutex::new(Self {
            ui_hwnd, timers: TrackedTimers::new(ui_hwnd), settings, swapper, self_weak: Weak::new(),
            hwnd: std::ptr::null_mut(), path: None, player: None, stopping: false, after_stop: Vec::new(), start_gen: 0, watcher: None,
            event_driven: false, display_on: true, state: State::Playing, snapshot_off: None,
            frame_job: None, frame_seq: 0,
            check_posted: AtomicBool::new(false),
            on_error: Arc::new(on_error), on_attach_failed: Arc::new(on_attach_failed), on_status: Arc::new(on_status), on_lost: Arc::new(on_lost),
        }));
        wall.with_lock(|state| state.self_weak = Arc::downgrade(&wall));
        wall
    }

    /// 원래 Windows 바탕화면으로 되돌린다. 실패하면 상태 표시줄에 알리고 `false` (다음 실행 때 다시 시도).
    fn restore_original(&self) -> bool {
        let restored = self.swapper.restore();
        if !restored { (self.on_status)(RESTORE_FAILED.into()); }
        restored
    }

    pub fn recover_previous_session(&mut self) { self.restore_original(); }

    pub fn active(&self) -> bool { self.player.is_some() }
    pub fn path(&self) -> Option<String> { self.player.as_ref().and(self.path.clone()) }

    pub fn start(handle: &WallpaperHandle, path: String) {
        let weak = Arc::downgrade(handle);
        { let mut state = handle.guard();
            // Latest start wins: an older queued start (or one cancelled by stop) is skipped.
            state.start_gen += 1;
            let generation = state.start_gen;
            let next: DoneCallback = Box::new(move || {
                if let Some(w) = weak.upgrade() {
                    { let mut s = w.guard(); if s.start_gen == generation { s.start_inner(path); } }
                }
            });
            state.stop_then(Some(next));
        }
    }

    fn start_inner(&mut self, path: String) {
        if path.is_empty() || self.stopping { return; }
        let hwnd = host::create_host_window();
        // 창을 만들거나 붙이지 못한 것은 "재생 오류"가 아니다. 부팅 직후에는 Progman 이 아직 없을 수 있어,
        // 앱이 조용히 다시 시도할 수 있도록 별도 콜백으로 알린다 (`App::on_attach_failed`).
        if hwnd.is_null() { (self.on_attach_failed)("Unable to create the desktop host window.".into()); return; }
        let (ok, attach_message) = host::attach_to_desktop(hwnd);
        if !ok { host::destroy_window(hwnd); (self.on_attach_failed)(attach_message); return; }
        self.hwnd = hwnd;
        self.path = Some(path.clone());
        let settings = self.settings.with_lock(|s| s.clone());
        let error_cb = self.on_error.clone();
        let player = Player::new(hwnd as isize, settings.fit_mode, &settings.hwdec, move |text| error_cb(text));
        player.play(&path);
        self.player = Some(player);
        self.display_on = true; self.state = State::Playing; self.snapshot_off = None;
        self.event_driven = false; self.check_posted.store(false, Ordering::Release);

        let ui_hwnd = self.ui_hwnd as usize;
        let (weak_display, weak_change) = (self.self_weak.clone(), self.self_weak.clone());
        // At most one "something changed" job in flight, however many window events arrive.
        let change_pending = Arc::new(AtomicBool::new(false));
        let watcher = SystemWatcher::new(
            move |on| { ui::post_to(ui_hwnd, weak_display.clone(), move |s| { s.display_on = on; s.request_check(); }); },
            move || {
                if change_pending.swap(true, Ordering::AcqRel) { return; }
                let pending = change_pending.clone();
                let posted = ui::post_to(ui_hwnd, weak_change.clone(), move |s| {
                    pending.store(false, Ordering::Release);
                    s.request_check();
                });
                if !posted { change_pending.store(false, Ordering::Release); }
            },
        );
        match watcher {
            Ok(w) if w.ok() => { self.event_driven = true; self.watcher = Some(w); }
            Ok(mut w) => { w.close(); self.watcher = None; }
            Err(_) => { self.watcher = None; }
        }
        self.evaluate();
        if !self.event_driven { self.arm_poll(POLL_VISIBLE_MS); }
        if !self.state.is_hidden() { (self.on_status)("Playing".into()); }
    }

    fn stop_then(&mut self, after: Option<DoneCallback>) {
        // Overlapping requests are queued and all run, in order, once the stop completes.
        // Stale start requests are filtered by `start_gen`, so no callback is silently lost.
        if let Some(cb) = after { self.after_stop.push(cb); }
        if self.stopping { return; }
        self.stopping = true;
        self.timers.kill_all();
        if let Some(mut watcher) = self.watcher.take() { watcher.close(); }
        self.event_driven = false;
        self.path = None;
        self.state = State::Playing; self.snapshot_off = None; self.frame_job = None;
        let weak = self.self_weak.clone(); let ui_hwnd = self.ui_hwnd as usize;
        let completion: Box<dyn FnOnce() + Send> = Box::new(move || {
            ui::post(ui_hwnd as f::HWND, Box::new(move || {
                let after: Vec<DoneCallback> = if let Some(w) = weak.upgrade() {
                    {
                        let mut s = w.guard();
                        s.stopping = false;
                        s.player = None;
                        s.restore_original();
                        if !s.hwnd.is_null() { host::destroy_window(s.hwnd); s.hwnd = std::ptr::null_mut(); }
                        (s.on_status)(String::new());
                        std::mem::take(&mut s.after_stop)
                    }
                } else { Vec::new() };
                for done in after { done(); }
            }));
        });
        if let Some(player) = self.player.as_ref() { player.stop_then(Some(completion)); }
        else { completion(); }
    }

    /// Explicit stops cancel any start that is still waiting for the previous stop.
    pub fn stop(&mut self) { self.start_gen += 1; self.stop_then(None); }
    pub fn stop_with_done(&mut self, callback: DoneCallback) { self.start_gen += 1; self.stop_then(Some(callback)); }

    pub fn request_check(&self) {
        if self.check_posted.swap(true, Ordering::AcqRel) { return; }
        let posted = ui::post_to(self.ui_hwnd as usize, self.self_weak.clone(), |s| {
            s.check_posted.store(false, Ordering::Release);
            s.schedule_check();
        });
        if !posted { self.check_posted.store(false, Ordering::Release); }
    }

    fn schedule_check(&mut self) {
        if self.stopping || self.player.is_none() || self.timers.is_armed(TimerId::Eval) { return; }
        self.timers.set(TimerId::Eval, EVAL_DEBOUNCE_MS);
    }

    pub fn handle_timer(&mut self, timer_id: usize) {
        let id = match TimerId::from_usize(timer_id) { Some(id) => id, None => return };
        match id {
            TimerId::Eval => { self.timers.kill(id); self.check_now(); self.timers.set(TimerId::Settle, EVAL_SETTLE_MS); }
            TimerId::Settle => { self.timers.kill(id); self.check_now(); }
            TimerId::Snapshot => { self.timers.kill(id); self.maybe_enter_static(); self.arm_snapshot_timer(); }
            TimerId::Resume => { self.timers.kill(id); self.finish_resume(); }
            TimerId::Poll => { self.timers.kill(id); self.tick(); }
            _ => {}
        }
    }

    fn check_now(&mut self) {
        if self.stopping || self.player.is_none() { return; }
        if !host::host_alive(self.hwnd) {
            let path = self.path.clone();
            let callback = self.on_lost.clone();
            self.stop_then(Some(Box::new(move || if let Some(path) = path { callback(path) })));
            return;
        }
        host::refit_host(self.hwnd); // resolution / DPI change: keep the host window the size of the desktop
        self.evaluate();
        self.maybe_enter_static();
        self.arm_snapshot_timer();
    }

    fn tick(&mut self) {
        self.check_now();
        if self.player.is_some() {
            // 재생 중이 아니면(멈춤/정적/재개 중 모두) 느린 간격. 예전의 `hidden || is_static`과 같다.
            self.arm_poll(if self.state.is_playing() { POLL_VISIBLE_MS } else { POLL_HIDDEN_MS });
        }
    }

    fn arm_poll(&mut self, ms: u32) { self.timers.set(TimerId::Poll, ms); }

    /// 판단 순서는 `wallpaper_logic::hidden_reason`에 있다. 여기서는 Win32 조회만 연결한다.
    fn hidden_reason(&self) -> Option<HideReason> {
        let (pause_on_battery_saver, coverage_threshold) =
            self.settings.with_lock(|s| (s.pause_on_battery_saver, s.coverage_threshold));
        logic::hidden_reason(
            self.display_on,
            cover::notification_blocked,
            pause_on_battery_saver,
            cover::battery_saver_on,
            || cover::desktop_is_covered(coverage_threshold),
        )
    }

    /// 지금 정적 전환을 시도해도 되는 상태인가 (`logic::static_possible`에 현재 필드를 넘긴다).
    fn static_possible(&self, static_snapshot: bool) -> bool {
        logic::static_possible(self.player.is_some(), self.state.is_hidden(), self.state.is_static(),
            self.state.is_resuming(), static_snapshot, self.snapshot_off.is_some())
    }

    fn reason_text(&self) -> &'static str { self.state.reason_text() }

    fn evaluate(&mut self) {
        if self.stopping || self.player.is_none() || self.state.is_resuming() { return; }
        let reason = self.hidden_reason();
        let hidden = reason.is_some();
        if self.state.is_static() {
            let snapshot = self.settings.with_lock(|s| s.static_snapshot);
            if logic::should_leave_static(hidden, snapshot) { self.begin_resume(); }
            return;
        }
        // 여기서는 Playing 또는 Paused뿐이다. 이미 멈춰 있으면 이유가 바뀌어도 처음 이유를 유지한다.
        if hidden == self.state.is_hidden() { return; }
        if let Some(reason) = reason {
            self.state = State::Paused { since: Instant::now(), reason };
            if let Some(player) = &self.player { player.pause(); }
            (self.on_status)(format!("Paused ({})", reason.text()));
            self.arm_snapshot_timer();
        } else {
            self.state = State::Playing;
            self.timers.kill_if_armed(TimerId::Snapshot);
            if let Some(player) = &self.player { player.resume(None); }
            (self.on_status)("Playing".into());
        }
    }

    fn arm_snapshot_timer(&mut self) {
        let settings = self.settings.with_lock(|s| s.clone());
        if self.frame_job.is_some() || self.timers.is_armed(TimerId::Snapshot) || !self.static_possible(settings.static_snapshot) { return; }
        let since = match self.state.paused_since() { Some(t) => t, None => return };
        let ms = logic::snapshot_timer_ms(settings.snapshot_delay_s, since.elapsed());
        self.timers.set(TimerId::Snapshot, ms);
    }

    fn maybe_enter_static(&mut self) {
        if self.frame_job.is_some() { return; }
        let settings = self.settings.with_lock(|s| s.clone());
        if !self.static_possible(settings.static_snapshot) { return; }
        let since = match self.state.paused_since() { Some(t) => t, None => return };
        if !logic::snapshot_due(settings.snapshot_delay_s, since.elapsed()) { return; }
        self.enter_static(&settings);
    }

    /// 정적 전환을 시작한다. UI 스레드에서는 ffmpeg 를 실행하지 않는다:
    /// 첫 프레임이 이미 cache 에 있으면 바로 적용하고, 없으면 워커 스레드에서 만든 뒤 `on_frame_ready` 로 이어 간다.
    fn enter_static(&mut self, settings: &Settings) {
        self.timers.kill(TimerId::Snapshot);
        if self.frame_job.is_some() { return; }
        if util::monitor_count() != 1 {
            self.snapshot_off = Some("Static wallpaper supports a single monitor only".into()); return;
        }
        let Some(path) = self.path.clone() else {
            self.snapshot_off = Some("No video is loaded".into()); return;
        };
        let (w, h) = util::screen_size();
        let (w, h, fit) = (w.max(0) as u32, h.max(0) as u32, settings.fit_mode);
        if let Some(frame) = media::cached_first_frame(&path, w, h, fit) {
            self.finish_enter_static(&frame);
            return;
        }
        self.frame_seq += 1;
        let job = self.frame_seq;
        self.frame_job = Some(job);
        (self.on_status)(format!("Paused ({}) · preparing static frame...", self.reason_text()));
        let weak = self.self_weak.clone();
        let ui_hwnd = self.ui_hwnd as usize;
        let spawned = std::thread::Builder::new().name("first-frame".into()).spawn(move || {
            // A panic must not leave the job marked as running forever.
            let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| media::ensure_first_frame(&path, w, h, fit)))
                .unwrap_or(None);
            ui::post_to(ui_hwnd, weak, move |wall| wall.on_frame_ready(job, path, w, h, fit, frame));
        });
        if spawned.is_err() {
            self.frame_job = None;
            self.snapshot_off = Some("Unable to start the first-frame worker".into());
        }
    }

    /// 워커가 끝났을 때 (UI 스레드). 그 사이 영상·설정·상태가 바뀌었으면 결과를 버리고 필요하면 다시 예약한다.
    fn on_frame_ready(&mut self, job: u64, path: String, w: u32, h: u32, fit: FitMode, frame: Option<String>) {
        if self.frame_job != Some(job) { return; }
        self.frame_job = None;
        if self.stopping || self.player.is_none() { return; }
        let settings = self.settings.with_lock(|s| s.clone());
        let (cw, ch) = util::screen_size();
        let target_current = self.path.as_deref() == Some(path.as_str())
            && settings.fit_mode == fit
            && (cw.max(0) as u32, ch.max(0) as u32) == (w, h);
        if !target_current { self.arm_snapshot_timer(); return; }
        let Some(frame) = frame else {
            let msg = "Unable to extract the first frame (is ffmpeg available?)";
            self.snapshot_off = Some(msg.into());
            if self.state.is_hidden() { (self.on_status)(format!("Paused ({}) · {}", self.reason_text(), msg)); }
            return;
        };
        let paused_since = self.state.paused_since();
        let due = paused_since.map_or(false, |t| logic::snapshot_due(settings.snapshot_delay_s, t.elapsed()));
        if logic::should_apply_frame(target_current, paused_since.is_some(), due)
            && self.static_possible(settings.static_snapshot)
            && util::monitor_count() == 1 {
            self.finish_enter_static(&frame);
        } else {
            self.arm_snapshot_timer();
        }
    }

    /// 첫 프레임을 바탕화면으로 깔고 mpv 를 끈다 (예전 `enter_static` 의 뒤쪽 절반).
    fn finish_enter_static(&mut self, frame: &str) {
        if let Err(why) = self.swapper.enter(frame) {
            self.snapshot_off = Some(why.clone());
            (self.on_status)(format!("Paused ({}) · static wallpaper unavailable: {}", self.reason_text(), why)); return;
        }
        // 이유 문구는 Paused 상태에만 있으므로, 상태를 바꾸기 전에 꺼내 둔다.
        let reason_text = self.reason_text();
        host::hide_window(self.hwnd);
        self.state = State::Static { stopping_mpv: true };
        let weak = self.self_weak.clone();
        let ui_hwnd = self.ui_hwnd as usize;
        if let Some(player) = &self.player {
            player.suspend_then(Some(Box::new(move || {
                ui::post_to(ui_hwnd, weak, |wall| wall.on_mpv_stopped());
            })));
        } else {
            self.state = State::Static { stopping_mpv: false };
        }
        (self.on_status)(format!("Static first frame ({})", reason_text));
    }

    /// mpv 종료가 끝났을 때 (정적 전환 직후 콜백). 재개를 기다리던 중이었다면 이제 mpv를 다시 띄운다.
    fn on_mpv_stopped(&mut self) {
        let (next, restart) = self.state.mpv_stopped();
        self.state = next;
        if restart { self.start_resume(); }
        // mpv is gone and the static frame is up: give the freed pages back (Task Manager shows the working set).
        else if matches!(next, State::Static { stopping_mpv: false }) { util::trim_working_set(); }
    }

    fn begin_resume(&mut self) {
        if self.state.is_resuming() || self.player.is_none() { return; }
        self.state = self.state.begin_resume();
        self.timers.kill_if_armed(TimerId::Snapshot);
        (self.on_status)("Resuming...".into());
        // Do not create a new mpv instance until the static-mode instance has fully terminated.
        if !self.state.waiting_mpv() { self.start_resume(); }
    }

    fn start_resume(&mut self) {
        if !self.state.is_resuming() || self.state.waiting_mpv() || self.timers.is_armed(TimerId::Resume) || self.player.is_none() { return; }
        let weak = self.self_weak.clone(); let ui_hwnd = self.ui_hwnd as usize;
        if let Some(player) = &self.player {
            player.resume(Some(Box::new(move || { ui::post_to(ui_hwnd, weak, |s| s.finish_resume()); })));
        }
        self.timers.set(TimerId::Resume, RESUME_TIMEOUT_MS);
    }

    fn finish_resume(&mut self) {
        if !self.state.is_resuming() || self.player.is_none() { return; }
        self.timers.kill_if_armed(TimerId::Resume);
        host::show_window(self.hwnd);
        if self.restore_original() { (self.on_status)("Playing".into()); }
        self.state = State::Playing;
        self.request_check();
    }

    pub fn refresh(&self) { self.request_check(); }
    pub fn set_fit_mode(&self, mode: FitMode) { if let Some(player) = &self.player { player.set_fit_mode(mode); } }
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        // Defensive only: the normal path is stop() / quit_app().
        self.timers.kill_all();
        if let Some(mut watcher) = self.watcher.take() { watcher.close(); }
    }
}
