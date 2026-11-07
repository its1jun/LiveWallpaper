//! Win32 유틸: 단일 인스턴스 뮤텍스, error_box, 바탕화면 get/set, monitor_count 등.
//!
//! 순수 함수(`wide`, `from_wide`, `mutex_outcome`)는 모든 OS 에서 컴파일/테스트된다.
//! 나머지는 `#[cfg(windows)]`.

// ---- 상수 ----
pub const SPI_GETDESKWALLPAPER: u32 = 0x0073;
pub const SPI_SETDESKWALLPAPER: u32 = 0x0014;
/// SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE
pub const SPIF_UPDATE_AND_BROADCAST: u32 = 0x01 | 0x02;
pub const SM_CXSCREEN: i32 = 0;
pub const SM_CYSCREEN: i32 = 1;
pub const SM_CMONITORS: i32 = 80;
pub const MB_ICONERROR: u32 = 0x10;
pub const MB_ICONWARNING: u32 = 0x30;
pub const MB_YESNO: u32 = 0x04;
pub const IDYES: i32 = 6;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const MUTEX_NAME: &str = "LiveWallpaper.SingleInstance";
pub const APP_TITLE: &str = "LiveWallpaper";
/// get_desktop_wallpaper 버퍼 크기 (wchar 수)
const WALLPAPER_BUF: usize = 1024;

// ---- 순수 함수 ----

/// UTF-8 → NUL 로 끝나는 UTF-16. (원본 문자열에 NUL 이 있으면 Win32 는 거기서 끊는다.)
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 버퍼 → String. 첫 NUL 까지만, 잘못된 서로게이트는 U+FFFD.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// `CreateMutexW` 결과 해석. `last_error` 는 **CreateMutexW 직후** 읽은 GetLastError.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MutexOutcome {
    Failed,
    AlreadyRunning,
    Acquired,
}

pub fn mutex_outcome(handle_is_null: bool, last_error: u32) -> MutexOutcome {
    if handle_is_null {
        MutexOutcome::Failed
    } else if last_error == ERROR_ALREADY_EXISTS {
        MutexOutcome::AlreadyRunning
    } else {
        MutexOutcome::Acquired
    }
}

// ---- Win32 ----

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::win::ffi;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 프로세스 우선순위를 BELOW_NORMAL 로 낮춘다. 배경화면은 다른 작업을 방해하면 안 된다. 실패는 무시.
    /// (EcoQoS 전원 스로틀링은 쓰지 않는다: 디코드/렌더 스레드가 늦어져 프레임이 떨어질 수 있다.)
    pub fn lower_priority() {
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        unsafe { let _ = ffi::SetPriorityClass(ffi::GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS); }
    }

    /// 작업 집합(Working Set)을 비운다. mpv 를 끈 직후에 불러 작업 관리자에 보이는 메모리를 줄인다.
    /// 필요한 페이지는 다시 읽히므로 동작에는 영향이 없다. 실패는 무시.
    pub fn trim_working_set() {
        unsafe { let _ = ffi::SetProcessWorkingSetSize(ffi::GetCurrentProcess(), usize::MAX, usize::MAX); }
    }

    /// 소유 중인 뮤텍스 핸들 (0 = 없음). 앱 종료까지 유지.
    static MUTEX: AtomicUsize = AtomicUsize::new(0);

    /// 창을 만들기 전에 호출. 안 하면 화면 크기가 배율만큼 줄어든 값으로 보인다.
    /// builder/build.rs 의 매니페스트(System DPI)와 같은 값이다. 리소스 컴파일러가 없어 매니페스트가 빠진 빌드의 대비책이며, 실패(이미 설정됨 등)는 무시.
    pub fn enable_dpi_awareness() {
        unsafe {
            let _ = ffi::SetProcessDpiAwareness(1); // PROCESS_SYSTEM_DPI_AWARE
        }
    }

    fn show_box(owner: ffi::HWND, title: &str, text: &str, style: u32) -> i32 {
        let (t, c) = (wide(text), wide(title));
        unsafe { ffi::MessageBoxW(owner, t.as_ptr(), c.as_ptr(), style) }
    }

    /// 오류 대화상자 (오류 아이콘, 소유 창 없음). 확인을 누를 때까지 블록.
    pub fn error_box(title: &str, text: &str) {
        show_box(ptr::null_mut(), title, text, MB_ICONERROR);
    }

    /// 예/아니오 확인 대화상자. "예" 이면 `true`.
    pub fn confirm(owner: ffi::HWND, title: &str, text: &str) -> bool {
        show_box(owner, title, text, MB_ICONWARNING | MB_YESNO) == IDYES
    }

    /// 현재 Windows 바탕화면 경로. 단색이거나 실패하면 `None`.
    /// Uncertain: 슬라이드쇼에서는 TranscodedWallpaper 경로가 올 수 있다.
    pub fn get_desktop_wallpaper() -> Option<String> {
        let mut buf = [0u16; WALLPAPER_BUF];
        let ok = unsafe {
            ffi::SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                WALLPAPER_BUF as u32,
                buf.as_mut_ptr() as *mut c_void,
                0,
            )
        };
        if ok != 0 {
            Some(from_wide(&buf)).filter(|s| !s.is_empty())
        } else {
            None
        }
    }

    /// Windows 바탕화면을 `path` 로 바꾼다 (프로필에 저장됨 → 복원은 snapshot.rs 가 책임).
    pub fn set_desktop_wallpaper(path: &str) -> bool {
        let mut w = wide(path);
        let ok = unsafe {
            ffi::SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                w.as_mut_ptr() as *mut c_void,
                SPIF_UPDATE_AND_BROADCAST,
            )
        };
        ok != 0
    }

    /// 연결된 모니터 수 (가상 화면 기준). 실패하면 0 (1 로 간주하지 않음).
    pub fn monitor_count() -> u32 {
        let n = unsafe { ffi::GetSystemMetrics(SM_CMONITORS) };
        n.max(0) as u32
    }

    /// 주 모니터 화면 크기 (DPI 인식 이후의 값).
    pub fn screen_size() -> (i32, i32) {
        unsafe { (ffi::GetSystemMetrics(SM_CXSCREEN), ffi::GetSystemMetrics(SM_CYSCREEN)) }
    }

    /// 단일 실행 뮤텍스. `Ok(true)` = 내가 첫 인스턴스, `Ok(false)` = 이미 실행 중, `Err` = 생성 실패.
    /// 이미 획득했다면 `Ok(true)`.
    pub fn acquire_instance() -> Result<bool, String> {
        if MUTEX.load(Ordering::SeqCst) != 0 {
            return Ok(true);
        }
        let name = wide(MUTEX_NAME);
        // CreateMutexW documents ERROR_ALREADY_EXISTS for an existing name but may
        // leave last-error untouched on success. Clear stale thread-local error first.
        unsafe { ffi::SetLastError(0); }
        let handle = unsafe { ffi::CreateMutexW(ptr::null(), 1, name.as_ptr()) };
        // Read immediately: any later Win32 call could overwrite the result.
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        match mutex_outcome(handle.is_null(), err) {
            MutexOutcome::Failed => Err(format!("CreateMutexW failed (error {err})")),
            MutexOutcome::AlreadyRunning => {
                unsafe { ffi::CloseHandle(handle) };
                Ok(false)
            }
            MutexOutcome::Acquired => {
                MUTEX.store(handle as usize, Ordering::SeqCst);
                Ok(true)
            }
        }
    }

    /// `acquire_instance` 를 호출한 스레드(메인)에서 종료 시 호출. 여러 번 불러도 안전.
    pub fn release_instance() {
        let h = MUTEX.swap(0, Ordering::SeqCst);
        if h != 0 {
            unsafe {
                ffi::ReleaseMutex(h as ffi::HANDLE);
                ffi::CloseHandle(h as ffi::HANDLE);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::*;

