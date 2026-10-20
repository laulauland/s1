//! A token bucket plus a log of the last minute's sends, so that no
//! 60-second window ever holds more than the limit.
//!
//! The bucket shapes the rate. A plain bucket with capacity `C` and refill
//! rate `R` lets `C + R * 60 s` through in a 60-second window. Here the
//! capacity (the burst) is a tenth of the limit and the refill fills the
//! rest: `C + R * 60 s = limit + 1`. For requests no larger than the burst,
//! that alone keeps every half-open window `[t, t + 60 s)` at or below the
//! limit.
//!
//! A request that costs more than the burst (a long context against a small
//! TPM) can never fit in the bucket. It goes when the bucket is full and
//! leaves the bucket in debt. The debt alone does not hold the bound: two
//! such requests can go less than 60 s apart. So every send is also put in
//! a log, and a request goes only when the sends in the last 60 s plus its
//! cost are at most the limit. If a send at `t` passes that check, the
//! window ending at `t` holds at most the limit; every window's last send
//! passed it, so no window holds more.
//!
//! A request that costs more than the limit itself cannot fit in any
//! window. It goes alone: only after 60 s with no sends and with a full
//! bucket. Its own window is over the limit, and nothing else goes in the
//! 60 s after it. Without that it would wait forever.
//!
//! Bucket arithmetic is exact: one token is [`UNIT`] units and the bucket
//! refills `limit + 1 - burst` units per nanosecond, so there is no
//! rounding drift. The log holds at most `limit` entries (each costs at
//! least one token; zero-cost sends are not logged).

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// The window the limit applies to.
pub const WINDOW: Duration = Duration::from_secs(60);
/// Units per token: nanoseconds per window.
const UNIT: i128 = WINDOW.as_nanos() as i128;
/// Burst capacity is `limit / BURST_DIVISOR` (at least 1 token).
pub const BURST_DIVISOR: u64 = 10;

#[derive(Debug, Clone)]
pub struct TokenBucket {
    limit: u64,
    burst: u64,
    /// Units added per nanosecond.
    refill: i128,
    /// Current units; negative after an oversized request.
    units: i128,
    last: Instant,
    /// Sends in the last [`WINDOW`]: (time, tokens), oldest first.
    log: VecDeque<(Instant, u64)>,
    /// Sum of the tokens in `log`.
    logged: u64,
}

impl TokenBucket {
    /// A full bucket for `limit` tokens per [`WINDOW`].
    pub fn new(limit: u64, now: Instant) -> Self {
        let limit = limit.max(1);
        let burst = (limit / BURST_DIVISOR).max(1);
        Self {
            limit,
            burst,
            refill: i128::from(limit + 1 - burst),
            units: i128::from(burst) * UNIT,
            last: now,
            log: VecDeque::new(),
            logged: 0,
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Most tokens available at once.
    pub fn burst(&self) -> u64 {
        self.burst
    }

    fn refill_to(&mut self, now: Instant) {
        while let Some(&(t, c)) = self.log.front() {
            if now.saturating_duration_since(t) < WINDOW {
                break;
            }
            self.log.pop_front();
            self.logged -= c;
        }
        if now <= self.last {
            return;
        }
        let ns = now.duration_since(self.last).as_nanos() as i128;
        let cap = i128::from(self.burst) * UNIT;
        self.units = self
            .units
            .saturating_add(ns.saturating_mul(self.refill))
            .min(cap);
        self.last = now;
    }

    /// Units a request of `cost` tokens needs before it may go.
    fn need(&self, cost: u64) -> i128 {
        i128::from(cost.min(self.burst)) * UNIT
    }

    /// How long until the log has room for `cost` tokens (zero if now).
    /// A cost above the limit needs an empty log.
    fn log_wait(&self, now: Instant, cost: u64) -> Duration {
        let cost = cost.min(self.limit);
        if cost == 0 || self.logged + cost <= self.limit {
            return Duration::ZERO;
        }
        let excess = self.logged + cost - self.limit;
        let mut freed = 0;
        for &(t, c) in &self.log {
            freed += c;
            if freed >= excess {
                return (t + WINDOW).saturating_duration_since(now);
            }
        }
        // Unreachable: the whole log frees `logged >= excess` tokens.
        WINDOW
    }

    /// `None` if `cost` tokens can go now, else how long until they can.
    pub fn wait(&mut self, now: Instant, cost: u64) -> Option<Duration> {
        self.refill_to(now);
        let missing = self.need(cost) - self.units;
        let bucket = if missing <= 0 {
            Duration::ZERO
        } else {
            let ns = (missing + self.refill - 1) / self.refill;
            Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
        };
        let wait = bucket.max(self.log_wait(now, cost));
        (!wait.is_zero()).then_some(wait)
    }

    /// Spend `cost` tokens. Call only after [`TokenBucket::wait`] gave `None`.
    pub fn take(&mut self, now: Instant, cost: u64) {
        self.refill_to(now);
        self.units -= i128::from(cost) * UNIT;
        if cost > 0 {
            self.log.push_back((now, cost));
            self.logged += cost;
        }
    }

    /// Whole tokens available now (0 when in debt).
    pub fn available(&mut self, now: Instant) -> u64 {
        self.refill_to(now);
        u64::try_from((self.units / UNIT).max(0)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn small_limits_still_refill() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1, t0);
        assert_eq!(b.wait(t0, 1), None);
        b.take(t0, 1);
        let w = b.wait(t0, 1).unwrap();
        assert!(w <= WINDOW, "{w:?}");
        assert_eq!(b.wait(t0 + WINDOW, 1), None);
    }

    #[test]
    fn oversized_cost_goes_when_full_then_waits() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(100, t0);
        assert_eq!(b.burst(), 10);
        assert_eq!(b.wait(t0, 500), None, "full bucket lets a big request go");
        b.take(t0, 500);
        assert!(b.wait(t0, 1).is_some(), "then the debt must be paid");
        assert_eq!(b.available(t0), 0);
    }

    /// Sends `costs` as early as the bucket allows, from `t0`.
    fn send_all(b: &mut TokenBucket, t0: Instant, costs: &[u64]) -> Vec<(Instant, u64)> {
        let mut now = t0;
        let mut sent = Vec::new();
        for &cost in costs {
            if let Some(w) = b.wait(now, cost) {
                now += w;
                assert_eq!(b.wait(now, cost), None, "the wait was too short");
            }
            b.take(now, cost);
            sent.push((now, cost));
        }
        sent
    }

    /// Most tokens in any window `[t, t + 60 s)` that starts at a send.
    fn max_window(sent: &[(Instant, u64)]) -> u64 {
        (0..sent.len())
            .map(|i| {
                sent[i..]
                    .iter()
                    .take_while(|(t, _)| *t < sent[i].0 + WINDOW)
                    .map(|(_, c)| c)
                    .sum()
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn oversized_requests_never_share_an_overfull_window() {
        // tpm=100, burst=10, each request 60 tokens: the bucket debt alone
        // let the second one go at t=39.6 s (120 tokens in a minute).
        let t0 = Instant::now();
        let mut b = TokenBucket::new(100, t0);
        let sent = send_all(&mut b, t0, &[60, 60, 60]);
        assert_eq!(max_window(&sent), 60);
        assert!(
            sent[1].0 - sent[0].0 >= WINDOW,
            "{:?}",
            sent[1].0 - sent[0].0
        );
    }

    #[test]
    fn small_requests_wait_behind_an_oversized_one() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(100, t0);
        let sent = send_all(&mut b, t0, &[95, 10, 10]);
        assert!(max_window(&sent) <= 100, "{sent:?}");
    }

    #[test]
    fn cost_above_the_limit_goes_alone() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(100, t0);
        b.take(t0, 1);
        let w = b.wait(t0, 500).expect("waits for an empty window");
        assert_eq!(w, WINDOW);
        let t1 = t0 + w;
        assert_eq!(b.wait(t1, 500), None);
        b.take(t1, 500);
        let almost = t1 + WINDOW - Duration::from_nanos(1);
        assert!(b.wait(almost, 1).is_some());
        let sent = send_all(&mut b, almost, &[1]);
        assert!(sent[0].0 >= t1 + WINDOW, "nothing else in its window");
    }

    proptest! {
        /// Sends at random times never exceed the limit in any 60 s window,
        /// for costs up to the limit (above the burst too).
        #[test]
        fn no_window_exceeds_the_limit(
            limit in 1u64..200,
            gaps in proptest::collection::vec(0u64..3_000, 1..400),
            fractions in proptest::collection::vec(0.0f64..=1.0, 1..400),
        ) {
            let t0 = Instant::now();
            let mut b = TokenBucket::new(limit, t0);
            let mut now = t0;
            let mut sent: Vec<(Instant, u64)> = Vec::new();
            for (i, gap) in gaps.iter().enumerate() {
                now += Duration::from_millis(*gap);
                let f = fractions[i % fractions.len()];
                // Mostly small costs, some up to the whole limit.
                let cost = ((f * f * f * limit as f64).ceil() as u64).clamp(1, limit);
                if let Some(w) = b.wait(now, cost) {
                    now += w;
                    prop_assert_eq!(b.wait(now, cost), None);
                }
                b.take(now, cost);
                sent.push((now, cost));
            }
            let used = max_window(&sent);
            prop_assert!(used <= limit, "{} tokens in a window, limit {}", used, limit);
        }
    }
}
