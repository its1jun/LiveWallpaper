//! Per-user named-pipe SHOW IPC. The named mutex remains the single-instance authority.
#![cfg(windows)]

use crate::win::ffi as f;
use crate::win::util::wide;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
const PIPE_TYPE_MESSAGE: u32 = 0x00000004;
const PIPE_READMODE_MESSAGE: u32 = 0x00000002;
const PIPE_WAIT: u32 = 0x00000000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const ERROR_PIPE_CONNECTED: u32 = 535;

fn pipe_name() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    let identity = format!("{}\\{}", domain, user).to_lowercase();
    // The pipe is already scoped by Windows' default per-user access control. This short stable hash
    // merely avoids collisions between local accounts; it is not an authentication mechanism.
    let mut hash = 0xcbf29ce484222325u64;
    for b in identity.as_bytes() { hash ^= *b as u64; hash = hash.wrapping_mul(0x100000001b3); }
    format!("\\\\.\\pipe\\LiveWallpaper-rs-{hash:016x}")
}

fn serve_one(pipe: f::HANDLE, on_show: &(dyn Fn() + Send + Sync)) {
    let mut buf = [0u8; 32]; let mut read = 0u32;
    let ok = unsafe { f::ReadFile(pipe, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read, std::ptr::null_mut()) };
    let response: &[u8] = if ok != 0 && &buf[..read as usize] == b"SHOW" {
        on_show(); b"OK"
    } else { b"UNKNOWN_COMMAND" };
    let mut written = 0u32;
    unsafe {
        f::WriteFile(pipe, response.as_ptr() as *const c_void, response.len() as u32, &mut written, std::ptr::null_mut());
        f::FlushFileBuffers(pipe);
        f::DisconnectNamedPipe(pipe);
        f::CloseHandle(pipe);
    }
}

/// CreateFileW / CreateNamedPipeW 는 실패하면 null 이 아니라 INVALID_HANDLE_VALUE(-1) 를 돌려준다.
fn is_valid(handle: f::HANDLE) -> bool { !handle.is_null() && handle as isize != -1 }

/// 서버 파이프에 클라이언트로 접속한다. `timeout` 안에 안 되면 `None`. 재시도 사이에 `pause_ms` 쉰다.
fn connect_client(name: &[u16], timeout: Duration, pause_ms: u32) -> Option<f::HANDLE> {
    let deadline = Instant::now() + timeout;
    loop {
        let client = unsafe { f::CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0,
            std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if is_valid(client) { return Some(client); }
        if Instant::now() >= deadline { return None; }
        unsafe { f::WaitNamedPipeW(name.as_ptr(), 100); f::Sleep(pause_ms); }
    }
}

pub struct InstanceServer {
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl InstanceServer {
    pub fn new(on_show: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let name = pipe_name();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone(); let thread_name = name.clone();
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(on_show);
        let join = thread::Builder::new().name("LiveWallpaper-IPC".into()).spawn(move || {
            let pipe_w = wide(&thread_name);
            while !thread_stop.load(Ordering::Acquire) {
                let pipe = unsafe { f::CreateNamedPipeW(pipe_w.as_ptr(), PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES,
                    128, 128, 0, std::ptr::null_mut()) };
                if !is_valid(pipe) { break; }
                let connected = unsafe { f::ConnectNamedPipe(pipe, std::ptr::null_mut()) != 0 || f::GetLastError() == ERROR_PIPE_CONNECTED };
                if thread_stop.load(Ordering::Acquire) { unsafe { f::CloseHandle(pipe); } break; }
                if connected { serve_one(pipe, callback.as_ref()); }
                else { unsafe { f::CloseHandle(pipe); } }
            }
        }).map_err(|e| format!("Unable to start the named-pipe server: {e}"))?;
        Ok(Self { name, stop, thread: Some(join) })
    }

    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        // A short client connection wakes a blocking ConnectNamedPipe call. The worker may not
        // have created its first pipe instance yet, so retry briefly. Skip it when the worker is
        // already gone (closed before, or it failed to create the pipe): nothing is left to wake.
        if self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            let name = wide(&self.name);
            if let Some(client) = connect_client(&name, Duration::from_secs(3), 20) {
                unsafe { f::CloseHandle(client); }
            }
        }
        if let Some(join) = self.thread.take() { let _ = join.join(); }
    }
}
impl Drop for InstanceServer { fn drop(&mut self) { self.close(); } }

pub fn request_instance_show(timeout: Duration) -> bool {
    let name = wide(&pipe_name());
    let Some(client) = connect_client(&name, timeout, 100) else { return false };
    let command = b"SHOW"; let mut written = 0u32;
    let sent = unsafe { f::WriteFile(client, command.as_ptr() as *const c_void, command.len() as u32, &mut written, std::ptr::null_mut()) != 0 };
    let mut response = [0u8; 32]; let mut read = 0u32;
    let received = sent && unsafe { f::ReadFile(client, response.as_mut_ptr() as *mut c_void, response.len() as u32, &mut read, std::ptr::null_mut()) != 0 };
    unsafe { f::CloseHandle(client); }
    received && &response[..read as usize] == b"OK"
}
