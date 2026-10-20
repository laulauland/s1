//! Shared test helpers: in-process mock TypeSafe and Workers AI servers, and fixtures.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

pub const TEST_KEY: &str = "ts-test-key-0123456789";
/// A key the mock treats like no key at all (TypeSafe's 403 case). s1
/// always sends a bearer key, so tests use this to reach that branch.
pub const MISSING_KEY: &str = "ts-key-the-mock-treats-as-missing";
pub const CF_TOKEN: &str = "cf-test-token-0123456789";
pub const CF_ACCOUNT: &str = "acc0123";

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
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
/// no key (or [`MISSING_KEY`]) -> 403, wrong key -> 401, right key -> fixture answer.
pub fn mock_typesafe_router(rec: Recorder) -> Router {
    Router::new()
        .route("/v1/systemone", post(systemone))
        .layer(DefaultBodyLimit::disable())
        .with_state(rec)
}

async fn systemone(
    State(rec): State<Recorder>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    rec.0.lock().unwrap().push(Seen {
        path: "/v1/systemone".into(),
        headers: headers.clone(),
        body: body.clone(),
    });
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let rid = [(
        "x-typesafe-request-id",
        "req_0123456789abcdef0123456789abcdef",
    )];
    let missing = format!("Bearer {MISSING_KEY}");
    match auth.as_deref() {
        None => (
            StatusCode::FORBIDDEN,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/missing_key.403.json"),
        ),
        Some(a) if a == missing => (
            StatusCode::FORBIDDEN,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/missing_key.403.json"),
        ),
        Some(a) if a == format!("Bearer {TEST_KEY}") => {
            let (status, body) = fake_jev(&headers, &body).await;
            (status, rid, [("content-type", "application/json")], body)
        }
        Some(_) => (
            StatusCode::UNAUTHORIZED,
            rid,
            [("content-type", "application/json")],
            fixture("typesafe/invalid_key.401.json"),
        ),
    }
}

/// Jev behaviour for an authorized request: the recorded fixture for the
/// fixture request (and anything that is not a systemone body), else
/// synthesized answers. Enforces the images opt-in header like TypeSafe.
async fn fake_jev(headers: &HeaderMap, body: &[u8]) -> (StatusCode, Vec<u8>) {
    let fixture_req: Value =
        serde_json::from_slice(&fixture("typesafe/noul.request.json")).unwrap();
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return (StatusCode::OK, fixture("typesafe/noul.response.json"));
    };
    if req.get("questions").is_none() || req["questions"] == fixture_req["questions"] {
        return (StatusCode::OK, fixture("typesafe/noul.response.json"));
    }
    let images_on = headers.get("typesafe-danger-zone-images").is_some();
    let needs_images = req.get("assets").is_some()
        || req["questions"]
            .as_object()
            .is_some_and(|q| q.values().any(|q| q["type"] == "box"));
    if needs_images && !images_on {
        return (
            StatusCode::BAD_REQUEST,
            br#"{"detail":{"error_type":"invalid_request_error","message":"Images are not enabled."}}"#.to_vec(),
        );
    }
    mock_sleep(&req).await;
    let model = match req["model"].as_str() {
        Some("jev-latest") => "jev-1.13.0",
        Some(m) => m,
        None => "jev-1.13.0",
    };
    (
        StatusCode::OK,
        serde_json::to_vec(&fake_response(&req, model)).unwrap(),
    )
}

/// Sleep `N` ms when the state holds `sleep:N` (for ordering tests).
async fn mock_sleep(req: &Value) {
    let state = req["state"].to_string();
    if let Some(rest) = state.split("sleep:").nth(1) {
        let ms: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(ms) = ms.parse::<u64>() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

/// Deterministic systemone answers: noul is 0.9 when the state text holds
/// "yes", else 0.1; choice picks the first option; score the top level.
pub fn fake_response(req: &Value, model: &str) -> Value {
    let state = req["state"].to_string();
    let mut answers = serde_json::Map::new();
    for (name, q) in req["questions"].as_object().into_iter().flatten() {
        let a = match q["type"].as_str() {
            Some("noul") => {
                json!({"type": "noul", "noul": if state.contains("yes") { 0.9 } else { 0.1 }})
            }
            Some("choice") => {
                let opts: Vec<&String> = q["criteria"].as_object().unwrap().keys().collect();
                let mut p = serde_json::Map::new();
                for (i, o) in opts.iter().enumerate() {
                    let v = if i == 0 {
                        0.7
                    } else {
                        0.3 / (opts.len() - 1) as f64
                    };
                    p.insert((*o).clone(), json!(v));
                }
                json!({"type": "choice", "choice": opts[0], "confidence": 0.6, "probabilities": p})
            }
            Some("score") => {
                let levels = q["criteria"].as_array().unwrap();
                let mut p = serde_json::Map::new();
                let mut legend = serde_json::Map::new();
                for (i, l) in levels.iter().enumerate() {
                    let v = if i + 1 == levels.len() { 1.0 } else { 0.0 };
                    p.insert(i.to_string(), json!(v));
                    legend.insert(i.to_string(), l.clone());
                }
                json!({"type": "score", "score": (levels.len() - 1) as f64, "confidence": 0.5, "legend": legend, "probabilities": p})
            }
            Some("box") => {
                json!({"type": "box", "box": {"top": 0.1, "bottom": 0.9, "left": 0.2, "right": 0.8}, "confidence": null})
            }
            _ => json!({"type": "unknown"}),
        };
        answers.insert(name.clone(), a);
    }
    json!({"model": model, "answers": answers, "usage": {"input_tokens": 100, "output_tokens": 0}})
}

/// Mock of Workers AI `POST /accounts/{account}/ai/run/{*model}` for Clef,
/// with the behaviour seen live (fixtures in `workers-ai/`).
pub fn mock_workers_ai_router(rec: Recorder) -> Router {
    Router::new()
        .route("/accounts/{account}/ai/run/{*model}", post(workers_ai_run))
        .layer(DefaultBodyLimit::disable())
        .with_state(rec)
}

fn envelope_reply(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("cf-ai-req-id", "mock-req-1"),
        ],
        body,
    )
        .into_response()
}

async fn workers_ai_run(
    State(rec): State<Recorder>,
    Path((account, model)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    rec.0.lock().unwrap().push(Seen {
        path: format!("/accounts/{account}/ai/run/{model}"),
        headers: headers.clone(),
        body: body.clone(),
    });
    let authorized = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {CF_TOKEN}"));
    if !authorized || account != CF_ACCOUNT {
        return envelope_reply(
            StatusCode::UNAUTHORIZED,
            fixture("workers-ai/invalid_token.401.json"),
        );
    }
    // Test trigger: a provider redirect, to check that `location` passes.
    if body.windows(11).any(|w| w == b"REDIRECT_ME") {
        return (
            StatusCode::FOUND,
            [("location", "/elsewhere"), ("content-type", "text/plain")],
            "moved",
        )
            .into_response();
    }
    let selector = model.rsplit('/').next().unwrap_or("").to_string();
    let ctx: usize = match selector.as_str() {
        "clef-flash" => 24576,
        _ => 65536,
    };
    let parsed = serde_json::from_slice::<Value>(&body);
    // Live: the `bytes / 4` pre-check does not count image data (a 325 KB
    // body with a 244 KB JPEG passed clef-flash; 263 KB of text did not).
    let image_bytes: usize = parsed
        .as_ref()
        .ok()
        .and_then(|r| r["images"].as_array())
        .map_or(0, |a| {
            a.iter().map(|i| i.as_str().map_or(0, str::len)).sum()
        });
    let text_bytes = body.len().saturating_sub(image_bytes);
    if text_bytes / 4 > ctx {
        return envelope_reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            fixture("workers-ai/context_estimate.413.json"),
        );
    }
    let Ok(req) = parsed else {
        return envelope_reply(
            StatusCode::BAD_REQUEST,
            br#"{"errors":[{"message":"AiError: Bad input: Error: required properties at '/' are 'model,state,questions'","code":5006}],"success":false,"result":{},"messages":[]}"#.to_vec(),
        );
    };
    if req["model"].as_str() != Some(selector.as_str()) {
        return envelope_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            fixture("workers-ai/model_mismatch.422.json"),
        );
    }
    let allowed = ["model", "state", "questions", "images"];
    if req
        .as_object()
        .unwrap()
        .keys()
        .any(|k| !allowed.contains(&k.as_str()))
    {
        return envelope_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            fixture("workers-ai/extra_field.422.json"),
        );
    }
    // Clef question rules (live, 2026-10-20): a missing or null
    // `instructions` gives 400 code 5006 (a misleading "required
    // properties" message); a blank string gives 422 code 3030; ids must
    // match `^[A-Za-z0-9_.-]{1,100}$` (422 code 3030). Other values pass.
    let questions = req["questions"].as_object().cloned().unwrap_or_default();
    if questions.values().any(|q| q["instructions"].is_null()) {
        return envelope_reply(
            StatusCode::BAD_REQUEST,
            fixture("workers-ai/missing_instructions.400.json"),
        );
    }
    if questions.values().any(|q| {
        q["instructions"]
            .as_str()
            .is_some_and(|s| s.trim().is_empty())
    }) {
        return envelope_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            fixture("workers-ai/blank_instructions.422.json"),
        );
    }
    let bad_id = questions.keys().any(|k| {
        !(1..=100).contains(&k.len())
            || !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    });
    if bad_id {
        return envelope_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            fixture("workers-ai/question_name.422.json"),
        );
    }
    // Real tokens are about bytes / 3.2: mimic the explicit overflow error.
    if text_bytes * 10 / 32 > ctx {
        return envelope_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            fixture("workers-ai/too_many_tokens.422.json"),
        );
    }
    let fixture_req: Value =
        serde_json::from_slice(&fixture("workers-ai/clef.request.json")).unwrap();
    if req == fixture_req {
        return envelope_reply(StatusCode::OK, fixture("workers-ai/clef.response.json"));
    }
    mock_sleep(&req).await;
    let mut result = fake_response(&req, &selector);
    if req["state"].to_string().contains("TRUNCATE_ME") {
        result["usage"]["input_tokens"] = json!(ctx);
    }
    let env = json!({"result": result, "success": true, "errors": [], "messages": []});
    envelope_reply(StatusCode::OK, serde_json::to_vec(&env).unwrap())
}

/// Both mocks, started in the current runtime.
pub struct Mocks {
    pub typesafe: SocketAddr,
    pub jev: Recorder,
    pub workers_ai: SocketAddr,
    pub clef: Recorder,
}

impl Mocks {
    pub async fn start() -> Self {
        let (typesafe, jev) = start_mock().await;
        let clef = Recorder::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let workers_ai = listener.local_addr().unwrap();
        let app = mock_workers_ai_router(clef.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            typesafe,
            jev,
            workers_ai,
            clef,
        }
    }

    /// Overlay pointing both providers at the mocks.
    pub fn overlay(&self) -> String {
        format!(
            "[providers.typesafe]\nbase_url = \"http://{}\"\n[providers.workers-ai]\nbase_url = \"http://{}\"\n",
            self.typesafe, self.workers_ai
        )
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
