"""libmpv 재생기 (소리 없음, 무한 반복). UI 프레임워크와 무관하게 창 핸들(wid) 하나만 받는다.

mpv가 그 HWND에 직접 렌더링한다. 하드웨어 디코딩(d3d11va)은 프레임이 GPU 텍스처로 유지되어
Python으로 픽셀이 넘어오지 않으므로 CPU·RAM 사용이 가장 낮다.

  suspend(): mpv 인스턴스를 완전히 종료 (디코더, GPU 컨텍스트, 파일 핸들 모두 해제) → 설정 창 미리보기용
  pause()  : 일시정지만 (디코더 유지) → 배경화면이 가려졌을 때 사용. 재개 시 같은 자리에서 이어짐
  resume() : 일시정지 해제, 또는 종료된 상태면 다시 시작
  resume(on_ready) : 종료된 상태에서 다시 시작할 때 첫 화면이 준비되면 on_ready()를 부른다.
             항상 영상 처음부터 재생한다 (정적 배경 = 첫 프레임 → 라이브 전환이 이어져 보이도록)

Uncertain: 바탕화면(Progman) 자식 + layered 창 안에서 mpv의 자식 창이 실제로 보이는지는 Windows에서 확인되지 않음.
"""
import locale
import os
import sys
import threading
import time

FIT_MODES = {"contain": "Fit", "cover": "Fill", "stretch": "Stretch"}

_mpv = None          # 지연 로드된 mpv 모듈
_load_error = ""


def _add_dll_dirs():
    """프로젝트 폴더(또는 exe 옆)의 libs/, mpv/ 에 둔 libmpv DLL을 찾을 수 있게 한다."""
    if sys.platform != "win32":
        return
    if getattr(sys, "frozen", False):
        base = os.path.dirname(os.path.abspath(sys.executable))
    else:  # 개발 모드: src/ 의 한 단계 위 = 프로젝트 루트
        base = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for d in (base, os.path.join(base, "libs"), os.path.join(base, "mpv")):
        if os.path.isdir(d):
            try:
                os.add_dll_directory(d)
            except (OSError, AttributeError):
                pass
            os.environ["PATH"] = d + os.pathsep + os.environ.get("PATH", "")


def load_mpv():
    """(mpv 모듈 또는 None, 오류 메시지)."""
    global _mpv, _load_error
    if _mpv is not None:
        return _mpv, ""
    if _load_error:
        return None, _load_error
    _add_dll_dirs()
    try:
        locale.setlocale(locale.LC_NUMERIC, "C")  # libmpv 요구사항
    except locale.Error:
        pass
    try:
        import mpv
        _mpv = mpv
        return mpv, ""
    except (ImportError, OSError) as e:
        _load_error = ("Unable to load libmpv. Run `pip install python-mpv`, then "
                       "put libmpv-2.dll (or mpv-2.dll) in the project's libs folder.\n"
                       f"({e})")
        return None, _load_error


def core_options(hwdec: str) -> dict:
    """낮은 CPU/메모리를 위한 mpv 옵션. 이름은 libmpv 0.37에서 검증됨."""
    return dict(
        input_default_bindings=False, input_vo_keyboard=False, osc=False, osd_level=0,
        config=False, load_scripts=False, ytdl=False,   # 사용자 설정/스크립트/yt-dlp 로드 안 함
        idle=True, aid="no", loop_file="inf",           # 오디오 없음, 무한 반복
        cache="no", demuxer_max_bytes="4MiB", demuxer_max_back_bytes="0",
        demuxer_readahead_secs=1,                       # 읽기 버퍼 최소화
        hwdec=hwdec, profile="fast",                    # GPU 디코딩 + 가벼운 스케일러
    )


# libmpv 이벤트 번호 (client.h의 mpv_event_id). 앱이 쓰지 않는 이벤트는 큐에 쌓이지 않게 끈다.
# 쓰는 클라이언트가 없는 큐에 계속 쌓이면 가득 차서 "Too many events queued"가 나온다.
_EV_START_FILE, _EV_END_FILE, _EV_FILE_LOADED = 6, 7, 8
_EV_IDLE, _EV_PAUSE, _EV_UNPAUSE, _EV_TICK = 11, 12, 13, 14            # 구버전 호환용(deprecated) 이벤트
_EV_VIDEO_RECONFIG, _EV_AUDIO_RECONFIG, _EV_METADATA_UPDATE = 17, 18, 19
_EV_SEEK, _EV_PLAYBACK_RESTART, _EV_CHAPTER_CHANGE = 20, 21, 23
# 이벤트를 아무도 꺼내 가지 않는 기본 핸들("main"): 선택 이벤트를 모두 끈다.
_OFF_MAIN = (_EV_START_FILE, _EV_END_FILE, _EV_FILE_LOADED, _EV_IDLE, _EV_PAUSE, _EV_UNPAUSE, _EV_TICK,
             _EV_VIDEO_RECONFIG, _EV_AUDIO_RECONFIG, _EV_METADATA_UPDATE, _EV_SEEK, _EV_PLAYBACK_RESTART,
             _EV_CHAPTER_CHANGE)
# python-mpv의 이벤트 스레드가 읽는 핸들: playback-restart(on_ready)와 파일 시작/끝은 남기고, 반복 재생이나
# 일시정지/재개 때마다 생기는 이벤트만 끈다.
_OFF_EVENT = (_EV_IDLE, _EV_PAUSE, _EV_UNPAUSE, _EV_TICK, _EV_VIDEO_RECONFIG, _EV_AUDIO_RECONFIG,
              _EV_METADATA_UPDATE, _EV_SEEK, _EV_CHAPTER_CHANGE)


def trim_events(mpv, p) -> tuple:
    """쓰지 않는 mpv 이벤트를 끈다. 반환: (끈 수, 실패 수). 실패해도 재생에는 영향이 없다.
    Uncertain: python-mpv의 내부 이름(_mpv_request_event, _event_handle)에 의존한다. 이름이 없거나
    호출이 실패하면 아무것도 끄지 않고 (0, 실패 수)를 돌려준다 (이벤트 제한이 없던 예전 동작과 같다)."""
    request = getattr(mpv, "_mpv_request_event", None)
    done = failed = 0
    if request is None:
        return 0, 1
    for handle, events in ((getattr(p, "handle", None), _OFF_MAIN),
                           (getattr(p, "_event_handle", None), _OFF_EVENT)):
        if handle is None:
            failed += 1
            continue
        for ev in events:
            try:
                request(handle, ev, 0)   # mpv_request_event(handle, id, enable=0)
                done += 1
            except Exception:  # noqa: BLE001  번호를 모르는 구버전/ctypes 형 불일치 등
                failed += 1
    return done, failed


def _apply_fit(player, mode: str):
    player.keepaspect = mode != "stretch"
    player.panscan = 1.0 if mode == "cover" else 0.0


class Player:
    def __init__(self, wid: int, fit_mode: str = "contain", hwdec: str = "auto-safe", on_error=None):
        """on_error(text)는 mpv 스레드에서 호출될 수 있으므로, UI를 건드린다면 호출하는 쪽에서
        메인 스레드로 넘겨야 한다 (root.after(0, ...))."""
        self._wid = wid
        self._fit = fit_mode if fit_mode in FIT_MODES else "contain"
        self._hwdec = hwdec or "auto-safe"
        self._on_error = on_error
        self._path = ""
        self._player = None
        self._paused = False
        self._reported = False
        self._on_ready = None       # resume(on_ready)로 받은 콜백 (mpv 스레드에서 호출됨)
        self.trimmed = (0, 0)       # trim_events 결과 (끈 이벤트 수, 실패 수)

    # ---- 내부 ----
    def _ensure_player(self) -> bool:
        if self._player:
            return True
        mpv, err = load_mpv()
        if not mpv:
            self._report(err)
            return False
        try:
            self._player = mpv.MPV(wid=self._wid, log_handler=self._on_log, loglevel="error",
                                   **core_options(self._hwdec))
        except Exception as e:  # noqa: BLE001  일부 옵션이 이 빌드에 없으면 최소 옵션으로 재시도
            try:
                self._player = mpv.MPV(wid=self._wid, log_handler=self._on_log, loglevel="error",
                                       idle=True, aid="no", loop_file="inf", hwdec=self._hwdec)
            except Exception as e2:  # noqa: BLE001
                self._report(f"mpv initialization failed: {e2} (previous attempt: {e})")
                return False
        _apply_fit(self._player, self._fit)
        self._register_events(self._player)
        self.trimmed = trim_events(mpv, self._player)   # (끈 이벤트 수, 실패 수) — 진단용
        return True

    def _register_events(self, p):
        """첫 화면이 준비되면(playback-restart) on_ready를 한 번만 부른다.
        Uncertain: playback-restart가 첫 화면 표시 직후에 오는지는 Windows 실기기에서 미검증 (안 와도 wallpaper.py의 시간 제한이 처리)."""
        @p.event_callback("playback-restart")
        def _restarted(_event):
            if p is not self._player:   # 이미 종료 중인 이전 mpv의 이벤트는 무시
                return
            cb, self._on_ready = self._on_ready, None
            if cb:
                try:
                    cb()
                except Exception:  # noqa: BLE001  콜백 오류가 mpv 이벤트 스레드를 죽이지 않게
                    pass

    def _on_log(self, level, prefix, text):
        if level in ("error", "fatal"):
            self._report(f"[{prefix}] {text.strip()}")

    def _report(self, text):
        if not self._reported and self._on_error:  # 재생 한 번당 오류 알림은 한 번만
            self._reported = True
            self._on_error(text)

    def _start(self):
        self._reported = False
        if not self._path or not self._ensure_player():
            return
        self._paused = False
        self._player.pause = False
        self._player.play(self._path)

    # ---- 공개 API ----
    def load(self, path: str):
        """재생은 시작하지 않고 경로만 기억 (창이 숨겨진 미리보기용)."""
        self._path = path

    def play(self, path: str):
        self._path = path
        self._start()

    def pause(self):
        if self._player and not self._paused:
            self._player.pause = True
            self._paused = True

    def suspend(self):
        """mpv를 완전히 종료해 디코더/GPU 리소스/파일을 해제 (경로는 기억).

        terminate()를 메인(UI) 스레드에서 그대로 부르면 앱 전체가 멈출 수 있다: mpv가 자기 창을 없앨 때
        부모 창(UI 스레드 소유)에 동기 메시지를 보내는데, UI 스레드가 terminate()에서 기다리느라
        그 메시지를 못 받으면 서로 영원히 기다린다. 그래서 별도 스레드에서 종료하고, 기다리는 동안
        UI 스레드는 들어오는 동기 메시지를 계속 처리한다 (최대 3초, 넘으면 포기하고 계속 진행)."""
        p, self._player = self._player, None
        self._paused = False
        self._on_ready = None
        if not p:
            return

        def _kill():
            try:
                p.terminate()
            except Exception:  # noqa: BLE001
                pass

        t = threading.Thread(target=_kill, daemon=True)
        t.start()
        deadline = time.monotonic() + 3.0
        while t.is_alive() and time.monotonic() < deadline:
            if sys.platform == "win32":
                try:
                    import winapi
                    winapi.pump_sent_messages()
                except Exception:  # noqa: BLE001
                    pass
            t.join(0.02)

    def resume(self, on_ready=None):
        """종료된 상태면 처음부터 다시 시작하고, 첫 화면이 준비되면 on_ready()를 mpv 스레드에서 한 번 호출한다.
        이미 재생 중/일시정지 상태라면 기다릴 것이 없으므로 바로 호출한다."""
        if not self._player:
            self._on_ready = on_ready
            self._start()
            return
        if self._paused:
            self._player.pause = False
            self._paused = False
        if on_ready:
            on_ready()

    def stop(self):
        self.suspend()
        self._path = ""

    def set_fit_mode(self, mode: str):
        if mode in FIT_MODES:
            self._fit = mode
            if self._player:
                _apply_fit(self._player, mode)
