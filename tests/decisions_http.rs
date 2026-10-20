//! `POST /v1/decisions` (OpenAI Decisions ingress) and the OpenAI backend:
//! native calls, translation to and from systemone, errors in each
//! ingress's own format, compressed bodies, image URLs, and the cache,
//! against in-process mock providers.

mod common;

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use base64::Engine;
use bytes::Bytes;
use common::{CF_ACCOUNT, CF_TOKEN, Mocks, OPENAI_KEY, TEST_KEY, fixture};
use s1::cache::Cache;
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use s1::translate::fetch::FetchPolicy;
use serde_json::{Value, json};

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    _server: RunningServer,
    mocks: Mocks,
}

async fn harness_with(openai_key: &str, cache: bool) -> Harness {
    // The tests serve images on loopback; the daemon default refuses that.
    harness_full(openai_key, cache, FetchPolicy::ALLOW_LOCAL, None).await
}

async fn harness_full(
    openai_key: &str,
    cache: bool,
    images: FetchPolicy,
    openai_at: Option<std::net::SocketAddr>,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
        f.set_api_key("workers-ai", &Secret::new(CF_TOKEN), Some(CF_ACCOUNT));
        f.set_api_key("openai", &Secret::new(openai_key), None);
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let mocks = Mocks::start().await;
    let mut overlay = mocks.overlay();
    if let Some(addr) = openai_at {
        overlay = overlay.replace(&mocks.openai.to_string(), &addr.to_string());
    }
    let catalog = Catalog::with_overlay_str(&overlay).unwrap();
    let mut state = AppState::new(catalog, keys).with_image_fetch(images);
    if cache {
        state = state.with_cache(Arc::new(Cache::open(&paths.cache_file(), 1 << 20).unwrap()));
    }
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
        _server: server,
        mocks,
    }
}

async fn harness() -> Harness {
    harness_with(OPENAI_KEY, false).await
}

impl Harness {
    async fn post(&self, path: &str, body: &[u8]) -> Reply {
        self.post_h(path, &[], Bytes::copy_from_slice(body)).await
    }
    async fn post_h(
        &self,
        path: &str,
        extra: &[(&'static str, &'static str)],
        body: Bytes,
    ) -> Reply {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        for (k, v) in extra {
            h.insert(*k, HeaderValue::from_static(v));
        }
        request(&self.paths.socket, Method::POST, path, h, body)
            .await
            .unwrap()
    }
}

fn json_of(r: &Reply) -> Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
}

fn header<'a>(r: &'a Reply, name: &str) -> &'a str {
    r.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn triage(model: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&fixture("openai/triage.request.json")).unwrap();
    v["model"] = json!(model);
    serde_json::to_vec(&v).unwrap()
}

fn s1_req(model: &str, questions: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "state": "I was charged twice. yes",
        "questions": questions,
    }))
    .unwrap()
}

/// A 1x1 PNG.
fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
        .unwrap()
}

#[tokio::test]
async fn native_openai_call_passes_the_answer_through() {
    let h = harness().await;
    let r = h.post("/v1/decisions", &triage("openai/gpt-6-luna")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-backend"), "openai");
    assert_eq!(header(&r, "s1-model"), "openai/gpt-6-luna");
    assert_eq!(header(&r, "x-request-id"), "req_mock_openai");
    assert_eq!(&r.body[..], &fixture("openai/triage.response.json")[..]);
    let seen = h.mocks.luna.last();
    assert_eq!(seen.path, "/v1/decisions");
    // Only `model` is replaced; the key is s1's own.
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    let want: Value = serde_json::from_slice(&triage("gpt-6-luna")).unwrap();
    assert_eq!(sent, want);
    assert_eq!(
        seen.headers.get("authorization").unwrap(),
        &format!("Bearer {OPENAI_KEY}")
    );
}

#[tokio::test]
async fn decide_alias_reaches_clef_and_answers_in_openai_format() {
    let h = harness().await;
    let r = h.post("/v1/decisions", &triage("decide")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-model"), "workers-ai/clef-flash");
    assert_eq!(header(&r, "content-type"), "application/json");
    // Clef got systemone with q-names for unnamed questions.
    let sent: Value = serde_json::from_slice(&h.mocks.clef.last().body).unwrap();
    assert_eq!(sent["model"], "clef-flash");
    assert_eq!(sent["state"], "I was charged twice for my order.");
    let names: Vec<&String> = sent["questions"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["billing", "q1", "urgency"]);
    assert_eq!(sent["questions"]["billing"]["type"], "noul");
    assert_eq!(
        sent["questions"]["q1"]["criteria"],
        json!({"billing": "Payments", "technical": null, "bool:true": null})
    );
    // The client gets its own names, order and typed values back.
    let v = json_of(&r);
    let a = v["answers"].as_array().unwrap();
    assert_eq!(a.len(), 3);
    assert_eq!(a[0]["type"], "predicate");
    assert_eq!(a[0]["name"], "billing");
    assert_eq!(a[1]["type"], "choice");
    assert_eq!(a[1]["name"], Value::Null);
    assert_eq!(a[1]["choice"], "billing");
    let values: Vec<&Value> = a[1]["probabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["value"])
        .collect();
    assert_eq!(
        values,
        [&json!("billing"), &json!("technical"), &json!(true)]
    );
    assert_eq!(a[2]["type"], "score");
    assert_eq!(a[2]["name"], "urgency");
    assert_eq!(a[2]["probabilities"][1]["label"], "Medium");
    assert_eq!(v["usage"]["input_tokens"], 100);
}

#[tokio::test]
async fn images_go_to_jev_as_assets_and_urls_are_fetched() {
    let h = harness().await;
    // An image server for the URL case.
    let img = png();
    let app = axum::Router::new().route(
        "/cat.png",
        axum::routing::get(move || {
            let img = img.clone();
            async move { img }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let body = json!({
        "model": "decide",
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "Is there a cat?"},
            {"type": "input_image", "image_url": format!("http://{addr}/cat.png")}
        ]}],
        "questions": [{"type": "predicate", "name": "cat", "instructions": "Is there a cat?"}]
    });
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&body).unwrap())
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest");
    let sent: Value = serde_json::from_slice(&h.mocks.jev.last().body).unwrap();
    let assets = sent["assets"].as_object().unwrap();
    assert_eq!(assets.len(), 1);
    let (ph, asset) = assets.iter().next().unwrap();
    assert!(sent["state"].as_str().unwrap().contains(ph.as_str()));
    assert_eq!(asset["format"], "png");
    assert_eq!(
        asset["data_base64"],
        base64::engine::general_purpose::STANDARD.encode(png())
    );
    assert_eq!(json_of(&r)["answers"][0]["name"], "cat");

    // A URL that fails is the client's error, in OpenAI form.
    let mut bad = body.clone();
    bad["input"][0]["content"][1]["image_url"] = json!(format!("http://{addr}/missing.png"));
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&bad).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = json_of(&r);
    assert_eq!(v["error"]["code"], "invalid_request");
    assert_eq!(v["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn native_openai_gets_image_urls_unfetched() {
    let h = harness().await;
    let body = json!({
        "model": "openai/gpt-6-luna",
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://example.invalid/cat.png"}
        ]}],
        "questions": [{"type": "predicate", "instructions": "Cat?"}]
    });
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&body).unwrap())
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let sent: Value = serde_json::from_slice(&h.mocks.luna.last().body).unwrap();
    assert_eq!(
        sent["input"][0]["content"][0]["image_url"],
        "https://example.invalid/cat.png"
    );
}

#[tokio::test]
async fn systemone_client_gets_refusals_as_per_question_errors() {
    let h = harness().await;
    let body = s1_req(
        "openai/gpt-6-luna",
        json!({
            "tone": {"type": "choice", "instructions": "Tone?", "criteria": {"angry": "Upset", "calm": null}},
            "billing": {"type": "noul", "instructions": "Billing?", "criteria": {"true": "money"}},
            "nope": {"type": "noul", "instructions": "REFUSE this"}
        }),
    );
    let r = h.post("/v1/systemone", &body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-backend"), "openai");
    let sent: Value = serde_json::from_slice(&h.mocks.luna.last().body).unwrap();
    assert_eq!(sent["model"], "gpt-6-luna");
    assert_eq!(sent["input"], "I was charged twice. yes");
    let q = sent["questions"].as_array().unwrap();
    assert_eq!(q[0]["name"], "tone");
    assert_eq!(q[0]["choices"][1], json!({"value": "calm"}));
    assert_eq!(q[1]["type"], "predicate");
    assert_eq!(q[1]["instructions"], "Billing?\n\nTrue if: money");
    let v = json_of(&r);
    let keys: Vec<&String> = v["answers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["tone", "billing", "nope"]);
    assert_eq!(v["answers"]["tone"]["choice"], "angry");
    assert_eq!(v["answers"]["billing"]["noul"], 0.9);
    assert_eq!(v["answers"]["nope"]["type"], "error");
    assert_eq!(v["answers"]["nope"]["error"]["code"], "refusal");
    assert_eq!(v["model"], "gpt-6-luna");
}

#[tokio::test]
async fn errors_come_in_each_ingress_format_with_s1_codes() {
    let h = harness().await;
    // s1's own errors on /v1/decisions: OpenAI shape, s1 code.
    let r = h.post("/v1/decisions", br#"{"questions": []}"#).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = json_of(&r);
    assert_eq!(v["error"]["code"], "invalid_request");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["param"], Value::Null);

    let r = h.post("/v1/decisions", &triage("workers-ai/clairic")).await;
    assert_eq!(json_of(&r)["error"]["code"], "wrong_family");

    // Eleven levels cannot go to systemone.
    let mut v: Value = serde_json::from_slice(&triage("decide")).unwrap();
    v["questions"][2]["levels"] = json!(
        (0..11)
            .map(|i| json!({"label": format!("L{i}")}))
            .collect::<Vec<_>>()
    );
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&v).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&r)["error"]["code"], "unsupported");

    // `box` has no OpenAI form: systemone shape, s1 code.
    let body = s1_req(
        "openai/gpt-6-luna",
        json!({"where": {"type": "box", "instructions": "Where?"}}),
    );
    let r = h.post("/v1/systemone", &body).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&r)["detail"]["error_type"], "unsupported");

    // Catalog limits (OpenAI takes 2..=255 choices) are checked before
    // the call.
    let body = s1_req(
        "openai/gpt-6-luna",
        json!({"t": {"type": "choice", "instructions": "?", "criteria": {"only": null}}}),
    );
    let r = h.post("/v1/systemone", &body).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&r)["detail"]["error_type"], "unsupported");
    assert!(h.mocks.luna.all().is_empty());
}

#[tokio::test]
async fn provider_auth_errors_are_translated() {
    let h = harness_with("sk-wrong", false).await;
    let body = s1_req(
        "openai/gpt-6-luna",
        json!({"b": {"type": "noul", "instructions": "Billing?"}}),
    );
    let r = h.post("/v1/systemone", &body).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let v = json_of(&r);
    assert_eq!(v["detail"]["error_type"], "invalid_api_key");
    // The native path passes OpenAI's own error unchanged.
    let r = h.post("/v1/decisions", &triage("openai/gpt-6-luna")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_of(&r)["error"]["code"], "invalid_api_key");
    // No key value in any reply.
    assert!(!String::from_utf8_lossy(&r.body).contains("sk-wrong"));
}

#[tokio::test]
async fn openai_client_can_name_a_systemone_model() {
    let h = harness().await;
    let body = json!({
        "model": "typesafe/jev-latest",
        "input": "x",
        "questions": [{"type": "predicate", "instructions": "?"}]
    });
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&body).unwrap())
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let v = json_of(&r);
    assert_eq!(v["answers"][0]["type"], "predicate");
    assert_eq!(v["answers"][0]["name"], Value::Null);
    assert!(v["usage"]["total_tokens"].is_number());
    assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest");

    // A Jev error reaches the OpenAI client in OpenAI shape.
    let h = harness().await;
    let mut wrong = body.clone();
    wrong["model"] = json!("typesafe/jev-latest");
    wrong["questions"][0]["type"] = json!("choice");
    wrong["questions"][0]["choices"] = json!([{"value": "a"}, {"value": "b"}]);
    wrong["input"] = json!([{"role": "user", "content": [
        {"type": "input_image", "image_url": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png()))}
    ]}]);
    let r = h
        .post("/v1/decisions", &serde_json::to_vec(&wrong).unwrap())
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
}

#[tokio::test]
async fn zstd_and_gzip_bodies_are_decoded() {
    let h = harness().await;
    let plain = triage("openai/gpt-6-luna");
    let z = zstd::encode_all(&plain[..], 3).unwrap();
    let r = h
        .post_h(
            "/v1/decisions",
            &[("content-encoding", "zstd")],
            Bytes::from(z),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let seen = h.mocks.luna.last();
    assert!(seen.headers.get("content-encoding").is_none());
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["model"], "gpt-6-luna");

    // The systemone ingress decodes too (TypeSafe SDK default).
    let plain = s1_req(
        "decide",
        json!({"b": {"type": "noul", "instructions": "?"}}),
    );
    let z = zstd::encode_all(&plain[..], 3).unwrap();
    let r = h
        .post_h(
            "/v1/systemone",
            &[("content-encoding", "zstd")],
            Bytes::from(z),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );

    let r = h
        .post_h(
            "/v1/decisions",
            &[("content-encoding", "br")],
            Bytes::from(plain),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&r)["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn translated_answers_are_cached_in_the_client_format() {
    let h = harness_with(OPENAI_KEY, true).await;
    let body = triage("decide");
    let r1 = h.post("/v1/decisions", &body).await;
    assert_eq!(r1.status, StatusCode::OK);
    assert_eq!(header(&r1, "s1-cache"), "miss");
    let r2 = h.post("/v1/decisions", &body).await;
    assert_eq!(header(&r2, "s1-cache"), "hit");
    assert_eq!(r1.body, r2.body);
    assert_eq!(h.mocks.clef.all().len(), 1);
    // The same questions as systemone are another request.
    let s1 = s1_req(
        "decide",
        json!({"b": {"type": "noul", "instructions": "?"}}),
    );
    assert_eq!(
        header(&h.post("/v1/systemone", &s1).await, "s1-cache"),
        "miss"
    );
}

#[tokio::test]
async fn oversize_body_is_too_long_in_each_ingress_shape() {
    let h = harness().await;
    let big = Bytes::from(vec![b' '; s1::daemon::MAX_BODY + 1]);
    let r = h.post_h("/v1/decisions", &[], big.clone()).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!header(&r, "s1-request-id").is_empty());
    assert_eq!(header(&r, "content-type"), "application/json");
    let v = json_of(&r);
    assert_eq!(v["error"]["code"], "too_long");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    let r = h.post_h("/v1/systemone", &[], big).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!header(&r, "s1-request-id").is_empty());
    assert_eq!(json_of(&r)["detail"]["error_type"], "too_long");
    // Exactly at the limit is read (and then fails as bad JSON, not size).
    let at = Bytes::from(vec![b' '; s1::daemon::MAX_BODY]);
    let r = h.post_h("/v1/decisions", &[], at).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

// Spec "Cache key": image content is hashed, not its path (or URL).

#[tokio::test]
async fn cache_key_covers_fetched_image_content_not_the_url() {
    let h = harness_with(OPENAI_KEY, true).await;
    let served = Arc::new(std::sync::Mutex::new(png()));
    let app = {
        let served = served.clone();
        axum::Router::new().route(
            "/cat.png",
            axum::routing::get(move || {
                let img = served.lock().unwrap().clone();
                async move { img }
            }),
        )
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let body = serde_json::to_vec(&json!({
        "model": "decide",
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": format!("http://{addr}/cat.png")}
        ]}],
        "questions": [{"type": "predicate", "name": "cat", "instructions": "Is there a cat?"}]
    }))
    .unwrap();

    let r = h.post("/v1/decisions", &body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest");
    assert_eq!(header(&r, "s1-cache"), "miss");
    assert_eq!(
        header(&h.post("/v1/decisions", &body).await, "s1-cache"),
        "hit"
    );
    assert_eq!(h.mocks.jev.all().len(), 1);

    // Same URL, new content: not a stale hit.
    served.lock().unwrap().extend_from_slice(b"changed");
    let r = h.post("/v1/decisions", &body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(header(&r, "s1-cache"), "miss");
    assert_eq!(h.mocks.jev.all().len(), 2);
    let sent: Value = serde_json::from_slice(&h.mocks.jev.last().body).unwrap();
    let asset = sent["assets"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let mut changed = png();
    changed.extend_from_slice(b"changed");
    assert_eq!(
        asset["data_base64"],
        base64::engine::general_purpose::STANDARD.encode(changed)
    );
}

#[tokio::test]
async fn image_urls_s1_does_not_fetch_bypass_the_cache() {
    let h = harness_with(OPENAI_KEY, true).await;
    let body = serde_json::to_vec(&json!({
        "model": "openai/gpt-6-luna",
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://example.invalid/cat.png"}
        ]}],
        "questions": [{"type": "predicate", "instructions": "Cat?"}]
    }))
    .unwrap();
    for _ in 0..2 {
        let r = h.post("/v1/decisions", &body).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        assert_eq!(header(&r, "s1-cache"), "bypass");
    }
    assert_eq!(h.mocks.luna.all().len(), 2);
    // Review B4: a key spelled with JSON escapes is still an image URL.
    let escaped = String::from_utf8(body.clone())
        .unwrap()
        .replace("\"image_url\"", "\"image\\u005furl\"");
    assert!(escaped.contains("\\u005f"));
    for _ in 0..2 {
        let r = h.post("/v1/decisions", escaped.as_bytes()).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(header(&r, "s1-cache"), "bypass");
    }
    assert_eq!(h.mocks.luna.all().len(), 4);
    // Data URLs are content, so they still cache.
    let inline = serde_json::to_vec(&json!({
        "model": "openai/gpt-6-luna",
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="}
        ]}],
        "questions": [{"type": "predicate", "instructions": "Cat?"}]
    }))
    .unwrap();
    assert_eq!(
        header(&h.post("/v1/decisions", &inline).await, "s1-cache"),
        "miss"
    );
    assert_eq!(
        header(&h.post("/v1/decisions", &inline).await, "s1-cache"),
        "hit"
    );
}

// Review B1 (SSRF): by default the daemon fetches only public HTTPS image
// URLs; a loopback URL is refused before any connection, and the error
// does not show the URL.
#[tokio::test]
async fn default_daemon_refuses_loopback_image_urls() {
    let h = harness_full(OPENAI_KEY, false, FetchPolicy::default(), None).await;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = hits.clone();
    let app = axum::Router::new().route(
        "/cat.png",
        axum::routing::get(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { png() }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    for url in [
        format!("http://{addr}/cat.png?sig=SECRET"),
        format!("https://localhost:{}/cat.png?sig=SECRET", addr.port()),
    ] {
        let body = json!({
            "model": "decide",
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": url}
            ]}],
            "questions": [{"type": "predicate", "name": "cat", "instructions": "Is there a cat?"}]
        });
        let r = h
            .post("/v1/decisions", &serde_json::to_vec(&body).unwrap())
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{url}");
        let v = json_of(&r);
        assert_eq!(v["error"]["code"], "invalid_request");
        assert!(!String::from_utf8_lossy(&r.body).contains("SECRET"));
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(h.mocks.jev.all().is_empty());
}

// Review R2-4.2: names Clef rejects, and predicates whose instructions are
// only an appended criterion, route past Clef to Jev on the decide alias.
#[tokio::test]
async fn clef_incompatible_openai_questions_fall_back_to_jev() {
    let h = harness().await;
    for q in [
        json!({"type": "predicate", "name": "a b", "instructions": "Q?"}),
        json!({"type": "predicate", "instructions": "True if: it rains"}),
        json!({"type": "predicate"}),
        json!({"type": "choice", "instructions": "  ", "choices": [{"value": "x"}, {"value": "y"}]}),
    ] {
        let jev_before = h.mocks.jev.all().len();
        let body = json!({"model": "decide", "input": "x", "questions": [q]});
        let r = h
            .post("/v1/decisions", &serde_json::to_vec(&body).unwrap())
            .await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{q}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest", "{q}");
        assert_eq!(h.mocks.jev.all().len(), jev_before + 1, "{q}");
    }
    assert!(h.mocks.clef.all().is_empty());
}

async fn openai_tokens_counted(h: &Harness) -> u64 {
    let r = request(
        &h.paths.socket,
        Method::GET,
        "/status",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();
    let s = json_of(&r);
    s["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["provider"] == "openai")
        .map_or(0, |b| b["rate"]["tokens_last_minute"].as_u64().unwrap())
}

// Review R3-4.2: toward OpenAI every asset reference is one image, so the
// limiter is charged per reference, and more than 128 do not go out.
#[tokio::test]
async fn systemone_asset_references_cost_one_image_each_toward_openai() {
    let h = harness().await;
    let png = base64::engine::general_purpose::STANDARD.encode(png());
    let req = |n: usize| {
        serde_json::to_vec(&json!({
            "model": "openai/gpt-6-luna",
            "state": vec!["P"; n].join(" "),
            "questions": {"q": {"type": "noul", "instructions": "?"}},
            "assets": {"P": {"type": "image", "format": "png", "data_base64": png}},
        }))
        .unwrap()
    };
    let before = openai_tokens_counted(&h).await;
    let r = h.post("/v1/systemone", &req(128)).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let cost = openai_tokens_counted(&h).await - before;
    assert!(cost >= 128 * 1_027, "charged {cost}");

    let calls = h.mocks.luna.all().len();
    let r = h.post("/v1/systemone", &req(129)).await;
    assert_eq!(
        r.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(
        json_of(&r)["detail"]["error_type"],
        "unsupported",
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(h.mocks.luna.all().len(), calls, "not sent");
}

// Review R3-2.1: a raw OpenAI redirect keeps its `location`.
#[tokio::test]
async fn raw_to_openai_passes_provider_redirects_with_location() {
    let app = axum::Router::new().route(
        "/v1/decisions",
        axum::routing::post(|| async {
            (
                StatusCode::FOUND,
                [("location", "/elsewhere"), ("content-type", "text/plain")],
                "moved",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let h = harness_full(OPENAI_KEY, false, FetchPolicy::ALLOW_LOCAL, Some(addr)).await;
    let r = h
        .post("/v1/raw/openai-decisions", &triage("openai/gpt-6-luna"))
        .await;
    assert_eq!(
        r.status,
        StatusCode::FOUND,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.body.as_ref(), b"moved");
    assert_eq!(
        r.headers.get("location").map(|v| v.to_str().unwrap()),
        Some("/elsewhere")
    );
}

#[tokio::test]
async fn gzip_bodies_with_several_members_are_decoded() {
    fn gz(plain: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(plain).unwrap();
        e.finish().unwrap()
    }
    let h = harness().await;
    for (path, plain) in [
        ("/v1/decisions", triage("openai/gpt-6-luna")),
        (
            "/v1/systemone",
            s1_req(
                "decide",
                json!({"b": {"type": "noul", "instructions": "?"}}),
            ),
        ),
    ] {
        let (head, tail) = plain.split_at(20);
        let mut multi = gz(head);
        multi.extend(gz(tail));
        let r = h
            .post_h(path, &[("content-encoding", "gzip")], Bytes::from(multi))
            .await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&r.body)
        );
    }
}
