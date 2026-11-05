//! 영상 준비: ffmpeg 변환 / 첫 프레임 / cache 관리.
//!
//! - `pure`: 파일·프로세스 없이 테스트되는 순수 로직
//! - 이 파일: ffmpeg 호출, `prepare`, `ensure_first_frame`, `prune_cache`
//!
//! 테스트하기 쉽도록 ffmpeg 경로를 인자로 받는 `*_with` 버전을 따로 두고,
//! 인자 없는 공개 함수(`prepare`, `ensure_first_frame`)는 `find_ffmpeg()` 결과를 넘겨 부른다.

pub mod pure;

use crate::sync::LockExt;
use crate::config::{self, norm_key, Codec, FitMode, Settings};
use pure::*;
use regex::Regex;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------- 타입
/// 사용자에게 보여 줄 수 있는 영어 문구를 담은 오류.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaError(pub String);

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for MediaError {}

#[derive(Debug, Clone, PartialEq)]
pub struct PrepareResult {
    pub path: String,
    pub note: Option<String>,
}

/// cache 폴더: 사용자 데이터 폴더 아래 `cache`.
pub fn cache_dir() -> PathBuf {
    config::data_dir().join("cache")
}

// ---------------------------------------------------------------- ffmpeg 찾기
fn ffmpeg_file_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(md) = std::fs::metadata(p) else { return false };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return md.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    true
}

/// `<리소스 폴더>/libs/ffmpeg(.exe)` 를 먼저, 없으면 `path_var` 의 각 폴더에서 찾는다.
fn find_ffmpeg_in_roots(roots: &[PathBuf], path_var: Option<&std::ffi::OsStr>) -> Option<String> {
    let name = ffmpeg_file_name();
    let bundled = roots.iter().map(|root| root.join("libs").join(name));
    let on_path = path_var.into_iter().flat_map(|v| std::env::split_paths(v)).map(|dir| dir.join(name));
    bundled.chain(on_path).find(|candidate| is_executable_file(candidate)).map(|p| p.to_string_lossy().into_owned())
}

pub fn find_ffmpeg() -> Option<String> {
    let roots = config::resource_dirs();
    let path_var = std::env::var_os("PATH");
    find_ffmpeg_in_roots(&roots, path_var.as_deref())
}

// ---------------------------------------------------------------- 하위 프로세스
#[derive(Debug)]
pub(crate) struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 창 없이 실행하고 stdout/stderr 를 모은다. 두 파이프를 **별도 스레드**로 읽어 파이프가 가득 차 멈추는 교착을 피한다.
/// `timeout` 이 지나면 프로세스를 죽이고 `ErrorKind::TimedOut`.
pub(crate) fn run_capture(cmd: &[String], timeout: Option<Duration>) -> io::Result<Captured> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = c.spawn()?;
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let status = match timeout {
        None => child.wait()?,
        Some(limit) => {
            let start = Instant::now();
            loop {
                if let Some(s) = child.try_wait()? {
                    break s;
                }
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    // 읽기 스레드는 기다리지 않는다 (손자 프로세스가 파이프를 쥐고 있어도 멈추지 않도록).
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "process timed out"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    Ok(Captured { code: status.code(), stdout, stderr })
}

/// `ffmpeg -hide_banner -i path` (타임아웃 30초). 출력 파일이 없어 returncode 1 이 정상이므로 종료 코드는 보지 않는다.
pub fn probe(path: &str, ffmpeg: &str) -> Option<VideoInfo> {
    let cmd = [ffmpeg.to_string(), "-hide_banner".into(), "-i".into(), path.to_string()];
    let r = run_capture(&cmd, Some(Duration::from_secs(30))).ok()?;
    parse_ffmpeg_info(&r.stderr)
}

/// 이 ffmpeg 빌드에 해당 인코더(libx264/libx265)가 있는가. 확인 못 하면 일단 있다고 보고 시도한다. 결과는 캐시.
pub fn has_encoder(ffmpeg: &str, name: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (ffmpeg.to_string(), name.to_string());
    if let Some(&v) = cache.guard().get(&key) {
        return v;
    }
    let cmd = [ffmpeg.to_string(), "-hide_banner".into(), "-encoders".into()];
    let v = match run_capture(&cmd, Some(Duration::from_secs(30))) {
        Err(_) => true,
        // A non-zero exit means the encoder list could not be trusted; let the
        // actual conversion return its concrete diagnostic instead of inventing
        // an "encoder missing" error from an inconclusive probe.
        Ok(r) if r.code != Some(0) => true,
        Ok(r) => Regex::new(&format!(r"\b{}\b", regex::escape(name))).map_or(true, |re| re.is_match(&r.stdout)),
    };
    cache.guard().insert(key, v);
    v
}

// ---------------------------------------------------------------- prepare
fn io_err(e: io::Error) -> MediaError {
    MediaError(format!("Failed to prepare the video: {}", e))
}

fn non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// 선택한 파일이 존재하는 일반 파일이고 비어 있지 않은지 확인한다 (cache/플레이어에 들어가기 전).
pub fn validate_media_file(src: &str) -> Result<(), MediaError> {
    let metadata = std::fs::metadata(src).map_err(io_err)?;
    if !metadata.is_file() {
        return Err(MediaError("The selected path is not a regular file.".into()));
    }
    if metadata.len() == 0 {
        return Err(MediaError("The selected media file is empty (0 bytes).".into()));
    }
    Ok(())
}

/// 선택/적용한 파일을 재생할 수 있게 준비한다 (작업 스레드에서 호출).
/// 이미 cache 안의 사본이면 그대로 쓰고, 아니면 `prepare` 로 변환/복사한다.
/// 정적 전환이 켜져 있으면 첫 프레임도 미리 만들어 둔다 (실패해도 무시: 부가 기능이다).
pub fn prepare_for_playback(path: &str, settings: &Settings, (width, height): (u32, u32)) -> Result<PrepareResult, MediaError> {
    validate_media_file(path)?;
    let cache = cache_dir();
    let result = if in_cache(path, &cache) {
        PrepareResult { path: path.to_string(), note: Some("Using saved cache copy".into()) }
    } else {
        prepare(path, width, height, settings.optimize, settings.video_codec, &cache)?
    };
    if settings.static_snapshot {
        let _ = ensure_first_frame(&result.path, width, height, settings.fit_mode);
    }
    Ok(result)
}

/// 재생할 파일 경로를 돌려준다. 항상 `directory`(cache) 안의 사본이며 원본 경로는 돌려주지 않는다.
/// 같은 조건의 사본이 있으면 그대로 쓰고, 없으면 변환(또는 복사)한다.
/// 사본은 `.part` 이름으로 쓴 뒤 이름을 바꿔 확정하므로 중간에 끊겨도 반쪽 파일이 재생되지 않는다.
pub fn prepare(src: &str, max_w: u32, max_h: u32, optimize: bool, codec: Codec, directory: &Path) -> Result<PrepareResult, MediaError> {
    prepare_with(find_ffmpeg().as_deref(), src, max_w, max_h, optimize, codec, directory)
}

/// cache 에 이미 있는 사본(변환본 → 복사본 순)을 찾는다. 비었거나 읽을 수 없는 항목은 지우고 건너뛴다.
/// 변환본은 영상 스트림이 읽혀야 한다 (깨진 항목을 재시작마다 libmpv 에 넘기지 않기 위해).
/// 복사본은 FFmpeg 가 못 읽는 형식을 libmpv 는 열 수도 있으므로 검사하지 않는다.
fn find_cached(ffmpeg: Option<&str>, converted: &Path, copied: &Path) -> Option<PathBuf> {
    for hit in [converted, copied] {
        if !hit.is_file() {
            continue;
        }
        let readable = hit != converted || ffmpeg.map_or(true, |f| probe(&hit.to_string_lossy(), f).is_some());
        if non_empty_file(hit) && readable {
            return Some(hit.to_path_buf());
        }
        let _ = std::fs::remove_file(hit);
    }
    None
}

/// 변환에 쓸 코덱을 정한다. 요청한 인코더가 이 FFmpeg 에 없으면 (다른) H.264 로 대신하고 안내 문구를 함께 돌려준다.
fn choose_codec(ffmpeg: &str, codec: Codec) -> Result<(Codec, String), MediaError> {
    let spec = codec_spec(codec);
    if has_encoder(ffmpeg, spec.encoder) {
        return Ok((codec, String::new()));
    }
    // H.264 자체가 없는데 H.264 로 대신하면 나중에 알 수 없는 FFmpeg 오류가 나므로 여기서 바로 알린다.
    let default = codec_spec(Codec::H264);
    if codec != Codec::H264 && has_encoder(ffmpeg, default.encoder) {
        Ok((Codec::H264, format!(" · {} encoder unavailable, used {}", spec.label, default.label)))
    } else {
        Err(MediaError(format!(
            "This FFmpeg build does not include the required video encoder '{}'. Install an FFmpeg build with {} enabled, or choose a source video that does not need conversion.",
            spec.encoder, spec.label
        )))
    }
}

/// ffmpeg 로 `cmd` 를 실행해 `tmp` 를 만들고, 읽을 수 있는 영상인지 확인한다.
fn convert(ffmpeg: &str, cmd: &[String], tmp: &Path) -> Result<(), MediaError> {
    // 큰 영상도 충분한 30분. 멈춘 자식 프로세스에서는 복구할 수 있어야 한다.
    let r = run_capture(cmd, Some(Duration::from_secs(30 * 60))).map_err(|e| {
        if e.kind() == io::ErrorKind::TimedOut {
            MediaError("Video conversion timed out after 30 minutes. Try a shorter or smaller source video.".into())
        } else { io_err(e) }
    })?;
    if r.code != Some(0) || !tmp.exists() {
        let s = r.stderr.trim();
        let tail: String = s.chars().skip(s.chars().count().saturating_sub(500)).collect();
        let detail = if tail.is_empty() { r.code.map_or("None".to_string(), |c| c.to_string()) } else { tail };
        return Err(MediaError(format!("Conversion failed: {}", detail)));
    }
    let Some(info) = probe(&tmp.to_string_lossy(), ffmpeg) else {
        return Err(MediaError("FFmpeg reported success, but the converted file has no readable video stream. Check the source file and FFmpeg build.".into()));
    };
    if info.width == 0 || info.height == 0 {
        return Err(MediaError("FFmpeg produced a video with invalid dimensions.".into()));
    }
    Ok(())
}

pub fn prepare_with(
    ffmpeg: Option<&str>,
    src: &str,
    max_w: u32,
    max_h: u32,
    optimize: bool,
    codec: Codec,
    directory: &Path,
) -> Result<PrepareResult, MediaError> {
    validate_media_file(src)?;
    let animated = is_animated_image(src);
    if ffmpeg.is_none() && animated {
        return Err(MediaError(
            "ffmpeg is required to convert GIF/APNG/WebP. Place ffmpeg.exe in the libs folder or add it to PATH.".to_string(),
        ));
    }

    let ext = suffix_lower(src);
    // 원본 파일이 없으면 MediaError 로 통일한다.
    let key = cache_key(src, max_w, max_h, optimize, codec).map_err(io_err)?;
    let converted = directory.join(format!("{key}.mp4"));
    let copied = directory.join(format!("{key}{ext}"));
    if let Some(hit) = find_cached(ffmpeg, &converted, &copied) {
        return Ok(PrepareResult { path: hit.to_string_lossy().into_owned(), note: Some("Using cached copy".to_string()) });
    }

    // 변환이 필요한 이유 (ffmpeg 가 없으면 항상 None → 그대로 복사).
    let info = ffmpeg.and_then(|f| probe(src, f));
    let reason = ffmpeg.and_then(|_| needs_conversion(src, info.as_ref(), max_w, max_h, optimize, codec));
    let (used, fallback) = match (&reason, ffmpeg) {
        (Some(_), Some(f)) => choose_codec(f, codec)?,
        _ => (codec, String::new()),
    };

    let (dst, part_suffix) = if reason.is_none() { (copied, ext) } else { (converted, ".mp4".to_string()) };
    let tmp = directory.join(format!("{key}.part{part_suffix}"));
    let work = || -> Result<(), MediaError> {
        std::fs::create_dir_all(directory).map_err(io_err)?;
        match (&reason, ffmpeg) {
            (Some(_), Some(f)) => {
                let cmd = build_command(f, src, &tmp.to_string_lossy(), info.as_ref(), max_w, max_h, used, optimize || animated);
                convert(f, &cmd, &tmp)?;
            }
            _ => { std::fs::copy(src, &tmp).map_err(io_err)?; }
        }
        if !non_empty_file(&tmp) {
            return Err(MediaError("Prepared media file is empty or invalid.".into()));
        }
        std::fs::rename(&tmp, &dst).map_err(io_err)
    };
    if let Err(e) = work() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let note = if let Some(reason) = &reason {
        Some(format!("Converted to {} ({}){}", codec_spec(used).label, reason, fallback))
    } else if ffmpeg.is_none() {
        Some("ffmpeg not found: copied without resolution check/optimization".to_string())
    } else if info.is_none() {
        Some("FFmpeg could not identify a video stream; copied the original unchanged and asked libmpv to open it".to_string())
    } else {
        None
    };
    Ok(PrepareResult { path: dst.to_string_lossy().into_owned(), note })
}

// ---------------------------------------------------------------- 정적 전환용 첫 프레임
/// 영상의 첫 프레임을 화면 크기 PNG 로 만들어 경로를 돌려준다 (이미 있으면 그대로). 실패하면 `None`.
/// 실패해도 오류를 내지 않는다: 정적 전환은 부가 기능이라 재생 자체를 막으면 안 된다.
pub fn ensure_first_frame(video: &str, w: u32, h: u32, fit: FitMode) -> Option<String> {
    ensure_first_frame_with(find_ffmpeg().as_deref(), video, w, h, fit)
}

pub fn ensure_first_frame_with(ffmpeg: Option<&str>, video: &str, w: u32, h: u32, fit: FitMode) -> Option<String> {
    if video.is_empty() || !Path::new(video).is_file() || w == 0 || h == 0 {
        return None;
    }
    let dst = frame_path(video, w, h, fit);
    // 이미 있으면 그대로 쓴다 (ffmpeg 가 있으면 읽히는 PNG 인지도 확인). 깨졌으면 지우고 다시 만든다.
    if non_empty_file(Path::new(&dst)) && ffmpeg.map_or(true, |f| probe(&dst, f).is_some()) {
        return Some(dst);
    }
    let _ = std::fs::remove_file(&dst);
    let ffmpeg = ffmpeg?;
    let tmp = format!("{}.part.png", dst.strip_suffix(".png").unwrap_or(&dst));
    let matrix = guess_matrix(probe(video, ffmpeg).as_ref());
    let cmd = build_frame_command(ffmpeg, video, &tmp, w, h, fit, matrix);
    let ok = matches!(run_capture(&cmd, Some(Duration::from_secs(60))), Ok(r) if r.code == Some(0))
        && non_empty_file(Path::new(&tmp))
        && std::fs::rename(&tmp, &dst).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(dst)
}

// ---------------------------------------------------------------- cache 정리
/// cache 안에서 `keep` 에 없는 파일(오래된 사본, 중단된 `.part`)을 지운다.
/// 사용 중이라 지워지지 않는 파일은 건너뛰고 다음 정리 때 다시 시도한다.
/// cache 폴더 **직속의 일반 파일만** 대상이며 하위 폴더나 다른 경로는 건드리지 않는다.
pub fn prune_cache(keep: &[Option<String>], directory: &Path) {
    let keep: Vec<&String> = keep.iter().flatten().collect();
    let keep_set: Vec<String> = keep.iter().map(|p| norm_key(p)).collect();
    let frame_prefixes: Vec<String> = keep.iter().map(|p| format!("{}{}", file_stem(p), FRAME_INFIX)).collect(); // 유지하는 영상의 첫 프레임
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if !std::fs::metadata(&path).map_or(false, |m| m.is_file()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if keep_set.contains(&norm_key(&path.to_string_lossy())) {
            continue;
        }
        if name.ends_with(".png") && !name.contains(".part") && frame_prefixes.iter().any(|x| name.starts_with(x)) {
            continue;
        }
        let _ = std::fs::remove_file(&path); // 실패는 무시
    }
}

// ================================================================ 테스트
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::Tmp;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    // ---------------------------------------------------------------- source-file validation
    #[test]
    fn validate_media_file_accepts_non_empty_regular_file() {
        let t = Tmp::new("validate_ok");
        let src = t.file("movie.mp4", b"not-empty");
        assert!(validate_media_file(&src).is_ok());
    }

    #[test]
    fn validate_media_file_rejects_empty_file() {
        let t = Tmp::new("validate_empty");
        let src = t.file("empty.mp4", b"");
        let error = validate_media_file(&src).unwrap_err();
        assert!(error.0.contains("empty"), "{error}");
    }

    #[test]
    fn validate_media_file_rejects_missing_file() {
        let t = Tmp::new("validate_missing");
        let error = validate_media_file(&t.path("missing.mp4").to_string_lossy()).unwrap_err();
        assert!(error.0.starts_with("Failed to prepare the video:"), "{error}");
    }

    #[test]
    fn prepare_rejects_empty_source_before_creating_cache() {
        let t = Tmp::new("prepare_empty");
        let src = t.file("empty.mp4", b"");
        let cache = t.path("cache");
        assert!(prepare_with(None, &src, 1920, 1080, true, Codec::H264, &cache).is_err());
        assert!(!cache.exists());
    }

    // ---------------------------------------------------------------- prepare (ffmpeg 불필요)
    #[test]
    fn prepare_copies_without_ffmpeg_then_hits_cache() {
        let t = Tmp::new("copy");
        let src = t.file("Movie.MP4", b"fake video");
        let cache = t.path("cache");
        let r = prepare_with(None, &src, 1920, 1080, true, Codec::H264, &cache).unwrap();
        assert_eq!(r.note.as_deref(), Some("ffmpeg not found: copied without resolution check/optimization"));
        assert!(in_cache(&r.path, &cache));
        assert!(r.path.ends_with(".mp4"), "확장자는 소문자: {}", r.path);
        assert_eq!(std::fs::read(&r.path).unwrap(), b"fake video");
        assert_eq!(names(&cache).len(), 1, ".part 가 남지 않는다");
        let r2 = prepare_with(None, &src, 1920, 1080, true, Codec::H264, &cache).unwrap();
        assert_eq!((r2.path.as_str(), r2.note.as_deref()), (r.path.as_str(), Some("Using cached copy")));
    }

    #[test]
    fn prepare_cache_key_depends_on_settings() {
        let t = Tmp::new("keys");
        let src = t.file("a.mp4", b"x");
        let cache = t.path("cache");
        let a = prepare_with(None, &src, 1920, 1080, true, Codec::H264, &cache).unwrap().path;
        let b = prepare_with(None, &src, 1920, 1080, false, Codec::H264, &cache).unwrap().path;
        let c = prepare_with(None, &src, 2560, 1440, true, Codec::H264, &cache).unwrap().path;
        assert!(a != b && a != c && b != c);
    }

    #[test]
    fn prepare_animated_image_requires_ffmpeg() {
        let t = Tmp::new("gif");
        let src = t.file("a.gif", b"GIF89a");
        let e = prepare_with(None, &src, 1920, 1080, true, Codec::H264, &t.path("cache")).unwrap_err();
        assert_eq!(e.0, "ffmpeg is required to convert GIF/APNG/WebP. Place ffmpeg.exe in the libs folder or add it to PATH.");
        assert!(!t.path("cache").exists(), "아무것도 만들지 않는다");
    }

    #[test]
    fn prepare_missing_source_is_a_media_error() {
        let t = Tmp::new("missing");
        let e = prepare_with(None, &t.path("nope.mp4").to_string_lossy(), 1, 1, true, Codec::H264, &t.path("cache")).unwrap_err();
        assert!(e.0.starts_with("Failed to prepare the video: "), "{}", e);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_failed_conversion_reports_and_cleans_part_file() {
        let t = Tmp::new("fail");
        let src = t.file("a.gif", b"GIF89a");
        let cache = t.path("cache");
        // /bin/false 는 항상 종료 코드 1, 출력 없음 → "Conversion failed: 1"
        let e = prepare_with(Some("/bin/false"), &src, 1920, 1080, true, Codec::H264, &cache).unwrap_err();
        assert_eq!(e.0, "Conversion failed: 1");
        assert!(names(&cache).is_empty(), "임시 파일이 남으면 안 된다: {:?}", names(&cache));
    }

    #[cfg(unix)]
    #[test]
    fn prepare_failed_conversion_reports_stderr_tail() {
        let t = Tmp::new("failmsg");
        let src = t.file("a.gif", b"GIF89a");
        let script = t.file("fakeffmpeg.sh", b"#!/bin/sh\necho 'boom: bad input' >&2\nexit 3\n");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = prepare_with(Some(&script), &src, 1920, 1080, true, Codec::H264, &t.path("cache")).unwrap_err();
        assert!(e.0.starts_with("Conversion failed: ") && e.0.contains("boom: bad input"), "{}", e);
    }

    // ---------------------------------------------------------------- run_capture / find_ffmpeg
    #[cfg(unix)]
    #[test]
    fn run_capture_reads_large_stderr_without_deadlock() {
        let cmd: Vec<String> = ["sh", "-c", "head -c 3000000 /dev/zero | tr '\\0' 'a' >&2; echo out"].iter().map(|s| s.to_string()).collect();
        let r = run_capture(&cmd, Some(Duration::from_secs(20))).unwrap();
        assert_eq!(r.code, Some(0));
        assert_eq!(r.stderr.len(), 3_000_000);
        assert_eq!(r.stdout.trim(), "out");
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_times_out_and_kills() {
        let cmd: Vec<String> = ["sleep", "10"].iter().map(|s| s.to_string()).collect();
        let t = Instant::now();
        let e = run_capture(&cmd, Some(Duration::from_millis(200))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_capture_missing_program_is_error() {
        assert!(run_capture(&["definitely-not-a-program-xyz".to_string()], None).is_err());
        assert!(probe("a.mp4", "definitely-not-a-program-xyz").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn find_ffmpeg_searches_all_bundles_before_path() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("find_roots");
        let source_ffmpeg = t.file("source/libs/ffmpeg", b"#!/bin/sh\n");
        let path_ffmpeg = t.file("path/ffmpeg", b"#!/bin/sh\n");
        for p in [&source_ffmpeg, &path_ffmpeg] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = std::env::join_paths([t.path("path")]).unwrap();
        let found = find_ffmpeg_in_roots(&[t.path("exe"), t.path("source")], Some(&path_var)).unwrap();
        assert_eq!(found, source_ffmpeg);
    }

    #[cfg(unix)]
    #[test]
    fn find_ffmpeg_prefers_libs_then_path() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("find");
        let mk = |rel: &str| {
            let p = t.file(rel, b"#!/bin/sh\n");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let on_path = mk("bin/ffmpeg");
        let path_var = std::env::join_paths([t.path("empty"), t.path("bin")]).unwrap();
        let root = vec![t.0.clone()];
        let no_root = vec![t.path("no-such-root")];
        assert_eq!(find_ffmpeg_in_roots(&root, Some(&path_var)), Some(on_path));
        let bundled = mk("libs/ffmpeg");
        assert_eq!(find_ffmpeg_in_roots(&root, Some(&path_var)), Some(bundled));
        assert_eq!(find_ffmpeg_in_roots(&no_root, None), None);
        // 실행 권한이 없으면 건너뛴다
        std::fs::set_permissions(t.path("bin/ffmpeg"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_ffmpeg_in_roots(&no_root, Some(&path_var)), None);
    }

    // ---------------------------------------------------------------- ensure_first_frame (ffmpeg 불필요한 경로)
    #[test]
    fn first_frame_guards_return_empty_string() {
        let t = Tmp::new("ff");
        let v = t.file("a.mp4", b"x");
        assert_eq!(ensure_first_frame_with(None, "", 1920, 1080, FitMode::Cover), None);
        assert_eq!(ensure_first_frame_with(None, &t.path("nope.mp4").to_string_lossy(), 1920, 1080, FitMode::Cover), None);
        assert_eq!(ensure_first_frame_with(None, &v, 0, 1080, FitMode::Cover), None);
        assert_eq!(ensure_first_frame_with(None, &v, 1920, 0, FitMode::Cover), None);
        assert_eq!(ensure_first_frame_with(None, &v, 1920, 1080, FitMode::Cover), None, "ffmpeg 가 없으면 None");
    }

    #[test]
    fn first_frame_existing_png_is_returned_without_ffmpeg() {
        let t = Tmp::new("ffhit");
        let v = t.file("a.mp4", b"x");
        let png = frame_path(&v, 1920, 1080, FitMode::Cover);
        std::fs::write(&png, b"png").unwrap();
        assert_eq!(ensure_first_frame_with(None, &v, 1920, 1080, FitMode::Cover), Some(png));
    }

    #[cfg(unix)]
    #[test]
    fn first_frame_failure_leaves_no_part_file() {
        let t = Tmp::new("ffbad");
        let v = t.file("a.mp4", b"x");
        assert_eq!(ensure_first_frame_with(Some("/bin/false"), &v, 64, 48, FitMode::Contain), None);
        assert_eq!(names(&t.0), vec!["a.mp4".to_string()]);
    }

    // ---------------------------------------------------------------- prune_cache
    #[test]
    fn prune_cache_keeps_only_needed_files() {
        let t = Tmp::new("prune");
        let cache = t.path("cache");
        let keep_video = t.file("cache/aaaa.mp4", b"1");
        t.file("cache/aaaa.frame2-cover-1920x1080.png", b"p"); // 유지: keep 영상의 첫 프레임
        t.file("cache/aaaa.frame2-cover-1920x1080.part.png", b"p"); // 삭제: 중단된 임시 프레임
        t.file("cache/bbbb.mp4", b"2"); // 삭제: 쓰지 않는 사본
        t.file("cache/bbbb.frame2-cover-1920x1080.png", b"p"); // 삭제: 다른 영상의 프레임
        t.file("cache/cccc.part.mp4", b"3"); // 삭제: 중단된 변환
        t.file("cache/sub/aaaa.mp4", b"keep-me"); // 하위 폴더는 불가침
        let outside = t.file("outside.mp4", b"keep-me"); // 다른 경로 불가침
        prune_cache(&[Some(keep_video), None], &cache);
        assert_eq!(names(&cache), vec!["aaaa.frame2-cover-1920x1080.png", "aaaa.mp4", "sub"]);
        assert!(t.path("cache/sub/aaaa.mp4").is_file() && Path::new(&outside).is_file());
    }

    #[test]
    fn prune_cache_missing_dir_is_noop() {
        prune_cache(&[], Path::new("/definitely/not/here"));
    }

    // ---------------------------------------------------------------- 실제 ffmpeg 가 있을 때만 (없으면 건너뜀)
    fn real_ffmpeg() -> Option<String> {
        let f = find_ffmpeg()?;
        has_encoder(&f, "libx264").then_some(f)
    }

    #[test]
    fn real_ffmpeg_gif_to_mp4_then_first_frame() {
        let Some(ff) = real_ffmpeg() else {
            eprintln!("건너뜀: libx264 가 있는 ffmpeg 없음");
            return;
        };
        let t = Tmp::new("real");
        let gif = t.path("a.gif").to_string_lossy().into_owned();
        let mk = run_capture(
            &[ff.clone(), "-y".into(), "-loglevel".into(), "error".into(), "-f".into(), "lavfi".into(), "-i".into(),
              "testsrc=size=64x48:rate=5:duration=1".into(), gif.clone()],
            Some(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(mk.code, Some(0), "{}", mk.stderr);
        let cache = t.path("cache");
        let r = prepare_with(Some(&ff), &gif, 1920, 1080, true, Codec::H264, &cache).unwrap();
        assert!(r.note.as_deref().unwrap_or("").starts_with("Converted to H.264 (AVC) (Animated images must be converted to MP4"), "{:?}", r.note);
        assert!(r.path.ends_with(".mp4") && Path::new(&r.path).is_file());
        assert_eq!(names(&cache).len(), 1, ".part 가 남지 않는다");
        let info = probe(&r.path, &ff).expect("변환 결과를 읽을 수 있다");
        assert_eq!((info.codec.as_str(), info.width, info.height), ("h264", 64, 48));
        // 두 번째는 캐시 적중
        let r2 = prepare_with(Some(&ff), &gif, 1920, 1080, true, Codec::H264, &cache).unwrap();
        assert_eq!((r2.path.as_str(), r2.note.as_deref()), (r.path.as_str(), Some("Using cached copy")));
        // 첫 프레임: 화면 크기 PNG
        let png = ensure_first_frame_with(Some(&ff), &r.path, 320, 200, FitMode::Cover).expect("첫 프레임");
        assert!(png.ends_with(".frame2-cover-320x200.png") && Path::new(&png).is_file(), "{}", png);
        let pi = probe(&png, &ff).expect("png 정보");
        assert_eq!((pi.width, pi.height), (320, 200));
        assert_eq!(ensure_first_frame_with(Some(&ff), &r.path, 320, 200, FitMode::Cover), Some(png), "두 번째는 그대로");
        // 정리: 영상과 프레임만 남기고 나머지는 지운다
        t.file("cache/junk.mp4", b"j");
        prune_cache(&[Some(r.path.clone())], &cache);
        let left = names(&cache);
        assert_eq!(left.len(), 2, "{:?}", left);
    }

    #[test]
    fn real_ffmpeg_h264_source_is_copied_when_no_conversion_needed() {
        let Some(ff) = real_ffmpeg() else { return };
        let t = Tmp::new("realcopy");
        let mp4 = t.path("a.mp4").to_string_lossy().into_owned();
        let mk = run_capture(
            &[ff.clone(), "-y".into(), "-loglevel".into(), "error".into(), "-f".into(), "lavfi".into(), "-i".into(),
              "testsrc=size=64x48:rate=10:duration=1".into(), "-c:v".into(), "libx264".into(), "-pix_fmt".into(), "yuv420p".into(), mp4.clone()],
            Some(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(mk.code, Some(0), "{}", mk.stderr);
        let r = prepare_with(Some(&ff), &mp4, 1920, 1080, true, Codec::H264, &t.path("cache")).unwrap();
        assert_eq!(r.note, None, "변환 없이 복사");
        assert_eq!(std::fs::read(&r.path).unwrap(), std::fs::read(&mp4).unwrap());
        // 선택 코덱이 다르면(원본 h264, 설정 hevc) 변환 이유가 생긴다
        let r2 = prepare_with(Some(&ff), &mp4, 1920, 1080, true, Codec::Hevc, &t.path("cache")).unwrap();
        let n2 = r2.note.as_deref().unwrap_or("");
        assert!(n2.contains("Codec h264 -> H.265 (HEVC)") || n2.starts_with("Conversion"), "{}", n2);
    }
}
