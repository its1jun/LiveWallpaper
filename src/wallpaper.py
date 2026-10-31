"""배경화면 수명 관리: 바탕화면 부착 + '보이지 않으면 일시정지, 보이면 같은 자리에서 이어서 재생' 정책.

가려져도 mpv를 끄지 않고 pause만 한다 → 디코더/GPU 컨텍스트가 유지되어 다시 보일 때
재시작 없이 멈춘 프레임에서 바로 이어진다. 검사 주기는 재생 1초 / 정지 2초.

UI(main.py)와 분리해 두어, 설정 창이 숨겨져(트레이) 있어도 같은 로직이 동작한다.
모든 메서드는 tk 메인 스레드에서 호출한다.
"""
import winapi
from player import Player, load_mpv

POLL_VISIBLE_MS = 1000   # 재생 중
POLL_HIDDEN_MS = 2000    # 멈춘 동안은 덜 자주 확인


class Wallpaper:
    def __init__(self, root, settings, on_error, on_status, on_lost):
        self._root = root
        self._s = settings
        self._on_error = on_error      # (text) — 메인 스레드에서 호출됨
        self._on_status = on_status    # (text)
        self._on_lost = on_lost        # (path) — Explorer 재시작 등으로 바탕화면 창이 사라졌을 때
        self._path = ""
        self._hwnd = 0
        self._player = None
        self._watcher = None
        self._job = None
        self._hidden = False
        self._display_on = True
        self._interval = POLL_VISIBLE_MS

    @property
    def active(self) -> bool:
        return self._player is not None

    @property
    def path(self) -> str:
        """지금 재생 중인 파일 (cache 정리 때 지우지 않도록 알려주는 용도)."""
        return self._path if self._player else ""

    def start(self, path: str):
        """반환: (성공여부, 메시지)."""
        self.stop()
        mpv, err = load_mpv()
        if not mpv:
            return False, err
        hwnd = winapi.create_host_window()
        if not hwnd:
            return False, "Unable to create the desktop host window."
        ok, msg = winapi.attach_to_desktop(hwnd)
        if not ok:
            winapi.destroy_window(hwnd)
            return False, msg
        self._hwnd = hwnd
        self._path = path
        # 부착이 끝난 뒤에 mpv를 만들어야 mpv의 자식 창이 올바른 부모 아래에 생성됨.
        # on_error는 mpv 스레드에서 오므로 메인 스레드로 넘긴다.
        self._player = Player(hwnd, self._s.fit_mode, self._s.hwdec,
                              self._error_from_mpv_thread)
        self._player.play(path)
        self._hidden = False
        self._interval = POLL_VISIBLE_MS
        self._display_on = True
        w = winapi.DisplayWatcher(self._on_display_state)
        if w.ok:
            self._watcher = w
        else:
            w.close()
        self.evaluate()  # 이미 가려져 있으면 곧바로 정지
        self._job = self._root.after(self._interval, self._tick)
        if not self._hidden:
            self._on_status("Playing")
        return True, msg

    def _error_from_mpv_thread(self, text: str):
        """mpv 스레드에서 호출됨 → 메인 스레드로 넘긴다. 종료 중이라 Tk가 이미 없으면 무시."""
        try:
            self._root.after(0, self._on_error, text)
        except Exception:  # noqa: BLE001  (RuntimeError/TclError)
            pass

    def set_fit_mode(self, mode: str):
        if self._player:
            self._player.set_fit_mode(mode)

    def stop(self):
        if self._job:
            self._root.after_cancel(self._job)
            self._job = None
        if self._watcher:
            self._watcher.close()
            self._watcher = None
        if self._player:
            self._player.stop()   # mpv를 먼저 끝낸 뒤 창을 없앤다
            self._player = None
        if self._hwnd:
            winapi.destroy_window(self._hwnd)
            self._hwnd = 0
        self._hidden = False

    # ---- 보이는지 판단 ----
    def _tick(self):
        self._job = None
        if self._player and not winapi.host_alive(self._hwnd):
            path = self._path
            self.stop()
            self._on_lost(path)
            return
        self.evaluate()
        if self._player:
            self._job = self._root.after(self._interval, self._tick)

    def _on_display_state(self, is_on: bool):
        self._display_on = is_on
        self.evaluate()

    def hidden_reason(self) -> str:
        """가려졌으면 이유, 아니면 빈 문자열. 싼 검사부터 순서대로."""
        if not self._display_on:
            return "Monitor off"
        if winapi.notification_state() in winapi.BLOCKED_STATES:
            return "Locked / screensaver / fullscreen app"
        if self._s.pause_on_battery_saver and winapi.battery_saver_on():
            return "Battery saver mode"
        if winapi.desktop_is_covered(self._s.coverage_threshold):
            return "Desktop covered by another window"
        return ""

    def evaluate(self):
        if not self._player:
            return
        try:
            reason = self.hidden_reason()
        except Exception:  # noqa: BLE001
            reason = ""
        hidden = bool(reason)
        if hidden == self._hidden:
            return
        self._hidden = hidden
        if hidden:
            self._player.pause()        # mpv는 살려 둠 → 재개 시 끊김 없이 이어짐
            self._interval = POLL_HIDDEN_MS
            self._on_status(f"Paused ({reason})")
        else:
            self._player.resume()
            self._interval = POLL_VISIBLE_MS
            self._on_status("Playing")
