//! `POST /v1/forecast`: TSFM ingress checks, the no-backend error, and a
//! mock TSFM backend registered through the `models.toml` overlay.

mod tsfm_mock;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::protocol::tsfm::flatten_rows;
use s1::secrets::{KeyResolver, Secret};
use serde_json::{Value, json};
use tsfm_mock::{PRODUCED_LEVELS, Recorder, TSFM_KEY, fixture};

struct Harness {
    dir: tempfile::TempDir,
    paths: Paths,
    server: RunningServer,
    mock: Recorder,
    token: String,
}

/// `overlay`: register the mock provider. `key`: store a key for it.
async fn harness(overlay: bool, key: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    if let Some(k) = key {
        keys.update(|f| f.set_api_key("tsfm", &Secret::new(k), None))
            .unwrap();
    }
    let token = keys.ensure_local_token().unwrap();
    let token_text = token.expose().to_string();
    let (addr, mock) = tsfm_mock::start().await;
    let catalog = if overlay {
        Catalog::with_overlay_str(&tsfm_mock::overlay(addr)).unwrap()
    } else {
        Catalog::builtin()
    };
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(catalog, keys),
        token,
    })
    .await
    .unwrap();
    Harness {
        dir,
        paths,
        server,
        mock,
        token: token_text,
    }
}

impl Harness {
    async fn post(&self, body: &[u8]) -> Reply {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        request(
            &self.paths.socket,
            Method::POST,
            "/v1/forecast",
            h,
            Bytes::copy_from_slice(body),
        )
        .await
        .unwrap()
    }
}

fn json_of(r: &Reply) -> Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
}

fn docs_request_with_model(model: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&fixture("docs.request.json")).unwrap();
    v["model"] = json!(model);
    serde_json::to_vec(&v).unwrap()
}

#[tokio::test]
async fn without_a_backend_requests_get_backend_unavailable_in_tsfm_format() {
    let h = harness(false, None).await;
    for model in ["forecast", "amazon/chronos-bolt-small"] {
        let r = h.post(&docs_request_with_model(model)).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{model}");
        let v = json_of(&r);
        assert_eq!(v["code"], "backend_unavailable");
        assert_eq!(v["endpoint"], "/v1/forecast");
        assert!(v["error"].as_str().unwrap().contains("forecast"), "{v}");
        assert!(r.headers.contains_key("s1-request-id"));
    }
    assert!(h.mock.all().is_empty());
}

#[tokio::test]
async fn provider_without_key_is_backend_unavailable() {
    let h = harness(true, None).await;
    let r = h.post(&docs_request_with_model("forecast")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    let v = json_of(&r);
    assert_eq!(v["code"], "backend_unavailable");
    assert!(
        v["error"].as_str().unwrap().contains("s1 auth set tsfm"),
        "{v}"
    );
    assert!(h.mock.all().is_empty());
}

#[tokio::test]
async fn irregular_and_malformed_series_are_rejected_before_the_backend() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let bad = [
        json!({"model": "forecast", "inputs": [{"target": [[1, 2], [3]]}]}),
        json!({"model": "forecast", "inputs": [{"target": [1, 2, 3]}]}),
        json!({"model": "forecast", "inputs": [{
            "target": [[1], [2], [3]],
            "timestamps": ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z", "2026-01-05T00:00:00Z"]
        }], "parameters": {"frequency": "D"}}),
        json!({"model": "forecast", "inputs": [{"target": [[1], [2]], "past_covariates": {"promo": [1]}}]}),
    ];
    for b in bad {
        let r = h.post(&serde_json::to_vec(&b).unwrap()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{b}");
        let v = json_of(&r);
        assert_eq!(v["code"], "invalid_request", "{v}");
        assert_eq!(v["endpoint"], "/v1/forecast");
    }
    assert!(h.mock.all().is_empty());
}

#[tokio::test]
async fn mock_backend_from_overlay_gets_the_request_and_returns_it_unchanged() {
    let h = harness(true, Some(TSFM_KEY)).await;
    // The docs request by alias: s1 sets only `model` to the upstream id.
    let r = h.post(&docs_request_with_model("forecast")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.body.as_ref(), fixture("docs.response.json").as_slice());
    assert_eq!(r.headers["s1-backend"], "tsfm");
    assert_eq!(r.headers["s1-model"], "tsfm/chronos-bolt-small");
    assert_eq!(r.headers["x-ratelimit-remaining"], "999");
    let seen = h.mock.last();
    assert_eq!(seen.headers["authorization"], format!("Bearer {TSFM_KEY}"));
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    let docs: Value = serde_json::from_slice(&fixture("docs.request.json")).unwrap();
    assert_eq!(sent, docs);
}

#[tokio::test]
async fn quantile_levels_come_back_as_produced() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let req = json!({
        "model": "tsfm/chronos-bolt-small",
        "inputs": [{"start": "2026-01-01T00:00:00Z", "target": [[1], [null], [3]], "metadata": {"id": "a"}}],
        "parameters": {"prediction_length": 2, "frequency": "D", "quantile_levels": [0.05, 0.25, 0.75]}
    });
    let r = h.post(&serde_json::to_vec(&req).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = json_of(&r);
    // Not the requested levels, not interpolated to them.
    assert_eq!(v["quantile_levels"], json!(PRODUCED_LEVELS));
    let levels: Vec<f64> = v["outputs"][0]["quantile_predictions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["level"].as_f64().unwrap())
        .collect();
    assert_eq!(levels, PRODUCED_LEVELS);
    let rows = flatten_rows(&r.body).unwrap();
    assert_eq!(rows.len(), 2);
    let cols: Vec<&String> = rows[0].keys().collect();
    assert_eq!(
        cols,
        ["id", "step", "timestamp", "mean", "q0.1", "q0.5", "q0.9"]
    );
    assert_eq!(rows[0]["id"], "a");
    assert_eq!(rows[0]["timestamp"], "2026-01-04T00:00:00Z");
    assert_eq!(rows[1]["timestamp"], "2026-01-05T00:00:00Z");
}

#[tokio::test]
async fn provider_errors_pass_through() {
    let h = harness(true, Some("tsfm-wrong-key")).await;
    let r = h.post(&docs_request_with_model("forecast")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.body.as_ref(),
        fixture("invalid_credential.401.json").as_slice()
    );
}

// MCP `forecast` tool.

const MCP_VERSION: &str = "2026-07-28";

fn mcp_body(method: &str, params: Value) -> Value {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": MCP_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

fn mcp_headers(body: &Value) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h.insert("accept", HeaderValue::from_static("application/json"));
    h.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MCP_VERSION),
    );
    h.insert(
        "mcp-method",
        HeaderValue::from_str(body["method"].as_str().unwrap()).unwrap(),
    );
    if let Some(name) = body["params"]["name"].as_str() {
        h.insert("mcp-name", HeaderValue::from_str(name).unwrap());
    }
    h
}

impl Harness {
    /// An MCP request on the Unix socket; returns the JSON-RPC `result`.
    async fn mcp(&self, method: &str, params: Value) -> Value {
        let body = mcp_body(method, params);
        let r = request(
            &self.paths.socket,
            Method::POST,
            "/mcp",
            mcp_headers(&body),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        json_of(&r)["result"].clone()
    }

    /// A `forecast` tool call over TCP with the bearer token.
    async fn forecast_tcp(&self, args: Value) -> Value {
        let body = mcp_body("tools/call", json!({"name": "forecast", "arguments": args}));
        let mut req = reqwest::Client::new()
            .post(format!("http://{}/mcp", self.server.tcp))
            .bearer_auth(&self.token)
            .body(serde_json::to_vec(&body).unwrap());
        for (k, v) in &mcp_headers(&body) {
            req = req.header(k.as_str(), v.to_str().unwrap());
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
        v["result"].clone()
    }
}

/// The `error` object of a tool error result.
fn tool_error(res: &Value) -> Value {
    assert_eq!(res["isError"], true, "{res}");
    let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    text["error"].clone()
}

#[tokio::test]
async fn mcp_lists_forecast_with_schemas() {
    let h = harness(false, None).await;
    let list = h.mcp("tools/list", json!({})).await;
    let tools = list["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["decide", "extract", "forecast", "models"]);
    let f = &tools[2];
    assert_eq!(f["inputSchema"]["type"], "object");
    assert!(f["inputSchema"]["properties"]["csv_path"].is_object());
    assert!(f["inputSchema"]["properties"]["inputs"].is_object());
    assert_eq!(f["outputSchema"]["required"], json!(["outputs"]));
}

#[tokio::test]
async fn mcp_forecast_without_a_backend_is_a_backend_unavailable_tool_error() {
    let h = harness(false, None).await;
    let req: Value = serde_json::from_slice(&docs_request_with_model("forecast")).unwrap();
    let res = h
        .mcp("tools/call", json!({"name": "forecast", "arguments": req}))
        .await;
    let e = tool_error(&res);
    assert_eq!(e["code"], "backend_unavailable", "{e}");
    assert_eq!(e["status"], 503);
    assert!(e["message"].as_str().unwrap().contains("forecast"), "{e}");
    assert!(h.mock.all().is_empty());
}

#[tokio::test]
async fn mcp_forecast_sends_a_tsfm_request_and_returns_the_response() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let req: Value = serde_json::from_slice(&docs_request_with_model("forecast")).unwrap();
    let res = h
        .mcp("tools/call", json!({"name": "forecast", "arguments": req}))
        .await;
    assert_eq!(res["isError"], false, "{res}");
    let docs: Value = serde_json::from_slice(&fixture("docs.response.json")).unwrap();
    assert_eq!(res["structuredContent"], docs);
    let sent: Value = serde_json::from_slice(&h.mock.last().body).unwrap();
    let want: Value = serde_json::from_slice(&fixture("docs.request.json")).unwrap();
    assert_eq!(sent, want);

    // Irregular series: an invalid_request tool error; nothing is sent.
    let n = h.mock.all().len();
    let bad = json!({"model": "forecast", "inputs": [{"target": [[1, 2], [3]]}]});
    let e = tool_error(
        &h.mcp("tools/call", json!({"name": "forecast", "arguments": bad}))
            .await,
    );
    assert_eq!(e["code"], "invalid_request", "{e}");
    assert_eq!(h.mock.all().len(), n);
}

#[tokio::test]
async fn mcp_forecast_reads_a_csv_path_over_tcp() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let csv = h.dir.path().join("sales.csv");
    std::fs::write(
        &csv,
        "sku,ds,y\na,2026-02-01,10\na,2026-02-02,11\nb,2026-02-01,5\na,2026-02-04,13\n",
    )
    .unwrap();
    let res = h
        .forecast_tcp(json!({
            "model": "forecast",
            "csv_path": csv,
            "id_column": "sku",
            "time_column": "ds",
            "value_column": "y",
            "parameters": {"prediction_length": 2, "quantile_levels": [0.5]},
        }))
        .await;
    assert_eq!(res["isError"], false, "{res}");
    assert_eq!(
        res["structuredContent"]["outputs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let sent: Value = serde_json::from_slice(&h.mock.last().body).unwrap();
    assert_eq!(sent["model"], "amazon/chronos-bolt-small");
    assert_eq!(
        sent["inputs"],
        json!([
            {"start": "2026-02-01T00:00:00Z", "target": [[10.0], [11.0], [null], [13.0]], "metadata": {"id": "a"}},
            {"start": "2026-02-01T00:00:00Z", "target": [[5.0]], "metadata": {"id": "b"}}
        ])
    );
    assert_eq!(
        sent["parameters"],
        json!({"prediction_length": 2, "quantile_levels": [0.5], "frequency": "D"})
    );
}

#[tokio::test]
async fn mcp_forecast_csv_path_rules() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let csv = h.dir.path().join("s.csv");
    std::fs::write(&csv, "ds,y\n2026-01-01,1\n2026-01-02,inf\n").unwrap();
    let cols = |path: Value| json!({"csv_path": path, "time_column": "ds", "value_column": "y"});
    let cases = [
        (cols(json!("relative.csv")), "invalid_request", "absolute"),
        (
            cols(json!(h.dir.path())),
            "invalid_request",
            "not a regular file",
        ),
        (
            cols(json!(h.dir.path().join("missing.csv"))),
            "invalid_request",
            "cannot read",
        ),
        (cols(json!(csv)), "invalid_request", "finite"),
        (
            json!({"csv_path": csv, "value_column": "y"}),
            "invalid_request",
            "time_column",
        ),
        (
            json!({"csv_path": csv, "time_column": "ds", "value_column": "y", "inputs": []}),
            "invalid_request",
            "not both",
        ),
        (
            json!({"model": "forecast", "inputs": [{"target": [[1]]}], "time_column": "ds"}),
            "invalid_request",
            "csv_path",
        ),
    ];
    for (args, code, needle) in cases {
        let e = tool_error(&h.forecast_tcp(args.clone()).await);
        assert_eq!(e["code"], code, "{args}: {e}");
        assert!(
            e["message"].as_str().unwrap().contains(needle),
            "{args}: {e}"
        );
    }
    assert!(h.mock.all().is_empty());
}

/// A small JSON Schema check: `type` (a name or a list of names),
/// `required`, `properties`, and `items`. Panics with the path on a
/// mismatch.
fn check_schema(schema: &Value, v: &Value, path: &str) {
    if let Some(t) = schema.get("type") {
        let names: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().map(|x| x.as_str().unwrap()).collect(),
            other => panic!("{path}: bad schema type {other}"),
        };
        let ok = names.iter().any(|t| match *t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "number" => v.is_number(),
            "integer" => v.is_i64() || v.is_u64(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            other => panic!("{path}: schema type `{other}` is not handled"),
        });
        assert!(ok, "{path}: {v} is not of type {t}");
    }
    if let Some(req) = schema.get("required").and_then(Value::as_array) {
        for k in req {
            let k = k.as_str().unwrap();
            assert!(v.get(k).is_some(), "{path}: `{k}` is required in {v}");
        }
    }
    if let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(Value::as_object),
        v.as_object(),
    ) {
        for (k, sub) in props {
            if let Some(x) = obj.get(k) {
                check_schema(sub, x, &format!("{path}.{k}"));
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            check_schema(items, x, &format!("{path}[{i}]"));
        }
    }
}

#[tokio::test]
async fn mcp_forecast_results_match_the_output_schema() {
    let h = harness(true, Some(TSFM_KEY)).await;
    let list = h.mcp("tools/list", json!({})).await;
    let schema = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "forecast")
        .unwrap()["outputSchema"]
        .clone();
    // The docs example: every field present.
    let req: Value = serde_json::from_slice(&docs_request_with_model("forecast")).unwrap();
    let res = h
        .mcp("tools/call", json!({"name": "forecast", "arguments": req}))
        .await;
    assert_eq!(res["isError"], false, "{res}");
    check_schema(&schema, &res["structuredContent"], "docs");
    // No `start` and no metadata: the mock (like TSFM) sends
    // `timestamps: null` and `metadata: null`.
    let bare = json!({"model": "forecast", "inputs": [{"target": [[1], [2], [3]]}],
                      "parameters": {"prediction_length": 2}});
    let res = h
        .mcp("tools/call", json!({"name": "forecast", "arguments": bare}))
        .await;
    assert_eq!(res["isError"], false, "{res}");
    let out = &res["structuredContent"]["outputs"][0];
    assert!(
        out["timestamps"].is_null() && out["metadata"].is_null(),
        "{out}"
    );
    check_schema(&schema, &res["structuredContent"], "bare");
    // TSFM also permits `quantile_predictions: null`.
    let mut v = res["structuredContent"].clone();
    v["outputs"][0]["quantile_predictions"] = Value::Null;
    check_schema(&schema, &v, "no quantiles");
}
