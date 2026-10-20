//! `POST /mcp`: JSON-RPC requests (MCP 2026-07-28, stateless) against the
//! daemon router, with in-process mock providers behind it.

mod common;

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{CF_ACCOUNT, CF_TOKEN, Mocks, TEST_KEY};
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use serde_json::{Value, json};

const VERSION: &str = "2026-07-28";

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    server: RunningServer,
    mocks: Mocks,
    token: String,
}

async fn harness() -> Harness {
    harness_with("").await
}

async fn harness_with(extra_overlay: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        f.set_api_key("workers-ai", &Secret::new(CF_TOKEN), Some(CF_ACCOUNT));
        f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let token_text = token.expose().to_string();
    let mocks = Mocks::start().await;
    let catalog =
        Catalog::with_overlay_str(&format!("{}{extra_overlay}", mocks.overlay())).unwrap();
    let state = AppState::new(catalog, keys);
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state,
        token,
    })
    .await
    .unwrap();
    Harness {
        _dir: dir,
        paths,
        server,
        mocks,
        token: token_text,
    }
}

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientInfo": {"name": "s1-test", "version": "0"},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A request body with `_meta` filled in.
fn rpc_body(id: Value, method: &str, mut params: Value) -> Value {
    if params.get("_meta").is_none() {
        params["_meta"] = meta();
    }
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// The headers a 2026-07-28 client sends for `body`.
fn rpc_headers(body: &Value) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h.insert(
        "accept",
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    h.insert("mcp-protocol-version", HeaderValue::from_static(VERSION));
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
    async fn post_raw(&self, headers: HeaderMap, body: Vec<u8>) -> Reply {
        request(
            &self.paths.socket,
            Method::POST,
            "/mcp",
            headers,
            Bytes::from(body),
        )
        .await
        .unwrap()
    }

    async fn rpc(&self, method: &str, params: Value) -> Reply {
        let body = rpc_body(json!(1), method, params);
        self.post_raw(rpc_headers(&body), serde_json::to_vec(&body).unwrap())
            .await
    }

    async fn call(&self, tool: &str, args: Value) -> Reply {
        self.rpc("tools/call", json!({"name": tool, "arguments": args}))
            .await
    }
}

fn json_of(r: &Reply) -> Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
}

fn ok_result(r: &Reply) -> Value {
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["content-type"], "application/json");
    let v = json_of(r);
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["resultType"], "complete");
    assert_eq!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "s1"
    );
    v["result"].clone()
}

fn rpc_error(r: &Reply, status: StatusCode, code: i64) -> Value {
    assert_eq!(r.status, status, "{}", String::from_utf8_lossy(&r.body));
    let v = json_of(r);
    assert_eq!(v["error"]["code"], code, "{v}");
    v["error"].clone()
}

fn decide_template() -> Value {
    json!({
        "model": "decide",
        "questions": {
            "urgent": {"type": "noul", "instructions": "Is this urgent?"},
            "team": {"type": "choice", "instructions": "Which team?", "criteria": {"billing": "Money", "tech": "Bugs"}},
        }
    })
}

#[tokio::test]
async fn discover_names_the_version_and_tools() {
    let h = harness().await;
    let r = ok_result(&h.rpc("server/discover", json!({})).await);
    assert_eq!(r["supportedVersions"], json!([VERSION]));
    assert!(r["capabilities"]["tools"].is_object());
    assert!(r["ttlMs"].as_u64().unwrap() > 0);
    assert_eq!(r["cacheScope"], "public");
    h.server.shutdown().await;
}

#[tokio::test]
async fn tools_list_is_static_cacheable_and_has_output_schemas() {
    let h = harness().await;
    let a = ok_result(&h.rpc("tools/list", json!({})).await);
    let b = ok_result(&h.rpc("tools/list", json!({})).await);
    assert_eq!(a, b, "static per catalog version");
    assert!(a["ttlMs"].as_u64().unwrap() > 0);
    assert_eq!(a["cacheScope"], "public");
    assert!(a["_meta"]["s1/catalogVersion"].is_string());
    let tools = a["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["decide", "extract", "models"]);
    for t in tools {
        assert!(t["outputSchema"].is_object(), "{t}");
        assert_eq!(t["inputSchema"]["type"], "object");
    }
    let decide = &tools[0];
    assert_eq!(
        decide["inputSchema"]["properties"]["states"]["maxItems"],
        1000
    );
    let desc = decide["description"].as_str().unwrap();
    assert!(desc.contains("1000") || desc.contains("1,000"), "{desc}");
    assert!(desc.contains("from code"), "{desc}");
    // One page only.
    let bad = h.rpc("tools/list", json!({"cursor": "x"})).await;
    rpc_error(&bad, StatusCode::OK, -32602);
    h.server.shutdown().await;
}

#[tokio::test]
async fn decide_list_answers_each_item_and_one_failure_never_fails_the_call() {
    let h = harness().await;
    let r = h
        .call(
            "decide",
            json!({
                "template": decide_template(),
                "states": [
                    {"id": "t1", "state": "yes, the site is down"},
                    {"id": "t2"},
                    {"id": 3, "state": {"body": "a question about invoices"}},
                    "not an object",
                ]
            }),
        )
        .await;
    let res = ok_result(&r);
    assert_eq!(res["isError"], false);
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["id"], "t1");
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["answers"]["urgent"]["noul"], 0.9);
    assert_eq!(results[0]["answers"]["team"]["choice"], "billing");
    assert_eq!(results[0]["model"], "workers-ai/clef-flash");
    assert_eq!(results[1]["id"], "t2");
    assert_eq!(results[1]["ok"], false);
    assert_eq!(results[1]["error"]["code"], "invalid_request");
    assert_eq!(results[2]["id"], 3);
    assert_eq!(results[2]["ok"], true);
    assert_eq!(results[2]["answers"]["urgent"]["noul"], 0.1);
    assert_eq!(results[3]["id"], 3, "index as id");
    assert_eq!(results[3]["ok"], false);
    // The text block holds the same JSON for older clients.
    let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, res["structuredContent"]);
    // Two upstream requests, each with the template plus its state.
    let seen = h.mocks.clef.all();
    assert_eq!(seen.len(), 2);
    let bodies: Vec<Value> = seen
        .iter()
        .map(|s| serde_json::from_slice(&s.body).unwrap())
        .collect();
    assert!(bodies.iter().all(|b| b["model"] == "clef-flash"));
    assert!(
        bodies
            .iter()
            .any(|b| b["state"] == json!({"body": "a question about invoices"}))
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn upstream_errors_stay_per_item() {
    let h = harness().await;
    // jev-latest via id; the typesafe mock rejects images without the opt-in.
    let template = json!({"model": "typesafe/jev-latest", "questions": {"q": {"type": "box", "instructions": "Where?"}}});
    let r = h
        .call(
            "decide",
            json!({"template": template, "states": [{"id": "a", "state": "x"}]}),
        )
        .await;
    let res = ok_result(&r);
    let item = &res["structuredContent"]["results"][0];
    assert_eq!(item["ok"], false, "{item}");
    assert!(item["error"]["code"].is_string());
    assert!(item["error"]["status"].as_u64().unwrap() >= 400);
    h.server.shutdown().await;
}

#[tokio::test]
async fn extract_list_sends_text_state_and_images() {
    let h = harness().await;
    let template = json!({"model": "extract", "fields": {"invoice_number": {"type": "text"}, "total": {"type": "number"}}});
    let r = h
        .call(
            "extract",
            json!({
                "template": template,
                "states": [
                    {"id": "a", "state": "INV-7 total 42"},
                    {"id": "b", "state": {"invoice": "INV-8"}, "images": ["data:image/png;base64,iVBORw0KGgo="]},
                ]
            }),
        )
        .await;
    let res = ok_result(&r);
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results[0]["ok"], true, "{}", results[0]);
    assert_eq!(results[0]["answers"]["invoice_number"]["value"], "INV-7");
    assert_eq!(results[0]["truncated"], false);
    assert_eq!(results[0]["model"], "workers-ai/clairic");
    assert_eq!(results[1]["ok"], true, "{}", results[1]);
    let seen = h.mocks.clef.all();
    let b: Vec<Value> = seen
        .iter()
        .map(|s| serde_json::from_slice(&s.body).unwrap())
        .collect();
    let with_images = b.iter().find(|b| b.get("images").is_some()).unwrap();
    assert_eq!(with_images["state"], r#"{"invoice":"INV-8"}"#);
    assert_eq!(with_images["images"].as_array().unwrap().len(), 1);
    h.server.shutdown().await;
}

#[tokio::test]
async fn models_tool_returns_the_catalog() {
    let h = harness().await;
    let res = ok_result(&h.call("models", json!({})).await);
    let s = &res["structuredContent"];
    assert!(
        s["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "workers-ai/clef")
    );
    assert!(s["aliases"]["decide"].is_object());
    h.server.shutdown().await;
}

#[tokio::test]
async fn item_limits_are_tool_errors() {
    let h = harness().await;
    for n in [0usize, 1001] {
        let states: Vec<Value> = (0..n).map(|i| json!({"id": i, "state": "x"})).collect();
        let res = ok_result(
            &h.call(
                "decide",
                json!({"template": decide_template(), "states": states}),
            )
            .await,
        );
        assert_eq!(res["isError"], true, "{n}");
        assert!(
            res["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid_request")
        );
    }
    assert!(h.mocks.clef.all().is_empty());
    let unknown = h.call("nope", json!({})).await;
    let e = rpc_error(&unknown, StatusCode::OK, -32602);
    assert!(e["message"].as_str().unwrap().contains("nope"));
    h.server.shutdown().await;
}

#[tokio::test]
async fn calls_over_the_client_timeout_are_refused_with_chunk_and_retry() {
    // One request per second: 100 items take 100 s.
    let h = harness_with("[limits.workers-ai]\nrpm = 60\nmax_in_flight = 1\n").await;
    let states: Vec<Value> = (0..100).map(|i| json!({"id": i, "state": "x"})).collect();
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": states, "timeout_secs": 10}),
        )
        .await,
    );
    assert_eq!(res["isError"], true, "{res}");
    let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    let e = &text["error"];
    assert_eq!(e["code"], "queue_full");
    assert_eq!(e["items"], 100);
    assert_eq!(e["timeout_secs"], 10);
    assert!(e["estimated_secs"].as_u64().unwrap() >= 100);
    // 80 % of 10 s at 1 per second.
    assert_eq!(e["suggested_chunk_size"], 8);
    assert_eq!(e["retry_after_secs"], 0);
    assert!(h.mocks.clef.all().is_empty(), "nothing was sent");
    // A chunk of the suggested size runs, and within the timeout.
    let chunk = e["suggested_chunk_size"].as_u64().unwrap();
    let states: Vec<Value> = (0..chunk).map(|i| json!({"id": i, "state": "x"})).collect();
    let started = std::time::Instant::now();
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": states, "timeout_secs": 10}),
        )
        .await,
    );
    assert_eq!(res["isError"], false, "{res}");
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len() as u64, chunk);
    assert!(results.iter().all(|r| r["ok"] == true), "{res}");
    assert!(started.elapsed() < Duration::from_secs(10));
    h.server.shutdown().await;
}

/// SSE events of a reply: the JSON of each `data:` line.
fn sse_events(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .split("\n\n")
        .filter_map(|ev| {
            ev.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .map(|d| serde_json::from_str(d).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn progress_streams_on_the_response_when_asked() {
    let h = harness().await;
    let mut m = meta();
    m["progressToken"] = json!("tok-1");
    let states: Vec<Value> = [0, 250, 500, 750]
        .iter()
        .enumerate()
        .map(|(i, ms)| json!({"id": i, "state": format!("sleep:{ms}")}))
        .collect();
    let body = rpc_body(
        json!(7),
        "tools/call",
        json!({"name": "decide", "arguments": {"template": decide_template(), "states": states}, "_meta": m}),
    );
    let r = h
        .post_raw(rpc_headers(&body), serde_json::to_vec(&body).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/event-stream");
    let events = sse_events(&r.body);
    let (last, progress) = events.split_last().unwrap();
    assert!(progress.len() >= 2, "{events:?}");
    let mut prev = 0;
    for p in progress {
        assert_eq!(p["method"], "notifications/progress");
        assert_eq!(p["params"]["progressToken"], "tok-1");
        assert_eq!(p["params"]["total"], 4);
        let n = p["params"]["progress"].as_u64().unwrap();
        assert!(n > prev, "progress must increase: {events:?}");
        prev = n;
    }
    assert_eq!(last["id"], 7);
    let results = last["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap();
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|r| r["ok"] == true));

    // Without a progress token the same call is plain JSON.
    let plain = h
        .call(
            "decide",
            json!({"template": decide_template(), "states": [{"id": 0, "state": "sleep:10"}]}),
        )
        .await;
    ok_result(&plain);
    h.server.shutdown().await;
}

#[tokio::test]
async fn closing_the_stream_cancels_the_call() {
    let h = harness().await;
    let mut m = meta();
    m["progressToken"] = json!(5);
    let mut states = vec![json!({"id": 0, "state": "sleep:0"})];
    states.extend((1..40).map(|i| json!({"id": i, "state": "sleep:400"})));
    let body = rpc_body(
        json!(1),
        "tools/call",
        json!({"name": "decide", "arguments": {"template": decide_template(), "states": states}, "_meta": m}),
    );
    let url = format!("http://{}/mcp", h.server.tcp);
    let mut req = reqwest::Client::new()
        .post(&url)
        .bearer_auth(&h.token)
        .body(serde_json::to_vec(&body).unwrap());
    for (k, v) in &rpc_headers(&body) {
        req = req.header(k.as_str(), v.to_str().unwrap());
    }
    let mut resp = req.send().await.unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let first = resp.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("notifications/progress"));
    drop(resp);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let sent = h.mocks.clef.all().len();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        h.mocks.clef.all().len(),
        sent,
        "no new requests after cancel"
    );
    assert!(sent < 40, "{sent}");
    h.server.shutdown().await;
}

#[tokio::test]
async fn tcp_needs_the_bearer_token() {
    let h = harness().await;
    let body = rpc_body(json!(1), "tools/list", json!({}));
    let url = format!("http://{}/mcp", h.server.tcp);
    let send = |token: Option<&str>| {
        let mut req = reqwest::Client::new()
            .post(&url)
            .body(serde_json::to_vec(&body).unwrap());
        for (k, v) in &rpc_headers(&body) {
            req = req.header(k.as_str(), v.to_str().unwrap());
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send()
    };
    let none = send(None).await.unwrap();
    assert_eq!(none.status(), 401);
    assert!(
        none.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Bearer")
    );
    assert_eq!(send(Some("wrong")).await.unwrap().status(), 401);
    let ok = send(Some(&h.token)).await.unwrap();
    assert_eq!(ok.status(), 200);
    let v: Value = serde_json::from_slice(&ok.bytes().await.unwrap()).unwrap();
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 3);
    h.server.shutdown().await;
}

#[tokio::test]
async fn protocol_rules_give_the_spec_errors() {
    let h = harness().await;
    let body = rpc_body(json!(1), "tools/list", json!({}));
    let bytes = serde_json::to_vec(&body).unwrap();

    // Missing Mcp-Method.
    let mut hd = rpc_headers(&body);
    hd.remove("mcp-method");
    rpc_error(
        &h.post_raw(hd, bytes.clone()).await,
        StatusCode::BAD_REQUEST,
        -32020,
    );
    // Header and body disagree.
    let mut hd = rpc_headers(&body);
    hd.insert("mcp-method", HeaderValue::from_static("tools/call"));
    rpc_error(
        &h.post_raw(hd, bytes.clone()).await,
        StatusCode::BAD_REQUEST,
        -32020,
    );
    // Missing MCP-Protocol-Version.
    let mut hd = rpc_headers(&body);
    hd.remove("mcp-protocol-version");
    rpc_error(
        &h.post_raw(hd, bytes.clone()).await,
        StatusCode::BAD_REQUEST,
        -32020,
    );

    // Mcp-Name must match; the base64 form is decoded first.
    let call = rpc_body(
        json!(1),
        "tools/call",
        json!({"name": "models", "arguments": {}}),
    );
    let call_bytes = serde_json::to_vec(&call).unwrap();
    let mut hd = rpc_headers(&call);
    hd.insert("mcp-name", HeaderValue::from_static("decide"));
    rpc_error(
        &h.post_raw(hd, call_bytes.clone()).await,
        StatusCode::BAD_REQUEST,
        -32020,
    );
    let mut hd = rpc_headers(&call);
    hd.remove("mcp-name");
    rpc_error(
        &h.post_raw(hd, call_bytes.clone()).await,
        StatusCode::BAD_REQUEST,
        -32020,
    );
    let mut hd = rpc_headers(&call);
    hd.insert("mcp-name", HeaderValue::from_static("=?base64?bW9kZWxz?="));
    ok_result(&h.post_raw(hd, call_bytes).await);

    // Missing _meta fields.
    let no_meta = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    rpc_error(
        &h.post_raw(rpc_headers(&no_meta), serde_json::to_vec(&no_meta).unwrap())
            .await,
        StatusCode::BAD_REQUEST,
        -32602,
    );
    let mut m = meta();
    m.as_object_mut()
        .unwrap()
        .remove("io.modelcontextprotocol/clientCapabilities");
    let no_caps = rpc_body(json!(1), "tools/list", json!({"_meta": m}));
    rpc_error(
        &h.post_raw(rpc_headers(&no_caps), serde_json::to_vec(&no_caps).unwrap())
            .await,
        StatusCode::BAD_REQUEST,
        -32602,
    );

    // Unsupported version (header and body agree on it).
    let mut m = meta();
    m["io.modelcontextprotocol/protocolVersion"] = json!("1900-01-01");
    let old = rpc_body(json!(1), "tools/list", json!({"_meta": m}));
    let mut hd = rpc_headers(&old);
    hd.insert(
        "mcp-protocol-version",
        HeaderValue::from_static("1900-01-01"),
    );
    let e = rpc_error(
        &h.post_raw(hd, serde_json::to_vec(&old).unwrap()).await,
        StatusCode::BAD_REQUEST,
        -32022,
    );
    assert_eq!(e["data"]["supported"], json!([VERSION]));
    assert_eq!(e["data"]["requested"], "1900-01-01");

    // A legacy initialize learns the supported versions.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}});
    let mut hd = HeaderMap::new();
    hd.insert("content-type", HeaderValue::from_static("application/json"));
    let e = rpc_error(
        &h.post_raw(hd, serde_json::to_vec(&init).unwrap()).await,
        StatusCode::BAD_REQUEST,
        -32022,
    );
    assert_eq!(e["data"]["supported"], json!([VERSION]));

    // Unknown method.
    let other = rpc_body(json!(1), "prompts/list", json!({}));
    rpc_error(
        &h.post_raw(rpc_headers(&other), serde_json::to_vec(&other).unwrap())
            .await,
        StatusCode::NOT_FOUND,
        -32601,
    );
    // Parse error and batch.
    rpc_error(
        &h.post_raw(rpc_headers(&body), b"{".to_vec()).await,
        StatusCode::BAD_REQUEST,
        -32700,
    );
    rpc_error(
        &h.post_raw(rpc_headers(&body), b"[]".to_vec()).await,
        StatusCode::BAD_REQUEST,
        -32600,
    );
    // Notification: accepted, no body.
    let note = json!({"jsonrpc": "2.0", "method": "notifications/whatever"});
    let r = h
        .post_raw(rpc_headers(&note), serde_json::to_vec(&note).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert!(r.body.is_empty());
    // Foreign Origin.
    let mut hd = rpc_headers(&body);
    hd.insert("origin", HeaderValue::from_static("http://evil.example"));
    assert_eq!(h.post_raw(hd, bytes).await.status, StatusCode::FORBIDDEN);
    // No GET stream, no session delete.
    for m in [Method::GET, Method::DELETE] {
        let r = request(&h.paths.socket, m, "/mcp", HeaderMap::new(), Bytes::new())
            .await
            .unwrap();
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    }
    h.server.shutdown().await;
}

/// Check `v` against the subset of JSON Schema the tool output schemas use
/// (`type`, `const`, `properties`, `required`, `items`, `if`/`then`/`else`).
fn check_schema(schema: &Value, v: &Value, path: &str) {
    if let Some(c) = schema.get("const") {
        assert_eq!(v, c, "{path}: {v} is not {c}");
    }
    if let Some(cond) = schema.get("if") {
        let branch = if schema_ok(cond, v) { "then" } else { "else" };
        if let Some(sub) = schema.get(branch) {
            check_schema(sub, v, &format!("{path}({branch})"));
        }
    }
    if let Some(t) = schema.get("type").and_then(Value::as_str) {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            other => panic!("{path}: schema type `{other}` is not handled"),
        };
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

/// True if `v` matches `schema` under [`check_schema`].
fn schema_ok(schema: &Value, v: &Value) -> bool {
    let (schema, v) = (schema.clone(), v.clone());
    std::panic::catch_unwind(move || check_schema(&schema, &v, "")).is_ok()
}

#[tokio::test]
async fn output_schemas_tie_answers_to_ok_and_error_to_failure() {
    let h = harness().await;
    let list = ok_result(&h.rpc("tools/list", json!({})).await);
    for name in ["decide", "extract"] {
        let schema = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["outputSchema"]
            .clone();
        let ok = json!({"results": [{"id": 0, "ok": true, "answers": {}}]});
        let failed =
            json!({"results": [{"id": 0, "ok": false, "error": {"code": "x", "message": "y"}}]});
        assert!(schema_ok(&schema, &ok), "{name}");
        assert!(schema_ok(&schema, &failed), "{name}");
        let no_answers = json!({"results": [{"id": 0, "ok": true}]});
        let no_error = json!({"results": [{"id": 0, "ok": false}]});
        assert!(
            !schema_ok(&schema, &no_answers),
            "{name}: ok without answers"
        );
        assert!(
            !schema_ok(&schema, &no_error),
            "{name}: failure without error"
        );
    }
    h.server.shutdown().await;
}

#[tokio::test]
async fn structured_content_matches_the_output_schema() {
    let h = harness().await;
    let list = ok_result(&h.rpc("tools/list", json!({})).await);
    let schema = |name: &str| {
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["outputSchema"]
            .clone()
    };
    let decide = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": [
                {"id": "a", "state": "the site is down"},
                {"id": "b"},
            ]}),
        )
        .await,
    );
    let results = &decide["structuredContent"]["results"];
    assert_eq!(results[0]["ok"], true, "{results}");
    assert_eq!(results[1]["ok"], false, "{results}");
    check_schema(&schema("decide"), &decide["structuredContent"], "decide");
    let template = json!({"model": "extract", "fields": {"invoice_number": {"type": "text"}}});
    let extract = ok_result(
        &h.call(
            "extract",
            json!({"template": template, "states": [{"id": "a", "state": "INV-7"}]}),
        )
        .await,
    );
    assert_eq!(extract["structuredContent"]["results"][0]["ok"], true);
    check_schema(&schema("extract"), &extract["structuredContent"], "extract");
    let models = ok_result(&h.call("models", json!({})).await);
    check_schema(&schema("models"), &models["structuredContent"], "models");
    h.server.shutdown().await;
}

#[tokio::test]
async fn tool_arguments_must_be_an_object() {
    let h = harness().await;
    for bad in [json!(null), json!([]), json!("x"), json!(3)] {
        let r = h
            .rpc("tools/call", json!({"name": "models", "arguments": bad}))
            .await;
        let e = rpc_error(&r, StatusCode::OK, -32602);
        assert!(
            e["message"].as_str().unwrap().contains("arguments"),
            "{bad}: {e}"
        );
    }
    // Omitted arguments are an empty object.
    let res = ok_result(&h.rpc("tools/call", json!({"name": "models"})).await);
    assert_eq!(res["isError"], false, "{res}");
    h.server.shutdown().await;
}

#[tokio::test]
async fn image_paths_are_refused_per_item_over_tcp() {
    let h = harness().await;
    let png = h._dir.path().join("a.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
    let template = json!({
        "model": "typesafe/jev-latest",
        "questions": {"q": {"type": "noul", "instructions": "?"}},
        "assets": {"img-1": {"type": "image", "s1_path": png}},
    });
    let body = rpc_body(
        json!(1),
        "tools/call",
        json!({"name": "decide", "arguments": {"template": template, "states": [{"id": "a", "state": "x"}]}}),
    );
    let mut req = reqwest::Client::new()
        .post(format!("http://{}/mcp", h.server.tcp))
        .bearer_auth(&h.token)
        .body(serde_json::to_vec(&body).unwrap());
    for (k, v) in &rpc_headers(&body) {
        req = req.header(k.as_str(), v.to_str().unwrap());
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    let item = &v["result"]["structuredContent"]["results"][0];
    assert_eq!(item["ok"], false, "{v}");
    assert!(
        item["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unix socket"),
        "{item}"
    );
    assert!(h.mocks.jev.all().is_empty(), "nothing was sent");
    h.server.shutdown().await;
}

/// The `error` object of a refused list call.
fn refusal(res: &Value) -> Value {
    assert_eq!(res["isError"], true, "{res}");
    let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["error"]["code"], "queue_full", "{text}");
    text["error"].clone()
}

#[tokio::test]
async fn an_invalid_first_item_does_not_skip_the_estimate() {
    let h = harness_with("[limits.workers-ai]\nrpm = 60\nmax_in_flight = 1\n").await;
    // The first item builds but Clairic cannot read it (an empty state and
    // no images); the second has no state at all.
    let template = json!({"model": "extract", "fields": {"invoice_number": {"type": "text"}}});
    let mut states = vec![
        json!({"id": "empty", "state": ""}),
        json!({"id": "no-state"}),
    ];
    states.extend((0..98).map(|i| json!({"id": i, "state": "x"})));
    let call = h.call(
        "extract",
        json!({"template": template, "states": states, "timeout_secs": 10}),
    );
    let res = ok_result(
        &tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("the call is refused up front"),
    );
    let e = refusal(&res);
    assert_eq!(e["items"], 98, "{e}");
    assert_eq!(e["suggested_chunk_size"], 8);
    assert!(h.mocks.clef.all().is_empty(), "nothing was sent");
    h.server.shutdown().await;
}

#[tokio::test]
async fn the_estimate_counts_every_item_size() {
    // 1,000 tokens per second. The first state is short; the others are
    // about 10,000 tokens each, so each one takes about 10 s.
    let h = harness_with("[limits.workers-ai]\ntpm = 60000\n").await;
    let big = "y".repeat(30_000);
    let mut states = vec![json!({"id": "small", "state": "x"})];
    states.extend((0..4).map(|i| json!({"id": i, "state": big})));
    let call = h.call(
        "decide",
        json!({"template": decide_template(), "states": states, "timeout_secs": 10}),
    );
    let res = ok_result(
        &tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("the call is refused up front"),
    );
    let e = refusal(&res);
    assert_eq!(e["items"], 5);
    assert!(e["estimated_secs"].as_u64().unwrap() >= 30, "{e}");
    assert!(h.mocks.clef.all().is_empty(), "nothing was sent");
    h.server.shutdown().await;
}

#[tokio::test]
async fn a_call_no_chunk_can_fix_names_the_timeout_it_needs() {
    // 6 per minute: one item takes 10 s, over 80 % of a 2 s timeout.
    let h = harness_with("[limits.workers-ai]\nrpm = 6\nmax_in_flight = 1\n").await;
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": [{"id": 0, "state": "x"}], "timeout_secs": 2}),
        )
        .await,
    );
    let e = refusal(&res);
    assert_eq!(e["suggested_chunk_size"], 1);
    assert_eq!(e["retry_after_secs"], 0);
    assert_eq!(e["required_timeout_secs"], 13, "{e}");
    assert!(
        e["message"].as_str().unwrap().contains("timeout_secs"),
        "{e}"
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn a_large_template_runs_with_bounded_item_bodies() {
    // A 900 KiB template (Jev takes up to 262,144 tokens, about 1 MiB) and
    // 64 items: every item runs and is sent once with the whole template.
    // (Clef's context check would refuse bodies this large.)
    // tests/mcp_memory.rs checks the memory bound itself.
    let h = harness_with("[limits.typesafe]\ntpm = 1000000000\n").await;
    let mut template = decide_template();
    template["model"] = json!("typesafe/jev-latest");
    template["questions"]["urgent"]["instructions"] = json!("u".repeat(900 << 10));
    let states: Vec<Value> = (0..64).map(|i| json!({"id": i, "state": "x"})).collect();
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": template, "states": states, "timeout_secs": 600}),
        )
        .await,
    );
    assert_eq!(res["isError"], false, "{}", res["content"]);
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 64);
    let failed: Vec<&Value> = results.iter().filter(|r| r["ok"] != true).collect();
    assert!(failed.is_empty(), "{:?}", failed.first());
    let seen = h.mocks.jev.all();
    assert_eq!(seen.len(), 64, "every item was sent once");
    assert!(seen.iter().all(|s| s.body.len() > 900 << 10));
    h.server.shutdown().await;
}

#[tokio::test]
async fn items_that_fail_protocol_checks_do_not_count_in_the_estimate() {
    // One request per second and a 2 s timeout (1.6 s budget): one item
    // fits. Nine items carry more images than Clairic takes; they fail on
    // their own and must not push the valid item over the budget.
    let h = harness_with("[limits.workers-ai]\nrpm = 60\nmax_in_flight = 1\n").await;
    let template = json!({"model": "extract", "fields": {"invoice_number": {"type": "text"}}});
    let img = "data:image/png;base64,iVBORw0KGgo=";
    let mut states = vec![json!({"id": "ok", "state": "INV-7"})];
    states.extend(
        (0..9).map(|i| json!({"id": i, "state": "x", "images": [img, img, img, img, img]})),
    );
    let call = h.call(
        "extract",
        json!({"template": template, "states": states, "timeout_secs": 2}),
    );
    let res = ok_result(
        &tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("the call finishes"),
    );
    assert_eq!(res["isError"], false, "{}", res["content"]);
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 10);
    assert_eq!(results[0]["ok"], true, "{}", results[0]);
    for r in &results[1..] {
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["error"]["code"], "unsupported", "{r}");
    }
    assert_eq!(h.mocks.clef.all().len(), 1, "only the valid item was sent");
    h.server.shutdown().await;
}

#[tokio::test]
async fn a_suggested_chunk_of_uneven_items_fits_when_resubmitted() {
    // 1,000 tokens per second, 8 s budget. Ten small states, one of about
    // 10,000 tokens, then 89 small ones. The average cost would suggest
    // about 61 items, a prefix that holds the big state and takes over
    // 10 s; the chunk must come from the prefix's own costs.
    let h = harness_with("[limits.workers-ai]\ntpm = 60000\n").await;
    let mut states: Vec<Value> = (0..10).map(|i| json!({"id": i, "state": "x"})).collect();
    states.push(json!({"id": "big", "state": "y".repeat(30_000)}));
    states.extend((11..100).map(|i| json!({"id": i, "state": "x"})));
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": states.clone(), "timeout_secs": 10}),
        )
        .await,
    );
    let e = refusal(&res);
    assert_eq!(e["retry_after_secs"], 0, "{e}");
    let chunk = e["suggested_chunk_size"].as_u64().unwrap() as usize;
    assert!((1..=10).contains(&chunk), "{e}");
    let res = ok_result(
        &h.call(
            "decide",
            json!({"template": decide_template(), "states": &states[..chunk], "timeout_secs": 10}),
        )
        .await,
    );
    assert_eq!(res["isError"], false, "{}", res["content"]);
    h.server.shutdown().await;
}
