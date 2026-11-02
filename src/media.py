"""영상 준비: 선택한 영상을 프로젝트 cache/ 폴더에 두고 그 사본으로 재생한다 (원본을 지워도 배경이 유지됨).

- GIF 등 움직이는 이미지, 화면보다 큰 영상, 30fps 초과 영상은 모니터 해상도의 MP4로 변환해 cache/에 저장
- 사본의 코덱은 설정(video_codec)에 따라 H.264(AVC) 또는 H.265(HEVC). 원본 코덱이 다르면 ffmpeg로 재인코딩한다
- 이미 같은 코덱이고 크기/fps/비트레이트가 적당한 영상은 변환 없이 cache/에 그대로 복사
- 재생 경로는 항상 cache/ 안의 파일이다. 더 이상 쓰지 않는 파일은 prune_cache()로 정리한다.

Qt에 의존하지 않으므로 어느 OS에서든 단독 테스트할 수 있다. ffmpeg가 필요하다:
시스템 PATH의 ffmpeg 또는 `pip install imageio-ffmpeg`. (ffmpeg가 없으면 변환 없이 복사만 한다.)
"""
import functools
import hashlib
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

ANIMATED_IMAGE_EXTS = {".gif", ".apng", ".webp"}
MAX_FPS = 30

# 코덱별 인코딩 설정 (퀄리티/용량 균형 기준). 배경화면은 오디오 없는 반복 재생이라
# 화질 저하가 눈에 덜 띄는 대신 용량·디코딩 부담은 계속 쌓이므로, 기본 화질보다 약간 높은 수준에서
# CRF(품질 기준)로 화질을 정하고 maxrate(상한)로 복잡한 장면의 비트레이트 폭주를 막는다.
#   - crf: x264 22 ≈ x265 26 (x265는 같은 CRF에서 더 작게 나오므로 4 높게 잡아 체감 화질을 맞춤)
#   - base_kbps: 1080p 기준 상한. 해상도(픽셀 수)에 비례해 조정. HEVC는 효율이 좋아 낮게 잡음
#   - profile/pix_fmt: 8-bit 4:2:0만 사용 → GPU 하드웨어 디코딩 호환성이 가장 넓음
CODECS = {
    "h264": {"label": "H.264 (AVC)", "encoder": "libx264", "crf": 22, "base_kbps": 8000, "profile": "high"},
    "hevc": {"label": "H.265 (HEVC)", "encoder": "libx265", "crf": 26, "base_kbps": 5000, "profile": "main"},
}
DEFAULT_CODEC = "h264"
CODEC_LABELS = {k: v["label"] for k, v in CODECS.items()}
_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0) if sys.platform == "win32" else 0
FRAME_INFIX = ".frame2-"   # cache/<영상>.frame2-<맞춤>-<가로>x<세로>.png = 정적 전환용 첫 프레임 (색 변환 수정 후 형식: 이전 ".frame-" 파일은 정리 때 지워짐)


_COLORSPACE_TAG = re.compile(r"\b(?:bt709|bt470bg|bt470m|smpte170m|smpte240m|bt2020nc|bt2020c|fcc|ycgco|ictcp)\b")


class MediaError(Exception):
    pass


@dataclass
class VideoInfo:
    codec: str
    width: int
    height: int
    fps: float | None
    pix_fmt: str = ""
    bitrate_kbps: int | None = None
    color_tagged: bool = False   # 영상 파일에 색 변환 규칙(BT.709 등)이 표시되어 있는가


@dataclass
class PrepareResult:
    path: str
    note: str = ""


def find_ffmpeg() -> str | None:
    exe = shutil.which("ffmpeg")
    if exe:
        return exe
    try:
        import imageio_ffmpeg
        return imageio_ffmpeg.get_ffmpeg_exe()
    except Exception:  # noqa: BLE001
        return None


def is_animated_image(path: str) -> bool:
    return Path(path).suffix.lower() in ANIMATED_IMAGE_EXTS


def parse_ffmpeg_info(text: str) -> VideoInfo | None:
    """`ffmpeg -i` 출력의 첫 비디오 스트림 줄에서 코덱/해상도/fps/픽셀 형식/비트레이트를 읽는다."""
    overall = re.search(r"Duration:.*?bitrate:\s*(\d+)\s*kb/s", text)
    overall_kbps = int(overall.group(1)) if overall else None
    for line in text.splitlines():
        m = re.search(r"Video:\s*([\w-]+)", line)
        if not m:
            continue
        size = re.search(r"(?<![\w])(\d{2,5})x(\d{2,5})(?=[\s,\[])", line[m.end():])
        if not size:
            continue
        fps = re.search(r"([\d.]+)\s*fps", line)
        pix = re.search(r",\s*((?:yuvj?|nv|p0|gray|gbr|rgb|bgr|pal)\w*)", line[m.end():])
        stream_kbps = re.search(r"(\d+)\s*kb/s", line[m.end():])
        paren = re.search(r",\s*(?:yuvj?|nv|p0)\w*\(([^)]*)\)", line[m.end():])   # 예: yuv420p(tv, bt709, progressive)
        tagged = bool(paren and _COLORSPACE_TAG.search(paren.group(1)))
        return VideoInfo(m.group(1), int(size.group(1)), int(size.group(2)),
                         float(fps.group(1)) if fps else None,
                         pix.group(1) if pix else "",
                         int(stream_kbps.group(1)) if stream_kbps else overall_kbps,
                         tagged)
    return None


def probe(path: str, ffmpeg: str) -> VideoInfo | None:
    try:
        r = subprocess.run([ffmpeg, "-hide_banner", "-i", path], capture_output=True, text=True,
                           errors="replace", timeout=30, creationflags=_NO_WINDOW)
    except (OSError, subprocess.SubprocessError):
        return None
    return parse_ffmpeg_info(r.stderr)  # 출력 파일이 없어 returncode=1이 정상


def fit_size(w: int, h: int, max_w: int, max_h: int) -> tuple[int, int]:
    """비율을 유지하며 (max_w, max_h) 안으로 축소(확대 없음). H.264를 위해 짝수로 맞춘다."""
    scale = min(1.0, max_w / w, max_h / h)
    nw, nh = int(w * scale), int(h * scale)
    return max(2, nw - nw % 2), max(2, nh - nh % 2)


def max_bitrate_kbps(codec: str, w: int, h: int) -> int:
    """출력 해상도에 비례한 비트레이트 상한(kb/s). 1080p 기준값의 0.3배 ~ 3.75배로 제한."""
    base = CODECS.get(codec, CODECS[DEFAULT_CODEC])["base_kbps"]
    return int(min(base * 3.75, max(base * 0.3, base * (w * h) / (1920 * 1080))))


@functools.lru_cache(maxsize=None)
def has_encoder(ffmpeg: str, name: str) -> bool:
    """이 ffmpeg 빌드에 해당 인코더(libx264/libx265)가 있는가. 확인 못 하면 일단 있다고 보고 시도한다."""
    try:
        r = subprocess.run([ffmpeg, "-hide_banner", "-encoders"], capture_output=True, text=True,
                           errors="replace", timeout=30, creationflags=_NO_WINDOW)
    except (OSError, subprocess.SubprocessError):
        return True
    return re.search(rf"\b{re.escape(name)}\b", r.stdout) is not None


def needs_conversion(path: str, info: VideoInfo | None, max_w: int, max_h: int,
                     optimize: bool, codec: str = DEFAULT_CODEC) -> str:
    """변환이 필요하면 이유(문자열), 아니면 빈 문자열.

    코덱이 설정과 다르면 optimize와 무관하게 재인코딩한다. 크기/fps/비트레이트/비트 심도 검사는 optimize일 때만.
    """
    if is_animated_image(path):
        return "Animated images must be converted to MP4 to keep CPU load low"
    if not info:
        return ""
    label = CODECS[codec]["label"]
    if info.codec != codec:
        return f"Codec {info.codec} -> {label}"
    if optimize:
        if info.width > max_w * 1.05 or info.height > max_h * 1.05:
            return f"Source {info.width}x{info.height} is larger than the screen ({max_w}x{max_h})"
        if info.fps and info.fps > MAX_FPS + 0.5:
            return f"{info.fps:g}fps -> {MAX_FPS}fps"
        if re.search(r"(?:10|12|16)(?:le|be)", info.pix_fmt):
            return f"{info.pix_fmt} -> 8-bit"
        if info.bitrate_kbps:
            w, h = fit_size(info.width, info.height, max_w, max_h)
            cap = max_bitrate_kbps(codec, w, h)
            if info.bitrate_kbps > cap * 1.5:
                return f"Bitrate {info.bitrate_kbps} kb/s exceeds target {cap} kb/s"
    return ""


def cache_dir() -> Path:
    from config import config_dir
    return config_dir() / "cache"


def cache_key(src: str, max_w: int, max_h: int, optimize: bool = True,
              codec: str = DEFAULT_CODEC) -> str:
    """원본 경로·크기·수정시각·화면 크기·최적화 여부·코덱이 같으면 같은 이름. 하나라도 바뀌면 새 사본을 만든다."""
    st = os.stat(src)
    key = f"{os.path.abspath(src)}|{st.st_size}|{st.st_mtime_ns}|{max_w}x{max_h}|{int(optimize)}|{codec}"
    return hashlib.sha1(key.encode("utf-8")).hexdigest()[:16]


def cache_path(src: str, max_w: int, max_h: int, directory: Path | None = None,
               optimize: bool = True, suffix: str = ".mp4", codec: str = DEFAULT_CODEC) -> Path:
    return (directory or cache_dir()) / f"{cache_key(src, max_w, max_h, optimize, codec)}{suffix}"


def _norm(path) -> str:
    return os.path.normcase(os.path.abspath(path))


def in_cache(path: str, directory: Path | None = None) -> bool:
    """path가 cache/ 폴더 바로 아래의 파일인가 (이미 사본이면 다시 복사하지 않기 위해)."""
    return os.path.dirname(_norm(path)) == _norm(directory or cache_dir())


def prune_cache(keep, directory: Path | None = None) -> None:
    """cache/ 안에서 keep에 없는 파일(오래된 사본, 중단된 .part 파일)을 지운다.

    사용 중이라 지워지지 않는 파일은 건너뛰고, 다음 정리 때 다시 시도한다.
    cache/ 폴더 직속의 일반 파일만 대상으로 하며 하위 폴더나 다른 경로는 건드리지 않는다.
    """
    d = Path(directory or cache_dir())
    keep_set = {_norm(p) for p in keep if p}
    frame_prefixes = {Path(p).stem + FRAME_INFIX for p in keep if p}   # 유지하는 영상의 첫 프레임 사본
    try:
        entries = list(d.iterdir())
    except OSError:
        return
    for f in entries:
        try:
            if not f.is_file() or _norm(f) in keep_set:
                continue
            if f.suffix == ".png" and ".part" not in f.name and any(f.name.startswith(x) for x in frame_prefixes):
                continue
            f.unlink()
        except OSError:
            pass


def build_command(ffmpeg: str, src: str, dst: str, info: VideoInfo | None,
                  max_w: int, max_h: int, codec: str = DEFAULT_CODEC, limit: bool = True) -> list[str]:
    """ffmpeg 인코딩 명령. limit=True면 화면 크기로 축소하고 fps를 MAX_FPS로 제한한다 (False면 원본 크기/fps 유지)."""
    spec = CODECS[codec]
    filters = []
    if limit and info and info.fps and info.fps > MAX_FPS + 0.5:
        filters.append(f"fps={MAX_FPS}")
    if info and limit:
        w, h = fit_size(info.width, info.height, max_w, max_h)
        filters.append(f"scale={w}:{h}")
    else:
        w, h = (info.width, info.height) if info else (max_w, max_h)
        filters.append("scale=trunc(iw/2)*2:trunc(ih/2)*2")   # 인코더는 짝수 크기만 받는다
    cap = max_bitrate_kbps(codec, w, h)
    cmd = [ffmpeg, "-y", "-hide_banner", "-loglevel", "error", "-i", src, "-an",
           "-vf", ",".join(filters), "-c:v", spec["encoder"], "-preset", "medium",
           "-crf", str(spec["crf"]), "-maxrate", f"{cap}k", "-bufsize", f"{cap * 2}k",
           "-profile:v", spec["profile"], "-pix_fmt", "yuv420p"]
    if codec == "hevc":
        cmd += ["-tag:v", "hvc1", "-x265-params", "log-level=error"]   # hvc1: MP4 호환성, x265 로그 억제
    return cmd + ["-movflags", "+faststart", dst]


def prepare(src: str, max_w: int, max_h: int, optimize: bool = True,
            codec: str = DEFAULT_CODEC, directory: Path | None = None) -> PrepareResult:
    """재생할 파일 경로를 돌려준다. 항상 cache/ 안의 사본이며, 원본 경로는 돌려주지 않는다.

    이미 같은 조건의 사본이 있으면 그대로 쓰고, 없으면 변환(또는 복사)해서 만든다.
    사본은 임시 이름(.part)으로 쓴 뒤 os.replace로 확정하므로 중간에 끊겨도 반쪽 파일이 재생되지 않는다.
    """
    codec = codec if codec in CODECS else DEFAULT_CODEC
    animated = is_animated_image(src)
    ffmpeg = find_ffmpeg()
    if not ffmpeg and animated:
        raise MediaError("ffmpeg is required to convert GIF/WebP. "
                         "Run `pip install imageio-ffmpeg` or add ffmpeg to PATH.")

    directory = directory or cache_dir()
    ext = Path(src).suffix.lower()
    converted = cache_path(src, max_w, max_h, directory, optimize, ".mp4", codec)
    copied = cache_path(src, max_w, max_h, directory, optimize, ext, codec)
    for hit in (converted, copied):
        if hit.is_file():
            return PrepareResult(str(hit), "Using cached copy")

    info = probe(src, ffmpeg) if ffmpeg else None
    reason = needs_conversion(src, info, max_w, max_h, optimize, codec) if ffmpeg else ""
    used, fallback = codec, ""
    if reason and not has_encoder(ffmpeg, CODECS[codec]["encoder"]):   # 이 ffmpeg 빌드에 H.265 인코더가 없는 경우
        used = DEFAULT_CODEC
        fallback = f" · {CODECS[codec]['label']} encoder unavailable, used {CODECS[used]['label']}"
    dst = converted if reason else copied
    tmp = dst.with_name(dst.stem + ".part" + dst.suffix)
    try:
        dst.parent.mkdir(parents=True, exist_ok=True)
        if reason:
            r = subprocess.run(build_command(ffmpeg, src, str(tmp), info, max_w, max_h, used,
                                         limit=optimize or animated),
                               capture_output=True, text=True, errors="replace",
                               creationflags=_NO_WINDOW)
            if r.returncode != 0 or not tmp.exists():
                raise MediaError(f"Conversion failed: {r.stderr.strip()[-300:] or r.returncode}")
        else:
            shutil.copyfile(src, tmp)
        os.replace(tmp, dst)
    except OSError as e:
        tmp.unlink(missing_ok=True)
        raise MediaError(f"Failed to prepare the video: {e}") from e
    except MediaError:
        tmp.unlink(missing_ok=True)
        raise
    if reason:
        return PrepareResult(str(dst), f"Converted to {CODECS[used]['label']} ({reason}){fallback}")
    note = "" if ffmpeg else "ffmpeg not found: copied without resolution check/optimization"
    return PrepareResult(str(dst), note)


# ---------------- 정적 전환용 첫 프레임 ----------------
def frame_path(video: str, w: int, h: int, fit: str) -> Path:
    """영상 옆(cache/)에 두는 첫 프레임 PNG 경로. 화면 크기와 맞춤 모드가 이름에 들어가므로 바뀌면 새로 만든다."""
    p = Path(video)
    return p.with_name(f"{p.stem}{FRAME_INFIX}{fit}-{w}x{h}.png")


def guess_matrix(info: VideoInfo | None) -> str:
    """YUV → RGB 변환 규칙. mpv와 같은 결과가 되도록 정한다.

    영상에 규칙이 표시돼 있으면 ffmpeg가 그대로 따르므로 "auto". 표시가 없을 때 ffmpeg는 항상 BT.601로 가정하지만
    mpv는 해상도로 추측한다(HD면 BT.709). 그래서 표시가 없는 YUV 영상은 mpv와 같은 기준으로 직접 정한다.
    Uncertain: mpv의 정확한 경계값(가로 1280 초과 또는 세로 576 초과 → BT.709)은 mpv 소스를 확인하지 못했다.
    """
    if not info or info.color_tagged or not info.pix_fmt.startswith(("yuv", "nv", "p0")):
        return "auto"
    return "bt709" if info.width > 1280 or info.height > 576 else "bt601"


def frame_filter(w: int, h: int, fit: str, matrix: str = "auto") -> str:
    """mpv의 맞춤 모드와 같은 결과가 되도록 화면 크기(w x h)의 이미지로 만드는 ffmpeg 필터.

    맨 앞에서 먼저 RGB로 바꾼다: 크기 조절·여백(pad)을 YUV 상태로 한 뒤 마지막에 RGB로 바꾸면
    그 마지막 변환은 규칙이 없는 것으로 보고 항상 BT.601을 쓰기 때문이다.
    """
    head = f"scale=in_color_matrix={matrix},format=rgb24,"
    if fit == "cover":
        return head + f"scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"
    if fit == "stretch":
        return head + f"scale={w}:{h}"
    return (head + f"scale={w}:{h}:force_original_aspect_ratio=decrease,"
            f"pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black")


def build_frame_command(ffmpeg: str, src: str, dst: str, w: int, h: int, fit: str,
                        matrix: str = "auto") -> list[str]:
    return [ffmpeg, "-y", "-hide_banner", "-loglevel", "error", "-i", src,
            "-vf", frame_filter(w, h, fit, matrix), "-frames:v", "1", dst]


def ensure_first_frame(video: str, w: int, h: int, fit: str) -> str:
    """영상의 첫 프레임을 화면 크기 PNG로 만들어 경로를 돌려준다 (이미 있으면 그대로). 실패하면 빈 문자열.

    실패해도 예외를 내지 않는다: 정적 전환은 부가 기능이라 재생 자체를 막으면 안 된다.
    """
    if not video or not os.path.isfile(video) or w <= 0 or h <= 0:
        return ""
    dst = frame_path(video, w, h, fit)
    if dst.is_file():
        return str(dst)
    ffmpeg = find_ffmpeg()
    if not ffmpeg:
        return ""
    tmp = dst.with_name(dst.stem + ".part.png")
    try:
        matrix = guess_matrix(probe(video, ffmpeg))
        r = subprocess.run(build_frame_command(ffmpeg, video, str(tmp), w, h, fit, matrix),
                           capture_output=True, text=True, errors="replace",
                           timeout=60, creationflags=_NO_WINDOW)
        if r.returncode != 0 or not tmp.is_file():
            tmp.unlink(missing_ok=True)
            return ""
        os.replace(tmp, dst)
    except (OSError, subprocess.SubprocessError):
        tmp.unlink(missing_ok=True)
        return ""
    return str(dst)
