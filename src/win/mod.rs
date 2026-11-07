//! Win32 backend. Platform-independent math remains available on every target.

pub mod cover;
pub mod util;

#[cfg(windows)] pub(crate) mod consts;
#[cfg(windows)] pub(crate) mod ffi;
#[cfg(windows)] pub mod host;
#[cfg(windows)] pub mod watcher;
#[cfg(windows)] pub mod tray;
#[cfg(windows)] pub mod autostart;
