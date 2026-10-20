//! Retries (spec "Backpressure and rate limits"): exponential backoff with
//! jitter, at most 4 attempts, only on 429, 5xx, and connection errors.

use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use crate::backend::ProviderResponse;
use crate::error::S1Error;
use crate::limits::Limits;

/// What one attempt's result means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 2xx: done.
    Success,
    /// 429: throttled; retry after the pause.
    RateLimited,
    /// 5xx or a connection error: retry, counts toward the breaker.
    Failure,
    /// Anything else (4xx, local errors): return as is.
    Final,
}

impl Verdict {
    pub fn of(result: &Result<ProviderResponse, S1Error>) -> Self {
        match result {
            Ok(r) if r.status.is_success() => Verdict::Success,
            Ok(r) if r.status.as_u16() == 429 => Verdict::RateLimited,
            Ok(r) if r.status.is_server_error() => Verdict::Failure,
            Ok(_) => Verdict::Final,
            Err(S1Error::Unreachable(_)) => Verdict::Failure,
            Err(_) => Verdict::Final,
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, Verdict::RateLimited | Verdict::Failure)
    }
}

/// The spec's hard cap on attempts per request.
pub const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_attempts: u32,
    pub base: Duration,
    pub max: Duration,
    /// Longest `Retry-After` s1 waits out with a retry.
    pub retry_after_max: Duration,
}

impl Policy {
    pub fn from_limits(l: &Limits) -> Self {
        Self {
            max_attempts: l.retry_attempts.clamp(1, MAX_ATTEMPTS),
            base: Duration::from_millis(l.retry_base_ms),
            max: Duration::from_millis(l.retry_max_ms.max(l.retry_base_ms)),
            retry_after_max: Duration::from_millis(l.retry_after_max_ms),
        }
    }

    /// Delay before attempt `attempt + 1` (`attempt` starts at 1): the
    /// exponential step `base * 2^(attempt-1)` capped at `max`, with "equal
    /// jitter" (a random point in the upper half). `unit` is a random
    /// number in `[0, 1)`.
    pub fn backoff(&self, attempt: u32, unit: f64) -> Duration {
        let step = self
            .base
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(20))
            .min(self.max);
        step / 2 + step.mul_f64(unit.clamp(0.0, 1.0) / 2.0)
    }
}

/// A random number in `[0, 1)` (not for secrets).
pub fn jitter() -> f64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    );
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use bytes::Bytes;
    use proptest::prelude::*;

    fn resp(code: u16) -> Result<ProviderResponse, S1Error> {
        Ok(ProviderResponse {
            status: StatusCode::from_u16(code).unwrap(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        })
    }

    #[test]
    fn retries_only_429_5xx_and_connection_errors() {
        assert_eq!(Verdict::of(&resp(200)), Verdict::Success);
        assert_eq!(Verdict::of(&resp(429)), Verdict::RateLimited);
        for code in [500, 502, 503, 529] {
            assert_eq!(Verdict::of(&resp(code)), Verdict::Failure, "{code}");
        }
        for code in [400, 401, 403, 413, 422] {
            assert!(!Verdict::of(&resp(code)).retryable(), "{code}");
        }
        assert!(Verdict::of(&Err(S1Error::Unreachable("x".into()))).retryable());
        assert!(!Verdict::of(&Err(S1Error::BackendUnavailable("no key".into()))).retryable());
    }

    #[test]
    fn default_policy_is_four_attempts() {
        assert_eq!(Policy::from_limits(&Limits::default()).max_attempts, 4);
    }

    #[test]
    fn attempts_never_exceed_four() {
        let l = Limits {
            retry_attempts: 50,
            ..Limits::default()
        };
        assert_eq!(Policy::from_limits(&l).max_attempts, MAX_ATTEMPTS);
        let l = Limits {
            retry_attempts: 0,
            ..Limits::default()
        };
        assert_eq!(Policy::from_limits(&l).max_attempts, 1);
    }

    proptest! {
        #[test]
        fn backoff_grows_and_stays_bounded(attempt in 1u32..30, unit in 0.0f64..1.0) {
            let p = Policy::from_limits(&Limits::default());
            let d = p.backoff(attempt, unit);
            let step = p.base.saturating_mul(1u32 << (attempt - 1).min(20)).min(p.max);
            prop_assert!(d >= step / 2 && d <= step);
            prop_assert!(d <= p.max);
        }
    }

    #[test]
    fn jitter_is_in_unit_range() {
        for _ in 0..100 {
            let j = jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }
}
