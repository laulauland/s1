//! In-process mock of a TSFM.ai-compatible `POST /v1/forecast` server,
//! shaped after the TSFM OpenAPI schema and docs examples
//! (`tests/fixtures/tsfm/`). It always produces the levels in
//! [`PRODUCED_LEVELS`], whatever the request asks for, so tests can check
//! that s1 returns the produced levels unchanged.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use s1::protocol::tsfm::{Freq, format_time, parse_time};
use serde_json::{Value, json};

pub const TSFM_KEY: &str = "tsfm-test-key-0123456789";
pub const PRODUCED_LEVELS: [f64; 3] = [0.1, 0.5, 0.9];

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/tsfm/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

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

pub async fn start() -> (SocketAddr, Recorder) {
    let rec = Recorder::default();
    let app = Router::new()
        .route("/v1/forecast", post(forecast))
        .with_state(rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, rec)
}

/// Overlay with a `tsfm` provider at the mock and a `forecast` alias.
pub fn overlay(addr: SocketAddr) -> String {
    format!(
        "[providers.tsfm]\nbase_url = \"http://{addr}\"\napi = \"tsfm\"\nauth = \"tsfm\"\n\
         [[providers.tsfm.models]]\nid = \"chronos-bolt-small\"\nupstream = \"amazon/chronos-bolt-small\"\n\
         [aliases.forecast]\nmodels = [\"tsfm/chronos-bolt-small\"]\n"
    )
}

fn json_reply(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("x-ratelimit-remaining", "999"),
        ],
        body,
    )
        .into_response()
}

async fn forecast(State(rec): State<Recorder>, headers: HeaderMap, body: Bytes) -> Response {
    rec.0.lock().unwrap().push(Seen {
        headers: headers.clone(),
        body: body.clone(),
    });
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TSFM_KEY}"))
    {
        return json_reply(
            StatusCode::UNAUTHORIZED,
            fixture("invalid_credential.401.json"),
        );
    }
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return json_reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            br#"{"detail":[{"loc":["body"],"msg":"model is required","type":"value_error"}]}"#
                .to_vec(),
        );
    };
    let docs_req: Value = serde_json::from_slice(&fixture("docs.request.json")).unwrap();
    if req == docs_req {
        return json_reply(StatusCode::OK, fixture("docs.response.json"));
    }
    let h = req["parameters"]["prediction_length"]
        .as_u64()
        .unwrap_or(24) as usize;
    let freq = req["parameters"]["frequency"]
        .as_str()
        .and_then(|f| Freq::parse(f).ok());
    let outputs: Vec<Value> = req["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|input| {
            let target = input["target"].as_array().unwrap();
            let last = target
                .iter()
                .rev()
                .find_map(|r| r[0].as_f64())
                .unwrap_or(0.0);
            let mean: Vec<Value> = (0..h).map(|s| json!([last + s as f64])).collect();
            let qp: Vec<Value> = PRODUCED_LEVELS
                .iter()
                .map(|l| {
                    json!({"level": l, "values": (0..h).map(|s| json!([last + s as f64 + (l - 0.5) * 10.0])).collect::<Vec<_>>()})
                })
                .collect();
            let timestamps = match (input["start"].as_str().and_then(parse_time), freq) {
                (Some(start), Some(f)) => json!((0..h)
                    .map(|s| format_time(f.at(start, (target.len() + s) as i64).unwrap()))
                    .collect::<Vec<_>>()),
                _ => Value::Null,
            };
            json!({"mean": mean, "quantile_predictions": qp, "timestamps": timestamps, "metadata": input["metadata"]})
        })
        .collect();
    let resp = json!({
        "id": "mock-forecast-1",
        "object": "forecast",
        "model": req["model"],
        "provider": "mock",
        "horizon": h,
        "prediction_length": h,
        "quantile_levels": PRODUCED_LEVELS,
        "outputs": outputs,
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
        "metadata": req.get("metadata").cloned().unwrap_or(Value::Null)
    });
    json_reply(StatusCode::OK, serde_json::to_vec(&resp).unwrap())
}
