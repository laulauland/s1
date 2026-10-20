//! Rate-limit response headers. s1 obeys:
//!
//! - `retry-after-ms` (TypeSafe SDKs) and `retry-after` (seconds or an
//!   HTTP date), on 429 and other retryable responses;
//! - Cloudflare `ratelimit: "default";r=<remaining>;t=<reset seconds>`
//!   (also the IETF `remaining=..., reset=...` form): `r=0` throttles for `t`;
//! - OpenAI `x-ratelimit-remaining-{requests,tokens}: 0` with
//!   `x-ratelimit-reset-{requests,tokens}` (`1s`, `6m0s`, `20ms`).
//!
//! Sources: /tmp/s1-docs notes (TypeSafe errors and SDK retry policy,
//! Cloudflare API limits, OpenAI rate-limit guide).

use std::time::{Duration, SystemTime};

use axum::http::HeaderMap;

/// Longest pause s1 takes from one response. Larger header values (or
/// values that do not fit a `Duration`) are clamped to this, so a bad
/// header cannot overflow time arithmetic or block a backend forever.
pub const MAX_PAUSE: Duration = Duration::from_secs(24 * 3600);

/// Seconds as a `Duration`, clamped to [`MAX_PAUSE`]; `None` if negative or NaN.
fn clamped_secs(secs: f64) -> Option<Duration> {
    if secs.is_nan() || secs < 0.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(secs).map_or(MAX_PAUSE, |d| d.min(MAX_PAUSE)))
}

/// What the provider said about its limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateHint {
    /// `Retry-After` (or `retry-after-ms`).
    pub retry_after: Option<Duration>,
    /// A quota is used up for this long.
    pub exhausted_for: Option<Duration>,
}

impl RateHint {
    /// Read the headers of one response. `now` converts HTTP dates.
    pub fn from_headers(h: &HeaderMap, now: SystemTime) -> Self {
        let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
        let retry_after = get("retry-after-ms")
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(|ms| clamped_secs(ms / 1000.0))
            .or_else(|| get("retry-after").and_then(|v| parse_retry_after(v, now)));
        let mut exhausted: Option<Duration> = None;
        let mut widen = |d: Duration| exhausted = Some(exhausted.map_or(d, |e| e.max(d)));
        if let Some(d) = parse_ratelimit(
            h.get_all("ratelimit")
                .iter()
                .filter_map(|v| v.to_str().ok()),
        ) {
            widen(d);
        }
        for kind in ["requests", "tokens"] {
            let remaining =
                get(&format!("x-ratelimit-remaining-{kind}")).and_then(|v| v.parse::<u64>().ok());
            if remaining == Some(0)
                && let Some(d) =
                    get(&format!("x-ratelimit-reset-{kind}")).and_then(parse_go_duration)
            {
                widen(d);
            }
        }
        Self {
            retry_after,
            exhausted_for: exhausted,
        }
    }

    /// How long to stop sending, if at all (at most [`MAX_PAUSE`]).
    pub fn pause(&self) -> Option<Duration> {
        let d = match (self.retry_after, self.exhausted_for) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        d.map(|d| d.min(MAX_PAUSE))
    }
}

/// `Retry-After`: delay seconds (integer, decimals tolerated) or an HTTP date.
pub fn parse_retry_after(v: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(secs) = v.parse::<f64>() {
        return clamped_secs(secs);
    }
    let at = parse_http_date(v)?;
    Some(
        at.duration_since(now)
            .unwrap_or(Duration::ZERO)
            .min(MAX_PAUSE),
    )
}

/// The `ratelimit` header: every policy in every header value counts.
/// Structured-field values list policies separated by commas
/// (`"short";r=0;t=30, "long";r=5;t=300`); the older IETF form spreads one
/// policy over comma-separated keys (`limit=100, remaining=0, reset=30`).
/// Returns the longest reset among the exhausted policies.
fn parse_ratelimit<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<Duration> {
    let mut longest: Option<Duration> = None;
    let mut widen = |d: Option<Duration>| {
        if let Some(d) = d {
            longest = Some(longest.map_or(d, |l| l.max(d)));
        }
    };
    for v in values {
        // Keys of the IETF form, gathered across the whole value.
        let mut ietf = Policy::default();
        for item in v.split(',') {
            if item.contains(';') {
                let mut p = Policy::default();
                for part in item.split(';') {
                    p.read(part);
                }
                widen(p.exhausted_for());
            } else {
                ietf.read(item);
            }
        }
        widen(ietf.exhausted_for());
    }
    longest
}

/// One `ratelimit` policy as it is read.
#[derive(Default)]
struct Policy {
    remaining: Option<u64>,
    reset: Option<u64>,
}

impl Policy {
    fn read(&mut self, part: &str) {
        let Some((k, val)) = part.trim().split_once('=') else {
            return;
        };
        let val = val.trim().trim_matches('"');
        match k.trim() {
            "r" | "remaining" => self.remaining = val.parse::<u64>().ok(),
            "t" | "reset" => self.reset = val.parse::<u64>().ok(),
            _ => {}
        }
    }

    /// The reset time if nothing remains.
    fn exhausted_for(&self) -> Option<Duration> {
        (self.remaining == Some(0))
            .then(|| Duration::from_secs(self.reset.unwrap_or(1)).min(MAX_PAUSE))
    }
}

/// Go-style durations as OpenAI sends them: `1s`, `6m0s`, `20ms`, `1h2m3.5s`.
pub fn parse_go_duration(v: &str) -> Option<Duration> {
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = v.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        let unit = match c {
            'h' => 3600.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                0.001
            }
            'm' => 60.0,
            's' => 1.0,
            _ => return None,
        };
        total += n * unit;
        any = true;
    }
    if !num.is_empty() {
        // A bare number: seconds.
        total += num.parse::<f64>().ok()?;
        any = true;
    }
    if !any {
        return None;
    }
    clamped_secs(total)
}

/// IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`).
fn parse_http_date(v: &str) -> Option<SystemTime> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    let [_, day, mon, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == mon)? as i64
        + 1;
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let hms: Vec<i64> = time
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let [h, m, s] = hms.as_slice() else {
        return None;
    };
    // Bound every provider-supplied field before the arithmetic so a hostile
    // header can neither overflow (panic in debug) nor wrap (release).
    let (h, m, s) = (*h, *m, *s);
    if !(1970..=9999).contains(&year)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&h)
        || !(0..60).contains(&m)
        || !(0..=60).contains(&s)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(h * 3600 + m * 60 + s)?;
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 3339 UTC time with milliseconds, for `/status` and `/v1/models`.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn retry_after_forms() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_470);
        let hint = RateHint::from_headers(&headers(&[("retry-after", "3")]), now);
        assert_eq!(hint.retry_after, Some(Duration::from_secs(3)));
        let hint = RateHint::from_headers(
            &headers(&[("retry-after", "2"), ("retry-after-ms", "250")]),
            now,
        );
        assert_eq!(hint.retry_after, Some(Duration::from_millis(250)));
        // 1445412480 = Wed, 21 Oct 2015 07:28:00 GMT.
        let hint = RateHint::from_headers(
            &headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
            now,
        );
        assert_eq!(hint.retry_after, Some(Duration::from_secs(10)));
        assert_eq!(
            RateHint::from_headers(&headers(&[("retry-after", "soon")]), now).pause(),
            None
        );
    }

    #[test]
    fn cloudflare_and_openai_quota_headers() {
        let now = SystemTime::now();
        let cf = RateHint::from_headers(&headers(&[("ratelimit", "\"default\";r=0;t=30")]), now);
        assert_eq!(cf.exhausted_for, Some(Duration::from_secs(30)));
        let cf = RateHint::from_headers(&headers(&[("ratelimit", "\"default\";r=5;t=30")]), now);
        assert_eq!(cf.pause(), None);
        let ietf = RateHint::from_headers(
            &headers(&[("ratelimit", "limit=10, remaining=0, reset=7")]),
            now,
        );
        assert_eq!(ietf.exhausted_for, Some(Duration::from_secs(7)));
        let oa = RateHint::from_headers(
            &headers(&[
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "6m0s"),
                ("x-ratelimit-remaining-tokens", "10"),
                ("x-ratelimit-reset-tokens", "1s"),
            ]),
            now,
        );
        assert_eq!(oa.exhausted_for, Some(Duration::from_secs(360)));
    }

    #[test]
    fn every_ratelimit_policy_counts() {
        let now = SystemTime::now();
        // One header value with two policies, in both orders.
        for v in [
            "\"short\";r=0;t=30, \"long\";r=5;t=300",
            "\"long\";r=5;t=300, \"short\";r=0;t=30",
        ] {
            let mut h = HeaderMap::new();
            h.insert("ratelimit", HeaderValue::from_str(v).unwrap());
            let hint = RateHint::from_headers(&h, now);
            assert_eq!(hint.exhausted_for, Some(Duration::from_secs(30)), "{v}");
        }
        // Two exhausted policies: the longest reset wins, in both orders.
        for v in [
            "\"short\";r=0;t=30, \"long\";r=0;t=300",
            "\"long\";r=0;t=300, \"short\";r=0;t=30",
        ] {
            let mut h = HeaderMap::new();
            h.insert("ratelimit", HeaderValue::from_str(v).unwrap());
            let hint = RateHint::from_headers(&h, now);
            assert_eq!(hint.exhausted_for, Some(Duration::from_secs(300)), "{v}");
        }
        // Policies split across several header values, in both orders.
        for (a, b) in [
            ("\"short\";r=0;t=30", "\"long\";r=5;t=300"),
            ("\"long\";r=5;t=300", "\"short\";r=0;t=30"),
            ("\"short\";r=0;t=30", "\"long\";r=0;t=300"),
        ] {
            let mut h = HeaderMap::new();
            h.append("ratelimit", HeaderValue::from_str(a).unwrap());
            h.append("ratelimit", HeaderValue::from_str(b).unwrap());
            let hint = RateHint::from_headers(&h, now);
            let want = if a.contains("r=0;t=300") || b.contains("r=0;t=300") {
                300
            } else {
                30
            };
            assert_eq!(
                hint.exhausted_for,
                Some(Duration::from_secs(want)),
                "{a} | {b}"
            );
        }
        // The IETF single-policy form still parses as one policy.
        let mut h = HeaderMap::new();
        h.insert(
            "ratelimit",
            HeaderValue::from_static("limit=10, remaining=0, reset=7"),
        );
        assert_eq!(
            RateHint::from_headers(&h, now).exhausted_for,
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("20ms"), Some(Duration::from_millis(20)));
        assert_eq!(
            parse_go_duration("1h2m3.5s"),
            Some(Duration::from_secs_f64(3723.5))
        );
        assert_eq!(parse_go_duration("x"), None);
    }

    #[test]
    fn extreme_values_are_clamped_not_panics() {
        let now = SystemTime::now();
        for (k, v) in [
            ("retry-after", "1e300"),
            ("retry-after", "inf"),
            ("retry-after", "10000000000000000000"),
            ("retry-after-ms", "1e300"),
            ("retry-after-ms", "18446744073709551615000"),
            ("ratelimit", "\"default\";r=0;t=18446744073709551615"),
            ("ratelimit", "limit=1, remaining=0, reset=99999999999"),
        ] {
            let hint = RateHint::from_headers(&headers(&[(k, v)]), now);
            assert_eq!(hint.pause(), Some(MAX_PAUSE), "{k}: {v}");
        }
        let oa = RateHint::from_headers(
            &headers(&[
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "99999999999999999999h"),
            ]),
            now,
        );
        assert_eq!(oa.pause(), Some(MAX_PAUSE));
        assert_eq!(parse_go_duration("1e999"), None);
        // HTTP-date fields out of range: rejected, never overflow.
        for v in [
            "Wed, 21 Oct 9999999999999999 07:28:00 GMT",
            "Wed, 21 Oct 2015 99999999999999999:00:00 GMT",
            "Wed, 21 Oct 2015 07:99999999999999999:00 GMT",
            "Wed, 21 Oct 2015 07:28:99999999999999999 GMT",
            "Wed, 99999999999999999 Oct 2015 07:28:00 GMT",
            "Wed, -9223372036854775808 Oct 2015 07:28:00 GMT",
            "Wed, 21 Oct -9223372036854775808 07:28:00 GMT",
            "Wed, 21 Oct 2015 24:00:00 GMT",
        ] {
            assert_eq!(parse_http_date(v), None, "{v}");
            let hint = RateHint::from_headers(&headers(&[("retry-after", v)]), now);
            assert_eq!(hint.pause(), None, "{v}");
        }
        // The largest accepted date is far in the future: clamped, not wrapped.
        let far = RateHint::from_headers(
            &headers(&[("retry-after", "Fri, 31 Dec 9999 23:59:60 GMT")]),
            now,
        );
        assert_eq!(far.pause(), Some(MAX_PAUSE));
        assert_eq!(
            RateHint::from_headers(&headers(&[("retry-after", "NaN")]), now).pause(),
            None
        );
        assert_eq!(
            RateHint::from_headers(&headers(&[("retry-after", "-5")]), now).pause(),
            None
        );
    }

    #[test]
    fn rfc3339_formats_utc() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_445_412_480_123);
        assert_eq!(rfc3339(t), "2015-10-21T07:28:00.123Z");
        assert_eq!(days_from_civil(2015, 10, 21), 16_729);
    }
}
