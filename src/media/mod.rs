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
use std::sync::atomic::{AtomicBool, Ordering};
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
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
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
/// Set when the app is quitting: every running `run_capture` child is killed and returns `Interrupted`.
static CANCEL_ALL: AtomicBool = AtomicBool::new(false);
pub fn cancel_all() { CANCEL_ALL.store(true, Ordering::Release); }

/// 자식 프로세스 출력 한 줄기에서 보관할 최대 크기. 앞부분(`probe`의 스트림 정보, `-encoders` 목록)과
/// 뒷부분(변환 오류의 마지막 메시지)만 필요하므로 그 사이는 읽어서 버린다.
const CAPTURE_HEAD_CAP: usize = 256 * 1024;
const CAPTURE_TAIL_CAP: usize = 64 * 1024;

/// 끝까지 읽되 메모리에는 앞 `CAPTURE_HEAD_CAP` + 뒤 `CAPTURE_TAIL_CAP` 바이트만 남긴다 (파이프는 끝까지 비워 자식이 막히지 않게 한다).
fn read_bounded<R: Read>(mut r: R) -> Vec<u8> {
    let mut head: Vec<u8> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut skipped = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut chunk = &buf[..n];
        if head.len() < CAPTURE_HEAD_CAP {
            let take = (CAPTURE_HEAD_CAP - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if !chunk.is_empty() {
            tail.extend_from_slice(chunk);
            if tail.len() > CAPTURE_TAIL_CAP * 2 {
                let cut = tail.len() - CAPTURE_TAIL_CAP;
                tail.drain(..cut);
                skipped = true;
            }
        }
    }
    if tail.len() > CAPTURE_TAIL_CAP {
        let cut = tail.len() - CAPTURE_TAIL_CAP;
        tail.drain(..cut);
        skipped = true;
    }
    if skipped {
        head.extend_from_slice(b"\n[... output truncated ...]\n");
    }
    head.extend_from_slice(&tail);
    head
}

pub(crate) fn run_capture(cmd: &[String], timeout: Option<Duration>) -> io::Result<Captured> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = c.spawn()?;
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || read_bounded(out));
    let t_err = std::thread::spawn(move || read_bounded(err));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                // 상태 확인 실패: 자식을 남겨 두지 않고 정리한 뒤 오류를 돌려준다.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if CANCEL_ALL.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            let _ = child.kill();
            let _ = child.wait();
            // 읽기 스레드는 기다리지 않는다 (손자 프로세스가 파이프를 쥐고 있어도 멈추지 않도록).
            return Err(io::Error::new(io::ErrorKind::TimedOut, "process timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
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
/// 원본 확장자가 `.mp4` 이면 복사본과 변환본의 이름이 같아서(`{key}.mp4`) 머리말만 보는 값싼 검사로는
/// 둘을 구별할 수 없다. 정상 영상을 지우지 않도록 여기서는 `ffmpeg -i` 확인을 유지한다.
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
/// 첫 프레임이 이미 cache 에 있으면 경로를 돌려준다. ffmpeg 를 실행하지 않으므로 UI 스레드에서 불러도 된다.
/// 파일이 비어 있지 않고 PNG 시그니처로 시작하는지만 본다 (임시 `.part` 이름으로 쓴 뒤 이름을 바꾸므로 반쪽 파일은 드물다).
pub fn cached_first_frame(video: &str, w: u32, h: u32, fit: FitMode) -> Option<String> {
    if video.is_empty() || !Path::new(video).is_file() || w == 0 || h == 0 {
        return None;
    }
    let dst = frame_path(video, w, h, fit);
    let p = Path::new(&dst);
    (non_empty_file(p) && has_png_signature(p)).then_some(dst)
}

fn has_png_signature(path: &Path) -> bool {
    let mut head = [0u8; 8];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == *b"\x89PNG\r\n\x1a\n"
}

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
    // 이미 있으면 그대로 쓴다 (ffmpeg 가 있으면 PNG 머리말도 확인; 매번 `ffmpeg -i` 를 띄우지 않는다).
    // 깨졌으면 지우고 다시 만든다.
    if non_empty_file(Path::new(&dst)) && (ffmpeg.is_none() || has_png_signature(Path::new(&dst))) {
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
