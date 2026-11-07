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

/// 백그라운드에서 만든 첫 프레임을 지금 적용해도 되는가.
/// 영상·화면 크기·맞춤 방식이 그대로이고(`target_current`), 아직 멈춘 상태이며, 지연 시간이 지났을 때만 참.
pub fn should_apply_frame(target_current: bool, paused: bool, due: bool) -> bool {
    target_current && paused && due
}

