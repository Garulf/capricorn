use std::time::{Duration, Instant};

use capricorn_core::proto::RecorderState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Hyprland reported a change that may animate: focus, workspace, special workspace,
    /// a window opening, closing or moving. While those animations run Hyprland already
    /// reports the end state, so what is drawn cannot be trusted.
    HyprEvent,
    /// A window title changed. Titles do not animate, so this does not hold captures back.
    TitleChanged,
    /// No input for the settle duration.
    Settled,
    /// Input resumed after a settle or idle period.
    Active,
    /// No input for the idle duration.
    Idle,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no session-lock source is wired up yet")
    )]
    Locked(bool),
    Paused(bool),
    Fullscreen {
        excluded: bool,
    },
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Capture,
    Wait,
    Stop,
}

/// Shortest wait after a Hyprland event, longer than Hyprland's default animations.
const MIN_ANIMATION_QUIET: Duration = Duration::from_secs(1);

/// Decides when to grab the screen: after a change has settled, and at least every `floor`
/// while the user is active, but never while a Hyprland animation may still be running.
/// Calling `on` and getting `Capture` counts as having captured.
#[derive(Debug)]
pub struct Scheduler {
    floor: Duration,
    settle: Duration,
    animation_quiet: Duration,
    idle: bool,
    locked: bool,
    paused: bool,
    fullscreen_excluded: bool,
    settled: bool,
    changed_at: Option<Instant>,
    animated_at: Instant,
    last_capture: Instant,
}

impl Scheduler {
    /// Starts as if a change just happened, so the first frame follows one settle delay.
    /// Captures wait `settle`, but at least `MIN_ANIMATION_QUIET`, after any Hyprland event.
    pub fn new(floor: Duration, settle: Duration, now: Instant) -> Self {
        Self {
            floor,
            settle,
            animation_quiet: settle.max(MIN_ANIMATION_QUIET),
            idle: false,
            locked: false,
            paused: false,
            fullscreen_excluded: false,
            settled: true,
            changed_at: Some(now),
            animated_at: now,
            last_capture: now,
        }
    }

    pub fn on(&mut self, input: Input, now: Instant) -> Decision {
        let was_stopped = self.stopped();
        self.apply(input, now);
        if self.stopped() {
            return Decision::Stop;
        }
        if was_stopped {
            self.changed_at = Some(now);
            self.animated_at = now;
        }
        if self.due(input, now) {
            self.changed_at = None;
            self.last_capture = now;
            Decision::Capture
        } else {
            Decision::Wait
        }
    }

    pub fn state(&self) -> RecorderState {
        if self.locked {
            RecorderState::Locked
        } else if self.paused || self.fullscreen_excluded {
            RecorderState::Paused
        } else if self.idle {
            RecorderState::Idle
        } else {
            RecorderState::Recording
        }
    }

    fn stopped(&self) -> bool {
        self.state() != RecorderState::Recording
    }

    fn apply(&mut self, input: Input, now: Instant) {
        match input {
            Input::HyprEvent => {
                self.changed_at = Some(now);
                self.animated_at = now;
            }
            Input::TitleChanged => self.changed_at = Some(now),
            Input::Active => {
                self.idle = false;
                self.settled = false;
                self.changed_at = Some(now);
            }
            Input::Settled => self.settled = true,
            Input::Idle => self.idle = true,
            Input::Locked(locked) => self.locked = locked,
            Input::Paused(paused) => self.paused = paused,
            Input::Fullscreen { excluded } => self.fullscreen_excluded = excluded,
            Input::Tick => {}
        }
    }

    fn due(&self, input: Input, now: Instant) -> bool {
        if now.duration_since(self.animated_at) < self.animation_quiet {
            return false;
        }
        let floor_reached = now.duration_since(self.last_capture) >= self.floor;
        let change_settled = self.settled
            && self
                .changed_at
                .is_some_and(|at| now.duration_since(at) >= self.settle);
        match input {
            Input::Settled => self.changed_at.is_some(),
            Input::Tick => floor_reached || change_settled,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: Duration = Duration::from_secs(5);
    const SETTLE: Duration = Duration::from_millis(1500);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A scheduler that has just captured at `t0` and has nothing pending.
    fn fresh() -> (Scheduler, Instant) {
        let t0 = Instant::now();
        let mut s = Scheduler::new(FLOOR, SETTLE, t0);
        assert_eq!(s.on(Input::Tick, t0 + SETTLE), Decision::Capture);
        (s, t0 + SETTLE)
    }

    #[test]
    fn event_then_settle_captures() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Active, t + ms(100)), Decision::Wait);
        assert_eq!(s.on(Input::HyprEvent, t + ms(200)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(300)), Decision::Wait);
        assert_eq!(s.on(Input::Settled, t + ms(1700)), Decision::Capture);
        assert_eq!(s.on(Input::Tick, t + ms(1800)), Decision::Wait);
    }

    #[test]
    fn event_without_input_captures_after_settle_delay() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::HyprEvent, t + ms(100)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(1000)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(1600)), Decision::Capture);
    }

    #[test]
    fn settle_without_change_waits() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Settled, t + ms(100)), Decision::Wait);
    }

    #[test]
    fn floor_captures_when_active() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Tick, t + ms(2000)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR), Decision::Capture);
        assert_eq!(s.on(Input::Tick, t + FLOOR + ms(250)), Decision::Wait);
        assert_eq!(s.state(), RecorderState::Recording);
    }

    #[test]
    fn idle_stops() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Idle, t + ms(10)), Decision::Stop);
        assert_eq!(s.on(Input::Tick, t + FLOOR * 3), Decision::Stop);
        assert_eq!(s.on(Input::HyprEvent, t + FLOOR * 3), Decision::Stop);
        assert_eq!(s.state(), RecorderState::Idle);
        assert_eq!(s.on(Input::Active, t + FLOOR * 4), Decision::Wait);
        assert_eq!(s.on(Input::Settled, t + FLOOR * 5), Decision::Capture);
    }

    #[test]
    fn locked_stops() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Locked(true), t), Decision::Stop);
        assert_eq!(s.on(Input::Tick, t + FLOOR * 2), Decision::Stop);
        assert_eq!(s.state(), RecorderState::Locked);
        assert_eq!(s.on(Input::Locked(false), t + FLOOR * 3), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR * 3 + SETTLE), Decision::Capture);
    }

    #[test]
    fn paused_stops() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Paused(true), t), Decision::Stop);
        assert_eq!(s.on(Input::Settled, t + FLOOR * 2), Decision::Stop);
        assert_eq!(s.state(), RecorderState::Paused);
        s.on(Input::Paused(false), t + FLOOR * 3);
        assert_eq!(s.state(), RecorderState::Recording);
    }

    #[test]
    fn excluded_fullscreen_stops() {
        let (mut s, t) = fresh();
        assert_eq!(
            s.on(Input::Fullscreen { excluded: true }, t),
            Decision::Stop
        );
        assert_eq!(s.on(Input::Tick, t + FLOOR * 2), Decision::Stop);
        assert_eq!(s.state(), RecorderState::Paused);
        assert_eq!(
            s.on(Input::Fullscreen { excluded: false }, t + FLOOR * 3),
            Decision::Wait
        );
        assert_eq!(s.on(Input::Tick, t + FLOOR * 3 + SETTLE), Decision::Capture);
    }

    #[test]
    fn first_tick_after_start_captures_once_settled() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(FLOOR, SETTLE, t0);
        assert_eq!(s.on(Input::Tick, t0 + ms(100)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t0 + SETTLE), Decision::Capture);
    }

    #[test]
    fn floor_waits_for_hyprland_to_go_quiet() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::HyprEvent, t + FLOOR - ms(100)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR + ms(1300)), Decision::Wait);
        assert_eq!(s.on(Input::HyprEvent, t + FLOOR + ms(1350)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR + ms(2800)), Decision::Wait);
        assert_eq!(
            s.on(Input::Tick, t + FLOOR + ms(1350) + SETTLE),
            Decision::Capture
        );
    }

    #[test]
    fn settled_input_waits_for_hyprland_to_go_quiet() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Active, t + ms(100)), Decision::Wait);
        assert_eq!(s.on(Input::HyprEvent, t + ms(1500)), Decision::Wait);
        assert_eq!(s.on(Input::Settled, t + ms(1600)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(2000)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(3000)), Decision::Capture);
    }

    #[test]
    fn short_settle_still_waits_out_animations() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(FLOOR, ms(200), t0);
        assert_eq!(s.on(Input::Tick, t0 + ms(200)), Decision::Wait);
        assert_eq!(
            s.on(Input::Tick, t0 + MIN_ANIMATION_QUIET),
            Decision::Capture
        );
        let t = t0 + MIN_ANIMATION_QUIET;
        assert_eq!(s.on(Input::HyprEvent, t), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + ms(500)), Decision::Wait);
        assert_eq!(
            s.on(Input::Tick, t + MIN_ANIMATION_QUIET),
            Decision::Capture
        );
    }

    #[test]
    fn title_changes_do_not_hold_back_the_floor() {
        let (mut s, t) = fresh();
        for i in 0..20 {
            assert_eq!(s.on(Input::TitleChanged, t + ms(i * 250)), Decision::Wait);
        }
        assert_eq!(s.on(Input::Tick, t + FLOOR), Decision::Capture);
    }

    #[test]
    fn resuming_waits_out_animations_before_the_floor() {
        let (mut s, t) = fresh();
        assert_eq!(s.on(Input::Locked(true), t), Decision::Stop);
        assert_eq!(s.on(Input::Locked(false), t + FLOOR * 3), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR * 3 + ms(250)), Decision::Wait);
        assert_eq!(s.on(Input::Tick, t + FLOOR * 3 + SETTLE), Decision::Capture);
    }

    #[test]
    fn lock_wins_over_pause_in_reported_state() {
        let (mut s, t) = fresh();
        s.on(Input::Paused(true), t);
        s.on(Input::Locked(true), t);
        assert_eq!(s.state(), RecorderState::Locked);
    }
}
