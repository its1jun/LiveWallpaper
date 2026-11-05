//! 정적 전환: Windows 바탕화면을 영상 첫 프레임으로 바꿨다가 원래대로 되돌리는 로직.
//!
//! Windows API 는 함수(클로저)로 주입받으므로 이 모듈은 Windows 없이도 테스트할 수 있다.
//!
//! 안전 규칙
//!   - 원래 바탕화면 정보는 바꾸기 *전에* 파일(wallpaper_state.json)에 저장한다 → 비정상 종료 뒤에도 복구 가능
//!   - 지금 바탕화면이 우리가 적용한 것일 때만 되돌린다 → 사용자가 중간에 바꿨다면 덮어쓰지 않음
//!   - 슬라이드쇼/단색/원본 파일 없음처럼 원상 복원이 확실하지 않은 경우에는 바꾸지 않는다

use serde::{Deserialize, Serialize};
use crate::config::norm_key;
use std::path::{Path, PathBuf};

/// 슬라이드쇼 등에서 Windows 가 만드는 임시 파일. 우리가 바탕화면을 바꾸면 덮어써지므로 복원할 수 없다.
const UNRESTORABLE_PREFIX: &str = "transcodedwallpaper";

/// wallpaper_state.json 내용. 세 값이 모두 문자열이어야 유효하다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    original: String,
    frame: String,
    applied: String,
}

// ---------------------------------------------------------------- 경로 비교
/// 두 경로가 같은 파일을 가리키는가 (둘 다 비어있지 않아야 함).
fn same(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    Path::new(&norm_key(a)) == Path::new(&norm_key(b))
}

// ---------------------------------------------------------------- WallpaperSwapper
pub struct WallpaperSwapper {
    path: PathBuf,
    get: Box<dyn Fn() -> Option<String>>,
    set: Box<dyn Fn(&str) -> bool>,
    isfile: Box<dyn Fn(&str) -> bool>,
}

impl WallpaperSwapper {
    /// `get() -> 현재 바탕화면 경로(없으면 None)`, `set(path) -> 성공 여부`.
    pub fn new(
        state_path: impl Into<PathBuf>,
        get: impl Fn() -> Option<String> + 'static,
        set: impl Fn(&str) -> bool + 'static,
    ) -> Self {
        Self {
            path: state_path.into(),
            get: Box::new(get),
            set: Box::new(set),
            isfile: Box::new(|p| Path::new(p).is_file()),
        }
    }

    /// 파일 존재 검사를 바꿔 끼운다 (테스트용).
    #[cfg(test)]
    pub fn with_isfile(mut self, isfile: impl Fn(&str) -> bool + 'static) -> Self {
        self.isfile = Box::new(isfile);
        self
    }

    // ---- 상태 파일 ----
    fn load(&self) -> Option<State> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str::<State>(text.trim_start_matches('\u{feff}')).ok()
    }

    fn save(&self, st: &State) -> bool {
        serde_json::to_string_pretty(st).is_ok_and(|json| crate::config::write_atomic(&self.path, &json).is_ok())
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// 우리가 바꾼 바탕화면이 남아 있을 수 있는가 (유효한 상태 파일이 있는가). 테스트용.
    #[cfg(test)]
    pub fn swapped(&self) -> bool {
        self.load().is_some()
    }

    fn is_ours(st: &State, current: Option<&str>) -> bool {
        current.is_some_and(|c| same(c, &st.applied) || same(c, &st.frame))
    }

    // ---- 전환 ----
    /// Windows 바탕화면을 `frame` 이미지로 바꾼다. 실패하면 이유(영어 문구, 상태 표시줄에 그대로 쓰임).
    pub fn enter(&self, frame: &str) -> Result<(), String> {
        if frame.is_empty() || !(self.isfile)(frame) {
            return Err("First-frame image is missing".into());
        }
        let st = self.load();
        let current = (self.get)();
        let original = match &st {
            // 이미 우리가 바꾼 상태 → 진짜 원본은 저장해 둔 것
            Some(s) if Self::is_ours(s, current.as_deref()) => s.original.clone(),
            // 지금 바탕화면이 사용자의 진짜 바탕화면
            _ => {
                let Some(current) = current else {
                    return Err("Windows wallpaper is not a single picture (solid color?)".into());
                };
                let file_name = Path::new(&current).file_name().map_or(String::new(), |n| n.to_string_lossy().to_lowercase());
                if file_name.starts_with(UNRESTORABLE_PREFIX) {
                    return Err("Windows wallpaper is a slideshow/transcoded image and cannot be restored".into());
                }
                if !(self.isfile)(&current) {
                    return Err("Original wallpaper file is missing".into());
                }
                current
            }
        };
        // 바꾸기 전에 먼저 저장 (바꾸는 도중 종료돼도 복구되도록 applied 에는 일단 frame 을 넣어 둔다)
        let new_state = State { original: original.clone(), frame: frame.to_string(), applied: frame.to_string() };
        if !self.save(&new_state) {
            return Err("Unable to save the wallpaper restore info".into());
        }
        if !(self.set)(frame) {
            match &st {
                Some(old) => {
                    self.save(old); // 이전 상태 유지 (원본 정보를 잃지 않는다)
                }
                None => self.clear(),
            }
            return Err("Windows rejected the wallpaper change".into());
        }
        // 적용 뒤 Windows 가 돌려주는 값 (경로 표기가 달라질 수 있어 읽어서 저장)
        if let Some(applied) = (self.get)() {
            if !same(&applied, frame) {
                self.save(&State { original, frame: frame.to_string(), applied });
            }
        }
        Ok(())
    }

    /// 원래 바탕화면으로 되돌린다. 바꾼 적이 없거나 사용자가 직접 바꿨다면 아무것도 덮어쓰지 않는다.
    /// 반환: 정상 처리 여부 (false 면 다음 실행에서 다시 시도하도록 상태 파일을 남긴다 — 단, 원본이 사라진 경우는 삭제).
    pub fn restore(&self) -> bool {
        let Some(st) = self.load() else { return true };
        if Self::is_ours(&st, (self.get)().as_deref()) {
            if !(self.isfile)(&st.original) {
                self.clear(); // 원본이 사라져 복원 불가 → 더 시도해도 소용없음
                return false;
            }
            if !(self.set)(&st.original) {
                return false;
            }
        }
        self.clear();
        true
    }
}

// ---------------------------------------------------------------- 테스트
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    /// 가짜 Windows: 현재 바탕화면, 존재하는 파일, set 호출 기록.
    #[derive(Default)]
    struct Fake {
        current: String,
        files: HashSet<String>,
        set_ok: bool,
        /// set 성공 후 get() 이 돌려줄 표기 (Windows 가 경로 표기를 바꾸는 상황)
        report_as: Option<String>,
        set_calls: Vec<String>,
        /// set 이 불리는 순간 상태 파일이 이미 있었는가 (저장이 먼저인지 확인)
        state_existed_at_set: Vec<bool>,
    }

    struct Env {
        fake: Rc<RefCell<Fake>>,
        dir: PathBuf,
        state: PathBuf,
    }

    impl Env {
        fn new(current: &str, files: &[&str]) -> Env {
            let dir = crate::test_util::unique_dir("snap");
            let state = dir.join("wallpaper_state.json");
            let fake = Rc::new(RefCell::new(Fake {
                current: current.into(),
                files: files.iter().map(|s| s.to_string()).collect(),
                set_ok: true,
                ..Default::default()
            }));
            Env { fake, dir, state }
        }

        fn swapper(&self) -> WallpaperSwapper {
            let (f1, f2, f3) = (self.fake.clone(), self.fake.clone(), self.fake.clone());
            let state = self.state.clone();
            WallpaperSwapper::new(
                self.state.clone(),
                move || Some(f1.borrow().current.clone()).filter(|c| !c.is_empty()),
                move |p| {
                    let mut f = f2.borrow_mut();
                    f.set_calls.push(p.to_string());
                    f.state_existed_at_set.push(state.exists());
                    if f.set_ok {
                        f.current = f.report_as.clone().unwrap_or_else(|| p.to_string());
                        true
                    } else {
                        false
                    }
                },
            )
            .with_isfile(move |p| f3.borrow().files.contains(p))
        }

        fn read_state(&self) -> Option<State> {
            serde_json::from_str(&std::fs::read_to_string(&self.state).ok()?).ok()
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const ORIG: &str = "C:\\Pics\\home.jpg";
    const FRAME: &str = "C:\\app\\cache\\abc.frame2-contain-1920x1080.png";

    // ---- 경로 비교 (Windows 규칙) ----
    #[cfg(windows)]
    #[test]
    fn norm_key_windows_rules() {
        let k = norm_key;
        assert_eq!(k("C:\\Pics\\A.JPG"), k("c:/pics/a.jpg"));
        assert_eq!(k("C:\\Pics\\sub\\..\\a.jpg"), k("C:\\Pics\\a.jpg"));
        assert_eq!(k("C:\\Pics\\.\\a.jpg"), k("C:\\Pics\\a.jpg"));
        assert_ne!(k("C:\\Pics\\a.jpg"), k("C:\\Pics\\b.jpg"));
        assert_ne!(k("\\\\srv\\share\\a.png"), k("\\srv\\share\\a.png")); // UNC 보존
    }

    #[test]
    fn same_requires_non_empty() {
        assert!(!same("", ""));
        assert!(!same("a", ""));
        assert!(same("/x/y.png", "/x/y.png"));
    }

    // ---- enter ----
    #[test]
    fn enter_happy_path_saves_before_set() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        assert!(!sw.swapped());
        assert_eq!(sw.enter(FRAME), Ok(()));
        assert!(sw.swapped());
        let f = env.fake.borrow();
        assert_eq!(f.set_calls, vec![FRAME.to_string()]);
        assert_eq!(f.state_existed_at_set, vec![true], "상태 파일은 set 보다 먼저 저장돼야 한다");
        drop(f);
        assert_eq!(
            env.read_state().unwrap(),
            State { original: ORIG.into(), frame: FRAME.into(), applied: FRAME.into() }
        );
    }

    #[test]
    fn enter_rejects_missing_frame() {
        let env = Env::new(ORIG, &[ORIG]); // FRAME 파일 없음
        let sw = env.swapper();
        assert_eq!(sw.enter(FRAME), Err("First-frame image is missing".into()));
        assert_eq!(sw.enter(""), Err("First-frame image is missing".into()));
        assert!(env.fake.borrow().set_calls.is_empty());
        assert!(!sw.swapped());
    }

    #[test]
    fn enter_rejects_solid_color() {
        let env = Env::new("", &[FRAME]);
        assert_eq!(
            env.swapper().enter(FRAME),
            Err("Windows wallpaper is not a single picture (solid color?)".into())
        );
        assert!(env.fake.borrow().set_calls.is_empty());
    }

    #[test]
    fn enter_rejects_slideshow_transcoded_case_insensitive() {
        // '/' 로 써서 Windows/Linux 어느 쪽에서 돌려도 같은 결과
        let tw = "C:/Users/u/AppData/Roaming/Microsoft/Windows/Themes/TranscodedWallpaper";
        let env = Env::new(tw, &[tw, FRAME]);
        let r = env.swapper().enter(FRAME);
        assert_eq!(r, Err("Windows wallpaper is a slideshow/transcoded image and cannot be restored".into()));
        assert!(!env.state.exists());
    }

    #[test]
    fn enter_rejects_missing_original_file() {
        let env = Env::new(ORIG, &[FRAME]); // ORIG 파일 없음
        assert_eq!(env.swapper().enter(FRAME), Err("Original wallpaper file is missing".into()));
        assert!(env.fake.borrow().set_calls.is_empty());
    }

    #[test]
    fn enter_fails_if_state_cannot_be_saved_and_does_not_touch_wallpaper() {
        let mut env = Env::new(ORIG, &[ORIG, FRAME]);
        let blocker = env.dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        env.state = blocker.join("wallpaper_state.json"); // 부모가 파일이라 저장 불가
        let sw = env.swapper();
        assert_eq!(sw.enter(FRAME), Err("Unable to save the wallpaper restore info".into()));
        assert!(env.fake.borrow().set_calls.is_empty(), "저장 실패 시 바탕화면을 바꾸면 안 된다");
    }

    #[test]
    fn enter_set_rejected_without_previous_state_clears_state() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        env.fake.borrow_mut().set_ok = false;
        let sw = env.swapper();
        assert_eq!(sw.enter(FRAME), Err("Windows rejected the wallpaper change".into()));
        assert!(!env.state.exists());
        assert!(!sw.swapped());
    }

    #[test]
    fn enter_set_rejected_with_previous_state_keeps_old_state() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        assert!(sw.enter(FRAME).is_ok());
        let before = env.read_state().unwrap();
        // 두 번째 프레임으로 교체하려는데 Windows 가 거부
        let frame2 = "C:\\app\\cache\\zzz.frame2-cover-1920x1080.png";
        env.fake.borrow_mut().files.insert(frame2.into());
        env.fake.borrow_mut().set_ok = false;
        assert_eq!(sw.enter(frame2), Err("Windows rejected the wallpaper change".into()));
        assert_eq!(env.read_state().unwrap(), before, "원본 정보를 잃으면 안 된다");
        assert_eq!(before.original, ORIG);
    }

    #[test]
    fn enter_records_applied_when_windows_reports_other_spelling() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        env.fake.borrow_mut().report_as = Some("C:\\app\\cache\\ABC~1.PNG".into());
        let sw = env.swapper();
        assert!(sw.enter(FRAME).is_ok());
        let st = env.read_state().unwrap();
        assert_eq!(st.applied, "C:\\app\\cache\\ABC~1.PNG");
        assert_eq!(st.frame, FRAME);
        assert_eq!(st.original, ORIG);
    }

    #[test]
    fn enter_twice_keeps_true_original() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        assert!(sw.enter(FRAME).is_ok());
        // 지금 바탕화면은 FRAME. 다시 enter 해도 original 이 FRAME 으로 덮이면 안 된다.
        assert!(sw.enter(FRAME).is_ok());
        assert_eq!(env.read_state().unwrap().original, ORIG);
    }

    #[test]
    fn enter_after_user_changed_wallpaper_uses_new_original() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        assert!(sw.enter(FRAME).is_ok());
        // 사용자가 중간에 다른 그림으로 바꿈
        let mine = "C:\\Pics\\new.jpg";
        env.fake.borrow_mut().files.insert(mine.into());
        env.fake.borrow_mut().current = mine.into();
        assert!(sw.enter(FRAME).is_ok());
        assert_eq!(env.read_state().unwrap().original, mine);
    }

    // ---- restore ----
    #[test]
    fn restore_without_state_is_ok_and_does_nothing() {
        let env = Env::new(ORIG, &[ORIG]);
        assert!(env.swapper().restore());
        assert!(env.fake.borrow().set_calls.is_empty());
    }

    #[test]
    fn restore_after_enter_puts_original_back() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        sw.enter(FRAME).unwrap();
        assert!(sw.restore());
        assert_eq!(env.fake.borrow().current, ORIG);
        assert!(!sw.swapped());
        assert!(!env.state.exists());
    }

    #[test]
    fn restore_matches_by_frame_when_applied_differs() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        env.fake.borrow_mut().report_as = Some("C:\\app\\cache\\ABC~1.PNG".into());
        let sw = env.swapper();
        sw.enter(FRAME).unwrap();
        // 어떤 이유로 Windows 가 이번엔 frame 표기로 돌려준다
        env.fake.borrow_mut().current = FRAME.into();
        env.fake.borrow_mut().report_as = None; // 복원 때는 요청한 경로 그대로 적용된다고 가정
        assert!(sw.restore());
        assert_eq!(env.fake.borrow().current, ORIG);
    }

    #[test]
    fn restore_does_not_overwrite_user_change() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        sw.enter(FRAME).unwrap();
        let mine = "C:\\Pics\\mine.jpg";
        env.fake.borrow_mut().current = mine.into();
        let calls_before = env.fake.borrow().set_calls.len();
        assert!(sw.restore());
        assert_eq!(env.fake.borrow().set_calls.len(), calls_before, "사용자 설정을 덮어쓰면 안 된다");
        assert_eq!(env.fake.borrow().current, mine);
        assert!(!env.state.exists(), "상태 파일은 정리");
    }

    #[test]
    fn restore_original_gone_returns_false_and_clears() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        sw.enter(FRAME).unwrap();
        env.fake.borrow_mut().files.remove(ORIG);
        assert!(!sw.restore());
        assert!(!env.state.exists(), "더 시도해도 소용없으므로 상태 삭제");
    }

    #[test]
    fn restore_set_failure_returns_false_and_keeps_state_for_retry() {
        let env = Env::new(ORIG, &[ORIG, FRAME]);
        let sw = env.swapper();
        sw.enter(FRAME).unwrap();
        env.fake.borrow_mut().set_ok = false;
        assert!(!sw.restore());
        assert!(env.state.exists());
        // 다음 실행에서 set 이 되면 복원된다
        env.fake.borrow_mut().set_ok = true;
        assert!(sw.restore());
        assert_eq!(env.fake.borrow().current, ORIG);
    }

    // ---- 상태 파일 검증 ----
    #[test]
    fn invalid_state_files_are_treated_as_absent() {
        for bad in [
            "not json",
            "[]",
            r#"{"original":"a","frame":"b"}"#,                  // applied 없음
            r#"{"original":"a","frame":"b","applied":3}"#,      // 문자열 아님
            r#"{"original":null,"frame":"b","applied":"c"}"#,
        ] {
            let env = Env::new(ORIG, &[ORIG, FRAME]);
            std::fs::write(&env.state, bad).unwrap();
            let sw = env.swapper();
            assert!(!sw.swapped(), "무효: {bad}");
            assert!(sw.restore(), "무효 상태는 복원할 것이 없음");
        }
    }

    #[test]
    fn state_file_format_is_pretty_utf8() {
        let env = Env::new("C:\\사진\\집.jpg", &["C:\\사진\\집.jpg", FRAME]);
        env.swapper().enter(FRAME).unwrap();
        let text = std::fs::read_to_string(&env.state).unwrap();
        assert!(text.contains("C:\\\\사진\\\\집.jpg"), "한글은 그대로, 역슬래시는 JSON 이스케이프");
        assert!(text.starts_with("{\n  \"original\""));
        assert!(!env.dir.join("wallpaper_state.tmp").exists());
    }
}
