"""배경화면 수명 관리: 바탕화면 부착 + '보이지 않으면 일시정지, 보이면 같은 자리에서 이어서 재생' 정책.

가려져도 mpv를 끄지 않고 pause만 한다 → 디코더/GPU 컨텍스트가 유지되어 다시 보일 때
재시작 없이 멈춘 프레임에서 바로 이어진다. 검사 주기는 재생 1초 / 정지 2초.

정적 전환(설정 static_snapshot, 기본 꺼짐): 가려진 상태가 snapshot_delay_s초 이상 이어지면
  1) Windows 바탕화면을 영상 첫 프레임 PNG로 바꾸고  2) 호스트 창을 숨기고  3) mpv를 완전히 종료한다.
다시 보이면 mpv를 새로 만들어 영상 처음부터 재생 → 첫 화면이 준비되면 호스트 창을 다시 보이고 → 원래 바탕화면을 복원한다.
(정적 배경이 영상의 첫 프레임이므로, 처음부터 재생해야 전환이 자연스럽다.)
호스트 창은 파괴하지 않고 숨기기만 한다. 원래 바탕화면 정보는 snapshot.py가 파일에 저장해 비정상 종료에도 복구한다.

UI(main.py)와 분리해 두어, 설정 창이 숨겨져(트레이) 있어도 같은 로직이 동작한다.
모든 메서드는 tk 메인 스레드에서 호출한다.
"""
import time

import media
import winapi
from config import wallpaper_state_path
from player import Player, load_mpv
from snapshot import WallpaperSwapper

POLL_VISIBLE_MS = 1000   # 재생 중
POLL_HIDDEN_MS = 2000    # 멈춘 동안은 덜 자주 확인
RESUME_TIMEOUT_MS = 3000  # 첫 화면 준비 신호가 없어도 이 시간 뒤에는 정적 → 라이브 전환을 마친다


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
        self._reason = ""
        self._hidden_since = None      # 가려지기 시작한 시각 (time.monotonic)
        self._static = False           # True: mpv 종료 + Windows 바탕화면이 첫 프레임
        self._resuming = False         # True: 정적 → 라이브 전환 중 (첫 화면 준비 대기)
        self._resume_job = None
        self._snapshot_off = ""        # 비어 있지 않으면 이번 실행 동안 정적 전환 안 함 (이유)
        self._swapper = WallpaperSwapper(wallpaper_state_path(), winapi.get_desktop_wallpaper,
                                         winapi.set_desktop_wallpaper)

    def recover_previous_session(self):
        """이전 실행이 정적 상태로 비정상 종료됐다면 원래 바탕화면을 복원한다 (단일 실행 확인 뒤, 시작 직후 한 번)."""
        if not self._swapper.restore():
            self._on_status("Unable to restore the original Windows wallpaper (will retry next launch)")

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
        self._snapshot_off = ""
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
        self._cancel_resume_job()
        if self._player:
            self._player.stop()   # mpv를 먼저 끝낸 뒤 창을 없앤다
            self._player = None
        if not self._swapper.restore():   # 정적 상태였다면 원래 바탕화면 복원 (다음 실행 때 다시 시도하도록 상태 파일은 남음)
            self._on_status("Unable to restore the original Windows wallpaper (will retry next launch)")
        if self._hwnd:
            winapi.destroy_window(self._hwnd)
            self._hwnd = 0
        self._hidden = False
        self._static = False
        self._resuming = False
        self._hidden_since = None

    # ---- 보이는지 판단 ----
    def _tick(self):
        self._job = None
        if self._player and not winapi.host_alive(self._hwnd):
            path = self._path
            self.stop()
            self._on_lost(path)
            return
        self.evaluate()
        self._maybe_enter_static()
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
        if not self._player or self._resuming:   # 전환 중에는 끝난 뒤 다음 검사에서 다시 판단
            return
        try:
            reason = self.hidden_reason()
        except Exception:  # noqa: BLE001
            reason = ""
        hidden = bool(reason)
        if self._static:
            # 다시 보이거나, 옵션을 껐다면 라이브로 돌아간다 (돌아간 뒤 여전히 가려져 있으면 일반 일시정지로 처리)
            if not hidden or not self._s.static_snapshot:
                self._begin_resume()
            return
        if hidden == self._hidden:
            return
        self._hidden = hidden
        self._reason = reason
        if hidden:
            self._hidden_since = time.monotonic()
            self._player.pause()        # mpv는 살려 둠 → 재개 시 끊김 없이 이어짐
            self._interval = POLL_HIDDEN_MS
            self._on_status(f"Paused ({reason})")
        else:
            self._hidden_since = None
            self._player.resume()
            self._interval = POLL_VISIBLE_MS
            self._on_status("Playing")

    # ---- 정적 전환: PLAYING(일시정지) → STATIC ----
    def _maybe_enter_static(self):
        if (not self._player or not self._hidden or self._static or self._resuming
                or not self._s.static_snapshot or self._snapshot_off or self._hidden_since is None):
            return
        if time.monotonic() - self._hidden_since < self._s.snapshot_delay_s:
            return
        self._enter_static()

    def _enter_static(self):
        if winapi.monitor_count() != 1:
            self._snapshot_off = "Static wallpaper supports a single monitor only"
            return
        w, h = self._root.winfo_screenwidth(), self._root.winfo_screenheight()   # DPI aware → 물리 픽셀
        frame = media.ensure_first_frame(self._path, w, h, self._s.fit_mode)
        if not frame:
            self._snapshot_off = "Unable to extract the first frame (is ffmpeg available?)"
            self._on_status(f"Paused ({self._reason}) · {self._snapshot_off}")
            return
        ok, why = self._swapper.enter(frame)   # 1) Windows 바탕화면 교체 (원래 것은 파일에 저장됨)
        if not ok:
            self._snapshot_off = why
            self._on_status(f"Paused ({self._reason}) · static wallpaper unavailable: {why}")
            return
        winapi.hide_window(self._hwnd)         # 2) 호스트 창 숨김 (파괴하지 않음)
        self._player.suspend()                 # 3) mpv 완전 종료
        self._static = True
        self._interval = POLL_HIDDEN_MS
        self._on_status(f"Static first frame ({self._reason})")

    # ---- 정적 → 라이브 복귀: STATIC → RESTORED ----
    def _begin_resume(self):
        self._resuming = True
        self._hidden = False
        self._hidden_since = None
        self._on_status("Resuming...")
        # 호스트 창은 숨긴 채로 mpv를 새로 만들고(처음부터 재생), 첫 화면이 준비되면 _finish_resume으로 넘어온다
        self._player.resume(on_ready=self._ready_from_mpv_thread)
        # 준비 신호가 오지 않아도 정적 상태에 갇히지 않도록 시간 제한을 둔다
        self._resume_job = self._root.after(RESUME_TIMEOUT_MS, self._finish_resume)

    def _ready_from_mpv_thread(self):
        try:
            self._root.after(0, self._finish_resume)
        except Exception:  # noqa: BLE001  (RuntimeError/TclError)
            pass

    def _cancel_resume_job(self):
        if self._resume_job:
            try:
                self._root.after_cancel(self._resume_job)
            except Exception:  # noqa: BLE001
                pass
            self._resume_job = None

    def _finish_resume(self):
        if not self._resuming or not self._player:
            return
        self._cancel_resume_job()
        winapi.show_window(self._hwnd)         # 4) 호스트 창 표시 (이제 영상이 보임)
        if not self._swapper.restore():        # 5) 그 뒤에 원래 바탕화면 복원 → 복원 순간이 화면에 보이지 않음
            self._on_status("Unable to restore the original Windows wallpaper (will retry next launch)")
        else:
            self._on_status("Playing")
        self._static = False
        self._resuming = False
        self._interval = POLL_VISIBLE_MS
