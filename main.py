"""진입점: tkinter 설정 창 + 트레이(winapi.Tray).

레이아웃: 미리보기 → 경로 → [선택][화면 맞춤][적용][중지]. 추가 옵션은 트레이 '옵션' 메뉴에 둔다.
python main.py          설정 창 표시 (닫으면 트레이로 숨김). 이미 백그라운드로 실행 중이면 그 창을 연다
python main.py --tray   마지막 영상을 복원하고 트레이로 시작 (이미 실행 중이면 아무것도 안 함)

스레드 규칙: tkinter는 메인 스레드에서만 만진다. ffmpeg/mpv 스레드에서는 root.after(0, ...)로 넘긴다.
"""
import os
import queue
import sys
import threading
import tkinter as tk
from tkinter import filedialog, messagebox, ttk

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "src"))

import media
import winapi
from instance import InstanceServer, request_instance_show
from config import Settings, icon_path
from player import FIT_MODES, Player
from wallpaper import Wallpaper

VIDEO_TYPES = [("Video/Image", "*.mp4 *.m4v *.mov *.webm *.mkv *.avi *.wmv *.gif *.webp *.apng"),
               ("All files", "*.*")]

MORE_CLOSED, MORE_OPEN = "More \u2228", "More \u2227"   # More v / More ^

INSTANCE_POLL_MS = 200   # 다른 실행 요청(SHOW)을 확인하는 주기

# 트레이 '옵션' 메뉴: (표시 이름, Settings 속성)
OPTIONS = [("Optimize videos (screen-size resize, 30fps cap, bitrate cap)", "optimize"),
           ("Pause in battery saver mode", "pause_on_battery_saver"),
           ("Auto-apply last video on launch", "restore_on_start")]


class App:
    def __init__(self, settings, instance_events):
        self.s = settings
        self._busy = False
        self._pending = None          # 변환 중인 작업: (원본 경로, 적용 여부, 선택 여부)
        self._preview_path = ""
        self._retry = None            # 자동 적용 재시도 예약
        self.tray = None
        self._instance_events = instance_events  # IPC 스레드와 Tk 메인 스레드 사이의 안전한 큐

        self.root = root = tk.Tk()
        self._seen_errors = set()
        root.report_callback_exception = self._on_callback_error  # exe에서는 콘솔이 없어 오류가 사라지므로 오류 창으로 알림
        root.title("LiveWallpaper")
        if icon_path():
            try:
                root.iconbitmap(icon_path())   # 설정 창/작업 표시줄 아이콘
            except tk.TclError:
                pass                           # 아이콘 파일이 깨져도 기본 아이콘으로 계속 실행
        root.geometry("560x480")
        root.minsize(420, 320)
        root.protocol("WM_DELETE_WINDOW", self.close_window)

        self.preview_frame = tk.Frame(root, bg="black", height=260)
        self.preview_frame.pack(fill="both", expand=True, padx=8, pady=8)
        self.path_var = tk.StringVar(value=self._path_label())
        ttk.Label(root, textvariable=self.path_var, wraplength=520, justify="left").pack(fill="x", padx=8)

        row = ttk.Frame(root)
        row.pack(fill="x", padx=8, pady=8)
        self.btn_pick = ttk.Button(row, text="Select video", command=self.pick)
        self.btn_apply = ttk.Button(row, text="Apply wallpaper", command=self.apply)
        self.btn_stop = ttk.Button(row, text="Stop wallpaper", command=self.stop_wallpaper)
        self.fit_var = tk.StringVar(value=FIT_MODES[settings.fit_mode])
        fit_combo = ttk.Combobox(row, textvariable=self.fit_var, values=list(FIT_MODES.values()),
                                 state="readonly", width=8)
        fit_combo.bind("<<ComboboxSelected>>", self.change_fit)
        self.btn_pick.pack(side="left")
        ttk.Label(row, text="Fit mode:").pack(side="left", padx=(12, 4))
        fit_combo.pack(side="left")
        self.btn_stop.pack(side="right")
        self.btn_apply.pack(side="right", padx=(0, 6))

        # 'More' 접이식 영역: 기본은 숨김. 항목을 늘리려면 _build_more()에서 _add_more_row()만 호출하면 된다.
        self._more_open = False
        self._more_delta = 0          # 펼칠 때 늘린 창 높이 (접을 때 그만큼 되돌림)
        self._more_rows = 0
        # 버튼 모양 없이 글씨만 보이는 토글. 항상 창의 가장 아래에 두고, 펼친 내용은 그 위에 끼워 넣는다.
        self.btn_more = ttk.Label(root, text=MORE_CLOSED, cursor="hand2")
        self.btn_more.bind("<Button-1>", lambda _e: self.toggle_more())
        self.btn_more.pack(side="bottom", anchor="w", padx=8, pady=(0, 8))
        self.more_frame = ttk.Frame(root)   # 펼칠 때만 pack (btn_more 바로 위)
        self._build_more()

        self.preview = Player(self.preview_frame.winfo_id(), settings.fit_mode, settings.hwdec,
                              lambda _t: self._later(self.path_var.set, "Unable to read the video."))
        self.wall = Wallpaper(root, settings, lambda t: self._warn("Playback failed", t), self._on_status,
                         lambda p: self._auto_apply(p))

        # 미리보기: 창이 숨겨지거나 최소화되면 재생을 멈추고 디코더 해제
        root.bind("<Unmap>", lambda e: self.preview.suspend() if e.widget is root else None)
        root.bind("<Map>", lambda e: self.preview.resume() if e.widget is root else None)

        try:
            self._build_tray()
        except Exception:  # noqa: BLE001  트레이를 못 만들면 창 닫기 = 종료
            self.tray = None
        # Named Pipe 요청은 큐에만 넣고, 실제 Tk 작업은 메인 스레드에서 처리한다.
        self._instance_poll = self.root.after(INSTANCE_POLL_MS, self._process_instance_events)
        self._refresh_buttons()

    def source(self) -> str:
        """재생에 쓸 입력 경로: 원본이 있으면 원본, 없으면 cache/의 사본, 둘 다 없으면 빈 문자열."""
        for p in (self.s.video_path, self.s.media_path):
            if p and os.path.isfile(p):
                return p
        return ""

    def _path_label(self) -> str:
        s = self.s
        if s.video_path and os.path.isfile(s.video_path):
            return s.video_path
        if s.media_path and os.path.isfile(s.media_path):
            name = os.path.basename(s.video_path) or "Saved video"
            return f"{name}\n(Original missing · playing saved copy)"
        return "No video selected"

    def _on_callback_error(self, exc, val, tb):
        """tk 콜백에서 예외가 나면 오류 창으로 알린다. 파일에는 아무것도 쓰지 않는다.
        같은 오류가 반복돼도(예: 주기 작업) 창은 한 번만 띄운다."""
        import traceback
        summary = "".join(traceback.format_exception_only(exc, val)).strip()
        if sys.stderr:   # 개발 중 콘솔 실행일 때만 전체 내용을 보여줌 (windowed exe에서는 None)
            traceback.print_exception(exc, val, tb)
        if summary in self._seen_errors or len(self._seen_errors) >= 20:
            return
        self._seen_errors.add(summary)
        winapi.message_box(f"An unexpected error occurred.\n\n{summary}")

    def _process_instance_events(self):
        """IPC 작업 스레드에서 온 요청을 Tk 메인 스레드에서 처리한다."""
        try:
            while True:
                command = self._instance_events.get_nowait()
                if command == "SHOW":
                    self.show_window()
        except queue.Empty:
            pass
        try:
            self._instance_poll = self.root.after(INSTANCE_POLL_MS, self._process_instance_events)
        except tk.TclError:
            self._instance_poll = None

    # ---- 스레드 ----
    def _later(self, fn, *args):
        """다른 스레드(트레이/ffmpeg/mpv)에서 tk 메인 스레드로 넘김. 창이 이미 닫혔으면 무시."""
        try:
            self.root.after(0, fn, *args)
        except (RuntimeError, tk.TclError):
            pass

    # ---- 트레이 (콜백은 tk 스레드에서 호출됨. 창을 없애는 동작이 있어 after로 한 템포 미룬다) ----
    def _post(self, fn):
        return lambda: self.root.after(0, fn)

    def _menu(self):
        """트레이 메뉴를 열 때마다 호출 → 체크 표시가 항상 최신."""
        s = self.s
        opts = [(t, self._post(lambda a=a: self._set(a, not getattr(s, a))), getattr(s, a))
                for t, a in OPTIONS] + [None]
        opts.append(("Run at Windows startup", self._post(self._toggle_autostart),
                     s.autostart, winapi.autostart_supported()))
        return [("Open settings", self._post(self.show_window)),
                ("Stop wallpaper", self._post(self.stop_wallpaper)),
                ("Options", opts), None,
                ("Exit", self._post(self.quit_app))]

    def _build_tray(self):
        tray = winapi.Tray("LiveWallpaper", self._menu, self._post(self.show_window), icon_path())
        self.tray = tray if tray.alive else None   # 아이콘 등록이 늦어도 객체는 유지 (작업 표시줄 생성 시 복구)

    def _toggle_autostart(self):
        want = not self.s.autostart
        if winapi.set_autostart(want):
            self._set("autostart", want)
        else:
            self._warn("Startup", "Failed to write to the registry.")

    def _set(self, attr, value):
        setattr(self.s, attr, value)
        self.s.save()

    def _on_status(self, text):
        self._tooltip(text)

    def _tooltip(self, text=""):
        if self.tray:
            self.tray.set_tooltip(f"LiveWallpaper - {text}" if text else "LiveWallpaper")

    # ---- 창 ----
    def show_window(self):
        """숨겨진/최소화된 설정 창을 앞으로 가져온다. 포그라운드 잠금 때문에 다른 창 뒤에 뜨는 일을 막으려고
        잠깐 topmost로 올렸다 내린다. 실패하면 오류 창으로 알린다 (창 없는 exe라 오류가 어디에도 안 보이므로)."""
        try:
            r = self.root
            r.deiconify()
            r.state("normal")
            r.update_idletasks()
            r.attributes("-topmost", True)
            r.lift()
            r.focus_force()
            r.after(400, lambda: r.attributes("-topmost", False))
        except Exception as e:  # noqa: BLE001
            winapi.message_box(f"Failed to open the settings window.\n\n{e!r}")

    def close_window(self):
        if not (self.tray and self.tray.ok):  # 트레이 아이콘이 보이지 않으면 숨길 곳이 없으므로 종료
            self.quit_app()
            return
        self.root.withdraw()  # 닫기 = 트레이로 숨김
        if self.wall.active:
            self.tray.notify("Still running in the system tray.")

    def quit_app(self):
        try:
            self.wall.stop()
            self.preview.stop()
        finally:   # 위에서 예외가 나도 트레이 아이콘(유령 아이콘)과 창은 반드시 정리
            try:
                if self.tray:
                    self.tray.close()
            finally:
                self.root.destroy()

    def _warn(self, title, text):
        self.show_window()
        messagebox.showwarning(title, text, parent=self.root)

    # ---- 영상 준비 (ffmpeg는 별도 스레드: UI가 멈추지 않도록) ----
    def _begin_prepare(self, path, *, apply_after, picked=False):
        if self._busy:
            return
        self._pending = (path, apply_after, picked)
        self.path_var.set(f"{path}\n(Preparing video...)")
        self._set_busy(True)
        w, h = self.root.winfo_screenwidth(), self.root.winfo_screenheight()  # DPI aware → 물리 픽셀
        optimize, codec = self.s.optimize, self.s.video_codec

        def work():
            try:
                if media.in_cache(path):   # 원본이 없어 cache의 사본을 그대로 쓰는 경우
                    res = media.PrepareResult(path, "Original missing: using saved copy")
                else:
                    res = media.prepare(path, w, h, optimize, codec)
            except Exception as e:  # noqa: BLE001
                self._later(self._on_prepare_failed, str(e))
            else:
                self._later(self._on_prepared, res)

        threading.Thread(target=work, daemon=True).start()

    def _set_busy(self, busy):
        self._busy = busy
        state = "disabled" if busy else "normal"
        self.btn_pick.config(state=state)
        self.btn_apply.config(state=state)

    def _on_prepare_failed(self, text):
        self._set_busy(False)
        path = self._pending[0] if self._pending else ""
        self.path_var.set(self._path_label() if self.source() else "Unable to prepare the video.")
        self._warn("Video preparation failed", f"{os.path.basename(path)}\n{text}")

    def _on_prepared(self, res):
        self._set_busy(False)
        path, apply_after, picked = self._pending
        if picked:
            self.s.video_path = path
        if picked or res.path != self.s.media_path:   # 옵션/원본 변경으로 사본이 바뀐 경우도 갱신
            self.s.media_path = res.path
            self.s.save()
        self.path_var.set(self._path_label())
        if res.path != self._preview_path:
            self._preview_path = res.path
            if self.root.winfo_viewable():
                self.preview.play(res.path)   # 이미 재생 중이어도 새 영상으로 교체
            else:
                self.preview.load(res.path)   # 숨겨져 있으면 경로만 기억 (창이 보일 때 재생)
        if res.note:
            self._tooltip(res.note)
        if apply_after == "auto":
            self._auto_apply(res.path)
        elif apply_after:
            self._cancel_retry()   # 수동 적용 뒤에 예전 영상으로 재시도가 다시 덮어쓰지 않게
            ok, msg = self.wall.start(res.path)
            if not ok:
                self._warn("Apply failed", msg)
            self._refresh_buttons()
        self._prune_cache(res.path)

    def _prune_cache(self, current=""):
        """지금 쓰는 사본(설정/배경화면/미리보기)만 남기고 cache/를 정리한다."""
        media.prune_cache({current, self.s.media_path, self.wall.path, self._preview_path})

    def _cancel_retry(self):
        if self._retry:
            try:
                self.root.after_cancel(self._retry)
            except tk.TclError:
                pass
            self._retry = None

    def _auto_apply(self, path, attempt=0):
        """시작 직후/Explorer 재시작 직후에는 바탕화면 창이 아직 없을 수 있어 5초마다 조용히 재시도 (최대 2분)."""
        self._cancel_retry()   # 재시도 예약이 겹쳐서 여러 갈래로 늘어나지 않게
        ok, msg = self.wall.start(path)
        if not ok:
            if attempt < 24:
                self._retry = self.root.after(5000, self._auto_apply, path, attempt + 1)
            else:
                self._warn("Apply failed", msg)
        self._refresh_buttons()

    # ---- 동작 ----
    def pick(self):
        start = os.path.dirname(self.s.video_path) if self.s.video_path else ""
        path = filedialog.askopenfilename(parent=self.root, title="Select video", initialdir=start,
                                          filetypes=VIDEO_TYPES)
        if not path:
            return
        mb = self._size_mb(path)
        if media.is_animated_image(path) and mb > self.s.size_warning_mb:
            if not messagebox.askyesno(
                    "Size warning", f"The file is large: {mb:.1f}MB (threshold {self.s.size_warning_mb}MB).\n"
                    "Conversion may take a long time and use a lot of memory. Continue?", parent=self.root):
                return
        self._begin_prepare(path, apply_after=False, picked=True)

    @staticmethod
    def _size_mb(path):
        try:
            return os.path.getsize(path) / (1024 * 1024)
        except OSError:
            return 0.0

    def change_fit(self, _event=None):
        mode = next(k for k, v in FIT_MODES.items() if v == self.fit_var.get())
        self.preview.set_fit_mode(mode)
        self.wall.set_fit_mode(mode)
        self._set("fit_mode", mode)

    # ---- 'More' 영역 ----
    def _build_more(self):
        """'More' 영역에 들어갈 항목들. 새 설정을 넣으려면 아래처럼 한 줄 추가한다:
            self._add_more_row("Label:", self._make_xxx_widget, "(optional hint)")
        make_widget(parent)는 parent를 부모로 하는 위젯을 만들어 돌려주면 된다."""
        self.more_frame.columnconfigure(2, weight=1)
        self._add_more_row("Cache codec:", self._make_codec_combo, "(applied on next Select/Apply)")

    def _add_more_row(self, label, make_widget, hint=""):
        r = self._more_rows
        self._more_rows += 1
        ttk.Label(self.more_frame, text=label).grid(row=r, column=0, sticky="w", padx=(0, 6), pady=2)
        widget = make_widget(self.more_frame)
        widget.grid(row=r, column=1, sticky="w", pady=2)
        if hint:
            ttk.Label(self.more_frame, text=hint).grid(row=r, column=2, sticky="w", padx=(8, 0), pady=2)
        return widget

    def _make_codec_combo(self, parent):
        self.codec_var = tk.StringVar(value=media.CODEC_LABELS[self.s.video_codec])
        combo = ttk.Combobox(parent, textvariable=self.codec_var, values=list(media.CODEC_LABELS.values()),
                             state="readonly", width=14)
        combo.bind("<<ComboboxSelected>>", self.change_codec)
        return combo

    def toggle_more(self):
        """More 영역을 펼치고 접는다. 펼칠 때 창 높이를 그만큼 늘리고, 접을 때 되돌린다 (미리보기 크기 유지)."""
        r = self.root
        self._more_open = not self._more_open
        self.btn_more.config(text=MORE_OPEN if self._more_open else MORE_CLOSED)
        resizable = r.state() == "normal"   # 최대화 상태에서는 창 크기를 건드리지 않는다
        if self._more_open:
            self.more_frame.pack(side="bottom", fill="x", padx=8, pady=(0, 4))
            r.update_idletasks()
            self._more_delta = self.more_frame.winfo_reqheight() + 4
            if resizable:
                r.geometry(f"{r.winfo_width()}x{r.winfo_height() + self._more_delta}")
        else:
            self.more_frame.pack_forget()
            if resizable:
                r.geometry(f"{r.winfo_width()}x{max(r.winfo_height() - self._more_delta, 1)}")
            self._more_delta = 0

    def change_codec(self, _event=None):
        """cache 사본 코덱 변경. 이미 만든 사본은 그대로 두고, 다음 선택/적용 때 새 코덱으로 사본을 만든다."""
        codec = next(k for k, v in media.CODEC_LABELS.items() if v == self.codec_var.get())
        self._set("video_codec", codec)

    def apply(self):
        path = self.source()
        if not (self.s.video_path or self.s.media_path):
            messagebox.showinfo("Notice", "Please select a video first.", parent=self.root)
        elif not path:
            messagebox.showwarning("Apply failed", "Video file not found. "
                                   "Both the original and the saved copy are missing. Please select the video again.", parent=self.root)
        else:
            self._begin_prepare(path, apply_after=True)

    def start_up(self, restoring: bool):
        """실행 직후: 마지막 영상을 미리보기에 불러오고, 설정에 따라 배경화면으로도 적용."""
        path = self.source()
        if path:
            self._begin_prepare(path, apply_after="auto" if restoring else False)

    def stop_wallpaper(self):
        self._cancel_retry()
        self.wall.stop()
        self._refresh_buttons()
        self._tooltip()

    def _refresh_buttons(self):
        self.btn_stop.config(state="normal" if self.wall.active else "disabled")


def main():
    # 단일 실행 잠금과 IPC를 분리한다. 기존 인스턴스가 있으면 Named Pipe로
    # SHOW 명령만 전달하고, 절대로 다른 프로세스를 강제 종료하지 않는다.
    if not winapi.acquire_instance():
        if "--tray" in sys.argv:  # 부팅 자동 실행이 겹치면 조용히 종료
            return
        if not request_instance_show():
            winapi.message_box(
                "LiveWallpaper is already running but did not respond to the open-window request.\n"
                "If you just updated from an older version, exit the running LiveWallpaper once and "
                "launch the new version."
            )
        return

    server = None
    try:
        winapi.enable_dpi_awareness()  # Tk 창을 만들기 전에
        settings = Settings.load()
        if settings.autostart:
            winapi.set_autostart(True)  # 폴더를 옮기거나 파이썬 경로가 바뀌어도 매 실행 때 복구

        instance_events = queue.Queue()
        # 앱 UI가 완전히 만들어지기 전 도착한 요청도 큐에 보관된다.
        server = InstanceServer(lambda: instance_events.put("SHOW"))
        app = App(settings, instance_events)
        restoring = settings.restore_on_start and bool(app.source())
        if "--tray" in sys.argv and restoring and app.tray and app.tray.ok:  # 복원 실패 시에는 창이 다시 열림
            app.root.withdraw()
        app.start_up(restoring)
        app.root.mainloop()
    except Exception as e:  # noqa: BLE001
        import traceback
        if sys.stderr:   # 개발 중 콘솔 실행일 때만 (windowed exe에서는 None)
            traceback.print_exc()
        winapi.message_box("Failed to start LiveWallpaper.\n\n"
                           + "".join(traceback.format_exception_only(type(e), e)).strip())
    finally:
        if server:
            server.close()
        winapi.release_instance()


if __name__ == "__main__":
    main()
