//! `POST /mcp`: MCP 2026-07-28 over Streamable HTTP, stateless.
//!
//! A small hand-written handler (not `rmcp`): s1 serves three methods
//! (`server/discover`, `tools/list`, `tools/call`), and its list tools run
//! each item through the typed ingress handler in-process.
//!
//! - No sessions and no `initialize`: each request carries its protocol
//!   version and client capabilities in `params._meta`, and the
//!   `MCP-Protocol-Version`, `Mcp-Method`, and (for `tools/call`)
//!   `Mcp-Name` headers must match the body (`-32020` otherwise).
//! - A request with `_meta.progressToken` may get a `text/event-stream`
//!   reply: `notifications/progress` events, then the result. A call that
//!   ends before its first progress event gets plain JSON.
//! - Closing the response stream cancels the call.
//! - The TCP listener needs the local bearer token (see `daemon::auth`).
//!
//! Tools live in [`tools`]; adding one is one line in [`tools::all`].

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::daemon::AppState;
use crate::images::Origin;

pub mod decide;
pub mod extract;
pub mod forecast;
pub mod jsonrpc;
pub mod list;
pub mod models;
pub mod tools;

use jsonrpc::{Incoming, RpcError};
use tools::{CallCtx, Progress, ToolOutput};

/// The protocol version s1 speaks.
pub const PROTOCOL_VERSION: &str = "2026-07-28";
/// Versions s1 accepts (no 2025-11-25 fallback in v1).
pub const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION];
/// How long clients may reuse `server/discover`.
const DISCOVER_TTL_MS: u64 = 3_600_000;
/// Largest MCP request body (a list of up to 1,000 states).
const MAX_BODY: usize = 4 * crate::daemon::raw::MAX_BODY;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mcp", post(handle).get(not_allowed).delete(not_allowed))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// No GET stream and no session to delete in 2026-07-28.
async fn not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "s1 serves MCP 2026-07-28 statelessly: POST only",
    )
        .into_response()
}

/// What the tool task sends to the response.
pub(crate) enum Event {
    Notify(Value),
    Done(ToolOutput),
}

async fn handle(
    State(st): State<AppState>,
    origin: Option<Extension<Origin>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = check_origin(&headers) {
        return error_reply(&Value::Null, &e);
    }
    let (id, method, params) = match jsonrpc::parse(&body) {
        Ok(Incoming::Request { id, method, params }) => (id, method, params),
        // The core protocol has no client notifications on HTTP; accept and drop.
        Ok(Incoming::Notification { .. }) => return StatusCode::ACCEPTED.into_response(),
        Err(rejected) => {
            let (id, e) = *rejected;
            return error_reply(&id, &e);
        }
    };
    let meta = match check_request(&headers, &method, &params) {
        Ok(m) => m,
        Err(e) => return error_reply(&id, &e),
    };
    match method.as_str() {
        "server/discover" => json_reply(&id, with_server_info(discover())),
        "tools/list" => {
            if params.get("cursor").is_some_and(|c| !c.is_null()) {
                let e = RpcError::new(
                    StatusCode::OK,
                    jsonrpc::INVALID_PARAMS,
                    "unknown cursor: the tool list has one page",
                );
                return error_reply(&id, &e);
            }
            json_reply(&id, with_server_info(tools::list_result(&st.catalog)))
        }
        "tools/call" => {
            let origin = origin.map_or(Origin::Tcp, |Extension(o)| o);
            call_tool(st, origin, id, params, meta).await
        }
        other => error_reply(
            &id,
            &RpcError::new(
                StatusCode::NOT_FOUND,
                jsonrpc::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ),
        ),
    }
}

/// What s1 reads from `params._meta`.
struct Meta {
    progress_token: Option<Value>,
}

/// Header and `_meta` rules of MCP 2026-07-28 (spec "Statelessness",
/// "Streamable HTTP"), in this order: version headers, required `_meta`
/// fields, header/body agreement, supported version.
fn check_request(
    headers: &HeaderMap,
    method: &str,
    params: &Map<String, Value>,
) -> Result<Meta, RpcError> {
    if method == "initialize" {
        return Err(unsupported_version(
            params
                .get("protocolVersion")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .with_message("s1 serves MCP 2026-07-28 only (stateless, no initialize)"));
    }
    let header_version = header_value(headers, "mcp-protocol-version")
        .ok_or_else(|| RpcError::header("missing MCP-Protocol-Version header"))?;
    let header_method = header_value(headers, "mcp-method")
        .ok_or_else(|| RpcError::header("missing Mcp-Method header"))?;
    if header_method != method {
        return Err(RpcError::header(format!(
            "Header mismatch: Mcp-Method header value '{header_method}' does not match body value '{method}'"
        )));
    }
    let invalid = |msg: &str| RpcError::new(StatusCode::BAD_REQUEST, jsonrpc::INVALID_PARAMS, msg);
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("params._meta is required (MCP 2026-07-28 is stateless)"))?;
    let version = meta
        .get(META_VERSION)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("params._meta needs io.modelcontextprotocol/protocolVersion"))?;
    if !meta.get(META_CAPS).is_some_and(Value::is_object) {
        return Err(invalid(
            "params._meta needs io.modelcontextprotocol/clientCapabilities",
        ));
    }
    if header_version != version {
        return Err(RpcError::header(format!(
            "Header mismatch: MCP-Protocol-Version header value '{header_version}' does not match body value '{version}'"
        )));
    }
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(unsupported_version(json!(version)));
    }
    if method == "tools/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let header_name = header_value(headers, "mcp-name")
            .ok_or_else(|| RpcError::header("missing Mcp-Name header"))?;
        if header_name != name {
            return Err(RpcError::header(format!(
                "Header mismatch: Mcp-Name header value '{header_name}' does not match body value '{name}'"
            )));
        }
    }
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|a| a.contains("text/event-stream"));
    let progress_token = meta
        .get("progressToken")
        .filter(|t| t.is_string() || t.is_i64() || t.is_u64())
        .filter(|_| wants_stream)
        .cloned();
    Ok(Meta { progress_token })
}

fn unsupported_version(requested: Value) -> RpcError {
    RpcError::new(
        StatusCode::BAD_REQUEST,
        jsonrpc::UNSUPPORTED_PROTOCOL_VERSION,
        "Unsupported protocol version",
    )
    .with_data(json!({"supported": SUPPORTED_VERSIONS, "requested": requested}))
}

impl RpcError {
    fn with_message(mut self, msg: &str) -> Self {
        self.message = format!("{}: {msg}", self.message);
        self
    }
}

/// A header value, decoded from the `=?base64?...?=` form if needed.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    Some(decode_header(raw).unwrap_or_else(|| raw.to_string()))
}

/// Decode `=?base64?{b64}?=` (UTF-8). `None` if `v` is not in that form.
pub fn decode_header(v: &str) -> Option<String> {
    let inner = v.strip_prefix("=?base64?")?.strip_suffix("?=")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inner)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// An `Origin` header, when present, must name this machine (DNS rebinding
/// protection; spec "Streamable HTTP").
fn check_origin(headers: &HeaderMap) -> Result<(), RpcError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let o = origin.to_str().unwrap_or("");
    let rest = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
        .unwrap_or("");
    let host = if let Some(v6) = rest.strip_prefix('[') {
        v6.split(']').next().map(|h| format!("[{h}]"))
    } else {
        rest.split([':', '/']).next().map(str::to_string)
    };
    match host.as_deref() {
        Some("127.0.0.1" | "localhost" | "[::1]") => Ok(()),
        _ => Err(RpcError::new(
            StatusCode::FORBIDDEN,
            jsonrpc::INVALID_REQUEST,
            format!("Origin `{o}` is not allowed"),
        )),
    }
}

fn discover() -> Value {
    json!({
        "resultType": "complete",
        "supportedVersions": SUPPORTED_VERSIONS,
        "capabilities": {"tools": {}},
        "instructions": "s1 is a local gateway for System One models. `decide` and `extract` take \
                         a request template plus a list of states (1 to 1,000) and answer each \
                         one; `models` lists the models and aliases. Lists are meant for use \
                         from code.",
        "ttlMs": DISCOVER_TTL_MS,
        "cacheScope": "public",
    })
}

fn with_server_info(mut result: Value) -> Value {
    let info = json!({"name": "s1", "version": env!("CARGO_PKG_VERSION")});
    match result.get_mut("_meta").and_then(Value::as_object_mut) {
        Some(m) => {
            m.insert(META_SERVER.into(), info);
        }
        None => result["_meta"] = json!({ META_SERVER: info }),
    }
    result
}

async fn call_tool(
    st: AppState,
    origin: Origin,
    id: Value,
    params: Map<String, Value>,
    meta: Meta,
) -> Response {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(tool) = tools::find(name) else {
        let e = RpcError::new(
            StatusCode::OK,
            jsonrpc::INVALID_PARAMS,
            format!("Unknown tool: {name}"),
        );
        return error_reply(&id, &e);
    };
    // Omitted arguments are `{}`; anything else must be an object.
    let args = match params.get("arguments") {
        None => json!({}),
        Some(a @ Value::Object(_)) => a.clone(),
        Some(_) => {
            let e = RpcError::new(
                StatusCode::OK,
                jsonrpc::INVALID_PARAMS,
                "`arguments` must be an object",
            );
            return error_reply(&id, &e);
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let progress = meta.progress_token.map(|token| Progress {
        token,
        tx: tx.clone(),
    });
    let ctx = CallCtx {
        state: st,
        origin,
        progress,
    };
    let task = tokio::spawn(async move {
        let out = tool.call(ctx, args).await;
        let _ = tx.send(Event::Done(out));
    });
    // Dropping the response (the client went away) aborts the call.
    let abort = AbortOnDrop(task.abort_handle());
    match rx.recv().await {
        Some(Event::Done(out)) => json_reply(&id, with_server_info(out.to_result())),
        Some(Event::Notify(first)) => {
            let body = SseBody {
                first: Some(sse_frame(&first)),
                rx,
                id,
                done: false,
                _abort: abort,
            };
            let mut r = Response::new(Body::new(body));
            let h = r.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            r
        }
        None => error_reply(&id, &internal("the tool call failed")),
    }
}

fn internal(msg: &str) -> RpcError {
    RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, -32603, msg)
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One SSE event with a JSON-RPC message.
fn sse_frame(msg: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {msg}\n\n"))
}

/// The `text/event-stream` reply: progress notifications, then the result.
struct SseBody {
    first: Option<Bytes>,
    rx: mpsc::UnboundedReceiver<Event>,
    id: Value,
    done: bool,
    _abort: AbortOnDrop,
}

impl http_body::Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        let this = &mut *self;
        if let Some(b) = this.first.take() {
            return Poll::Ready(Some(Ok(http_body::Frame::data(b))));
        }
        if this.done {
            return Poll::Ready(None);
        }
        let msg = match this.rx.poll_recv(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(Event::Notify(v))) => v,
            Poll::Ready(Some(Event::Done(out))) => {
                this.done = true;
                jsonrpc::result(&this.id, with_server_info(out.to_result()))
            }
            Poll::Ready(None) => {
                this.done = true;
                internal("the tool call failed").to_message(&this.id)
            }
        };
        Poll::Ready(Some(Ok(http_body::Frame::data(sse_frame(&msg)))))
    }
}

fn json_reply(id: &Value, result: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        jsonrpc::result(id, result).to_string(),
    )
        .into_response()
}

fn error_reply(id: &Value, e: &RpcError) -> Response {
    (
        e.http,
        [(header::CONTENT_TYPE, "application/json")],
        e.to_message(id).to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_header_values() {
        let enc = base64::engine::general_purpose::STANDARD.encode("dé cide");
        assert_eq!(
            decode_header(&format!("=?base64?{enc}?=")).as_deref(),
            Some("dé cide")
        );
        assert_eq!(decode_header("decide"), None);
        assert_eq!(decode_header("=?base64?***?="), None);
    }

    #[test]
    fn origin_must_be_local() {
        let mut h = HeaderMap::new();
        assert!(check_origin(&h).is_ok());
        for ok in [
            "http://127.0.0.1:7777",
            "http://localhost",
            "https://localhost:1/x",
            "http://[::1]:8080",
        ] {
            h.insert(header::ORIGIN, HeaderValue::from_static(ok));
            assert!(check_origin(&h).is_ok(), "{ok}");
        }
        for bad in [
            "http://evil.example",
            "http://127.0.0.1.evil.example",
            "null",
            "http://localhost.evil:80",
        ] {
            h.insert(header::ORIGIN, HeaderValue::from_static(bad));
            let e = check_origin(&h).unwrap_err();
            assert_eq!(e.http, StatusCode::FORBIDDEN, "{bad}");
        }
    }
}
