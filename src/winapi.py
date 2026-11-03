"""Windows 전용 호출 모음 (ctypes만 사용, UI 프레임워크 무관).

  1. 창 만들기/부착   create_host_window, attach_to_desktop   ← Windows 업데이트로 깨지면 여기만 수정
  2. 가림 판단        desktop_is_covered, covered_ratio
  3. 시스템 상태      notification_state, power_status, SystemWatcher(창/전원/세션 변화를 알림으로 받음)
  4. 트레이 아이콘    Tray
  5. 단일 실행 잠금   acquire_instance / release_instance
  6. 시작 프로그램    autostart_*

Uncertain: 부착 방식 A/B, 가림 판단, 전원 알림 모두 Windows 11 실기기에서 미검증.
"""
import ctypes
import math
import ntpath
import os
import sys
import uuid
from ctypes import wintypes

user32 = ctypes.WinDLL("user32")
dwmapi = ctypes.WinDLL("dwmapi")
shell32 = ctypes.WinDLL("shell32")
shcore = ctypes.WinDLL("shcore")
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32")
wtsapi32 = ctypes.WinDLL("wtsapi32")

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
WNDPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, wintypes.HWND, wintypes.UINT,
                             wintypes.WPARAM, wintypes.LPARAM)

_HWND, _INT, _UINT = wintypes.HWND, ctypes.c_int, wintypes.UINT
for _name, _res, _args in [
    ("FindWindowW", _HWND, [wintypes.LPCWSTR, wintypes.LPCWSTR]),
    ("FindWindowExW", _HWND, [_HWND, _HWND, wintypes.LPCWSTR, wintypes.LPCWSTR]),
    ("SendMessageTimeoutW", ctypes.c_ssize_t,
     [_HWND, _UINT, wintypes.WPARAM, wintypes.LPARAM, _UINT, _UINT, ctypes.POINTER(ctypes.c_size_t)]),
    ("SetParent", _HWND, [_HWND, _HWND]),
    ("GetParent", _HWND, [_HWND]),
    ("EnumWindows", wintypes.BOOL, [WNDENUMPROC, wintypes.LPARAM]),
    ("GetClientRect", wintypes.BOOL, [_HWND, ctypes.POINTER(wintypes.RECT)]),
    ("GetWindowRect", wintypes.BOOL, [_HWND, ctypes.POINTER(wintypes.RECT)]),
    ("SetWindowPos", wintypes.BOOL, [_HWND, _HWND, _INT, _INT, _INT, _INT, _UINT]),
    ("GetWindowLongPtrW", ctypes.c_ssize_t, [_HWND, _INT]),
    ("SetWindowLongPtrW", ctypes.c_ssize_t, [_HWND, _INT, ctypes.c_ssize_t]),
    ("SetLayeredWindowAttributes", wintypes.BOOL,
     [_HWND, wintypes.DWORD, ctypes.c_ubyte, wintypes.DWORD]),
    ("IsWindowVisible", wintypes.BOOL, [_HWND]),
    ("IsWindow", wintypes.BOOL, [_HWND]),
    ("IsZoomed", wintypes.BOOL, [_HWND]),
    ("IsIconic", wintypes.BOOL, [_HWND]),
    ("GetForegroundWindow", _HWND, []),
    ("GetClassNameW", _INT, [_HWND, wintypes.LPWSTR, _INT]),
    ("GetWindowThreadProcessId", wintypes.DWORD, [_HWND, ctypes.POINTER(wintypes.DWORD)]),
    ("SystemParametersInfoW", wintypes.BOOL, [_UINT, _UINT, ctypes.c_void_p, _UINT]),
    ("ShowWindow", wintypes.BOOL, [_HWND, _INT]),
    ("DestroyWindow", wintypes.BOOL, [_HWND]),
    ("DefWindowProcW", ctypes.c_ssize_t, [_HWND, _UINT, wintypes.WPARAM, wintypes.LPARAM]),
    ("CreateWindowExW", _HWND,
     [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.DWORD, _INT, _INT, _INT, _INT,
      _HWND, wintypes.HANDLE, wintypes.HANDLE, ctypes.c_void_p]),
    ("RegisterPowerSettingNotification", ctypes.c_void_p,
     [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]),
    ("UnregisterPowerSettingNotification", wintypes.BOOL, [ctypes.c_void_p]),
    ("LoadIconW", ctypes.c_void_p, [wintypes.HANDLE, ctypes.c_void_p]),
    ("LoadImageW", ctypes.c_void_p, [wintypes.HANDLE, wintypes.LPCWSTR, _UINT, _INT, _INT, _UINT]),
    ("CreatePopupMenu", ctypes.c_void_p, []),
    ("AppendMenuW", wintypes.BOOL, [ctypes.c_void_p, _UINT, ctypes.c_size_t, wintypes.LPCWSTR]),
    ("TrackPopupMenu", _INT, [ctypes.c_void_p, _UINT, _INT, _INT, _INT, _HWND, ctypes.c_void_p]),
    ("DestroyMenu", wintypes.BOOL, [ctypes.c_void_p]),
    ("GetCursorPos", wintypes.BOOL, [ctypes.POINTER(wintypes.POINT)]),
    ("SetForegroundWindow", wintypes.BOOL, [_HWND]),
    ("PostMessageW", wintypes.BOOL, [_HWND, _UINT, wintypes.WPARAM, wintypes.LPARAM]),
    ("AllowSetForegroundWindow", wintypes.BOOL, [wintypes.DWORD]),
    ("PeekMessageW", wintypes.BOOL, [ctypes.c_void_p, _HWND, _UINT, _UINT, _UINT]),
]:
    _f = getattr(user32, _name)
    _f.restype, _f.argtypes = _res, _args
dwmapi.DwmGetWindowAttribute.restype = ctypes.c_long
dwmapi.DwmGetWindowAttribute.argtypes = [_HWND, wintypes.DWORD, ctypes.c_void_p, wintypes.DWORD]
shell32.Shell_NotifyIconW.restype = wintypes.BOOL
shell32.SHQueryUserNotificationState.restype = ctypes.c_long
shell32.SHQueryUserNotificationState.argtypes = [ctypes.POINTER(ctypes.c_int)]
kernel32.GetSystemPowerStatus.argtypes = [ctypes.c_void_p]
kernel32.GetModuleHandleW.restype = wintypes.HANDLE
kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
kernel32.CloseHandle.restype, kernel32.CloseHandle.argtypes = wintypes.BOOL, [wintypes.HANDLE]
kernel32.ReleaseMutex.restype, kernel32.ReleaseMutex.argtypes = wintypes.BOOL, [wintypes.HANDLE]
gdi32.GetStockObject.restype = wintypes.HANDLE
gdi32.GetStockObject.argtypes = [_INT]

GWL_STYLE, GWL_EXSTYLE = -16, -20
WS_CHILD, WS_POPUP = 0x40000000, 0x80000000
WS_EX_LAYERED, WS_EX_TRANSPARENT = 0x00080000, 0x00000020
WS_EX_TOOLWINDOW, WS_EX_NOACTIVATE = 0x00000080, 0x08000000
SWP_NOACTIVATE, SWP_SHOWWINDOW = 0x0010, 0x0040
HWND_BOTTOM, HWND_MESSAGE = wintypes.HWND(1), wintypes.HWND(-3)
BLACK_BRUSH, LWA_ALPHA = 4, 0x2
SM_CXSCREEN, SM_CYSCREEN = 0, 1
SM_CMONITORS = 80
SW_HIDE, SW_SHOWNA = 0, 8


def enable_dpi_awareness():
    """tkinter 창을 만들기 전에 호출. 안 하면 화면 크기가 배율만큼 줄어든 값으로 보임."""
    try:
        shcore.SetProcessDpiAwareness(1)  # system DPI aware
    except Exception:  # noqa: BLE001
        pass


# ======================= 1. 창 만들기 / 바탕화면 부착 =======================
class _WNDCLASS(ctypes.Structure):
    _fields_ = [("style", _UINT), ("lpfnWndProc", WNDPROC), ("cbClsExtra", _INT),
                ("cbWndExtra", _INT), ("hInstance", wintypes.HANDLE), ("hIcon", wintypes.HANDLE),
                ("hCursor", wintypes.HANDLE), ("hbrBackground", wintypes.HANDLE),
                ("lpszMenuName", wintypes.LPCWSTR), ("lpszClassName", wintypes.LPCWSTR)]


user32.RegisterClassW.restype = wintypes.WORD
user32.RegisterClassW.argtypes = [ctypes.POINTER(_WNDCLASS)]

WM_POWERBROADCAST, PBT_POWERSETTINGCHANGE = 0x0218, 0x8013
_CLASS_NAME = "LiveWallpaperWindow"
_hooks = {}   # hwnd -> fn(msg, wparam, lparam): 처리했으면 결과(int), 아니면 None
_class_ready = False


def _wndproc(hwnd, msg, wparam, lparam):
    hook = _hooks.get(hwnd)
    if hook:
        try:
            r = hook(msg, wparam, lparam)
        except Exception:  # noqa: BLE001  WndProc에서 예외가 새면 안 됨
            r = 0
        if r is not None:
            return r
    return user32.DefWindowProcW(hwnd, msg, wparam, lparam)


_wndproc_ref = WNDPROC(_wndproc)  # GC 되지 않도록 모듈에 보관


def _ensure_class():
    global _class_ready
    if _class_ready:
        return
    wc = _WNDCLASS()
    wc.lpfnWndProc = _wndproc_ref
    wc.hInstance = kernel32.GetModuleHandleW(None)
    wc.hbrBackground = gdi32.GetStockObject(BLACK_BRUSH)
    wc.lpszClassName = _CLASS_NAME
    _class_ready = bool(user32.RegisterClassW(ctypes.byref(wc)))  # 실패(0)면 다음 호출에서 다시 시도


def _create(ex_style, style, parent, size, title="LiveWallpaper"):
    _ensure_class()
    hinst = kernel32.GetModuleHandleW(None)
    return user32.CreateWindowExW(ex_style, _CLASS_NAME, title, style, 0, 0,
                                  size[0], size[1], parent, None, hinst, None) or 0


def create_host_window() -> int:
    """mpv가 그릴 검은 창(HWND). 아직 숨김 상태이며 attach_to_desktop()이 부착하고 보여준다. 실패하면 0."""
    return _create(WS_EX_TOOLWINDOW, WS_POPUP,
                   None, (user32.GetSystemMetrics(SM_CXSCREEN), user32.GetSystemMetrics(SM_CYSCREEN)))


PUMP_SENT_MESSAGES = False   # Uncertain: True로 하면 Tk 콜백 안에서 네이티브 메시지를 처리해 재진입 위험이 있다 (미검증)


def pump_sent_messages():
    """다른 스레드가 이 스레드의 창에 보낸 동기 메시지(SendMessage)를 처리한다 (player.suspend가 호출).
    원래는 이 함수가 없어서 호출이 조용히 실패(AttributeError가 삼켜짐)하고 있었다. 한 번 구현해 켰더니
    Tk 콜백 안에서 네이티브 메시지가 처리되는 경로가 늘어나므로, 크래시 원인이 확인될 때까지 꺼 둔다
    (= 원래와 같은 동작). 켜려면 PUMP_SENT_MESSAGES = True."""
    if not PUMP_SENT_MESSAGES:
        return
    msg = wintypes.MSG()
    user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, 0)  # PM_NOREMOVE: 대기 중인 '보낸' 메시지만 처리됨


def host_alive(hwnd: int) -> bool:
    """Explorer가 재시작되면 Progman의 자식인 호스트 창도 함께 사라진다."""
    return bool(hwnd) and bool(user32.IsWindow(hwnd))


_mutex = None


def acquire_instance() -> bool:
    """Named mutex를 소유한 프로세스 하나만 앱을 시작할 수 있게 한다.

    CreateMutex의 initial_owner=True로 잠금 획득과 프로세스 선출을 한 번에 처리한다.
    핸들은 앱이 종료될 때까지 유지한다.
    """
    global _mutex
    if _mutex:
        return True

    kernel32.CreateMutexW.restype = wintypes.HANDLE
    kernel32.CreateMutexW.argtypes = [ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR]
    ctypes.set_last_error(0)
    handle = kernel32.CreateMutexW(None, True, "LiveWallpaper.SingleInstance")
    error = ctypes.get_last_error()
    if not handle:
        raise ctypes.WinError(error)
    if error == 183:  # ERROR_ALREADY_EXISTS: 다른 프로세스가 먼저 시작했다.
        kernel32.CloseHandle(handle)
        return False
    _mutex = handle
    return True


def release_instance():
    """acquire_instance()를 호출한 메인 스레드에서 종료 시 호출한다."""
    global _mutex
    if not _mutex:
        return
    try:
        kernel32.ReleaseMutex(_mutex)
    finally:
        kernel32.CloseHandle(_mutex)
        _mutex = None


def message_box(text: str):
    user32.MessageBoxW(None, text, "LiveWallpaper", 0x10)  # MB_ICONERROR


def destroy_window(hwnd: int):
    if hwnd:
        user32.DestroyWindow(hwnd)


def hide_window(hwnd: int):
    """호스트 창을 파괴하지 않고 숨긴다 (정적 배경 전환용). 부착 상태는 그대로 유지된다."""
    if hwnd:
        user32.ShowWindow(hwnd, SW_HIDE)


def show_window(hwnd: int):
    """숨겼던 호스트 창을 활성화 없이 다시 표시한다."""
    if hwnd:
        user32.ShowWindow(hwnd, SW_SHOWNA)


def _find_progman():
    progman = user32.FindWindowW("Progman", None)
    if progman:
        res = ctypes.c_size_t()  # WorkerW 생성을 요청하는 비공식 메시지
        user32.SendMessageTimeoutW(progman, 0x052C, 0xD, 0x1, 0, 1000, ctypes.byref(res))
    return progman


def _make_child_style(hwnd):
    style = user32.GetWindowLongPtrW(hwnd, GWL_STYLE)
    user32.SetWindowLongPtrW(hwnd, GWL_STYLE, (style & ~WS_POPUP) | WS_CHILD)


def _fit_to_parent(hwnd, parent, insert_after):
    rect = wintypes.RECT()
    user32.GetClientRect(parent, ctypes.byref(rect))
    w, h = rect.right - rect.left, rect.bottom - rect.top
    if w <= 0 or h <= 0:
        w, h = user32.GetSystemMetrics(SM_CXSCREEN), user32.GetSystemMetrics(SM_CYSCREEN)
    user32.SetWindowPos(hwnd, insert_after, 0, 0, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW)


def _make_layered(hwnd):
    """24H2+ 'raised desktop'에서는 DWM이 일반 자식 창을 그리지 않음.
    SetParent 전에 layered(alpha 255)로 만들어야 한다는 것이 여러 오픈소스의 공통 결론."""
    ex = user32.GetWindowLongPtrW(hwnd, GWL_EXSTYLE)
    user32.SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED)
    user32.SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA)


def _attach_a(hwnd):
    """Windows 11 24H2+: layered 자식으로 Progman에 넣고 SHELLDLL_DefView(아이콘) 바로 아래에 배치."""
    progman = _find_progman()
    if not progman:
        return False, "Progman window not found"
    defview = user32.FindWindowExW(progman, None, "SHELLDLL_DefView", None)
    if not defview:
        return False, "No icon layer under Progman (this PC likely uses the legacy layout)"
    _make_layered(hwnd)  # 반드시 SetParent 이전
    _make_child_style(hwnd)
    if not user32.SetParent(hwnd, progman):
        return False, "SetParent failed"
    _fit_to_parent(hwnd, progman, defview)  # defview 바로 뒤 = 아이콘 아래
    return (user32.GetParent(hwnd) == progman and bool(user32.IsWindowVisible(hwnd))), \
        "Method A (Progman child, layered)"


def _attach_b(hwnd):
    """기존 방식: 아이콘 층을 가진 창 다음의 WorkerW에 부착."""
    if not _find_progman():
        return False, "Progman window not found"
    found = []

    def cb(top, _lparam):
        if user32.FindWindowExW(top, None, "SHELLDLL_DefView", None):
            worker = user32.FindWindowExW(None, top, "WorkerW", None)
            if worker:
                found.append(worker)
        return True

    user32.EnumWindows(WNDENUMPROC(cb), 0)
    if not found:
        return False, "WorkerW window not found"
    _make_child_style(hwnd)
    if not user32.SetParent(hwnd, found[0]):
        return False, "SetParent failed"
    _fit_to_parent(hwnd, found[0], HWND_BOTTOM)
    return (user32.GetParent(hwnd) == found[0]), "Method B (WorkerW sibling)"


def attach_to_desktop(hwnd: int):
    """방식 A → B 순서로 시도. 반환: (성공여부, 메시지). 예외가 나도 프로그램은 죽지 않음."""
    errors = []
    h = wintypes.HWND(hwnd)
    for method in (_attach_a, _attach_b):
        try:
            ok, msg = method(h)
            if ok:
                return True, msg
            errors.append(f"{method.__name__}: {msg}")
        except Exception as e:  # noqa: BLE001
            errors.append(f"{method.__name__}: {e}")
    return False, " / ".join(errors)


# ======================= 1-b. Windows 바탕화면 이미지 (정적 배경 전환용) =======================
SPI_GETDESKWALLPAPER, SPI_SETDESKWALLPAPER = 0x0073, 0x0014
SPIF_UPDATEINIFILE, SPIF_SENDWININICHANGE = 0x01, 0x02


def get_desktop_wallpaper() -> str:
    """현재 Windows 바탕화면 이미지 경로. 단색 배경이거나 실패하면 빈 문자열.
    Uncertain: 슬라이드쇼/일부 Windows 버전에서는 원본이 아닌 TranscodedWallpaper 경로가 올 수 있다."""
    buf = ctypes.create_unicode_buffer(1024)
    if user32.SystemParametersInfoW(SPI_GETDESKWALLPAPER, len(buf), buf, 0):
        return buf.value
    return ""


def set_desktop_wallpaper(path: str) -> bool:
    """Windows 바탕화면 이미지를 path로 바꾼다. 사용자 프로필에 저장(UPDATEINIFILE)되므로
    비정상 종료 뒤에도 남는다 → 복원은 snapshot.WallpaperSwapper가 상태 파일로 책임진다."""
    buf = ctypes.create_unicode_buffer(path)
    return bool(user32.SystemParametersInfoW(
        SPI_SETDESKWALLPAPER, 0, buf, SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE))


def monitor_count() -> int:
    """연결된 모니터 수 (가상 화면 기준). 실패하면 1로 간주하지 않고 0을 돌려준다."""
    return int(user32.GetSystemMetrics(SM_CMONITORS))


# ======================= 2. 바탕화면이 가려졌는지 =======================
def covered_ratio(rects, width, height, cols=32, rows=18):
    """rects: (left, top, right, bottom) 목록 (영역 좌상단 기준, right/bottom 미포함).
    영역을 격자로 나눠 칸 중심이 창 안이면 '덮임'으로 센다 (Lively의 Grid 방식). 반환 0.0~1.0"""
    if width <= 0 or height <= 0:
        return 0.0
    tw, th = width / cols, height / rows
    total = cols * rows
    cells = bytearray(total)
    count = 0
    for left, top, right, bottom in rects:
        c0 = max(0, math.ceil(left / tw - 0.5))
        c1 = min(cols - 1, math.ceil(right / tw - 0.5) - 1)
        r0 = max(0, math.ceil(top / th - 0.5))
        r1 = min(rows - 1, math.ceil(bottom / th - 0.5) - 1)
        if c0 > c1 or r0 > r1:
            continue
        span = c1 - c0 + 1
        fill = b"\x01" * span
        for y in range(r0, r1 + 1):
            s = y * cols + c0
            count += span - sum(cells[s:s + span])  # 새로 덮인 칸만 더함
            cells[s:s + span] = fill
        if count >= total:
            break
    return count / total


SPI_GETWORKAREA = 0x0030
DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_CLOAKED = 9, 14
_SHELL = {"Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd",
          "NotifyIconOverflowWindow", "TopLevelWindowForOverflowXamlIsland"}
_MY_PID = os.getpid()


def _class_name(h) -> str:
    buf = ctypes.create_unicode_buffer(256)
    user32.GetClassNameW(h, buf, 256)
    return buf.value


def _screen_size():
    return user32.GetSystemMetrics(SM_CXSCREEN), user32.GetSystemMetrics(SM_CYSCREEN)  # 주 모니터


def _work_area():
    """작업 표시줄을 뺀 영역 (최대화 창도 작업 표시줄은 덮지 못함)."""
    r = wintypes.RECT()
    if user32.SystemParametersInfoW(SPI_GETWORKAREA, 0, ctypes.byref(r), 0):
        return r.left, r.top, r.right, r.bottom
    sw, sh = _screen_size()
    return 0, 0, sw, sh


def _foreground_covers() -> bool:
    """빠른 경로: 포그라운드 창이 최대화/전체화면이면 가려짐."""
    fg = user32.GetForegroundWindow()
    if not fg or user32.IsIconic(fg) or _class_name(fg) in _SHELL:
        return False
    r = wintypes.RECT()
    user32.GetWindowRect(fg, ctypes.byref(r))
    sw, sh = _screen_size()
    cx, cy = (r.left + r.right) // 2, (r.top + r.bottom) // 2
    if not (0 <= cx < sw and 0 <= cy < sh):
        return False  # 주 모니터 밖의 창
    fullscreen = r.left <= 0 and r.top <= 0 and r.right >= sw and r.bottom >= sh
    return bool(user32.IsZoomed(fg)) or fullscreen


def _visible_rect(h):
    """그림자(투명 테두리)를 뺀 실제 창 영역."""
    r = wintypes.RECT()
    if dwmapi.DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, ctypes.byref(r),
                                    ctypes.sizeof(r)) != 0:
        user32.GetWindowRect(h, ctypes.byref(r))
    return r.left, r.top, r.right, r.bottom


def _collect_rects(area):
    al, at, ar, ab = area
    min_area = (ar - al) * (ab - at) * 0.01  # 작업 영역의 1% 미만인 창은 무시
    rects = []

    def cb(h, _lparam):
        if not user32.IsWindowVisible(h) or user32.IsIconic(h):
            return True
        cloaked = wintypes.DWORD(0)  # 다른 가상 데스크톱 / 숨겨진 UWP 창
        if dwmapi.DwmGetWindowAttribute(h, DWMWA_CLOAKED, ctypes.byref(cloaked), 4) == 0 and cloaked.value:
            return True
        if user32.GetWindowLongPtrW(h, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE):
            return True  # 오버레이/툴팁류
        l, t, r, b = _visible_rect(h)
        if (r - l) * (b - t) < min_area or _class_name(h) in _SHELL:
            return True
        pid = wintypes.DWORD(0)
        user32.GetWindowThreadProcessId(h, ctypes.byref(pid))
        if pid.value == _MY_PID:
            return True  # 이 프로그램의 설정 창은 제외
        rects.append((l - al, t - at, r - al, b - at))
        return True

    proc = WNDENUMPROC(cb)  # 호출이 끝날 때까지 참조 유지
    user32.EnumWindows(proc, 0)
    return rects


def desktop_is_covered(threshold: float = 0.95) -> bool:
    """Uncertain: 투명도가 있는 큰 창 등에서의 오탐/미탐은 미검증."""
    try:
        if _foreground_covers():
            return True
        if not (0.0 < threshold <= 1.0):
            return False
        area = _work_area()
        return covered_ratio(_collect_rects(area), area[2] - area[0], area[3] - area[1]) >= threshold
    except Exception:  # noqa: BLE001
        return False


# ======================= 3. 시스템 상태 =======================
# QUERY_USER_NOTIFICATION_STATE: 1=화면보호기/잠금, 2=전체화면 앱, 3=D3D 전체화면, 4=프레젠테이션
BLOCKED_STATES = {1, 2, 3, 4}


class _SystemPowerStatus(ctypes.Structure):
    _fields_ = [("ACLineStatus", ctypes.c_ubyte), ("BatteryFlag", ctypes.c_ubyte),
                ("BatteryLifePercent", ctypes.c_ubyte), ("SystemStatusFlag", ctypes.c_ubyte),
                ("BatteryLifeTime", wintypes.DWORD), ("BatteryFullLifeTime", wintypes.DWORD)]


def notification_state() -> int:
    """실패하면 0. Uncertain: 전체화면 앱이 보조 모니터에 있어도 BUSY가 될 수 있음."""
    try:
        s = ctypes.c_int(0)
        return s.value if shell32.SHQueryUserNotificationState(ctypes.byref(s)) == 0 else 0
    except Exception:  # noqa: BLE001
        return 0


def battery_saver_on() -> bool:
    try:
        st = _SystemPowerStatus()
        return bool(kernel32.GetSystemPowerStatus(ctypes.byref(st)) and st.SystemStatusFlag & 1)
    except Exception:  # noqa: BLE001
        return False


_DISPLAY_GUID_BYTES = uuid.UUID("6fe69556-704a-47a0-8f24-c28d936fda47").bytes_le  # CONSOLE_DISPLAY_STATE


class _GUID(ctypes.Structure):
    _fields_ = [("Data1", wintypes.DWORD), ("Data2", wintypes.WORD),
                ("Data3", wintypes.WORD), ("Data4", ctypes.c_ubyte * 8)]


# WinEvent 훅: 다른 프로세스의 창 변화를 알림으로 받는다 (주기적으로 EnumWindows를 돌리지 않기 위함).
WINEVENTPROC = ctypes.WINFUNCTYPE(None, wintypes.HANDLE, wintypes.DWORD, wintypes.HWND,
                                  wintypes.LONG, wintypes.LONG, wintypes.DWORD, wintypes.DWORD)
user32.SetWinEventHook.restype = wintypes.HANDLE
user32.SetWinEventHook.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE, WINEVENTPROC,
                                   wintypes.DWORD, wintypes.DWORD, wintypes.DWORD]
user32.UnhookWinEvent.restype = wintypes.BOOL
user32.UnhookWinEvent.argtypes = [wintypes.HANDLE]
wtsapi32.WTSRegisterSessionNotification.restype = wintypes.BOOL
wtsapi32.WTSRegisterSessionNotification.argtypes = [_HWND, wintypes.DWORD]
wtsapi32.WTSUnRegisterSessionNotification.restype = wintypes.BOOL
wtsapi32.WTSUnRegisterSessionNotification.argtypes = [_HWND]

WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS = 0x0, 0x2   # 우리 프로세스(설정 창)의 이벤트는 받지 않음
OBJID_WINDOW, CHILDID_SELF = 0, 0
# 필요한 이벤트 범위만 따로 구독한다 (범위를 넓게 잡으면 메뉴/캡처 같은 불필요한 이벤트까지 온다).
_WIN_EVENT_RANGES = (
    (0x0003, 0x0003),   # EVENT_SYSTEM_FOREGROUND     포그라운드 창이 바뀜 (전체화면 앱 실행 포함)
    (0x000B, 0x000B),   # EVENT_SYSTEM_MOVESIZEEND    창 이동/크기 조절이 끝남
    (0x0016, 0x0017),   # EVENT_SYSTEM_MINIMIZESTART/END
    (0x8002, 0x8003),   # EVENT_OBJECT_SHOW / HIDE    창이 보이거나 숨겨짐 (화면보호기 포함)
    (0x800B, 0x800B),   # EVENT_OBJECT_LOCATIONCHANGE 최대화/복원/스냅 등 위치·크기 변화
    (0x8017, 0x8018),   # EVENT_OBJECT_CLOAKED/UNCLOAKED 가상 데스크톱 전환 등
)
WM_SETTINGCHANGE, WM_DISPLAYCHANGE, WM_WTSSESSION_CHANGE = 0x001A, 0x007E, 0x02B1
PBT_APMSUSPEND, PBT_APMRESUMESUSPEND, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC = 0x4, 0x7, 0xA, 0x12


class SystemWatcher:
    """바탕화면이 보이는지에 영향을 주는 변화를 알림으로 받는다 (예전의 1~2초 주기 검사를 대신함).

      on_display(is_on: bool)   모니터 켜짐/꺼짐
      on_change()               그 밖의 변화: 포그라운드/최소화/최대화/창 표시·숨김/가상 데스크톱 전환,
                                화면 잠금·해제, 전원 상태·절전 복귀, 해상도·작업 영역 변경, 탐색기 재시작

    두 콜백 모두 메시지를 처리하는 스레드(= tk 메인 스레드)에서 호출된다. on_change는 짧은 시간에 여러 번
    올 수 있으므로 받는 쪽에서 합쳐서 처리해야 한다. 숨은 최상위 창 하나를 쓰며(브로드캐스트 메시지는
    메시지 전용 창에는 오지 않는다), 배경화면이 실행 중일 때만 만들고 close()로 정리한다.
    Uncertain: 배터리 절약 모드 켜짐/꺼짐이 PBT_APMPOWERSTATUSCHANGE로 오는지는 Windows 실기기에서 미검증."""

    def __init__(self, on_display, on_change):
        self._on_display, self._on_change = on_display, on_change
        self._power = None
        self._hooks = []
        self._wts = False
        self._proc = WINEVENTPROC(self._on_win_event)   # GC 되지 않도록 보관
        self._hwnd = _create(0, WS_POPUP, None, (0, 0))   # 한 번도 show하지 않으므로 보이지 않는다
        if not self._hwnd:
            return
        try:
            _hooks[self._hwnd] = self._on_message
            guid = _GUID.from_buffer_copy(_DISPLAY_GUID_BYTES)
            self._power = user32.RegisterPowerSettingNotification(self._hwnd, ctypes.byref(guid), 0)
            for lo, hi in _WIN_EVENT_RANGES:
                h = user32.SetWinEventHook(lo, hi, None, self._proc, 0, 0,
                                           WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
                if h:
                    self._hooks.append(h)
            self._wts = bool(wtsapi32.WTSRegisterSessionNotification(self._hwnd, 0))  # 0 = 이 세션만
        except Exception:  # noqa: BLE001  생성 도중 실패하면 만든 창/hook을 남기지 않는다
            self.close()
            raise

    @property
    def ok(self) -> bool:
        """모든 알림을 받을 수 있는가. False면 쓰는 쪽이 비상 폴링으로 대신해야 한다."""
        return bool(self._hwnd and self._power and len(self._hooks) == len(_WIN_EVENT_RANGES))

    def _on_win_event(self, _hook, _event, hwnd, id_object, id_child, _tid, _time):
        # 아주 자주 불리므로 가장 싼 검사부터: 창 자체의 이벤트만 (커서/캐럿/하위 개체 제외)
        if hwnd and id_object == OBJID_WINDOW and id_child == CHILDID_SELF:
            try:
                self._on_change()
            except Exception:  # noqa: BLE001  콜백 예외가 Windows 쪽으로 새면 안 됨
                pass

    def _on_message(self, msg, wparam, lparam):
        if msg == WM_POWERBROADCAST:
            if wparam == PBT_POWERSETTINGCHANGE:
                # POWERBROADCAST_SETTING: GUID(16) + DataLength(4) + Data. 0=꺼짐 1=켜짐 2=어두워짐
                if ctypes.string_at(lparam, 16) == _DISPLAY_GUID_BYTES:
                    self._on_display(ctypes.string_at(lparam + 20, 1)[0] != 0)
                return 1
            if wparam in (PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMESUSPEND, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND):
                self._on_change()
                return 1
            return None
        if msg in (WM_WTSSESSION_CHANGE, WM_DISPLAYCHANGE, WM_SETTINGCHANGE) \
                or (_WM_TASKBAR_CREATED and msg == _WM_TASKBAR_CREATED):
            self._on_change()
        return None

    def close(self):
        for h in self._hooks:
            user32.UnhookWinEvent(h)
        self._hooks = []
        if self._power:
            user32.UnregisterPowerSettingNotification(self._power)
            self._power = None
        if self._hwnd:
            if self._wts:
                wtsapi32.WTSUnRegisterSessionNotification(self._hwnd)
                self._wts = False
            _hooks.pop(self._hwnd, None)
            user32.DestroyWindow(self._hwnd)
            self._hwnd = 0


# ======================= 4. 트레이 아이콘 (Shell_NotifyIcon) =======================
class _NOTIFYICONDATA(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("hWnd", _HWND), ("uID", _UINT), ("uFlags", _UINT),
                ("uCallbackMessage", _UINT), ("hIcon", ctypes.c_void_p), ("szTip", wintypes.WCHAR * 128),
                ("dwState", wintypes.DWORD), ("dwStateMask", wintypes.DWORD),
                ("szInfo", wintypes.WCHAR * 256), ("uTimeout", _UINT),
                ("szInfoTitle", wintypes.WCHAR * 64), ("dwInfoFlags", wintypes.DWORD),
                ("guidItem", _GUID), ("hBalloonIcon", ctypes.c_void_p)]


shell32.Shell_NotifyIconW.argtypes = [wintypes.DWORD, ctypes.POINTER(_NOTIFYICONDATA)]
NIM_ADD, NIM_MODIFY, NIM_DELETE = 0, 1, 2
NIF_MESSAGE, NIF_ICON, NIF_TIP, NIF_INFO = 0x1, 0x2, 0x4, 0x10
WM_TRAY, WM_LBUTTONUP, WM_RBUTTONUP, WM_NULL = 0x8001, 0x0202, 0x0205, 0
MF_STRING, MF_GRAYED, MF_CHECKED, MF_POPUP, MF_SEPARATOR = 0x0, 0x1, 0x8, 0x10, 0x800
TPM_RIGHTBUTTON, TPM_RETURNCMD, TPM_NONOTIFY = 0x2, 0x100, 0x80
IDI_APPLICATION = 32512


def load_icon_file(path: str):
    """.ico 파일에서 트레이용 작은 아이콘(HICON)을 읽는다. 실패하면 0 (호출하는 쪽이 기본 아이콘으로 대체)."""
    if not path:
        return 0
    try:
        size = user32.GetSystemMetrics(49) or 16   # SM_CXSMICON (DPI 배율 반영)
        # IMAGE_ICON=1, LR_LOADFROMFILE=0x10
        return user32.LoadImageW(None, path, 1, size, size, 0x10) or 0
    except Exception:  # noqa: BLE001
        return 0
user32.RegisterWindowMessageW.restype, user32.RegisterWindowMessageW.argtypes = _UINT, [wintypes.LPCWSTR]
_WM_TASKBAR_CREATED = user32.RegisterWindowMessageW("TaskbarCreated")  # Explorer가 (재)시작되면 브로드캐스트됨


class Tray:
    """트레이 아이콘 + 오른쪽 클릭 메뉴. 콜백은 모두 메시지를 처리하는 스레드(= tk 메인 스레드)에서 호출됨.

    menu_provider() -> 메뉴를 열 때마다 호출되는 항목 목록 (체크 표시를 최신으로 유지):
        None                              구분선
        (텍스트, 콜백, 체크여부, 활성여부)    일반 항목 (뒤의 둘은 생략 가능)
        (텍스트, [하위 항목...])             하위 메뉴
    on_click(): 왼쪽 클릭. icon_path의 .ico를 쓰고, 없거나 읽지 못하면 시스템 기본 아이콘."""

    def __init__(self, tooltip, menu_provider, on_click, icon_path=""):
        self._provider, self._on_click = menu_provider, on_click
        self._hwnd = _create(WS_EX_TOOLWINDOW, WS_POPUP, None, (0, 0))  # 메뉴가 닫히려면 일반(숨은) 최상위 창이어야 함
        self.ok = False
        if not self._hwnd:
            return
        _hooks[self._hwnd] = self._on_message
        n = self._nid = _NOTIFYICONDATA()
        n.cbSize, n.hWnd, n.uID = ctypes.sizeof(n), self._hwnd, 1
        n.uFlags, n.uCallbackMessage = NIF_MESSAGE | NIF_ICON | NIF_TIP, WM_TRAY
        n.hIcon = load_icon_file(icon_path) or user32.LoadIconW(None, IDI_APPLICATION)
        n.szTip = tooltip[:127]
        self.ok = bool(shell32.Shell_NotifyIconW(NIM_ADD, ctypes.byref(n)))

    @property
    def alive(self) -> bool:
        """트레이용 창이 있는가. 아이콘이 지금 보이는지는 ok. (시작 직후 작업 표시줄이 아직 없으면
        ok=False로 시작했다가 TaskbarCreated를 받으면 ok=True가 된다.)"""
        return bool(self._hwnd)

    def _modify(self, flags):
        if self.ok:
            self._nid.uFlags = flags
            shell32.Shell_NotifyIconW(NIM_MODIFY, ctypes.byref(self._nid))

    def set_tooltip(self, text):
        self._nid.szTip = text[:127]
        self._modify(NIF_TIP)

    def notify(self, message, title="LiveWallpaper"):
        self._nid.szInfo, self._nid.szInfoTitle, self._nid.dwInfoFlags = message[:255], title[:63], 1
        self._modify(NIF_INFO)

    def _readd(self):
        """Explorer가 다시 시작돼 작업 표시줄이 새로 만들어졌을 때 같은 창/데이터로 아이콘만 다시 등록."""
        if not self._hwnd:
            return
        self._nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP
        if shell32.Shell_NotifyIconW(NIM_ADD, ctypes.byref(self._nid)):
            self.ok = True

    def _on_message(self, msg, wparam, lparam):
        if _WM_TASKBAR_CREATED and msg == _WM_TASKBAR_CREATED:
            self._readd()
            return 0
        if msg != WM_TRAY:
            return None
        if lparam == WM_LBUTTONUP:
            self._on_click()
        elif lparam == WM_RBUTTONUP:
            self._popup()
        return 0

    def _build(self, items, commands):
        menu = user32.CreatePopupMenu()
        if not menu:
            return 0
        try:
            for it in items:
                if it is None:
                    user32.AppendMenuW(menu, MF_SEPARATOR, 0, None)
                elif isinstance(it[1], list):
                    sub = self._build(it[1], commands)
                    if not user32.AppendMenuW(menu, MF_POPUP, sub or 0, it[0]) and sub:
                        user32.DestroyMenu(sub)   # 부모에 붙지 못한 하위 메뉴는 직접 해제
                else:
                    text, callback, *rest = it
                    checked = rest[0] if rest else False
                    enabled = rest[1] if len(rest) > 1 else True
                    commands[len(commands) + 1] = callback
                    flags = MF_STRING | (MF_CHECKED if checked else 0) | (0 if enabled else MF_GRAYED)
                    user32.AppendMenuW(menu, flags, len(commands), text)
            return menu
        except BaseException:
            user32.DestroyMenu(menu)   # 만드는 도중 실패하면 지금까지 붙인 하위 메뉴까지 함께 해제됨
            raise

    def _popup(self):
        commands = {}
        menu = self._build(self._provider(), commands)
        if not menu:
            return
        try:
            pt = wintypes.POINT()
            user32.GetCursorPos(ctypes.byref(pt))
            user32.SetForegroundWindow(self._hwnd)  # 없으면 메뉴 밖을 클릭해도 안 닫힘
            cmd = user32.TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                                        pt.x, pt.y, 0, self._hwnd, None)
            user32.PostMessageW(self._hwnd, WM_NULL, 0, 0)
        finally:
            user32.DestroyMenu(menu)  # 하위 메뉴도 함께 해제됨. 중간에 예외가 나도 반드시 실행
        if cmd in commands:
            commands[cmd]()

    def close(self):
        if self.ok:
            shell32.Shell_NotifyIconW(NIM_DELETE, ctypes.byref(self._nid))
            self.ok = False
        if self._hwnd:
            _hooks.pop(self._hwnd, None)
            user32.DestroyWindow(self._hwnd)
            self._hwnd = 0


# ======================= 5. 시작 프로그램 (HKCU\...\Run, 관리자 권한 불필요) =======================
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"
RUN_NAME = "LiveWallpaper"


def autostart_command(executable: str, script: str, frozen: bool, exists=os.path.isfile) -> str:
    """트레이로 시작하는 명령줄. 개발 모드에서는 콘솔 창이 안 뜨도록 pythonw.exe를 우선 사용."""
    if frozen:
        return f'"{executable}" --tray'
    folder, name = ntpath.split(executable)
    if name.lower() == "python.exe" and exists(ntpath.join(folder, "pythonw.exe")):
        executable = ntpath.join(folder, "pythonw.exe")
    return f'"{executable}" "{script}" --tray'


def autostart_supported() -> bool:
    return sys.platform == "win32"


def autostart_enabled() -> bool:
    if not autostart_supported():
        return False
    import winreg
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as k:
            winreg.QueryValueEx(k, RUN_NAME)
            return True
    except OSError:
        return False


def set_autostart(enabled: bool) -> bool:
    if not autostart_supported():
        return False
    import winreg
    script = os.path.abspath(sys.argv[0]) if sys.argv and sys.argv[0] else ""
    cmd = autostart_command(sys.executable, script, getattr(sys, "frozen", False))
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as k:
            if enabled:
                winreg.SetValueEx(k, RUN_NAME, 0, winreg.REG_SZ, cmd)
            else:
                try:
                    winreg.DeleteValue(k, RUN_NAME)
                except FileNotFoundError:
                    pass
        return True
    except OSError:
        return False
