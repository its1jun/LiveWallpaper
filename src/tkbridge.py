"""네이티브(ctypes) 콜백 → tk 메인 스레드 전달.

문제: WNDPROC, WinEvent 훅 같은 네이티브 콜백은 tk 메인 스레드에서, 그것도 Tk 이벤트 루프(Tcl_DoOneEvent)
안에서 불린다. 이 안에서 tkinter(root.after 등)를 직접 부르면 tkinter 내부의 스레드 상태(tcl_tstate)가
지워져서, 바로 뒤에 Tk가 다음 파이썬 콜백을 실행할 때 다음 오류로 프로세스가 강제 종료된다:
    Fatal Python error: PyEval_RestoreThread: the function must be called with the GIL held ...

해결: 네이티브 콜백은 post()만 부른다. post()는 큐에 넣기만 하고 tkinter를 건드리지 않는다.
작은 스레드가 큐에서 꺼내 root.after(0, ...)를 부른다 (다른 스레드에서 부르는 root.after는 tkinter가
Tcl 스레드로 안전하게 넘겨 준다 — mpv/ffmpeg 스레드가 이미 쓰는 방식과 같다).
"""
import queue
import threading
import time

_RETRY_DELAY_S = 0.1
_RETRY_MAX = 50   # mainloop가 아직 시작 전이면 RuntimeError → 최대 5초 재시도


class TkBridge:
    def __init__(self, root):
        self._root = root
        self._q = queue.SimpleQueue()
        self._closed = False
        self._thread = threading.Thread(target=self._run, name="LiveWallpaper-TkBridge", daemon=True)
        self._thread.start()

    def post(self, fn, *args):
        """어느 스레드/콜백에서 불러도 안전하다 (tkinter를 부르지 않는다). fn은 tk 메인 스레드에서 실행된다."""
        if not self._closed:
            self._q.put((fn, args))

    def close(self):
        self._closed = True
        self._q.put(None)

    def _run(self):
        while True:
            item = self._q.get()
            if item is None or self._closed:
                return
            fn, args = item
            for _ in range(_RETRY_MAX):
                try:
                    self._root.after(0, fn, *args)
                    break
                except RuntimeError:        # main thread is not in main loop (시작 직후)
                    if self._closed:
                        return
                    time.sleep(_RETRY_DELAY_S)
                except Exception:           # noqa: BLE001  TclError: 창이 이미 없어짐 → 버림
                    break
