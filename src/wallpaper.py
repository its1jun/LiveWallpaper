"""배경화면 수명 관리: 바탕화면 부착 + '보이지 않으면 일시정지, 보이면 같은 자리에서 이어서 재생' 정책.

가려져도 mpv를 끄지 않고 pause만 한다 → 디코더/GPU 컨텍스트가 유지되어 다시 보일 때
재시작 없이 멈춘 프레임에서 바로 이어진다.

가림 검사는 주기(폴링)가 아니라 이벤트로 한다 (winapi.SystemWatcher):
  포그라운드/최소화/최대화/창 표시·숨김/가상 데스크톱 전환, 화면 잠금·해제, 전원 상태·절전 복귀,
  해상도 변경, 탐색기 재시작, 모니터 켜짐/꺼짐 → request_check()가 짧은 시간에 몰린 이벤트를 합쳐 한 번만 검사.
  이벤트 직후에는 시스템 상태(전체화면 판정 등)가 아직 안 바뀌었을 수 있어 1회만 한 번 더 확인한다.
  이벤트 감시를 만들지 못하면(Windows 정책 등) 예전처럼 주기 검사(1초/2초)로 대신한다.

정적 전환(설정 static_snapshot, 기본 켜짐): 가려진 상태가 snapshot_delay_s초 이상 이어지면
  1) Windows 바탕화면을 영상 첫 프레임 PNG로 바꾸고  2) 호스트 창을 숨기고  3) mpv를 완전히 종료한다.
  '가려진 뒤 N초'는 가려지는 순간 거는 1회성 타이머로 처리한다 (다시 보이면 취소).
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
from tkbridge import TkBridge

EVAL_DEBOUNCE_MS = 150    # 짧은 시간에 몰려오는 창 이벤트를 한 번의 검사로 합친다
EVAL_SETTLE_MS = 1500     # 이벤트 뒤 상태가 늦게 바뀌는 경우(전체화면 판정 등)를 위한 1회성 재확인
POLL_VISIBLE_MS = 1000    # 비상 폴링(이벤트 감시를 못 만들었을 때만) — 재생 중
POLL_HIDDEN_MS = 2000     # 비상 폴링 — 멈춘 동안
RESUME_TIMEOUT_MS = 3000  # 첫 화면 준비 신호가 없어도 이 시간 뒤에는 정적 → 라이브 전환을 마친다 (안전장치)


class Wallpaper:
    def __init__(self, root, settings, on_error, on_status, on_lost, bridge=None):
        self._root = root
        self._bridge = bridge or TkBridge(root)   # 네이티브 콜백 → tk 스레드 전달 (tkbridge.py 참고)
        self._check_posted = False     # request_check가 이미 tk 스레드로 넘겨졌는가 (이벤트 폭주 합치기)
        self._s = settings
        self._on_error = on_error      # (text) — 메인 스레드에서 호출됨
        self._on_status = on_status    # (text)
        self._on_lost = on_lost        # (path) — Explorer 재시작 등으로 바탕화면 창이 사라졌을 때
        self._path = ""
        self._hwnd = 0
        self._player = None
        self._watcher = None
        self._event_driven = False     # True: 이벤트로 검사 / False: 비상 폴링
        self._job = None               # 비상 폴링 예약
        self._eval_job = None          # 이벤트 합치기(debounce) 예약
        self._settle_job = None        # 이벤트 뒤 1회 재확인 예약
        self._snapshot_job = None      # '가려진 뒤 N초' 정적 전환 타이머 (1회성)
        self._hidden = False
        self._display_on = True
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
        self._display_on = True
        self._snapshot_off = ""
        self._event_driven = False
        self._check_posted = False
        try:
            w = winapi.SystemWatcher(self._on_display_state, self.request_check)
        except Exception:  # noqa: BLE001  감시를 못 만들어도 비상 폴링으로 계속 동작
            w = None
        if w is not None and w.ok:
            self._watcher = w
            self._event_driven = True
        elif w is not None:
            w.close()
        self.evaluate()  # 이미 가려져 있으면 곧바로 정지
        if not self._event_driven:
            self._job = self._root.after(POLL_VISIBLE_MS, self._tick)
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
        self._job = self._cancel(self._job)
        self._eval_job = self._cancel(self._eval_job)
        self._settle_job = self._cancel(self._settle_job)
        self._snapshot_job = self._cancel(self._snapshot_job)
        if self._watcher:
            self._watcher.close()
            self._watcher = None
        self._event_driven = False
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

    def _cancel(self, job):
        """예약된 after 작업을 취소하고 None을 돌려준다 (self._x = self._cancel(self._x) 형태로 쓴다)."""
        if job:
            try:
                self._root.after_cancel(job)
            except Exception:  # noqa: BLE001  (TclError)
                pass
        return None

    # ---- 보이는지 판단: 이벤트가 오면 검사 ----
    def request_check(self):
        """창/전원/세션 이벤트가 올 때마다 불린다 (몰려오는 이벤트는 한 번의 검사로 합친다).
        설정(정적 전환/배터리 절약 옵션)이 바뀌었을 때도 불러서 바로 반영한다.

        WinEvent 훅/WNDPROC 같은 네이티브 콜백에서도 불리므로 tkinter를 직접 부르면 안 된다:
        큐에 넣기만 하고(TkBridge.post), 실제 예약(_schedule_check)은 tk 스레드에서 한다."""
        if self._check_posted:
            return
        self._check_posted = True
        self._bridge.post(self._schedule_check)

    def _schedule_check(self):   # tk 메인 스레드
        self._check_posted = False
        if not self._player or self._eval_job:
            return
        self._eval_job = self._root.after(EVAL_DEBOUNCE_MS, self._run_check)

    def refresh(self):
        """설정이 바뀌었을 때 UI가 부른다."""
        self.request_check()

    def _run_check(self):
        self._eval_job = None
        self._check_now()
        if self._player:   # 상태가 늦게 바뀌는 경우를 위해 한 번만 더 확인 (이것이 다시 예약을 만들지는 않는다)
            self._settle_job = self._cancel(self._settle_job)
            self._settle_job = self._root.after(EVAL_SETTLE_MS, self._run_settle)

    def _run_settle(self):
        self._settle_job = None
        self._check_now()

    def _check_now(self):
        if not self._player:
            return
        if not winapi.host_alive(self._hwnd):   # 탐색기 재시작 등으로 바탕화면 창이 사라짐
            path = self._path
            self.stop()
            self._on_lost(path)
            return
        self.evaluate()
        self._maybe_enter_static()
        self._arm_snapshot_timer()

    def _tick(self):
        """비상 폴링 (이벤트 감시를 만들지 못했을 때만 쓴다)."""
        self._job = None
        self._check_now()
        if self._player:
            self._job = self._root.after(POLL_HIDDEN_MS if (self._hidden or self._static) else POLL_VISIBLE_MS,
                                         self._tick)

    def _on_display_state(self, is_on: bool):
        """모니터 켜짐/꺼짐 (네이티브 콜백에서 불림 → tkinter를 부르는 evaluate()를 직접 부르지 않는다)."""
        self._display_on = is_on
        self.request_check()

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
            self._on_status(f"Paused ({reason})")
            self._arm_snapshot_timer()
        else:
            self._hidden_since = None
            self._snapshot_job = self._cancel(self._snapshot_job)
            self._player.resume()
            self._on_status("Playing")

    # ---- 정적 전환: PLAYING(일시정지) → STATIC ----
    def _arm_snapshot_timer(self):
        """가려진 채 snapshot_delay_s가 지나면 정적 전환하도록 1회성 타이머를 건다 (주기 검사가 아님)."""
        if (self._snapshot_job or not self._player or not self._hidden or self._static or self._resuming
                or not self._s.static_snapshot or self._snapshot_off or self._hidden_since is None):
            return
        remain = self._s.snapshot_delay_s - (time.monotonic() - self._hidden_since)
        self._snapshot_job = self._root.after(max(0, int(remain * 1000)) + 50, self._on_snapshot_timer)

    def _on_snapshot_timer(self):
        self._snapshot_job = None
        self._maybe_enter_static()
        self._arm_snapshot_timer()   # 반올림 때문에 아직 시간이 모자랐다면 남은 만큼만 다시 건다 (정적 전환했거나 불가면 걸리지 않음)

    def _maybe_enter_static(self):
        if (not self._player or not self._hidden or self._static or self._resuming
                or not self._s.static_snapshot or self._snapshot_off or self._hidden_since is None):
            return
        if time.monotonic() - self._hidden_since < self._s.snapshot_delay_s:
            return
        self._enter_static()

    def _enter_static(self):
        self._snapshot_job = self._cancel(self._snapshot_job)
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
        self._on_status(f"Static first frame ({self._reason})")

    # ---- 정적 → 라이브 복귀: STATIC → RESTORED ----
    def _begin_resume(self):
        self._resuming = True
        self._hidden = False
        self._hidden_since = None
        self._snapshot_job = self._cancel(self._snapshot_job)
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
        self._resume_job = self._cancel(self._resume_job)

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
        self.request_check()   # 전환 중에는 검사를 건너뛰었으므로, 끝나면 현재 상태를 한 번 다시 판단
