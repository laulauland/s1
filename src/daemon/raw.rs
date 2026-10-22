//! `POST /v1/raw/{protocol}[?provider=<name>]`: pure passthrough.
//!
//! The original request bytes go to the provider unchanged, and the provider
//! response comes back unchanged, plus `s1-backend`, `s1-model`, and
//! `s1-request-id` headers. No routing by alias and no cache, but the
//! request goes through the provider's queue, limits, retries, and breaker
//! like any other (lane from `s1-lane`). A `zstd` or `gzip` body is decoded
//! into a copy only to size the limiter cost and read `model`.

use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde::Deserialize;

use super::decode::decode_body;
use super::{AppState, request_id};
use crate::backend::BackendRequest;
use crate::error::S1Error;
use crate::protect::{self, CallStats};
use crate::protocol::{self, ProtocolId};
use crate::queue::Lane;

/// Largest body accepted (TypeSafe allows 32 MiB).
pub const MAX_BODY: usize = 64 * 1024 * 1024;

pub const H_BACKEND: &str = "s1-backend";
pub const H_MODEL: &str = "s1-model";
pub const H_REQUEST_ID: &str = "s1-request-id";
/// The backend's current in-flight cap (the CLI sizes its window to it).
pub const H_CAPACITY: &str = "s1-capacity";
/// Attempts after the first.
pub const H_RETRIES: &str = "s1-retries";

pub fn routes() -> Router<crate::daemon::reload::Shared> {
    Router::new().route("/v1/raw/{protocol}", post(raw))
}

/// The request body, at most [`MAX_BODY`] bytes. Over that is `too_long`,
/// so the caller renders it in the ingress's own error format (axum's own
/// limit rejection is plain text).
pub(super) async fn read_body(body: Body) -> Result<Bytes, S1Error> {
    axum::body::to_bytes(body, MAX_BODY).await.map_err(|e| {
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            if s.is::<http_body_util::LengthLimitError>() {
                return S1Error::TooLong(format!("request body is over {MAX_BODY} bytes"));
            }
            source = s.source();
        }
        S1Error::InvalidRequest(format!("cannot read the request body: {e}"))
    })
}

#[derive(Debug, Deserialize)]
struct RawQuery {
    provider: Option<String>,
}

/// Provider used when the request names none.
pub fn default_provider(protocol: ProtocolId) -> Option<&'static str> {
    match protocol {
        ProtocolId::Systemone => Some("typesafe"),
        ProtocolId::OpenaiDecisions => Some("openai"),
        ProtocolId::Clairic => Some("workers-ai"),
        ProtocolId::Tsfm => None,
    }
}

async fn raw(
    State(st): State<AppState>,
    Path(protocol): Path<String>,
    Query(q): Query<RawQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let rid = request_id();
    let started = Instant::now();
    let Ok(pid) = protocol.parse::<ProtocolId>() else {
        let err = S1Error::Unsupported(format!("unknown protocol `{protocol}`"));
        return error_response(None, &err, &rid, None);
    };
    let proto = protocol::by_id(pid);
    let body = match read_body(body).await {
        Ok(b) => b,
        Err(err) => return error_response(proto, &err, &rid, None),
    };
    let provider = match q.provider.as_deref().or(default_provider(pid)) {
        Some(p) => p.to_string(),
        None => {
            let err = S1Error::Unsupported(format!(
                "no default provider for `{pid}`; pass ?provider=<name>"
            ));
            return error_response(proto, &err, &rid, None);
        }
    };
    let Some(backend) = st.backends.get(&provider).cloned() else {
        let err = S1Error::Unsupported(format!(
            "provider `{provider}` has no backend in this build"
        ));
        return error_response(proto, &err, &rid, Some(&provider));
    };
    if !backend.protocols().contains(&pid) {
        let err = S1Error::Unsupported(format!("provider `{provider}` does not speak `{pid}`"));
        return error_response(proto, &err, &rid, Some(&provider));
    }

    // Read the model for the header and the provider path only; the body
    // is sent as is. Only the top-level `model` is read, so a member the
    // typed protocol cannot read (left for the provider to judge) does not
    // change where the request goes. Inspection only sizes the limiter cost.
    //
    // A compressed body is read from a decoded copy (same cap as the typed
    // ingresses), so compression does not lower the limiter cost. The
    // provider still gets the original bytes and Content-Encoding. A body
    // that does not decode (unknown encoding, bad data) is costed by its
    // own size and left for the provider to judge.
    let view = match decode_body(&mut headers.clone(), body.clone(), MAX_BODY) {
        Ok(decoded) => decoded,
        Err(err @ S1Error::TooLong(_)) => return error_response(proto, &err, &rid, None),
        Err(_) => body.clone(),
    };
    let inspected = proto.and_then(|p| p.inspect(&view).ok());
    let tokens = inspected.as_ref().map_or(
        view.len().div_ceil(protocol::BYTES_PER_TOKEN_SAFE) as u64,
        |i| i.tokens_high(),
    );
    let sent_model = proto.and_then(|_| protocol::top_level_model(&view));
    drop(view);
    let lane = Lane::from_headers(&headers);
    let (s1_model, upstream) = match sent_model.as_deref() {
        Some(m) => match st.catalog.provider_model(&provider, m) {
            Some(r) => (Some(r.id()), r.upstream().to_string()),
            None => (Some(format!("{provider}/{m}")), m.to_string()),
        },
        None => (None, String::new()),
    };

    let (guard, credential) = st.guard_with_key(&provider).unzip();
    let req = BackendRequest {
        upstream_model: upstream,
        body: body.clone(),
        headers,
        credential,
    };
    let (result, stats) = match &guard {
        Some(g) => protect::call(g, backend.as_ref(), req, lane, tokens).await,
        None => (backend.call(req).await, CallStats::default()),
    };
    let queue_wait_ms = stats.queue_wait.as_millis() as u64;
    let upstream_ms = (started.elapsed().as_millis() as u64).saturating_sub(queue_wait_ms);
    let capacity = guard.as_ref().map(|g| g.cap().to_string());

    match result {
        Ok(resp) => {
            tracing::info!(
                request_id = %rid,
                ingress = %format!("raw/{pid}"),
                backend = %provider,
                model = s1_model.as_deref().unwrap_or(""),
                lane = lane.as_str(),
                status = resp.status.as_u16(),
                upstream_ms,
                queue_wait_ms,
                retries = stats.retries,
                input_tokens = tokens,
                input_bytes = body.len(),
                "request"
            );
            let mut out = Response::builder().status(resp.status);
            let h = out.headers_mut().expect("builder has headers");
            h.extend(resp.headers);
            set(h, H_BACKEND, &provider);
            if let Some(m) = &s1_model {
                set(h, H_MODEL, m);
            }
            set(h, H_REQUEST_ID, &rid);
            set(h, H_RETRIES, &stats.retries.to_string());
            if let Some(c) = &capacity {
                set(h, H_CAPACITY, c);
            }
            out.body(Body::from(resp.body)).expect("valid response")
        }
        Err(err) => {
            tracing::warn!(
                request_id = %rid,
                ingress = %format!("raw/{pid}"),
                backend = %provider,
                model = s1_model.as_deref().unwrap_or(""),
                status = err.status().as_u16(),
                code = err.code(),
                error = %err,
                lane = lane.as_str(),
                upstream_ms,
                queue_wait_ms,
                retries = stats.retries,
                input_tokens = tokens,
                input_bytes = body.len(),
                "request failed"
            );
            let mut r = error_response(proto, &err, &rid, Some(&provider));
            let h = r.headers_mut();
            if let Some(m) = &s1_model {
                set(h, H_MODEL, m);
            }
            set(h, H_RETRIES, &stats.retries.to_string());
            if let Some(c) = &capacity {
                set(h, H_CAPACITY, c);
            }
            r
        }
    }
}

pub(super) fn set(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(HeaderName::from_static(name), v);
    }
}

pub(super) fn error_response(
    proto: Option<&dyn protocol::Protocol>,
    err: &S1Error,
    rid: &str,
    backend: Option<&str>,
) -> Response {
    let body = match proto {
        Some(p) => p.error_body(err),
        None => protocol::generic_error_body(err),
    };
    let mut r = Response::builder()
        .status(err.status())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            r
        });
    let h = r.headers_mut();
    set(h, H_REQUEST_ID, rid);
    if let Some(b) = backend {
        set(h, H_BACKEND, b);
    }
    if let Some(secs) = err.retry_after() {
        set(h, "retry-after", &secs.to_string());
    }
    r
}
