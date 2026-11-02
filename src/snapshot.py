"""정적 전환: Windows 바탕화면을 영상 첫 프레임으로 바꿨다가 원래대로 되돌리는 로직.

Windows API는 get/set 함수로 주입받으므로 이 모듈은 Windows 없이도 테스트할 수 있다.

안전 규칙
  - 원래 바탕화면 정보는 바꾸기 *전에* 파일(wallpaper_state.json)에 저장한다 → 비정상 종료 뒤에도 복구 가능
  - 지금 바탕화면이 우리가 적용한 것일 때만 되돌린다 → 사용자가 중간에 직접 바꿨다면 덮어쓰지 않음
  - 슬라이드쇼/단색/원본 파일 없음처럼 원상 복원이 확실하지 않은 경우에는 바꾸지 않는다
"""
import json
import os
from pathlib import Path

# 슬라이드쇼 등에서 Windows가 만드는 임시 파일. 우리가 바탕화면을 바꾸면 덮어써지므로 복원할 수 없다.
_UNRESTORABLE_PREFIX = "transcodedwallpaper"


def _same(a: str, b: str) -> bool:
    if not a or not b:
        return False
    return os.path.normcase(os.path.abspath(a)) == os.path.normcase(os.path.abspath(b))


class WallpaperSwapper:
    def __init__(self, state_path: Path, get_wallpaper, set_wallpaper, isfile=os.path.isfile):
        self._path = Path(state_path)
        self._get = get_wallpaper      # () -> str
        self._set = set_wallpaper      # (path) -> bool
        self._isfile = isfile

    # ---- 상태 파일 ----
    def _load(self) -> dict | None:
        try:
            data = json.loads(self._path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        if isinstance(data, dict) and all(isinstance(data.get(k), str) for k in ("original", "frame", "applied")):
            return data
        return None

    def _save(self, data: dict) -> bool:
        try:
            self._path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._path.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
            os.replace(tmp, self._path)
            return True
        except OSError:
            return False

    def _clear(self):
        try:
            self._path.unlink()
        except OSError:
            pass

    @property
    def swapped(self) -> bool:
        """우리가 바꾼 바탕화면이 남아 있을 수 있는가 (상태 파일이 있는가)."""
        return self._load() is not None

    def _is_ours(self, st: dict, current: str) -> bool:
        return _same(current, st["applied"]) or _same(current, st["frame"])

    # ---- 전환 ----
    def enter(self, frame: str) -> tuple[bool, str]:
        """Windows 바탕화면을 frame 이미지로 바꾼다. 반환: (성공, 실패 이유)."""
        if not frame or not self._isfile(frame):
            return False, "First-frame image is missing"
        st = self._load()
        current = self._get()
        if st and self._is_ours(st, current):
            original = st["original"]          # 이미 우리가 바꾼 상태 → 진짜 원본은 저장해 둔 것
        else:
            original = current                 # 지금 바탕화면이 사용자의 진짜 바탕화면
            if not original:
                return False, "Windows wallpaper is not a single picture (solid color?)"
            if os.path.basename(original).lower().startswith(_UNRESTORABLE_PREFIX):
                return False, "Windows wallpaper is a slideshow/transcoded image and cannot be restored"
            if not self._isfile(original):
                return False, "Original wallpaper file is missing"
        # 바꾸기 전에 먼저 저장 (바꾸는 도중 종료돼도 복구되도록 applied에는 일단 frame을 넣어 둔다)
        if not self._save({"original": original, "frame": frame, "applied": frame}):
            return False, "Unable to save the wallpaper restore info"
        if not self._set(frame):
            if st:
                self._save(st)                 # 이전 상태 유지 (원본 정보를 잃지 않는다)
            else:
                self._clear()
            return False, "Windows rejected the wallpaper change"
        applied = self._get()                  # 적용 뒤 Windows가 돌려주는 값 (경로 표기가 달라질 수 있어 읽어서 저장)
        if applied and not _same(applied, frame):
            self._save({"original": original, "frame": frame, "applied": applied})
        return True, ""

    def restore(self) -> bool:
        """원래 바탕화면으로 되돌린다. 바꾼 적이 없거나 사용자가 직접 바꿨다면 아무것도 덮어쓰지 않는다.
        반환: 정상 처리 여부 (False면 상태 파일을 남겨 다음 실행에서 다시 시도)."""
        st = self._load()
        if not st:
            return True
        if self._is_ours(st, self._get()):
            if not self._isfile(st["original"]):
                self._clear()                  # 원본이 사라져 복원 불가 → 더 시도해도 소용없음
                return False
            if not self._set(st["original"]):
                return False
        self._clear()
        return True
