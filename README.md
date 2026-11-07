# Tree
```text
LiveWallpaper
│   Cargo.toml
│   README.md
│
├───builder
│       build.bat
│       build.rs
│       build_installer.bat
│       icon.ico
│       installer.iss
│
├───libs
│       libmpv-2.dll
│
└───src
    │   config.rs
    │   instance.rs
    │   main.rs
    │   mpv_sys.rs
    │   player.rs
    │   snapshot.rs
    │   sync.rs
    │   ui.rs
    │   wallpaper.rs
    │   wallpaper_logic.rs
    │
    ├───app
    │       mod.rs
    │       window.rs
    │
    ├───media
    │       mod.rs
    │       pure.rs
    │
    └───win
            autostart.rs
            consts.rs
            cover.rs
            ffi.rs
            host.rs
            mod.rs
            tray.rs
            util.rs
            watcher.rs
```
# Building LiveWallpaper (Windows)

## Requirements
- Rust 1.79 or newer (`rustup`), Windows x64.
- `libs\libmpv-2.dll` (or `mpv-2.dll`) with its dependency DLLs beside it. Not included in the repo.
- Optional: `libs\ffmpeg.exe`, needed for GIF/WebP conversion, video optimization and the static wallpaper frame.
  If it is missing, `builder\build.bat` fetches one from the pinned `imageio-ffmpeg` pip package.

## Build
- `cargo test` runs the unit tests (pure logic runs on any OS).
- `builder\build.bat` builds a portable folder in `builder\dist\LiveWallpaper`.
- `builder\build_installer.bat` builds the Inno Setup installer (run `build.bat` first).

## Running outside the install folder
Release builds look for `libs\` next to the exe. To run `target\release\LiveWallpaper.exe` directly,
set `LIVEWALLPAPER_HOME` to the folder that contains `libs\`. Debug builds also search the source tree.

## Command-line flags
- `--tray`: start hidden in the tray.
- `--restore-wallpaper`: put the original Windows wallpaper back and exit (used by the uninstaller).

## Licenses
libmpv and ffmpeg have their own licenses. Check them before redistributing the binaries.

## Code map (where to look)
- `main.rs`: startup, single instance, message loop.
- `app/`: settings window (`window.rs`) and orchestration (`mod.rs`). Rule: never show a modal dialog while the
  `App` mutex is held; use `error_later` / `pick_flow` (a modal loop would deadlock queued UI jobs).
- `wallpaper.rs` + `wallpaper_logic.rs`: desktop host window and the play/pause/static/resume state machine
  (the pure decision tables live in `wallpaper_logic.rs` and are unit-tested).
- `player.rs` / `mpv_sys.rs`: libmpv wrapper and dynamic loading.
- `media/`: ffmpeg probing, conversion, cache (`pure.rs` = pure helpers, testable on any OS).
- `win/`: Win32 plumbing (`ffi`, `host`, `tray`, `cover`, `watcher`, `util`, `autostart`).
- `config.rs`, `snapshot.rs`: settings file and original-wallpaper save/restore.
- `ui.rs`: posting jobs/timers to the UI thread. `sync.rs`: poison-tolerant `Mutex` helpers.
