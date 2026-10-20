//! Family ingress endpoints: read routing facts, route by `model`, look in
//! the cache, send the original bytes (with `model` set to the upstream id)
//! through the backend's queue and limits with retries, and return the
//! provider response plus `s1-backend`, `s1-model`, `s1-request-id`,
//! `s1-cache`, `s1-capacity`, and `s1-retries`.
//!
//! Request headers s1 reads: `s1-lane: bulk` (the bulk lane; the default is
//! interactive), `Cache-Control: no-cache` (skip the lookup, store the fresh
//! answer), and `Cache-Control: no-store` (do not use the cache).
//!
//! Unlike `/v1/raw`, a typed ingress may adjust the request and response:
//! systemone image file paths are read on the Unix socket ([`images`]), and
//! [`crate::backend::Backend::typed_response`] fixes provider quirks.
//!
//! One handler serves every wire format; each endpoint is one line in
//! [`routes`] (stage 4 adds `/v1/decisions`, stage 5 `/v1/extract`).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::post;

use super::raw::{
    H_BACKEND, H_CAPACITY, H_MODEL, H_REQUEST_ID, H_RETRIES, MAX_BODY, error_response, set,
};
use super::{AppState, request_id};
use crate::backend::BackendRequest;
use crate::cache::{self, CACHE_HEADER, Entry, Mode};
use crate::error::S1Error;
use crate::images::{self, Origin};
use crate::protect::{self, CallStats};
use crate::protocol::{self, ProtocolId};
use crate::queue::Lane;
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
    let flags = flags(&headers);
    let lane = Lane::from_headers(&headers);
    let mode = Mode::from_headers(&headers);
    let routed = {
        let available = |m: &crate::catalog::ModelRef<'_>| st.available(m);
        let throttled = |m: &crate::catalog::ModelRef<'_>| st.throttled(m);
        ModelRouter {
            catalog: &st.catalog,
            backends: &st.backends,
            available: &available,
            throttled: &throttled,
        }
        .route(pid, &info, flags)
    };
    let route = match routed {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                lane = lane.as_str(),
                status = err.status().as_u16(),
                code = err.code(),
                error = %err,
                input_bytes = body.len(),
                "request not routed"
            );
            return error_response(proto, &err, &rid, None);
        }
    };
    let tokens = info.tokens_high();

    // The cache key covers the family, the last resolved model version, the
    // request without `model` (image content already inline), and flags.
    let long_context = if flags.long_context { "true" } else { "false" };
    let cache = st.cache.clone().filter(|_| mode != Mode::Off);
    let key_for = |version: &str| {
        cache::key::key(
            p.family(),
            version,
            &body,
            &[("long_context", long_context)],
        )
        .ok()
    };
    let version = st.versions.current(&route.model_id);
    if let Some(c) = cache.as_ref().filter(|_| mode.lookup())
        && let Some(k) = key_for(&version)
    {
        match c.get(&k) {
            Ok(Some(entry)) => {
                tracing::info!(
                    request_id = %rid,
                    ingress = pid.as_str(),
                    alias = %info.model,
                    backend = %route.provider,
                    model = %route.model_id,
                    resolved_model = %entry.resolved_model,
                    lane = lane.as_str(),
                    status = 200u16,
                    cache = "hit",
                    input_tokens = tokens,
                    input_bytes = body.len(),
                    "request"
                );
                // The backend's cap, so the streaming CLI can size its
                // window from hits too.
                let capacity = st.guard(&route.provider).map(|g| g.cap());
                return hit_response(&entry, &route.provider, &route.model_id, &rid, capacity);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(request_id = %rid, error = %e, "cache read failed"),
        }
    }

    let send_body = match protocol::replace_model(&body, &route.backend.body_model(&route.upstream))
    {
        Ok(b) => b,
        Err(e) => return error_response(proto, &S1Error::InvalidRequest(e.0), &rid, None),
    };
    let (guard, credential) = st.guard_with_key(&route.provider).unzip();
    let req = BackendRequest {
        upstream_model: route.upstream.clone(),
        body: send_body,
        headers,
        credential,
    };
    let (result, stats) = match &guard {
        Some(g) => protect::call(g, route.backend.as_ref(), req, lane, tokens).await,
        // No key: the backend reports it.
        None => (route.backend.call(req).await, CallStats::default()),
    };
    // Typed ingress only: provider quirks such as TypeSafe's 403 for a
    // missing key or the Workers AI envelope. `/v1/raw` skips this.
    let result = result.and_then(|resp| route.backend.typed_response(&route.upstream, resp));
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let queue_wait_ms = stats.queue_wait.as_millis() as u64;
    let capacity = guard.as_ref().map(|g| g.cap());
    match result {
        Ok(resp) => {
            let mut cache_state = if cache.is_none() {
                "bypass"
            } else if mode.lookup() {
                "miss"
            } else {
                "refresh"
            };
            let resolved = cache::resolved_model(&resp.body);
            if resp.status.is_success() {
                if let Some(v) = &resolved {
                    st.versions.record(&route.model_id, v);
                }
                if let Some(c) = &cache
                    && mode.store()
                {
                    let version = resolved.clone().unwrap_or(version);
                    if let Some(k) = key_for(&version) {
                        let entry = Entry {
                            body: resp.body.clone(),
                            content_type: resp
                                .headers
                                .get(header::CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            resolved_model: version,
                            created: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_or(0, |d| d.as_secs()),
                        };
                        if let Err(e) = c.put(&k, &entry) {
                            tracing::warn!(request_id = %rid, error = %e, "cache write failed");
                        }
                    }
                }
            } else if cache.is_some() {
                cache_state = "bypass";
            }
            tracing::info!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                backend = %route.provider,
                model = %route.model_id,
                resolved_model = resolved.as_deref().unwrap_or(""),
                lane = lane.as_str(),
                status = resp.status.as_u16(),
                upstream_ms = elapsed_ms.saturating_sub(queue_wait_ms),
                queue_wait_ms,
                retries = stats.retries,
                cache = cache_state,
                input_tokens = tokens,
                input_bytes = body.len(),
                "request"
            );
            let mut out = Response::builder().status(resp.status);
            let h = out.headers_mut().expect("builder has headers");
            h.extend(resp.headers);
            set(h, H_BACKEND, &route.provider);
            set(h, H_MODEL, &route.model_id);
            set(h, H_REQUEST_ID, &rid);
            set(h, CACHE_HEADER, cache_state);
            set(h, H_RETRIES, &stats.retries.to_string());
            if let Some(cap) = capacity {
                set(h, H_CAPACITY, &cap.to_string());
            }
            out.body(Body::from(resp.body)).expect("valid response")
        }
        Err(err) => {
            tracing::warn!(
                request_id = %rid,
                ingress = pid.as_str(),
                alias = %info.model,
                backend = %route.provider,
                model = %route.model_id,
                lane = lane.as_str(),
                status = err.status().as_u16(),
                code = err.code(),
                error = %err,
                upstream_ms = elapsed_ms.saturating_sub(queue_wait_ms),
                queue_wait_ms,
                retries = stats.retries,
                input_tokens = tokens,
                input_bytes = body.len(),
                "request failed"
            );
            let mut r = error_response(proto, &err, &rid, Some(&route.provider));
            let h = r.headers_mut();
            set(h, H_MODEL, &route.model_id);
            set(h, H_RETRIES, &stats.retries.to_string());
            if let Some(cap) = capacity {
                set(h, H_CAPACITY, &cap.to_string());
            }
            r
        }
    }
}

/// A cached answer: the stored bytes and content type, plus s1 headers.
fn hit_response(
    entry: &Entry,
    provider: &str,
    model_id: &str,
    rid: &str,
    capacity: Option<u32>,
) -> Response {
    let mut out = Response::builder().status(200);
    let h = out.headers_mut().expect("builder has headers");
    if let Some(ct) = entry
        .content_type
        .as_deref()
        .and_then(|v| axum::http::HeaderValue::from_str(v).ok())
    {
        h.insert(header::CONTENT_TYPE, ct);
    }
    set(h, H_BACKEND, provider);
    set(h, H_MODEL, model_id);
    set(h, H_REQUEST_ID, rid);
    set(h, CACHE_HEADER, "hit");
    set(h, H_RETRIES, "0");
    if let Some(cap) = capacity {
        set(h, H_CAPACITY, &cap.to_string());
    }
    out.body(Body::from(entry.body.clone()))
        .expect("valid response")
}
