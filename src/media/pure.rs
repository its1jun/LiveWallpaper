//! media 순수 로직.
//!
//! 하위 프로세스를 실행하지 않는다. 파일시스템은 `cache_key`(stat 1회)만 건드린다.
//! `regex` 크레이트는 lookaround 를 지원하지 않으므로 소비형 그룹으로 대신한다.

use crate::config::{abspath_key, norm_key, Codec, FitMode};
use regex::Regex;
use sha1::{Digest, Sha1};
use std::io;
use std::path::Path;
use std::sync::OnceLock;

pub const ANIMATED_IMAGE_EXTS: [&str; 3] = [".gif", ".apng", ".webp"];
pub const MAX_FPS: u32 = 30;
/// cache/<영상>.frame2-<맞춤>-<가로>x<세로>.png = 정적 전환용 첫 프레임
pub const FRAME_INFIX: &str = ".frame2-";

// ---------------------------------------------------------------- 코덱 설정
/// 코덱별 인코딩 설정. crf: x264 22 ≈ x265 26, base_kbps: 1080p 기준 상한(해상도에 비례해 조정).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodecSpec {
    pub label: &'static str,
    pub encoder: &'static str,
    pub crf: u32,
    pub base_kbps: i64,
    pub profile: &'static str,
}

/// 코덱별 설정. 순서는 `Codec::index()` 와 같다 (UI 콤보박스 순서).
pub const CODECS: [CodecSpec; 2] = [
    CodecSpec { label: "H.264 (AVC)", encoder: "libx264", crf: 22, base_kbps: 8000, profile: "high" },
    CodecSpec { label: "H.265 (HEVC)", encoder: "libx265", crf: 26, base_kbps: 5000, profile: "main" },
];

pub fn codec_spec(codec: Codec) -> &'static CodecSpec {
    &CODECS[codec.index()]
}

// ---------------------------------------------------------------- 영상 정보
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub pix_fmt: String,
    pub bitrate_kbps: Option<i64>,
    /// 영상 파일에 색 변환 규칙(BT.709 등)이 표시되어 있는가
    pub color_tagged: bool,
}

// ---------------------------------------------------------------- 경로 문자열 도구
/// 소문자 확장자(점 포함). 없으면 빈 문자열.
pub fn suffix_lower(path: &str) -> String {
    Path::new(path)
        .extension()
        .filter(|e| !e.is_empty())
        .map_or(String::new(), |e| format!(".{}", e.to_string_lossy().to_lowercase()))
}

/// 확장자를 뺀 파일명.
pub fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().map_or(String::new(), |s| s.to_string_lossy().into_owned())
}

pub fn is_animated_image(path: &str) -> bool {
    let s = suffix_lower(path);
    ANIMATED_IMAGE_EXTS.contains(&s.as_str())
}

// ---------------------------------------------------------------- ffmpeg 출력 파싱
/// `ffmpeg -i` 출력을 읽는 정규식 모음. 처음 쓸 때 한 번만 컴파일한다.
struct Patterns {
    overall_kbps: Regex,
    video: Regex,
    size: Regex,
    fps: Regex,
    pix_fmt: Regex,
    stream_kbps: Regex,
    color_tags: Regex,
    color_matrix: Regex,
}

fn patterns() -> &'static Patterns {
    static CELL: OnceLock<Patterns> = OnceLock::new();
    CELL.get_or_init(|| {
        let re = |pattern: &str| Regex::new(pattern).expect("정규식 컴파일 실패");
        Patterns {
            overall_kbps: re(r"Duration:.*?bitrate:\s*([0-9]+)\s*kb/s"),
            video: re(r"Video:\s*([\w-]+)"),
            // 크기는 단어 문자에 붙어 있으면 안 된다: 앞 문자는 `^|[^\w]`, 뒤 문자는 `[\s,\[]` 로 소비한다.
            size: re(r"(?:^|[^\w])([0-9]{2,5})x([0-9]{2,5})[\s,\[]"),
            fps: re(r"([0-9.]+)\s*fps"),
            pix_fmt: re(r",\s*((?:yuvj?|nv|p0|gray|gbr|rgb|bgr|pal)\w*)"),
            stream_kbps: re(r"([0-9]+)\s*kb/s"),
            // 예: yuv420p(tv, bt709, progressive) → 괄호 안 내용
            color_tags: re(r",\s*(?:yuvj?|nv|p0)\w*\(([^)]*)\)"),
            color_matrix: re(r"\b(?:bt709|bt470bg|bt470m|smpte170m|smpte240m|bt2020nc|bt2020c|fcc|ycgco|ictcp)\b"),
        }
    })
}

/// `ffmpeg -i` 출력의 첫 비디오 스트림 줄에서 코덱/해상도/fps/픽셀 형식/비트레이트를 읽는다.
/// 비디오 줄이 없으면 `None`.
pub fn parse_ffmpeg_info(text: &str) -> Option<VideoInfo> {
    let re = patterns();
    // 숫자는 ASCII 만 (ffmpeg 출력은 항상 ASCII 이고, 파싱 실패로 panic 하지 않게)
    let overall = re.overall_kbps.captures(text).and_then(|c| c[1].parse::<i64>().ok());
    for line in text.lines() {
        // 표지 이미지(attached pic)도 FFmpeg 는 비디오 스트림으로 보고하지만 실제 영상이 아니다.
        if line.contains("(attached pic)") { continue; }
        let Some(m) = re.video.captures(line) else { continue };
        let rest = &line[m.get(0).unwrap().end()..];
        let Some(size) = re.size.captures(rest) else { continue };
        let stream_kbps = re.stream_kbps.captures(rest).and_then(|c| c[1].parse::<i64>().ok());
        let color_tagged = re.color_tags.captures(rest).is_some_and(|c| re.color_matrix.is_match(&c[1]));
        return Some(VideoInfo {
            codec: m[1].to_string(),
            width: size[1].parse().unwrap_or(0),
            height: size[2].parse().unwrap_or(0),
            fps: re.fps.captures(line).and_then(|c| c[1].parse::<f64>().ok()),
            pix_fmt: re.pix_fmt.captures(rest).map_or(String::new(), |c| c[1].to_string()),
            bitrate_kbps: stream_kbps.or(overall),
            color_tagged,
        });
    }
    None
}

// ---------------------------------------------------------------- 크기 / 비트레이트
/// 비율을 유지하며 `(max_w, max_h)` 안으로 축소(확대 없음). 인코더를 위해 짝수로 맞추고 최소 2.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = 1.0f64.min(max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = (w as f64 * scale) as u32; // `as` 는 0 방향 절단
    let nh = (h as f64 * scale) as u32;
    ((nw - nw % 2).max(2), (nh - nh % 2).max(2))
}

/// 출력 해상도에 비례한 비트레이트 상한(kb/s). 1080p 기준값의 0.3배 ~ 3.75배로 제한.
pub fn max_bitrate_kbps(codec: Codec, w: u32, h: u32) -> i64 {
    let base = codec_spec(codec).base_kbps;
    // 연산 순서: (정수 곱) / (정수) → 실수 나눗셈
    let scaled = (base * (w as i64 * h as i64)) as f64 / (1920i64 * 1080) as f64;
    let b = base as f64;
    (b * 3.75).min((b * 0.3).max(scaled)) as i64
}

/// 변환이 필요하면 이유(`Some`), 아니면 `None`.
/// 코덱이 설정과 다르면 `optimize` 와 무관하게 재인코딩한다. 크기/fps/비트 심도/비트레이트 검사는 `optimize` 일 때만.
pub fn needs_conversion(
    path: &str,
    info: Option<&VideoInfo>,
    max_w: u32,
    max_h: u32,
    optimize: bool,
    codec: Codec,
) -> Option<String> {
    if is_animated_image(path) {
        return Some("Animated images must be converted to MP4 to keep CPU load low".to_string());
    }
    let info = info?;
    let spec = codec_spec(codec);
    if info.codec != codec.as_str() {
        return Some(format!("Codec {} -> {}", info.codec, spec.label));
    }
    if optimize {
        if info.width as f64 > max_w as f64 * 1.05 || info.height as f64 > max_h as f64 * 1.05 {
            return Some(format!("Source {}x{} is larger than the screen ({}x{})", info.width, info.height, max_w, max_h));
        }
        if let Some(fps) = info.fps {
            if fps > MAX_FPS as f64 + 0.5 {
                return Some(format!("{}fps -> {}fps", (fps * 100.0).round() / 100.0, MAX_FPS));
            }
        }
        if ["10le", "10be", "12le", "12be", "16le", "16be"].iter().any(|s| info.pix_fmt.contains(s)) {
            return Some(format!("{} -> 8-bit", info.pix_fmt));
        }
        if let Some(b) = info.bitrate_kbps.filter(|&b| b != 0) {
            let (w, h) = fit_size(info.width, info.height, max_w, max_h);
            let cap = max_bitrate_kbps(codec, w, h);
            if b as f64 > cap as f64 * 1.5 {
                return Some(format!("Bitrate {} kb/s exceeds target {} kb/s", b, cap));
            }
        }
    }
    None
}

// ---------------------------------------------------------------- cache 이름
/// cache_key 의 순수 부분. `abs_src` 는 절대 경로 문자열.
/// 형식: `sha1("{abspath}|{size}|{mtime_ns}|{max_w}x{max_h}|{optimize 0/1}|{codec}")` 의 hex 앞 16자.
pub fn cache_key_from_parts(
    abs_src: &str,
    size: u64,
    mtime_ns: i128,
    max_w: u32,
    max_h: u32,
    optimize: bool,
    codec: Codec,
) -> String {
    let key = format!("{}|{}|{}|{}x{}|{}|{}", abs_src, size, mtime_ns, max_w, max_h, optimize as u8, codec.as_str());
    let digest = Sha1::digest(key.as_bytes());
    digest.iter().take(8).map(|b| format!("{:02x}", b)).collect() // 8바이트 = hex 16자
}

fn mtime_ns(t: std::time::SystemTime) -> i128 {
    use std::time::UNIX_EPOCH;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

/// 원본 경로·크기·수정시각·화면 크기·최적화 여부·코덱이 같으면 같은 이름. 파일이 없으면 `Err`.
pub fn cache_key(src: &str, max_w: u32, max_h: u32, optimize: bool, codec: Codec) -> io::Result<String> {
    let md = std::fs::metadata(src)?;
    let abs = abspath_key(src);
    Ok(cache_key_from_parts(&abs, md.len(), mtime_ns(md.modified()?), max_w, max_h, optimize, codec))
}

/// `path` 가 cache 폴더 **바로 아래**의 파일인가 (이미 사본이면 다시 복사하지 않기 위해).
pub fn in_cache(path: &str, directory: &Path) -> bool {
    let (p, d) = (norm_key(path), norm_key(&directory.to_string_lossy()));
    Path::new(&p).parent() == Some(Path::new(&d))
}

/// 영상과 같은 폴더의 첫 프레임 PNG 경로: `{stem}.frame2-{fit}-{w}x{h}.png`.
/// 화면 크기와 맞춤 모드가 이름에 들어가므로 바뀌면 새로 만든다. 폴더 부분은 입력 그대로 둔다.
pub fn frame_path(video: &str, w: u32, h: u32, fit: FitMode) -> String {
    let name_len = Path::new(video).file_name().map_or(0, |n| n.len());
    let dir = &video[..video.len() - name_len];
    format!("{}{}{}{}-{}x{}.png", dir, file_stem(video), FRAME_INFIX, fit.as_str(), w, h)
}

// ---------------------------------------------------------------- 첫 프레임 / 인코딩 명령
/// YUV → RGB 변환 규칙: `"auto" | "bt709" | "bt601"`.
/// 영상에 규칙이 표시돼 있으면 ffmpeg 가 그대로 따르므로 auto. 표시가 없는 YUV 영상은 mpv 처럼 해상도로 추측한다.
/// Uncertain: mpv 의 정확한 경계값(가로 1280 초과 또는 세로 576 초과 → BT.709)은 mpv 소스를 확인하지 못했다.
pub fn guess_matrix(info: Option<&VideoInfo>) -> &'static str {
    let Some(info) = info else { return "auto" };
    if info.color_tagged || !["yuv", "nv", "p0"].iter().any(|p| info.pix_fmt.starts_with(p)) {
        return "auto";
    }
    if info.width > 1280 || info.height > 576 { "bt709" } else { "bt601" }
}

/// mpv 의 맞춤 모드와 같은 결과가 되도록 화면 크기(w x h) 이미지를 만드는 ffmpeg 필터.
/// 맨 앞에서 먼저 RGB 로 바꾼다 (크기 조절·pad 를 YUV 로 한 뒤 마지막에 RGB 로 바꾸면 항상 BT.601 이 쓰이기 때문).
pub fn frame_filter(w: u32, h: u32, fit: FitMode, matrix: &str) -> String {
    let head = format!("scale=in_color_matrix={},format=rgb24,", matrix);
    match fit {
        FitMode::Cover => format!("{}scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}", head, w, h, w, h),
        FitMode::Stretch => format!("{}scale={}:{}", head, w, h),
        FitMode::Contain => format!(
            "{}scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:black",
            head, w, h, w, h
        ),
    }
}

pub fn build_frame_command(ffmpeg: &str, src: &str, dst: &str, w: u32, h: u32, fit: FitMode, matrix: &str) -> Vec<String> {
    let mut v: Vec<String> = ["-y", "-hide_banner", "-loglevel", "error", "-i"].iter().map(|s| s.to_string()).collect();
    v.insert(0, ffmpeg.to_string());
    v.extend([src.to_string(), "-map".into(), "0:V:0".into(), "-vf".into(), frame_filter(w, h, fit, matrix), "-frames:v".into(), "1".into(), dst.to_string()]);
    v
}

/// ffmpeg 인코딩 명령. `limit=true` 면 화면 크기로 축소하고 fps 를 `MAX_FPS` 로 제한한다 (false 면 원본 크기/fps 유지).
pub fn build_command(
    ffmpeg: &str,
    src: &str,
    dst: &str,
    info: Option<&VideoInfo>,
    max_w: u32,
    max_h: u32,
    codec: Codec,
    limit: bool,
) -> Vec<String> {
    let spec = codec_spec(codec);
    let mut filters: Vec<String> = Vec::new();
    if limit {
        if let Some(fps) = info.and_then(|i| i.fps) {
            if fps > MAX_FPS as f64 + 0.5 {
                filters.push(format!("fps={}", MAX_FPS));
            }
        }
    }
    let (w, h);
    match (info, limit) {
        (Some(i), true) => {
            let (fw, fh) = fit_size(i.width, i.height, max_w, max_h);
            filters.push(format!("scale={}:{}", fw, fh));
            (w, h) = (fw, fh);
        }
        _ => {
            (w, h) = info.map_or((max_w, max_h), |i| (i.width, i.height));
            filters.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string()); // 인코더는 짝수 크기만 받는다
        }
    }
    let cap = max_bitrate_kbps(codec, w, h);
    let mut cmd: Vec<String> = vec![
        ffmpeg.into(), "-y".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(),
        "-i".into(), src.into(), "-map".into(), "0:V:0".into(), "-an".into(), "-sn".into(), "-dn".into(),
        "-vf".into(), filters.join(","),
        "-c:v".into(), spec.encoder.into(), "-preset".into(), "medium".into(),
        "-crf".into(), spec.crf.to_string(),
        "-maxrate".into(), format!("{}k", cap), "-bufsize".into(), format!("{}k", cap * 2),
        "-profile:v".into(), spec.profile.into(), "-pix_fmt".into(), "yuv420p".into(),
    ];
    if codec == Codec::Hevc {
        // hvc1: MP4 호환성, x265 로그 억제
        cmd.extend(["-tag:v".into(), "hvc1".into(), "-x265-params".into(), "log-level=error".into()]);
    }
    cmd.extend(["-movflags".into(), "+faststart".into(), dst.into()]);
    cmd
}

// ================================================================ 테스트
#[cfg(test)]
mod tests {
    use super::*;

    fn vi(codec: &str, w: u32, h: u32, fps: Option<f64>, pix: &str, kbps: Option<i64>, tagged: bool) -> VideoInfo {
        VideoInfo { codec: codec.into(), width: w, height: h, fps, pix_fmt: pix.into(), bitrate_kbps: kbps, color_tagged: tagged }
    }

    // ---------------------------------------------------------------- fit_size
    #[test]
    fn fit_size_golden() {
        assert_eq!(fit_size(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_size(1921, 1081, 1920, 1080), (1918, 1080));
        assert_eq!(fit_size(1001, 501, 1920, 1080), (1000, 500));
        assert_eq!(fit_size(1, 1, 1920, 1080), (2, 2));
        assert_eq!(fit_size(1080, 1920, 1920, 1080), (606, 1080));
    }

    #[test]
    fn fit_size_does_not_panic_on_zero() {
        // 0 으로 나누지 않고 최소값 2 를 돌려준다.
        assert_eq!(fit_size(0, 0, 1920, 1080), (2, 2));
        assert_eq!(fit_size(100, 100, 0, 0), (2, 2));
    }

    // ---------------------------------------------------------------- ffmpeg 출력 파싱: 표지 이미지 제외
    #[test]
    fn ffmpeg_parser_skips_attached_cover_art_and_uses_real_video_stream() {
        let text = concat!(
            "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'sample.mp4':\n",
            "  Stream #0:0: Video: mjpeg, yuvj420p, 600x600, 90k tbr (attached pic)\n",
            "  Stream #0:1: Video: hevc, yuv420p, 1920x1080, 30 fps, 5000 kb/s\n",
        );
        let info = parse_ffmpeg_info(text).expect("main video stream should be parsed");
        assert_eq!(info.codec, "hevc");
        assert_eq!((info.width, info.height), (1920, 1080));
    }

    #[test]
    fn max_bitrate_golden() {
        let sizes = [(1920, 1080), (3840, 2160), (640, 360), (7680, 4320), (2560, 1440)];
        let h264 = [8000, 30000, 2400, 30000, 14222];
        let hevc = [5000, 18750, 1500, 18750, 8888];
        for (i, &(w, h)) in sizes.iter().enumerate() {
            assert_eq!(max_bitrate_kbps(Codec::H264, w, h), h264[i], "h264 {}x{}", w, h);
            assert_eq!(max_bitrate_kbps(Codec::Hevc, w, h), hevc[i], "hevc {}x{}", w, h);
        }
    }

    // ---------------------------------------------------------------- ffmpeg 출력 파싱
    #[test]
    fn parse_ffmpeg_info_input1_h264_tagged() {
        let t = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':\n  Duration: 00:00:10.00, start: 0.000000, bitrate: 5000 kb/s\n  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), 1920x1080 [SAR 1:1 DAR 16:9], 4900 kb/s, 60 fps, 60 tbr, 15360 tbn (default)\n";
        assert_eq!(parse_ffmpeg_info(t), Some(vi("h264", 1920, 1080, Some(60.0), "yuv420p", Some(4900), true)));
    }

    #[test]
    fn parse_ffmpeg_info_input2_gif_overall_bitrate_na() {
        let t = "  Duration: 00:00:03.00, start: 0.000000, bitrate: N/A\n  Stream #0:0: Video: gif, bgra, 480x270, 12.5 fps, 12.5 tbr, 100 tbn\n";
        assert_eq!(parse_ffmpeg_info(t), Some(vi("gif", 480, 270, Some(12.5), "bgra", None, false)));
    }

    #[test]
    fn parse_ffmpeg_info_input3_hevc_10bit() {
        let t = "  Duration: 00:00:05.00, start: 0.000000, bitrate: 20000 kb/s\n  Stream #0:0[0x1](und): Video: hevc (Main 10) (hvc1 / 0x31637668), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), 3840x2160, 19800 kb/s, 29.97 fps, 29.97 tbr, 30k tbn (default)\n";
        assert_eq!(parse_ffmpeg_info(t), Some(vi("hevc", 3840, 2160, Some(29.97), "yuv420p10le", Some(19800), true)));
    }

    #[test]
    fn parse_ffmpeg_info_input4_garbage_is_none() {
        assert_eq!(parse_ffmpeg_info("garbage text without video"), None);
        assert_eq!(parse_ffmpeg_info(""), None);
    }

    #[test]
    fn parse_ffmpeg_info_falls_back_to_overall_bitrate_and_handles_crlf() {
        let t = "  Duration: 00:00:05.00, start: 0.0, bitrate: 1234 kb/s\r\n  Stream #0:0: Video: mpeg4, yuv420p, 640x480 [SAR 1:1 DAR 4:3], 25 fps, 25 tbr\r\n";
        let i = parse_ffmpeg_info(t).unwrap();
        assert_eq!((i.codec.as_str(), i.width, i.height, i.bitrate_kbps), ("mpeg4", 640, 480, Some(1234)));
        assert!(!i.color_tagged);
    }

    #[test]
    fn parse_ffmpeg_info_skips_video_line_without_size() {
        // 첫 "Video:" 줄에 크기가 없으면(예: 썸네일 mjpeg 아님, 가짜 줄) 다음 줄로 넘어간다
        let t = "  Video: none\n  Stream #0:1: Video: h264, yuv420p, 1280x720, 25 fps\n";
        assert_eq!(parse_ffmpeg_info(t).unwrap().width, 1280);
    }

    #[test]
    fn parse_ffmpeg_info_size_must_not_be_glued_to_word_chars() {
        // (?<![\w]) 와 같은 뜻: 앞이 단어 문자면 크기로 보지 않는다
        let t = "  Stream #0:0: Video: h264, yuv420p, abc1920x1080, 25 fps\n";
        assert_eq!(parse_ffmpeg_info(t), None);
    }

    // ---------------------------------------------------------------- needs_conversion
    #[test]
    fn needs_conversion_golden() {
        let msg = |p: &str, i: Option<&VideoInfo>, opt: bool, c: Codec| needs_conversion(p, i, 1920, 1080, opt, c);
        let base = |w, h, fps, pix: &str, kb| vi("h264", w, h, Some(fps), pix, Some(kb), false);

        let gif = vi("gif", 480, 270, Some(12.5), "bgra", None, false);
        assert_eq!(msg("a.gif", Some(&gif), true, Codec::H264).as_deref(), Some("Animated images must be converted to MP4 to keep CPU load low"));
        assert_eq!(msg("a.mp4", None, true, Codec::H264).as_deref(), None);
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 30.0, "yuv420p", 4000)), true, Codec::H264).as_deref(), None);

        let hevc = vi("hevc", 1920, 1080, Some(30.0), "yuv420p", Some(4000), false);
        assert_eq!(msg("a.mp4", Some(&hevc), true, Codec::H264).as_deref(), Some("Codec hevc -> H.264 (AVC)"));
        // 코덱 불일치는 optimize 와 무관
        assert_eq!(msg("a.mp4", Some(&hevc), false, Codec::H264).as_deref(), Some("Codec hevc -> H.264 (AVC)"));
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 30.0, "yuv420p", 4000)), true, Codec::Hevc).as_deref(), Some("Codec h264 -> H.265 (HEVC)"));

        assert_eq!(
            msg("a.mp4", Some(&base(3840, 2160, 30.0, "yuv420p", 4000)), true, Codec::H264).as_deref(),
            Some("Source 3840x2160 is larger than the screen (1920x1080)")
        );
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 60.0, "yuv420p", 4000)), true, Codec::H264).as_deref(), Some("60fps -> 30fps"));
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 30.5, "yuv420p", 4000)), true, Codec::H264).as_deref(), None); // 경계: > 30.5 만
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 30.6, "yuv420p", 4000)), true, Codec::H264).as_deref(), Some("30.6fps -> 30fps"));
        assert_eq!(msg("a.mp4", Some(&base(1920, 1080, 30.0, "yuv420p10le", 4000)), true, Codec::H264).as_deref(), Some("yuv420p10le -> 8-bit"));

        let hi = base(1920, 1080, 30.0, "yuv420p", 20000);
        assert_eq!(msg("a.mp4", Some(&hi), true, Codec::H264).as_deref(), Some("Bitrate 20000 kb/s exceeds target 8000 kb/s"));
        assert_eq!(msg("a.mp4", Some(&hi), false, Codec::H264).as_deref(), None);

        // 1.05 배 경계
        assert_eq!(msg("a.mp4", Some(&base(2016, 1134, 30.0, "yuv420p", 4000)), true, Codec::H264).as_deref(), None);
        assert_eq!(
            msg("a.mp4", Some(&base(2017, 1134, 30.0, "yuv420p", 4000)), true, Codec::H264).as_deref(),
            Some("Source 2017x1134 is larger than the screen (1920x1080)")
        );
    }

    #[test]
    fn needs_conversion_animated_extension_is_case_insensitive_and_path_aware() {
        assert!(is_animated_image(r"C:\x\A.GIF"));
        assert!(is_animated_image("dir/a.WebP"));
        assert!(is_animated_image("a.apng"));
        assert!(!is_animated_image("a.mp4"));
        assert!(!is_animated_image(r"C:\dir.gif\a.mp4")); // 폴더 이름의 .gif 는 무시
        assert!(!is_animated_image(".gif")); // 점으로 시작하는 이름은 확장자가 아님
    }

    // ---------------------------------------------------------------- 색 변환 행렬 추측
    #[test]
    fn guess_matrix_golden() {
        assert_eq!(guess_matrix(None), "auto");
        assert_eq!(guess_matrix(Some(&vi("h264", 1920, 1080, None, "yuv420p", None, true))), "auto");
        assert_eq!(guess_matrix(Some(&vi("h264", 1920, 1080, None, "yuv420p", None, false))), "bt709");
        assert_eq!(guess_matrix(Some(&vi("h264", 1280, 720, None, "yuv420p", None, false))), "bt709");
        assert_eq!(guess_matrix(Some(&vi("h264", 1281, 576, None, "yuv420p", None, false))), "bt709");
        assert_eq!(guess_matrix(Some(&vi("h264", 640, 577, None, "yuv420p", None, false))), "bt709");
        assert_eq!(guess_matrix(Some(&vi("h264", 640, 480, None, "yuv420p", None, false))), "bt601");
        assert_eq!(guess_matrix(Some(&vi("gif", 480, 270, None, "bgra", None, false))), "auto");
        assert_eq!(guess_matrix(Some(&vi("h264", 640, 480, None, "nv12", None, false))), "bt601");
        assert_eq!(guess_matrix(Some(&vi("h264", 640, 480, None, "", None, false))), "auto");
    }

    // ---------------------------------------------------------------- 프레임 필터
    #[test]
    fn frame_filter_golden() {
        assert_eq!(
            frame_filter(1920, 1080, FitMode::Contain, "bt709"),
            "scale=in_color_matrix=bt709,format=rgb24,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black"
        );
        assert_eq!(
            frame_filter(1920, 1080, FitMode::Cover, "bt709"),
            "scale=in_color_matrix=bt709,format=rgb24,scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080"
        );
        assert_eq!(frame_filter(1920, 1080, FitMode::Stretch, "bt709"), "scale=in_color_matrix=bt709,format=rgb24,scale=1920:1080");
        assert_eq!(
            frame_filter(1920, 1080, FitMode::Contain, "auto"),
            "scale=in_color_matrix=auto,format=rgb24,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black"
        );
    }

    // ---------------------------------------------------------------- 인코딩 명령
    fn cmd(vf: &str, enc: &str, crf: &str, cap: i64, profile: &str, hevc: bool) -> Vec<String> {
        let mut v: Vec<String> = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", "in.mp4", "-map", "0:V:0", "-an", "-sn", "-dn", "-vf"]
            .iter().map(|s| s.to_string()).collect();
        v.extend([vf, "-c:v", enc, "-preset", "medium", "-crf", crf].iter().map(|s| s.to_string()));
        v.extend(["-maxrate".to_string(), format!("{}k", cap), "-bufsize".to_string(), format!("{}k", cap * 2)]);
        v.extend(["-profile:v", profile, "-pix_fmt", "yuv420p"].iter().map(|s| s.to_string()));
        if hevc {
            v.extend(["-tag:v", "hvc1", "-x265-params", "log-level=error"].iter().map(|s| s.to_string()));
        }
        v.extend(["-movflags", "+faststart", "out.mp4"].iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn build_command_golden() {
        let uhd60 = vi("h264", 3840, 2160, Some(60.0), "yuv420p", None, false);
        let build = |i: Option<&VideoInfo>, c: Codec, limit: bool| build_command("ffmpeg", "in.mp4", "out.mp4", i, 1920, 1080, c, limit);

        assert_eq!(build(Some(&uhd60), Codec::H264, true), cmd("fps=30,scale=1920:1080", "libx264", "22", 8000, "high", false));
        assert_eq!(build(Some(&uhd60), Codec::Hevc, true), cmd("fps=30,scale=1920:1080", "libx265", "26", 5000, "main", true));
        // limit=false: 원본 크기(3840x2160) 기준 상한
        assert_eq!(
            build(Some(&uhd60), Codec::H264, false),
            cmd("scale=trunc(iw/2)*2:trunc(ih/2)*2", "libx264", "22", 30000, "high", false)
        );
        // info 없음(gif 등): 화면 크기 기준
        assert_eq!(build(None, Codec::H264, true), cmd("scale=trunc(iw/2)*2:trunc(ih/2)*2", "libx264", "22", 8000, "high", false));
        let hd24 = vi("h264", 1280, 720, Some(24.0), "yuv420p", None, false);
        assert_eq!(build(Some(&hd24), Codec::H264, true), cmd("scale=1280:720", "libx264", "22", 3555, "high", false));
    }

    // ---------------------------------------------------------------- 첫 프레임 명령
    #[test]
    fn build_frame_command_golden() {
        let got = build_frame_command("ffmpeg", "v.mp4", "f.png", 1920, 1080, FitMode::Contain, "bt709");
        let want: Vec<String> = [
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", "v.mp4", "-map", "0:V:0", "-vf",
            "scale=in_color_matrix=bt709,format=rgb24,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black",
            "-frames:v", "1", "f.png",
        ].iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn frame_path_golden() {
        #[cfg(windows)]
        assert_eq!(frame_path(r"C:\app\cache\abcd.mp4", 1920, 1080, FitMode::Cover), r"C:\app\cache\abcd.frame2-cover-1920x1080.png");
        assert_eq!(frame_path("/a/b/c.d.mp4", 800, 600, FitMode::Contain), "/a/b/c.d.frame2-contain-800x600.png");
        assert_eq!(frame_path("noext", 1, 2, FitMode::Stretch), "noext.frame2-stretch-1x2.png");
        assert_eq!(frame_path("x.mp4", 1, 2, FitMode::Stretch), "x.frame2-stretch-1x2.png");
    }

    #[test]
    fn cache_key_golden() {
        assert_eq!(cache_key_from_parts(r"C:\v\a.mp4", 123, 456, 1920, 1080, true, Codec::H264), "4515167daf30efce");
    }

    #[test]
    fn cache_key_changes_with_each_input() {
        let k = |a: &str, s, m, w, h, o, c: Codec| cache_key_from_parts(a, s, m, w, h, o, c);
        let base = k("/v/a.mp4", 1, 2, 1920, 1080, true, Codec::H264);
        assert_eq!(base.len(), 16);
        assert!(base.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        for other in [
            k("/v/b.mp4", 1, 2, 1920, 1080, true, Codec::H264),
            k("/v/a.mp4", 9, 2, 1920, 1080, true, Codec::H264),
            k("/v/a.mp4", 1, 9, 1920, 1080, true, Codec::H264),
            k("/v/a.mp4", 1, 2, 1921, 1080, true, Codec::H264),
            k("/v/a.mp4", 1, 2, 1920, 1081, true, Codec::H264),
            k("/v/a.mp4", 1, 2, 1920, 1080, false, Codec::H264),
            k("/v/a.mp4", 1, 2, 1920, 1080, true, Codec::Hevc),
        ] {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn cache_key_reads_real_file_and_reacts_to_content_change() {
        let dir = crate::test_util::unique_dir("media");
        let f = dir.join("a.mp4");
        std::fs::write(&f, b"one").unwrap();
        let s = f.to_string_lossy().into_owned();
        let k1 = cache_key(&s, 1920, 1080, true, Codec::H264).unwrap();
        assert_eq!(k1, cache_key(&s, 1920, 1080, true, Codec::H264).unwrap(), "같은 파일은 같은 키");
        std::fs::write(&f, b"three").unwrap(); // 크기가 바뀜
        assert_ne!(k1, cache_key(&s, 1920, 1080, true, Codec::H264).unwrap());
        assert!(cache_key(&dir.join("missing.mp4").to_string_lossy(), 1, 1, true, Codec::H264).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- in_cache
    #[cfg(windows)]
    #[test]
    fn in_cache_windows_rules() {
        let c = Path::new(r"C:\App\cache");
        assert!(in_cache(r"C:\App\cache\abcd.mp4", c));
        assert!(in_cache(r"c:/app/CACHE/abcd.mp4", c)); // 대소문자·구분자 무시
        assert!(in_cache(r"C:\App\cache\..\cache\abcd.mp4", c));
        assert!(!in_cache(r"C:\App\cache\sub\abcd.mp4", c)); // 하위 폴더는 아님
        assert!(!in_cache(r"C:\App\cache2\abcd.mp4", c));
        assert!(!in_cache(r"C:\Videos\abcd.mp4", c));
    }

    // ---------------------------------------------------------------- 보조 함수
    #[test]
    fn codec_helpers() {
        assert_eq!(codec_spec(Codec::H264).label, "H.264 (AVC)");
        assert_eq!(codec_spec(Codec::Hevc).encoder, "libx265");
    }

    #[test]
    fn suffix_and_stem() {
        assert_eq!(suffix_lower("a.MP4"), ".mp4");
        assert_eq!(suffix_lower("a.tar.gz"), ".gz");
        assert_eq!(suffix_lower("a."), "");
        assert_eq!(suffix_lower("noext"), "");
        assert_eq!(file_stem("a.tar.gz"), "a.tar");
        assert_eq!(file_stem(".hidden"), ".hidden");
        #[cfg(windows)]
        assert_eq!(file_stem(r"C:\x\y.mp4"), "y");
    }
}
