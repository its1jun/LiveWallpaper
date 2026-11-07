//! libmpv Player abstraction. mpv destruction is always performed off the UI thread.
#![cfg(windows)]

use crate::sync::LockExt;
use crate::config::FitMode;
use crate::mpv_sys::{self, MpvApi, MpvEvent};
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type ReadyCallback = Box<dyn FnOnce() + Send + 'static>;
type ErrorCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;

struct Session {
    handle: usize,
    api: Arc<MpvApi>,
    stop: Arc<AtomicBool>,
    event_thread: Option<JoinHandle<()>>,
    ready: Arc<Mutex<Option<ReadyCallback>>>,
    reported: Arc<AtomicBool>,
    playing: Arc<AtomicBool>,
}

type DoneCallback = Box<dyn FnOnce() + Send + 'static>;

struct Inner {
    path: Option<String>,
    paused: bool,
    session: Option<Session>,
    terminating: bool,
    done_callbacks: Vec<DoneCallback>,
}

pub struct Player {
    wid: i64,
    fit_mode: Mutex<FitMode>,
    hwdec: String,
    on_error: ErrorCallback,
    inner: Arc<Mutex<Inner>>,
}

fn set_string(api: &MpvApi, handle: *mut std::ffi::c_void, name: &str, value: &str) -> Result<(), String> {
    let n = mpv_sys::cstring(name)?; let v = mpv_sys::cstring(value)?;
    let code = unsafe { (api.set_option_string)(handle, n.as_ptr(), v.as_ptr()) };
    if code < 0 { Err(mpv_sys::error_text(api, code)) } else { Ok(()) }
}

fn set_property(api: &MpvApi, handle: *mut std::ffi::c_void, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (mpv_sys::cstring(name), mpv_sys::cstring(value)) {
        unsafe { (api.set_property_string)(handle, n.as_ptr(), v.as_ptr()); }
    }
}

fn command(api: &MpvApi, handle: *mut std::ffi::c_void, args: &[&str]) -> Result<(), String> {
    let strings: Result<Vec<_>, _> = args.iter().map(|s| mpv_sys::cstring(s)).collect();
    let strings = strings?;
    let mut ptrs: Vec<*const std::ffi::c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let code = unsafe { (api.command)(handle, ptrs.as_ptr()) };
    if code < 0 { Err(mpv_sys::error_text(api, code)) } else { Ok(()) }
}

/// 사용자/외부 콜백의 패닉이 mpv 이벤트 스레드나 정지 절차를 끊지 않도록 가둔다.
fn run_guarded(f: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
}

fn configure(api: &MpvApi, handle: *mut std::ffi::c_void, wid: i64, hwdec: &str, minimal: bool) -> Result<(), String> {
    if minimal {
        // Safety options are best effort: an old libmpv build may reject one, which must not
        // break the fallback. They keep mpv from reading the user's own config and scripts.
        for (k, v) in [("config", "no"), ("load-scripts", "no"), ("ytdl", "no")] {
            let _ = set_string(api, handle, k, v);
        }
        for (k, v) in [("idle", "yes"), ("aid", "no"), ("loop-file", "inf"), ("hwdec", hwdec)] {
            set_string(api, handle, k, v)?;
        }
    } else {
        let options = [
            ("input-default-bindings", "no"), ("input-vo-keyboard", "no"), ("osc", "no"),
            ("osd-level", "0"), ("config", "no"), ("load-scripts", "no"), ("ytdl", "no"),
            ("idle", "yes"), ("aid", "no"), ("loop-file", "inf"), ("cache", "no"),
            ("demuxer-max-bytes", "4MiB"), ("demuxer-max-back-bytes", "0"),
            ("demuxer-readahead-secs", "1"), ("hwdec", hwdec), ("profile", "fast"),
        ];
        for (k, v) in options { set_string(api, handle, k, v)?; }
    }
    let name = mpv_sys::cstring("wid")?;
    let code = unsafe { (api.set_option)(handle, name.as_ptr(), mpv_sys::MPV_FORMAT_INT64, &wid as *const i64 as *const _) };
    if code < 0 { return Err(mpv_sys::error_text(api, code)); }
    Ok(())
}

/// Records which hwdec mode mpv really initialized with, in `<data dir>\\mpv_init.log`.
/// If it says `hwdec=no`, video is decoded on the CPU (a common cause of high CPU use).
/// Best effort only: the log stops growing at 64 KB and never affects playback.
fn log_init(mode: &str, minimal: bool, wid: i64) {
    use std::io::Write;
    let path = crate::config::data_dir().join("mpv_init.log");
    if std::fs::metadata(&path).map(|m| m.len() > 64 * 1024).unwrap_or(false) { return; }
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "mpv initialized: hwdec={mode}, minimal={minimal}, wid={wid}");
    }
}

fn create_handle(api: &MpvApi, wid: i64, hwdec: &str) -> Result<*mut std::ffi::c_void, String> {
    // A stale or machine-specific hwdec setting must not make every video fail to
    // initialize. Try the requested mode first, then mpv's safe automatic mode,
    // and finally software decoding. Keep both full and minimal option sets.
    let mut modes = vec![hwdec];
    // Respect an explicit software-only choice. Other modes may fall back to
    // auto-safe and then software decoding if the requested backend is rejected.
    if hwdec != "no" {
        for fallback in ["auto-safe", "no"] {
            if !modes.iter().any(|mode| *mode == fallback) { modes.push(fallback); }
        }
    }
    let mut errors = Vec::new();
    for mode in modes {
        for minimal in [false, true] {
            let handle = unsafe { (api.create)() };
            if handle.is_null() { return Err("mpv_create returned a null handle".into()); }
            let configured = configure(api, handle, wid, mode, minimal);
            let initialized = if configured.is_ok() { unsafe { (api.initialize)(handle) } } else { -1 };
            if configured.is_ok() && initialized >= 0 {
                log_init(mode, minimal, wid);
                return Ok(handle);
            }
            let current = configured.err().unwrap_or_else(|| mpv_sys::error_text(api, initialized));
            errors.push(format!("hwdec={mode}, minimal={minimal}: {current}"));
            unsafe { (api.terminate_destroy)(handle); }
        }
    }
    Err(format!("mpv initialization failed after hardware/software fallback attempts: {}", errors.join("; ")))
}

/// Destroy a freshly created handle that never got an event thread, off the UI thread.
/// Falls back to a synchronous destroy only if the worker cannot be spawned.
fn destroy_async(api: &Arc<MpvApi>, handle: *mut std::ffi::c_void) {
    let (worker_api, raw) = (api.clone(), handle as usize);
    let spawned = thread::Builder::new().name("LiveWallpaper-mpv-destroy".into()).spawn(move || {
        unsafe { (worker_api.terminate_destroy)(raw as *mut _); }
    });
    if spawned.is_err() { unsafe { (api.terminate_destroy)(handle); } }
}

/// 세션을 정리한다: 이벤트 스레드를 멈추고 기다린 뒤 mpv 를 파괴한다 (UI 스레드에서 부르지 않는다).
fn teardown(mut session: Session) {
    session.stop.store(true, Ordering::Release);
    unsafe { (session.api.wakeup)(session.handle as *mut _); }
    // The event thread must leave mpv_wait_event before its handle is destroyed.
    if let Some(join) = session.event_thread.take() { let _ = join.join(); }
    unsafe { (session.api.terminate_destroy)(session.handle as *mut _); }
}

/// 정리가 끝났음을 표시하고, 기다리던 완료 콜백을 모두 실행한다.
fn finish_termination(inner: &Mutex<Inner>) {
    let callbacks = inner.with_lock(|i| {
        i.terminating = false;
        std::mem::take(&mut i.done_callbacks)
    });
    for callback in callbacks { run_guarded(callback); }
}

fn apply_fit(api: &MpvApi, handle: *mut std::ffi::c_void, mode: FitMode) {
    set_property(api, handle, "keepaspect", if mode == FitMode::Stretch { "no" } else { "yes" });
    set_property(api, handle, "panscan", if mode == FitMode::Cover { "1.0" } else { "0.0" });
}

fn event_loop(handle: usize, api: Arc<MpvApi>, stop: Arc<AtomicBool>, ready: Arc<Mutex<Option<ReadyCallback>>>,
              on_error: ErrorCallback, reported: Arc<AtomicBool>, playing: Arc<AtomicBool>) {
    let raw = handle as *mut std::ffi::c_void;
    let mut last_error_log = String::new();
    loop {
        if stop.load(Ordering::Acquire) { break; }
        let event_ptr = unsafe { (api.wait_event)(raw, 1.0) };
        // 1 s timeout: teardown() sets `stop` and then calls mpv_wakeup, so shutdown stays prompt.
        if event_ptr.is_null() { continue; }
        let event: &MpvEvent = unsafe { &*event_ptr };
        if event.event_id == mpv_sys::MPV_EVENT_SHUTDOWN {
            playing.store(false, Ordering::Release);
            break;
        }
        if event.event_id == mpv_sys::MPV_EVENT_START_FILE {
            // Clear diagnostics from the previous playlist item. START_FILE is
            // emitted for a replacement before its new decode attempt begins.
            last_error_log.clear();
        } else if event.event_id == mpv_sys::MPV_EVENT_PLAYBACK_RESTART {
            playing.store(true, Ordering::Release);
            last_error_log.clear();
            let callback = ready.with_lock(|slot| slot.take());
            if let Some(callback) = callback { run_guarded(callback); }
        } else if event.event_id == mpv_sys::MPV_EVENT_END_FILE && !event.data.is_null() {
            // A failed asynchronous loadfile command returns success initially;
            // the definitive reason arrives as END_FILE. Some damaged inputs end
            // with EOF before PLAYBACK_RESTART, so catch that path too.
            let end = unsafe { &*(event.data as *const mpv_sys::MpvEventEndFile) };
            let failed = end.reason == mpv_sys::MPV_END_FILE_REASON_ERROR
                || (end.reason == mpv_sys::MPV_END_FILE_REASON_EOF && !playing.load(Ordering::Acquire));
            if failed {
                playing.store(false, Ordering::Release);
                if !reported.swap(true, Ordering::AcqRel) {
                    let detail = if end.reason == mpv_sys::MPV_END_FILE_REASON_ERROR {
                        if end.error == 0 { "mpv reported a media-load error without an error code".to_string() }
                        else { mpv_sys::error_text(&api, end.error) }
                    } else {
                        "mpv reached end-of-file before playback started. The file may be damaged, unsupported, or contain no video stream.".to_string()
                    };
                    let context = if last_error_log.is_empty() { String::new() }
                        else { format!("\n\nmpv diagnostic: {}", last_error_log.trim()) };
                    let message = format!("Unable to decode/open the selected video: {detail}{context}");
                    run_guarded(|| on_error(message));
                }
            }
        } else if event.event_id == mpv_sys::MPV_EVENT_LOG_MESSAGE && !event.data.is_null() {
            // A log-level error alone is not proof that playback failed: mpv can
            // report a recoverable decoder/vo warning and then successfully play.
            // Keep the latest line only as context for a later END_FILE failure.
            let log = unsafe { &*(event.data as *const mpv_sys::MpvEventLogMessage) };
            if !log.level.is_null() && !log.text.is_null() {
                let level = unsafe { CStr::from_ptr(log.level) }.to_string_lossy();
                if level == "error" || level == "fatal" {
                    let prefix = if log.prefix.is_null() { "mpv".into() } else { unsafe { CStr::from_ptr(log.prefix) }.to_string_lossy().into_owned() };
                    let text = unsafe { CStr::from_ptr(log.text) }.to_string_lossy().trim().to_string();
                    last_error_log = format!("[{prefix}] {text}");
                    if last_error_log.len() > 1200 {
                        let start = last_error_log.len().saturating_sub(1200);
                        let boundary = last_error_log.char_indices().find(|(i, _)| *i >= start).map(|(i, _)| i).unwrap_or(0);
                        last_error_log = last_error_log[boundary..].to_string();
                    }
                }
            }
        }
    }
}

impl Player {
    pub fn new(wid: isize, fit_mode: FitMode, hwdec: &str, on_error: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self { wid: wid as i64, fit_mode: Mutex::new(fit_mode), hwdec: if hwdec.trim().is_empty() { "auto-safe".into() } else { hwdec.into() },
            on_error: Arc::new(on_error), inner: Arc::new(Mutex::new(Inner { path: None, paused: false, session: None, terminating: false, done_callbacks: Vec::new() })) }
    }

    pub fn load(&self, path: &str) { self.inner.with_lock(|i| i.path = (!path.is_empty()).then(|| path.to_string())); }

    pub fn play(&self, path: &str) {
        if path.is_empty() { return; }
        // Errors are collected under the lock and reported after it is released, so an
        // on_error callback may safely call back into this player.
        let mut errors: Vec<String> = Vec::new();
        { let mut i = self.inner.guard();
            i.path = Some(path.to_string());
            i.paused = false;
            if let Some(session) = i.session.as_ref() {
                // Reuse the existing libmpv instance for a new preview file.
                session.playing.store(false, Ordering::Release);
                session.reported.store(false, Ordering::Release);
                set_property(&session.api, session.handle as *mut _, "pause", "no");
                if let Err(error) = command(&session.api, session.handle as *mut _, &["loadfile", path, "replace"]) {
                    session.reported.store(true, Ordering::Release);
                    errors.push(format!("Unable to load video: {error}"));
                }
            } else {
                self.start_locked(&mut i, None, &mut errors);
            }
        }
        for message in errors { (self.on_error)(message); }
    }

    fn start_locked(&self, inner: &mut Inner, ready_cb: Option<ReadyCallback>, errors: &mut Vec<String>) {
        if inner.session.is_some() || inner.terminating { return; }
        let Some(path) = inner.path.clone() else { return; };
        let api = match mpv_sys::load() { Ok(api) => api, Err(e) => { errors.push(e); return; } };
        let handle = match create_handle(&api, self.wid, &self.hwdec) { Ok(h) => h, Err(e) => { errors.push(e); return; } };
        // Disable noisy/unneeded events, then enable only the events consumed below.
        for id in [6, 7, 8, 11, 12, 13, 14, 17, 18, 19, 20, 23] {
            unsafe { (api.request_event)(handle, id, 0); }
        }
        for id in [mpv_sys::MPV_EVENT_SHUTDOWN, mpv_sys::MPV_EVENT_LOG_MESSAGE,
                   mpv_sys::MPV_EVENT_START_FILE, mpv_sys::MPV_EVENT_END_FILE,
                   mpv_sys::MPV_EVENT_PLAYBACK_RESTART] {
            let code = unsafe { (api.request_event)(handle, id, 1) };
            if code < 0 {
                let error = mpv_sys::error_text(&api, code);
                destroy_async(&api, handle);
                errors.push(format!("libmpv could not enable required playback event {id}: {error}"));
                return;
            }
        }
        if let Ok(level) = mpv_sys::cstring("error") {
            // Event-based END_FILE errors remain available if log collection is not
            // supported by an older libmpv build; logs only enrich diagnostics.
            unsafe { (api.request_log_messages)(handle, level.as_ptr()); }
        }
        let ready = Arc::new(Mutex::new(ready_cb));
        let stop = Arc::new(AtomicBool::new(false));
        let reported = Arc::new(AtomicBool::new(false));
        let playing = Arc::new(AtomicBool::new(false));
        let mode = self.fit_mode.with_lock(|s| *s);
        apply_fit(&api, handle, mode);
        set_property(&api, handle, "pause", "no");

        // Start loading before the event thread. If loadfile fails synchronously,
        // no worker can still be waiting on a handle that we are about to destroy.
        if let Err(error) = command(&api, handle, &["loadfile", &path, "replace"]) {
            errors.push(format!("Unable to load video: {error}"));
            destroy_async(&api, handle);
            return;
        }

        let event_thread = {
            let (api2, stop2, ready2, error2, reported2, playing2) = (
                api.clone(), stop.clone(), ready.clone(), self.on_error.clone(), reported.clone(), playing.clone()
            );
            let h = handle as usize;
            match thread::Builder::new().name("LiveWallpaper-mpv-events".into()).spawn(move || {
                event_loop(h, api2, stop2, ready2, error2, reported2, playing2);
            }) {
                Ok(thread) => Some(thread),
                Err(error) => {
                    unsafe { (api.terminate_destroy)(handle); } // no event thread exists yet
                    errors.push(format!("Unable to start the video event worker: {error}"));
                    return;
                }
            }
        };
        inner.session = Some(Session { handle: handle as usize, api, stop, event_thread, ready, reported, playing });
        inner.paused = false;
    }

    pub fn pause(&self) {
        { let mut i = self.inner.guard();
            if !i.paused {
                if let Some(s) = i.session.as_ref() { set_property(&s.api, s.handle as *mut _, "pause", "yes"); i.paused = true; }
            }
        }
    }

    pub fn resume(&self, on_ready: Option<Box<dyn FnOnce() + Send + 'static>>) {
        let mut errors: Vec<String> = Vec::new();
        let mut immediate: Option<Box<dyn FnOnce() + Send + 'static>> = None;
        { let mut i = self.inner.guard();
            if let Some(s) = i.session.as_ref() {
                // Resume completion must mean that mpv actually restarted playback,
                // not merely that the unpause command was queued. Wallpaper uses this
                // callback before restoring the user's original still wallpaper.
                if let Some(callback) = on_ready {
                    // If this session has already reached PLAYBACK_RESTART, it was
                    // genuinely playing before a normal pause; unpausing it does not
                    // require a new file-load event. A not-yet-started session waits
                    // for its first PLAYBACK_RESTART instead.
                    if s.playing.load(Ordering::Acquire) {
                        immediate = Some(callback);
                    } else {
                        *s.ready.guard() = Some(callback);
                    }
                }
                if i.paused {
                    set_property(&s.api, s.handle as *mut _, "pause", "no");
                    i.paused = false;
                }
            } else {
                self.start_locked(&mut i, on_ready, &mut errors);
            }
        }
        // Run outside the player lock (like play()), so a callback that re-enters the player cannot deadlock.
        if let Some(callback) = immediate { run_guarded(callback); }
        for message in errors { (self.on_error)(message); }
    }

    /// Destroy mpv asynchronously, then run `on_done`. The caller must not destroy the host HWND
    /// until its own completion flow runs.
    pub fn suspend_then(&self, on_done: Option<DoneCallback>) {
        let session = {
            let mut inner = self.inner.guard();
            inner.paused = false;
            if let Some(callback) = on_done { inner.done_callbacks.push(callback); }
            if inner.terminating { return; }
            match inner.session.take() {
                Some(session) => { inner.terminating = true; Some(session) }
                None => None,
            }
        };

        let Some(session) = session else {
            finish_termination(&self.inner);
            return;
        };

        // Keep ownership recoverable if the OS refuses to create the termination thread.
        let pending = Arc::new(Mutex::new(Some(session)));
        let worker_pending = pending.clone();
        let inner = self.inner.clone();
        let spawn = thread::Builder::new().name("LiveWallpaper-mpv-terminate".into()).spawn(move || {
            if let Some(session) = worker_pending.with_lock(|slot| slot.take()) { teardown(session); }
            finish_termination(&inner);
        });
        if let Err(error) = spawn {
            // Thread creation can fail under resource pressure. Falling back to a
            // synchronous teardown is safer than restoring a session that keeps the
            // desktop host alive forever and never runs its completion callbacks.
            if let Some(session) = pending.with_lock(|slot| slot.take()) { teardown(session); }
            finish_termination(&self.inner);
            (self.on_error)(format!("Unable to start mpv termination thread; stopped the player synchronously instead: {error}"));
        }
    }

    pub fn stop(&self) { self.stop_then(None); }

    pub fn stop_then(&self, on_done: Option<Box<dyn FnOnce() + Send + 'static>>) {
        self.inner.with_lock(|i| i.path = None);
        self.suspend_then(on_done);
    }

    pub fn set_fit_mode(&self, mode: FitMode) {
        self.fit_mode.with_lock(|m| *m = mode);
        { let i = self.inner.guard();
            if let Some(s) = i.session.as_ref() {
                apply_fit(&s.api, s.handle as *mut _, mode);
            }
        }
    }
}

impl Drop for Player { fn drop(&mut self) { self.stop(); } }
