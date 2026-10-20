//! `POST /v1/extract`: Clairic requests routed to Workers AI, against an
//! in-process mock with the behaviour of the recorded fixtures.

mod common;

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{CF_ACCOUNT, CF_TOKEN, Mocks, fixture};
use s1::cache::Cache;
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use serde_json::{Value, json};

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    server: RunningServer,
    mocks: Mocks,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| f.set_api_key("workers-ai", &Secret::new(CF_TOKEN), Some(CF_ACCOUNT)))
        .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let mocks = Mocks::start().await;
    let catalog = Catalog::with_overlay_str(&mocks.overlay()).unwrap();
    let cache = Cache::open(&paths.cache_file(), 1 << 20).unwrap();
    let state = AppState::new(catalog, keys).with_cache(Arc::new(cache));
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
    }
}

impl Harness {
    async fn post_to(&self, path: &str, body: &[u8]) -> Reply {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        request(
            &self.paths.socket,
            Method::POST,
            path,
            h,
            Bytes::copy_from_slice(body),
        )
        .await
        .unwrap()
    }
    async fn extract(&self, body: &Value) -> Reply {
        self.post_to("/v1/extract", &serde_json::to_vec(body).unwrap())
            .await
    }
}

fn json_of(r: &Reply) -> Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
}

fn invoice(model: &str, state: &str) -> Value {
    json!({
        "model": model,
        "state": state,
        "fields": {
            "invoice_number": {"type": "text"},
            "total": {"type": "number", "decimals": 2},
            "days_to_pay": {"type": "estimate", "min": 0, "max": 120, "step": 1}
        }
    })
}

/// The `result` of a recorded envelope, as bytes.
fn recorded_result(name: &str) -> Vec<u8> {
    let raw = fixture(name);
    let v: Value = serde_json::from_slice(&raw).unwrap();
    serde_json::to_vec(&v["result"]).unwrap()
}

#[tokio::test]
async fn recorded_request_returns_the_recorded_result_without_envelope() {
    let h = harness().await;
    let body = fixture("clairic/invoice.request.json");
    let r = h.post_to("/v1/extract", &body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-backend"], "workers-ai");
    assert_eq!(r.headers["s1-model"], "workers-ai/clairic");
    assert_eq!(r.headers["content-type"], "application/json");
    let got = json_of(&r);
    let want: Value =
        serde_json::from_slice(&recorded_result("clairic/invoice.response.json")).unwrap();
    assert_eq!(got, want);
    let seen = h.mocks.clef.last();
    assert_eq!(
        seen.path,
        format!("/accounts/{CF_ACCOUNT}/ai/run/@cf/cloudflare/clairic")
    );
    assert_eq!(
        seen.body.as_ref(),
        body.as_slice(),
        "model already upstream: bytes unchanged"
    );
    assert_eq!(seen.headers["authorization"], format!("Bearer {CF_TOKEN}"));
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_sets_the_body_selector_and_answers_are_cached() {
    let h = harness().await;
    let req = invoice("extract", "INV-7 total 42");
    let r = h.extract(&req).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-cache"], "miss");
    let v = json_of(&r);
    assert_eq!(v["model"], "clairic");
    assert_eq!(v["answers"]["invoice_number"]["value"], "INV-7");
    let sent: Value = serde_json::from_slice(&h.mocks.clef.last().body).unwrap();
    let mut want = req.clone();
    want["model"] = json!("clairic");
    assert_eq!(sent, want, "only `model` changes");

    let again = h.extract(&req).await;
    assert_eq!(again.headers["s1-cache"], "hit");
    assert_eq!(again.body, r.body);
    assert_eq!(h.mocks.clef.all().len(), 1);
    // Another field: another key.
    let mut other = req.clone();
    other["fields"]["po"] = json!({"type": "text"});
    let r = h.extract(&other).await;
    assert_eq!(r.headers["s1-cache"], "miss");
    h.server.shutdown().await;
}

#[tokio::test]
async fn long_state_is_truncated_by_clairic_not_refused() {
    let h = harness().await;
    // About 75k tokens by `bytes / 4`: over the context, under the pre-check.
    let long = "INV-9 ".to_string() + &"Line: the checkout page is slow. ".repeat(9_000);
    let r = h.extract(&invoice("extract", &long)).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(json_of(&r)["truncated"], true);

    // Past the Workers AI pre-check: refused before the call.
    let calls = h.mocks.clef.all().len();
    let huge = "x".repeat(4 * 131_072 + 100);
    let r = h.extract(&invoice("workers-ai/clairic", &huge)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    let v = json_of(&r);
    assert_eq!(v["success"], false);
    assert_eq!(v["errors"][0]["code"], "too_long");
    assert_eq!(h.mocks.clef.all().len(), calls, "not sent");
    h.server.shutdown().await;
}

#[tokio::test]
async fn image_limits_and_bad_requests_are_refused_before_the_call() {
    let h = harness().await;
    let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let with_images = |images: Value| {
        let mut b = invoice("extract", "");
        b["images"] = images;
        b
    };
    // Up to four images and an empty state are fine.
    let r = h.extract(&with_images(json!([png, png, png, png]))).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let calls = h.mocks.clef.all().len();

    let big = format!(
        "data:image/png;base64,{}",
        "A".repeat(4 * 1024 * 1024 / 3 * 4 + 8)
    );
    let mut state_obj = invoice("extract", "x");
    state_obj["state"] = json!({"a": 1});
    for (body, status, code) in [
        (
            with_images(json!([png, png, png, png, png])),
            422,
            "unsupported",
        ),
        (with_images(json!([big])), 413, "too_long"),
        (
            with_images(json!(["https://example.com/a.png"])),
            400,
            "invalid_request",
        ),
        (invoice("extract", ""), 400, "invalid_request"),
        (state_obj, 400, "invalid_request"),
        (invoice("workers-ai/clef", "x"), 400, "wrong_family"),
        (invoice("decide", "x"), 400, "wrong_family"),
    ] {
        let r = h.extract(&body).await;
        assert_eq!(
            r.status.as_u16(),
            status,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        let v = json_of(&r);
        assert_eq!(v["success"], false);
        assert_eq!(v["errors"][0]["code"], code, "{v}");
    }
    assert_eq!(h.mocks.clef.all().len(), calls, "nothing sent");

    // Real base64 at and just above the limits: 4 MiB per image, 8 MiB in
    // total. 4 MiB is 3k+1 bytes ("==" padding); one byte more is "=".
    use base64::Engine as _;
    let b64 = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![0u8; n]);
    let mib4 = 4 * 1024 * 1024;
    let at = format!("data:image/png;base64,{}", b64(mib4));
    let over = format!("data:image/png;base64,{}", b64(mib4 + 1));
    let one = json!({"content_type": "image/png", "base64": b64(1)});
    for (images, status) in [
        (json!([at]), 200),
        (json!([at, at]), 200),
        (json!([over]), 413),
        (json!([at, at, one]), 413),
    ] {
        let n = images.as_array().unwrap().len();
        let before = h.mocks.clef.all().len();
        let r = h.extract(&with_images(images)).await;
        assert_eq!(
            r.status.as_u16(),
            status,
            "{n} images: {}",
            String::from_utf8_lossy(&r.body)
        );
        let sent = h.mocks.clef.all().len() - before;
        if status == 413 {
            assert_eq!(json_of(&r)["errors"][0]["code"], "too_long");
            assert_eq!(sent, 0, "refused before the call");
        } else {
            assert_eq!(sent, 1);
        }
    }

    // The Extract alias is not a Decide model.
    let systemone = json!({"model": "extract", "state": "x", "questions": {"q": {"type": "noul", "instructions": "?"}}});
    let r = h
        .post_to("/v1/systemone", &serde_json::to_vec(&systemone).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&r)["detail"]["error_type"], "wrong_family");
    h.server.shutdown().await;
}

#[tokio::test]
async fn provider_errors_keep_the_workers_ai_envelope() {
    let h = harness().await;
    let mut body = invoice("extract", "INV-1");
    body["extra"] = json!(1);
    let r = h.extract(&body).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body.as_ref(), fixture("clairic/extra_field.422.json"));
    assert_eq!(r.headers["s1-model"], "workers-ai/clairic");
    assert_eq!(r.headers["s1-cache"], "bypass");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_clairic_is_a_pure_passthrough() {
    let h = harness().await;
    let body = fixture("clairic/invoice.request.json");
    let r = h.post_to("/v1/raw/clairic", &body).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body.as_ref(), fixture("clairic/invoice.response.json"));
    assert_eq!(r.headers["s1-backend"], "workers-ai");
    assert_eq!(r.headers["s1-model"], "workers-ai/clairic");
    assert_eq!(h.mocks.clef.last().body.as_ref(), body.as_slice());
    h.server.shutdown().await;
}
