//! Protection per backend and API key: one [`Scheduler`] (queue, limits,
//! AIMD, breaker) per pair, and [`call`], which sends a request through it
//! with retries.
//!
//! Keys are named by a hash prefix ([`key_id`]), never by value.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use crate::backend::{Backend, BackendRequest, ProviderResponse};
use crate::catalog::Catalog;
use crate::error::S1Error;
use crate::limits::{Limits, RateHint};
use crate::queue::{Bounds, Lane, Scheduler};
use crate::retry::{Policy, Verdict, jitter};

/// Short, stable name of an API key: `k_` + 8 hex digits of its blake3 hash.
pub fn key_id(key: &str) -> String {
    let h = blake3::hash(key.as_bytes());
    format!("k_{}", &h.to_hex()[..8])
}

/// The schedulers of one provider, one per key.
type KeyGuards = Mutex<BTreeMap<String, Arc<Scheduler>>>;

/// All schedulers, created on first use.
pub struct Protection {
    limits: BTreeMap<String, Limits>,
    bounds: Bounds,
    /// Per provider. A reload shares the map of a provider whose limits did
    /// not change, so a scheduler made later through either snapshot is the
    /// same one.
    guards: Mutex<BTreeMap<String, Arc<KeyGuards>>>,
}

impl Protection {
    /// Limits per provider from the catalog's `[limits.*]` tables (already
    /// validated when the catalog loaded); defaults for the rest.
    pub fn new(catalog: &Catalog, bounds: Bounds) -> Self {
        let limits: BTreeMap<String, Limits> = catalog
            .providers
            .keys()
            .map(|p| (p.clone(), catalog.limits_for(p).unwrap_or_default()))
            .collect();
        let guards = limits.keys().map(|p| (p.clone(), Arc::default())).collect();
        Self {
            limits,
            bounds,
            guards: Mutex::new(guards),
        }
    }

    /// Protection for a reloaded catalog: the schedulers (queue, AIMD rate,
    /// breaker) of providers whose limits did not change carry over, also
    /// ones made after the reload through the old snapshot; the rest start
    /// fresh on first use. Requests already queued on a dropped scheduler
    /// finish there.
    pub fn reloaded(&self, catalog: &Catalog) -> Self {
        let next = Self::new(catalog, self.bounds);
        {
            let old = self.guards.lock().unwrap_or_else(|e| e.into_inner());
            let mut new = next.guards.lock().unwrap_or_else(|e| e.into_inner());
            for (provider, keys) in old.iter() {
                if self.limits.contains_key(provider)
                    && self.limits.get(provider) == next.limits.get(provider)
                {
                    new.insert(provider.clone(), keys.clone());
                }
            }
        }
        next
    }

    pub fn limits(&self, provider: &str) -> Limits {
        self.limits.get(provider).cloned().unwrap_or_default()
    }

    fn keys(&self, provider: &str) -> Arc<KeyGuards> {
        let mut g = self.guards.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(provider.to_string()).or_default().clone()
    }

    /// The scheduler for a provider and key, created on first use.
    pub fn guard(&self, provider: &str, key: &str) -> Arc<Scheduler> {
        let id = key_id(key);
        let keys = self.keys(provider);
        let mut g = keys.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(id.clone())
            .or_insert_with(|| Scheduler::new(provider, &id, self.limits(provider), self.bounds))
            .clone()
    }

    /// The scheduler for a provider and key, if it exists.
    pub fn existing(&self, provider: &str, key: &str) -> Option<Arc<Scheduler>> {
        let keys = self.keys(provider);
        let g = keys.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&key_id(key)).cloned()
    }

    /// Every scheduler made so far.
    pub fn all(&self) -> Vec<Arc<Scheduler>> {
        let providers: Vec<Arc<KeyGuards>> = {
            let g = self.guards.lock().unwrap_or_else(|e| e.into_inner());
            g.values().cloned().collect()
        };
        providers
            .iter()
            .flat_map(|keys| {
                let g = keys.lock().unwrap_or_else(|e| e.into_inner());
                g.values().cloned().collect::<Vec<_>>()
            })
            .collect()
    }
}

/// What happened during a guarded call (for logs and headers).
#[derive(Debug, Clone, Copy, Default)]
pub struct CallStats {
    /// Attempts after the first.
    pub retries: u32,
    /// Time spent waiting in the queue (all attempts).
    pub queue_wait: Duration,
}

/// Send `req` through `guard` with retries: at most `max_attempts`
/// attempts, only after 429, 5xx, or a connection error; exponential
/// backoff with jitter, and never earlier than `Retry-After`. Returns the
/// last provider response unchanged (also a final 429 or 5xx).
pub async fn call(
    guard: &Arc<Scheduler>,
    backend: &dyn Backend,
    req: BackendRequest,
    lane: Lane,
    cost: u64,
) -> (Result<ProviderResponse, S1Error>, CallStats) {
    let policy = Policy::from_limits(guard.limits());
    let mut stats = CallStats::default();
    let mut attempt = 1u32;
    loop {
        let waited = Instant::now();
        let permit = match guard.acquire(lane, cost, attempt > 1).await {
            Ok(p) => p,
            Err(e) => return (Err(e), stats),
        };
        stats.queue_wait += permit.sent_at().duration_since(waited);
        let result = backend.call(req.clone()).await;
        let verdict = Verdict::of(&result);
        let hint = match &result {
            Ok(r) => RateHint::from_headers(&r.headers, SystemTime::now()),
            Err(_) => RateHint::default(),
        };
        permit.finish(verdict, &hint);
        // The gate pauses for `hint.pause()` (Retry-After and quota headers):
        // a pause past `retry_after_max` ends the retries at once.
        let too_long_a_pause = hint.pause().is_some_and(|d| d > policy.retry_after_max);
        if !verdict.retryable() || attempt >= policy.max_attempts || too_long_a_pause {
            return (result, stats);
        }
        let status = match &result {
            Ok(r) => r.status.as_u16().to_string(),
            Err(e) => e.code().to_string(),
        };
        let delay = policy.backoff(attempt, jitter());
        tracing::info!(
            event = "retry",
            backend = %guard.provider(),
            attempt,
            status = %status,
            delay_ms = delay.as_millis() as u64,
            retry_after_ms = hint.retry_after.map(|d| d.as_millis() as u64),
            "retrying"
        );
        // `Retry-After` is enforced by the gate: the next acquire waits for it.
        tokio::time::sleep(delay).await;
        attempt += 1;
        stats.retries += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::protocol::ProtocolId;
    use async_trait::async_trait;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Answers from a script of statuses (the last one repeats).
    struct Scripted {
        script: Vec<Result<u16, ()>>,
        calls: AtomicU32,
        retry_after: Option<&'static str>,
        /// Extra headers sent with each 429.
        headers_429: Vec<(&'static str, &'static str)>,
    }

    #[async_trait]
    impl Backend for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn protocols(&self) -> &[ProtocolId] {
            &[ProtocolId::Systemone]
        }
        fn caps(&self, _: &str) -> Option<&Model> {
            None
        }
        async fn call(&self, _: BackendRequest) -> Result<ProviderResponse, S1Error> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
            let step = self.script[n.min(self.script.len() - 1)];
            match step {
                Err(()) => Err(S1Error::Unreachable("connection reset".into())),
                Ok(code) => {
                    let mut headers = HeaderMap::new();
                    if code == 429
                        && let Some(ra) = self.retry_after
                    {
                        headers.insert("retry-after", HeaderValue::from_static(ra));
                    }
                    if code == 429 {
                        for (k, v) in &self.headers_429 {
                            headers.insert(*k, HeaderValue::from_static(v));
                        }
                    }
                    Ok(ProviderResponse {
                        status: StatusCode::from_u16(code).unwrap(),
                        headers,
                        body: Bytes::new(),
                    })
                }
            }
        }
    }

    fn scripted(script: Vec<Result<u16, ()>>) -> Scripted {
        Scripted {
            script,
            calls: AtomicU32::new(0),
            retry_after: None,
            headers_429: Vec::new(),
        }
    }

    fn req() -> BackendRequest {
        BackendRequest {
            upstream_model: "m".into(),
            body: Bytes::from_static(b"{}"),
            headers: HeaderMap::new(),
            credential: None,
        }
    }

    fn guard() -> Arc<Scheduler> {
        Scheduler::new("p", "k", Limits::default(), Bounds::default())
    }

    #[tokio::test(start_paused = true)]
    async fn retries_5xx_and_connection_errors_then_succeeds() {
        let b = scripted(vec![Ok(503), Err(()), Ok(200)]);
        let (res, stats) = call(&guard(), &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 200);
        assert_eq!(stats.retries, 2);
        assert_eq!(b.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_four_attempts() {
        let b = scripted(vec![Ok(500)]);
        let (res, stats) = call(&guard(), &b, req(), Lane::Interactive, 1).await;
        assert_eq!(
            res.unwrap().status,
            500,
            "last provider response, unchanged"
        );
        assert_eq!(stats.retries, 3);
        assert_eq!(b.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn no_retry_on_4xx() {
        for code in [400, 401, 403, 413] {
            let b = scripted(vec![Ok(code)]);
            let (res, _) = call(&guard(), &b, req(), Lane::Interactive, 1).await;
            assert_eq!(res.unwrap().status, code);
            assert_eq!(b.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_retry_after_on_429() {
        let mut b = scripted(vec![Ok(429), Ok(200)]);
        b.retry_after = Some("3");
        let t0 = Instant::now();
        let g = guard();
        let (res, _) = call(&g, &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 200);
        assert!(Instant::now() - t0 >= Duration::from_secs(3));
        assert_eq!(g.cap(), Limits::default().max_in_flight / 2);
    }

    #[tokio::test(start_paused = true)]
    async fn long_retry_after_is_returned_not_waited() {
        let mut b = scripted(vec![Ok(429), Ok(200)]);
        b.retry_after = Some("3600");
        let g = guard();
        let (res, _) = call(&g, &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 429);
        assert_eq!(b.calls.load(Ordering::SeqCst), 1);
        assert!(g.throttled_until().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn long_cloudflare_ratelimit_pause_is_returned_not_waited() {
        let mut b = scripted(vec![Ok(429), Ok(200)]);
        b.headers_429 = vec![("ratelimit", "\"default\";r=0;t=3600")];
        let g = guard();
        let t0 = Instant::now();
        let (res, stats) = call(&g, &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 429);
        assert_eq!(b.calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.retries, 0);
        assert!(Instant::now() - t0 < Duration::from_secs(1));
        assert!(g.throttled_until().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn long_openai_reset_pause_is_returned_not_waited() {
        let mut b = scripted(vec![Ok(429), Ok(200)]);
        b.headers_429 = vec![
            ("x-ratelimit-remaining-tokens", "0"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ];
        let (res, _) = call(&guard(), &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 429);
        assert_eq!(b.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn short_quota_pause_is_waited_then_retried() {
        let mut b = scripted(vec![Ok(429), Ok(200)]);
        b.headers_429 = vec![("ratelimit", "\"default\";r=0;t=2")];
        let t0 = Instant::now();
        let (res, _) = call(&guard(), &b, req(), Lane::Interactive, 1).await;
        assert_eq!(res.unwrap().status, 200);
        assert_eq!(b.calls.load(Ordering::SeqCst), 2);
        assert!(Instant::now() - t0 >= Duration::from_secs(2));
    }

    /// Extreme rate-limit headers neither panic nor leak the in-flight slot:
    /// the pause is capped and the next request still gets a turn.
    #[tokio::test(start_paused = true)]
    async fn extreme_rate_limit_headers_are_capped_and_release_the_slot() {
        let extremes: [(&'static str, &'static str); 5] = [
            ("retry-after", "1e300"),
            ("retry-after", "10000000000000000000"),
            ("retry-after-ms", "1e300"),
            ("ratelimit", "\"default\";r=0;t=18446744073709551615"),
            ("x-ratelimit-reset-requests", "99999999999999999999h"),
        ];
        for (k, v) in extremes {
            let mut b = scripted(vec![Ok(429), Ok(200)]);
            b.headers_429 = vec![(k, v), ("x-ratelimit-remaining-requests", "0")];
            let limits = Limits {
                max_in_flight: 1,
                ..Limits::default()
            };
            let g = Scheduler::new("p", "k", limits, Bounds::default());
            let g2 = g.clone();
            let task = tokio::spawn(async move {
                let (res, _) = call(&g2, &b, req(), Lane::Interactive, 1).await;
                res.map(|r| r.status.as_u16())
            });
            assert_eq!(task.await.expect("no panic").unwrap(), 429, "{k}: {v}");
            assert_eq!(g.snapshot().in_flight, 0, "{k}: {v}");
            let until = g.throttled_until().expect("throttled");
            let max = SystemTime::now() + crate::limits::headers::MAX_PAUSE;
            assert!(until <= max + Duration::from_secs(1), "{k}: {v}");
            // The next request waits out the capped pause, then goes.
            let b2 = scripted(vec![Ok(200)]);
            let next = tokio::time::timeout(
                crate::limits::headers::MAX_PAUSE + Duration::from_secs(60),
                call(&g, &b2, req(), Lane::Interactive, 1),
            )
            .await
            .expect("next request must not hang: slot leaked");
            assert_eq!(next.0.unwrap().status, 200, "{k}: {v}");
        }
    }

    #[test]
    fn key_ids_hide_the_key() {
        let id = key_id("sk-secret-value");
        assert!(id.starts_with("k_") && id.len() == 10);
        assert!(!id.contains("secret"));
        assert_eq!(id, key_id("sk-secret-value"));
    }

    #[test]
    fn reload_keeps_schedulers_of_providers_with_the_same_limits() {
        let p = Protection::new(&Catalog::builtin(), Bounds::default());
        let ts = p.guard("typesafe", "k1");
        let wa = p.guard("workers-ai", "k1");
        let cat = Catalog::with_overlay_str(
            "[limits.workers-ai]\nrpm = 7\n[providers.typesafe]\nbase_url = \"http://x\"",
        )
        .unwrap();
        let next = p.reloaded(&cat);
        // Same limits (a base_url change does not matter): same scheduler.
        assert!(Arc::ptr_eq(&next.existing("typesafe", "k1").unwrap(), &ts));
        // New limits: a fresh scheduler with them.
        assert!(next.existing("workers-ai", "k1").is_none());
        let fresh = next.guard("workers-ai", "k1");
        assert!(!Arc::ptr_eq(&fresh, &wa));
        assert_eq!(fresh.limits().rpm, Some(7));
        // A provider that is gone keeps nothing.
        let mut gone = Catalog::builtin();
        gone.providers.remove("typesafe");
        assert!(next.reloaded(&gone).existing("typesafe", "k1").is_none());
    }

    #[test]
    fn a_scheduler_made_after_the_reload_through_the_old_snapshot_is_shared() {
        // A request holds the old snapshot (say, while its images download)
        // and makes the first scheduler for its key after the reload.
        let old = Protection::new(&Catalog::builtin(), Bounds::default());
        let new = old.reloaded(&Catalog::builtin());
        let late = old.guard("typesafe", "k2");
        assert!(Arc::ptr_eq(&new.guard("typesafe", "k2"), &late));
        assert_eq!(new.all().len(), 1);
    }
}
