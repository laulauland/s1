//! Circuit breaker per backend and key: after `failures` failures (5xx or
//! connection errors) within `window`, stop sending for `cooldown` and
//! mark the backend `breaker_open`. After the cool-down the breaker is
//! half-open: requests go again; the first failure opens it again, the
//! first success closes it.
//!
//! 429s are throttling, not failures; AIMD and `Retry-After` handle them.

use std::collections::VecDeque;
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Closed,
    Open,
    HalfOpen,
}

/// A state change worth logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Opened { until: Instant },
    Closed,
}

#[derive(Debug, Clone)]
pub struct Breaker {
    threshold: u32,
    window: Duration,
    cooldown: Duration,
    failures: VecDeque<Instant>,
    open_until: Option<Instant>,
    half_open: bool,
}

impl Breaker {
    pub fn new(threshold: u32, window: Duration, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            window,
            cooldown,
            failures: VecDeque::new(),
            open_until: None,
            half_open: false,
        }
    }

    pub fn state(&self, now: Instant) -> State {
        match self.open_until {
            Some(u) if now < u => State::Open,
            Some(_) => State::HalfOpen,
            None if self.half_open => State::HalfOpen,
            None => State::Closed,
        }
    }

    /// `Err(until)` while open.
    pub fn allow(&mut self, now: Instant) -> Result<(), Instant> {
        match self.open_until {
            Some(u) if now < u => Err(u),
            Some(_) => {
                self.open_until = None;
                self.half_open = true;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn open_until(&self, now: Instant) -> Option<Instant> {
        self.open_until.filter(|u| now < *u)
    }

    pub fn recent_failures(&mut self, now: Instant) -> usize {
        self.trim(now);
        self.failures.len()
    }

    pub fn record_success(&mut self, _now: Instant) -> Option<Event> {
        let was_half_open = self.half_open;
        self.half_open = false;
        if was_half_open {
            self.failures.clear();
            return Some(Event::Closed);
        }
        None
    }

    pub fn record_failure(&mut self, now: Instant) -> Option<Event> {
        if self.open_until(now).is_some() {
            return None;
        }
        self.trim(now);
        self.failures.push_back(now);
        if self.half_open || self.failures.len() >= self.threshold as usize {
            let until = now + self.cooldown;
            self.open_until = Some(until);
            self.half_open = false;
            self.failures.clear();
            return Some(Event::Opened { until });
        }
        None
    }

    fn trim(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) >= self.window)
        {
            self.failures.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn five_failures_in_thirty_seconds_open_it() {
        let t0 = Instant::now();
        let mut b = Breaker::new(5, S * 30, S * 30);
        for i in 0..4 {
            assert_eq!(b.record_failure(t0 + S * i), None);
        }
        assert_eq!(b.state(t0 + S * 4), State::Closed);
        assert!(matches!(
            b.record_failure(t0 + S * 4),
            Some(Event::Opened { .. })
        ));
        assert_eq!(b.state(t0 + S * 5), State::Open);
        assert_eq!(b.allow(t0 + S * 5), Err(t0 + S * 34));
        // Cool-down over: half-open; a failure opens it again at once.
        assert_eq!(b.allow(t0 + S * 34), Ok(()));
        assert_eq!(b.state(t0 + S * 34), State::HalfOpen);
        assert!(matches!(
            b.record_failure(t0 + S * 35),
            Some(Event::Opened { .. })
        ));
        // After the next cool-down a success closes it.
        assert_eq!(b.allow(t0 + S * 65), Ok(()));
        assert_eq!(b.record_success(t0 + S * 66), Some(Event::Closed));
        assert_eq!(b.state(t0 + S * 66), State::Closed);
    }

    #[test]
    fn old_failures_fall_out_of_the_window() {
        let t0 = Instant::now();
        let mut b = Breaker::new(5, S * 30, S * 30);
        for i in 0..4 {
            b.record_failure(t0 + S * i);
        }
        assert_eq!(b.record_failure(t0 + S * 40), None);
        assert_eq!(b.recent_failures(t0 + S * 40), 1);
        assert_eq!(b.state(t0 + S * 40), State::Closed);
    }
}
