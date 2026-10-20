//! Live smoke test against the real TypeSafe API. Manual and opt-in:
//!
//! ```sh
//! S1_LIVE=1 cargo test --test live -- --ignored
//! ```
//!
//! The key comes from `TYPESAFE_API_KEY` or `~/.s1/auth.json` (read only).
//! Without `S1_LIVE=1` or without a key, the test skips.

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
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    request(
        &paths.socket,
        Method::POST,
        "/v1/systemone",
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
