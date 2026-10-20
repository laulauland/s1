//! Async admission for one backend and key: a request waits in the
//! [`Queue`] until it is at the head and the [`Gate`] lets it go, then holds
//! a [`Permit`] while it is in flight.
//!
//! Waiters sleep on one [`Notify`]; every state change (a release, a
//! departure, a new pause) wakes them, and the head re-checks the gate.
//! A dropped waiter (the client went away) leaves the queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{Bounds, Lane, Queue, Ticket};
use crate::breaker::{self, Breaker};
use crate::error::S1Error;
use crate::limits::aimd::Change;
use crate::limits::headers::rfc3339;
use crate::limits::{Blocked, Gate, Limits, Outcome, RateHint};
use crate::retry::Verdict;

/// Completions kept for the drain rate.
const DRAIN_WINDOW: Duration = Duration::from_secs(60);
/// Longest `Retry-After` s1 suggests for a full queue.
const MAX_RETRY_AFTER_SECS: u64 = 3600;

pub struct Scheduler {
    provider: String,
    key_id: String,
    limits: Limits,
    created: Instant,
    inner: Mutex<Inner>,
    notify: Notify,
}

struct Inner {
    gate: Gate,
    queue: Queue,
    breaker: Breaker,
    next_ticket: Ticket,
    /// Completion times in the last [`DRAIN_WINDOW`].
    done: VecDeque<Instant>,
}

impl Scheduler {
    /// `key_id` names the API key without revealing it (a hash prefix).
    pub fn new(provider: &str, key_id: &str, limits: Limits, bounds: Bounds) -> Arc<Self> {
        let now = Instant::now();
        let breaker = Breaker::new(
            limits.breaker_failures,
            Duration::from_millis(limits.breaker_window_ms),
            Duration::from_millis(limits.breaker_cooldown_ms),
        );
        Arc::new(Self {
            provider: provider.to_string(),
            key_id: key_id.to_string(),
            created: now,
            inner: Mutex::new(Inner {
                gate: Gate::new(&limits, now),
                queue: Queue::new(bounds),
                breaker,
                next_ticket: 0,
                done: VecDeque::new(),
            }),
            limits,
            notify: Notify::new(),
        })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for a turn to send `cost` input tokens. `retry` marks a retry of
    /// an admitted request: it goes to the front of its lane and is not
    /// bounded. Fails with `queue_full` when the queue is full, and with
    /// `backend_unavailable` while the breaker is open.
    pub async fn acquire(
        self: &Arc<Self>,
        lane: Lane,
        cost: u64,
        retry: bool,
    ) -> Result<Permit, S1Error> {
        let ticket = {
            let mut st = self.lock();
            let now = Instant::now();
            if let Some(until) = st.breaker.open_until(now) {
                return Err(self.breaker_error(now, until));
            }
            let t = st.next_ticket;
            st.next_ticket += 1;
            if retry {
                st.queue.push_front(lane, t, cost);
            } else if st.queue.push(lane, t, cost).is_err() {
                let secs = self.retry_after_secs(&mut st, now);
                let depth = st.queue.len();
                drop(st);
                tracing::warn!(
                    event = "queue_full",
                    backend = %self.provider,
                    key = %self.key_id,
                    lane = lane.as_str(),
                    depth,
                    retry_after_secs = secs,
                    "queue full"
                );
                return Err(S1Error::QueueFull {
                    message: format!(
                        "the queue for `{}` is full ({depth} requests waiting); retry in {secs} s",
                        self.provider
                    ),
                    retry_after_secs: secs,
                });
            }
            t
        };
        let mut waiting = Waiting {
            sched: self,
            ticket,
            active: true,
        };
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut st = self.lock();
                let now = Instant::now();
                if st.queue.head() != Some(ticket) {
                    None
                } else {
                    if let Err(until) = st.breaker.allow(now) {
                        st.queue.remove(ticket);
                        waiting.active = false;
                        drop(st);
                        self.notify.notify_waiters();
                        return Err(self.breaker_error(now, until));
                    }
                    match st.gate.try_send(now, cost) {
                        Ok(()) => {
                            st.queue.pop_head();
                            waiting.active = false;
                            drop(st);
                            // The next head may go too.
                            self.notify.notify_waiters();
                            return Ok(Permit {
                                sched: self.clone(),
                                sent_at: now,
                                done: false,
                            });
                        }
                        Err(Blocked::InFlight) => None,
                        Err(Blocked::Until(u)) => Some(u),
                    }
                }
            };
            match wait {
                Some(u) => {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(u) => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    fn breaker_error(&self, now: Instant, until: Instant) -> S1Error {
        let secs = until.duration_since(now).as_secs_f64().ceil().max(1.0) as u64;
        S1Error::BreakerOpen {
            message: format!(
                "`{}` failed repeatedly; its circuit breaker is open for {secs} s more",
                self.provider
            ),
            retry_after_secs: secs,
        }
    }

    /// Seconds until the queue has drained, from the current drain rate.
    fn retry_after_secs(&self, st: &mut Inner, now: Instant) -> u64 {
        trim_done(&mut st.done, now);
        let elapsed = now.duration_since(self.created).min(DRAIN_WINDOW);
        let observed = if st.done.is_empty() || elapsed.is_zero() {
            0.0
        } else {
            st.done.len() as f64 / elapsed.as_secs_f64().max(1.0)
        };
        let rate = if observed > 0.0 {
            observed
        } else if let Some(rpm) = self.limits.rpm {
            rpm as f64 / 60.0
        } else {
            f64::from(st.gate.cap())
        };
        let pause = st
            .gate
            .throttled_until(now)
            .map_or(0.0, |u| (u - now).as_secs_f64());
        let secs = (st.queue.len() as f64 + 1.0) / rate.max(1e-3) + pause;
        (secs.ceil() as u64).clamp(1, MAX_RETRY_AFTER_SECS)
    }

    /// True if a new request would wait: a provider pause, a queue, or no
    /// room under the limits. Aliases with `spill = true` skip such entries.
    pub fn throttled(&self) -> bool {
        let mut st = self.lock();
        let now = Instant::now();
        st.gate.throttled_until(now).is_some()
            || !st.queue.is_empty()
            || st.gate.check(now, 1).is_err()
    }

    /// The earliest time a new request could start (provider pause, RPM,
    /// TPM), as wall-clock time; `None` if it could start now.
    pub fn throttled_until(&self) -> Option<SystemTime> {
        let mut st = self.lock();
        let now = Instant::now();
        st.gate.throttled_until(now).map(|u| wall(now, u))
    }

    /// `Err(breaker_open error)` while the breaker stops requests.
    pub fn check_breaker(&self) -> Result<(), S1Error> {
        let now = Instant::now();
        let until = self.lock().breaker.open_until(now);
        match until {
            Some(u) => Err(self.breaker_error(now, u)),
            None => Ok(()),
        }
    }

    pub fn breaker_open(&self) -> bool {
        self.lock().breaker.open_until(Instant::now()).is_some()
    }

    /// Current in-flight cap (AIMD).
    pub fn cap(&self) -> u32 {
        self.lock().gate.cap()
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut st = self.lock();
        let now = Instant::now();
        trim_done(&mut st.done, now);
        let (requests, tokens) = st.gate.rate(now);
        let breaker_state = st.breaker.state(now);
        let open_until = st.breaker.open_until(now).map(|u| rfc3339(wall(now, u)));
        let recent_failures = st.breaker.recent_failures(now);
        Snapshot {
            provider: self.provider.clone(),
            key: self.key_id.clone(),
            queue: QueueState {
                interactive: st.queue.depth(Lane::Interactive),
                bulk: st.queue.depth(Lane::Bulk),
                tokens: st.queue.tokens(),
                max_requests: st.queue.bounds().max_requests,
                max_tokens: st.queue.bounds().max_tokens,
            },
            in_flight: st.gate.in_flight(),
            cap: st.gate.cap(),
            rate: RateState {
                requests_last_minute: requests,
                tokens_last_minute: tokens,
                completed_last_minute: st.done.len() as u64,
            },
            limits: LimitState {
                rpm: self.limits.rpm,
                tpm: self.limits.tpm,
                max_in_flight: self.limits.max_in_flight,
                adaptive: self.limits.adaptive,
            },
            throttled_until: st.gate.throttled_until(now).map(|u| rfc3339(wall(now, u))),
            breaker: BreakerState {
                state: breaker_state,
                open_until,
                recent_failures,
            },
        }
    }

    fn finish(&self, sent_at: Instant, verdict: Option<Verdict>, hint: &RateHint) {
        let mut st = self.lock();
        let now = Instant::now();
        let outcome = match verdict {
            Some(Verdict::Success) => Outcome::Success,
            Some(Verdict::RateLimited) => Outcome::RateLimited,
            _ => Outcome::Other,
        };
        // Release first: nothing after this may leak the in-flight slot.
        let change = st.gate.release(now, sent_at, outcome);
        if let Some(until) = st.gate.observe(now, hint) {
            tracing::info!(
                event = "throttled",
                backend = %self.provider,
                key = %self.key_id,
                pause_ms = until.duration_since(now).as_millis() as u64,
                "provider asked s1 to pause"
            );
        }
        let event = match verdict {
            Some(Verdict::Failure) => st.breaker.record_failure(now),
            Some(Verdict::Success | Verdict::Final) => st.breaker.record_success(now),
            Some(Verdict::RateLimited) | None => None,
        };
        if verdict.is_some() {
            st.done.push_back(now);
            trim_done(&mut st.done, now);
        }
        drop(st);
        match change {
            Change::Down { from, to } | Change::Up { from, to } => tracing::info!(
                event = "limit_change",
                backend = %self.provider,
                key = %self.key_id,
                from,
                to,
                "in-flight cap changed"
            ),
            Change::None => {}
        }
        match event {
            Some(breaker::Event::Opened { until }) => tracing::warn!(
                event = "breaker_open",
                backend = %self.provider,
                key = %self.key_id,
                cooldown_ms = until.duration_since(now).as_millis() as u64,
                "circuit breaker opened"
            ),
            Some(breaker::Event::Closed) => tracing::info!(
                event = "breaker_close",
                backend = %self.provider,
                key = %self.key_id,
                "circuit breaker closed"
            ),
            None => {}
        }
        self.notify.notify_waiters();
    }
}

fn trim_done(done: &mut VecDeque<Instant>, now: Instant) {
    while done
        .front()
        .is_some_and(|t| now.duration_since(*t) >= DRAIN_WINDOW)
    {
        done.pop_front();
    }
}

/// Wall-clock time of a future `Instant`.
fn wall(now: Instant, at: Instant) -> SystemTime {
    let now_wall = SystemTime::now();
    now_wall
        .checked_add(at.saturating_duration_since(now))
        .unwrap_or(now_wall)
}

/// Removes a waiter that gave up (dropped future) from the queue.
struct Waiting<'a> {
    sched: &'a Arc<Scheduler>,
    ticket: Ticket,
    active: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.active {
            self.sched.lock().queue.remove(self.ticket);
            self.sched.notify.notify_waiters();
        }
    }
}

/// A request in flight. Report how it ended with [`Permit::finish`];
/// dropping it releases the slot without an AIMD or breaker signal.
pub struct Permit {
    sched: Arc<Scheduler>,
    sent_at: Instant,
    done: bool,
}

impl Permit {
    pub fn finish(mut self, verdict: Verdict, hint: &RateHint) {
        self.done = true;
        self.sched.finish(self.sent_at, Some(verdict), hint);
    }

    pub fn sent_at(&self) -> Instant {
        self.sent_at
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.done {
            self.sched.finish(self.sent_at, None, &RateHint::default());
        }
    }
}

/// One backend+key in `GET /status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Snapshot {
    pub provider: String,
    /// Hash prefix of the API key (never the key).
    pub key: String,
    pub queue: QueueState,
    pub in_flight: u32,
    /// Current in-flight cap (AIMD).
    pub cap: u32,
    pub rate: RateState,
    pub limits: LimitState,
    pub throttled_until: Option<String>,
    pub breaker: BreakerState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QueueState {
    pub interactive: usize,
    pub bulk: usize,
    pub tokens: u64,
    pub max_requests: usize,
    pub max_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RateState {
    pub requests_last_minute: u64,
    pub tokens_last_minute: u64,
    pub completed_last_minute: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LimitState {
    pub rpm: Option<u64>,
    pub tpm: Option<u64>,
    pub max_in_flight: u32,
    pub adaptive: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BreakerState {
    pub state: breaker::State,
    pub open_until: Option<String>,
    pub recent_failures: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn limits(cap: u32) -> Limits {
        Limits {
            max_in_flight: cap,
            ..Limits::default()
        }
    }

    fn bounds(n: usize) -> Bounds {
        Bounds {
            max_requests: n,
            max_tokens: 1_000_000,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_queue_gives_queue_full_with_retry_after() {
        let s = Scheduler::new("p", "k", limits(1), bounds(1));
        let p1 = s.acquire(Lane::Interactive, 1, false).await.unwrap();
        let s2 = s.clone();
        let waiter =
            tokio::spawn(async move { s2.acquire(Lane::Interactive, 1, false).await.map(|_| ()) });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        let err = s.acquire(Lane::Interactive, 1, false).await.err().unwrap();
        assert_eq!(err.code(), "queue_full");
        assert!(err.retry_after().unwrap() >= 1);
        drop(p1);
        waiter.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_goes_before_queued_bulk() {
        let s = Scheduler::new("p", "k", limits(1), bounds(100));
        let first = s.acquire(Lane::Bulk, 1, false).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (i, lane) in [Lane::Bulk, Lane::Bulk, Lane::Interactive]
            .into_iter()
            .enumerate()
        {
            let s = s.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                let p = s.acquire(lane, 1, false).await.unwrap();
                order.lock().unwrap().push(i);
                tokio::time::sleep(Duration::from_millis(10)).await;
                p.finish(Verdict::Success, &RateHint::default());
            }));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(first);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![2, 0, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_pause_holds_requests() {
        let s = Scheduler::new("p", "k", limits(4), bounds(10));
        let p = s.acquire(Lane::Interactive, 1, false).await.unwrap();
        let hint = RateHint {
            retry_after: Some(Duration::from_secs(5)),
            exhausted_for: None,
        };
        p.finish(Verdict::RateLimited, &hint);
        assert!(s.throttled());
        assert!(s.throttled_until().is_some());
        assert_eq!(s.cap(), 2, "429 halves the cap");
        let t0 = Instant::now();
        let _p = s.acquire(Lane::Interactive, 1, false).await.unwrap();
        assert!(Instant::now() - t0 >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_opens_after_failures_and_fails_fast() {
        let l = Limits {
            breaker_failures: 2,
            ..limits(4)
        };
        let s = Scheduler::new("p", "k", l, bounds(10));
        for _ in 0..2 {
            let p = s.acquire(Lane::Interactive, 1, false).await.unwrap();
            p.finish(Verdict::Failure, &RateHint::default());
        }
        assert!(s.breaker_open());
        let err = s.acquire(Lane::Interactive, 1, false).await.err().unwrap();
        assert_eq!(err.code(), "backend_unavailable");
        assert!(matches!(err, S1Error::BreakerOpen { .. }));
        assert_eq!(s.snapshot().breaker.state, breaker::State::Open);
        tokio::time::sleep(Duration::from_millis(l_cooldown())).await;
        assert!(!s.breaker_open());
        assert!(s.acquire(Lane::Interactive, 1, false).await.is_ok());
    }

    fn l_cooldown() -> u64 {
        Limits::default().breaker_cooldown_ms
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_waiters_leave_the_queue() {
        let s = Scheduler::new("p", "k", limits(1), bounds(10));
        let p = s.acquire(Lane::Interactive, 1, false).await.unwrap();
        let s2 = s.clone();
        let w = tokio::spawn(async move { s2.acquire(Lane::Bulk, 1, false).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(s.snapshot().queue.bulk, 1);
        w.abort();
        let _ = w.await;
        assert_eq!(s.snapshot().queue.bulk, 0);
        drop(p);
        assert_eq!(s.snapshot().in_flight, 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        /// Every accepted request ends with exactly one permit or error;
        /// in flight never exceeds the cap; the queue never exceeds its bound.
        #[test]
        fn every_request_gets_one_result(
            cap in 1u32..5,
            max_requests in 1usize..8,
            jobs in proptest::collection::vec((any::<bool>(), 0u64..30, 1u64..50), 1..40),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .unwrap();
            rt.block_on(async move {
                let s = Scheduler::new("p", "k", limits(cap), bounds(max_requests));
                let in_flight = Arc::new(AtomicUsize::new(0));
                let max_seen = Arc::new(AtomicUsize::new(0));
                let mut tasks = Vec::new();
                for (bulk, gap, dur) in jobs.clone() {
                    tokio::time::sleep(Duration::from_millis(gap)).await;
                    let lane = if bulk { Lane::Bulk } else { Lane::Interactive };
                    let (s2, in_flight, max_seen) = (s.clone(), in_flight.clone(), max_seen.clone());
                    tasks.push(tokio::spawn(async move {
                        match s2.acquire(lane, 1, false).await {
                            Ok(p) => {
                                let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                                max_seen.fetch_max(n, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(dur)).await;
                                in_flight.fetch_sub(1, Ordering::SeqCst);
                                p.finish(Verdict::Success, &RateHint::default());
                                Ok(())
                            }
                            Err(e) => Err(e.code()),
                        }
                    }));
                    let snap = s.snapshot();
                    assert!(snap.queue.interactive + snap.queue.bulk <= max_requests);
                }
                let mut results = 0;
                for t in tasks {
                    match t.await.unwrap() {
                        Ok(()) | Err("queue_full") => results += 1,
                        Err(other) => panic!("unexpected {other}"),
                    }
                }
                assert_eq!(results, jobs.len());
                assert!(max_seen.load(Ordering::SeqCst) <= cap as usize);
                let snap = s.snapshot();
                assert_eq!(snap.in_flight, 0);
                assert_eq!(snap.queue.interactive + snap.queue.bulk, 0);
            });
        }

        /// Lanes: an interactive request never waits behind more than the
        /// bulk requests already in flight. No bulk request starts while an
        /// interactive request is waiting.
        #[test]
        fn no_bulk_request_starts_while_an_interactive_one_waits(
            cap in 1u32..4,
            jobs in proptest::collection::vec((any::<bool>(), 0u64..20, 1u64..40), 1..40),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .unwrap();
            let events = rt.block_on(async move {
                let s = Scheduler::new("p", "k", limits(cap), bounds(1000));
                let clock = Arc::new(AtomicUsize::new(0));
                let mut tasks = Vec::new();
                for (bulk, gap, dur) in jobs {
                    tokio::time::sleep(Duration::from_millis(gap)).await;
                    let lane = if bulk { Lane::Bulk } else { Lane::Interactive };
                    let (s2, clock) = (s.clone(), clock.clone());
                    tasks.push(tokio::spawn(async move {
                        // No await between this tick and the push in acquire.
                        let arrived = clock.fetch_add(1, Ordering::SeqCst);
                        let p = s2.acquire(lane, 1, false).await.unwrap();
                        let started = clock.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(dur)).await;
                        p.finish(Verdict::Success, &RateHint::default());
                        (lane, arrived, started)
                    }));
                }
                let mut events = Vec::new();
                for t in tasks {
                    events.push(t.await.unwrap());
                }
                events
            });
            for (la, arrived, started) in &events {
                if *la != Lane::Interactive {
                    continue;
                }
                for (lb, _, b_started) in &events {
                    if *lb == Lane::Bulk {
                        prop_assert!(
                            !(arrived < b_started && b_started < started),
                            "bulk started at {} while interactive waited {}..{}",
                            b_started, arrived, started
                        );
                    }
                }
            }
        }
    }
}
