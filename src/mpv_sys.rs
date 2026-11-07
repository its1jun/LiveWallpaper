//! Minimal libmpv client API loaded dynamically.
//! Function signatures follow libmpv's public client.h ABI.
#![cfg(windows)]

use crate::sync::LockExt;
use crate::win::ffi as f;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;

const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;

#[repr(C)]
pub struct MpvEvent { pub event_id: c_int, pub error: c_int, pub reply_userdata: u64, pub data: *mut c_void }
#[repr(C)]
pub struct MpvEventLogMessage {
    // These are the only fields consumed. Keep the ABI view limited to the
    // original three pointers so older libmpv builds need not expose appended fields.
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
}

#[repr(C)]
pub struct MpvEventEndFile {
    // Only the stable leading fields are read. Newer libmpv versions append
    // playlist IDs; do not create an oversized Rust reference for older builds.
    pub reason: c_int,
    pub error: c_int,
}

pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;

pub type MpvHandle = *mut c_void;
type Create = unsafe extern "C" fn() -> MpvHandle;
type Initialize = unsafe extern "C" fn(MpvHandle) -> c_int;
type SetOptionString = unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int;
type SetOption = unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *const c_void) -> c_int;
type SetPropertyString = unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int;
type Command = unsafe extern "C" fn(MpvHandle, *const *const c_char) -> c_int;
type WaitEvent = unsafe extern "C" fn(MpvHandle, f64) -> *const MpvEvent;
type RequestEvent = unsafe extern "C" fn(MpvHandle, c_int, c_int) -> c_int;
type RequestLogMessages = unsafe extern "C" fn(MpvHandle, *const c_char) -> c_int;
type Wakeup = unsafe extern "C" fn(MpvHandle);
type TerminateDestroy = unsafe extern "C" fn(MpvHandle);
type ErrorString = unsafe extern "C" fn(c_int) -> *const c_char;

pub struct MpvApi {
    _module: usize,
    pub create: Create,
    pub initialize: Initialize,
    pub set_option_string: SetOptionString,
    pub set_option: SetOption,
    pub set_property_string: SetPropertyString,
    pub command: Command,
    pub wait_event: WaitEvent,
    pub request_event: RequestEvent,
    pub request_log_messages: RequestLogMessages,
    pub wakeup: Wakeup,
    pub terminate_destroy: TerminateDestroy,
    pub error_string: ErrorString,
}
unsafe impl Send for MpvApi {}
unsafe impl Sync for MpvApi {}

fn get_symbol<T: Copy>(module: f::HANDLE, name: &'static [u8]) -> Result<T, String> {
    let p = unsafe { f::GetProcAddress(module, name.as_ptr()) };
    if p.is_null() { return Err(format!("missing libmpv symbol {}", String::from_utf8_lossy(&name[..name.len()-1]))); }
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

impl MpvApi {
    unsafe fn from_module(module: f::HANDLE) -> Result<Self, String> {
        let loaded = (|| Ok(Self {
            _module: module as usize,
            create: get_symbol(module, b"mpv_create\0")?,
            initialize: get_symbol(module, b"mpv_initialize\0")?,
            set_option_string: get_symbol(module, b"mpv_set_option_string\0")?,
            set_option: get_symbol(module, b"mpv_set_option\0")?,
            set_property_string: get_symbol(module, b"mpv_set_property_string\0")?,
            command: get_symbol(module, b"mpv_command\0")?,
            wait_event: get_symbol(module, b"mpv_wait_event\0")?,
            request_event: get_symbol(module, b"mpv_request_event\0")?,
            request_log_messages: get_symbol(module, b"mpv_request_log_messages\0")?,
            wakeup: get_symbol(module, b"mpv_wakeup\0")?,
            terminate_destroy: get_symbol(module, b"mpv_terminate_destroy\0")?,
            error_string: get_symbol(module, b"mpv_error_string\0")?,
        }))();
        if loaded.is_err() { f::FreeLibrary(module); }
        loaded
    }
}

fn candidate_paths_in(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in roots {
        paths.extend([
            root.join("libs/libmpv-2.dll"), root.join("libs/mpv-2.dll"),
            root.join("libmpv-2.dll"), root.join("mpv-2.dll"),
            root.join("mpv/libmpv-2.dll"), root.join("mpv/mpv-2.dll"),
        ]);
    }
    paths.dedup();
    paths
}

fn candidate_paths() -> Vec<PathBuf> {
    candidate_paths_in(&crate::config::resource_dirs())
}

fn windows_loader_hint(code: u32) -> &'static str {
    match code {
        126 => "The DLL or one of its dependent DLLs is missing. Keep the complete libmpv DLL bundle together in libs\\.",
        193 => "The DLL architecture does not match the app. Use 64-bit DLLs with the x64 build.",
        127 => "A required exported function is missing. The libmpv DLL may be incompatible; use a matching current build.",
        1114 => "A DLL initialization routine failed. Check the libmpv build and its dependent runtime files.",
        5 => "Windows denied access to the DLL. Check file permissions and security software.",
        _ => "Check the DLL file, architecture, dependencies, and permissions.",
    }
}

fn load_uncached() -> Result<Arc<MpvApi>, String> {
    let mut errors = Vec::new();
    for path in candidate_paths() {
        if !path.is_file() { continue; }
        let name = crate::win::util::wide(&path.to_string_lossy());
        // Include the DLL's own directory when resolving its dependent DLLs. This is
        // important for portable libmpv bundles whose FFmpeg/codec DLLs live in libs\.
        let module = unsafe {
            f::LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if module.is_null() {
            let code = unsafe { f::GetLastError() };
            errors.push(format!("{}: Windows error {}. {}", path.display(), code, windows_loader_hint(code)));
            continue;
        }
        match unsafe { MpvApi::from_module(module) } {
            Ok(api) => return Ok(Arc::new(api)),
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    // No plain LoadLibraryW fallback: it would search PATH and the current directory (DLL planting).
    // Only the app's own libs\ folders (above) are used.
    let detail = if errors.is_empty() { "file was not found".to_string() } else { errors.join("; ") };
    Err(format!(
        "Unable to load libmpv. Put a compatible 64-bit libmpv-2.dll (or mpv-2.dll) in the app's libs folder. Keep its required dependency DLLs beside it. Windows error 126 usually means a dependency DLL is missing.\n({detail})"
    ))
}

/// The DLL is intentionally never unloaded: mpv worker threads and cached function
/// pointers may outlive any single player, so it stays mapped until process exit.
pub fn load() -> Result<Arc<MpvApi>, String> {
    // Cache successful loads only. A failed first attempt must not permanently
    // poison the process if the user fixes the DLL installation while it is open.
    static API: OnceLock<Mutex<Option<Arc<MpvApi>>>> = OnceLock::new();
    let cache = API.get_or_init(|| Mutex::new(None));
    let mut guard = cache.guard();
    if let Some(api) = guard.as_ref() {
        return Ok(api.clone());
    }
    let api = load_uncached()?;
    *guard = Some(api.clone());
    Ok(api)
}

pub fn cstring(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "mpv argument contains an interior NUL byte".to_string())
}

pub fn error_text(api: &MpvApi, code: i32) -> String {
    let ptr = unsafe { (api.error_string)(code) };
    if ptr.is_null() { format!("mpv error {code}") }
    else { unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned() }
}

