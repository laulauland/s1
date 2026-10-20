//! In-process daemon tests: Unix socket + TCP listeners, `/v1/models`, and
//! `/v1/raw/systemone` against a mock TypeSafe server.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{MISSING_KEY, Recorder, TEST_KEY, fixture, overlay_for, start_mock};
use proptest::prelude::*;
use s1::catalog::Catalog;
use s1::cli::client::request;
use s1::daemon::{AppState, DaemonError, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use serde_json::Value;

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    server: RunningServer,
    token: Secret,
    mock: Recorder,
}

async fn harness_with(key: Option<&str>, overlay: Option<String>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    if let Some(k) = key {
        keys.update(|f| f.set_api_key("typesafe", &Secret::new(k), None))
            .unwrap();
    }
    let token = keys.ensure_local_token().unwrap();
    let (addr, mock) = start_mock().await;
    let catalog = Catalog::with_overlay_str(&overlay.unwrap_or_else(|| overlay_for(addr))).unwrap();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(catalog, keys),
        token: token.clone(),
    })
    .await
    .unwrap();
    Harness {
        _dir: dir,
        paths,
        server,
        token,
        mock,
    }
}

async fn harness() -> Harness {
    harness_with(Some(TEST_KEY), None).await
}

fn json_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h
}

async fn unix_post(
    h: &Harness,
    path: &str,
    headers: HeaderMap,
    body: &[u8],
) -> s1::cli::client::Reply {
    request(
        &h.paths.socket,
        Method::POST,
        path,
        headers,
        Bytes::copy_from_slice(body),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn raw_systemone_forwards_original_bytes_and_returns_response_unchanged() {
    let h = harness().await;
    let body = fixture("typesafe/noul.request.json");
    let reply = unix_post(&h, "/v1/raw/systemone", json_headers(), &body).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_ref(), fixture("typesafe/noul.response.json"));
    assert_eq!(reply.headers["s1-backend"], "typesafe");
    assert_eq!(reply.headers["s1-model"], "typesafe/jev-latest");
    assert!(
        reply.headers["s1-request-id"]
            .to_str()
            .unwrap()
            .starts_with("req_")
    );
    assert_eq!(
        reply.headers["x-typesafe-request-id"],
        "req_0123456789abcdef0123456789abcdef"
    );
    assert_eq!(reply.headers["content-type"], "application/json");

    let seen = h.mock.last();
    assert_eq!(
        seen.body.as_ref(),
        body.as_slice(),
        "bytes reach TypeSafe unchanged"
    );
    assert_eq!(seen.headers["authorization"], format!("Bearer {TEST_KEY}"));
    assert_eq!(seen.headers["content-type"], "application/json");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_passes_provider_errors_through_unchanged() {
    let h = harness_with(Some("wrong-key"), None).await;
    let reply = unix_post(
        &h,
        "/v1/raw/systemone",
        json_headers(),
        &fixture("typesafe/noul.request.json"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reply.body.as_ref(),
        fixture("typesafe/invalid_key.401.json")
    );
    assert_eq!(reply.headers["s1-backend"], "typesafe");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_returns_typesafe_403_unchanged() {
    // `/v1/raw` is pure passthrough: no 403 -> 401 mapping (that is only on
    // the typed `/v1/systemone` ingress).
    let h = harness_with(Some(MISSING_KEY), None).await;
    let reply = unix_post(
        &h,
        "/v1/raw/systemone",
        json_headers(),
        &fixture("typesafe/noul.request.json"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(
        reply.body.as_ref(),
        fixture("typesafe/missing_key.403.json")
    );
    assert!(reply.headers.get("s1-upstream-status").is_none());
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_does_not_read_image_paths() {
    // Paths are a typed-ingress feature; raw sends the bytes as they are.
    let h = harness().await;
    let body = br#"{"model":"jev-latest","state":"a","questions":{"q":{"type":"noul"}},"assets":{"a":{"type":"image","s1_path":"/nonexistent/x.png"}}}"#;
    let reply = unix_post(&h, "/v1/raw/systemone", json_headers(), body).await;
    assert_eq!(h.mock.last().body.as_ref(), body.as_slice());
    assert_eq!(reply.headers["s1-backend"], "typesafe");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_forwards_typesafe_headers_but_not_client_authorization() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/v1/raw/systemone", h.server.tcp))
        .bearer_auth(h.token.expose())
        .header("content-type", "application/json")
        .header("TypeSafe-Danger-Zone-Images", "true")
        .header("X-Other", "nope")
        .body(fixture("typesafe/noul.request.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let seen = h.mock.last();
    assert_eq!(seen.headers["typesafe-danger-zone-images"], "true");
    assert!(seen.headers.get("x-other").is_none());
    assert_eq!(seen.headers["authorization"], format!("Bearer {TEST_KEY}"));
    h.server.shutdown().await;
}

#[tokio::test]
async fn missing_key_is_backend_unavailable_in_systemone_format() {
    let h = harness_with(None, None).await;
    let reply = unix_post(
        &h,
        "/v1/raw/systemone",
        json_headers(),
        &fixture("typesafe/noul.request.json"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["detail"]["error_type"], "backend_unavailable");
    assert!(
        v["detail"]["message"]
            .as_str()
            .unwrap()
            .contains("s1 auth set typesafe")
    );
    assert!(h.mock.all().is_empty(), "nothing sent without a key");
    h.server.shutdown().await;
}

#[tokio::test]
async fn unreachable_provider_is_backend_unavailable() {
    // Port 9 (discard) on loopback: nothing listens.
    let h = harness_with(
        Some(TEST_KEY),
        Some("[providers.typesafe]\nbase_url = \"http://127.0.0.1:9\"\n".into()),
    )
    .await;
    let reply = unix_post(&h, "/v1/raw/systemone", json_headers(), b"{}").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["detail"]["error_type"], "backend_unavailable");
    // Connection errors are retried (stage 3): 4 attempts in all.
    assert_eq!(reply.headers["s1-retries"], "3");
    h.server.shutdown().await;
}

#[tokio::test]
async fn unknown_protocol_or_provider_is_unsupported() {
    let h = harness().await;
    let reply = unix_post(&h, "/v1/raw/nope", json_headers(), b"{}").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["error"]["code"], "unsupported");

    let reply = unix_post(
        &h,
        "/v1/raw/systemone?provider=nobody",
        json_headers(),
        b"{}",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["detail"]["error_type"], "unsupported");
    h.server.shutdown().await;
}

#[tokio::test]
async fn invalid_json_still_passes_through() {
    let h = harness().await;
    let reply = unix_post(&h, "/v1/raw/systemone", json_headers(), b"not json").await;
    assert_eq!(reply.status, StatusCode::OK, "the mock answers anything");
    assert_eq!(h.mock.last().body.as_ref(), b"not json");
    assert_eq!(reply.headers["s1-backend"], "typesafe");
    assert!(reply.headers.get("s1-model").is_none());
    h.server.shutdown().await;
}

#[tokio::test]
async fn tcp_needs_bearer_token_and_shares_the_router() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/models", h.server.tcp);
    assert!(h.server.tcp.ip().is_loopback());

    let none = client.get(&url).send().await.unwrap();
    assert_eq!(none.status().as_u16(), 401);
    assert_eq!(none.headers()["www-authenticate"], "Bearer");

    let wrong = client.get(&url).bearer_auth("wrong").send().await.unwrap();
    assert_eq!(wrong.status().as_u16(), 401);

    let ok = client
        .get(&url)
        .bearer_auth(h.token.expose())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    let tcp_body: Value = serde_json::from_slice(&ok.bytes().await.unwrap()).unwrap();

    let unix = request(
        &h.paths.socket,
        Method::GET,
        "/v1/models",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        unix.status,
        StatusCode::OK,
        "the Unix socket needs no token"
    );
    let unix_body: Value = serde_json::from_slice(&unix.body).unwrap();
    assert_eq!(tcp_body, unix_body);
    h.server.shutdown().await;
}

#[tokio::test]
async fn unix_socket_has_mode_0600_and_is_removed_on_shutdown() {
    let h = harness().await;
    let socket: PathBuf = h.server.socket.clone();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    h.server.shutdown().await;
    assert!(!socket.exists());
}

#[tokio::test]
async fn second_daemon_on_the_same_socket_is_refused() {
    let h = harness().await;
    let keys = KeyResolver::new(h.paths.auth_file(), |_| None);
    let err = serve(ServerOptions {
        socket: h.paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token: h.token.clone(),
    })
    .await
    .err()
    .expect("second bind fails");
    assert!(matches!(err, DaemonError::AlreadyRunning(_)));
    h.server.shutdown().await;
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    std::fs::write(&paths.socket, b"").unwrap();
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    let token = keys.ensure_local_token().unwrap();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token,
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn models_lists_merged_catalog_with_availability() {
    let h = harness().await;
    let reply = request(
        &h.paths.socket,
        Method::GET,
        "/v1/models",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    let find = |id: &str| {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed"))
            .clone()
    };
    let jev = find("typesafe/jev-latest");
    assert_eq!(jev["state"]["available"], true);
    assert!(
        jev["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert_eq!(find("openai/gpt-6-luna")["state"]["available"], false);
    assert_eq!(find("workers-ai/clairic")["family"], "extract");
    assert_eq!(v["aliases"]["decide"]["spill"], false);
    h.server.shutdown().await;
}

#[test]
fn raw_body_bytes_always_arrive_unchanged() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = rt.block_on(harness());
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 32,
        ..ProptestConfig::default()
    });
    runner
        .run(&proptest::collection::vec(any::<u8>(), 0..4096), |body| {
            let reply = rt.block_on(unix_post(&h, "/v1/raw/systemone", json_headers(), &body));
            prop_assert_eq!(reply.status, StatusCode::OK);
            let seen = h.mock.last();
            let expected = fixture("typesafe/noul.response.json");
            prop_assert_eq!(seen.body.as_ref(), body.as_slice());
            prop_assert_eq!(reply.body.as_ref(), expected.as_slice());
            Ok(())
        })
        .unwrap();
    rt.block_on(h.server.shutdown());
}

#[tokio::test]
async fn running_daemon_holds_the_lifecycle_lock_even_without_its_socket_file() {
    // Two starters can both see a stale socket. The lock, not the socket
    // file, decides who runs.
    let h = harness().await;
    std::fs::remove_file(&h.paths.socket).unwrap();
    let keys = KeyResolver::new(h.paths.auth_file(), |_| None);
    let err = serve(ServerOptions {
        socket: h.paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token: h.token.clone(),
    })
    .await
    .err()
    .expect("second daemon is refused while the first runs");
    assert!(matches!(err, DaemonError::AlreadyRunning(_)), "{err}");
    h.server.shutdown().await;
}

#[tokio::test]
async fn shutdown_removes_only_its_own_socket() {
    let h = harness().await;
    let socket = h.paths.socket.clone();
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, b"someone else").unwrap();
    h.server.shutdown().await;
    assert_eq!(std::fs::read(&socket).unwrap(), b"someone else");
}

#[tokio::test]
async fn raw_passes_provider_redirects_through_unchanged() {
    use axum::routing::post;
    let app = axum::Router::new()
        .route(
            "/v1/systemone",
            post(|| async {
                (
                    StatusCode::FOUND,
                    [("location", "/elsewhere"), ("content-type", "text/plain")],
                    "moved",
                )
            }),
        )
        .route(
            "/elsewhere",
            axum::routing::any(|| async { (StatusCode::OK, "followed") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let h = harness_with(Some(TEST_KEY), Some(overlay_for(addr))).await;
    let reply = unix_post(
        &h,
        "/v1/raw/systemone",
        json_headers(),
        &fixture("typesafe/noul.request.json"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FOUND);
    assert_eq!(reply.body.as_ref(), b"moved");
    assert_eq!(
        reply.headers.get("location").map(|v| v.to_str().unwrap()),
        Some("/elsewhere")
    );
    h.server.shutdown().await;
}
