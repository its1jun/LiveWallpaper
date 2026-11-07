//! 설정 저장/불러오기 (JSON). 잘못된 값이나 깨진 파일이 있어도 기본값으로 복구한다.
//!
//! 플랫폼 독립 모듈이다 (`#[cfg(windows)]` 없음) → 어느 OS 에서든 `cargo test` 가 돈다.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// 프로그램이 설치된 폴더(exe 가 있는 폴더). 아이콘, `libs\` 같은 읽기 전용 리소스가 여기 있다.
/// 설치 위치(`Program Files`)에는 쓸 수 없으므로 설정과 캐시는 `data_dir()` 에 둔다.
pub fn install_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 사용자 데이터 폴더: 설정(config.json), 바탕화면 복원 정보, 변환 캐시(cache/).
/// 우선순위: 환경변수 `LIVEWALLPAPER_HOME` → `%LOCALAPPDATA%\LiveWallpaper` → (없으면) 설치 폴더.
pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("LIVEWALLPAPER_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    match std::env::var_os("LOCALAPPDATA") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("LiveWallpaper"),
        _ => install_dir(),
    }
}

/// 이전 버전(설정을 exe 폴더에 두던 포터블 방식)의 설정을 새 데이터 폴더로 한 번 복사한다.
/// 새 위치에 이미 파일이 있으면 건드리지 않는다. 캐시는 다시 만들어지므로 옮기지 않는다.
pub fn migrate_legacy_data() {
    let (old, new) = (install_dir(), data_dir());
    if old == new {
        return;
    }
    let _ = std::fs::create_dir_all(&new);
    for name in ["config.json", "wallpaper_state.json"] {
        let (from, to) = (old.join(name), new.join(name));
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

/// 런타임 리소스(libmpv, ffmpeg 등)를 찾을 폴더 목록: exe 폴더 → (지정된 경우) `LIVEWALLPAPER_HOME` → 소스 트리.
/// 이렇게 하면 `target\release\LiveWallpaper.exe` 를 직접 실행해도 저장소의 `libs\` 를 찾는다.
pub fn resource_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![install_dir()];
    let home = std::env::var_os("LIVEWALLPAPER_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    // The source tree path is only compiled into debug builds, so release exes do not carry the
    // builder's local path. Release builds run from outside the install folder: set LIVEWALLPAPER_HOME.
    #[cfg(debug_assertions)]
    let dev_root = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    #[cfg(not(debug_assertions))]
    let dev_root: Option<PathBuf> = None;
    for root in home.into_iter().chain(dev_root) {
        if !dirs.contains(&root) {
            dirs.push(root);
        }
    }
    dirs
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// Windows 원래 바탕화면 복원 정보 (정적 전환 중 비정상 종료돼도 다음 실행에서 복구하기 위한 영구 파일).
pub fn wallpaper_state_path() -> PathBuf {
    data_dir().join("wallpaper_state.json")
}

/// 앱 아이콘(.ico) 경로. 없으면 `None`(→ 시스템 기본 아이콘).
/// 릴리스는 exe 옆 icon.ico (설치 프로그램이 복사), 디버그는 builder/icon.ico.
pub fn icon_path() -> Option<String> {
    #[cfg(debug_assertions)]
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("builder/icon.ico");
    #[cfg(not(debug_assertions))]
    let p = install_dir().join("icon.ico");
    p.is_file().then(|| p.to_string_lossy().into_owned())
}

/// 절대 경로화 (대소문자 보존). Windows 에서는 `/` 와 `\\` 를 같게 보고 `.`/`..` 를 정리한다.
pub fn abspath_key(p: &str) -> String {
    std::path::absolute(p).map_or_else(|_| p.to_string(), |a| a.to_string_lossy().into_owned())
}

/// 경로 비교용 키: 절대 경로를 소문자로 바꾼 것 (Windows 파일 시스템은 대소문자를 구분하지 않는다).
pub fn norm_key(p: &str) -> String {
    abspath_key(p).to_lowercase()
}

/// 임시 파일(`*.tmp`)에 쓴 뒤 이름을 바꿔 교체한다 → 중간에 끊겨도 반쪽 파일이 남지 않는다.
/// 상위 폴더가 없으면 만든다. (config.json 과 wallpaper_state.json 이 함께 쓴다.)
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        // Flush to disk before the rename, so a power cut cannot leave an empty file under the final name
        // (wallpaper_state.json holds the only record of the user's original wallpaper).
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        let _ = file.sync_all(); // best effort: a failed flush must not block saving
    }
    // Windows 에서도 기존 파일을 덮어쓴다 (std::fs::rename 은 MoveFileExW REPLACE_EXISTING).
    let result = std::fs::rename(&tmp, path);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 영상 맞춤 방식. config.json 에는 소문자 문자열(`contain`/`cover`/`stretch`)로 저장된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    /// 맞추기 (비율 유지, 여백 허용)
    #[default]
    Contain,
    /// 채우기 (비율 유지, 넘치는 부분 잘림)
    Cover,
    /// 늘리기 (비율 무시)
    Stretch,
}

impl FitMode {
    pub fn as_str(self) -> &'static str {
        match self { Self::Contain => "contain", Self::Cover => "cover", Self::Stretch => "stretch" }
    }
    /// 설정창 콤보박스 항목 순서 (맞추기, 채우기, 늘리기). `index`/`from_index` 가 이 배열에서 나온다.
    pub const ALL: [FitMode; 3] = [Self::Contain, Self::Cover, Self::Stretch];
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or_default()
    }
}

/// cache 사본 코덱. config.json 에는 소문자 문자열(`h264`/`hevc`)로 저장된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self { Self::H264 => "h264", Self::Hevc => "hevc" }
    }
    /// 설정창 콤보박스 항목 순서 (H.264, H.265). `media::pure::CODECS` 순서와 같다.
    pub const ALL: [Codec; 2] = [Self::H264, Self::Hevc];
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or_default()
    }
}

/// 필드 선언 순서 = config.json 키 순서.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    /// 사용자가 선택한 원본 경로 (표시/다음 선택 시작 위치용, 지워져도 됨)
    pub video_path: String,
    /// 실제로 재생하는 cache/ 안의 사본 (원본이 없어도 이것으로 복원)
    pub media_path: String,
    pub fit_mode: FitMode,
    /// GIF 등 움직이는 이미지가 이 용량을 넘으면 선택 시 경고
    pub size_warning_mb: i64,
    /// 화면보다 큰 영상은 모니터 해상도로 변환, fps/비트레이트 제한
    pub optimize: bool,
    pub video_codec: Codec,
    pub pause_on_battery_saver: bool,
    pub coverage_threshold: f64,
    /// 실행 시 마지막 영상을 자동 적용
    pub restore_on_start: bool,
    /// Windows 시작 시 자동 실행 (실행할 때마다 등록을 확인/복구)
    pub autostart: bool,
    /// mpv 하드웨어 디코딩. "no"=끔, "d3d11va" 등 직접 지정 가능
    pub hwdec: String,
    /// 오래 가려져 있으면 Windows 바탕화면을 영상 첫 프레임으로 바꾸고 mpv 를 종료
    pub static_snapshot: bool,
    /// 가려진 뒤 이 시간(초)이 지나야 정적 전환
    pub snapshot_delay_s: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            video_path: String::new(),
            media_path: String::new(),
            fit_mode: FitMode::Contain,
            size_warning_mb: 50,
            optimize: true,
            video_codec: Codec::H264,
            pause_on_battery_saver: true,
            coverage_threshold: 0.95,
            restore_on_start: true,
            autostart: true,
            hwdec: "auto-safe".into(),
            static_snapshot: true,
            snapshot_delay_s: 30,
        }
    }
}

/// `data[key]` 가 `dst` 와 같은 타입일 때만 반영한다 (타입이 다르거나 enum 값이 낯설면 기본값 유지).
/// 정수 필드에 실수/bool 은 거부되고, 실수 필드에는 정수도 허용된다 (serde 기본 동작).
fn take<T: DeserializeOwned>(data: &Map<String, Value>, key: &str, dst: &mut T) {
    if let Some(v) = data.get(key).and_then(|v| T::deserialize(v).ok()) {
        *dst = v;
    }
}

impl Settings {
    /// 기본 경로(`config_path()`)에서 읽는다.
    pub fn load() -> Settings { Self::load_from(&config_path()) }

    /// 깨진 파일/없는 파일/잘못된 타입은 전부 기본값으로 복구한다.
    pub fn load_from(path: &Path) -> Settings {
        let mut s = Settings::default();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return s,
        };
        // BOM 이 있으면 제거 (메모장 저장 대비).
        let text = text.trim_start_matches('\u{feff}');
        let data = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => m,
            _ => return s,
        };

        // 필드별로 "JSON 타입이 기본값 타입과 같을 때만" 반영 (한 필드가 틀려도 나머지는 살린다).
        take(&data, "video_path", &mut s.video_path);
        take(&data, "media_path", &mut s.media_path);
        take(&data, "fit_mode", &mut s.fit_mode);
        take(&data, "size_warning_mb", &mut s.size_warning_mb);
        take(&data, "optimize", &mut s.optimize);
        take(&data, "video_codec", &mut s.video_codec);
        take(&data, "pause_on_battery_saver", &mut s.pause_on_battery_saver);
        take(&data, "coverage_threshold", &mut s.coverage_threshold);
        take(&data, "restore_on_start", &mut s.restore_on_start);
        take(&data, "autostart", &mut s.autostart);
        take(&data, "hwdec", &mut s.hwdec);
        take(&data, "static_snapshot", &mut s.static_snapshot);
        take(&data, "snapshot_delay_s", &mut s.snapshot_delay_s);

        s.normalize();
        s
    }

    /// 범위/허용값 검증. 벗어나면 기본값으로 되돌린다.
    fn normalize(&mut self) {
        if self.hwdec.trim().is_empty() {
            self.hwdec = "auto-safe".into();
        }
        // NaN 도 이 조건에서 걸러진다 (비교가 false).
        if !(self.coverage_threshold > 0.0 && self.coverage_threshold <= 1.0) {
            self.coverage_threshold = 0.95;
        }
        if self.snapshot_delay_s < 0 {
            self.snapshot_delay_s = 30;
        }
    }

    /// 기본 경로(`config_path()`)에 저장한다.
    pub fn save(&self) -> bool { self.save_to(&config_path()) }

    /// `write_atomic` 으로 저장한다. 성공 여부를 돌려준다.
    pub fn save_to(&self, path: &Path) -> bool {
        serde_json::to_string_pretty(self).is_ok_and(|json| write_atomic(path, &json).is_ok())
    }
}
