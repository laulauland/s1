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
//! `zstd` and `gzip` bodies are decoded ([`super::decode`]), systemone image
//! file paths are read on the Unix socket ([`images`]), and
//! [`crate::backend::Backend::typed_response`] fixes provider quirks.
//!
//! When the routed backend speaks another format of the family, the request
//! and the answer are translated ([`crate::translate`]): for example
//! `/v1/decisions` with `"model": "decide"` reaches Clef as systemone and
//! the client gets OpenAI Decisions answers. Provider errors are rendered
//! in the client's format. The cache keeps the client-format answer.
//!
//! One handler serves every wire format; each endpoint is one line in
//! [`routes`] (stage 5 adds `/v1/extract`).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::post;

use super::decode::decode_body;
use super::raw::{
    H_BACKEND, H_CAPACITY, H_MODEL, H_REQUEST_ID, H_RETRIES, MAX_BODY, error_response, read_body,
    set,
};
use super::{AppState, request_id};
use crate::backend::{BackendRequest, ProviderResponse};
use crate::cache::{self, CACHE_HEADER, Entry, Mode};
use crate::error::S1Error;
use crate::images::{self, Origin};
use crate::protect::{self, CallStats};
use crate::protocol::{self, ProtocolId};
use crate::queue::Lane;
use crate::router::{LONG_CONTEXT_HEADER, RequestFlags, Router as ModelRouter};
use crate::translate::{self, NameMap, Translated};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/systemone",
            post(
                |st: State<AppState>, o: Option<Extension<Origin>>, h: HeaderMap, b: Body| {
                    handle(st, ProtocolId::Systemone, origin(o), h, b)
                },
            ),
        )
        .route(
            "/v1/decisions",
            post(
                |st: State<AppState>, o: Option<Extension<Origin>>, h: HeaderMap, b: Body| {
                    handle(st, ProtocolId::OpenaiDecisions, origin(o), h, b)
                },
            ),
        )
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
    mut headers: HeaderMap,
    body: Body,
) -> Response {
    let rid = request_id();
    let started = Instant::now();
    let proto = protocol::by_id(pid);
    let Some(p) = proto else {
        let err = S1Error::Unsupported(format!("`{pid}` is not in this build"));
        return error_response(None, &err, &rid, None);
    };
    // Over the body limit: `too_long` in this ingress's error format.
    let body = match read_body(body).await {
        Ok(b) => b,
        Err(err) => return error_response(proto, &err, &rid, None),
    };
    // Compressed bodies (TypeSafe SDK default) are decoded first.
    let body = match decode_body(&mut headers, body, MAX_BODY) {
        Ok(b) => b,
        Err(err) => return error_response(proto, &err, &rid, None),
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
    let mut info = match p.inspect(&body) {
        Ok(i) => i,
        Err(e) => {
            return error_response(proto, &S1Error::InvalidRequest(e.0), &rid, None);
        }
    };
    // Toward OpenAI every asset reference is one image: routing and the
    // limiter cost count them per reference (spec "Translation rules").
    if pid == ProtocolId::Systemone && info.assets > 0 {
        info.openai_images = translate::decide::openai_image_occurrences(&body);
    }
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
    let tokens = info.toward(route.api).tokens_high();

    // Spec "Cache key": image content is hashed, not its path or URL.
    // Toward systemone s1 fetches image URLs anyway, so it fetches them
    // before the lookup and keys the inlined body. When the provider
    // fetches the URLs itself, s1 never sees the content: no cache.
    let fetches = pid == ProtocolId::OpenaiDecisions && route.api == ProtocolId::Systemone;
    let body = if fetches {
        match translate::fetch::inline_image_urls(&body, st.image_fetch, MAX_BODY).await {
            Ok(Some(inlined)) => inlined,
            Ok(None) => body,
            Err(err) => {
                tracing::warn!(
                    request_id = %rid,
                    ingress = pid.as_str(),
                    backend = %route.provider,
                    model = %route.model_id,
                    code = err.code(),
                    error = %err,
                    "image URLs not fetched"
                );
                return with_model(
                    error_response(proto, &err, &rid, Some(&route.provider)),
                    &route.model_id,
                );
            }
        }
    } else {
        body
    };
    let unseen_images =
        pid == ProtocolId::OpenaiDecisions && !fetches && translate::fetch::has_image_urls(&body);

    // The cache key covers the family, the last resolved model version, the
    // request without `model` (image content already inline), and flags.
    let long_context = if flags.long_context { "true" } else { "false" };
    let cache = st
        .cache
        .clone()
        .filter(|_| mode != Mode::Off && !unseen_images);
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

    // Another format of the family: translate the request (image URLs are
    // already inline when going to systemone) and keep the name map.
    let translation = if route.api == pid {
        None
    } else {
        match translate_request(pid, route.api, &body) {
            Ok(t) => Some(t),
            Err(err) => {
                tracing::warn!(
                    request_id = %rid,
                    ingress = pid.as_str(),
                    backend = %route.provider,
                    model = %route.model_id,
                    code = err.code(),
                    error = %err,
                    "request not translated"
                );
                return with_model(
                    error_response(proto, &err, &rid, Some(&route.provider)),
                    &route.model_id,
                );
            }
        }
    };
    let upstream_src = translation.as_ref().map_or(&body, |t| &t.body);
    let send_body =
        match protocol::replace_model(upstream_src, &route.backend.body_model(&route.upstream)) {
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
    // The resolved version is in the provider's own answer.
    let resolved = result
        .as_ref()
        .ok()
        .and_then(|r| cache::resolved_model(&r.body));
    let result = match &translation {
        Some(t) => result.and_then(|resp| translate_response(route.api, pid, &t.map, resp)),
        None => result,
    };
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

fn with_model(mut r: Response, model_id: &str) -> Response {
    set(r.headers_mut(), H_MODEL, model_id);
    r
}

/// A client request in `from` as a request in `to`. Image URLs toward
/// systemone must already be inline (the caller fetches them before the
/// cache lookup).
fn translate_request(
    from: ProtocolId,
    to: ProtocolId,
    body: &Bytes,
) -> Result<Translated, S1Error> {
    let tr = translate::translator(from, to)
        .ok_or_else(|| S1Error::Unsupported(format!("no translation from `{from}` to `{to}`")))?;
    tr.request(from, to, body)
}

/// A provider response in `from` for a client of `to`: answers are
/// translated with the name map; errors are rendered in the client's
/// format with the provider's status.
fn translate_response(
    from: ProtocolId,
    to: ProtocolId,
    map: &NameMap,
    resp: ProviderResponse,
) -> Result<ProviderResponse, S1Error> {
    let ProviderResponse {
        status,
        mut headers,
        body,
    } = resp;
    let body = if status.is_success() {
        let tr = translate::translator(from, to).ok_or_else(|| {
            S1Error::Unsupported(format!("no translation from `{from}` to `{to}`"))
        })?;
        tr.response(from, to, &body, map).map_err(|e| {
            S1Error::BackendUnavailable(format!("cannot translate the `{from}` answer: {e}"))
        })?
    } else {
        translate::errors::translate(to, status, &body)
    };
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok(ProviderResponse {
        status,
        headers,
        body,
    })
}
