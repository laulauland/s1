//! One backend+key's sending state: request and token buckets, the
//! in-flight count against the AIMD cap, and a pause from rate-limit
//! headers. Pure: every method takes the time, so tests use a simulated clock.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::aimd::{Aimd, Change};
use super::bucket::{TokenBucket, WINDOW};
use super::{Limits, RateHint};

/// Why a request cannot go now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The in-flight cap is reached: wait for a release.
    InFlight,
    /// Wait until this time (bucket refill or a provider pause).
    Until(Instant),
}

/// How a request ended, for AIMD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// 429 from the provider.
    RateLimited,
    /// Any other end (errors, cancelled requests): no AIMD change.
    Other,
}

#[derive(Debug, Clone)]
pub struct Gate {
    rpm: Option<TokenBucket>,
    tpm: Option<TokenBucket>,
    aimd: Aimd,
    in_flight: u32,
    paused_until: Option<Instant>,
    /// Sends in the last [`WINDOW`]: (time, tokens), for `/status`.
    recent: VecDeque<(Instant, u64)>,
}

impl Gate {
    pub fn new(limits: &Limits, now: Instant) -> Self {
        Self {
            rpm: limits.rpm.map(|l| TokenBucket::new(l, now)),
            tpm: limits.tpm.map(|l| TokenBucket::new(l, now)),
            aimd: Aimd::new(
                limits.max_in_flight,
                limits.adaptive,
                limits.success_window(),
            ),
            in_flight: 0,
            paused_until: None,
            recent: VecDeque::new(),
        }
    }

    /// May a request of `cost` input tokens go now? No side effects.
    pub fn check(&mut self, now: Instant, cost: u64) -> Result<(), Blocked> {
        if self.in_flight >= self.aimd.cap() {
            return Err(Blocked::InFlight);
        }
        match self.wait_until(now, cost) {
            None => Ok(()),
            Some(u) => Err(Blocked::Until(u)),
        }
    }

    /// The earliest time a request of `cost` tokens could start, if later
    /// than `now`: the provider pause and the RPM and TPM buckets (not the
    /// in-flight cap). The same time [`Gate::check`] gives as `Blocked::Until`.
    pub fn wait_until(&mut self, now: Instant, cost: u64) -> Option<Instant> {
        let mut wait = Duration::ZERO;
        if let Some(p) = self.paused_until
            && p > now
        {
            wait = wait.max(p - now);
        }
        if let Some(w) = self.rpm.as_mut().and_then(|b| b.wait(now, 1)) {
            wait = wait.max(w);
        }
        if let Some(w) = self.tpm.as_mut().and_then(|b| b.wait(now, cost)) {
            wait = wait.max(w);
        }
        (!wait.is_zero()).then(|| now.checked_add(wait).unwrap_or(now))
    }

    /// Send if allowed: spend tokens and count the request in flight.
    pub fn try_send(&mut self, now: Instant, cost: u64) -> Result<(), Blocked> {
        self.check(now, cost)?;
        if let Some(b) = self.rpm.as_mut() {
            b.take(now, 1);
        }
        if let Some(b) = self.tpm.as_mut() {
            b.take(now, cost);
        }
        self.in_flight += 1;
        self.recent.push_back((now, cost));
        self.trim(now);
        Ok(())
    }

    /// A request sent at `sent_at` ended at `now`.
    pub fn release(&mut self, now: Instant, sent_at: Instant, outcome: Outcome) -> Change {
        self.in_flight = self.in_flight.saturating_sub(1);
        match outcome {
            Outcome::Success => self.aimd.on_success(now),
            Outcome::RateLimited => self.aimd.on_rate_limited(now, sent_at),
            Outcome::Other => Change::None,
        }
    }

    /// Obey `Retry-After` and quota headers: stop sending for a while.
    pub fn observe(&mut self, now: Instant, hint: &RateHint) -> Option<Instant> {
        let pause = hint.pause()?.min(super::headers::MAX_PAUSE);
        // `checked_add` keeps a huge pause from panicking; fall back to the cap.
        let until = now
            .checked_add(pause)
            .or_else(|| now.checked_add(super::headers::MAX_PAUSE))
            .unwrap_or(now);
        if self.paused_until.is_none_or(|p| p < until) {
            self.paused_until = Some(until);
        }
        self.paused_until
    }

    /// The provider pause, if it is still running.
    pub fn paused_until(&self, now: Instant) -> Option<Instant> {
        self.paused_until.filter(|p| *p > now)
    }

    /// When a new one-token request could start (provider pause, RPM, TPM),
    /// if not now. Reported as `throttled_until`.
    pub fn throttled_until(&mut self, now: Instant) -> Option<Instant> {
        self.wait_until(now, 1)
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight
    }

    pub fn cap(&self) -> u32 {
        self.aimd.cap()
    }

    /// Requests and tokens sent in the last minute.
    pub fn rate(&mut self, now: Instant) -> (u64, u64) {
        self.trim(now);
        let tokens = self.recent.iter().map(|(_, c)| c).sum();
        (self.recent.len() as u64, tokens)
    }

    fn trim(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) >= WINDOW)
        {
            self.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn limits(rpm: u64, tpm: u64, cap: u32) -> Limits {
        Limits {
            rpm: Some(rpm),
            tpm: Some(tpm),
            max_in_flight: cap,
            adaptive: false,
            ..Limits::default()
        }
    }

    #[test]
    fn pauses_follow_retry_after() {
        let t0 = Instant::now();
        let mut g = Gate::new(&Limits::default(), t0);
        let hint = RateHint {
            retry_after: Some(Duration::from_secs(2)),
            exhausted_for: None,
        };
        g.observe(t0, &hint);
        assert_eq!(
            g.check(t0, 1),
            Err(Blocked::Until(t0 + Duration::from_secs(2)))
        );
        assert_eq!(g.check(t0 + Duration::from_secs(2), 1), Ok(()));
    }

    #[test]
    fn huge_pause_is_capped_not_a_panic() {
        let t0 = Instant::now();
        let mut g = Gate::new(&Limits::default(), t0);
        let hint = RateHint {
            retry_after: Some(Duration::MAX),
            exhausted_for: Some(Duration::from_secs(u64::MAX)),
        };
        let until = g.observe(t0, &hint).unwrap();
        assert_eq!(until, t0 + super::super::headers::MAX_PAUSE);
        assert_eq!(g.check(t0, 1), Err(Blocked::Until(until)));
    }

    #[test]
    fn throttled_until_includes_empty_buckets() {
        let t0 = Instant::now();
        let mut g = Gate::new(&limits(1, 1_000_000, 4), t0);
        assert_eq!(g.throttled_until(t0), None);
        g.try_send(t0, 1).unwrap();
        let Err(Blocked::Until(u)) = g.check(t0, 1) else {
            panic!("rpm bucket is empty");
        };
        assert_eq!(g.throttled_until(t0), Some(u));
        assert_eq!(g.paused_until(t0), None, "no provider pause");
    }

    #[test]
    fn in_flight_cap_blocks_until_release() {
        let t0 = Instant::now();
        let mut g = Gate::new(&limits(1000, 1_000_000, 2), t0);
        g.try_send(t0, 1).unwrap();
        g.try_send(t0, 1).unwrap();
        assert_eq!(g.check(t0, 1), Err(Blocked::InFlight));
        g.release(t0, t0, Outcome::Success);
        assert_eq!(g.check(t0, 1), Ok(()));
    }

    #[derive(Debug, Clone)]
    struct Arrival {
        gap_ms: u64,
        /// Size as a share of TPM, cubed: mostly small, some up to the limit.
        size: f64,
        duration_ms: u64,
    }

    fn arrival() -> impl Strategy<Value = Arrival> {
        (0u64..2_000, 0.0f64..=1.0, 1u64..20_000).prop_map(|(gap_ms, size, duration_ms)| Arrival {
            gap_ms,
            size,
            duration_ms,
        })
    }

    #[test]
    fn oversized_requests_keep_tpm_per_window() {
        // tpm=100 (burst 10), two 60-token requests: the second must wait
        // a full window, not just for the bucket debt (39.6 s).
        let t0 = Instant::now();
        let mut g = Gate::new(&limits(1000, 100, 8), t0);
        g.try_send(t0, 60).unwrap();
        let Err(Blocked::Until(u)) = g.check(t0, 60) else {
            panic!("second request must wait");
        };
        assert_eq!(u, t0 + WINDOW);
        assert!(g.check(u - Duration::from_millis(1), 60).is_err());
        assert_eq!(g.try_send(u, 60), Ok(()));
    }

    proptest! {
        /// Simulated clock, random arrivals, a FIFO of waiting requests:
        /// sent requests never exceed RPM, TPM, or the in-flight cap in
        /// any window.
        #[test]
        fn sends_respect_rpm_tpm_and_in_flight(
            rpm in 1u64..120,
            tpm_factor in 10u64..100,
            cap in 1u32..8,
            arrivals in proptest::collection::vec(arrival(), 1..150),
        ) {
            let tpm = rpm * tpm_factor;
            let t0 = Instant::now();
            let mut g = Gate::new(&limits(rpm, tpm, cap), t0);
            // Pending arrivals (time, tokens, duration) and running ends.
            let mut queue: VecDeque<(Instant, u64, Duration)> = VecDeque::new();
            let mut t = t0;
            for a in &arrivals {
                t += Duration::from_millis(a.gap_ms);
                let tokens = ((a.size.powi(3) * tpm as f64).ceil() as u64).clamp(1, tpm);
                queue.push_back((t, tokens, Duration::from_millis(a.duration_ms)));
            }
            let mut running: Vec<(Instant, Instant)> = Vec::new(); // (sent, ends)
            let mut sent: Vec<(Instant, u64)> = Vec::new();
            let mut now = t0;
            let mut max_concurrent = 0usize;
            let mut steps = 0;
            while !queue.is_empty() || !running.is_empty() {
                steps += 1;
                prop_assert!(steps < 100_000, "simulation stuck");
                // Release requests that ended.
                running.retain(|(s, e)| {
                    if *e <= now {
                        g.release(now, *s, Outcome::Success);
                        false
                    } else {
                        true
                    }
                });
                if queue.is_empty() && running.is_empty() {
                    break;
                }
                let mut next = running.iter().map(|(_, e)| *e).min();
                if let Some(&(arrive, tokens, dur)) = queue.front() {
                    if arrive > now {
                        next = Some(next.map_or(arrive, |n| n.min(arrive)));
                    } else {
                        match g.try_send(now, tokens) {
                            Ok(()) => {
                                queue.pop_front();
                                running.push((now, now + dur));
                                sent.push((now, tokens));
                                max_concurrent = max_concurrent.max(running.len());
                                continue;
                            }
                            Err(Blocked::Until(u)) => {
                                next = Some(next.map_or(u, |n| n.min(u)));
                            }
                            Err(Blocked::InFlight) => {}
                        }
                    }
                }
                now = next.expect("something to wait for");
            }
            prop_assert!(max_concurrent <= cap as usize);
            prop_assert_eq!(sent.len(), arrivals.len());
            for (i, (start, _)) in sent.iter().enumerate() {
                let in_window = sent[i..].iter().take_while(|(t, _)| *t < *start + WINDOW);
                let (n, tokens) = in_window.fold((0u64, 0u64), |(n, s), (_, c)| (n + 1, s + c));
                prop_assert!(n <= rpm, "{} requests in a minute, rpm {}", n, rpm);
                prop_assert!(tokens <= tpm, "{} tokens in a minute, tpm {}", tokens, tpm);
            }
        }
    }
}
