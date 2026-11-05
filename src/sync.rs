//! Mutex 보조. 다른 스레드가 패닉해 락이 poisoned 되어도 값은 그대로 쓴다
//! (이 앱의 상태는 락을 잡은 채 중간 상태로 남지 않으므로 복구해도 안전하다).
use std::sync::{Mutex, MutexGuard, PoisonError};

pub trait LockExt<T> {
    /// 락을 잡는다. poisoned 여도 내부 값을 꺼내 돌려준다.
    fn guard(&self) -> MutexGuard<'_, T>;

    /// 락을 잡고 `f` 를 실행한 뒤 바로 푼다.
    fn with_lock<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut *self.guard())
    }
}

impl<T> LockExt<T> for Mutex<T> {
    fn guard(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
