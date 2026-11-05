// Release Windows executable has no console; debug keeps it for diagnostics.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod config;
mod media;
mod snapshot;
mod sync;
#[cfg(test)] mod test_util;
mod wallpaper_logic;
mod win;

#[cfg(windows)] mod app;
#[cfg(windows)] mod instance;
#[cfg(windows)] mod mpv_sys;
#[cfg(windows)] mod player;
#[cfg(windows)] mod ui;
#[cfg(windows)] mod wallpaper;

#[cfg(windows)]
fn run() -> Result<(), String> {
    use crate::app::App;
    use crate::win::{autostart, ffi as f, util};
    use crate::sync::LockExt;
    use std::sync::Arc;

    if !util::acquire_instance()? {
        if std::env::args().any(|a| a == "--tray") { return Ok(()); }
        if !instance::request_instance_show(std::time::Duration::from_secs(5)) {
            util::message_box("LiveWallpaper is already running but did not respond to the open-window request.\nIf you just updated from an older version, exit the running LiveWallpaper once and launch the new version.");
        }
        return Ok(());
    }

    let result = (|| {
        util::enable_dpi_awareness();
        config::migrate_legacy_data();
        let settings = config::Settings::load();
        if settings.autostart { let _ = autostart::set_autostart(true); }
        let app = App::create(settings.clone())?;
        App::recover_previous_session(&app);
        let weak = Arc::downgrade(&app);
        let ui_hwnd = App::window_handle(&app) as usize;
        let mut server = instance::InstanceServer::new(move || App::request_show(ui_hwnd as f::HWND, &weak))?;
        // Only auto-restore if the saved source exists.
        let restoring = settings.restore_on_start && app.with_lock(|a| a.has_source());
        if std::env::args().any(|a| a == "--tray") && restoring && App::tray_ok(&app) {
            App::hide_on_start(&app);
        }
        App::start_up_handle(&app, restoring);

        let mut message: f::MSG = unsafe { std::mem::zeroed() };
        loop {
            let got = unsafe { f::GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
            if got == 0 { break; }
            if got < 0 { server.close(); return Err(format!("GetMessageW failed ({})", unsafe { f::GetLastError() })); }
            unsafe { f::TranslateMessage(&message); f::DispatchMessageW(&message); }
        }
        server.close();
        Ok(())
    })();
    util::release_instance();
    result
}

#[cfg(windows)]
fn main() {
    std::panic::set_hook(Box::new(|info| {
        let summary = info.to_string();
        crate::win::util::message_box(&format!("An unexpected error occurred.\n\n{summary}"));
    }));
    if let Err(error) = run() {
        crate::win::util::message_box(&format!("Failed to start LiveWallpaper.\n\n{error}"));
    }
}

#[cfg(not(windows))]
fn main() {
    println!("LiveWallpaper is a Windows desktop application. Run the Windows build to use the UI.");
}
