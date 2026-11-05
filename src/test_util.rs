//! 테스트 전용 도우미: 테스트마다 겹치지 않는 임시 폴더.
#![cfg(test)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 비어 있는 새 임시 폴더를 만들어 돌려준다 (지우는 것은 호출한 쪽 몫).
pub fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lw-{}-{}-{}", tag, std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 스코프를 벗어나면 폴더를 지우는 임시 폴더.
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Tmp {
        Tmp(unique_dir(tag))
    }
    /// 폴더 안에 파일을 만들고(필요하면 하위 폴더도) 전체 경로 문자열을 돌려준다.
    pub fn file(&self, name: &str, data: &[u8]) -> String {
        let p = self.0.join(name);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
