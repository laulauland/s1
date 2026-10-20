//! Adaptive in-flight cap: additive increase, multiplicative decrease.
//!
//! - On a 429 the cap halves (at least 1) and stays down for one success
//!   window. A 429 for a request sent before the last decrease only extends
//!   the hold: it reports the old load, which the decrease already answered.
//! - After the hold, each success window with a success adds one, up to the
//!   configured maximum.
//! - With `adaptive = false` the cap stays at the maximum.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct Aimd {
    max: u32,
    cap: u32,
    adaptive: bool,
    window: Duration,
    /// No increase before this time.
    hold_until: Option<Instant>,
    /// Time of the last decrease.
    last_decrease: Option<Instant>,
    /// Time of the last change (increase or decrease).
    last_change: Option<Instant>,
}

/// What a 429 did to the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    Down { from: u32, to: u32 },
    Up { from: u32, to: u32 },
}

impl Aimd {
    pub fn new(max: u32, adaptive: bool, window: Duration) -> Self {
        let max = max.max(1);
        Self {
            max,
            cap: max,
            adaptive,
            window,
            hold_until: None,
            last_decrease: None,
            last_change: None,
        }
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    /// A request sent at `sent_at` got a 429 at `now`.
    pub fn on_rate_limited(&mut self, now: Instant, sent_at: Instant) -> Change {
        if !self.adaptive {
            return Change::None;
        }
        let hold = now + self.window;
        self.hold_until = Some(self.hold_until.map_or(hold, |h| h.max(hold)));
        if self.last_decrease.is_some_and(|d| sent_at < d) {
            return Change::None;
        }
        let from = self.cap;
        self.cap = (self.cap / 2).max(1);
        self.last_decrease = Some(now);
        self.last_change = Some(now);
        if from == self.cap {
            Change::None
        } else {
            Change::Down { from, to: self.cap }
        }
    }

    /// A request succeeded at `now`.
    pub fn on_success(&mut self, now: Instant) -> Change {
        if !self.adaptive || self.cap >= self.max {
            return Change::None;
        }
        if self.hold_until.is_some_and(|h| now < h) {
            return Change::None;
        }
        if self
            .last_change
            .is_some_and(|c| now.duration_since(c) < self.window)
        {
            return Change::None;
        }
        let from = self.cap;
        self.cap += 1;
        self.last_change = Some(now);
        Change::Up { from, to: self.cap }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const W: Duration = Duration::from_secs(5);

    #[test]
    fn halves_then_grows_one_per_window() {
        let t0 = Instant::now();
        let mut a = Aimd::new(16, true, W);
        assert_eq!(a.on_rate_limited(t0, t0), Change::Down { from: 16, to: 8 });
        assert_eq!(a.on_success(t0 + Duration::from_secs(1)), Change::None);
        assert_eq!(a.on_success(t0 + W), Change::Up { from: 8, to: 9 });
        assert_eq!(a.on_success(t0 + W + Duration::from_secs(1)), Change::None);
        assert_eq!(a.on_success(t0 + W * 2), Change::Up { from: 9, to: 10 });
    }

    #[test]
    fn late_429s_from_old_requests_do_not_halve_again() {
        let t0 = Instant::now();
        let mut a = Aimd::new(32, true, W);
        a.on_rate_limited(t0 + Duration::from_millis(10), t0);
        assert_eq!(a.cap(), 16);
        // Sent before the decrease: no second halving.
        a.on_rate_limited(t0 + Duration::from_millis(20), t0);
        assert_eq!(a.cap(), 16);
        // Sent after it: halves.
        let t1 = t0 + Duration::from_millis(30);
        a.on_rate_limited(t1 + Duration::from_millis(5), t1);
        assert_eq!(a.cap(), 8);
    }

    #[test]
    fn not_adaptive_keeps_the_cap() {
        let t0 = Instant::now();
        let mut a = Aimd::new(4, false, W);
        assert_eq!(a.on_rate_limited(t0, t0), Change::None);
        assert_eq!(a.cap(), 4);
    }

    #[derive(Debug, Clone)]
    enum Ev {
        Ok(u64),
        Limited { after_ms: u64, sent_back_ms: u64 },
    }

    fn ev() -> impl Strategy<Value = Ev> {
        prop_oneof![
            (0u64..3_000).prop_map(Ev::Ok),
            (0u64..3_000, 0u64..2_000).prop_map(|(a, s)| Ev::Limited {
                after_ms: a,
                sent_back_ms: s
            }),
        ]
    }

    proptest! {
        /// After a 429, the cap stays at or below half its earlier value
        /// until a success window has passed.
        #[test]
        fn cap_stays_halved_for_a_window(
            max in 1u32..64,
            events in proptest::collection::vec(ev(), 1..200),
        ) {
            let t0 = Instant::now();
            let mut a = Aimd::new(max, true, W);
            let mut now = t0;
            // (time of the 429, ceiling until time + W)
            let mut ceilings: Vec<(Instant, u32)> = Vec::new();
            for e in events {
                match e {
                    Ev::Ok(gap) => {
                        now += Duration::from_millis(gap);
                        a.on_success(now);
                    }
                    Ev::Limited { after_ms, sent_back_ms } => {
                        now += Duration::from_millis(after_ms);
                        let before = a.cap();
                        let sent = now.checked_sub(Duration::from_millis(sent_back_ms)).unwrap_or(now);
                        a.on_rate_limited(now, sent);
                        ceilings.push((now, (before / 2).max(1).max(a.cap().min(before))));
                        prop_assert!(a.cap() <= before);
                    }
                }
                for (t, ceiling) in &ceilings {
                    if now < *t + W {
                        prop_assert!(a.cap() <= *ceiling, "cap {} above {}", a.cap(), ceiling);
                    }
                }
                prop_assert!(a.cap() >= 1 && a.cap() <= max);
            }
        }
    }
}
