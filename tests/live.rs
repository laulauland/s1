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
