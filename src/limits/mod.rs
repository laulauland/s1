//! Limits toward providers (spec "Backpressure and rate limits"): one
//! [`Gate`] per backend and API key.
//!
//! - [`bucket`]: token buckets for requests and input tokens per minute.
//! - [`aimd`]: the adaptive in-flight cap (halve on 429, +1 per success window).
//! - [`headers`]: rate-limit response headers and `Retry-After`.
//! - [`gate`]: all of the above plus the in-flight count, as one pure state
//!   machine that takes the time as an argument (simulated clocks in tests).
//!
//! The numbers come from `[limits.<provider>]` in the catalog (built-in
//! placeholders, overridden in `models.toml`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod aimd;
pub mod bucket;
pub mod gate;
pub mod headers;

pub use aimd::Aimd;
pub use bucket::TokenBucket;
pub use gate::{Blocked, Gate, Outcome};
pub use headers::RateHint;

/// `[limits.<provider>]`: limits, retries, and the circuit breaker for one
/// provider. Every field has a default, so a table may set only some.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Requests per minute; `None` (the default): no request limit. The
    /// built-in catalog sets placeholders for the v1 providers.
    pub rpm: Option<u64>,
    /// Input tokens per minute; `None`: no token limit.
    pub tpm: Option<u64>,
    /// Most requests in flight (the AIMD ceiling).
    pub max_in_flight: u32,
    /// Halve the in-flight cap on 429, add one per success window.
    pub adaptive: bool,
    /// AIMD success window in milliseconds.
    pub success_window_ms: u64,
    /// Most attempts per request, the first one included.
    pub retry_attempts: u32,
    /// First backoff delay in milliseconds (doubles per attempt, with jitter).
    pub retry_base_ms: u64,
    /// Largest backoff delay in milliseconds.
    pub retry_max_ms: u64,
    /// A `Retry-After` longer than this ends the retries: the client gets
    /// the provider's 429 and the backend stays throttled.
    pub retry_after_max_ms: u64,
    /// Failures (5xx or connection errors) in the window that open the breaker.
    pub breaker_failures: u32,
    pub breaker_window_ms: u64,
    /// How long an open breaker stops requests.
    pub breaker_cooldown_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rpm: None,
            tpm: None,
            max_in_flight: 32,
            adaptive: true,
            success_window_ms: 5_000,
            retry_attempts: 4,
            retry_base_ms: 500,
            retry_max_ms: 8_000,
            retry_after_max_ms: 60_000,
            breaker_failures: 5,
            breaker_window_ms: 30_000,
            breaker_cooldown_ms: 30_000,
        }
    }
}

impl Limits {
    /// Read one `[limits.<provider>]` table.
    pub fn from_toml(v: &toml::Value) -> Result<Self, String> {
        let l: Limits = v
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;
        l.validate()?;
        Ok(l)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_in_flight == 0 {
            return Err("max_in_flight must be at least 1".into());
        }
        if self.rpm == Some(0) || self.tpm == Some(0) {
            return Err("rpm and tpm must be at least 1".into());
        }
        if !(1..=crate::retry::MAX_ATTEMPTS).contains(&self.retry_attempts) {
            return Err(format!(
                "retry_attempts must be 1 to {}",
                crate::retry::MAX_ATTEMPTS
            ));
        }
        if self.breaker_failures == 0 {
            return Err("breaker_failures must be at least 1".into());
        }
        Ok(())
    }

    pub fn success_window(&self) -> Duration {
        Duration::from_millis(self.success_window_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_override_some_fields() {
        let v: toml::Value = toml::from_str("rpm = 60\nmax_in_flight = 4").unwrap();
        let l = Limits::from_toml(&v).unwrap();
        assert_eq!(l.rpm, Some(60));
        assert_eq!(l.max_in_flight, 4);
        assert_eq!(l.tpm, Limits::default().tpm);
        assert_eq!(l.retry_attempts, 4);
    }

    #[test]
    fn bad_tables_are_rejected() {
        for text in [
            "max_in_flight = 0",
            "rpm = 0",
            "nope = 1",
            "retry_attempts = 0",
            "retry_attempts = 5",
            "retry_attempts = 100",
        ] {
            let v: toml::Value = toml::from_str(text).unwrap();
            assert!(Limits::from_toml(&v).is_err(), "{text}");
        }
    }
}
