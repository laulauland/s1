//! Shared test helpers: an in-process mock TypeSafe server and fixtures.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;

pub const TEST_KEY: &str = "ts-test-key-0123456789";

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<Seen>>>);

impl Recorder {
    pub fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }
    pub fn last(&self) -> Seen {
        self.all().pop().expect("mock got a request")
    }
}

/// Mock of `POST /v1/systemone` that behaves like TypeSafe for auth:
/// no key -> 403, wrong key -> 401, right key -> fixture answer.
pub fn mock_typesafe_router(rec: Recorder) -> Router {
    Router::new()
        .route("/v1/systemone", post(systemone))
        .with_state(rec)
}

async fn systemone(
    State(rec): State<Recorder>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    rec.0.lock().unwrap().push(Seen {
        headers: headers.clone(),
        body,
    });
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let rid = [(
        "x-typesafe-request-id",
        "req_0123456789abcdef0123456789abcdef",
    )];
    match auth.as_deref() {
        None => (
            StatusCode::FORBIDDEN,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/missing_key.403.json"),
        ),
        Some(a) if a == format!("Bearer {TEST_KEY}") => (
            StatusCode::OK,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/noul.response.json"),
        ),
        Some(_) => (
            StatusCode::UNAUTHORIZED,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/invalid_key.401.json"),
        ),
    }
}

/// Start the mock on 127.0.0.1 in the current runtime.
pub async fn start_mock() -> (SocketAddr, Recorder) {
    let rec = Recorder::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mock_typesafe_router(rec.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, rec)
}

/// Overlay that points the `typesafe` provider at a mock.
pub fn overlay_for(addr: SocketAddr) -> String {
    format!("[providers.typesafe]\nbase_url = \"http://{addr}\"\n")
}
