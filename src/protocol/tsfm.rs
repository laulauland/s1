//! TSFM.ai forecast format (`POST /v1/forecast`).
//!
//! Wire shape (TSFM OpenAPI `ForecastRequest` / `ForecastResponseBody`):
//! - Request: `model`, `inputs: [{target, start?, past_covariates?,
//!   future_covariates?, static_covariates?, metadata?}]`, optional
//!   `parameters: {prediction_length, frequency, quantile_levels, ...}`.
//!   `target` is always 2D (`[timesteps][channels]`), `null` marks a
//!   missing value. The time axis is `inputs[].start` plus the pandas
//!   offset alias `parameters.frequency`: a regular grid.
//! - Response: `quantile_levels` and `outputs[]` (one per input, same
//!   order) with `mean`, `quantile_predictions: [{level, values}]`, and
//!   `timestamps` (only when the request had `start` and `frequency`).
//! - Errors: `{"error": "...", "code": "...", "endpoint": "/v1/forecast"}`.
//!
//! s1 checks that every series lies on a regular grid ([`Tsfm::inspect`])
//! and passes the body on unchanged. It never touches the quantiles: the
//! levels a backend produced are the levels the client gets
//! ([`flatten_rows`] names columns after them).

use bytes::Bytes;
use chrono::{
    DateTime, Datelike, Months, NaiveDate, NaiveDateTime, SecondsFormat, Timelike, Utc, Weekday,
};
use serde_json::{Map, Value, json};

use super::{InspectError, Protocol, ProtocolId, RouteInfo};
use crate::error::S1Error;

/// Path of the forecast endpoint, echoed in TSFM error bodies.
pub const ENDPOINT: &str = "/v1/forecast";

pub struct Tsfm;

impl Protocol for Tsfm {
    fn id(&self) -> ProtocolId {
        ProtocolId::Tsfm
    }

    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| InspectError(format!("request is not JSON: {e}")))?;
        let model = validate(&v).map_err(InspectError)?;
        Ok(RouteInfo {
            model,
            input_bytes: body.len(),
            ..Default::default()
        })
    }

    /// A forecast is many rows, not one: see [`flatten_rows`].
    fn flatten(&self, _resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError> {
        Err(InspectError(
            "a forecast flattens to many rows; use protocol::tsfm::flatten_rows".into(),
        ))
    }

    fn error_body(&self, err: &S1Error) -> Bytes {
        Bytes::from(
            json!({"error": err.to_string(), "code": err.code(), "endpoint": ENDPOINT}).to_string(),
        )
    }
}

/// Check a request; return its `model`. Rejects series that are not on a
/// regular grid: a 1D or ragged `target`, past covariates of another
/// length than the target, an unreadable `start` or `frequency`, and
/// `timestamps` (not a TSFM field) that are not evenly spaced.
pub fn validate(v: &Value) -> Result<String, String> {
    let obj = v.as_object().ok_or("request must be a JSON object")?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or("`model` must be a string")?
        .to_string();
    let params = match obj.get("parameters") {
        None | Some(Value::Null) => None,
        Some(Value::Object(p)) => Some(p),
        Some(_) => return Err("`parameters` must be an object".into()),
    };
    let freq_alias = match params.and_then(|p| p.get("frequency")) {
        None | Some(Value::Null) => None,
        Some(Value::String(f)) => {
            check_alias(f)?;
            Some(f.as_str())
        }
        Some(_) => {
            return Err("`parameters.frequency` must be a pandas offset alias string".into());
        }
    };
    if let Some(p) = params {
        if let Some(n) = p.get("prediction_length").filter(|n| !n.is_null())
            && n.as_u64().is_none_or(|n| n < 1)
        {
            return Err("`parameters.prediction_length` must be an integer >= 1".into());
        }
        match p.get("quantile_levels") {
            None | Some(Value::Null) => {}
            Some(Value::Array(levels)) => {
                for l in levels {
                    if l.as_f64().is_none_or(|l| l <= 0.0 || l >= 1.0) {
                        return Err(format!(
                            "`parameters.quantile_levels` must be numbers between 0 and 1 (exclusive), not {l}"
                        ));
                    }
                }
            }
            Some(_) => return Err("`parameters.quantile_levels` must be an array".into()),
        }
    }
    let inputs = match obj.get("inputs") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err("`inputs` must be a non-empty array of series".into()),
    };
    for (i, input) in inputs.iter().enumerate() {
        validate_series(input, freq_alias).map_err(|e| format!("inputs[{i}]: {e}"))?;
    }
    Ok(model)
}

fn validate_series(input: &Value, freq_alias: Option<&str>) -> Result<(), String> {
    let s = input.as_object().ok_or("a series must be an object")?;
    let target = match s.get("target") {
        Some(Value::Array(t)) if !t.is_empty() => t,
        _ => return Err("`target` must be a non-empty 2D array [timesteps][channels]".into()),
    };
    let mut channels = None;
    for (k, row) in target.iter().enumerate() {
        let Value::Array(row) = row else {
            return Err(format!(
                "`target` must be 2D ([[v1], [v2], ...]); target[{k}] is not an array"
            ));
        };
        if row.is_empty() {
            return Err(format!("target[{k}] has no channels"));
        }
        if row.iter().any(|x| !(x.is_number() || x.is_null())) {
            return Err(format!(
                "target[{k}] holds a value that is not a number or null"
            ));
        }
        match channels {
            None => channels = Some(row.len()),
            Some(c) if c != row.len() => {
                return Err(format!(
                    "irregular series: target[{k}] has {} channels, target[0] has {c}",
                    row.len()
                ));
            }
            Some(_) => {}
        }
    }
    let len = target.len();
    match s.get("past_covariates") {
        None | Some(Value::Null) => {}
        Some(Value::Object(cov)) => {
            for (name, c) in cov {
                let Value::Array(c) = c else {
                    return Err(format!("past_covariates.{name} must be an array"));
                };
                if c.len() != len {
                    return Err(format!(
                        "irregular series: past_covariates.{name} has {} points, target has {len}",
                        c.len()
                    ));
                }
            }
        }
        Some(_) => return Err("`past_covariates` must be an object".into()),
    }
    let start = match s.get("start") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => {
            Some(parse_time(t).ok_or_else(|| format!("`start` `{t}` is not an ISO 8601 time"))?)
        }
        Some(_) => return Err("`start` must be an ISO 8601 string".into()),
    };
    // Not a TSFM field, but the one way a client can send an irregular
    // series. Accept it only when it is a regular grid.
    match s.get("timestamps") {
        None | Some(Value::Null) => {}
        Some(Value::Array(ts)) => {
            let times = ts
                .iter()
                .map(|t| t.as_str().and_then(parse_time))
                .collect::<Option<Vec<_>>>()
                .ok_or("`timestamps` must be ISO 8601 strings")?;
            // Wall-clock times in each timestamp's own offset: calendar
            // anchors (weekday, month start) are checked on these.
            let wall = ts
                .iter()
                .map(|t| t.as_str().and_then(parse_wall_time))
                .collect::<Option<Vec<_>>>()
                .ok_or("`timestamps` must be ISO 8601 strings")?;
            if times.len() != len {
                return Err(format!(
                    "irregular series: {} timestamps for {len} target points",
                    times.len()
                ));
            }
            if let Some(st) = start
                && times.first() != Some(&st)
            {
                return Err("irregular series: `start` is not the first timestamp".into());
            }
            // A declared frequency is the grid the timestamps must follow.
            // One s1 cannot interpret cannot vouch for them: reject.
            let grid = freq_alias
                .map(|a| {
                    DeclaredGrid::parse(a).map_err(|e| {
                        format!("cannot check `timestamps` against `parameters.frequency`: {e}")
                    })
                })
                .transpose()?;
            check_regular(&times, &wall, grid.as_ref())?;
        }
        Some(_) => return Err("`timestamps` must be an array".into()),
    }
    Ok(())
}

/// A declared `parameters.frequency` as a grid: a step, plus the weekday
/// every point falls on for anchored weekly aliases (`W` = `W-SUN`,
/// `W-WED`, `2W-MON`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredGrid {
    freq: Freq,
    weekday: Option<Weekday>,
    alias: String,
}

impl DeclaredGrid {
    fn parse(alias: &str) -> Result<DeclaredGrid, String> {
        let digits = alias.chars().take_while(char::is_ascii_digit).count();
        let (count, unit) = alias.split_at(digits);
        let weekday = match unit {
            "W" => Some(Weekday::Sun),
            _ => match unit.strip_prefix("W-") {
                Some(day) => Some(match day {
                    "SUN" => Weekday::Sun,
                    "MON" => Weekday::Mon,
                    "TUE" => Weekday::Tue,
                    "WED" => Weekday::Wed,
                    "THU" => Weekday::Thu,
                    "FRI" => Weekday::Fri,
                    "SAT" => Weekday::Sat,
                    _ => return Err(format!("`{alias}` has an unknown weekly anchor")),
                }),
                None => None,
            },
        };
        let freq = match weekday {
            // Weekly: n weeks of 7 days, anchored on a weekday.
            Some(_) => Freq::parse(&format!("{count}W"))?,
            None => Freq::parse(alias)?,
        };
        Ok(DeclaredGrid {
            freq,
            weekday,
            alias: alias.to_string(),
        })
    }
}

/// Every time is one step after the one before: the declared grid, or
/// (without one) the first gap. `wall` holds the same times as wall-clock
/// times in their own offsets (as UTC values); calendar grids (weekly
/// anchors and month starts) are checked on them, as pandas does for
/// times with an offset.
fn check_regular(
    times: &[DateTime<Utc>],
    wall: &[DateTime<Utc>],
    grid: Option<&DeclaredGrid>,
) -> Result<(), String> {
    let Some(&t0) = times.first() else {
        return Ok(());
    };
    let (freq, name) = match grid {
        Some(g) => {
            if let Some(day) = g.weekday
                && wall[0].weekday() != day
            {
                return Err(format!(
                    "irregular series: timestamps[0] ({}) is not a {day} as the {} grid needs",
                    format_time(t0),
                    g.alias
                ));
            }
            (g.freq, g.alias.clone())
        }
        None if times.len() < 2 => return Ok(()),
        None => {
            let gap = (times[1] - t0).num_seconds();
            if gap <= 0 {
                return Err("irregular series: timestamps are not increasing".into());
            }
            let f = Freq::Seconds(gap);
            (f, f.alias())
        }
    };
    let calendar =
        grid.is_some_and(|g| g.weekday.is_some() || matches!(g.freq, Freq::MonthStart(_)));
    let axis = if calendar { wall } else { times };
    for (k, t) in axis.iter().enumerate() {
        if freq.index(axis[0], *t) != Some(k as i64) {
            return Err(format!(
                "irregular series: timestamps[{k}] ({}) is not on the {name} grid from {}; \
                 use `s1 forecast --csv` to build a regular grid",
                format_time(times[k]),
                format_time(t0)
            ));
        }
    }
    Ok(())
}

/// A pandas offset alias: optional count, then letters, optional anchor
/// (`D`, `15min`, `1h`, `W-SUN`, `MS`, `Q`).
fn check_alias(f: &str) -> Result<(), String> {
    let rest = f.trim_start_matches(|c: char| c.is_ascii_digit());
    let (unit, anchor) = rest.split_once('-').unwrap_or((rest, ""));
    let ok = !unit.is_empty()
        && unit.chars().all(|c| c.is_ascii_alphabetic())
        && anchor.chars().all(|c| c.is_ascii_alphanumeric())
        && !f.starts_with('0');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`parameters.frequency` `{f}` is not a pandas offset alias (e.g. D, 1h, 15min, MS)"
        ))
    }
}

/// A regular step that s1 can build a grid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    /// A fixed number of seconds (s, min, h, D, W = 7D).
    Seconds(i64),
    /// N months, each point the first day of a month at midnight (MS).
    MonthStart(u32),
}

impl Freq {
    /// Parse a pandas offset alias s1 can build a grid for: `s`, `min`
    /// (`T`), `h` (`H`), `D`, `W` (as 7 days), `MS`, each with an optional
    /// count (`15min`, `2D`).
    pub fn parse(alias: &str) -> Result<Freq, String> {
        let digits: String = alias.chars().take_while(char::is_ascii_digit).collect();
        let unit = &alias[digits.len()..];
        let n: i64 = if digits.is_empty() {
            1
        } else {
            digits
                .parse()
                .map_err(|_| format!("bad count in frequency `{alias}`"))?
        };
        if n < 1 {
            return Err(format!("frequency `{alias}` must have a count >= 1"));
        }
        let secs = match unit {
            "s" | "S" => 1,
            "min" | "T" => 60,
            "h" | "H" => 3_600,
            "D" => 86_400,
            "W" => 7 * 86_400,
            "MS" => {
                return u32::try_from(n)
                    .map(Freq::MonthStart)
                    .map_err(|_| format!("frequency `{alias}` is too large"));
            }
            _ => {
                return Err(format!(
                    "cannot build a grid for frequency `{alias}`; use s, min, h, D, W, or MS with an optional count"
                ));
            }
        };
        n.checked_mul(secs)
            .map(Freq::Seconds)
            .ok_or_else(|| format!("frequency `{alias}` is too large"))
    }

    /// The pandas offset alias sent as `parameters.frequency`. Weeks are
    /// sent as days, because pandas `W` anchors on Sundays.
    pub fn alias(self) -> String {
        match self {
            Freq::Seconds(s) if s % 86_400 == 0 => match s / 86_400 {
                1 => "D".into(),
                n => format!("{n}D"),
            },
            Freq::Seconds(s) if s % 3_600 == 0 => format!("{}h", s / 3_600),
            Freq::Seconds(s) if s % 60 == 0 => format!("{}min", s / 60),
            Freq::Seconds(s) => format!("{s}s"),
            Freq::MonthStart(1) => "MS".into(),
            Freq::MonthStart(n) => format!("{n}MS"),
        }
    }

    /// Grid index of `t` on the grid that starts at `t0`; `None` if `t`
    /// is not a grid point.
    pub fn index(self, t0: DateTime<Utc>, t: DateTime<Utc>) -> Option<i64> {
        match self {
            Freq::Seconds(s) => {
                let d = t - t0;
                let ms = d.num_milliseconds();
                let step = s.checked_mul(1_000)?;
                (ms % step == 0 && d.subsec_nanos() % 1_000_000 == 0).then_some(ms / step)
            }
            Freq::MonthStart(n) => {
                if !is_month_start(t0) || !is_month_start(t) {
                    return None;
                }
                let months = month_number(t) - month_number(t0);
                (months % i64::from(n) == 0).then_some(months / i64::from(n))
            }
        }
    }

    /// The grid point `k` steps after `t0`.
    pub fn at(self, t0: DateTime<Utc>, k: i64) -> Option<DateTime<Utc>> {
        match self {
            Freq::Seconds(s) => {
                t0.checked_add_signed(chrono::TimeDelta::try_seconds(s.checked_mul(k)?)?)
            }
            Freq::MonthStart(n) => {
                let months = u32::try_from(k.checked_mul(i64::from(n))?).ok()?;
                t0.checked_add_months(Months::new(months))
            }
        }
    }
}

/// Midnight on the first day of a month.
pub fn is_month_start(t: DateTime<Utc>) -> bool {
    t.day() == 1 && t.num_seconds_from_midnight() == 0 && t.nanosecond() == 0
}

fn month_number(t: DateTime<Utc>) -> i64 {
    i64::from(t.year()) * 12 + i64::from(t.month0())
}

/// Read a time: RFC 3339 (`2026-02-01T00:00:00Z`, offsets become UTC),
/// a time without zone (taken as UTC, `T` or space), or a date.
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(t) = DateTime::parse_from_str(s, f) {
            return Some(t.with_timezone(&Utc));
        }
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(t.and_utc());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc())
}

/// Read a time as [`parse_time`] does, but keep its wall-clock time in its
/// own offset (returned as a UTC value). A time without an offset is
/// taken as UTC, so it is the same as [`parse_time`].
fn parse_wall_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.naive_local().and_utc());
    }
    for f in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(t) = DateTime::parse_from_str(s, f) {
            return Some(t.naive_local().and_utc());
        }
    }
    parse_time(s)
}

/// ISO 8601 in UTC with `Z`, as in the TSFM docs (`2026-02-01T00:00:00Z`).
pub fn format_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Long format for `--flat`: one row per series, step, and channel, with
/// `id`, `step` (1 = first forecast step), `timestamp` (or null),
/// `channel` (multivariate series only), `mean`, and one `q<level>` column
/// per quantile the backend produced, named and ordered as produced.
///
/// The row id is the series' echoed `metadata.id`, then `metadata.item_id`,
/// else its index in `outputs`.
pub fn flatten_rows(resp: &[u8]) -> Result<Vec<Map<String, Value>>, InspectError> {
    let v: Value = serde_json::from_slice(resp)
        .map_err(|e| InspectError(format!("response is not JSON: {e}")))?;
    let outputs = v
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| InspectError("response has no `outputs` array".into()))?;
    let mut rows = Vec::new();
    for (i, out) in outputs.iter().enumerate() {
        let meta = out.get("metadata");
        let id = ["id", "item_id"]
            .iter()
            .find_map(|k| meta.and_then(|m| m.get(*k)).filter(|v| !v.is_null()))
            .cloned()
            .unwrap_or_else(|| json!(i));
        let mean = out
            .get("mean")
            .and_then(Value::as_array)
            .ok_or_else(|| InspectError(format!("outputs[{i}] has no `mean` array")))?;
        let timestamps = out.get("timestamps").and_then(Value::as_array);
        // Quantile columns in the order the backend produced them; a level
        // that repeats keeps its first values.
        let mut quantiles: Vec<(String, &Vec<Value>)> = Vec::new();
        for q in out
            .get("quantile_predictions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(level), Some(values)) = (
                q.get("level").filter(|l| l.is_number()),
                q.get("values").and_then(Value::as_array),
            ) else {
                return Err(InspectError(format!(
                    "outputs[{i}].quantile_predictions holds an entry without `level` and `values`"
                )));
            };
            let col = format!("q{level}");
            if !quantiles.iter().any(|(c, _)| *c == col) {
                quantiles.push((col, values));
            }
        }
        let channels = mean
            .iter()
            .map(|r| r.as_array().map_or(1, Vec::len))
            .max()
            .unwrap_or(1);
        for (s, m) in mean.iter().enumerate() {
            for ch in 0..channels {
                let mut row = Map::new();
                row.insert("id".into(), id.clone());
                row.insert("step".into(), json!(s + 1));
                row.insert(
                    "timestamp".into(),
                    timestamps
                        .and_then(|t| t.get(s))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                if channels > 1 {
                    row.insert("channel".into(), json!(ch));
                }
                row.insert("mean".into(), cell(m, ch));
                for (col, values) in &quantiles {
                    let v = values.get(s).map_or(Value::Null, |r| cell(r, ch));
                    row.insert(col.clone(), v);
                }
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

/// Channel `ch` of one step (`[v1, v2]`), or the value itself if it is a
/// bare number.
fn cell(step: &Value, ch: usize) -> Value {
    match step {
        Value::Array(a) => a.get(ch).cloned().unwrap_or(Value::Null),
        other if ch == 0 => other.clone(),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Canonical request from https://tsfm.ai/docs/api.
    fn docs_request() -> Value {
        json!({
            "model": "amazon/chronos-bolt-small",
            "inputs": [{
                "start": "2026-02-01T00:00:00Z",
                "target": [[428], [435], [441], [438], [446], [452], [460], [458], [466], [472]],
                "metadata": { "item_id": "store_017" }
            }],
            "parameters": {"prediction_length": 7, "frequency": "D", "quantile_levels": [0.1, 0.5, 0.9]},
            "metadata": {"surface": "docs.quickstart"}
        })
    }

    fn inspect(v: &Value) -> Result<RouteInfo, InspectError> {
        Tsfm.inspect(&serde_json::to_vec(v).unwrap())
    }

    #[test]
    fn inspect_reads_model_of_docs_examples() {
        let info = inspect(&docs_request()).unwrap();
        assert_eq!(info.model, "amazon/chronos-bolt-small");
        assert!(info.input_bytes > 0);
        // Covariates example from https://tsfm.ai/docs/forecast.
        let cov = json!({
            "model": "google/timesfm-2.0-500m-pytorch",
            "inputs": [{
                "start": "2026-03-01T00:00:00Z",
                "target": [[110], [113], [108], [116], [119], [121]],
                "past_covariates": { "promo": [0, 1, 0, 1, 1, 0] },
                "future_covariates": { "promo": [1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0] },
                "static_covariates": { "store_cluster": 2, "shelf_capacity": 1 },
                "metadata": { "item_id": "sku_42" }
            }],
            "parameters": {"prediction_length": 12, "frequency": "D", "quantile_levels": [0.1, 0.5, 0.9], "context_length": 256},
            "metadata": { "surface": "docs.forecast" }
        });
        assert_eq!(
            inspect(&cov).unwrap().model,
            "google/timesfm-2.0-500m-pytorch"
        );
        // Missing values are null; multivariate rows are fine.
        let mut v = docs_request();
        v["inputs"][0]["target"] = json!([[1, 2], [null, 3], [4, null]]);
        assert!(inspect(&v).is_ok());
    }

    #[test]
    fn inspect_rejects_malformed_requests() {
        let cases = [
            (json!([1]), "object"),
            (json!({"inputs": [{"target": [[1]]}]}), "model"),
            (json!({"model": "m"}), "inputs"),
            (json!({"model": "m", "inputs": []}), "inputs"),
            (
                json!({"model": "m", "inputs": [{"target": [1, 2, 3]}]}),
                "2D",
            ),
            (
                json!({"model": "m", "inputs": [{"target": [[1], ["x"]]}]}),
                "number",
            ),
            (
                json!({"model": "m", "inputs": [{"target": [[1]], "start": "yesterday"}]}),
                "ISO 8601",
            ),
            (
                json!({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"frequency": "every day"}}),
                "offset alias",
            ),
            (
                json!({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"prediction_length": 0}}),
                "prediction_length",
            ),
            (
                json!({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"quantile_levels": [0.5, 1.0]}}),
                "quantile_levels",
            ),
        ];
        for (v, needle) in cases {
            let err = inspect(&v).unwrap_err().0;
            assert!(err.contains(needle), "{v}: {err}");
        }
    }

    #[test]
    fn inspect_rejects_irregular_series() {
        // Ragged channels.
        let mut v = docs_request();
        v["inputs"][0]["target"] = json!([[1, 2], [3], [4, 5]]);
        let err = inspect(&v).unwrap_err().0;
        assert!(
            err.contains("irregular") && err.contains("inputs[0]"),
            "{err}"
        );
        // Past covariates not aligned with the target.
        let mut v = docs_request();
        v["inputs"][0]["past_covariates"] = json!({"promo": [0, 1]});
        assert!(inspect(&v).unwrap_err().0.contains("irregular"));
        // Uneven timestamps.
        let mut v = docs_request();
        v["inputs"][0]["target"] = json!([[1], [2], [3]]);
        v["inputs"][0]["timestamps"] = json!([
            "2026-02-01T00:00:00Z",
            "2026-02-02T00:00:00Z",
            "2026-02-04T00:00:00Z"
        ]);
        let err = inspect(&v).unwrap_err().0;
        assert!(
            err.contains("irregular") && err.contains("timestamps[2]"),
            "{err}"
        );
        // Even timestamps that match start and frequency pass.
        v["inputs"][0]["timestamps"] = json!([
            "2026-02-01T00:00:00Z",
            "2026-02-02T00:00:00Z",
            "2026-02-03T00:00:00Z"
        ]);
        assert!(inspect(&v).is_ok());
        // Even, but not the declared frequency.
        v["parameters"]["frequency"] = json!("1h");
        assert!(inspect(&v).unwrap_err().0.contains("irregular"));
    }

    #[test]
    fn timestamps_follow_the_declared_frequency_or_are_rejected() {
        let req = |freq: &str, ts: &[&str]| {
            json!({"model": "m", "inputs": [{
                "target": ts.iter().map(|_| json!([1])).collect::<Vec<_>>(),
                "timestamps": ts,
            }], "parameters": {"frequency": freq}})
        };
        let daily = [
            "2026-01-04T00:00:00Z",
            "2026-01-05T00:00:00Z",
            "2026-01-06T00:00:00Z",
        ];
        // Daily timestamps do not fit a weekly anchored frequency.
        for f in ["W-SUN", "W", "2W-SUN"] {
            let err = validate(&req(f, &daily)).unwrap_err();
            assert!(err.contains("irregular"), "{f}: {err}");
        }
        // 2026-01-04 is a Sunday: weekly Sundays fit W-SUN and W.
        let sundays = [
            "2026-01-04T00:00:00Z",
            "2026-01-11T00:00:00Z",
            "2026-01-18T00:00:00Z",
        ];
        for f in ["W-SUN", "W"] {
            assert!(validate(&req(f, &sundays)).is_ok(), "{f}");
        }
        // Weekly, but on Wednesdays: not W-SUN, but W-WED.
        let wednesdays = [
            "2026-01-07T00:00:00Z",
            "2026-01-14T00:00:00Z",
            "2026-01-21T00:00:00Z",
        ];
        let err = validate(&req("W-SUN", &wednesdays)).unwrap_err();
        assert!(err.contains("irregular") && err.contains("W-SUN"), "{err}");
        assert!(validate(&req("W-WED", &wednesdays)).is_ok());
        // Calendar anchors are checked in the timestamps' own offset:
        // Sundays at +14:00 are Saturdays in UTC, but still W-SUN.
        let east_sundays = ["2026-01-04T00:00:00+14:00", "2026-01-11T00:00:00+14:00"];
        for f in ["W-SUN", "W"] {
            assert!(validate(&req(f, &east_sundays)).is_ok(), "{f}");
        }
        assert!(validate(&req("W-SAT", &east_sundays)).is_err());
        // Month starts at +01:00 are the last day of a month in UTC.
        let month_starts = [
            "2026-01-01T00:00:00+01:00",
            "2026-02-01T00:00:00+01:00",
            "2026-03-01T00:00:00+01:00",
        ];
        assert!(validate(&req("MS", &month_starts)).is_ok());
        let west = ["2026-01-31T23:00:00-01:00", "2026-02-28T23:00:00-01:00"];
        assert!(validate(&req("MS", &west)).is_err());
        // A start in another offset that names the same instant matches.
        let mut v = req("W-SUN", &east_sundays);
        v["inputs"][0]["start"] = json!("2026-01-03T10:00:00Z");
        assert!(validate(&v).is_ok(), "{v}");
        // A frequency s1 cannot interpret cannot vouch for timestamps.
        for f in ["Q", "B", "W-XYZ", "QS-JAN"] {
            let err = validate(&req(f, &sundays)).unwrap_err();
            assert!(err.contains("cannot check `timestamps`"), "{f}: {err}");
        }
        // Without timestamps such a frequency is passed on as is.
        let v =
            json!({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"frequency": "Q"}});
        assert!(validate(&v).is_ok());
    }

    #[test]
    fn null_metadata_id_falls_back_to_item_id() {
        let resp =
            json!({"outputs": [{"mean": [[1]], "metadata": {"id": null, "item_id": "sku"}}]});
        let rows = flatten_rows(&serde_json::to_vec(&resp).unwrap()).unwrap();
        assert_eq!(rows[0]["id"], "sku");
        let resp = json!({"outputs": [{"mean": [[1]], "metadata": {"id": null, "item_id": null}}]});
        let rows = flatten_rows(&serde_json::to_vec(&resp).unwrap()).unwrap();
        assert_eq!(rows[0]["id"], 0);
    }

    #[test]
    fn error_body_has_tsfm_shape() {
        let body = Tsfm.error_body(&S1Error::BackendUnavailable("no backend".into()));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({"error": "no backend", "code": "backend_unavailable", "endpoint": "/v1/forecast"})
        );
    }

    #[test]
    fn freq_parse_and_alias() {
        assert_eq!(Freq::parse("D").unwrap(), Freq::Seconds(86_400));
        assert_eq!(Freq::parse("15min").unwrap(), Freq::Seconds(900));
        assert_eq!(Freq::parse("1h").unwrap(), Freq::Seconds(3_600));
        assert_eq!(Freq::parse("H").unwrap(), Freq::Seconds(3_600));
        assert_eq!(Freq::parse("W").unwrap(), Freq::Seconds(604_800));
        assert_eq!(Freq::parse("MS").unwrap(), Freq::MonthStart(1));
        assert_eq!(Freq::parse("3MS").unwrap(), Freq::MonthStart(3));
        assert!(Freq::parse("Q").is_err());
        assert!(Freq::parse("0D").is_err());
        assert_eq!(Freq::Seconds(86_400).alias(), "D");
        assert_eq!(Freq::Seconds(604_800).alias(), "7D");
        assert_eq!(Freq::Seconds(3_600).alias(), "1h");
        assert_eq!(Freq::Seconds(900).alias(), "15min");
        assert_eq!(Freq::Seconds(5).alias(), "5s");
        assert_eq!(Freq::MonthStart(1).alias(), "MS");
        for a in ["D", "2D", "1h", "15min", "5s", "MS", "2MS"] {
            assert_eq!(Freq::parse(a).unwrap().alias(), a);
        }
    }

    #[test]
    fn month_grid() {
        let t0 = parse_time("2026-01-01").unwrap();
        let f = Freq::MonthStart(1);
        assert_eq!(f.index(t0, parse_time("2026-03-01").unwrap()), Some(2));
        assert_eq!(f.index(t0, parse_time("2026-03-02").unwrap()), None);
        assert_eq!(
            f.at(t0, 13).map(format_time).unwrap(),
            "2027-02-01T00:00:00Z"
        );
    }

    #[test]
    fn times_parse_in_common_shapes() {
        let want = "2026-02-01T00:00:00Z";
        for s in [
            "2026-02-01T00:00:00Z",
            "2026-02-01T01:00:00+01:00",
            "2026-02-01 00:00:00",
            "2026-02-01T00:00",
            "2026-02-01",
        ] {
            assert_eq!(parse_time(s).map(format_time).as_deref(), Some(want), "{s}");
        }
        assert!(parse_time("02/01/2026").is_none());
    }

    /// Example response from https://tsfm.ai/docs/api.
    fn docs_response() -> Value {
        serde_json::from_slice(include_bytes!(
            "../../tests/fixtures/tsfm/docs.response.json"
        ))
        .unwrap()
    }

    #[test]
    fn flatten_rows_long_format() {
        let rows = flatten_rows(&serde_json::to_vec(&docs_response()).unwrap()).unwrap();
        assert_eq!(rows.len(), 7);
        let first = Value::Object(rows[0].clone());
        assert_eq!(
            first,
            json!({"id": "store_017", "step": 1, "timestamp": "2026-02-11T00:00:00Z",
                   "mean": 478.1, "q0.1": 470.9, "q0.5": 478.1, "q0.9": 485.5})
        );
        let cols: Vec<&String> = rows[6].keys().collect();
        assert_eq!(
            cols,
            ["id", "step", "timestamp", "mean", "q0.1", "q0.5", "q0.9"]
        );
        assert_eq!(rows[6]["step"], 7);
        assert!(Tsfm.flatten(b"{}").is_err());
    }

    #[test]
    fn flatten_rows_multivariate_and_no_metadata() {
        let resp = json!({
            "model": "m", "horizon": 2, "quantile_levels": [0.5],
            "outputs": [{"mean": [[1, 10], [2, 20]],
                         "quantile_predictions": [{"level": 0.5, "values": [[1, 11], [2, 21]]}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        let rows = flatten_rows(&serde_json::to_vec(&resp).unwrap()).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            Value::Object(rows[1].clone()),
            json!({"id": 0, "step": 1, "timestamp": null, "channel": 1, "mean": 10, "q0.5": 11})
        );
    }

    fn level_strategy() -> impl Strategy<Value = Vec<f64>> {
        prop::collection::btree_set(1u32..1000, 1..6).prop_map(|s| {
            let mut v: Vec<f64> = s.into_iter().map(|n| f64::from(n) / 1000.0).collect();
            v.reverse();
            v
        })
    }

    proptest! {
        /// Quantile levels come back as produced: the same levels, in the
        /// produced order, with the produced values; none added.
        #[test]
        fn flatten_keeps_produced_levels(
            levels in level_strategy(),
            steps in 1usize..5,
            seed in 0u32..1000,
        ) {
            let val = |l: usize, s: usize| f64::from(seed) + (l * 100 + s) as f64;
            let qp: Vec<Value> = levels.iter().enumerate().map(|(li, l)| json!({
                "level": l,
                "values": (0..steps).map(|s| json!([val(li, s)])).collect::<Vec<_>>(),
            })).collect();
            let resp = json!({
                "model": "m", "horizon": steps, "quantile_levels": levels,
                "outputs": [{"mean": (0..steps).map(|s| json!([s])).collect::<Vec<_>>(),
                             "quantile_predictions": qp}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
            });
            let rows = flatten_rows(&serde_json::to_vec(&resp).unwrap()).unwrap();
            prop_assert_eq!(rows.len(), steps);
            for (s, row) in rows.iter().enumerate() {
                let qcols: Vec<&String> = row.keys().filter(|k| k.starts_with('q')).collect();
                let want: Vec<String> = levels.iter().map(|l| format!("q{}", json!(l))).collect();
                prop_assert_eq!(qcols.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                                want.iter().map(String::as_str).collect::<Vec<_>>());
                for (li, c) in want.iter().enumerate() {
                    prop_assert_eq!(row[c].as_f64(), Some(val(li, s)));
                }
            }
        }

        /// Evenly spaced timestamps pass; moving one point off the grid fails.
        #[test]
        fn irregular_timestamps_are_rejected(
            n in 2usize..20,
            step_min in 1i64..2000,
            bad in 0usize..20,
            shift in 1i64..59,
        ) {
            let t0 = parse_time("2026-01-01T00:00:00Z").unwrap();
            let f = Freq::Seconds(step_min * 60);
            let times: Vec<DateTime<Utc>> = (0..n as i64).map(|k| f.at(t0, k).unwrap()).collect();
            let req = |ts: &[DateTime<Utc>]| json!({
                "model": "m",
                "inputs": [{"start": format_time(ts[0]),
                            "target": ts.iter().map(|_| json!([1.0])).collect::<Vec<_>>(),
                            "timestamps": ts.iter().map(|t| format_time(*t)).collect::<Vec<_>>()}],
                "parameters": {"frequency": f.alias()}
            });
            prop_assert!(validate(&req(&times)).is_ok());
            let mut off = times.clone();
            let i = 1 + bad % (n - 1);
            off[i] += chrono::TimeDelta::try_seconds(shift).unwrap();
            let err = validate(&req(&off)).unwrap_err();
            prop_assert!(err.contains("irregular"), "{}", err);
        }

        /// Ragged targets fail; rectangular ones pass.
        #[test]
        fn ragged_targets_are_rejected(widths in prop::collection::vec(1usize..4, 2..10)) {
            let target: Vec<Value> = widths.iter().map(|w| json!(vec![1.0; *w])).collect();
            let v = json!({"model": "m", "inputs": [{"target": target}]});
            let rectangular = widths.iter().all(|w| *w == widths[0]);
            prop_assert_eq!(validate(&v).is_ok(), rectangular);
        }
    }
}
