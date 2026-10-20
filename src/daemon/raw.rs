//! `POST /v1/raw/{protocol}[?provider=<name>]`: pure passthrough.
//!
//! The original request bytes go to the provider unchanged, and the provider
//! response comes back unchanged, plus `s1-backend`, `s1-model`, and
//! `s1-request-id` headers. No routing by alias, no queues (later stages).

use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde::Deserialize;

use super::{AppState, request_id};
use crate::backend::BackendRequest;
use crate::error::S1Error;
use crate::protocol::{self, ProtocolId};

/// Largest body accepted (TypeSafe allows 32 MiB).
pub const MAX_BODY: usize = 64 * 1024 * 1024;

pub const H_BACKEND: &str = "s1-backend";
pub const H_MODEL: &str = "s1-model";
pub const H_REQUEST_ID: &str = "s1-request-id";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/raw/{protocol}", post(raw))
        .layer(DefaultBodyLimit::max(MAX_BODY))
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
    body: Bytes,
) -> Response {
    let rid = request_id();
    let started = Instant::now();
    let Ok(pid) = protocol.parse::<ProtocolId>() else {
        let err = S1Error::Unsupported(format!("unknown protocol `{protocol}`"));
        return error_response(None, &err, &rid, None);
    };
    let proto = protocol::by_id(pid);
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
    // change where the request goes.
    let sent_model = proto.and_then(|_| protocol::top_level_model(&body));
    let (s1_model, upstream) = match sent_model.as_deref() {
        Some(m) => match st.catalog.provider_model(&provider, m) {
            Some(r) => (Some(r.id()), r.upstream().to_string()),
            None => (Some(format!("{provider}/{m}")), m.to_string()),
        },
        None => (None, String::new()),
    };

    let result = backend
        .call(BackendRequest {
            upstream_model: upstream,
            body: body.clone(),
            headers,
        })
        .await;
    let upstream_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => {
            tracing::info!(
                request_id = %rid,
                ingress = %format!("raw/{pid}"),
                backend = %provider,
                model = s1_model.as_deref().unwrap_or(""),
                status = resp.status.as_u16(),
                upstream_ms,
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
                upstream_ms,
                input_bytes = body.len(),
                "request failed"
            );
            let mut r = error_response(proto, &err, &rid, Some(&provider));
            if let Some(m) = &s1_model {
                set(r.headers_mut(), H_MODEL, m);
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
