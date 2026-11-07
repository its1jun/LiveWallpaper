//! Desktop coverage detection. The grid algorithm is platform-independent and tested on CI.

/// Approximate the fraction of the desktop covered by the union of window rectangles.
pub fn covered_ratio(rects: &[(i32, i32, i32, i32)], width: i32, height: i32,
                     cols: usize, rows: usize) -> f64 {
    if width <= 0 || height <= 0 || cols == 0 || rows == 0 { return 0.0; }
    let tw = width as f64 / cols as f64;
    let th = height as f64 / rows as f64;
    let total = cols.saturating_mul(rows);
    if total == 0 { return 0.0; }
    let mut cells = vec![false; total];
    let mut count = 0usize;
    for &(left, top, right, bottom) in rects {
        let c0 = ((left as f64 / tw - 0.5).ceil() as isize).max(0);
        let c1 = ((right as f64 / tw - 0.5).ceil() as isize - 1).min(cols as isize - 1);
        let r0 = ((top as f64 / th - 0.5).ceil() as isize).max(0);
        let r1 = ((bottom as f64 / th - 0.5).ceil() as isize - 1).min(rows as isize - 1);
        if c0 > c1 || r0 > r1 { continue; }
        let c0 = c0 as usize; let c1 = c1 as usize;
        for y in r0 as usize..=r1 as usize {
            for x in c0..=c1 {
                let i = y * cols + x;
                if !cells[i] { cells[i] = true; count += 1; }
            }
        }
        if count >= total { break; }
    }
    count as f64 / total as f64
}

pub fn covered_ratio_default(rects: &[(i32, i32, i32, i32)], width: i32, height: i32) -> f64 {
    covered_ratio(rects, width, height, 32, 18)
}

#[cfg(windows)]
mod imp {
    use super::covered_ratio_default;
    use crate::win::consts::{GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT};
    use crate::win::{ffi as f, host::window_class_name};
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    const SPI_GETWORKAREA: u32 = 0x0030;
    const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
    const DWMWA_CLOAKED: u32 = 14;
    const BLOCKED_STATES: [i32; 4] = [1, 2, 3, 4];
    const SHELL_CLASSES: [&str; 6] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd",
        "NotifyIconOverflowWindow", "TopLevelWindowForOverflowXamlIsland"];

    fn shell_class(hwnd: f::HWND) -> bool { SHELL_CLASSES.iter().any(|s| *s == window_class_name(hwnd)) }

    fn work_area() -> (i32, i32, i32, i32) {
        let mut r = f::RECT::default();
        if unsafe { f::SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut c_void, 0) } != 0 {
            (r.left, r.top, r.right, r.bottom)
        } else {
            let (w, h) = crate::win::util::screen_size(); (0, 0, w, h)
        }
    }

    fn visible_rect(hwnd: f::HWND) -> (i32, i32, i32, i32) {
        let mut r = f::RECT::default();
        let result = unsafe { f::DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut c_void, std::mem::size_of::<f::RECT>() as u32) };
        if result != 0 { unsafe { f::GetWindowRect(hwnd, &mut r); } }
        (r.left, r.top, r.right, r.bottom)
    }

    /// `EnumWindows` 로 넘기는 작업 상태. 값싼 검사로 먼저 걸러 내려고 기준값도 함께 들고 간다.
    struct Collect {
        /// (left, top, right, bottom) — 작업 영역 원점 기준
        rects: Vec<(i32, i32, i32, i32)>,
        origin: (i32, i32),
        min_area: f64,
        own_pid: u32,
    }

    unsafe extern "system" fn collect(hwnd: f::HWND, param: isize) -> i32 {
        // Vec growth and class-name conversion may allocate; contain panics at the FFI boundary.
        catch_unwind(AssertUnwindSafe(|| {
            let ctx = &mut *(param as *mut Collect);
            if f::IsWindowVisible(hwnd) == 0 || f::IsIconic(hwnd) != 0 { return 1; }
            let mut cloaked = 0u32;
            if f::DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4) == 0 && cloaked != 0 {
                return 1;
            }
            let ex = f::GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if ex & ((WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE) as isize) != 0 { return 1; }
            // 크기를 먼저 거른다: 작은 창이 대부분이라 클래스 이름(문자열 할당)을 만들 일이 거의 없어진다.
            let (l, t, r, b) = visible_rect(hwnd);
            let area = (r - l) as f64 * (b - t) as f64;
            if area < ctx.min_area { return 1; }
            let mut pid = 0u32;
            f::GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == ctx.own_pid { return 1; }
            if shell_class(hwnd) { return 1; }
            ctx.rects.push((l - ctx.origin.0, t - ctx.origin.1, r - ctx.origin.0, b - ctx.origin.1));
            1
        })).unwrap_or(1)
    }

    fn collect_rects(area: (i32, i32, i32, i32)) -> Vec<(i32, i32, i32, i32)> {
        let (al, at, ar, ab) = area;
        let mut ctx = Collect {
            rects: Vec::new(),
            origin: (al, at),
            min_area: ((ar - al) as i64 * (ab - at) as i64) as f64 * 0.01,
            own_pid: unsafe { f::GetCurrentProcessId() },
        };
        unsafe { f::EnumWindows(Some(collect), &mut ctx as *mut Collect as isize); }
        ctx.rects
    }

    fn foreground_covers() -> bool {
        let hwnd = unsafe { f::GetForegroundWindow() };
        if hwnd.is_null() || unsafe { f::IsIconic(hwnd) } != 0 || shell_class(hwnd) { return false; }
        let mut r = f::RECT::default();
        if unsafe { f::GetWindowRect(hwnd, &mut r) } == 0 { return false; }
        let (sw, sh) = crate::win::util::screen_size();
        let cx = (r.left + r.right) / 2; let cy = (r.top + r.bottom) / 2;
        if cx < 0 || cy < 0 || cx >= sw || cy >= sh { return false; }
        (unsafe { f::IsZoomed(hwnd) != 0 }) || r.left <= 0 && r.top <= 0 && r.right >= sw && r.bottom >= sh
    }

    pub fn desktop_is_covered(threshold: f64) -> bool {
        if foreground_covers() { return true; }
        if !(threshold > 0.0 && threshold <= 1.0) { return false; }
        let area = work_area();
        let rects = collect_rects(area);
        covered_ratio_default(&rects, area.2 - area.0, area.3 - area.1) >= threshold
    }

    pub fn notification_state() -> i32 {
        let mut state = 0i32;
        if unsafe { f::SHQueryUserNotificationState(&mut state) } == 0 { state } else { 0 }
    }

    pub fn battery_saver_on() -> bool {
        let mut status = unsafe { std::mem::zeroed::<f::SYSTEM_POWER_STATUS>() };
        unsafe { f::GetSystemPowerStatus(&mut status) != 0 && status.SystemStatusFlag & 1 != 0 }
    }

    pub fn notification_blocked() -> bool { BLOCKED_STATES.contains(&notification_state()) }
}

#[cfg(windows)]
pub use imp::{battery_saver_on, desktop_is_covered, notification_blocked};

