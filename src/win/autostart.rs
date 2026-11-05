//! Current-user Run registry integration.
#![cfg(windows)]

use super::ffi as f;
use crate::win::util::wide;
use std::path::Path;

const HKEY_CURRENT_USER: f::HKEY = (-2_147_483_647isize) as f::HKEY;
const KEY_SET_VALUE: u32 = 0x0002;
const REG_SZ: u32 = 1;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "LiveWallpaper";

pub fn autostart_command(executable: &str) -> String {
    // A quoted executable path preserves spaces; `--tray` requests quiet startup.
    format!("\"{}\" --tray", executable.replace('"', "\\\""))
}

fn open_key(access: u32, create: bool) -> Result<f::HKEY, String> {
    let subkey = wide(RUN_KEY); let mut key = std::ptr::null_mut();
    let status = unsafe {
        if create {
            let mut disposition = 0u32;
            f::RegCreateKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, std::ptr::null_mut(),
                0, access, std::ptr::null_mut(), &mut key, &mut disposition)
        } else {
            f::RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key)
        }
    };
    if status == 0 { Ok(key) } else { Err(format!("Registry open failed ({status})")) }
}

pub fn set_autostart(enabled: bool) -> bool {
    if enabled {
        let exe = match std::env::current_exe() { Ok(p) => p, Err(_) => return false };
        set_autostart_command(&exe)
    } else {
        let key = match open_key(KEY_SET_VALUE, false) { Ok(k) => k, Err(_) => return true };
        let name = wide(RUN_NAME);
        let status = unsafe { f::RegDeleteValueW(key, name.as_ptr()) };
        unsafe { f::RegCloseKey(key); }
        status == 0 || status == 2 // ERROR_FILE_NOT_FOUND
    }
}

pub fn set_autostart_command(executable: &Path) -> bool {
    let key = match open_key(KEY_SET_VALUE, true) { Ok(k) => k, Err(_) => return false };
    let name = wide(RUN_NAME);
    let command = wide(&autostart_command(&executable.to_string_lossy()));
    let bytes = (command.len() * std::mem::size_of::<u16>()) as u32;
    let status = unsafe { f::RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, command.as_ptr() as *const u8, bytes) };
    unsafe { f::RegCloseKey(key); }
    status == 0
}
