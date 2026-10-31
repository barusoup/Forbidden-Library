//! The playback position, as a pure function of time.
//!
//! The clock never reads the system time itself. Every method takes the
//! `Instant` the caller considers "now", so the player can be driven
//! deterministically in tests and one poll can use one consistent time for
//! every decision it makes.

use std::time::Instant;

pub struct Clock {
    /// The media time at the moment the clock was last anchored.
    base_ms: i64,
    /// When the clock started advancing from `base_ms`. `None` while stopped,
    /// which freezes the position at `base_ms`.
    started: Option<Instant>,
}

impl Clock {
    /// A stopped clock holding `at_ms`.
    pub fn new(at_ms: i64) -> Self {
        Self {
            base_ms: at_ms,
            started: None,
        }
    }

    pub fn running(&self) -> bool {
        self.started.is_some()
    }

    /// Begin advancing from the held position. Does nothing when already running,
    /// so a repeated play cannot jump the position forward.
    pub fn start(&mut self, now: Instant) {
        if self.started.is_none() {
            self.started = Some(now);
        }
    }

    /// Freeze at the current position. The held position becomes the value
    /// `position(now)` reported just before the stop.
    pub fn stop(&mut self, now: Instant) {
        if self.started.is_some() {
            self.base_ms = self.position(now);
            self.started = None;
        }
    }

    pub fn position(&self, now: Instant) -> i64 {
        match self.started {
            None => self.base_ms,
            Some(started) => {
                let elapsed = now.saturating_duration_since(started);
                self.base_ms + elapsed.as_micros() as i64 / 1000
            }
        }
    }

    /// Re-anchor to `ms`. Running state is kept: a running clock keeps
    /// advancing from the new value, and a stopped one holds it.
    pub fn set(&mut self, ms: i64, now: Instant) {
        self.base_ms = ms;
        if self.started.is_some() {
            self.started = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_new_clock_is_stopped_and_holds_its_position() {
        let base = Instant::now();
        let clock = Clock::new(1500);
        assert!(!clock.running());
        assert_eq!(clock.position(base), 1500);
        assert_eq!(clock.position(at(base, 9_000)), 1500);
    }

    #[test]
    fn a_started_clock_advances_with_time() {
        let base = Instant::now();
        let mut clock = Clock::new(1000);
        clock.start(base);
        assert!(clock.running());
        assert_eq!(clock.position(at(base, 0)), 1000);
        assert_eq!(clock.position(at(base, 250)), 1250);
    }

    #[test]
    fn stop_freezes_at_the_current_position() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.start(base);
        clock.stop(at(base, 400));
        assert!(!clock.running());
        assert_eq!(clock.position(at(base, 400)), 400);
        assert_eq!(clock.position(at(base, 5_000)), 400);
    }

    #[test]
    fn start_after_stop_resumes_from_the_frozen_position() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.start(base);
        clock.stop(at(base, 400));
        clock.start(at(base, 1_000));
        assert_eq!(clock.position(at(base, 1_000)), 400);
        assert_eq!(clock.position(at(base, 1_100)), 500);
    }

    #[test]
    fn starting_twice_does_not_reanchor() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.start(base);
        clock.start(at(base, 300));
        assert_eq!(clock.position(at(base, 300)), 300);
    }

    #[test]
    fn stopping_twice_keeps_the_first_position() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.start(base);
        clock.stop(at(base, 200));
        clock.stop(at(base, 900));
        assert_eq!(clock.position(at(base, 900)), 200);
    }

    #[test]
    fn set_while_stopped_moves_the_held_position_without_starting() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.set(7_000, base);
        assert!(!clock.running());
        assert_eq!(clock.position(at(base, 500)), 7_000);
    }

    #[test]
    fn set_while_running_reanchors_and_keeps_running() {
        let base = Instant::now();
        let mut clock = Clock::new(0);
        clock.start(base);
        clock.set(3_000, at(base, 100));
        assert!(clock.running());
        assert_eq!(clock.position(at(base, 100)), 3_000);
        assert_eq!(clock.position(at(base, 150)), 3_050);
    }

    #[test]
    fn a_position_before_the_anchor_time_does_not_go_negative_in_elapsed() {
        let base = Instant::now();
        let mut clock = Clock::new(10);
        clock.start(at(base, 500));
        // An earlier `now` than the start saturates to zero elapsed time.
        assert_eq!(clock.position(base), 10);
    }
}
