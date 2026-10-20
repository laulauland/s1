//! Memory bound of MCP list calls: item request bodies are built when their
//! item starts, so a large template is not held once per item. This is its
//! own test binary (one test) so that no other test adds to the peak.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{CF_ACCOUNT, CF_TOKEN, Mocks, TEST_KEY};
use s1::catalog::Catalog;
use s1::cli::client::request;
use s1::daemon::{AppState, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use serde_json::{Value, json};

/// Counts live heap bytes and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call goes to `System` unchanged; only counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
            grow(new);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const MIB: usize = 1 << 20;
/// Jev takes up to 262,144 tokens (about 1 MiB of text) per request.
const TEMPLATE_BYTES: usize = 900 << 10;
const ITEMS: usize = 128;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_template_is_not_held_once_per_item() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        f.set_api_key("workers-ai", &Secret::new(CF_TOKEN), Some(CF_ACCOUNT));
        f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let mocks = Mocks::start().await;
    mocks.jev.drop_bodies();
    // No TPM wait; 8 items in flight, so about 8 bodies are live at once.
    let overlay = format!(
        "{}[limits.typesafe]\ntpm = 1000000000\nmax_in_flight = 8\n",
        mocks.overlay()
    );
    let state = AppState::new(Catalog::with_overlay_str(&overlay).unwrap(), keys);
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state,
        token,
    })
    .await
    .unwrap();

    let template = json!({
        "model": "typesafe/jev-latest",
        "questions": {
            "urgent": {"type": "noul", "instructions": "u".repeat(TEMPLATE_BYTES)},
        },
    });
    let states: Vec<Value> = (0..ITEMS).map(|i| json!({"id": i, "state": "x"})).collect();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "decide",
            "arguments": {"template": template, "states": states, "timeout_secs": 600},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "s1-test", "version": "0"},
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    });
    let body = Bytes::from(serde_json::to_vec(&body).unwrap());
    drop(template);
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h.insert(
        "accept",
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    h.insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2026-07-28"),
    );
    h.insert("mcp-method", HeaderValue::from_static("tools/call"));
    h.insert("mcp-name", HeaderValue::from_static("decide"));

    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let reply = request(&paths.socket, Method::POST, "/mcp", h, body)
        .await
        .unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);

    assert_eq!(reply.status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    let res = &v["result"];
    assert_eq!(res["isError"], false, "{}", res["content"]);
    let results = res["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len(), ITEMS);
    assert!(results.iter().all(|r| r["ok"] == true), "{:?}", results[0]);
    let seen = mocks.jev.all();
    assert_eq!(seen.len(), ITEMS);
    assert!(seen.iter().all(|s| s.body_len > TEMPLATE_BYTES));

    // All bodies at once are ITEMS x 900 KiB = 112 MiB (the eager code peaked
    // at about 245 MiB here). Built per item, only the in-flight window of
    // bodies and their copies on the way to the mock are live (about 25 MiB).
    assert!(
        peak < 64 * MIB,
        "peak live bytes during the call: {} MiB",
        peak / MIB
    );
    server.shutdown().await;
}
