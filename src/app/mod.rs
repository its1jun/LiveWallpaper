//! Native settings application and orchestration.
#![cfg(windows)]

pub mod window;

use crate::sync::LockExt;
use crate::config::{Codec, FitMode, Settings};
use crate::media::{self, PrepareResult};
use crate::player::Player;
use crate::ui::{self, TimerId, TimerSet};
use crate::wallpaper::{Wallpaper, WallpaperHandle};
use crate::win::consts::*;
use crate::win::{autostart, ffi as f, tray::{MenuItem, Tray}, util};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use window::{MainWindow, ID_APPLY, ID_AUTOSTART, ID_BATTERY, ID_CODEC, ID_FIT, ID_MORE, ID_OPTIMIZE,
    ID_RESTORE, ID_SELECT, ID_STATIC, ID_STOP};

const DIAGNOSE_HINT: &str = "If the error persists, run diagnose-video.bat from the project folder and check the FFmpeg/libmpv runtime files.";

/// 시작 실패를 조용히 다시 시도하는 최대 횟수 (5초 간격 → 약 2분).
const RETRY_MAX: u32 = 24;

/// UI 창 핸들(정수). `App` 잠금 없이 `post_app` 이 UI 스레드로 작업을 넘길 때 쓴다.
static UI_HWND: AtomicUsize = AtomicUsize::new(0);
/// 파일 선택 흐름이 진행 중인가 (버튼을 연달아 눌러도 창이 겹쳐 뜨지 않게 한다).
static PICKING: AtomicBool = AtomicBool::new(false);
/// 종료가 정상적으로 끝났는가 (종료 워치독이 보는 값).
static QUIT_DONE: AtomicBool = AtomicBool::new(false);
/// 종료 요청 뒤 이 시간 안에 mpv 정리가 끝나지 않으면 원래 바탕화면을 되돌리고 강제로 끝낸다.
const QUIT_TIMEOUT_SECS: u64 = 8;

/// mpv/GPU 드라이버가 destroy 에서 멈춰 "Exit" 가 끝나지 않는 경우를 막는 안전장치.
/// 정상 종료는 `finish_quit` 이 `QUIT_DONE` 을 켜므로 이 스레드는 아무것도 하지 않고 프로세스와 함께 사라진다.
fn arm_quit_watchdog() {
    let _ = thread::Builder::new().name("LiveWallpaper-quit-watchdog".into()).spawn(|| {
        thread::sleep(std::time::Duration::from_secs(QUIT_TIMEOUT_SECS));
        if QUIT_DONE.load(Ordering::Acquire) { return; }
        // Same restore the uninstaller uses: only touches the wallpaper if it is still ours.
        crate::snapshot::WallpaperSwapper::new(
            crate::config::wallpaper_state_path(),
            || util::get_desktop_wallpaper(),
            |p| util::set_desktop_wallpaper(p),
        ).restore();
        std::process::exit(0);
    });
}

/// 파일 선택 흐름이 어떻게 끝나든(반환, 패닉) 진행 표시를 지운다.
struct PickingGuard;
impl Drop for PickingGuard { fn drop(&mut self) { PICKING.store(false, Ordering::Release); } }

pub type AppHandle = Arc<Mutex<App>>;

pub struct App {
    self_weak: Weak<Mutex<App>>,
    settings: Arc<Mutex<Settings>>,
    window: Option<MainWindow>,
    wall: Option<WallpaperHandle>,
    preview: Option<Player>,
    tray: Option<Tray>,
    preview_path: Option<String>,
    busy: bool,
    quit_pending: bool,
    retry_path: Option<String>,
    retry_attempt: u32,
}
// Win32 handles are only touched under this mutex on the UI thread. Worker threads only enqueue jobs.
unsafe impl Send for App {}

impl App {
    pub fn create(settings: Settings) -> Result<AppHandle, String> {
        let settings = Arc::new(Mutex::new(settings));
        let app = Arc::new(Mutex::new(Self {
            self_weak: Weak::new(), settings: settings.clone(), window: None, wall: None,
            preview: None, tray: None, preview_path: None, busy: false,
            quit_pending: false, retry_path: None, retry_attempt: 0,
        }));
        app.with_lock(|a| a.self_weak = Arc::downgrade(&app));
        let weak = Arc::downgrade(&app);
        let handler: crate::win::host::WindowHandler = Arc::new(move |hwnd, msg, wp, lp| {
            if msg == ui::WM_APP_RUN { return Some(ui::dispatch_run(lp)); }
            let app = match weak.upgrade() { Some(a) => a, None => return None };
            let result = match app.try_lock() {
                Ok(mut a) => a.handle_message(hwnd, msg, wp, lp),
                Err(_) => None, // A synchronous Win32 re-entry must never block on the UI-state mutex.
            };
            result
        });
        let window = MainWindow::create(handler)?;
        let hwnd = window.hwnd;
        // HWND is an opaque Win32 handle. Keep its bit-pattern in an integer in
        // Send + Sync callbacks, then convert it back only at the PostMessage boundary.
        let hwnd_value = hwnd as usize;
        UI_HWND.store(hwnd_value, Ordering::Release);
        {
            let mut a = app.guard();
            a.window = Some(window);
            let weak = a.self_weak.clone();
            a.wall = Some(Wallpaper::new(hwnd, settings.clone(),
                ui_callback(hwnd_value, &weak, App::on_playback_error),
                ui_callback(hwnd_value, &weak, App::on_attach_failed),
                ui_callback(hwnd_value, &weak, App::on_status),
                ui_callback(hwnd_value, &weak, App::auto_apply)));
            let (fit_mode, hwdec) = settings.with_lock(|s| (s.fit_mode, s.hwdec.clone()));
            let preview_wid = a.window.as_ref().map_or(0, |w| w.preview as isize);
            a.preview = Some(Player::new(preview_wid, fit_mode, &hwdec,
                ui_callback(hwnd_value, &weak, App::on_preview_error)));
            a.initialize_controls();
            a.build_tray();
        }
        Ok(app)
    }

    fn hwnd(&self) -> f::HWND { self.window.as_ref().map(|w| w.hwnd).unwrap_or(std::ptr::null_mut()) }
    fn timers(&self) -> TimerSet { TimerSet::new(self.hwnd()) }

    /// 오류 창을 띄운다. 모달 창은 메시지 루프를 돌리므로 `App` 잠금을 잡은 채 열면, 그 사이 도착한
    /// UI 작업(트레이 메뉴, 두 번째 실행, mpv 알림)이 같은 잠금을 기다려 UI 스레드가 영구 정지한다.
    /// 그래서 창은 지금 바로 열지 않고, 잠금이 풀린 뒤 UI 스레드가 실행할 작업으로 예약한다.
    fn error_later(&self, title: &str, text: &str) { show_error_later(self.hwnd() as usize, title, text); }

    fn initialize_controls(&self) {
        if let Some(w) = &self.window {
            { let s = self.settings.guard();
                w.set_fit_mode(s.fit_mode);
                w.set_codec(s.video_codec);
            }
            w.set_path(&self.path_label());
            w.set_stop_enabled(false);
        }
    }

    fn build_tray(&mut self) {
        let weak_menu = self.self_weak.clone();
        let weak_click = self.self_weak.clone();
        let icon = crate::config::icon_path();
        let tray = Tray::new("LiveWallpaper", icon.as_deref(),
            move || make_tray_menu(weak_menu.clone()),
            move || post_app(weak_click.clone(), |app| app.show_window()));
        self.tray = tray.ok();
    }

    fn handle_message(&mut self, _hwnd: f::HWND, msg: u32, wp: usize, lp: isize) -> Option<isize> {
        match msg {
            WM_CLOSE => { self.close_window(); Some(0) }
            WM_DESTROY => { unsafe { f::PostQuitMessage(0); } Some(0) }
            WM_SIZE => {
                if let Some(w) = &self.window { w.layout((lp as u32 & 0xffff) as i32, ((lp as u32 >> 16) & 0xffff) as i32); }
                // Minimized: release the preview mpv entirely (matches the Python build's Unmap handling).
                // Any other size change resumes only while the window is actually showing, so a
                // DPI/resolution change on a tray-hidden window cannot recreate the preview mpv.
                if wp == 1 { self.suspend_preview(); }
                else if self.window.as_ref().map_or(false, |w| w.is_showing()) { self.resume_preview(); }
                Some(0)
            }
            WM_GETMINMAXINFO => {
                if lp != 0 {
                    unsafe {
                        let info = &mut *(lp as *mut f::MINMAXINFO);
                        let (min_w, min_h) = window::min_track_size();
                        info.ptMinTrackSize.x = min_w;
                        info.ptMinTrackSize.y = min_h;
                    }
                }
                Some(0)
            }
            WM_COMMAND => { self.on_command(wp); Some(0) }
            WM_TIMER => { self.on_timer(wp); Some(0) }
            _ => None,
        }
    }

    fn on_command(&mut self, wp: usize) {
        let id = wp & 0xffff; let notify = (wp >> 16) & 0xffff;
        match id {
            ID_SELECT if notify == 0 => self.pick(),
            ID_APPLY if notify == 0 => self.apply(),
            ID_STOP if notify == 0 => self.stop_wallpaper(),
            ID_MORE if notify == 0 => { if let Some(w) = self.window.as_mut() { w.toggle_more(); } }
            ID_FIT if notify == 1 => self.change_fit(),
            ID_CODEC if notify == 1 => self.change_codec(),
            ID_OPTIMIZE | ID_BATTERY | ID_RESTORE | ID_STATIC if notify == 0 => self.toggle_setting(id),
            ID_AUTOSTART if notify == 0 => self.toggle_autostart(),
            _ => {}
        }
    }

    fn on_timer(&mut self, id: usize) {
        let timer = TimerId::from_usize(id);
        if timer == Some(TimerId::Retry) {
            self.timers().kill(TimerId::Retry);
            if self.retry_path.is_none() { return; }
            // Like the Python build: retry only while the wallpaper failed to START (no player yet).
            // "mpv is playing" is false in static mode and until mpv's first frame (slow at login),
            // which made this timer tear the wallpaper down and restart mpv up to 24 times.
            let ready = self.wall.as_ref().map_or(false, |w| w.with_lock(|s| s.active()));
            if ready {
                self.retry_path = None;
                self.retry_attempt = 0;
                return;
            }
            if let (Some(path), true) = (self.retry_path.clone(), self.retry_attempt < RETRY_MAX) {
                self.retry_attempt += 1;
                self.start_wallpaper(&path);
                self.timers().set(TimerId::Retry, 5000);
            } else {
                self.retry_path = None;
                self.error_later("Apply failed", "Unable to start the wallpaper after multiple attempts.");
                self.refresh_buttons();
            }
            return;
        }
        if timer == Some(TimerId::CloseHint) {
            self.timers().kill(TimerId::CloseHint);
            unsafe { f::SetWindowPos(self.hwnd(), HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE); }
            return;
        }
        if let Some(wall) = &self.wall { wall.with_lock(|w| w.handle_timer(id)); }
    }

    /// Start (or restart from the beginning) the preview if there is a file to show.
    fn resume_preview(&self) {
        if self.preview_path.is_none() { return; }
        if let Some(p) = &self.preview { p.resume(None); }
    }

    /// Fully terminate the preview mpv to free decoder/GPU/file resources while the window is hidden
    /// or minimized. Termination is asynchronous: a resume requested while it is still running is
    /// ignored by the player, so when it finishes we restart the preview if the window is showing again.
    fn suspend_preview(&self) {
        let Some(p) = &self.preview else { return; };
        let weak = self.self_weak.clone();
        // Callers hold the App mutex, and the callback runs synchronously when no mpv session exists.
        // Capture the HWND now and use ui::post_to: post_app would lock the App mutex again (deadlock).
        let hwnd = self.hwnd() as usize;
        p.suspend_then(Some(Box::new(move || {
            crate::ui::post_to(hwnd, weak, |app| {
                if app.quit_pending { return; }
                if app.window.as_ref().map_or(false, |w| w.is_showing()) { app.resume_preview(); }
            });
        })));
    }

    fn show_window(&self) {
        if let Some(w) = &self.window {
            w.show();
            self.resume_preview();
            unsafe { f::SetWindowPos(w.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE); }
            TimerSet::new(w.hwnd).set(TimerId::CloseHint, 400);
        }
    }

    fn close_window(&mut self) {
        let tray_ok = self.tray.as_ref().map(|t| t.ok()).unwrap_or(false);
        if !tray_ok { self.quit_app(); return; }
        if let Some(w) = &self.window { w.hide(); }
        self.suspend_preview();
        if self.wall.as_ref().map_or(false, |w| w.with_lock(|s| s.active())) {
            if let Some(t) = &self.tray { t.notify("Still running in the system tray.", "LiveWallpaper"); }
        }
    }

    fn quit_app(&mut self) {
        if self.quit_pending { return; }
        self.quit_pending = true;
        arm_quit_watchdog();
        crate::media::cancel_all(); // kill any running ffmpeg so it is not orphaned
        self.retry_path = None;
        { let t = self.timers(); t.kill(TimerId::Retry); t.kill(TimerId::CloseHint); }
        let weak = self.self_weak.clone();
        let callback: Box<dyn FnOnce() + Send> = Box::new(move || {
            post_app(weak.clone(), |app| {
                let weak2 = app.self_weak.clone();
                // This job holds the App mutex and the callback runs synchronously when the preview has
                // no mpv session (window hidden in the tray): post_app would deadlock here.
                let hwnd = app.hwnd() as usize;
                if let Some(preview) = &app.preview {
                    preview.stop_then(Some(Box::new(move || { ui::post_to(hwnd, weak2, |a| a.finish_quit()); })));
                } else { app.finish_quit(); }
            });
        });
        if let Some(wall) = self.wall.clone() {
            wall.with_lock(|wallpaper| wallpaper.stop_with_done(callback));
        } else {
            callback();
        }
    }

    fn finish_quit(&mut self) {
        QUIT_DONE.store(true, Ordering::Release);
        if let Some(t) = self.tray.as_mut() { t.close(); }
        if let Some(w) = &self.window { unsafe { f::DestroyWindow(w.hwnd); } }
        // finish_quit runs with the App mutex held, so the synchronous WM_DESTROY sent by
        // DestroyWindow cannot reach handle_message (try_lock fails). Post WM_QUIT explicitly.
        unsafe { f::PostQuitMessage(0); }
    }

    /// 파일 선택 → 엔진 확인 → 크기 경고는 모두 모달 창이므로 `App` 잠금 밖(`pick_flow`)에서 한다.
    /// 여기서는 필요한 값만 챙겨 UI 작업으로 예약하고 곧바로 잠금을 푼다.
    fn pick(&mut self) {
        if PICKING.swap(true, Ordering::AcqRel) { return; }
        // A stale config path may point to a folder that has since been moved or
        // removed. Do not pass a non-existent lpstrInitialDir to GetOpenFileNameW.
        let initial_dir = self.settings.with_lock(|s| {
            Path::new(&s.video_path).parent()
                .filter(|p| p.is_dir())
                .map(|p| p.to_string_lossy().into_owned())
        });
        let warning_mb = self.settings.with_lock(|s| s.size_warning_mb);
        let hwnd = self.hwnd();
        let owner = hwnd as usize;
        let weak = self.self_weak.clone();
        let posted = ui::post(hwnd, Box::new(move || {
            let _done = PickingGuard;
            pick_flow(owner, weak, initial_dir, warning_mb);
        }));
        if !posted { PICKING.store(false, Ordering::Release); }
    }

    fn apply(&mut self) {
        self.retry_path = None;
        self.timers().kill(TimerId::Retry);
        let Some(path) = self.source() else {
            let s = self.settings.with_lock(|s| s.clone());
            if s.video_path.is_empty() && s.media_path.is_empty() {
                self.error_later("Notice", "Please select a video first.");
            } else {
                self.error_later("Apply failed", "Video file not found. Both the original and the saved copy are missing. Please select the video again.");
            }
            return;
        };
        self.begin_prepare(path, true, false);
    }

    pub fn has_source(&self) -> bool { self.source().is_some() }

    fn source(&self) -> Option<String> {
        let s = self.settings.guard();
        let found = [&s.video_path, &s.media_path].into_iter().find(|p| !p.is_empty() && Path::new(p.as_str()).is_file()).cloned();
        found
    }

    fn path_label(&self) -> String {
        { let s = self.settings.guard();
            if !s.video_path.is_empty() && Path::new(&s.video_path).is_file() { return s.video_path.clone(); }
            if !s.media_path.is_empty() && Path::new(&s.media_path).is_file() {
                let name = Path::new(&s.video_path).file_name().and_then(|v| v.to_str()).unwrap_or("Saved video");
                return format!("{name}\n(Original missing · playing saved copy)");
            }
        }
        "No video selected".into()
    }

    fn update_path_label(&self) { if let Some(w) = &self.window { w.set_path(&self.path_label()); } }

    fn begin_prepare(&mut self, path: String, apply_after: bool, picked: bool) {
        if self.busy || path.is_empty() { return; }
        self.busy = true;
        if let Some(w) = &self.window { w.set_busy(true); w.set_path(&format!("{path}\n(Preparing video...)")); }
        let hwnd = self.hwnd() as usize; let weak = self.self_weak.clone();
        let settings = self.settings.with_lock(|s| s.clone());
        let (width, height) = util::screen_size();
        let screen = (width.max(0) as u32, height.max(0) as u32);
        let spawn_result = thread::Builder::new().name("LiveWallpaper-prepare".into()).spawn(move || {
            let result = media::prepare_for_playback(&path, &settings, screen);
            ui::post_to(hwnd, weak, move |app| match result {
                Ok(prepared) => app.on_prepared(path, prepared, apply_after, picked),
                Err(error) => app.on_prepare_failed(&path, &error.0),
            });
        });
        if let Err(error) = spawn_result {
            self.busy = false;
            if let Some(w) = &self.window { w.set_busy(false); }
            self.error_later("Video preparation failed", &error.to_string());
        }
    }

    fn on_prepare_failed(&mut self, path: &str, error: &str) {
        self.busy = false;
        if let Some(w) = &self.window { w.set_busy(false); w.set_path(&self.path_label()); }
        let name = Path::new(path).file_name().and_then(|v| v.to_str()).unwrap_or(path);
        self.error_later("Video preparation failed", &format!("{name}\n{error}"));
    }

    fn on_prepared(&mut self, source: String, result: PrepareResult, apply_after: bool, picked: bool) {
        self.busy = false;
        { let mut s = self.settings.guard();
            let changed = picked || s.media_path != result.path;
            if picked { s.video_path = source; }
            s.media_path = result.path.clone();
            if changed { s.save(); }
        }
        if let Some(w) = &self.window { w.set_busy(false); w.set_path(&self.path_label()); }
        if self.preview_path.as_deref() != Some(result.path.as_str()) {
            self.preview_path = Some(result.path.clone());
            if let Some(p) = &self.preview {
                // is_showing (not is_visible): a minimized window is still "visible" to IsWindowVisible.
                let visible = self.window.as_ref().map(|w| w.is_showing()).unwrap_or(false);
                if visible { p.play(&result.path); } else { p.load(&result.path); }
            }
        }
        if let Some(note) = &result.note { self.on_status(note); }
        if apply_after {
            if self.retry_path.is_some() { self.auto_apply(&result.path); }
            else { self.retry_path = None; self.retry_attempt = 0; self.start_wallpaper(&result.path); }
        }
        self.prune_cache(&result.path);
    }

    fn start_wallpaper(&mut self, path: &str) {
        if let Some(wall) = &self.wall { Wallpaper::start(wall, path.to_string()); }
        self.refresh_buttons();
    }

    fn stop_wallpaper(&mut self) {
        if let Some(wall) = &self.wall { wall.with_lock(|w| w.stop()); }
        self.retry_path = None; self.retry_attempt = 0;
        self.timers().kill(TimerId::Retry);
        self.refresh_buttons(); self.on_status("");
    }

    fn auto_apply(&mut self, path: &str) {
        if !Path::new(path).is_file() {
            self.retry_path = None;
            self.timers().kill(TimerId::Retry);
            self.refresh_buttons();
            return;
        }
        self.retry_path = Some(path.to_string());
        self.retry_attempt = 0;
        self.timers().kill(TimerId::Retry);
        self.start_wallpaper(path);
        self.timers().set(TimerId::Retry, 5000);
    }

    fn on_preview_error(&mut self, text: &str) {
        // Let selecting the same source retry preview after the user fixes a DLL/file issue.
        let failed_path = self.preview_path.take().unwrap_or_default();
        self.update_path_label();
        self.error_later("Preview playback failed", &format!("{text}\n\nPrepared file: {failed_path}\n\n{}", DIAGNOSE_HINT));
    }

    fn on_playback_error(&mut self, text: &str) {
        // A decoder/file-load error is terminal for this attempt. Do not leave a
        // black desktop-host window attached behind the icons after playback fails.
        let failed_path = self.wall.as_ref().and_then(|w| w.with_lock(|s| s.path())).unwrap_or_default();
        self.retry_attempt = 0;
        self.retry_path = None;
        self.timers().kill(TimerId::Retry);
        let weak = self.self_weak.clone();
        if let Some(wall) = &self.wall {
            { let mut state = wall.guard();
                state.stop_with_done(Box::new(move || {
                    post_app(weak, |app| app.refresh_buttons());
                }));
            }
        }
        self.refresh_buttons();
        self.update_path_label();
        self.error_later("Playback failed", &format!("{text}\n\nPrepared file: {failed_path}\n\n{}", DIAGNOSE_HINT));
    }

    /// 바탕화면 창에 붙이지 못했다. 부팅 직후에는 Progman 이 아직 없을 수 있어 (Python 판은 최대 2분을 조용히
    /// 다시 시도했다), 자동 적용 중이면 오류 창 없이 재시도 타이머(`on_timer`)에 맡긴다.
    /// 직접 "적용"을 눌렀거나 재시도를 다 썼다면 사용자에게 알려야 하므로 재생 오류와 같게 처리한다.
    fn on_attach_failed(&mut self, text: &str) {
        if self.retry_path.is_some() && self.retry_attempt < RETRY_MAX {
            self.on_status("Waiting for the desktop...");
            return;
        }
        self.on_playback_error(text);
    }

    fn change_fit(&mut self) {
        let idx = self.window.as_ref().map(|w| w.fit_mode_index()).unwrap_or(0);
        let mode = FitMode::from_index(idx);
        self.settings.with_lock(|s| { s.fit_mode = mode; s.save(); });
        if let Some(p) = &self.preview { p.set_fit_mode(mode); }
        if let Some(wall) = &self.wall { wall.with_lock(|w| w.set_fit_mode(mode)); }
    }

    fn change_codec(&mut self) {
        let idx = self.window.as_ref().map(|w| w.codec_index()).unwrap_or(0);
        let codec = Codec::from_index(idx);
        self.settings.with_lock(|s| { s.video_codec = codec; s.save(); });
    }

    fn toggle_setting(&mut self, id: usize) {
        { let mut s = self.settings.guard();
            match id {
                ID_OPTIMIZE => s.optimize = !s.optimize,
                ID_BATTERY => s.pause_on_battery_saver = !s.pause_on_battery_saver,
                ID_RESTORE => s.restore_on_start = !s.restore_on_start,
                ID_STATIC => s.static_snapshot = !s.static_snapshot,
                _ => return,
            }
            s.save();
        }
        if id == ID_BATTERY || id == ID_STATIC {
            if let Some(wall) = &self.wall { wall.with_lock(|w| w.refresh()); }
        }
    }

    fn toggle_autostart(&mut self) {
        let current = self.settings.with_lock(|s| s.autostart);
        if autostart::set_autostart(!current) {
            self.settings.with_lock(|s| { s.autostart = !current; s.save(); });
        } else { self.error_later("Startup", "Failed to write to the registry."); }
    }

    fn on_status(&mut self, text: &str) {
        self.refresh_buttons();
        if let Some(t) = &self.tray {
            t.set_tooltip(if text.is_empty() { "LiveWallpaper".into() } else { format!("LiveWallpaper - {text}") }.as_str());
        }
    }

    fn refresh_buttons(&self) {
        let active = self.wall.as_ref().map_or(false, |w| w.with_lock(|s| s.active()));
        if let Some(w) = &self.window { w.set_stop_enabled(active); }
    }

    fn prune_cache(&self, current: &str) {
        let s = self.settings.with_lock(|s| s.clone());
        let wall_path = self.wall.as_ref().and_then(|w| w.with_lock(|s| s.path()));
        let saved_copy = (!s.media_path.is_empty()).then_some(s.media_path);
        media::prune_cache(&[Some(current.to_string()), saved_copy, wall_path, self.preview_path.clone()], &media::cache_dir());
    }

    fn start_up(&mut self, restoring: bool) {
        let Some(path) = self.source() else { self.update_path_label(); return; };
        if restoring {
            self.retry_path = Some(path.clone());
            self.retry_attempt = 0;
        } else {
            self.retry_path = None;
        }
        self.begin_prepare(path, restoring, false);
    }

    pub fn window_handle(app: &AppHandle) -> f::HWND { app.with_lock(|a| a.hwnd()) }
    pub fn recover_previous_session(app: &AppHandle) {
        app.with_lock(|a| { if let Some(w) = &a.wall { w.with_lock(|wall| wall.recover_previous_session()); } });
    }
    pub fn start_up_handle(app: &AppHandle, restoring: bool) { app.with_lock(|a| a.start_up(restoring)); }
    pub fn hide_on_start(app: &AppHandle) { app.with_lock(|a| { if let Some(w) = &a.window { w.hide(); } }); }
    pub fn tray_ok(app: &AppHandle) -> bool { app.with_lock(|a| a.tray.as_ref().map(|t| t.ok()).unwrap_or(false)) }
    /// Safe to invoke from the named-pipe worker: it only posts a closure to the UI thread.
    pub fn request_show(hwnd: f::HWND, app: &Weak<Mutex<App>>) {
        ui::post_to(hwnd as usize, app.clone(), |app| app.show_window());
    }
}

/// 오류 창을 UI 스레드의 다음 작업으로 예약한다 (호출 시점에 잠금을 잡고 있어도 안전하다).
fn show_error_later(hwnd: usize, title: &str, text: &str) {
    let (title, text) = (title.to_string(), text.to_string());
    ui::post(hwnd as f::HWND, Box::new(move || util::error_box(&title, &text)));
}

/// 모달 창은 잠금 밖에서 연다. 순서: 파일 선택 → 영상 엔진 확인 → (큰 애니메이션 이미지면) 크기 확인 →
/// 결과만 `App` 으로 돌려 준다 (`App` 잠금을 잡고 있지 않아야 한다).
fn pick_flow(owner: usize, weak: Weak<Mutex<App>>, initial_dir: Option<String>, warning_mb: i64) {
    let hwnd = owner as f::HWND;
    let Some(path) = window::pick_file(hwnd, initial_dir.as_deref()) else { return };
    // libmpv is mandatory for every playable video. Detect a missing DLL or
    // dependent runtime before starting an expensive FFmpeg/cache preparation.
    if let Err(error) = crate::mpv_sys::load() {
        util::error_box("Video engine unavailable", &error);
        return;
    }
    if media::pure::is_animated_image(&path) {
        if let Ok(meta) = std::fs::metadata(&path) {
            let mb = meta.len() as f64 / (1024.0 * 1024.0);
            if mb > warning_mb.max(0) as f64 {
                let text = format!("The file is large: {mb:.1}MB (threshold {warning_mb}MB).\nConversion may take a long time and use a lot of memory. Continue?");
                if !util::confirm(hwnd, "Size warning", &text) { return; }
            }
        }
    }
    ui::post_to(owner, weak, move |app| {
        app.retry_path = None;
        app.begin_prepare(path, false, true);
    });
}

fn post_app(weak: Weak<Mutex<App>>, action: impl FnOnce(&mut App) + Send + 'static) {
    // 핸들은 잠금 없이 읽는다. 잠금을 잡고 있는 UI 스레드가 이 함수를 부르는 경우(트레이 클릭 등)에도 멈추지 않는다.
    let hwnd = UI_HWND.load(Ordering::Acquire);
    if hwnd == 0 { return; }
    ui::post_to(hwnd, weak, action);
}

/// 다른 스레드에서 불러도 되는 콜백을 만든다: 받은 문자열을 UI 스레드에서 `action(app, 문자열)` 로 처리한다.
fn ui_callback(hwnd: usize, weak: &Weak<Mutex<App>>, action: fn(&mut App, &str)) -> impl Fn(String) + Send + Sync + 'static {
    let weak = weak.clone();
    move |text| { ui::post_to(hwnd, weak.clone(), move |app| action(app, &text)); }
}

/// 트레이 메뉴에 보여 줄 현재 설정과 재생 여부. `App` 이 다른 작업 중이라 잠금을 바로 못 잡으면 `None`
/// (메뉴를 열려고 UI 스레드가 기다리다 멈추는 일이 없게 한다).
fn tray_snapshot(weak: &Weak<Mutex<App>>) -> Option<(Settings, bool)> {
    let app = weak.upgrade()?;
    let a = match app.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let settings = a.settings.with_lock(|s| s.clone());
    let active = a.wall.as_ref().map_or(false, |w| w.with_lock(|s| s.active()));
    Some((settings, active))
}

fn make_tray_menu(weak: Weak<Mutex<App>>) -> Vec<MenuItem> {
    let open = MenuItem::command("Open settings", { let w = weak.clone(); move || post_app(w.clone(), |a| a.show_window()) });
    let exit = MenuItem::command("Exit", { let w = weak.clone(); move || post_app(w.clone(), |a| a.quit_app()) });
    let Some((settings, active)) = tray_snapshot(&weak) else {
        return vec![open, MenuItem::separator(), exit];
    };
    let mut stop = MenuItem::command("Stop wallpaper", { let w = weak.clone(); move || post_app(w.clone(), |a| a.stop_wallpaper()) });
    stop.disabled = !active;
    let options = vec![
        option_item("Optimize videos (screen-size resize, 30fps cap, bitrate cap)", settings.optimize, ID_OPTIMIZE, weak.clone()),
        option_item("Pause in battery saver mode", settings.pause_on_battery_saver, ID_BATTERY, weak.clone()),
        option_item("Auto-apply last video on launch", settings.restore_on_start, ID_RESTORE, weak.clone()),
        option_item("Static wallpaper while hidden (replaces Windows wallpaper)", settings.static_snapshot, ID_STATIC, weak.clone()),
        option_item("Run at Windows startup", settings.autostart, ID_AUTOSTART, weak.clone()),
    ];
    vec![open, stop, MenuItem { title: "Options".into(), checked: false, disabled: false, action: None, children: options, separator: false },
        MenuItem::separator(), exit]
}

fn option_item(label: &str, checked: bool, id: usize, weak: Weak<Mutex<App>>) -> MenuItem {
    let mut item = MenuItem::command(label, move || post_app(weak.clone(), move |app| {
        if id == ID_AUTOSTART { app.toggle_autostart(); } else { app.toggle_setting(id); }
    }));
    item.checked = checked; item
}
