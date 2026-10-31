"""단일 인스턴스 간 명령 전달.

프로세스 단일 실행 여부는 Win32 named mutex(winapi.py)가 책임지고,
실행 중인 프로세스에 명령을 전달하는 일은 Windows Named Pipe가 책임진다.
Tk/Win32 창 핸들을 IPC 주소로 사용하지 않는다.
"""
from __future__ import annotations

import getpass
import hashlib
import os
import threading
import time
from multiprocessing import AuthenticationError
from multiprocessing.connection import Client, Listener


_USER_ID = (os.environ.get("USERDOMAIN", "") + "\\" +
            (os.environ.get("USERNAME") or getpass.getuser())).casefold()
_USER_HASH = hashlib.sha256(_USER_ID.encode("utf-8", "replace")).hexdigest()[:20]
_PIPE_ADDRESS = rf"\\.\pipe\LiveWallpaper-v2-{_USER_HASH}"
_AUTHKEY = hashlib.sha256(("LiveWallpaper IPC v2:" + _USER_ID).encode("utf-8", "replace")).digest()


class InstanceServer:
    """기존 앱으로부터 SHOW 명령을 받는 Named Pipe 서버.

    on_show는 IPC 스레드에서 호출된다. GUI 코드를 직접 실행하지 말고,
    thread-safe queue에 이벤트를 넣는 콜백을 전달해야 한다.
    """

    def __init__(self, on_show):
        self._on_show = on_show
        self._stop = threading.Event()
        # named mutex를 먼저 획득한 프로세스만 서버를 생성하므로 주소 충돌은 비정상 상태다.
        self._listener = Listener(_PIPE_ADDRESS, family="AF_PIPE", authkey=_AUTHKEY)
        self._thread = threading.Thread(
            target=self._serve, name="LiveWallpaper-IPC", daemon=True
        )
        self._thread.start()

    def _serve(self):
        while not self._stop.is_set():
            try:
                conn = self._listener.accept()
            except AuthenticationError:
                continue
            except (OSError, EOFError):
                if not self._stop.is_set():
                    # listener가 예기치 않게 깨진 경우 더 이상 정상적으로 명령을 받을 수 없다.
                    break
                return

            try:
                with conn:
                    command = conn.recv_bytes(32)
                    if command == b"SHOW":
                        self._on_show()
                        conn.send_bytes(b"OK")
                    else:
                        conn.send_bytes(b"UNKNOWN_COMMAND")
            except (OSError, EOFError):
                # 클라이언트가 응답 전에 종료한 경우 서버는 계속 대기한다.
                continue
            except Exception:
                # 하나의 잘못된 요청 때문에 IPC 서버 전체가 죽지 않도록 한다.
                continue

    def close(self):
        self._stop.set()
        try:
            self._listener.close()
        except OSError:
            pass
        # accept()가 닫힌 pipe에서 풀리는 Windows 빌드라면 즉시 정리한다.
        if self._thread.is_alive() and threading.current_thread() is not self._thread:
            self._thread.join(timeout=0.3)


def request_instance_show(timeout: float = 5.0) -> bool:
    """기존 인스턴스에 설정 창 표시를 요청한다. 서버가 준비될 때까지 재시도한다."""
    deadline = time.monotonic() + max(0.0, timeout)
    while True:
        conn = None
        try:
            conn = Client(_PIPE_ADDRESS, family="AF_PIPE", authkey=_AUTHKEY)
            conn.send_bytes(b"SHOW")
            return conn.recv_bytes(32) == b"OK"
        except (OSError, EOFError, ValueError, AuthenticationError):
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.1)
        finally:
            if conn is not None:
                try:
                    conn.close()
                except OSError:
                    pass
