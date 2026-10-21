//! Live smoke tests against the real providers (TypeSafe, Workers AI Clef
//! and Clairic, OpenAI, Baseten). Manual and opt-in:
//!
//! ```sh
//! S1_LIVE=1 cargo test --test live -- --ignored
//! ```
//!
//! Keys come from `TYPESAFE_API_KEY`, `CLOUDFLARE_API_TOKEN` and
//! `CLOUDFLARE_ACCOUNT_ID`, `OPENAI_API_KEY`, `BASETEN_API_KEY`, or `~/.s1/auth.json` (read
//! only). Without `S1_LIVE=1` or without a key, the test skips.
//! `S1_RECORD=1` saves some answers as fixtures (never keys).

use axum::http::{HeaderMap, HeaderValue, Method};
use bytes::Bytes;
use s1::catalog::Catalog;
use s1::cli::client::request;
use s1::daemon::{AppState, ServerOptions, serve};
use s1::paths::Paths;
use s1::protocol::Protocol;
use s1::secrets::KeyResolver;
use serde_json::Value;

#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and a TypeSafe key"]
async fn live_raw_systemone_tiny_request() {
    if std::env::var("S1_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: S1_LIVE is not 1");
        return;
    }
    let real = KeyResolver::from_process(&Paths::from_env());
    let key = match real.resolve("typesafe") {
        Ok(Some(k)) => k.key,
        _ => {
            eprintln!("skip: no TypeSafe key");
            return;
        }
    };
    // Serve from temp paths; give the key through the env lookup only.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), move |k| {
        (k == "TYPESAFE_API_KEY").then(|| key.expose().to_string())
    });
    let token = keys.ensure_local_token().unwrap();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token,
    })
    .await
    .unwrap();
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    let body = br#"{"model":"jev-latest","state":"I was charged twice.","questions":{"billing":{"type":"noul","instructions":"Is this message about billing?"}}}"#;
    let reply = request(
        &paths.socket,
        Method::POST,
        "/v1/raw/systemone",
        h,
        Bytes::from_static(body),
    )
    .await
    .unwrap();
    server.shutdown().await;
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.headers["s1-backend"], "typesafe");
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["answers"]["billing"]["type"], "noul");
}

/// Live daemon on temp paths with the real keys passed through the env lookup.
/// `None` (with a skip message) when S1_LIVE is not 1 or a key is missing.
async fn live_server(
    providers: &[&str],
) -> Option<(tempfile::TempDir, Paths, s1::daemon::RunningServer)> {
    live_server_with(providers, false).await
}

/// As [`live_server`]; `cache` adds a response cache in the temp dir.
async fn live_server_with(
    providers: &[&str],
    cache: bool,
) -> Option<(tempfile::TempDir, Paths, s1::daemon::RunningServer)> {
    if std::env::var("S1_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: S1_LIVE is not 1");
        return None;
    }
    let real = KeyResolver::from_process(&Paths::from_env());
    let mut env: Vec<(&'static str, String)> = Vec::new();
    for p in providers {
        let Ok(Some(k)) = real.resolve(p) else {
            eprintln!("skip: no key for {p}");
            return None;
        };
        match *p {
            "typesafe" => env.push(("TYPESAFE_API_KEY", k.key.expose().to_string())),
            "openai" => env.push(("OPENAI_API_KEY", k.key.expose().to_string())),
            "baseten" => env.push(("BASETEN_API_KEY", k.key.expose().to_string())),
            "workers-ai" => {
                let Some(acc) = k.account_id.clone() else {
                    eprintln!("skip: no Cloudflare account id");
                    return None;
                };
                env.push(("CLOUDFLARE_API_TOKEN", k.key.expose().to_string()));
                env.push(("CLOUDFLARE_ACCOUNT_ID", acc));
            }
            _ => {}
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), move |name| {
        env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
    });
    let token = keys.ensure_local_token().unwrap();
    let mut state = AppState::new(Catalog::builtin(), keys)
        .with_versions(s1::cache::Versions::load(&paths.versions_file()));
    if cache {
        let c = s1::cache::Cache::open(&paths.cache_file(), 1 << 20).unwrap();
        state = state.with_cache(std::sync::Arc::new(c));
    }
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state,
        token,
    })
    .await
    .unwrap();
    Some((dir, paths, server))
}

async fn post_systemone(paths: &Paths, body: &[u8]) -> s1::cli::client::Reply {
    post_to(paths, "/v1/systemone", body).await
}

async fn post_to(paths: &Paths, path: &str, body: &[u8]) -> s1::cli::client::Reply {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    request(
        &paths.socket,
        Method::POST,
        path,
        h,
        Bytes::copy_from_slice(body),
    )
    .await
    .unwrap()
}

/// With `S1_RECORD=1`, save a live response as a fixture (never keys).
fn record(name: &str, body: &[u8]) {
    if std::env::var("S1_RECORD").as_deref() == Ok("1") {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(&path, body).unwrap();
        eprintln!("recorded {path}");
    }
}

const TRIAGE: &str = r#"{"model":"MODEL","state":"Checkout has been failing for every customer for the last hour.","questions":{"urgent":{"type":"noul","instructions":"Is this support request urgent?"},"team":{"type":"choice","instructions":"Which team should handle this request?","criteria":{"billing":"Payments, invoices, and refunds","technical":"Outages, errors, and configuration","sales":"Plans and upgrades"}},"severity":{"type":"score","instructions":"How severe is the customer impact?","criteria":["No impact","Minor","Major","Critical"]}}}"#;

#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and a TypeSafe key"]
async fn live_systemone_jev_by_id() {
    let Some((_dir, paths, server)) = live_server(&["typesafe"]).await else {
        return;
    };
    let reply = post_systemone(
        &paths,
        TRIAGE.replace("MODEL", "typesafe/jev-latest").as_bytes(),
    )
    .await;
    server.shutdown().await;
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.headers["s1-model"], "typesafe/jev-latest");
    record("typesafe/triage.response.json", &reply.body);
    let cols = s1::protocol::systemone::Systemone
        .flatten(&reply.body)
        .unwrap();
    let names: Vec<&str> = cols.iter().map(|(k, _)| k.as_str()).collect();
    assert!(names.contains(&"team__p__technical"), "{names:?}");
    assert!(names.contains(&"severity__p__3"), "{names:?}");
}

#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and Cloudflare keys"]
async fn live_systemone_clef_via_alias_and_too_long() {
    let Some((_dir, paths, server)) = live_server(&["workers-ai"]).await else {
        return;
    };
    // `decide` -> clef-flash (first entry; TypeSafe has no key here).
    let reply = post_systemone(&paths, TRIAGE.replace("MODEL", "decide").as_bytes()).await;
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.headers["s1-backend"], "workers-ai");
    assert_eq!(reply.headers["s1-model"], "workers-ai/clef-flash");
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(v.get("result").is_none(), "envelope removed");
    assert_eq!(v["model"], "clef-flash");
    assert_eq!(v["answers"]["severity"]["type"], "score");

    // Too long for clef-flash in real tokens: Clef refuses, s1 says too_long.
    // Numbered lines (as in the recorded probe: about 27k tokens, 87 KB).
    let filler = (0..2000)
        .map(|i| format!("Line {i}: the checkout page is slow today."))
        .collect::<Vec<_>>()
        .join(" ");
    let long = serde_json::json!({
        "model": "workers-ai/clef-flash",
        "state": filler,
        "questions": {"billing": {"type": "noul", "instructions": "Billing?"}}
    });
    let reply = post_systemone(&paths, &serde_json::to_vec(&long).unwrap()).await;
    server.shutdown().await;
    assert_eq!(
        reply.status.as_u16(),
        413,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["detail"]["error_type"], "too_long");
}

/// Stage 3 through the real provider: the second identical request is a
/// cache hit, the resolved version is saved, and `/status` shows the call.
/// Prints the provider's rate-limit headers (values are not secrets) to
/// help set real limits.
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and a TypeSafe key"]
async fn live_protection_cache_and_status() {
    let Some((_dir, paths, server)) = live_server_with(&["typesafe"], true).await else {
        return;
    };
    let body = br#"{"model":"typesafe/jev-latest","state":"I was charged twice.","questions":{"billing":{"type":"noul","instructions":"Is this message about billing?"}}}"#;
    let first = post_systemone(&paths, body).await;
    assert_eq!(
        first.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert_eq!(first.headers["s1-cache"], "miss");
    assert!(first.headers.contains_key("s1-capacity"));
    for (k, v) in &first.headers {
        let name = k.as_str();
        if name.contains("ratelimit") || name.contains("retry") {
            eprintln!("provider header {name}: {}", v.to_str().unwrap_or("?"));
        }
    }
    let second = post_systemone(&paths, body).await;
    assert_eq!(second.headers["s1-cache"], "hit");
    assert_eq!(second.body, first.body);

    let resolved: Value = serde_json::from_slice(&first.body).unwrap();
    let versions: Value =
        serde_json::from_slice(&std::fs::read(paths.versions_file()).unwrap()).unwrap();
    assert_eq!(versions["typesafe/jev-latest"], resolved["model"]);
    eprintln!("resolved version: {}", resolved["model"]);

    let status = request(
        &paths.socket,
        Method::GET,
        "/status",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();
    server.shutdown().await;
    let s: Value = serde_json::from_slice(&status.body).unwrap();
    let ts = s["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["provider"] == "typesafe")
        .cloned()
        .unwrap();
    assert_eq!(ts["rate"]["requests_last_minute"], 1, "{ts}");
    assert_eq!(ts["breaker"]["state"], "closed");
    assert_eq!(s["cache"]["entries"], 1);
}

fn openai_triage(model: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/openai/triage.request.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    v["model"] = Value::String(model.into());
    serde_json::to_vec(&v).unwrap()
}

fn ok(reply: &s1::cli::client::Reply) {
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
}

/// Stage 4: native `/v1/decisions`, systemone to OpenAI (with a refusal
/// as a per-question error), and OpenAI format to Jev.
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1, an OpenAI key and a TypeSafe key"]
async fn live_openai_decisions_both_ways() {
    let Some((_dir, paths, server)) = live_server(&["openai", "typesafe"]).await else {
        return;
    };
    // Native.
    let r = post_to(&paths, "/v1/decisions", &openai_triage("openai/gpt-6-luna")).await;
    ok(&r);
    assert_eq!(r.headers["s1-backend"], "openai");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["answers"][0]["name"], "billing");
    assert_eq!(v["answers"][1]["name"], Value::Null);
    assert_eq!(v["answers"][2]["type"], "score");

    // systemone client, OpenAI backend.
    let body = serde_json::json!({
        "model": "openai/gpt-6-luna",
        "state": "Checkout has been failing for every customer for the last hour.",
        "questions": {
            "team": {"type": "choice", "instructions": "Which team?", "criteria": {"billing": "Payments", "technical": "Outages"}},
            "urgent": {"type": "noul", "instructions": "Is this urgent?", "criteria": {"true": "Many customers are affected"}}
        }
    });
    let r = post_systemone(&paths, &serde_json::to_vec(&body).unwrap()).await;
    ok(&r);
    record("openai/systemone_via_openai.response.json", &r.body);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    let keys: Vec<&String> = v["answers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["team", "urgent"]);
    assert_eq!(v["answers"]["team"]["choice"], "technical");
    assert_eq!(v["answers"]["urgent"]["type"], "noul");

    // The recorded refusal case (`refusal.request.json`) as systemone: the
    // refusal is a per-question error. The model does not always refuse.
    let body = br#"{"model":"openai/gpt-6-luna","state":"rate 7","questions":{"blank":{"type":"noul","instructions":""}}}"#;
    let r = post_systemone(&paths, body).await;
    ok(&r);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    match v["answers"]["blank"]["type"].as_str() {
        Some("error") => {
            assert_eq!(v["answers"]["blank"]["error"]["code"], "refusal");
            record("openai/systemone_refusal.response.json", &r.body);
        }
        Some("noul") => eprintln!("note: the model answered instead of refusing this time"),
        other => panic!("unexpected answer type {other:?}: {v}"),
    }

    // OpenAI client, Jev backend.
    let r = post_to(
        &paths,
        "/v1/decisions",
        &openai_triage("typesafe/jev-latest"),
    )
    .await;
    server.shutdown().await;
    ok(&r);
    record("openai/decisions_via_jev.response.json", &r.body);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["answers"][0]["type"], "predicate");
    assert_eq!(v["answers"][0]["name"], "billing");
    assert_eq!(v["answers"][1]["choice"], "billing");
    let values: Vec<&Value> = v["answers"][1]["probabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["value"])
        .collect();
    assert!(values.contains(&&Value::Bool(true)), "{values:?}");
    assert_eq!(v["answers"][2]["probabilities"][1]["label"], "Medium");
}

/// Stage 5: `/v1/extract` through the real Clairic, by alias, and a long
/// state that Clairic cuts and flags instead of refusing.
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and Cloudflare keys"]
async fn live_extract_clairic_via_alias_and_truncation() {
    let Some((_dir, paths, server)) = live_server(&["workers-ai"]).await else {
        return;
    };
    let post = |body: Vec<u8>| {
        let paths = paths.clone();
        async move {
            let mut h = HeaderMap::new();
            h.insert("content-type", HeaderValue::from_static("application/json"));
            request(
                &paths.socket,
                Method::POST,
                "/v1/extract",
                h,
                Bytes::from(body),
            )
            .await
            .unwrap()
        }
    };
    let mut invoice: Value = serde_json::from_slice(
        &std::fs::read(format!(
            "{}/tests/fixtures/clairic/invoice.request.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    invoice["model"] = Value::from("extract");
    let reply = post(serde_json::to_vec(&invoice).unwrap()).await;
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.headers["s1-model"], "workers-ai/clairic");
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(v.get("result").is_none(), "envelope removed");
    assert_eq!(v["model"], "clairic");
    assert_eq!(v["answers"]["invoice_number"]["value"], "INV-2291");
    let cols: serde_json::Map<String, Value> = s1::protocol::clairic::Clairic
        .flatten(&reply.body)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(cols["invoice_number__span"], serde_json::json!([8, 16]));
    assert!(cols.contains_key("days_to_pay__p90"), "{cols:?}");

    // About 76k tokens by `bytes / 4`: over the context, under the pre-check.
    let filler = (0..7000)
        .map(|i| format!("Line {i}: the checkout page is slow today."))
        .collect::<Vec<_>>()
        .join(" ");
    let long = serde_json::json!({
        "model": "workers-ai/clairic",
        "state": format!("Invoice INV-77. {filler}"),
        "fields": {"invoice_number": {"type": "text"}}
    });
    let reply = post(serde_json::to_vec(&long).unwrap()).await;
    server.shutdown().await;
    assert_eq!(
        reply.status.as_u16(),
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["truncated"], true);
    assert_eq!(v["answers"]["invoice_number"]["value"], "INV-77");
}

/// Stage 6: an MCP `decide` list call over `POST /mcp` (Clef via the
/// `decide` alias) and an `extract` list call (Clairic), two tiny states each.
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and Cloudflare keys"]
async fn live_mcp_decide_and_extract_lists() {
    let Some((_dir, paths, server)) = live_server(&["workers-ai"]).await else {
        return;
    };
    let call = |tool: &str, args: Value| {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": tool, "arguments": args,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        });
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        h.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2026-07-28"),
        );
        h.insert("mcp-method", HeaderValue::from_static("tools/call"));
        h.insert("mcp-name", HeaderValue::from_str(tool).unwrap());
        let socket = paths.socket.clone();
        async move {
            request(
                &socket,
                Method::POST,
                "/mcp",
                h,
                Bytes::from(serde_json::to_vec(&body).unwrap()),
            )
            .await
            .unwrap()
        }
    };
    let decide = call(
        "decide",
        serde_json::json!({
            "template": {"model": "decide", "questions": {"urgent": {"type": "noul", "instructions": "Is this support request urgent?"}}},
            "states": [
                {"id": "a", "state": "Checkout is down for every customer."},
                {"id": "b", "state": "Could you add a dark mode some day?"},
            ],
        }),
    )
    .await;
    let extract = call(
        "extract",
        serde_json::json!({
            "template": {"model": "extract", "fields": {"invoice_number": {"type": "text"}, "total": {"type": "number"}}},
            "states": [
                {"id": 1, "state": "Invoice INV-2291, total due 1,080.00 USD."},
                {"id": 2, "state": "Invoice INV-77 for 12.50 USD."},
            ],
        }),
    )
    .await;
    server.shutdown().await;
    for (name, reply) in [("decide", &decide), ("extract", &extract)] {
        assert_eq!(
            reply.status.as_u16(),
            200,
            "{name}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let v: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(v["result"]["isError"], false, "{name}: {v}");
        let results = v["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 2, "{name}");
        for r in results {
            assert_eq!(r["ok"], true, "{name}: {r}");
        }
        eprintln!("{name}: {}", v["result"]["structuredContent"]);
    }
}

/// TSFM.ai forecast through a `tsfm` provider from the overlay. Needs a
/// `tsfm` key (`TSFM_API_KEY` or `~/.s1/auth.json`); there is none in v1,
/// so this normally skips. `S1_LIVE_TSFM_BASE_URL` and
/// `S1_LIVE_TSFM_UPSTREAM` point it at another TSFM-compatible server, for
/// example a local Timestral worker.
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and a TSFM.ai key"]
async fn live_forecast_tsfm_tiny_request() {
    if std::env::var("S1_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: S1_LIVE is not 1");
        return;
    }
    let real = KeyResolver::from_process(&Paths::from_env());
    let Ok(Some(k)) = real.resolve("tsfm") else {
        eprintln!("skip: no key for tsfm (TSFM_API_KEY or auth.json entry `tsfm`)");
        return;
    };
    let key = k.key.expose().to_string();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), move |name| {
        (name == "TSFM_API_KEY").then(|| key.clone())
    });
    let token = keys.ensure_local_token().unwrap();
    let base_url =
        std::env::var("S1_LIVE_TSFM_BASE_URL").unwrap_or_else(|_| "https://api.tsfm.ai".into());
    let upstream = std::env::var("S1_LIVE_TSFM_UPSTREAM")
        .unwrap_or_else(|_| "amazon/chronos-bolt-small".into());
    let catalog = Catalog::with_overlay_str(&format!(
        "[providers.tsfm]\nbase_url = {base_url:?}\napi = \"tsfm\"\n\
         [[providers.tsfm.models]]\nid = \"chronos-bolt-small\"\nupstream = {upstream:?}\n",
    ))
    .unwrap();
    let _server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(catalog, keys),
        token,
    })
    .await
    .unwrap();
    let body = br#"{"model":"tsfm/chronos-bolt-small","inputs":[{"start":"2026-02-01T00:00:00Z","target":[[428],[435],[441],[438],[446],[452],[460],[458],[466],[472]],"metadata":{"id":"store_017"}}],"parameters":{"prediction_length":3,"frequency":"D","quantile_levels":[0.1,0.5,0.9]}}"#;
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    let reply = request(
        &paths.socket,
        Method::POST,
        "/v1/forecast",
        h,
        Bytes::from_static(body),
    )
    .await
    .unwrap();
    assert!(
        reply.status.is_success(),
        "{}: {}",
        reply.status,
        String::from_utf8_lossy(&reply.body)
    );
    record("tsfm/live.response.json", &reply.body);
    let rows = s1::protocol::tsfm::flatten_rows(&reply.body).unwrap();
    assert_eq!(rows.len(), 3);
}

/// Both baselabs models on Baseten, each with a text state and a JSON state
/// (sent as text: `string_state`).
#[tokio::test]
#[ignore = "live: needs S1_LIVE=1 and a Baseten key"]
async fn live_systemone_baseten_mammoth_and_elephant() {
    let Some((_dir, paths, server)) = live_server(&["baseten"]).await else {
        return;
    };
    let mut replies = Vec::new();
    for model in ["baseten/mammoth-1", "baseten/elephant-1"] {
        let text: Value = serde_json::from_str(&TRIAGE.replace("MODEL", model)).unwrap();
        let mut json_state = text.clone();
        json_state["state"] =
            serde_json::json!({"ticket": text["state"], "customers_affected": "all"});
        for body in [text, json_state] {
            let reply = post_systemone(&paths, &serde_json::to_vec(&body).unwrap()).await;
            replies.push((model, reply));
        }
    }
    server.shutdown().await;
    for (model, reply) in replies {
        assert_eq!(
            reply.status.as_u16(),
            200,
            "{model}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(reply.headers["s1-model"], model);
        let v: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(v["answers"]["team"]["choice"], "technical", "{model}: {v}");
        assert!(
            v["answers"]["urgent"]["noul"].as_f64().unwrap() > 0.5,
            "{model}: {v}"
        );
    }
}
