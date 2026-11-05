//! 가려짐 / 정적 전환의 판단 로직. Win32를 쓰지 않아 어느 OS에서든 `cargo test`가 닿는다.
//! 입력은 `bool`, 숫자, `Duration`(과 Win32 조회를 감싼 클로저)뿐이다.
#![cfg_attr(not(windows), allow(dead_code))]

use std::time::{Duration, Instant};

/// 영상을 멈추는(가려진) 이유. 멈출 이유가 없으면 `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideReason { MonitorOff, Blocked, BatterySaver, Covered }

impl HideReason {
    pub fn text(self) -> &'static str {
        match self {
            Self::MonitorOff => "Monitor off",
            Self::Blocked => "Locked / screensaver / fullscreen app",
            Self::BatterySaver => "Battery saver mode",
            Self::Covered => "Desktop covered by another window",
        }
    }
}

/// 배경화면의 현재 상태. 예전의 `hidden`/`is_static`/`suspending`/`resuming` 스위치 4개를 하나로 합친 것이다.
/// 스위치로 만들 수 있는 16가지 중 실제로 도달하는 6가지만 표현한다.
///
/// | 상태 | hidden | is_static | suspending | resuming |
/// |---|---|---|---|---|
/// | `Playing` | F | F | F | F |
/// | `Paused` | T | F | F | F |
/// | `Static { stopping_mpv: true }` | T | T | T | F |
/// | `Static { stopping_mpv: false }` | T | T | F | F |
/// | `Resuming { waiting_mpv: true }` | F | T | T | T |
/// | `Resuming { waiting_mpv: false }` | F | T | F | T |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 영상 재생 중.
    Playing,
    /// 가려져서 영상을 멈춤. 가려진 시각과 처음 멈춘 이유를 들고 있다.
    Paused { since: Instant, reason: HideReason },
    /// 첫 프레임이 바탕화면으로 깔린 상태. `stopping_mpv`는 mpv가 아직 꺼지는 중이라는 뜻.
    Static { stopping_mpv: bool },
    /// 다시 보이게 되어 영상을 되살리는 중. `waiting_mpv`는 꺼지는 중인 mpv가 끝나기를 기다린다는 뜻.
    Resuming { waiting_mpv: bool },
}

impl State {
    /// 예전 `hidden` 스위치: `Paused`, `Static`일 때 참 (`Resuming`은 거짓).
    pub fn is_hidden(&self) -> bool { matches!(self, Self::Paused { .. } | Self::Static { .. }) }
    /// 예전 `is_static` 스위치: `Static`, `Resuming`일 때 참.
    pub fn is_static(&self) -> bool { matches!(self, Self::Static { .. } | Self::Resuming { .. }) }
    /// 예전 `resuming` 스위치.
    pub fn is_resuming(&self) -> bool { matches!(self, Self::Resuming { .. }) }
    pub fn is_playing(&self) -> bool { matches!(self, Self::Playing) }
    /// 예전 `suspending` 스위치: mpv가 꺼지는 중인가.
    pub fn waiting_mpv(&self) -> bool {
        matches!(self, Self::Static { stopping_mpv: true } | Self::Resuming { waiting_mpv: true })
    }
    /// `Paused`일 때만 가려진 시각.
    pub fn paused_since(&self) -> Option<Instant> {
        if let Self::Paused { since, .. } = self { Some(*since) } else { None }
    }
    /// `Paused`일 때만 이유 문구, 그 외에는 빈 문자열.
    pub fn reason_text(&self) -> &'static str {
        if let Self::Paused { reason, .. } = self { reason.text() } else { "" }
    }
    /// `Static`에서 재개를 시작한 뒤의 상태. mpv가 아직 꺼지는 중이면 그 사실을 이어받는다.
    pub fn begin_resume(self) -> Self { Self::Resuming { waiting_mpv: self.waiting_mpv() } }
    /// mpv 종료가 끝났을 때의 상태와, 지금 바로 mpv를 다시 띄워야 하는지(`true`).
    pub fn mpv_stopped(self) -> (Self, bool) {
        match self {
            Self::Static { .. } => (Self::Static { stopping_mpv: false }, false),
            Self::Resuming { .. } => (Self::Resuming { waiting_mpv: false }, true),
            other => (other, false),
        }
    }
}

/// 지금 걸려 있는 타이머의 기록 (타이머 번호 0..32를 비트로 저장한다).
/// 예전의 `eval_armed`, `snapshot_armed` 같은 불리언 5개를 대신한다.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Armed(u32);

impl Armed {
    /// `id`번 타이머가 걸려 있는지 기록한다 (`on == false`면 해제).
    pub fn mark(&mut self, id: usize, on: bool) {
        debug_assert!(id < 32, "timer id out of range: {id}");
        if on { self.0 |= 1 << id; } else { self.0 &= !(1 << id); }
    }
    pub fn is_armed(&self, id: usize) -> bool { id < 32 && self.0 & (1 << id) != 0 }
    pub fn clear(&mut self) { self.0 = 0; }
}

/// 가려짐 이유를 우선순위대로 판단한다: 모니터 꺼짐 > 차단 > 배터리 절약 > 가려짐.
/// 조회가 비싼 항목(Win32 호출)은 앞 단계에서 결정되면 부르지 않도록 클로저로 받는다.
pub fn hidden_reason(
    display_on: bool,
    blocked: impl FnOnce() -> bool,
    pause_on_battery_saver: bool,
    battery_saver_on: impl FnOnce() -> bool,
    covered: impl FnOnce() -> bool,
) -> Option<HideReason> {
    if !display_on { return Some(HideReason::MonitorOff); }
    if blocked() { return Some(HideReason::Blocked); }
    if pause_on_battery_saver && battery_saver_on() { return Some(HideReason::BatterySaver); }
    if covered() { return Some(HideReason::Covered); }
    None
}

/// 정적 전환(첫 프레임으로 바꾸기)을 시도해도 되는 상태인가.
/// 플레이어가 있고, 가려져 있고, 아직 정적이 아니며, 재개 중이 아니고,
/// 설정이 켜져 있고, 이전에 불가능하다고 기록되지 않았을 때만 `true`.
pub fn static_possible(
    has_player: bool, hidden: bool, is_static: bool, resuming: bool,
    static_snapshot: bool, snapshot_unavailable: bool,
) -> bool {
    has_player && hidden && !is_static && !resuming && static_snapshot && !snapshot_unavailable
}

/// 정적 상태에서 영상 재생으로 돌아가야 하는가 (가려짐이 풀렸거나 정적 기능이 꺼졌을 때).
pub fn should_leave_static(hidden: bool, static_snapshot: bool) -> bool {
    !hidden || !static_snapshot
}

/// 정적 전환 타이머를 몇 ms 뒤에 울릴지. 남은 시간에 여유 50ms를 더한다 (경계에서 한 번 더 깨는 것을 막는다).
pub fn snapshot_timer_ms(delay_s: i64, hidden_for: Duration) -> u32 {
    let remain = (delay_s as f64 - hidden_for.as_secs_f64()).max(0.0);
    ((remain * 1000.0) as u32).saturating_add(50)
}

/// 가려진 지 `delay_s`초가 지났는가 (음수 지연은 0으로 본다).
pub fn snapshot_due(delay_s: i64, hidden_for: Duration) -> bool {
    hidden_for >= Duration::from_secs(delay_s.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn reason(display_on: bool, blocked: bool, pause_bs: bool, bs: bool, covered: bool) -> Option<HideReason> {
        hidden_reason(display_on, || blocked, pause_bs, || bs, || covered)
    }

    #[test]
    fn nothing_hides_by_default() {
        assert_eq!(reason(true, false, true, false, false), None);
    }

    #[test]
    fn priority_monitor_off_beats_everything() {
        assert_eq!(reason(false, true, true, true, true), Some(HideReason::MonitorOff));
    }

    #[test]
    fn priority_blocked_beats_battery_and_covered() {
        assert_eq!(reason(true, true, true, true, true), Some(HideReason::Blocked));
    }

    #[test]
    fn priority_battery_beats_covered() {
        assert_eq!(reason(true, false, true, true, true), Some(HideReason::BatterySaver));
    }

    #[test]
    fn battery_saver_ignored_when_setting_off() {
        assert_eq!(reason(true, false, false, true, false), None);
        assert_eq!(reason(true, false, false, true, true), Some(HideReason::Covered));
    }

    #[test]
    fn covered_is_last() {
        assert_eq!(reason(true, false, true, false, true), Some(HideReason::Covered));
    }

    #[test]
    fn later_checks_are_skipped_once_decided() {
        let (blocked_calls, battery_calls, covered_calls) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let r = hidden_reason(
            true,
            || { blocked_calls.set(blocked_calls.get() + 1); true },
            true,
            || { battery_calls.set(battery_calls.get() + 1); true },
            || { covered_calls.set(covered_calls.get() + 1); true },
        );
        assert_eq!(r, Some(HideReason::Blocked));
        assert_eq!((blocked_calls.get(), battery_calls.get(), covered_calls.get()), (1, 0, 0));

        let calls = Cell::new(0);
        let r = hidden_reason(false, || { calls.set(calls.get() + 1); false }, true,
            || { calls.set(calls.get() + 1); false }, || { calls.set(calls.get() + 1); false });
        assert_eq!(r, Some(HideReason::MonitorOff));
        assert_eq!(calls.get(), 0);

        // 배터리 절약 설정이 꺼져 있으면 배터리 상태는 조회하지 않는다.
        let battery_calls = Cell::new(0);
        hidden_reason(true, || false, false, || { battery_calls.set(1); true }, || false);
        assert_eq!(battery_calls.get(), 0);
    }

    fn flags(s: State) -> (bool, bool, bool, bool) {
        (s.is_hidden(), s.is_static(), s.waiting_mpv(), s.is_resuming())
    }

    #[test]
    fn state_matches_old_flag_table() {
        let paused = State::Paused { since: Instant::now(), reason: HideReason::Covered };
        assert_eq!(flags(State::Playing), (false, false, false, false));
        assert_eq!(flags(paused), (true, false, false, false));
        assert_eq!(flags(State::Static { stopping_mpv: true }), (true, true, true, false));
        assert_eq!(flags(State::Static { stopping_mpv: false }), (true, true, false, false));
        assert_eq!(flags(State::Resuming { waiting_mpv: true }), (false, true, true, true));
        assert_eq!(flags(State::Resuming { waiting_mpv: false }), (false, true, false, true));
    }

    #[test]
    fn polling_is_slow_in_every_state_except_playing() {
        // `tick`은 `hidden || is_static`일 때 느린 간격을 쓴다. 그 조건이 "Playing이 아님"과 같아야 한다.
        let paused = State::Paused { since: Instant::now(), reason: HideReason::Blocked };
        for s in [paused, State::Static { stopping_mpv: true }, State::Static { stopping_mpv: false },
                  State::Resuming { waiting_mpv: true }, State::Resuming { waiting_mpv: false }] {
            assert!(s.is_hidden() || s.is_static(), "{s:?}");
            assert!(!s.is_playing(), "{s:?}");
        }
        assert!(!(State::Playing.is_hidden() || State::Playing.is_static()));
        assert!(State::Playing.is_playing());
    }

    #[test]
    fn paused_keeps_since_and_reason_only_there() {
        let now = Instant::now();
        let paused = State::Paused { since: now, reason: HideReason::BatterySaver };
        assert_eq!(paused.paused_since(), Some(now));
        assert_eq!(paused.reason_text(), "Battery saver mode");
        for s in [State::Playing, State::Static { stopping_mpv: false }, State::Resuming { waiting_mpv: false }] {
            assert_eq!(s.paused_since(), None);
            assert_eq!(s.reason_text(), "");
        }
    }

    #[test]
    fn begin_resume_inherits_waiting_for_mpv() {
        assert_eq!(State::Static { stopping_mpv: true }.begin_resume(), State::Resuming { waiting_mpv: true });
        assert_eq!(State::Static { stopping_mpv: false }.begin_resume(), State::Resuming { waiting_mpv: false });
    }

    #[test]
    fn mpv_stopped_in_static_only_clears_the_wait() {
        let (next, restart) = State::Static { stopping_mpv: true }.mpv_stopped();
        assert_eq!(next, State::Static { stopping_mpv: false });
        assert!(!restart);
    }

    #[test]
    fn mpv_stopped_while_resuming_restarts_mpv() {
        let (next, restart) = State::Resuming { waiting_mpv: true }.mpv_stopped();
        assert_eq!(next, State::Resuming { waiting_mpv: false });
        assert!(restart);
    }

    #[test]
    fn mpv_stopped_is_a_no_op_elsewhere() {
        let paused = State::Paused { since: Instant::now(), reason: HideReason::Covered };
        for s in [State::Playing, paused] {
            assert_eq!(s.mpv_stopped(), (s, false));
        }
    }

    #[test]
    fn armed_starts_empty_and_tracks_each_id_separately() {
        let mut a = Armed::default();
        assert!(!a.is_armed(1) && !a.is_armed(5));
        a.mark(1, true);
        a.mark(5, true);
        assert!(a.is_armed(1) && a.is_armed(5) && !a.is_armed(3));
        a.mark(1, false);
        assert!(!a.is_armed(1) && a.is_armed(5));
    }

    #[test]
    fn armed_mark_is_idempotent() {
        let mut a = Armed::default();
        a.mark(2, true);
        a.mark(2, true);
        assert!(a.is_armed(2));
        a.mark(2, false);
        a.mark(2, false);
        assert!(!a.is_armed(2));
    }

    #[test]
    fn armed_clear_drops_everything() {
        let mut a = Armed::default();
        for id in 1..=5 { a.mark(id, true); }
        a.clear();
        assert_eq!(a, Armed::default());
        for id in 1..=5 { assert!(!a.is_armed(id)); }
    }

    #[test]
    fn armed_out_of_range_reads_as_not_armed() {
        assert!(!Armed::default().is_armed(100));
    }

    #[test]
    fn reason_text_is_stable() {
        assert_eq!(HideReason::MonitorOff.text(), "Monitor off");
        assert_eq!(HideReason::Blocked.text(), "Locked / screensaver / fullscreen app");
        assert_eq!(HideReason::BatterySaver.text(), "Battery saver mode");
        assert_eq!(HideReason::Covered.text(), "Desktop covered by another window");
    }

    #[test]
    fn static_possible_needs_every_condition() {
        assert!(static_possible(true, true, false, false, true, false));
        assert!(!static_possible(false, true, false, false, true, false), "no player");
        assert!(!static_possible(true, false, false, false, true, false), "not hidden");
        assert!(!static_possible(true, true, true, false, true, false), "already static");
        assert!(!static_possible(true, true, false, true, true, false), "resuming");
        assert!(!static_possible(true, true, false, false, false, false), "setting off");
        assert!(!static_possible(true, true, false, false, true, true), "snapshot unavailable");
    }

    #[test]
    fn leave_static_when_visible_or_feature_off() {
        assert!(should_leave_static(false, true));
        assert!(should_leave_static(true, false));
        assert!(should_leave_static(false, false));
        assert!(!should_leave_static(true, true));
    }

    #[test]
    fn timer_ms_counts_down_with_margin() {
        assert_eq!(snapshot_timer_ms(30, Duration::from_secs(0)), 30_050);
        assert_eq!(snapshot_timer_ms(30, Duration::from_secs(10)), 20_050);
        assert_eq!(snapshot_timer_ms(30, Duration::from_millis(29_500)), 550);
    }

    #[test]
    fn timer_ms_never_below_margin() {
        assert_eq!(snapshot_timer_ms(30, Duration::from_secs(30)), 50);
        assert_eq!(snapshot_timer_ms(30, Duration::from_secs(300)), 50);
        assert_eq!(snapshot_timer_ms(0, Duration::from_secs(1)), 50);
        assert_eq!(snapshot_timer_ms(-5, Duration::from_secs(0)), 50);
    }

    #[test]
    fn timer_ms_saturates_instead_of_overflowing() {
        assert_eq!(snapshot_timer_ms(i64::MAX, Duration::from_secs(0)), u32::MAX);
    }

    #[test]
    fn due_at_exact_delay() {
        assert!(!snapshot_due(30, Duration::from_millis(29_999)));
        assert!(snapshot_due(30, Duration::from_secs(30)));
        assert!(snapshot_due(30, Duration::from_secs(31)));
    }

    #[test]
    fn due_immediately_for_zero_or_negative_delay() {
        assert!(snapshot_due(0, Duration::from_secs(0)));
        assert!(snapshot_due(-1, Duration::from_secs(0)));
    }
}
