//! The `forecast` tool: one TSFM request, or a long-format CSV file
//! (`csv_path` plus column names) turned into one with the same grid
//! builder as `s1 forecast --csv`. The request goes through the typed
//! `/v1/forecast` ingress in-process; the result is the TSFM response.
//!
//! `csv_path` is read on both listeners. MCP over TCP needs the local
//! bearer token and listens on loopback only, and the spec lists
//! `csv_path` as an MCP input. The path must be absolute and name a
//! regular file of at most [`MAX_CSV_FILE`] bytes.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use serde_json::{Map, Value, json};

use super::tools::{CallCtx, Tool, ToolOutput, family_models};
use crate::catalog::Catalog;
use crate::cli::forecast::{
    Columns, DEFAULT_MODEL, GridError, build_grid, build_request, read_csv, template_frequency,
};
use crate::daemon::ingress;
use crate::error::S1Error;
use crate::protocol::tsfm::Freq;
use crate::protocol::{Family, ProtocolId};

/// Largest CSV file the tool reads (the ingress body limit).
pub const MAX_CSV_FILE: u64 = crate::daemon::raw::MAX_BODY as u64;

/// Arguments that select and describe the CSV; everything else is the
/// TSFM request (or, with `csv_path`, its template).
const CSV_ARGS: [&str; 4] = ["csv_path", "id_column", "time_column", "value_column"];

pub struct Forecast;

#[async_trait]
impl Tool for Forecast {
    fn name(&self) -> &'static str {
        "forecast"
    }

    fn definition(&self, catalog: &Catalog) -> Value {
        let models = family_models(catalog, Family::Forecast);
        let models = if models.is_empty() {
            "none in this catalog (calls return backend_unavailable)".to_string()
        } else {
            models.join(", ")
        };
        json!({
            "name": "forecast",
            "title": "Forecast (TSFM)",
            "description": format!(
                "Forecasts time series in the TSFM.ai /v1/forecast format. Pass a TSFM request \
                 (`model`, `inputs: [{{target: [[v], ...], start?}}]`, `parameters: \
                 {{prediction_length, frequency, quantile_levels}}`), or `csv_path` (an absolute \
                 path to a long-format CSV file) with `time_column`, `value_column`, and optional \
                 `id_column`; then `model`, `parameters`, and `metadata` are the request \
                 template, and s1 puts each series on a regular grid (`parameters.frequency`, \
                 else the inferred step) with null for missing points. Returns the TSFM \
                 response; quantile levels are as the backend produced them. Models and \
                 aliases: {models}."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "A model id or alias (default `forecast`)."},
                    "inputs": {
                        "type": "array",
                        "minItems": 1,
                        "description": "TSFM series: `target` (2D, [timesteps][channels], null = missing), optional `start`, covariates, `metadata`.",
                        "items": {"type": "object"},
                    },
                    "parameters": {
                        "type": "object",
                        "description": "TSFM parameters: `prediction_length`, `frequency` (pandas offset alias), `quantile_levels`, ...",
                    },
                    "metadata": {"type": "object"},
                    "csv_path": {"type": "string", "description": "Absolute path to a long-format CSV file (instead of `inputs`)."},
                    "id_column": {"type": "string", "description": "CSV column with the series id (default: one series)."},
                    "time_column": {"type": "string", "description": "CSV column with ISO 8601 times."},
                    "value_column": {"type": "string", "description": "CSV column with values; empty, NaN, NA, or null is missing."},
                },
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string"},
                    "quantile_levels": {"type": "array", "items": {"type": "number"}},
                    "outputs": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "mean": {"type": "array"},
                                // TSFM allows null here, for
                                // `timestamps` (no `start`), and for
                                // `metadata`.
                                "quantile_predictions": {
                                    "type": ["array", "null"],
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "level": {"type": "number"},
                                            "values": {"type": "array"},
                                        },
                                        "required": ["level", "values"],
                                    },
                                },
                                "timestamps": {"type": ["array", "null"], "items": {"type": "string"}},
                                "metadata": {"type": ["object", "null"]},
                            },
                            "required": ["mean"],
                        },
                    },
                    "usage": {"type": "object"},
                },
                "required": ["outputs"],
            },
        })
    }

    async fn call(&self, ctx: CallCtx, args: Value) -> ToolOutput {
        let body = match tokio::task::spawn_blocking(move || request_body(args)).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return ToolOutput::error(e.code(), e.to_string()),
            Err(e) => return ToolOutput::error("internal", e.to_string()),
        };
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let resp = ingress::handle(
            State(ctx.state),
            ProtocolId::Tsfm,
            ctx.origin,
            h,
            Body::from(body.to_string()),
        )
        .await;
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        output(status, &bytes)
    }
}

/// The tool result for one `/v1/forecast` reply.
fn output(status: u16, body: &[u8]) -> ToolOutput {
    let v: Option<Value> = serde_json::from_slice(body).ok();
    if (200..300).contains(&status) {
        return match v {
            Some(v) if v.get("outputs").is_some_and(Value::is_array) => ToolOutput::Ok(v),
            _ => ToolOutput::Err(json!({
                "code": "bad_response",
                "message": "the provider response has no `outputs` array",
                "status": status,
            })),
        };
    }
    // TSFM errors: {"error": "<message>", "code": "<code>", "endpoint"}.
    if let Some(v) = &v
        && let (Some(msg), Some(code)) = (
            v.get("error").and_then(Value::as_str),
            v.get("code").and_then(Value::as_str),
        )
    {
        return ToolOutput::Err(json!({"code": code, "message": msg, "status": status}));
    }
    ToolOutput::Err(crate::cli::stream::reply_error(status, body))
}

fn invalid(msg: impl Into<String>) -> S1Error {
    S1Error::InvalidRequest(msg.into())
}

/// The TSFM request for the tool arguments.
fn request_body(args: Value) -> Result<Value, S1Error> {
    let Value::Object(mut args) = args else {
        return Err(invalid("arguments must be an object"));
    };
    let mut csv = Map::new();
    for k in CSV_ARGS {
        if let Some(v) = args.remove(k) {
            csv.insert(k.into(), v);
        }
    }
    let Some(path) = csv.get("csv_path") else {
        if let Some(k) = csv.keys().next() {
            return Err(invalid(format!("`{k}` needs `csv_path`")));
        }
        if !args.contains_key("inputs") {
            return Err(invalid(
                "pass a TSFM request with `inputs`, or `csv_path` with column names",
            ));
        }
        if !args.contains_key("model") {
            args.insert("model".into(), json!(DEFAULT_MODEL));
        }
        return Ok(Value::Object(args));
    };
    if args.contains_key("inputs") {
        return Err(invalid("pass `inputs` or `csv_path`, not both"));
    }
    let path = path
        .as_str()
        .ok_or_else(|| invalid("`csv_path` must be a string"))?;
    let column = |k: &str| -> Result<Option<&str>, S1Error> {
        match csv.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(invalid(format!("`{k}` must be a string"))),
        }
    };
    let cols = Columns {
        id: column("id_column")?,
        time: column("time_column")?.ok_or_else(|| invalid("`csv_path` needs `time_column`"))?,
        value: column("value_column")?.ok_or_else(|| invalid("`csv_path` needs `value_column`"))?,
    };
    let data = read_csv_file(Path::new(path))?;
    let raw = read_csv(data.as_slice(), cols).map_err(|e| invalid(format!("{path}: {e}")))?;
    let model = match args.get("model") {
        None | Some(Value::Null) => DEFAULT_MODEL.to_string(),
        Some(Value::String(m)) => m.clone(),
        Some(_) => return Err(invalid("`model` must be a string")),
    };
    let freq = template_frequency(&args)
        .map_err(invalid)?
        .map(Freq::parse)
        .transpose()
        .map_err(invalid)?;
    let grid = build_grid(raw, freq).map_err(|e| match e {
        GridError::Invalid(m) => invalid(m),
        GridError::TooLong(m) => S1Error::TooLong(m),
    })?;
    Ok(build_request(&args, &model, &grid, None, &[]))
}

/// Read a CSV file: an absolute path to a regular file of at most
/// [`MAX_CSV_FILE`] bytes.
fn read_csv_file(path: &Path) -> Result<Vec<u8>, S1Error> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(invalid(format!("`csv_path` {shown} must be absolute")));
    }
    let cannot = |e: std::io::Error| invalid(format!("cannot read `csv_path` {shown}: {e}"));
    // Open once and check that handle. O_NONBLOCK: opening a FIFO must not
    // wait for a writer (it has no effect on regular files).
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(cannot)?;
    let meta = file.metadata().map_err(cannot)?;
    if !meta.is_file() {
        return Err(invalid(format!("`csv_path` {shown} is not a regular file")));
    }
    let too_long = || {
        S1Error::TooLong(format!(
            "`csv_path` {shown} is over {MAX_CSV_FILE} bytes (64 MiB)"
        ))
    };
    if meta.len() > MAX_CSV_FILE {
        return Err(too_long());
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_CSV_FILE + 1)
        .read_to_end(&mut data)
        .map_err(cannot)?;
    if data.len() as u64 > MAX_CSV_FILE {
        return Err(too_long());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tsfm_request_passes_unchanged_and_gets_a_default_model() {
        let req =
            json!({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"frequency": "Q"}});
        assert_eq!(request_body(req.clone()).unwrap(), req);
        let v = request_body(json!({"inputs": [{"target": [[1]]}]})).unwrap();
        assert_eq!(v["model"], DEFAULT_MODEL);
        assert!(request_body(json!({"model": "m"})).is_err());
        assert!(request_body(json!([1])).is_err());
    }

    #[test]
    fn csv_template_fields_are_kept_and_frequency_sets_the_grid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.csv");
        std::fs::write(&p, "ds,y\n2026-01-01,1\n2026-01-08,2\n").unwrap();
        let v = request_body(json!({
            "csv_path": p, "time_column": "ds", "value_column": "y",
            "parameters": {"frequency": "D", "prediction_length": 3},
            "metadata": {"run": "x"},
        }))
        .unwrap();
        assert_eq!(v["model"], DEFAULT_MODEL);
        assert_eq!(v["inputs"][0]["target"].as_array().unwrap().len(), 8);
        assert_eq!(
            v["parameters"],
            json!({"frequency": "D", "prediction_length": 3})
        );
        assert_eq!(v["metadata"], json!({"run": "x"}));
        // A frequency the grid builder cannot use is an error.
        let e = request_body(json!({
            "csv_path": p, "time_column": "ds", "value_column": "y",
            "parameters": {"frequency": "Q"},
        }))
        .unwrap_err();
        assert_eq!(e.code(), "invalid_request");
    }

    #[test]
    fn many_sparse_series_are_too_long_before_any_grid_is_built() {
        // About 2 KB of CSV: 40 series, two points 999,999 s apart, step
        // 1 s. That is 40M grid points in total; refused as too_long.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sparse.csv");
        let mut csv = String::from("id,ds,y\n");
        for i in 0..40 {
            csv.push_str(&format!("s{i},2026-01-01T00:00:00Z,1\n"));
            csv.push_str(&format!("s{i},2026-01-12T13:46:39Z,2\n"));
        }
        std::fs::write(&p, &csv).unwrap();
        let e = request_body(json!({
            "csv_path": p, "id_column": "id", "time_column": "ds", "value_column": "y",
            "parameters": {"frequency": "s"},
        }))
        .unwrap_err();
        assert_eq!(e.code(), "too_long", "{e}");
    }

    #[test]
    fn malformed_template_parameters_are_refused_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.csv");
        std::fs::write(&p, "ds,y\n2026-01-01,1\n2026-01-02,2\n").unwrap();
        for (params, needle) in [
            (json!([1]), "`parameters` must be an object"),
            (json!("D"), "`parameters` must be an object"),
            (
                json!({"frequency": 7}),
                "`parameters.frequency` must be a string",
            ),
        ] {
            let e = request_body(json!({
                "csv_path": p, "time_column": "ds", "value_column": "y",
                "parameters": params,
            }))
            .unwrap_err();
            assert_eq!(e.code(), "invalid_request", "{params}");
            assert!(e.to_string().contains(needle), "{params}: {e}");
        }
        // Null parameters or frequency mean "not given".
        for params in [json!(null), json!({"frequency": null})] {
            let v = request_body(json!({
                "csv_path": p, "time_column": "ds", "value_column": "y",
                "parameters": params,
            }))
            .unwrap();
            assert_eq!(v["parameters"]["frequency"], "D", "{params}");
        }
    }

    #[test]
    fn csv_file_size_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.csv");
        // Sparse files: no disk use.
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_CSV_FILE + 1).unwrap();
        let e = read_csv_file(&p).unwrap_err();
        assert_eq!(e.code(), "too_long", "{e}");
        f.set_len(MAX_CSV_FILE).unwrap();
        assert_eq!(read_csv_file(&p).unwrap().len() as u64, MAX_CSV_FILE);
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pipe.csv");
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = read_csv_file(&p).unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        assert!(e.to_string().contains("not a regular file"), "{e}");
    }

    #[test]
    fn a_symlink_to_a_regular_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.csv");
        std::fs::write(&real, "ds,y\n2026-01-01,1\n2026-01-02,2\n").unwrap();
        let link = dir.path().join("link.csv");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let v = request_body(json!({
            "csv_path": link, "time_column": "ds", "value_column": "y",
        }))
        .unwrap();
        assert_eq!(v["inputs"][0]["target"], json!([[1.0], [2.0]]));
        // A symlink to a directory is still not a regular file.
        let dlink = dir.path().join("dir.csv");
        std::os::unix::fs::symlink(dir.path(), &dlink).unwrap();
        assert!(
            read_csv_file(&dlink)
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[test]
    fn tsfm_errors_become_tool_errors_with_their_code() {
        let out = output(
            503,
            br#"{"error":"no backend","code":"backend_unavailable","endpoint":"/v1/forecast"}"#,
        );
        assert_eq!(
            out,
            ToolOutput::Err(
                json!({"code": "backend_unavailable", "message": "no backend", "status": 503})
            )
        );
        assert!(matches!(output(200, b"{}"), ToolOutput::Err(_)));
        assert!(matches!(
            output(200, br#"{"outputs": []}"#),
            ToolOutput::Ok(_)
        ));
    }
}
