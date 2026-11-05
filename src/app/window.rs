//! Native Win32 settings window and controls.
#![cfg(windows)]

use crate::config::{Codec, FitMode};
use crate::win::consts::*;
use crate::win::{ffi as f, host};
use crate::win::util::{from_wide, wide};
use std::cell::RefCell;
use std::ffi::c_void;

pub const ID_SELECT: usize = 1001;
pub const ID_APPLY: usize = 1002;
pub const ID_STOP: usize = 1003;
pub const ID_FIT: usize = 1004;
pub const ID_MORE: usize = 1005;
pub const ID_CODEC: usize = 1006;
// Tray menu command IDs. These do not correspond to controls in the main window.
pub const ID_OPTIMIZE: usize = 1101;
pub const ID_BATTERY: usize = 1102;
pub const ID_RESTORE: usize = 1103;
pub const ID_STATIC: usize = 1104;
pub const ID_AUTOSTART: usize = 1105;

const WM_SETFONT: u32 = 0x0030;
const BS_PUSHBUTTON: u32 = 0;
const SS_LEFT: u32 = 0;
const SS_NOTIFY: u32 = 0x0100;
const SS_BLACKRECT: u32 = 4;
const CBS_DROPDOWNLIST: u32 = 0x0003;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETITEMHEIGHT: u32 = 0x0153;
const OFN_NOCHANGEDIR: u32 = 0x0008;
const OFN_EXPLORER: u32 = 0x0008_0000;
const OFN_FILEMUSTEXIST: u32 = 0x1000;
const OFN_PATHMUSTEXIST: u32 = 0x0800;
const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;
const IMAGE_ICON: u32 = 1;
const LR_LOADFROMFILE: u32 = 0x0010;

// GetOpenFileNameW expects a semicolon-separated pattern list for one filter.
// Spaces between patterns are treated as part of a single wildcard and hide matching files.
const VIDEO_FILE_FILTER: &str = concat!(
    "Video and animated images\0",
    "*.mp4;*.m4v;*.mov;*.webm;*.mkv;*.avi;*.wmv;*.flv;*.mpg;*.mpeg;*.m2ts;*.m2t;*.mts;*.ts;*.3gp;*.3g2;*.ogv;*.mxf;*.vob;*.m2v;*.mpe;*.asf;*.f4v;*.mjpeg;*.mjpg;*.dv;*.nut;*.gif;*.webp;*.apng\0",
    "All files\0*.*\0\0",
);

/// Screen DPI (system DPI awareness: it does not change while the app runs).
fn system_dpi() -> i32 {
    unsafe {
        let dc = f::GetDC(std::ptr::null_mut());
        if dc.is_null() { return 96; }
        let dpi = f::GetDeviceCaps(dc, 88); // LOGPIXELSX
        f::ReleaseDC(std::ptr::null_mut(), dc);
        if dpi > 0 { dpi } else { 96 }
    }
}

/// Layout units -> pixels. The window itself is not DPI-scaled (fixed outer size, like the Python/Tk
/// version), so control sizes follow the UI font instead: the units below were measured against the
/// Python version at 125% (a 15px Segoe UI 9pt font), where this is the identity.
fn scale(value: i32, dpi: i32) -> i32 {
    let font_px = ((9 * dpi + 36) / 72).max(1) as i64;
    ((value as i64 * font_px + 7) / 15) as i32
}

/// Default and minimum client area in physical pixels (not DPI-scaled): Tk's geometry("560x440") and
/// minsize(560, 440) in the Python version also describe the client area.
const CLIENT_W: i32 = 560;
const CLIENT_H: i32 = 440;
/// Height the window grows by when "More" opens: gap above the codec row (10) + combo (26) +
/// gap below (8) - the gap it replaces under the button row (11).
const MORE_STEP: i32 = 33;
/// Buttons are 30px high, closed combo boxes 26px (centered on the button row). Measured on the Python build.
const BTN_H: i32 = 30;
const COMBO_H: i32 = 26;
/// Closed combo height = selection field height (CB_SETITEMHEIGHT, -1) + this border allowance (measured: 6).
const COMBO_BORDER: i32 = 6;

/// Outer window size (title bar and borders included) for a client area of the given size.
fn outer_size(client_w: i32, client_h: i32) -> (i32, i32) {
    let mut rect = f::RECT { left: 0, top: 0, right: client_w, bottom: client_h };
    unsafe { f::AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, 0); }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Default and minimum outer window size: the same frame around a 560x440 client area as Tk's.
pub fn min_track_size() -> (i32, i32) { outer_size(CLIENT_W, CLIENT_H) }

/// Segoe UI 9pt like the Tk version (the default control font is the large bold "System" font).
fn create_ui_font(dpi: i32) -> f::HANDLE {
    let face = wide("Segoe UI");
    let height = -((9 * dpi + 36) / 72);
    unsafe { f::CreateFontW(height, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr()) }
}

/// Rough line count of a wrapped label (Segoe UI 9pt averages ~7px per character; CJK counts double).
fn wrapped_lines(text: &str, width: i32, dpi: i32) -> i32 {
    let per_line = (width / scale(8, dpi).max(1)).max(10) as usize;
    let mut total = 0usize;
    for line in text.lines() {
        let units: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
        total += units.div_ceil(per_line).max(1);
    }
    total.clamp(1, 4) as i32
}

fn child(parent: f::HWND, class: &str, text: &str, style: u32, id: usize) -> f::HWND {
    let class_w = wide(class);
    let text_w = wide(text);
    let instance = unsafe { f::GetModuleHandleW(std::ptr::null()) };
    unsafe {
        f::CreateWindowExW(0, class_w.as_ptr(), text_w.as_ptr(), WS_CHILD | WS_VISIBLE | style,
            0, 0, 20, 20, parent, id as f::HMENU, instance, std::ptr::null_mut::<c_void>())
    }
}

pub struct MainWindow {
    pub hwnd: f::HWND,
    pub preview: f::HWND,
    path: f::HWND,
    pick: f::HWND,
    apply: f::HWND,
    stop: f::HWND,
    fit_label: f::HWND,
    fit: f::HWND,
    more: f::HWND,
    codec_label: f::HWND,
    codec: f::HWND,
    codec_hint: f::HWND,
    more_open: bool,
    dpi: i32,
    font: f::HANDLE,
    path_text: RefCell<String>,
    icon: f::HICON,
    owns_icon: bool,
}

impl MainWindow {
    pub fn create(handler: host::WindowHandler) -> Result<Self, String> {
        let dpi = system_dpi();
        // 560x440 client area (like Tk's geometry); min_track_size() is the matching outer size.
        let (outer_w, outer_h) = min_track_size();
        let (screen_w, screen_h) = crate::win::util::screen_size();
        let hwnd = host::create_ui_window(WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            ((screen_w - outer_w) / 2).max(0), ((screen_h - outer_h) / 2).max(0), outer_w, outer_h,
            "LiveWallpaper", Some(handler))?;
        let preview = child(hwnd, "STATIC", "", SS_BLACKRECT, 0);
        let path = child(hwnd, "STATIC", "No video selected", SS_LEFT, 0);
        let pick = child(hwnd, "BUTTON", "Select video", BS_PUSHBUTTON | WS_TABSTOP, ID_SELECT);
        let fit_label = child(hwnd, "STATIC", "Fit mode:", SS_LEFT, 0);
        let fit = child(hwnd, "COMBOBOX", "", CBS_DROPDOWNLIST | WS_VSCROLL | WS_TABSTOP, ID_FIT);
        let apply = child(hwnd, "BUTTON", "Apply wallpaper", BS_PUSHBUTTON | WS_TABSTOP, ID_APPLY);
        let stop = child(hwnd, "BUTTON", "Stop wallpaper", BS_PUSHBUTTON | WS_TABSTOP, ID_STOP);
        let more = child(hwnd, "STATIC", "More ∨", SS_LEFT | SS_NOTIFY, ID_MORE);
        let codec_label = child(hwnd, "STATIC", "Cache codec:", SS_LEFT, 0);
        let codec = child(hwnd, "COMBOBOX", "", CBS_DROPDOWNLIST | WS_VSCROLL | WS_TABSTOP, ID_CODEC);
        let codec_hint = child(hwnd, "STATIC", "(applied on next Select/Apply)", SS_LEFT, 0);
        let font = create_ui_font(dpi);
        if !font.is_null() {
            for control in [path, pick, fit_label, fit, apply, stop, more, codec_label, codec, codec_hint] {
                unsafe { f::SendMessageW(control, WM_SETFONT, font as usize, 1); }
            }
        }
        for label in ["Fit", "Fill", "Stretch"] {
            let s = wide(label);
            unsafe { f::SendMessageW(fit, CB_ADDSTRING, 0, s.as_ptr() as isize); }
        }
        for spec in crate::media::pure::CODECS.iter() {
            let s = wide(spec.label);
            unsafe { f::SendMessageW(codec, CB_ADDSTRING, 0, s.as_ptr() as isize); }
        }
        unsafe {
            f::SendMessageW(fit, CB_SETCURSEL, 0, 0);
            f::SendMessageW(codec, CB_SETCURSEL, 0, 0);
        }

        let icon_path = crate::config::icon_path();
        let loaded = match &icon_path {
            None => std::ptr::null_mut(),
            Some(icon_path) => {
                let path = wide(icon_path);
                unsafe { f::LoadImageW(std::ptr::null_mut(), path.as_ptr(), IMAGE_ICON, 0, 0, LR_LOADFROMFILE) as f::HICON }
            }
        };
        let (icon, owns_icon) = if loaded.is_null() {
            (unsafe { f::LoadIconW(std::ptr::null_mut(), 32512usize as *const u16) }, false)
        } else { (loaded, true) };
        if !icon.is_null() {
            unsafe {
                f::SendMessageW(hwnd, WM_SETICON, ICON_BIG, icon as isize);
                f::SendMessageW(hwnd, WM_SETICON, ICON_SMALL, icon as isize);
            }
        }

        let result = Self { hwnd, preview, path, pick, apply, stop, fit_label, fit, more,
            codec_label, codec, codec_hint, more_open: false, dpi, font,
            path_text: RefCell::new("No video selected".to_string()), icon, owns_icon };
        result.layout_current();
        result.show_more(false);
        Ok(result)
    }

    pub fn layout(&self, width: i32, height: i32) {
        let d = self.dpi;
        let s = |v: i32| scale(v, d);
        let w = width.max(s(420));
        let h = height.max(s(320));
        // Expanded: the extra MORE_STEP px hold the codec row; everything else keeps its place.
        // Every number below was measured on the Python/Tk build at 125% (client area 560 wide).
        let base_h = if self.more_open { h - s(MORE_STEP) } else { h };
        let (pad, btn_h, combo_h, label_h) = (s(8), s(BTN_H), s(COMBO_H), s(20));
        let text_x = pad + s(2); // Tk labels sit 2px further right than the buttons
        let more_y = h - pad - label_h - s(1);
        let row_y = (base_h - pad - label_h - s(1)) - s(11) - btn_h;
        let path_h = label_h * wrapped_lines(&self.path_text.borrow(), w - text_x - pad, d);
        let path_y = row_y - s(10) - path_h;
        let preview_h = (path_y - s(10) - pad).max(s(130));
        let stop_w = s(111);
        let apply_w = s(119);
        let stop_x = w - pad - stop_w;
        let codec_y = row_y + btn_h + s(10);
        let combo_y = row_y + (btn_h - combo_h) / 2;
        let label_dy = (btn_h - label_h) / 2;
        let sel_h = (combo_h - s(COMBO_BORDER)) as isize;
        unsafe {
            f::MoveWindow(self.preview, pad, pad, w - pad * 2, preview_h, 1);
            f::MoveWindow(self.path, text_x, path_y, w - text_x - pad, path_h, 1);
            f::MoveWindow(self.pick, pad, row_y, s(98), btn_h, 1);
            f::MoveWindow(self.fit_label, s(120), row_y + label_dy, s(62), label_h, 1);
            f::MoveWindow(self.fit, s(188), combo_y, s(91), s(180), 1);
            f::SendMessageW(self.fit, CB_SETITEMHEIGHT, usize::MAX, sel_h);
            f::MoveWindow(self.apply, stop_x - s(6) - apply_w, row_y, apply_w, btn_h, 1);
            f::MoveWindow(self.stop, stop_x, row_y, stop_w, btn_h, 1);
            f::MoveWindow(self.more, text_x, more_y, s(100), label_h, 1);
            f::MoveWindow(self.codec_label, text_x, codec_y + (combo_h - label_h) / 2, s(90), label_h, 1);
            f::MoveWindow(self.codec, s(105), codec_y, s(140), s(180), 1);
            f::SendMessageW(self.codec, CB_SETITEMHEIGHT, usize::MAX, sel_h);
            f::MoveWindow(self.codec_hint, s(254), codec_y + (combo_h - label_h) / 2, (w - s(262)).max(s(120)), label_h, 1);
        }
    }

    fn layout_current(&self) {
        let mut rect = f::RECT::default();
        if unsafe { f::GetClientRect(self.hwnd, &mut rect) } != 0 {
            self.layout(rect.right - rect.left, rect.bottom - rect.top);
        }
    }

    fn show_more(&self, visible: bool) {
        let cmd = if visible { SW_SHOW } else { SW_HIDE };
        unsafe {
            for hwnd in [self.codec_label, self.codec, self.codec_hint] { f::ShowWindow(hwnd, cmd); }
            let label = wide(if visible { "More ∧" } else { "More ∨" });
            f::SetWindowTextW(self.more, label.as_ptr());
        }
    }

    pub fn toggle_more(&mut self) {
        self.more_open = !self.more_open;
        // Keep a maximized window maximized. In normal state, preserve the user's current
        // width/height and add/remove only the codec row's MORE_STEP px.
        unsafe {
            if f::IsZoomed(self.hwnd) == 0 {
                let mut rect = f::RECT::default();
                if f::GetWindowRect(self.hwnd, &mut rect) != 0 {
                    let width = rect.right - rect.left;
                    let step = scale(MORE_STEP, self.dpi);
                    let delta = if self.more_open { step } else { -step };
                    let height = (rect.bottom - rect.top + delta).max(min_track_size().1);
                    f::SetWindowPos(self.hwnd, std::ptr::null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER);
                }
            }
        }
        self.show_more(self.more_open);
        self.layout_current();
    }

    pub fn set_path(&self, text: &str) {
        *self.path_text.borrow_mut() = text.to_string();
        let w = wide(text);
        unsafe { f::SetWindowTextW(self.path, w.as_ptr()); }
        self.layout_current(); // the label height depends on the number of lines
    }

    pub fn set_busy(&self, busy: bool) {
        unsafe { f::EnableWindow(self.pick, (!busy) as i32); f::EnableWindow(self.apply, (!busy) as i32); }
    }

    pub fn set_stop_enabled(&self, enabled: bool) { unsafe { f::EnableWindow(self.stop, enabled as i32); } }

    pub fn set_fit_mode(&self, mode: FitMode) {
        unsafe { f::SendMessageW(self.fit, CB_SETCURSEL, mode.index(), 0); }
    }

    pub fn fit_mode_index(&self) -> usize {
        unsafe { f::SendMessageW(self.fit, CB_GETCURSEL, 0, 0).max(0) as usize }
    }

    pub fn set_codec(&self, codec: Codec) {
        unsafe { f::SendMessageW(self.codec, CB_SETCURSEL, codec.index(), 0); }
    }

    pub fn codec_index(&self) -> usize {
        unsafe { f::SendMessageW(self.codec, CB_GETCURSEL, 0, 0).max(0) as usize }
    }

    pub fn show(&self) { unsafe { f::ShowWindow(self.hwnd, SW_SHOW); f::SetForegroundWindow(self.hwnd); } }
    pub fn hide(&self) { unsafe { f::ShowWindow(self.hwnd, SW_HIDE); } }
    pub fn is_visible(&self) -> bool { unsafe { f::IsWindowVisible(self.hwnd) != 0 } }
    /// Visible and not minimized: the preview should be playing only in this state.
    pub fn is_showing(&self) -> bool { unsafe { f::IsWindowVisible(self.hwnd) != 0 && f::IsIconic(self.hwnd) == 0 } }

    pub fn pick_file(&self, initial_dir: Option<&str>) -> Option<String> {
        let filter: Vec<u16> = VIDEO_FILE_FILTER.encode_utf16().collect();
        let title = wide("Select video");
        let initial = initial_dir.map(wide);
        let mut file = vec![0u16; 32768];
        let mut ofn: f::OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<f::OPENFILENAMEW>() as u32;
        ofn.hwndOwner = self.hwnd;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file.as_mut_ptr();
        ofn.nMaxFile = file.len() as u32;
        ofn.lpstrInitialDir = initial.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR;
        if unsafe { f::GetOpenFileNameW(&mut ofn) } != 0 { Some(from_wide(&file)) } else { None }
    }
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        if !self.font.is_null() {
            unsafe { f::DeleteObject(self.font); }
            self.font = std::ptr::null_mut();
        }
        if self.owns_icon && !self.icon.is_null() {
            unsafe { f::DestroyIcon(self.icon); }
            self.icon = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VIDEO_FILE_FILTER;

    #[test]
    fn open_dialog_filter_uses_semicolon_separated_patterns() {
        let fields: Vec<&str> = VIDEO_FILE_FILTER.split('\0').collect();
        assert_eq!(fields[0], "Video and animated images");
        assert!(fields[1].split(';').count() >= 25);
        for extension in ["*.mp4", "*.mkv", "*.mov", "*.avi", "*.webm", "*.mxf", "*.vob", "*.m2v", "*.gif", "*.webp", "*.apng"] {
            assert!(fields[1].split(';').any(|pattern| pattern == extension), "missing {extension}");
        }
        assert!(fields[1].split(';').all(|pattern| {
            pattern.starts_with("*.") && !pattern.contains(' ')
        }));
        assert_eq!(fields[2], "All files");
        assert_eq!(fields[3], "*.*");
        assert!(VIDEO_FILE_FILTER.ends_with("\0\0"));
    }
}
