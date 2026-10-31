"""설정 저장/불러오기 (JSON). 잘못된 값이나 깨진 파일이 있어도 기본값으로 복구한다."""
import json
import os
import sys
from dataclasses import asdict, dataclass, fields
from pathlib import Path


def app_dir() -> Path:
    """프로그램 폴더. 개발 중에는 프로젝트 루트(src/의 상위), EXE로 실행하면 EXE가 있는 폴더.
    설정(config.json)과 변환 캐시(cache/)는 AppData가 아니라 모두 이 폴더 안에 둔다."""
    if getattr(sys, "frozen", False):
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent.parent


def config_dir() -> Path:
    return app_dir()


def icon_path() -> str:
    """앱 아이콘(.ico) 경로. 없으면 빈 문자열(→ 시스템 기본 아이콘).
    EXE로 실행하면 EXE 옆의 icon.ico(build.bat이 복사), 개발 중에는 builder/icon.ico."""
    p = app_dir() / ("icon.ico" if getattr(sys, "frozen", False) else "builder/icon.ico")
    return str(p) if p.is_file() else ""


@dataclass
class Settings:
    video_path: str = ""                 # 사용자가 선택한 원본 경로 (표시/다음 선택 시작 위치용, 지워져도 됨)
    media_path: str = ""                 # 실제로 재생하는 cache/ 안의 사본 (원본이 없어도 이것으로 복원)
    fit_mode: str = "contain"            # "contain"(맞추기) | "cover"(채우기) | "stretch"(늘리기)
    size_warning_mb: int = 50            # GIF 등 움직이는 이미지가 이 용량을 넘으면 선택 시 경고
    optimize: bool = True                # 화면보다 큰 영상은 모니터 해상도로 변환, fps/비트레이트 제한
    video_codec: str = "h264"            # cache 사본 코덱: "h264"(AVC) | "hevc"(H.265). 다르면 ffmpeg로 재인코딩
    pause_on_battery_saver: bool = True
    coverage_threshold: float = 0.95
    restore_on_start: bool = True        # 실행 시 마지막 영상을 자동 적용
    autostart: bool = True               # Windows 시작 시 자동 실행 (실행할 때마다 등록을 확인/복구)
    hwdec: str = "auto-safe"             # mpv 하드웨어 디코딩. "no"=끔, "d3d11va" 등 직접 지정 가능

    @classmethod
    def load(cls, path: Path | None = None) -> "Settings":
        path = path or config_dir() / "config.json"
        s = cls()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return s
        if not isinstance(data, dict):
            return s
        for f in fields(cls):
            if f.name in data and isinstance(data[f.name], type(getattr(s, f.name))):
                setattr(s, f.name, data[f.name])
        if s.fit_mode not in ("contain", "cover", "stretch"):
            s.fit_mode = "contain"
        if s.video_codec not in ("h264", "hevc"):
            s.video_codec = "h264"
        if not s.hwdec.strip():
            s.hwdec = "auto-safe"
        if not (0.0 < s.coverage_threshold <= 1.0):
            s.coverage_threshold = 0.95
        return s

    def save(self, path: Path | None = None) -> bool:
        path = path or config_dir() / "config.json"
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_suffix(".tmp")
            tmp.write_text(json.dumps(asdict(self), ensure_ascii=False, indent=2), encoding="utf-8")
            os.replace(tmp, path)
            return True
        except OSError:
            return False
