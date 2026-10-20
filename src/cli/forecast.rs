//! `s1 forecast -m <model> --csv sales.csv --id sku --time ds --value y -h 28`.
//!
//! The CSV helper turns long-format rows (one row per series and time)
//! into a TSFM request: it groups rows by `--id`, finds the step (or uses
//! `--freq`), puts each series on a regular grid from its first time, and
//! fills missing points with NaN (sent as `null`, TSFM's missing value).
//! Times that do not fit the grid stop the command; s1 never moves a point.
//!
//! Without `--csv`, stdin holds a whole TSFM request. Output is the TSFM
//! response, or with `--flat` one JSON line per series, step, and channel
//! (`id`, `step`, `timestamp`, `mean`, `q<level>`).
//!
//! [`read_csv`], [`build_grid`], and [`build_request`] are public so that
//! other clients (the MCP `forecast` tool) can reuse them.

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use axum::http::{HeaderMap, HeaderValue, Method, header};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use clap::Args;
use serde_json::{Map, Value, json};

use super::client::{ensure_daemon, request};
use crate::paths::Paths;
use crate::protocol::tsfm::{self, Freq, format_time, is_month_start, parse_time};

/// Alias used when neither the template nor `-m` names a model.
pub const DEFAULT_MODEL: &str = "forecast";
/// Most grid points in one request, over all series together (a guard
/// against a tiny step or sparse series that would expand a small CSV
/// into a huge grid). Checked before any grid is allocated.
pub const MAX_GRID_POINTS: i64 = 1_000_000;

/// Why a grid cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridError {
    /// The rows do not fit a regular grid (or the step cannot be found).
    Invalid(String),
    /// The grid would have more than the point budget.
    TooLong(String),
}

impl std::fmt::Display for GridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GridError::Invalid(m) | GridError::TooLong(m) => f.write_str(m),
        }
    }
}

impl From<GridError> for String {
    fn from(e: GridError) -> String {
        e.to_string()
    }
}

impl From<String> for GridError {
    fn from(m: String) -> GridError {
        GridError::Invalid(m)
    }
}

#[derive(Debug, Args)]
#[command(disable_help_flag = true)]
pub struct ForecastArgs {
    /// Print help (`-h` is the horizon).
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,
    /// Model id or alias (overrides the template's `model`).
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// Request template: a TSFM request without `inputs` (for example
    /// with `parameters` or `metadata`).
    #[arg(short = 't', long = "template")]
    pub template: Option<PathBuf>,
    /// Long-format CSV (`-` for stdin). Without it, stdin holds a TSFM request.
    #[arg(long, requires_all = ["time", "value"])]
    pub csv: Option<PathBuf>,
    /// Column with the series id (default: one series).
    #[arg(long, requires = "csv")]
    pub id: Option<String>,
    /// Column with the time (ISO 8601 date or time).
    #[arg(long, requires = "csv")]
    pub time: Option<String>,
    /// Column with the value; empty, NaN, NA, or null is a missing value.
    #[arg(long, requires = "csv")]
    pub value: Option<String>,
    /// Forecast horizon in steps (`parameters.prediction_length`).
    #[arg(short = 'h', long = "horizon")]
    pub horizon: Option<u32>,
    /// Grid step as a pandas offset alias: s, min, h, D, W, MS with an
    /// optional count (default: inferred from the smallest gap).
    #[arg(long)]
    pub freq: Option<String>,
    /// Quantile levels to ask for, e.g. `0.1,0.5,0.9`.
    #[arg(short = 'q', long, value_delimiter = ',')]
    pub quantiles: Vec<f64>,
    /// Write one JSON line per series, step, and channel.
    #[arg(long)]
    pub flat: bool,
    /// Print the request and exit without sending it.
    #[arg(long)]
    pub print_request: bool,
    /// Skip the cache lookup (the fresh answer is still stored).
    #[arg(long)]
    pub no_cache: bool,
}

/// CSV column names.
#[derive(Debug, Clone, Copy)]
pub struct Columns<'a> {
    pub id: Option<&'a str>,
    pub time: &'a str,
    pub value: &'a str,
}

/// Raw points of one series, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSeries {
    pub id: Option<String>,
    pub points: Vec<(DateTime<Utc>, f64)>,
}

/// One series on the grid: `values[k]` is the value at `freq.at(start, k)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub id: Option<String>,
    pub start: DateTime<Utc>,
    pub values: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub freq: Freq,
    pub series: Vec<Series>,
}

/// Missing-value spellings (compared without case).
fn is_missing(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "" | "nan" | "na" | "n/a" | "null" | "none"
    )
}

/// Read long-format CSV rows into series (first-seen order of ids).
pub fn read_csv<R: Read>(r: R, cols: Columns<'_>) -> Result<Vec<RawSeries>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(r);
    let headers = rdr
        .headers()
        .map_err(|e| format!("cannot read CSV header: {e}"))?
        .clone();
    let col = |name: &str| {
        headers.iter().position(|h| h == name).ok_or_else(|| {
            format!(
                "CSV has no column `{name}` (columns: {})",
                headers.iter().collect::<Vec<_>>().join(", ")
            )
        })
    };
    let id_col = cols.id.map(col).transpose()?;
    let time_col = col(cols.time)?;
    let value_col = col(cols.value)?;
    let mut order: Vec<RawSeries> = Vec::new();
    let mut index: HashMap<Option<String>, usize> = HashMap::new();
    for (n, rec) in rdr.records().enumerate() {
        let line = n + 2;
        let rec = rec.map_err(|e| format!("CSV line {line}: {e}"))?;
        let field = |i: usize| rec.get(i).unwrap_or("");
        let id = id_col.map(|i| field(i).to_string());
        let t = field(time_col);
        let time = parse_time(t)
            .ok_or_else(|| format!("CSV line {line}: `{t}` is not an ISO 8601 date or time"))?;
        let v = field(value_col);
        let value = if is_missing(v) {
            f64::NAN
        } else {
            let x = v
                .parse::<f64>()
                .map_err(|_| format!("CSV line {line}: value `{v}` is not a number"))?;
            // `inf` or `1e999` would be sent as null: refuse, so that only
            // the missing-value spellings become missing values.
            if !x.is_finite() {
                return Err(format!(
                    "CSV line {line}: value `{v}` is not a finite number; leave the cell \
                     empty or write NaN for a missing value"
                ));
            }
            x
        };
        let i = *index.entry(id.clone()).or_insert_with(|| {
            order.push(RawSeries {
                id,
                points: Vec::new(),
            });
            order.len() - 1
        });
        order[i].points.push((time, value));
    }
    if order.is_empty() {
        return Err("CSV has no rows".into());
    }
    Ok(order)
}

fn name(id: &Option<String>) -> String {
    id.as_deref()
        .map_or_else(|| "the series".to_string(), |i| format!("series `{i}`"))
}

/// The step: month starts when every time is a month start and the
/// smallest gap is at least 28 days, else the smallest gap in seconds.
pub fn infer_freq(series: &[RawSeries]) -> Result<Freq, String> {
    let mut min_gap: Option<chrono::TimeDelta> = None;
    let mut min_months: Option<i64> = None;
    let mut all_month_starts = true;
    for s in series {
        let mut times: Vec<DateTime<Utc>> = s.points.iter().map(|p| p.0).collect();
        times.sort();
        all_month_starts &= times.iter().all(|t| is_month_start(*t));
        for w in times.windows(2) {
            let gap = w[1] - w[0];
            if gap > chrono::TimeDelta::zero() {
                min_gap = Some(min_gap.map_or(gap, |g| g.min(gap)));
                let months = Freq::MonthStart(1).index(w[0], w[1]);
                if let Some(m) = months.filter(|m| *m > 0) {
                    min_months = Some(min_months.map_or(m, |x| x.min(m)));
                }
            }
        }
    }
    let gap =
        min_gap.ok_or("cannot infer the step: no series has two different times; pass --freq")?;
    if all_month_starts
        && gap >= chrono::TimeDelta::days(28)
        && let Some(m) = min_months.and_then(|m| u32::try_from(m).ok())
    {
        return Ok(Freq::MonthStart(m));
    }
    if gap.subsec_nanos() != 0 || gap.num_seconds() < 1 {
        return Err(format!(
            "cannot infer the step: the smallest gap ({} ms) is not a whole number of seconds; pass --freq",
            gap.num_milliseconds()
        ));
    }
    Ok(Freq::Seconds(gap.num_seconds()))
}

/// Put each series on a regular grid from its first time. Missing points
/// become NaN. A time off the grid or a repeated time is an error; a grid
/// over [`MAX_GRID_POINTS`] points in total is [`GridError::TooLong`].
pub fn build_grid(raw: Vec<RawSeries>, freq: Option<Freq>) -> Result<Grid, GridError> {
    build_grid_with_budget(raw, freq, MAX_GRID_POINTS)
}

/// [`build_grid`] with a given budget of grid points for all series.
pub fn build_grid_with_budget(
    mut raw: Vec<RawSeries>,
    freq: Option<Freq>,
    budget: i64,
) -> Result<Grid, GridError> {
    for s in &mut raw {
        s.points.sort_by_key(|p| p.0);
        if let Some(w) = s.points.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(GridError::Invalid(format!(
                "{} has two rows at {}",
                name(&s.id),
                format_time(w[0].0)
            )));
        }
    }
    let freq = match freq {
        Some(f) => f,
        None => infer_freq(&raw)?,
    };
    // First the grid index of every point and the total size, so that an
    // oversized grid is refused before anything is allocated for it.
    let mut placed = Vec::with_capacity(raw.len());
    let mut total: i64 = 0;
    for s in raw {
        let Some(&(start, _)) = s.points.first() else {
            continue;
        };
        let mut idx = Vec::with_capacity(s.points.len());
        for (t, _) in &s.points {
            let k = freq.index(start, *t).ok_or_else(|| {
                GridError::Invalid(format!(
                    "irregular series: {} has a time {} that is not on the {} grid from {}; pass --freq with a step that fits",
                    name(&s.id),
                    format_time(*t),
                    freq.alias(),
                    format_time(start)
                ))
            })?;
            idx.push(k);
        }
        let len = idx.last().copied().unwrap_or(0).saturating_add(1);
        total = total.saturating_add(len);
        if total > budget {
            return Err(GridError::TooLong(format!(
                "the series would have more than {budget} grid points in total at step {} \
                 ({} alone has {len}); pass a larger --freq or fewer rows",
                freq.alias(),
                name(&s.id),
            )));
        }
        placed.push((s, start, idx, len));
    }
    let series = placed
        .into_iter()
        .map(|(s, start, idx, len)| {
            let mut values = vec![f64::NAN; len as usize];
            for (k, (_, v)) in idx.into_iter().zip(s.points) {
                values[k as usize] = v;
            }
            Series {
                id: s.id,
                start,
                values,
            }
        })
        .collect();
    Ok(Grid { freq, series })
}

/// The template's `parameters.frequency`. `parameters` must be an object
/// (or absent or null) and `frequency` a string (or absent or null), so
/// that a malformed template is refused, not silently dropped.
pub fn template_frequency(template: &Map<String, Value>) -> Result<Option<&str>, String> {
    let params = match template.get("parameters") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(p)) => p,
        Some(_) => return Err("template: `parameters` must be an object".into()),
    };
    match params.get("frequency") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(f)) => Ok(Some(f)),
        Some(_) => Err("template: `parameters.frequency` must be a string".into()),
    }
}

/// A TSFM request: the template (without `inputs`), `model`, one input
/// per series (`start`, 2D `target` with `null` for NaN, `metadata.id`),
/// and `parameters` with `frequency`, plus `prediction_length` and
/// `quantile_levels` when given.
pub fn build_request(
    template: &Map<String, Value>,
    model: &str,
    grid: &Grid,
    horizon: Option<u32>,
    quantiles: &[f64],
) -> Value {
    let inputs: Vec<Value> = grid
        .series
        .iter()
        .map(|s| {
            let target: Vec<Value> = s
                .values
                .iter()
                .map(|v| json!([if v.is_finite() { json!(v) } else { Value::Null }]))
                .collect();
            let mut input = Map::new();
            input.insert("start".into(), json!(format_time(s.start)));
            input.insert("target".into(), Value::Array(target));
            if let Some(id) = &s.id {
                input.insert("metadata".into(), json!({ "id": id }));
            }
            Value::Object(input)
        })
        .collect();
    let mut params = template
        .get("parameters")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(h) = horizon {
        params.insert("prediction_length".into(), json!(h));
    }
    params.insert("frequency".into(), json!(grid.freq.alias()));
    if !quantiles.is_empty() {
        params.insert("quantile_levels".into(), json!(quantiles));
    }
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("inputs".into(), Value::Array(inputs));
    out.insert("parameters".into(), Value::Object(params));
    for (k, v) in template {
        if !matches!(k.as_str(), "model" | "inputs" | "parameters") {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn load_template(args: &ForecastArgs) -> Result<Map<String, Value>, String> {
    let Some(path) = &args.template else {
        return Ok(Map::new());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read template {}: {e}", path.display()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!(
            "template {}: must be a JSON object",
            path.display()
        )),
        Err(e) => Err(format!("template {}: not JSON: {e}", path.display())),
    }
}

/// The request body for these arguments, from the CSV or from stdin.
pub fn request_body(args: &ForecastArgs, stdin: impl Read) -> Result<Value, String> {
    let template = load_template(args)?;
    let model = args
        .model
        .clone()
        .or_else(|| {
            template
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let Some(csv_path) = &args.csv else {
        let mut v: Value = serde_json::from_reader(stdin)
            .map_err(|e| format!("stdin is not a TSFM request: {e}"))?;
        let obj = v.as_object_mut().ok_or("stdin must hold a JSON object")?;
        if args.model.is_some() || !obj.contains_key("model") {
            obj.insert("model".into(), json!(model));
        }
        // -h, --freq, and -q override the request's `parameters`.
        if args.horizon.is_some() || args.freq.is_some() || !args.quantiles.is_empty() {
            let params = obj
                .entry("parameters")
                .or_insert_with(|| Value::Object(Map::new()));
            if params.is_null() {
                *params = Value::Object(Map::new());
            }
            let params = params
                .as_object_mut()
                .ok_or("stdin request: `parameters` must be an object")?;
            if let Some(h) = args.horizon {
                params.insert("prediction_length".into(), json!(h));
            }
            if let Some(f) = &args.freq {
                params.insert("frequency".into(), json!(f));
            }
            if !args.quantiles.is_empty() {
                params.insert("quantile_levels".into(), json!(args.quantiles));
            }
        }
        return Ok(v);
    };
    let cols = Columns {
        id: args.id.as_deref(),
        time: args.time.as_deref().ok_or("--csv needs --time")?,
        value: args.value.as_deref().ok_or("--csv needs --value")?,
    };
    let raw = if csv_path.as_os_str() == "-" {
        read_csv(stdin, cols)?
    } else {
        let f = std::fs::File::open(csv_path)
            .map_err(|e| format!("cannot open {}: {e}", csv_path.display()))?;
        read_csv(f, cols).map_err(|e| format!("{}: {e}", csv_path.display()))?
    };
    // --freq, else the template's frequency, else inferred.
    let template_freq = template_frequency(&template)?;
    let alias = args.freq.as_deref().or(template_freq);
    let freq = alias.map(Freq::parse).transpose()?;
    let grid = build_grid(raw, freq)?;
    Ok(build_request(
        &template,
        &model,
        &grid,
        args.horizon,
        &args.quantiles,
    ))
}

pub fn run(args: ForecastArgs, paths: &Paths) -> Result<ExitCode, String> {
    if args.csv.is_none() && std::io::stdin().is_terminal() {
        return Err("pass --csv <file>, or pipe a TSFM request to stdin".into());
    }
    let body = request_body(&args, std::io::stdin().lock())?;
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| format!("cannot write stdout: {e}");
    if args.print_request {
        writeln!(out, "{body}").map_err(io)?;
        return Ok(ExitCode::SUCCESS);
    }
    // Check here too, so that a bad CSV or stdin request fails before the
    // daemon starts.
    tsfm::validate(&body)?;
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if args.no_cache {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::POST,
        tsfm::ENDPOINT,
        h,
        Bytes::from(body.to_string()),
    ))?;
    if reply.status.is_success() && args.flat {
        let rows = tsfm::flatten_rows(&reply.body)
            .map_err(|e| format!("cannot flatten the response: {e}"))?;
        for row in rows {
            writeln!(out, "{}", Value::Object(row)).map_err(io)?;
        }
    } else {
        out.write_all(&reply.body).map_err(io)?;
        if !reply.body.ends_with(b"\n") {
            out.write_all(b"\n").map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    Ok(if reply.status.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const COLS: Columns<'static> = Columns {
        id: Some("sku"),
        time: "ds",
        value: "y",
    };

    fn grid(csv: &str, freq: Option<&str>) -> Result<Grid, String> {
        let raw = read_csv(csv.as_bytes(), COLS)?;
        Ok(build_grid(raw, freq.map(|f| Freq::parse(f).unwrap()))?)
    }

    fn vals(s: &Series) -> Vec<Option<f64>> {
        s.values
            .iter()
            .map(|v| v.is_finite().then_some(*v))
            .collect()
    }

    #[test]
    fn builds_a_daily_grid_and_fills_gaps_with_nan() {
        let csv = "sku,ds,y\n\
                   a,2026-02-01,1\n\
                   b,2026-02-02,10\n\
                   a,2026-02-04,4\n\
                   a,2026-02-02,2\n\
                   b,2026-02-03,\n\
                   b,2026-02-04,NaN\n\
                   b,2026-02-05,13\n";
        let g = grid(csv, None).unwrap();
        assert_eq!(g.freq, Freq::Seconds(86_400));
        assert_eq!(g.series.len(), 2);
        assert_eq!(g.series[0].id.as_deref(), Some("a"));
        assert_eq!(format_time(g.series[0].start), "2026-02-01T00:00:00Z");
        assert_eq!(vals(&g.series[0]), [Some(1.0), Some(2.0), None, Some(4.0)]);
        assert_eq!(format_time(g.series[1].start), "2026-02-02T00:00:00Z");
        assert_eq!(vals(&g.series[1]), [Some(10.0), None, None, Some(13.0)]);

        let req = build_request(&Map::new(), "forecast", &g, Some(28), &[0.1, 0.5, 0.9]);
        assert_eq!(
            req,
            json!({
                "model": "forecast",
                "inputs": [
                    {"start": "2026-02-01T00:00:00Z", "target": [[1.0], [2.0], [null], [4.0]], "metadata": {"id": "a"}},
                    {"start": "2026-02-02T00:00:00Z", "target": [[10.0], [null], [null], [13.0]], "metadata": {"id": "b"}}
                ],
                "parameters": {"prediction_length": 28, "frequency": "D", "quantile_levels": [0.1, 0.5, 0.9]}
            })
        );
        // The request the helper builds passes the daemon's check.
        assert_eq!(tsfm::validate(&req).unwrap(), "forecast");
    }

    #[test]
    fn hourly_monthly_and_explicit_steps() {
        let g = grid(
            "sku,ds,y\nx,2026-01-01T00:00:00Z,1\nx,2026-01-01T03:00:00Z,2\nx,2026-01-01T01:00:00Z,3\n",
            None,
        )
        .unwrap();
        assert_eq!(g.freq.alias(), "1h");
        assert_eq!(vals(&g.series[0]), [Some(1.0), Some(3.0), None, Some(2.0)]);

        let g = grid(
            "sku,ds,y\nx,2026-01-01,1\nx,2026-02-01,2\nx,2026-04-01,4\n",
            None,
        )
        .unwrap();
        assert_eq!(g.freq, Freq::MonthStart(1));
        assert_eq!(vals(&g.series[0]), [Some(1.0), Some(2.0), None, Some(4.0)]);

        // Weekly data on a daily grid when asked.
        let g = grid("sku,ds,y\nx,2026-01-01,1\nx,2026-01-08,2\n", Some("D")).unwrap();
        assert_eq!(g.series[0].values.len(), 8);
    }

    #[test]
    fn rejects_what_does_not_fit_a_grid() {
        // 2-day and 3-day gaps: the inferred 2-day step does not fit.
        let err = grid(
            "sku,ds,y\nx,2026-01-01,1\nx,2026-01-03,2\nx,2026-01-06,3\n",
            None,
        )
        .unwrap_err();
        assert!(err.contains("irregular") && err.contains("--freq"), "{err}");
        let err = grid("sku,ds,y\nx,2026-01-01,1\nx,2026-01-01,2\n", None).unwrap_err();
        assert!(err.contains("two rows"), "{err}");
        let err = grid("sku,ds,y\nx,2026-01-01,1\ny,2026-01-01,2\n", None).unwrap_err();
        assert!(err.contains("pass --freq"), "{err}");
        let err = grid("sku,ds,y\nx,yesterday,1\n", None).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        let err = grid("sku,ds,y\nx,2026-01-01,abc\n", None).unwrap_err();
        assert!(err.contains("not a number"), "{err}");
        let err = read_csv("a,b\n1,2\n".as_bytes(), COLS).unwrap_err();
        assert!(err.contains("no column `sku`"), "{err}");
    }

    #[test]
    fn grid_points_have_one_budget_for_all_series() {
        // Each series is under the per-series limit, but together they
        // are over it: refused before any grid is allocated.
        let mut csv = String::from("sku,ds,y\n");
        for id in ["a", "b", "c"] {
            csv.push_str(&format!("{id},2026-01-01T00:00:00Z,1\n"));
            csv.push_str(&format!("{id},2026-01-07T22:40:00Z,2\n")); // +600,000 s
        }
        let raw = read_csv(csv.as_bytes(), COLS).unwrap();
        let err = build_grid(raw, Some(Freq::Seconds(1))).unwrap_err();
        assert!(matches!(err, GridError::TooLong(_)), "{err}");
        assert!(err.to_string().contains("grid points"), "{err}");
        // Exactly at the budget is fine.
        let csv = "sku,ds,y\na,2026-01-01T00:00:00Z,1\na,2026-01-01T00:00:09Z,2\n\
                   b,2026-01-01T00:00:00Z,1\n";
        let raw = read_csv(csv.as_bytes(), COLS).unwrap();
        let g = build_grid_with_budget(raw, Some(Freq::Seconds(1)), 11).unwrap();
        assert_eq!(g.series[0].values.len() + g.series[1].values.len(), 11);
        let raw = read_csv(csv.as_bytes(), COLS).unwrap();
        assert!(matches!(
            build_grid_with_budget(raw, Some(Freq::Seconds(1)), 10),
            Err(GridError::TooLong(_))
        ));
    }

    #[test]
    fn template_parameters_and_frequency_must_be_well_formed() {
        assert_eq!(template_frequency(&Map::new()).unwrap(), None);
        let t = |v: Value| -> Map<String, Value> { serde_json::from_value(v).unwrap() };
        assert_eq!(
            template_frequency(&t(json!({"parameters": {"frequency": "D"}}))).unwrap(),
            Some("D")
        );
        assert_eq!(
            template_frequency(&t(json!({"parameters": null}))).unwrap(),
            None
        );
        assert_eq!(
            template_frequency(&t(json!({"parameters": {"frequency": null}}))).unwrap(),
            None
        );
        let err = template_frequency(&t(json!({"parameters": 3}))).unwrap_err();
        assert!(err.contains("`parameters` must be an object"), "{err}");
        let err = template_frequency(&t(json!({"parameters": {"frequency": 7}}))).unwrap_err();
        assert!(
            err.contains("`parameters.frequency` must be a string"),
            "{err}"
        );
        // The CLI refuses such a template instead of dropping it.
        let dir = tempfile::tempdir().unwrap();
        let tpl = dir.path().join("t.json");
        let csv = dir.path().join("s.csv");
        std::fs::write(&csv, "ds,y\n2026-01-01,1\n2026-01-02,2\n").unwrap();
        for bad in [
            json!({"parameters": [1]}),
            json!({"parameters": {"frequency": 1}}),
        ] {
            std::fs::write(&tpl, bad.to_string()).unwrap();
            let a = args(&[
                "--csv",
                csv.to_str().unwrap(),
                "--time",
                "ds",
                "--value",
                "y",
                "-t",
                tpl.to_str().unwrap(),
            ]);
            assert!(request_body(&a, &b""[..]).is_err(), "{bad}");
        }
    }

    #[test]
    fn non_finite_values_are_rejected_not_sent_as_missing() {
        for v in [
            "inf",
            "-inf",
            "+inf",
            "Infinity",
            "-infinity",
            "1e999",
            "-1e999",
        ] {
            let err = grid(&format!("sku,ds,y\nx,2026-01-01,{v}\n"), None).unwrap_err();
            assert!(
                err.contains("line 2") && err.contains("finite"),
                "{v}: {err}"
            );
        }
        // The missing-value spellings stay missing.
        let g = grid("sku,ds,y\nx,2026-01-01,NaN\nx,2026-01-02,1\n", None).unwrap();
        assert_eq!(vals(&g.series[0]), [None, Some(1.0)]);
    }

    fn args(extra: &[&str]) -> ForecastArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ForecastArgs,
        }
        let mut argv = vec!["forecast"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).unwrap().args
    }

    #[test]
    fn stdin_request_takes_horizon_freq_and_quantiles() {
        let stdin = serde_json::to_vec(&json!({
            "model": "m",
            "inputs": [{"target": [[1], [2]], "start": "2026-01-04"}],
            "parameters": {"prediction_length": 3, "frequency": "D", "context_length": 8}
        }))
        .unwrap();
        let a = args(&["-h", "7", "--freq", "W-SUN", "-q", "0.1,0.9"]);
        let v = request_body(&a, stdin.as_slice()).unwrap();
        assert_eq!(
            v["parameters"],
            json!({"prediction_length": 7, "frequency": "W-SUN", "context_length": 8, "quantile_levels": [0.1, 0.9]})
        );
        // Without the flags the request is unchanged.
        let v = request_body(&args(&[]), stdin.as_slice()).unwrap();
        assert_eq!(
            v["parameters"],
            json!({"prediction_length": 3, "frequency": "D", "context_length": 8})
        );
        // A request without `parameters` gets them.
        let bare = br#"{"model": "m", "inputs": [{"target": [[1]]}]}"#;
        let v = request_body(&args(&["-h", "2"]), bare.as_slice()).unwrap();
        assert_eq!(v["parameters"], json!({"prediction_length": 2}));
        // `parameters` that is not an object is an error, not overwritten.
        let bad = br#"{"model": "m", "inputs": [{"target": [[1]]}], "parameters": 3}"#;
        assert!(request_body(&args(&["-h", "2"]), bad.as_slice()).is_err());
    }

    #[test]
    fn csv_columns_need_csv() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ForecastArgs,
        }
        for extra in [["--id", "sku"], ["--time", "ds"], ["--value", "y"]] {
            let mut argv = vec!["forecast"];
            argv.extend_from_slice(&extra);
            assert!(Cli::try_parse_from(argv).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn single_series_without_id_column() {
        let raw = read_csv(
            "ds,y\n2026-01-01,1\n2026-01-02,2\n".as_bytes(),
            Columns {
                id: None,
                time: "ds",
                value: "y",
            },
        )
        .unwrap();
        let g = build_grid(raw, None).unwrap();
        let req = build_request(&Map::new(), "m", &g, None, &[]);
        assert!(req["inputs"][0].get("metadata").is_none());
        assert!(req["parameters"].get("prediction_length").is_none());
        assert_eq!(req["parameters"]["frequency"], "D");
    }

    #[test]
    fn template_keeps_its_fields() {
        let g = grid("sku,ds,y\nx,2026-01-01,1\nx,2026-01-02,2\n", None).unwrap();
        let t: Map<String, Value> = serde_json::from_value(json!({
            "model": "tpl-model",
            "inputs": [{"target": [[0]]}],
            "parameters": {"context_length": 256, "quantile_levels": [0.2]},
            "metadata": {"run": "x"}
        }))
        .unwrap();
        let req = build_request(&t, "m", &g, Some(3), &[]);
        assert_eq!(req["model"], "m");
        assert_eq!(req["inputs"].as_array().unwrap().len(), 1);
        assert_eq!(req["inputs"][0]["metadata"]["id"], "x");
        assert_eq!(
            req["parameters"],
            json!({"context_length": 256, "quantile_levels": [0.2], "prediction_length": 3, "frequency": "D"})
        );
        assert_eq!(req["metadata"], json!({"run": "x"}));
    }

    proptest! {
        /// For any series on a step with random gaps and shuffled rows,
        /// the grid spans first to last time, keeps every value at its
        /// index, and has NaN exactly at the gaps.
        #[test]
        fn grid_has_values_at_their_index_and_nan_in_gaps(
            step_idx in 0usize..5,
            present in prop::collection::vec(any::<bool>(), 2..40),
            shuffle_seed in any::<u64>(),
        ) {
            let steps = ["15min", "1h", "D", "7D", "MS"];
            let f = Freq::parse(steps[step_idx]).unwrap();
            let t0 = parse_time("2026-01-01T00:00:00Z").unwrap();
            let mut ks: Vec<i64> = present.iter().enumerate()
                .filter(|(_, p)| **p).map(|(k, _)| k as i64).collect();
            // Keep the first and last so the span is known; need 2 points.
            if ks.first() != Some(&0) { ks.insert(0, 0); }
            let last = present.len() as i64 - 1;
            if ks.last() != Some(&last) { ks.push(last); }
            let mut rows: Vec<(i64, String)> = ks.iter()
                .map(|k| (*k, format_time(f.at(t0, *k).unwrap()))).collect();
            // Deterministic shuffle.
            let n = rows.len();
            for i in (1..n).rev() {
                let j = (shuffle_seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64) % (i as u64 + 1)) as usize;
                rows.swap(i, j);
            }
            let mut csv = String::from("sku,ds,y\n");
            for (k, t) in &rows {
                csv.push_str(&format!("s,{t},{k}\n"));
            }
            let g = grid(&csv, Some(steps[step_idx])).unwrap();
            let s = &g.series[0];
            prop_assert_eq!(s.values.len() as i64, last + 1);
            prop_assert_eq!(s.start, t0);
            for (k, v) in s.values.iter().enumerate() {
                if ks.contains(&(k as i64)) {
                    prop_assert_eq!(*v, k as f64);
                } else {
                    prop_assert!(v.is_nan());
                }
            }
            // Inference finds the same grid unless gaps make the smallest
            // gap a multiple of the step.
            if ks.windows(2).any(|w| w[1] - w[0] == 1) {
                let inferred = grid(&csv, None).unwrap();
                prop_assert_eq!(inferred.freq, f);
            }
            // And the request passes the daemon's check.
            let req = build_request(&Map::new(), "m", &g, Some(4), &[0.5]);
            prop_assert!(tsfm::validate(&req).is_ok());
        }
    }
}
