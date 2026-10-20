//! `POST /v1/systemone`: routing by model (ids, bare names, aliases), Jev
//! and Clef quirks, against in-process mock providers.

mod common;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use base64::Engine;
use bytes::Bytes;
use common::{CF_ACCOUNT, CF_TOKEN, MISSING_KEY, Mocks, TEST_KEY, fixture};
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

#[derive(Clone, Copy)]
struct Keys {
    typesafe: bool,
    workers_ai: bool,
    account: bool,
}

const ALL: Keys = Keys {
    typesafe: true,
    workers_ai: true,
    account: true,
};

async fn harness_with(keys_on: Keys, extra_overlay: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        if keys_on.typesafe {
            f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
        }
        if keys_on.workers_ai {
            f.set_api_key(
                "workers-ai",
                &Secret::new(CF_TOKEN),
                keys_on.account.then_some(CF_ACCOUNT),
            );
        }
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let mocks = Mocks::start().await;
    let catalog = Catalog::with_overlay_str(&(mocks.overlay() + extra_overlay)).unwrap();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(catalog, keys),
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

async fn harness() -> Harness {
    harness_with(ALL, "").await
}

impl Harness {
    async fn post(&self, body: &[u8]) -> Reply {
        self.post_to("/v1/systemone", body).await
    }
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
}

fn json_of(r: &Reply) -> Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
}

fn req(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "state": "I was charged twice. yes",
        "questions": {
            "billing": {"type": "noul", "instructions": "Is this about billing?"},
            "tone": {"type": "choice", "instructions": "Tone?", "criteria": {"angry": "Upset", "calm": "Neutral"}}
        }
    }))
    .unwrap()
}

#[tokio::test]
async fn jev_by_bare_name_sends_original_bytes() {
    let h = harness().await;
    let body = fixture("typesafe/noul.request.json");
    let r = h.post(&body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.body.as_ref(), fixture("typesafe/noul.response.json"));
    assert_eq!(r.headers["s1-backend"], "typesafe");
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    assert!(
        r.headers["s1-request-id"]
            .to_str()
            .unwrap()
            .starts_with("req_")
    );
    let seen = h.mocks.jev.last();
    assert_eq!(
        seen.body.as_ref(),
        body.as_slice(),
        "model already upstream: bytes unchanged"
    );
    assert!(h.mocks.clef.all().is_empty());
    h.server.shutdown().await;
}

#[tokio::test]
async fn full_id_is_rewritten_to_the_upstream_id() {
    let h = harness().await;
    let r = h.post(&req("typesafe/jev-latest")).await;
    assert_eq!(r.status, StatusCode::OK);
    let sent: Value = serde_json::from_slice(&h.mocks.jev.last().body).unwrap();
    assert_eq!(sent["model"], "jev-latest");
    let v = json_of(&r);
    assert_eq!(v["model"], "jev-1.13.0");
    assert_eq!(v["answers"]["billing"]["noul"], 0.9);
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_routes_to_clef_with_model_in_path_and_body_and_no_envelope() {
    let h = harness().await;
    let r = h.post(&req("decide")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-backend"], "workers-ai");
    assert_eq!(r.headers["s1-model"], "workers-ai/clef-flash");
    assert_eq!(r.headers["content-type"], "application/json");
    assert_eq!(r.headers["cf-ai-req-id"], "mock-req-1");
    let seen = h.mocks.clef.last();
    assert_eq!(
        seen.path,
        format!("/accounts/{CF_ACCOUNT}/ai/run/@cf/cloudflare/clef-flash")
    );
    assert_eq!(seen.headers["authorization"], format!("Bearer {CF_TOKEN}"));
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["model"], "clef-flash");
    let v = json_of(&r);
    assert!(v.get("result").is_none() && v.get("success").is_none());
    assert_eq!(v["model"], "clef-flash");
    assert_eq!(v["answers"]["tone"]["choice"], "angry");
    assert!(h.mocks.jev.all().is_empty());
    h.server.shutdown().await;
}

#[tokio::test]
async fn recorded_clef_fixture_comes_back_without_envelope() {
    let h = harness().await;
    let r = h.post(&fixture("workers-ai/clef.request.json")).await;
    assert_eq!(r.status, StatusCode::OK);
    let env: Value = serde_json::from_slice(&fixture("workers-ai/clef.response.json")).unwrap();
    assert_eq!(json_of(&r), env["result"]);
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_skips_providers_without_keys() {
    let h = harness_with(
        Keys {
            workers_ai: false,
            ..ALL
        },
        "",
    )
    .await;
    let r = h.post(&req("decide")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_skips_workers_ai_without_account_id() {
    // A Cloudflare token alone cannot call Workers AI: the alias must fall
    // through to Jev instead of picking Clef and failing with 503.
    let h = harness_with(
        Keys {
            account: false,
            ..ALL
        },
        "",
    )
    .await;
    let r = h.post(&req("decide")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-backend"], "typesafe");
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    assert!(h.mocks.clef.all().is_empty());
    h.server.shutdown().await;
}

#[tokio::test]
async fn models_list_workers_ai_as_unavailable_without_account_id() {
    let h = harness_with(
        Keys {
            account: false,
            ..ALL
        },
        "",
    )
    .await;
    let r = request(
        &h.paths.socket,
        Method::GET,
        "/v1/models",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();
    let v = json_of(&r);
    let state = |id: &str| {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed"))["state"]["available"]
            .clone()
    };
    assert_eq!(state("workers-ai/clef-flash"), false);
    assert_eq!(state("typesafe/jev-latest"), true);
    h.server.shutdown().await;
}

#[tokio::test]
async fn no_keys_at_all_is_backend_unavailable() {
    let h = harness_with(
        Keys {
            typesafe: false,
            workers_ai: false,
            account: false,
        },
        "",
    )
    .await;
    let r = h.post(&req("decide")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_of(&r)["detail"]["error_type"], "backend_unavailable");
    h.server.shutdown().await;
}

#[tokio::test]
async fn routing_errors_use_s1_codes_in_systemone_format() {
    let h = harness().await;
    let cases: [(&[u8], u16, &str); 4] = [
        (&req("workers-ai/clairic"), 400, "wrong_family"),
        (&req("extract"), 400, "wrong_family"),
        (&req("no/such-model"), 422, "unsupported"),
        (b"{not json", 400, "invalid_request"),
    ];
    for (body, status, code) in cases {
        let r = h.post(body).await;
        assert_eq!(r.status.as_u16(), status, "{code}");
        assert_eq!(json_of(&r)["detail"]["error_type"], code);
    }
    assert!(h.mocks.jev.all().is_empty() && h.mocks.clef.all().is_empty());
    h.server.shutdown().await;
}

#[tokio::test]
async fn assets_route_to_jev_with_images_header_added() {
    let h = harness().await;
    let body = json!({
        "model": "decide",
        "state": {"door": "img-1"},
        "assets": {"img-1": {"type": "image", "format": "png", "data_base64": "iVBORw0KGgo="}},
        "questions": {"person": {"type": "noul", "instructions": "Is there a person in `door`?"}}
    });
    let r = h.post(&serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    assert_eq!(
        h.mocks.jev.last().headers["typesafe-danger-zone-images"],
        "true"
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn box_needs_jev_preview() {
    let h = harness().await;
    let body = |model: &str| {
        serde_json::to_vec(&json!({
            "model": model,
            "state": "photo",
            "assets": {"photo": {"type": "image", "format": "png", "data_base64": "iVBORw0KGgo="}},
            "questions": {"red": {"type": "box", "instructions": "Locate the red rectangle."}}
        }))
        .unwrap()
    };
    let r = h.post(&body("workers-ai/clef")).await;
    assert_eq!(r.status.as_u16(), 422);
    assert_eq!(json_of(&r)["detail"]["error_type"], "unsupported");
    let r = h.post(&body("jev-preview")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(json_of(&r)["answers"]["red"]["type"], "box");
    assert_eq!(
        h.mocks.jev.last().headers["typesafe-danger-zone-images"],
        "true"
    );
    h.server.shutdown().await;
}

fn long_req(model: &str, bytes: usize) -> Vec<u8> {
    let filler = "Line: the checkout page is slow today. ".repeat(bytes / 40);
    serde_json::to_vec(&json!({
        "model": model,
        "state": filler,
        "questions": {"billing": {"type": "noul", "instructions": "Billing?"}}
    }))
    .unwrap()
}

#[tokio::test]
async fn clef_long_input_is_too_long_never_silently_truncated() {
    let h = harness().await;
    // Surely over clef-flash: refused before any call.
    let r = h.post(&long_req("workers-ai/clef-flash", 120_000)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_of(&r)["detail"]["error_type"], "too_long");
    assert!(h.mocks.clef.all().is_empty());

    // Over in real tokens: Clef's explicit error maps to too_long.
    let r = h.post(&long_req("workers-ai/clef-flash", 90_000)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_of(&r)["detail"]["error_type"], "too_long");
    assert_eq!(h.mocks.clef.all().len(), 1);
    assert_eq!(r.headers["s1-model"], "workers-ai/clef-flash");

    // A success that used the whole context: treated as truncated.
    let body = serde_json::to_vec(&json!({
        "model": "clef",
        "state": "TRUNCATE_ME",
        "questions": {"billing": {"type": "noul", "instructions": "Billing?"}}
    }))
    .unwrap();
    let r = h.post(&body).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        json_of(&r)["detail"]["message"]
            .as_str()
            .unwrap()
            .contains("truncated")
    );

    // The alias sends long input to the roomier Jev instead.
    let r = h.post(&long_req("decide", 90_000)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    h.server.shutdown().await;
}

#[tokio::test]
async fn jev_long_context_header_added_when_needed() {
    let h = harness().await;
    let r = h.post(&long_req("jev-latest", 300_000)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h.mocks.jev.last().headers["typesafe-danger-zone-long-context"],
        "true"
    );
    let r = h.post(&req("jev-latest")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        h.mocks
            .jev
            .last()
            .headers
            .get("typesafe-danger-zone-long-context")
            .is_none()
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn clef_errors_lose_the_envelope_and_keep_status() {
    let h = harness().await;
    let mut cf = s1::secrets::AuthFile::load(&h.paths.auth_file()).unwrap();
    cf.set_api_key("workers-ai", &Secret::new("wrong"), Some(CF_ACCOUNT));
    cf.save(&h.paths.auth_file()).unwrap();
    let r = h.post(&req("workers-ai/clef")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let v = json_of(&r);
    assert_eq!(v["detail"]["error_type"], "authentication_error");
    assert_eq!(v["detail"]["provider_errors"][0]["code"], 10000);
    assert_eq!(r.headers["s1-backend"], "workers-ai");
    h.server.shutdown().await;
}

#[tokio::test]
async fn clef_without_account_id_is_backend_unavailable() {
    let h = harness_with(
        Keys {
            account: false,
            ..ALL
        },
        "",
    )
    .await;
    let r = h.post(&req("workers-ai/clef")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    let msg = json_of(&r)["detail"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("CLOUDFLARE_ACCOUNT_ID"), "{msg}");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_passthrough_reaches_clef_with_selector_in_body() {
    let h = harness().await;
    let r = h
        .post_to(
            "/v1/raw/systemone?provider=workers-ai",
            &fixture("workers-ai/clef.request.json"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["s1-model"], "workers-ai/clef-flash");
    // Pure passthrough: the Workers AI envelope comes back unchanged.
    assert_eq!(r.body.as_ref(), fixture("workers-ai/clef.response.json"));
    assert!(
        h.mocks
            .clef
            .last()
            .path
            .ends_with("@cf/cloudflare/clef-flash")
    );
    h.server.shutdown().await;
}

/// Turn the stored TypeSafe key into one the mock treats as missing.
fn use_missing_key(h: &Harness) {
    KeyResolver::new(h.paths.auth_file(), |_| None)
        .update(|f| f.set_api_key("typesafe", &Secret::new(MISSING_KEY), None))
        .unwrap();
}

#[tokio::test]
async fn typed_ingress_turns_typesafe_403_into_clear_401() {
    let h = harness().await;
    use_missing_key(&h);
    let r = h.post(&req("typesafe/jev-latest")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers["s1-upstream-status"], "403");
    let v = json_of(&r);
    assert_eq!(v["detail"]["error_type"], "authentication_error");
    let msg = v["detail"]["message"].as_str().unwrap();
    assert!(msg.contains("s1 auth set typesafe"), "{msg}");
    // Raw on the same daemon keeps the provider's 403.
    let raw = h.post_to("/v1/raw/systemone", &req("jev-latest")).await;
    assert_eq!(raw.status, StatusCode::FORBIDDEN);
    assert_eq!(raw.body.as_ref(), fixture("typesafe/missing_key.403.json"));
    h.server.shutdown().await;
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000IHDR-not-a-real-image";

fn path_req(path: &std::path::Path) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "decide",
        "state": {"door": "img-1"},
        "assets": {"img-1": {"type": "image", "s1_path": path}},
        "questions": {"person": {"type": "noul", "instructions": "Is there a person in `door`?"}}
    }))
    .unwrap()
}

#[tokio::test]
async fn socket_reads_image_paths_into_base64_assets() {
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("door.png");
    std::fs::write(&p, PNG).unwrap();
    let r = h.post(&path_req(&p)).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    let seen = h.mocks.jev.last();
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(
        sent["assets"]["img-1"],
        json!({
            "type": "image",
            "format": "png",
            "data_base64": base64::engine::general_purpose::STANDARD.encode(PNG)
        })
    );
    assert_eq!(sent["model"], "jev-latest");
    assert!(!String::from_utf8_lossy(&seen.body).contains("s1_path"));
    assert_eq!(seen.headers["typesafe-danger-zone-images"], "true");
    h.server.shutdown().await;
}

#[tokio::test]
async fn tcp_rejects_image_paths_without_reading_them() {
    let h = harness().await;
    let token = KeyResolver::new(h.paths.auth_file(), |_| None)
        .ensure_local_token()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("door.png");
    std::fs::write(&p, PNG).unwrap();
    let r = reqwest::Client::new()
        .post(format!("http://{}/v1/systemone", h.server.tcp))
        .bearer_auth(token.expose())
        .header("content-type", "application/json")
        .body(path_req(&p))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let v: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(v["detail"]["error_type"], "invalid_request");
    let msg = v["detail"]["message"].as_str().unwrap();
    assert!(msg.contains("Unix socket"), "{msg}");
    assert!(h.mocks.jev.all().is_empty() && h.mocks.clef.all().is_empty());
    h.server.shutdown().await;
}

#[tokio::test]
async fn missing_and_oversized_image_files_fail_before_any_provider_call() {
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();

    let r = h.post(&path_req(&dir.path().join("nope.png"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = json_of(&r);
    assert_eq!(v["detail"]["error_type"], "invalid_request");
    assert!(
        v["detail"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read image"),
        "{v}"
    );

    let big = dir.path().join("big.png");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(s1::images::MAX_IMAGE_FILE + 1)
        .unwrap();
    let r = h.post(&path_req(&big)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_of(&r)["detail"]["error_type"], "too_long");

    assert!(h.mocks.jev.all().is_empty() && h.mocks.clef.all().is_empty());
    h.server.shutdown().await;
}

/// A 2 MB photo as base64 (about 2.7 MB of text).
fn photo_base64() -> String {
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    jpeg.resize(2_000_000, 0x5A);
    base64::engine::general_purpose::STANDARD.encode(jpeg)
}

#[tokio::test]
async fn large_images_are_counted_per_image_not_as_text() {
    let h = harness().await;
    let photo = photo_base64();
    // Jev by id, image in `assets`: about 680k "tokens" if counted as text.
    let body = json!({
        "model": "typesafe/jev-latest",
        "state": {"door": "img-1"},
        "assets": {"img-1": {"type": "image", "format": "jpeg", "data_base64": photo}},
        "questions": {"person": {"type": "noul", "instructions": "Is there a person in `door`?"}}
    });
    let r = h.post(&serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    let sent: Value = serde_json::from_slice(&h.mocks.jev.last().body).unwrap();
    assert_eq!(sent["assets"]["img-1"]["data_base64"], json!(photo));

    // Clef via the alias, image in an `images` array: fits clef-flash.
    let body = json!({
        "model": "decide",
        "state": "A photo of a door. yes",
        "images": [format!("data:image/jpeg;base64,{photo}")],
        "questions": {"person": {"type": "noul", "instructions": "Is there a person?"}}
    });
    let r = h.post(&serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "workers-ai/clef-flash");
    assert_eq!(h.mocks.clef.all().len(), 1);
    assert_eq!(json_of(&r)["answers"]["person"]["noul"], 0.9);
    h.server.shutdown().await;
}

#[tokio::test]
async fn socket_reads_a_large_image_path_and_routes_it() {
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("door.png");
    let mut png = PNG.to_vec();
    png.resize(2_000_000, 0);
    std::fs::write(&p, &png).unwrap();
    let r = h.post(&path_req(&p)).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "typesafe/jev-latest");
    h.server.shutdown().await;
}

/// The reply is the mock's envelope fixture: status and bytes unchanged.
fn assert_envelope(r: &Reply, status: StatusCode, name: &str) {
    assert_eq!(r.status, status, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.body.as_ref(), fixture(name));
}

#[tokio::test]
async fn raw_to_workers_ai_is_pure_passthrough() {
    let h = harness().await;
    let raw = "/v1/raw/systemone?provider=workers-ai";

    // Success: the envelope comes back unchanged.
    let r = h
        .post_to(raw, &fixture("workers-ai/clef.request.json"))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body.as_ref(), fixture("workers-ai/clef.response.json"));
    assert_eq!(
        h.mocks.clef.last().body.as_ref(),
        fixture("workers-ai/clef.request.json")
    );

    // The body `model` is not rewritten: path and body disagree, and the
    // provider's own 422 comes back unchanged.
    let body = req("workers-ai/clef");
    let r = h.post_to(raw, &body).await;
    assert_eq!(h.mocks.clef.last().body.as_ref(), body.as_slice());
    assert_envelope(
        &r,
        StatusCode::UNPROCESSABLE_ENTITY,
        "workers-ai/model_mismatch.422.json",
    );

    // No s1 size guard: long text reaches the provider, and its 413 comes
    // back in the envelope, not as an s1 `too_long`.
    let n = h.mocks.clef.all().len();
    let r = h.post_to(raw, &long_req("clef-flash", 120_000)).await;
    assert_eq!(h.mocks.clef.all().len(), n + 1);
    assert_envelope(
        &r,
        StatusCode::PAYLOAD_TOO_LARGE,
        "workers-ai/context_estimate.413.json",
    );

    // No truncation check: a success that used the whole context passes.
    let body = serde_json::to_vec(&json!({
        "model": "clef",
        "state": "TRUNCATE_ME",
        "questions": {"billing": {"type": "noul", "instructions": "Billing?"}}
    }))
    .unwrap();
    let r = h.post_to(raw, &body).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = json_of(&r);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"]["usage"]["input_tokens"], 65536);
    h.server.shutdown().await;
}

/// `path_req` with the `s1_path` key spelled with a `_` escape.
fn escaped_path_req(path: &std::path::Path) -> Vec<u8> {
    let escaped = format!("s1{}u005fpath", char::from(0x5c_u8));
    let body = String::from_utf8(path_req(path)).unwrap();
    assert!(body.contains("\"s1_path\""));
    body.replace("\"s1_path\"", &format!("\"{escaped}\""))
        .into_bytes()
}

#[tokio::test]
async fn escaped_path_keys_are_read_on_the_socket_and_refused_on_tcp() {
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("door.png");
    std::fs::write(&p, PNG).unwrap();
    let body = escaped_path_req(&p);

    let r = h.post(&body).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let sent: Value = serde_json::from_slice(&h.mocks.jev.last().body).unwrap();
    assert_eq!(sent["assets"]["img-1"]["format"], "png");
    assert!(sent["assets"]["img-1"].get("s1_path").is_none());

    let token = KeyResolver::new(h.paths.auth_file(), |_| None)
        .ensure_local_token()
        .unwrap();
    let before = h.mocks.jev.all().len();
    let r = reqwest::Client::new()
        .post(format!("http://{}/v1/systemone", h.server.tcp))
        .bearer_auth(token.expose())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let v: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(v["detail"]["error_type"], "invalid_request");
    assert_eq!(h.mocks.jev.all().len(), before);
    h.server.shutdown().await;
}

#[tokio::test]
async fn image_path_expansion_keeps_exact_numbers() {
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("door.png");
    std::fs::write(&p, PNG).unwrap();
    let body = String::from_utf8(path_req(&p)).unwrap().replace(
        r#""state":{"door":"img-1"}"#,
        r#""state":{"door":"img-1","n":18446744073709551617}"#,
    );
    assert!(body.contains("18446744073709551617"));
    let r = h.post(body.as_bytes()).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let sent = String::from_utf8(h.mocks.jev.last().body.to_vec()).unwrap();
    assert!(
        sent.contains(r#""state":{"door":"img-1","n":18446744073709551617}"#),
        "{sent}"
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_falls_through_to_jev_when_clef_would_reject_the_questions() {
    let h = harness().await;
    for questions in [
        json!({"billing": {"type": "noul"}}),
        json!({"billing": {"type": "noul", "instructions": " "}}),
        json!({"has space": {"type": "noul", "instructions": "Billing?"}}),
    ] {
        let body = json!({"model": "decide", "state": "x", "questions": questions});
        let r = h.post(&serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{questions}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert_eq!(r.headers["s1-model"], "typesafe/jev-latest", "{questions}");
    }
    assert!(h.mocks.clef.all().is_empty());
    // Named Clef: refused before any call.
    let body =
        json!({"model": "workers-ai/clef", "state": "x", "questions": {"q": {"type": "noul"}}});
    let r = h.post(&serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&r)["detail"]["error_type"], "unsupported");
    assert!(h.mocks.clef.all().is_empty());
    // The mock enforces the same rule on raw passthrough.
    let raw = json!({"model": "clef", "state": "x", "questions": {"q": {"type": "noul"}}});
    let r = h
        .post_to(
            "/v1/raw/systemone?provider=workers-ai",
            &serde_json::to_vec(&raw).unwrap(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(
        r.body.as_ref(),
        fixture("workers-ai/missing_instructions.400.json")
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn full_cloudflare_upstream_id_sends_the_short_body_selector() {
    let h = harness_with(
        ALL,
        "[[providers.workers-ai.models]]\nid = \"clef\"\nupstream = \"@cf/cloudflare/clef\"\n",
    )
    .await;
    let r = h.post(&req("workers-ai/clef")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let seen = h.mocks.clef.last();
    assert_eq!(
        seen.path,
        format!("/accounts/{CF_ACCOUNT}/ai/run/@cf/cloudflare/clef")
    );
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["model"], "clef");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_to_workers_ai_passes_provider_redirects_with_location() {
    let h = harness().await;
    let body = br#"{"model":"clef","state":"REDIRECT_ME","questions":{}}"#;
    let r = h
        .post_to("/v1/raw/systemone?provider=workers-ai", body)
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(r.body.as_ref(), b"moved");
    assert_eq!(
        r.headers.get("location").map(|v| v.to_str().unwrap()),
        Some("/elsewhere")
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_routing_reads_only_the_top_level_model() {
    // A question that typed inspection cannot read must not drop the model:
    // the provider gets its own model path and its own error comes back.
    let h = harness().await;
    let body = br#"{"model":"clef","state":"x","questions":{"q":null}}"#;
    let r = h
        .post_to("/v1/raw/systemone?provider=workers-ai", body)
        .await;
    assert_eq!(
        h.mocks.clef.last().path,
        format!("/accounts/{CF_ACCOUNT}/ai/run/@cf/cloudflare/clef")
    );
    assert_eq!(h.mocks.clef.last().body.as_ref(), body.as_slice());
    assert_envelope(
        &r,
        StatusCode::BAD_REQUEST,
        "workers-ai/missing_instructions.400.json",
    );
    assert_eq!(r.headers["s1-model"], "workers-ai/clef");
    h.server.shutdown().await;
}
