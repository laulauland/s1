//! Family ingress endpoints: read routing facts, route by `model`, send the
//! original bytes (with `model` set to the upstream id), return the provider
//! response plus `s1-backend`, `s1-model`, and `s1-request-id`.
//!
//! Unlike `/v1/raw`, a typed ingress may adjust the request and response:
//! systemone image file paths are read on the Unix socket ([`images`]), and
//! [`crate::backend::Backend::typed_response`] fixes provider quirks.
//!
//! One handler serves every wire format; each endpoint is one line in
//! [`routes`] (stage 4 adds `/v1/decisions`, stage 5 `/v1/extract`).

use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;

use super::raw::{H_BACKEND, H_MODEL, H_REQUEST_ID, MAX_BODY, error_response, set};
use super::{AppState, request_id};
use crate::backend::BackendRequest;
use crate::error::S1Error;
use crate::images::{self, Origin};
use crate::protocol::{self, ProtocolId};
use crate::router::{LONG_CONTEXT_HEADER, RequestFlags, Router as ModelRouter};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/systemone",
            post(
                |st: State<AppState>, o: Option<Extension<Origin>>, h: HeaderMap, b: Bytes| {
                    handle(st, ProtocolId::Systemone, origin(o), h, b)
                },
            ),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// Request flags carried in headers.
fn flags(headers: &HeaderMap) -> RequestFlags {
    RequestFlags {
        long_context: headers
            .get(LONG_CONTEXT_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
    }
}

/// Listener of a request; no extension means TCP.
fn origin(ext: Option<Extension<Origin>>) -> Origin {
    ext.map_or(Origin::Tcp, |Extension(o)| o)
}

pub async fn handle(
    State(st): State<AppState>,
    pid: ProtocolId,
    origin: Origin,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id();
    let started = Instant::now();
    let proto = protocol::by_id(pid);
    let Some(p) = proto else {
        let err = S1Error::Unsupported(format!("`{pid}` is not in this build"));
        return error_response(None, &err, &rid, None);
    };
    // Image file paths become inline data before anything reads the body.
    let body = if pid == ProtocolId::Systemone {
        match images::expand_paths(&body, origin, MAX_BODY) {
            Ok(Some(expanded)) => expanded,
            Ok(None) => body,
            Err(err) => return error_response(proto, &err, &rid, None),
        }
    } else {
        body
    };
    let info = match p.inspect(&body) {
        Ok(i) => i,
        Err(e) => {
            return error_response(proto, &S1Error::InvalidRequest(e.0), &rid, None);
        }
    };
    let routed = {
        let available = |m: &crate::catalog::ModelRef<'_>| st.backends.ready(&st.keys, m);
        ModelRouter {
            catalog: &st.catalog,
            backends: &st.backends,
            available: &available,
        }
        .route(pid, &info, flags(&headers))
    };
    let route = match routed {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                status = err.status().as_u16(),
                code = err.code(),
                error = %err,
                input_bytes = body.len(),
                "request not routed"
            );
            return error_response(proto, &err, &rid, None);
        }
    };
    let send_body = match protocol::replace_model(&body, &route.backend.body_model(&route.upstream))
    {
        Ok(b) => b,
        Err(e) => return error_response(proto, &S1Error::InvalidRequest(e.0), &rid, None),
    };

    let result = route
        .backend
        .call(BackendRequest {
            upstream_model: route.upstream.clone(),
            body: send_body,
            headers,
        })
        .await
        // Typed ingress only: provider quirks such as TypeSafe's 403 for a
        // missing key or the Workers AI envelope. `/v1/raw` skips this.
        .and_then(|resp| route.backend.typed_response(&route.upstream, resp));
    let upstream_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(resp) => {
            tracing::info!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                backend = %route.provider,
                model = %route.model_id,
                status = resp.status.as_u16(),
                upstream_ms,
                input_bytes = body.len(),
                "request"
            );
            let mut out = Response::builder().status(resp.status);
            let h = out.headers_mut().expect("builder has headers");
            h.extend(resp.headers);
            set(h, H_BACKEND, &route.provider);
            set(h, H_MODEL, &route.model_id);
            set(h, H_REQUEST_ID, &rid);
            out.body(Body::from(resp.body)).expect("valid response")
        }
        Err(err) => {
            tracing::warn!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                backend = %route.provider,
                model = %route.model_id,
                status = err.status().as_u16(),
                code = err.code(),
                error = %err,
                upstream_ms,
                input_bytes = body.len(),
                "request failed"
            );
            let mut r = error_response(proto, &err, &rid, Some(&route.provider));
            set(r.headers_mut(), H_MODEL, &route.model_id);
            r
        }
    }
}
