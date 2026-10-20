//! Stage 3 protection through the daemon: retries, `Retry-After`, AIMD,
//! the circuit breaker, alias spill, bounded queues with lanes, rate
//! limits, `/status`, live state in `/v1/models`, and the response cache.
//!
//! The provider is a scriptable raw-TCP mock, so a step can also drop the
//! connection without an answer.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{TEST_KEY, fake_response};
use s1::cache::{Cache, Versions};
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::queue::Bounds;
use s1::secrets::{KeyResolver, Secret};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------- mock

#[derive(Clone, Debug)]
enum Step {
    /// Answer with this status (and headers) after a delay.
    Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        delay_ms: u64,
    },
    /// Read the request, then close the connection without an answer.
    Drop,
}

fn status(code: u16) -> Step {
    Step::Reply {
        status: code,
        headers: vec![],
        delay_ms: 0,
    }
}

fn status_with(code: u16, name: &'static str, value: &str) -> Step {
    Step::Reply {
        status: code,
        headers: vec![(name, value.to_string())],
        delay_ms: 0,
    }
}

#[derive(Default)]
struct MockState {
    script: VecDeque<Step>,
    /// `state` field of every request, in arrival order.
    seen: Vec<String>,
    /// `authorization` header of every request, in arrival order.
    auths: Vec<String>,
    /// Request body bytes as received, in arrival order.
    bodies: Vec<Vec<u8>>,
    /// Model version in successful answers.
    version: String,
    /// Delay of default (scripted-out) answers.
    delay_ms: u64,
    in_flight: usize,
    max_in_flight: usize,
}

#[derive(Clone)]
struct Mock {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl Mock {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(MockState {
            version: "jev-1.13.0".into(),
            ..Default::default()
        }));
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_conn(sock, st.clone()));
            }
        });
        Self { addr, state }
    }

    fn script(&self, steps: impl IntoIterator<Item = Step>) {
        self.state.lock().unwrap().script.extend(steps);
    }
    fn calls(&self) -> usize {
        self.state.lock().unwrap().seen.len()
    }
    fn seen(&self) -> Vec<String> {
        self.state.lock().unwrap().seen.clone()
    }
    fn auths(&self) -> Vec<String> {
        self.state.lock().unwrap().auths.clone()
    }
    fn bodies(&self) -> Vec<Vec<u8>> {
        self.state.lock().unwrap().bodies.clone()
    }
    fn set_delay(&self, ms: u64) {
        self.state.lock().unwrap().delay_ms = ms;
    }
    fn set_version(&self, v: &str) {
        self.state.lock().unwrap().version = v.into();
    }
    fn max_in_flight(&self) -> usize {
        self.state.lock().unwrap().max_in_flight
    }
}

/// The request's `authorization` header and body.
async fn read_request(sock: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let auth = String::from_utf8_lossy(&buf[..head_end])
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("authorization")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default();
    Some((auth, buf[head_end..head_end + len].to_vec()))
}

async fn serve_conn(mut sock: TcpStream, st: Arc<Mutex<MockState>>) {
    let Some((auth, body)) = read_request(&mut sock).await else {
        return;
    };
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (step, version, default_delay) = {
        let mut s = st.lock().unwrap();
        s.seen.push(req["state"].as_str().unwrap_or("").to_string());
        s.auths.push(auth);
        s.bodies.push(body.clone());
        s.in_flight += 1;
        s.max_in_flight = s.max_in_flight.max(s.in_flight);
        (s.script.pop_front(), s.version.clone(), s.delay_ms)
    };
    let step = step.unwrap_or(Step::Reply {
        status: 200,
        headers: vec![],
        delay_ms: default_delay,
    });
    match step {
        Step::Drop => {
            st.lock().unwrap().in_flight -= 1;
            drop(sock);
        }
        Step::Reply {
            status,
            headers,
            delay_ms,
        } => {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let body = if status == 200 {
                serde_json::to_vec(&fake_response(&req, &version)).unwrap()
            } else {
                format!(r#"{{"detail":"mock status {status}"}}"#).into_bytes()
            };
            let mut out = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            let mut bytes = out.into_bytes();
            bytes.extend_from_slice(&body);
            st.lock().unwrap().in_flight -= 1;
            let _ = sock.write_all(&bytes).await;
            let _ = sock.shutdown().await;
        }
    }
}

// ------------------------------------------------------------- harness

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    keys: KeyResolver,
    server: RunningServer,
    ts: Mock,
    ts2: Mock,
}

/// Fast retries for tests; each test adds its own overlay lines.
const BASE_LIMITS: &str = "
[limits.typesafe]
retry_base_ms = 5
retry_max_ms = 20
[limits.ts2]
retry_base_ms = 5
retry_max_ms = 20
";

#[derive(Default)]
struct Opts {
    overlay: &'static str,
    bounds: Bounds,
    cache: bool,
}

async fn harness(opts: Opts) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
        f.set_api_key("ts2", &Secret::new(TEST_KEY), None);
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let ts = Mock::start().await;
    let ts2 = Mock::start().await;
    let base = format!(
        "[providers.typesafe]\nbase_url = \"http://{}\"\n\
         [providers.ts2]\nbase_url = \"http://{}\"\napi = \"systemone\"\n\
         [[providers.ts2.models]]\nid = \"jev-latest\"\ncontext_tokens = 65536\n\
         {BASE_LIMITS}",
        ts.addr, ts2.addr
    );
    // The test's own lines go over the base (same tables merge).
    let mut merged: toml::Value = toml::from_str(&base).unwrap();
    s1::catalog::merge(&mut merged, toml::from_str(opts.overlay).unwrap());
    let overlay = toml::to_string(&merged).unwrap();
    let catalog = Catalog::with_overlay_str(&overlay).unwrap();
    let mut state = AppState::new(catalog, keys.clone())
        .with_bounds(opts.bounds)
        .with_versions(Versions::load(&paths.versions_file()));
    if opts.cache {
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
        keys,
        server,
        ts,
        ts2,
    }
}

fn body(model: &str, state: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "state": state,
        "questions": {
            "billing": {"type": "noul", "instructions": "Is this about billing?"},
            "tone": {"type": "choice", "instructions": "Tone?", "criteria": {"angry": "Upset", "calm": "Neutral"}}
        }
    }))
    .unwrap()
}

impl Harness {
    async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        for (k, v) in headers {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        request(
            &self.paths.socket,
            method,
            path,
            h,
            Bytes::copy_from_slice(body),
        )
        .await
        .unwrap()
    }
    async fn post(&self, body: &[u8]) -> Reply {
        self.send(Method::POST, "/v1/systemone", &[], body).await
    }
    async fn post_h(&self, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        self.send(Method::POST, "/v1/systemone", headers, body)
            .await
    }
    async fn get_json(&self, path: &str) -> Value {
        let r = self.send(Method::GET, path, &[], b"").await;
        assert_eq!(r.status, StatusCode::OK);
        serde_json::from_slice(&r.body).unwrap()
    }
    async fn backend_status(&self, provider: &str) -> Value {
        let s = self.get_json("/status").await;
        s["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["provider"] == provider)
            .cloned()
            .unwrap_or_else(|| panic!("no {provider} in {s}"))
    }
    async fn model_state(&self, id: &str) -> Value {
        let m = self.get_json("/v1/models").await;
        m["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap()
    }
}

fn header<'a>(r: &'a Reply, name: &str) -> &'a str {
    r.headers
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or_else(|| panic!("no {name} header"))
}

fn text(r: &Reply) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

/// Wait until `f` holds (at most 5 s).
async fn until(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "condition not reached");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// --------------------------------------------------------------- retries

#[tokio::test]
async fn retries_5xx_and_dropped_connections_then_succeeds() {
    let h = harness(Opts::default()).await;
    h.ts.script([status(500), Step::Drop, status(502)]);
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-retries"), "3");
    assert_eq!(h.ts.calls(), 4);
    h.server.shutdown().await;
}

#[tokio::test]
async fn at_most_four_attempts_and_the_last_answer_is_returned() {
    let h = harness(Opts::default()).await;
    h.ts.script(std::iter::repeat_n(status(503), 10));
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        text(&r).contains("mock status 503"),
        "provider body unchanged"
    );
    assert_eq!(h.ts.calls(), 4);
    assert_eq!(header(&r, "s1-retries"), "3");
    h.server.shutdown().await;
}

#[tokio::test]
async fn dropped_connections_every_time_give_backend_unavailable() {
    let h = harness(Opts::default()).await;
    h.ts.script(std::iter::repeat_n(Step::Drop, 4));
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", text(&r));
    assert!(text(&r).contains("backend_unavailable"), "{}", text(&r));
    assert_eq!(h.ts.calls(), 4);
    h.server.shutdown().await;
}

#[tokio::test]
async fn no_retry_on_4xx() {
    let h = harness(Opts::default()).await;
    h.ts.script([status(400)]);
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.ts.calls(), 1);
    assert_eq!(header(&r, "s1-retries"), "0");
    h.server.shutdown().await;
}

#[tokio::test]
async fn retry_after_is_obeyed_and_429_halves_the_cap() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nmax_in_flight = 8\nsuccess_window_ms = 60000\n",
        ..Default::default()
    })
    .await;
    h.ts.script([status_with(429, "retry-after", "1")]);
    let t0 = Instant::now();
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert!(
        t0.elapsed() >= Duration::from_millis(950),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(header(&r, "s1-retries"), "1");
    assert_eq!(header(&r, "s1-capacity"), "4", "8 halved on 429");
    let st = h.backend_status("typesafe").await;
    assert_eq!(st["cap"], 4);
    assert_eq!(st["limits"]["max_in_flight"], 8);
    h.server.shutdown().await;
}

#[tokio::test]
async fn long_retry_after_returns_the_429_and_marks_the_backend_throttled() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nretry_after_max_ms = 1000\n",
        ..Default::default()
    })
    .await;
    h.ts.script([status_with(429, "retry-after", "120")]);
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        header(&r, "retry-after"),
        "120",
        "provider header passes through"
    );
    assert_eq!(h.ts.calls(), 1);
    let m = h.model_state("typesafe/jev-latest").await;
    assert!(m["state"]["throttled_until"].is_string(), "{m}");
    let st = h.backend_status("typesafe").await;
    assert!(st["throttled_until"].is_string(), "{st}");
    h.server.shutdown().await;
}

// --------------------------------------------------------------- breaker

#[tokio::test]
async fn breaker_opens_after_five_failures_and_shows_in_models_and_status() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nretry_attempts = 1\nbreaker_cooldown_ms = 60000\n",
        ..Default::default()
    })
    .await;
    h.ts.script(std::iter::repeat_n(status(500), 10));
    for _ in 0..5 {
        let r = h.post(&body("typesafe/jev-latest", "x")).await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    }
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", text(&r));
    assert!(text(&r).contains("backend_unavailable"));
    let ra: u64 = header(&r, "retry-after").parse().unwrap();
    assert!((1..=60).contains(&ra), "{ra}");
    assert_eq!(h.ts.calls(), 5, "an open breaker sends nothing");

    let m = h.model_state("typesafe/jev-latest").await;
    assert_eq!(m["state"]["breaker_open"], true, "{m}");
    assert_eq!(m["state"]["available"], false, "{m}");
    let st = h.backend_status("typesafe").await;
    assert_eq!(st["breaker"]["state"], "open", "{st}");
    assert!(st["breaker"]["open_until"].is_string());
    assert!(st["key"].as_str().unwrap().starts_with("k_"));
    assert!(
        !st.to_string().contains(TEST_KEY),
        "no key values in /status"
    );
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_skips_an_entry_with_an_open_breaker() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nretry_attempts = 1\nbreaker_failures = 2\nbreaker_cooldown_ms = 60000\n\
                  [aliases.pair]\nmodels = [\"typesafe/jev-latest\", \"ts2/jev-latest\"]\n",
        ..Default::default()
    })
    .await;
    h.ts.script([status(500), status(500)]);
    for _ in 0..2 {
        assert_eq!(
            h.post(&body("pair", "x")).await.status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let r = h.post(&body("pair", "x")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-backend"), "ts2");
    h.server.shutdown().await;
}

#[tokio::test]
async fn alias_with_every_breaker_open_gives_503_with_retry_after() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nretry_attempts = 1\nbreaker_failures = 2\nbreaker_cooldown_ms = 60000\n\
                  [aliases.solo]\nmodels = [\"typesafe/jev-latest\"]\n",
        ..Default::default()
    })
    .await;
    h.ts.script([status(500), status(500)]);
    for _ in 0..2 {
        assert_eq!(
            h.post(&body("solo", "x")).await.status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let r = h.post(&body("solo", "x")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", text(&r));
    assert!(text(&r).contains("backend_unavailable"), "{}", text(&r));
    let ra: u64 = header(&r, "retry-after").parse().unwrap();
    assert!((1..=60).contains(&ra), "{ra}");
    assert_eq!(h.ts.calls(), 2, "an open breaker sends nothing");
    h.server.shutdown().await;
}

// ----------------------------------------------------------------- spill

#[tokio::test]
async fn spill_true_uses_the_next_entry_while_the_first_is_throttled() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nretry_after_max_ms = 100\n\
                  [aliases.spilly]\nmodels = [\"typesafe/jev-latest\", \"ts2/jev-latest\"]\nspill = true\n\
                  [aliases.steady]\nmodels = [\"typesafe/jev-latest\", \"ts2/jev-latest\"]\n",
        ..Default::default()
    })
    .await;
    // Pause typesafe for 2 s.
    h.ts.script([status_with(429, "retry-after", "2")]);
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);

    let r = h.post(&body("spilly", "a")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-backend"), "ts2", "spill = true goes on");

    let t0 = Instant::now();
    let r = h.post(&body("steady", "b")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-backend"), "typesafe", "spill = false waits");
    assert!(
        t0.elapsed() >= Duration::from_millis(1200),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(h.ts2.calls(), 1);
    h.server.shutdown().await;
}

#[tokio::test]
async fn an_empty_rpm_bucket_shows_as_throttled_in_models_and_status() {
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nrpm = 1\n\
                  [aliases.spilly]\nmodels = [\"typesafe/jev-latest\", \"ts2/jev-latest\"]\nspill = true\n",
        ..Default::default()
    })
    .await;
    let r = h.post(&body("typesafe/jev-latest", "x")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    // The only request of this minute is spent: both views say throttled.
    let m = h.model_state("typesafe/jev-latest").await;
    assert!(m["state"]["throttled_until"].is_string(), "{m}");
    let st = h.backend_status("typesafe").await;
    assert!(st["throttled_until"].is_string(), "{st}");
    // And the router agrees: spill goes to the next entry.
    let r = h.post(&body("spilly", "a")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-backend"), "ts2");
    h.server.shutdown().await;
}

// ----------------------------------------------------- queues and lanes

#[tokio::test]
async fn full_queue_gives_429_queue_full_with_retry_after() {
    let h = Arc::new(
        harness(Opts {
            overlay: "[limits.typesafe]\nmax_in_flight = 1\n",
            bounds: Bounds {
                max_requests: 1,
                max_tokens: 1_000_000,
            },
            ..Default::default()
        })
        .await,
    );
    h.ts.set_delay(400);
    let a = tokio::spawn({
        let h = h.clone();
        async move { h.post(&body("typesafe/jev-latest", "a")).await.status }
    });
    until(|| h.ts.calls() == 1).await;
    let b = tokio::spawn({
        let h = h.clone();
        async move { h.post(&body("typesafe/jev-latest", "b")).await.status }
    });
    // Wait until b is queued.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let st = h.backend_status("typesafe").await;
        if st["queue"]["interactive"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let r = h.post(&body("typesafe/jev-latest", "c")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", text(&r));
    assert!(text(&r).contains("queue_full"), "{}", text(&r));
    let ra: u64 = header(&r, "retry-after").parse().unwrap();
    assert!(ra >= 1, "{ra}");
    assert_eq!(a.await.unwrap(), StatusCode::OK);
    assert_eq!(b.await.unwrap(), StatusCode::OK);
    assert_eq!(
        h.ts.calls(),
        2,
        "the rejected request never reached the provider"
    );
    assert_eq!(h.ts.max_in_flight(), 1);
}

#[tokio::test]
async fn interactive_requests_go_before_queued_bulk_requests() {
    let h = Arc::new(
        harness(Opts {
            overlay: "[limits.typesafe]\nmax_in_flight = 1\n",
            ..Default::default()
        })
        .await,
    );
    h.ts.set_delay(150);
    let mut tasks = Vec::new();
    let spawn = |state: &'static str, lane: &'static str| {
        let h = h.clone();
        tokio::spawn(async move {
            h.post_h(&[("s1-lane", lane)], &body("typesafe/jev-latest", state))
                .await
                .status
        })
    };
    tasks.push(spawn("first", "bulk"));
    until(|| h.ts.calls() == 1).await;
    for s in ["bulk-1", "bulk-2"] {
        tasks.push(spawn(s, "bulk"));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tasks.push(spawn("interactive", "interactive"));
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(h.ts.seen(), ["first", "interactive", "bulk-1", "bulk-2"]);
}

#[tokio::test]
async fn a_rotated_key_does_not_change_requests_already_queued() {
    // A request queues under the old key's scheduler; the key changes while
    // it waits. It must go out with the key its scheduler belongs to, so
    // the new key's limits only count requests made under it.
    let h = Arc::new(
        harness(Opts {
            overlay: "[limits.typesafe]\nmax_in_flight = 1\n",
            ..Default::default()
        })
        .await,
    );
    h.ts.set_delay(200);
    let spawn = |state: &'static str| {
        let h = h.clone();
        tokio::spawn(async move { h.post(&body("typesafe/jev-latest", state)).await.status })
    };
    let first = spawn("first");
    until(|| h.ts.calls() == 1).await;
    let queued = spawn("queued");
    // Wait until `queued` is in the queue (the first request holds the
    // only slot).
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.backend_status("typesafe").await["queue"]["interactive"] != 1 {
        assert!(
            Instant::now() < deadline,
            "`queued` never reached the queue"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.ts.calls(), 1);
    h.keys
        .update(|f| f.set_api_key("typesafe", &Secret::new("sk-rotated"), None))
        .unwrap();
    let after = spawn("after");
    for t in [first, queued, after] {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    let seen: Vec<(String, String)> = h.ts.seen().into_iter().zip(h.ts.auths()).collect();
    let auth_of = |state: &str| {
        seen.iter()
            .find(|(s, _)| s == state)
            .map(|(_, a)| a.clone())
            .unwrap()
    };
    let old = format!("Bearer {TEST_KEY}");
    assert_eq!(auth_of("first"), old);
    assert_eq!(auth_of("queued"), old, "queued under the old key");
    assert_eq!(auth_of("after"), "Bearer sk-rotated");
}

#[tokio::test]
async fn in_flight_cap_holds_under_a_burst() {
    let h = Arc::new(
        harness(Opts {
            overlay: "[limits.typesafe]\nmax_in_flight = 3\n",
            ..Default::default()
        })
        .await,
    );
    h.ts.set_delay(50);
    let tasks: Vec<_> = (0..12)
        .map(|i| {
            let h = h.clone();
            tokio::spawn(async move {
                h.post(&body("typesafe/jev-latest", &format!("n{i}")))
                    .await
                    .status
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(h.ts.calls(), 12);
    assert!(h.ts.max_in_flight() <= 3, "{}", h.ts.max_in_flight());
}

#[tokio::test]
async fn rpm_limit_spaces_requests_out() {
    // rpm 120: a burst of 12, then about 1.8 requests per second.
    let h = harness(Opts {
        overlay: "[limits.typesafe]\nrpm = 120\n",
        ..Default::default()
    })
    .await;
    let t0 = Instant::now();
    for i in 0..14 {
        let r = h.post(&body("typesafe/jev-latest", &format!("n{i}"))).await;
        assert_eq!(r.status, StatusCode::OK);
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    let st = h.backend_status("typesafe").await;
    assert_eq!(st["rate"]["requests_last_minute"], 14);
    assert_eq!(st["limits"]["rpm"], 120);
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_passthrough_is_guarded_too() {
    let h = harness(Opts::default()).await;
    h.ts.script([status(503)]);
    let r = h
        .send(
            Method::POST,
            "/v1/raw/systemone",
            &[],
            &body("jev-latest", "x"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    assert_eq!(header(&r, "s1-retries"), "1");
    assert!(header(&r, "s1-capacity").parse::<u32>().unwrap() >= 1);
    h.server.shutdown().await;
}

/// Tokens the `typesafe` limiter counted in the last minute.
async fn tokens_counted(h: &Harness) -> u64 {
    h.backend_status("typesafe").await["rate"]["tokens_last_minute"]
        .as_u64()
        .unwrap()
}

fn gzip(plain: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(plain).unwrap();
    gz.finish().unwrap()
}

#[tokio::test]
async fn raw_compressed_bodies_cost_their_decoded_size() {
    // A compressed raw body costs what its decoded body costs, so
    // compression cannot get around tpm. The provider still gets the
    // original bytes and Content-Encoding.
    let h = harness(Opts::default()).await;
    let plain = body("jev-latest", &"a".repeat(60_000));
    let z = zstd::encode_all(&plain[..], 3).unwrap();
    let gz = gzip(&plain);
    assert!(z.len() * 10 < plain.len() && gz.len() * 10 < plain.len());

    let mut costs = Vec::new();
    for (enc, bytes) in [(None, &plain), (Some("zstd"), &z), (Some("gzip"), &gz)] {
        let before = tokens_counted(&h).await;
        let headers: Vec<(&str, &str)> = enc.map(|e| ("content-encoding", e)).into_iter().collect();
        let r = h
            .send(Method::POST, "/v1/raw/systemone", &headers, bytes)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{enc:?}: {}", text(&r));
        assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest", "{enc:?}");
        costs.push(tokens_counted(&h).await - before);
    }
    assert!(costs[0] >= 15_000, "{costs:?}");
    assert_eq!(costs, [costs[0]; 3], "plain, zstd, gzip");
    let bodies = h.ts.bodies();
    assert_eq!(bodies[0], plain);
    assert_eq!(bodies[1], z, "zstd forwarded unchanged");
    assert_eq!(bodies[2], gz, "gzip forwarded unchanged");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_compressed_bodies_are_throttled_like_plain_ones() {
    // tpm 360000: burst 36000, refill about 6000 a second. Each body is
    // about 20000 tokens decoded, so the second one waits about 0.7 s for
    // the bucket; costed by compressed size it would not wait at all.
    for enc in [None, Some("zstd"), Some("gzip")] {
        let h = harness(Opts {
            overlay: "[limits.typesafe]\ntpm = 360000\n",
            ..Default::default()
        })
        .await;
        let t0 = Instant::now();
        for i in 0..2 {
            let plain = body("jev-latest", &format!("{i}{}", "a".repeat(60_000)));
            let bytes = match enc {
                None => plain,
                Some("zstd") => zstd::encode_all(&plain[..], 3).unwrap(),
                Some(_) => gzip(&plain),
            };
            let headers: Vec<(&str, &str)> =
                enc.map(|e| ("content-encoding", e)).into_iter().collect();
            let r = h
                .send(Method::POST, "/v1/raw/systemone", &headers, &bytes)
                .await;
            assert_eq!(r.status, StatusCode::OK, "{enc:?}: {}", text(&r));
        }
        assert!(
            t0.elapsed() >= Duration::from_millis(400),
            "{enc:?}: {:?}",
            t0.elapsed()
        );
        h.server.shutdown().await;
    }
}

#[tokio::test]
async fn raw_compressed_body_over_the_cap_is_too_long() {
    let h = harness(Opts::default()).await;
    let bomb = zstd::encode_all(&vec![b' '; 64 * 1024 * 1024 + 1][..], 19).unwrap();
    let r = h
        .send(
            Method::POST,
            "/v1/raw/systemone",
            &[("content-encoding", "zstd")],
            &bomb,
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", text(&r));
    assert_eq!(h.ts.calls(), 0);
    h.server.shutdown().await;
}

#[tokio::test]
async fn status_lists_idle_backends_with_keys() {
    let h = harness(Opts::default()).await;
    let s = h.get_json("/status").await;
    let providers: Vec<&str> = s["backends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["provider"].as_str().unwrap())
        .collect();
    assert!(providers.contains(&"typesafe"), "{s}");
    assert!(providers.contains(&"ts2"), "{s}");
    assert!(
        !providers.contains(&"workers-ai"),
        "no key, not listed: {s}"
    );
    let ts = h.backend_status("typesafe").await;
    assert_eq!(ts["queue"]["interactive"], 0);
    assert_eq!(ts["queue"]["bulk"], 0);
    assert_eq!(ts["breaker"]["state"], "closed");
    assert!(ts["throttled_until"].is_null());
    assert!(s["cache"].is_null(), "no cache in this harness");
    h.server.shutdown().await;
}

// ----------------------------------------------------------------- cache

fn cached_opts() -> Opts {
    Opts {
        cache: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn cache_hits_ignore_key_order_and_whitespace() {
    let h = harness(cached_opts()).await;
    let a = body("typesafe/jev-latest", "charged twice");
    let r1 = h.post(&a).await;
    assert_eq!(r1.status, StatusCode::OK);
    assert_eq!(header(&r1, "s1-cache"), "miss");
    let r2 = h.post(&a).await;
    assert_eq!(header(&r2, "s1-cache"), "hit");
    assert_eq!(r2.body, r1.body, "same bytes");
    assert_eq!(header(&r2, "s1-model"), "typesafe/jev-latest");
    assert_eq!(header(&r2, "content-type"), "application/json");
    // The streaming CLI sizes its window from these, hits included.
    assert_eq!(header(&r2, "s1-retries"), "0");
    assert!(header(&r2, "s1-capacity").parse::<u32>().unwrap() >= 1);
    assert_eq!(h.ts.calls(), 1);

    // Same request: other key order and whitespace.
    let reordered = br#"{
        "questions": {
            "tone": {"criteria": {"calm": "Neutral", "angry": "Upset"}, "instructions": "Tone?", "type": "choice"},
            "billing": {"instructions": "Is this about billing?", "type": "noul"}
        },
        "state":   "charged twice",
        "model": "typesafe/jev-latest"
    }"#;
    let r3 = h.post(reordered).await;
    assert_eq!(header(&r3, "s1-cache"), "hit", "{}", text(&r3));
    assert_eq!(h.ts.calls(), 1);

    // A changed option misses.
    let changed = serde_json::to_vec(&json!({
        "model": "typesafe/jev-latest",
        "state": "charged twice",
        "questions": {
            "billing": {"type": "noul", "instructions": "Is this about billing?"},
            "tone": {"type": "choice", "instructions": "Tone?", "criteria": {"angry": "Upset", "calm": "Relaxed"}}
        }
    }))
    .unwrap();
    assert_eq!(header(&h.post(&changed).await, "s1-cache"), "miss");
    // Another model misses too.
    let other = body("ts2/jev-latest", "charged twice");
    assert_eq!(header(&h.post(&other).await, "s1-cache"), "miss");
    assert_eq!(h.ts.calls(), 2);
    assert_eq!(h.ts2.calls(), 1);

    let s = h.get_json("/status").await;
    // The key holds the resolved version, not the provider: ts2 answered
    // with the same version and body, so it shares the first entry.
    assert_eq!(s["cache"]["entries"], 2, "{s}");
    h.server.shutdown().await;
}

#[tokio::test]
async fn no_cache_refreshes_and_no_store_bypasses() {
    let h = harness(cached_opts()).await;
    let a = body("typesafe/jev-latest", "x");
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "miss");
    let r = h.post_h(&[("cache-control", "no-cache")], &a).await;
    assert_eq!(header(&r, "s1-cache"), "refresh");
    assert_eq!(h.ts.calls(), 2);
    let r = h.post_h(&[("cache-control", "no-store")], &a).await;
    assert_eq!(header(&r, "s1-cache"), "bypass");
    assert_eq!(h.ts.calls(), 3);
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "hit");
    assert_eq!(h.ts.calls(), 3);
    h.server.shutdown().await;
}

#[tokio::test]
async fn deeply_nested_state_with_cache_on_does_not_kill_the_daemon() {
    // Inspection does not read `state`, so deep nesting there first meets
    // the cache key. It must not overflow the stack.
    let h = harness(cached_opts()).await;
    let n = 100_000;
    let deep = format!(
        r#"{{"model":"typesafe/jev-latest","questions":{{"q":{{"type":"noul"}}}},"state":{}{}}}"#,
        "[".repeat(n),
        "]".repeat(n)
    );
    let r = h.post(deep.as_bytes()).await;
    assert!(r.status.as_u16() > 0, "the daemon answered");
    let r = h.post(&body("typesafe/jev-latest", "after")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", text(&r));
    h.get_json("/status").await;
    h.server.shutdown().await;
}

#[tokio::test]
async fn failures_are_not_cached() {
    let h = harness(Opts {
        cache: true,
        overlay: "[limits.typesafe]\nretry_attempts = 1\n",
        ..Default::default()
    })
    .await;
    h.ts.script([status(500)]);
    let a = body("typesafe/jev-latest", "x");
    assert_eq!(h.post(&a).await.status, StatusCode::INTERNAL_SERVER_ERROR);
    let r = h.post(&a).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(header(&r, "s1-cache"), "miss");
    assert_eq!(h.ts.calls(), 2);
    h.server.shutdown().await;
}

#[tokio::test]
async fn a_new_resolved_version_retires_old_entries_and_is_persisted() {
    let h = harness(cached_opts()).await;
    let a = body("typesafe/jev-latest", "x");
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "miss");
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "hit");
    let versions: Value =
        serde_json::from_slice(&std::fs::read(h.paths.versions_file()).unwrap()).unwrap();
    assert_eq!(versions["typesafe/jev-latest"], "jev-1.13.0");

    // The provider updates jev-latest; any fresh call reveals it.
    h.ts.set_version("jev-1.14.0");
    let other = body("typesafe/jev-latest", "something else");
    assert_eq!(header(&h.post(&other).await, "s1-cache"), "miss");
    let versions: Value =
        serde_json::from_slice(&std::fs::read(h.paths.versions_file()).unwrap()).unwrap();
    assert_eq!(versions["typesafe/jev-latest"], "jev-1.14.0");

    // The old answer (made by 1.13.0) no longer matches.
    let r = h.post(&a).await;
    assert_eq!(header(&r, "s1-cache"), "miss");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["model"], "jev-1.14.0");
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "hit");
    assert_eq!(h.ts.calls(), 3);
    h.server.shutdown().await;
}

#[tokio::test]
async fn cache_clear_endpoint_empties_the_store() {
    let h = harness(cached_opts()).await;
    let a = body("typesafe/jev-latest", "x");
    h.post(&a).await;
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "hit");
    let r = h.send(Method::POST, "/cache/clear", &[], b"").await;
    assert_eq!(r.status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["cleared"], 1);
    assert_eq!(header(&h.post(&a).await, "s1-cache"), "miss");
    h.server.shutdown().await;
}

#[tokio::test]
async fn raw_gzip_with_several_members_costs_all_of_them() {
    // A gzip body may hold several members. The cost and the model come
    // from all of them, not from the first member only.
    let h = harness(Opts::default()).await;
    let plain = body("jev-latest", &"a".repeat(60_000));
    let (head, tail) = plain.split_at(31);
    let mut multi = gzip(head);
    multi.extend(gzip(tail));

    let mut costs = Vec::new();
    for (enc, bytes) in [(None, &plain), (Some("gzip"), &multi)] {
        let before = tokens_counted(&h).await;
        let headers: Vec<(&str, &str)> = enc.map(|e| ("content-encoding", e)).into_iter().collect();
        let r = h
            .send(Method::POST, "/v1/raw/systemone", &headers, bytes)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{enc:?}: {}", text(&r));
        assert_eq!(header(&r, "s1-model"), "typesafe/jev-latest", "{enc:?}");
        costs.push(tokens_counted(&h).await - before);
    }
    assert!(costs[0] >= 15_000, "{costs:?}");
    assert_eq!(costs[0], costs[1], "plain, multi-member gzip");
    assert_eq!(h.ts.bodies()[1], multi, "gzip forwarded unchanged");
    h.server.shutdown().await;
}
