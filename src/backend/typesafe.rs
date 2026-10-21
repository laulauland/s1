//! TypeSafe Jev: `POST {base_url}/v1/systemone` with a bearer key.
//!
//! Quirks:
//! - Images (`assets`) and `box` questions need `TypeSafe-Danger-Zone-Images: true`.
//!   Long context needs `TypeSafe-Danger-Zone-Long-Context: true`. Client
//!   headers pass through; s1 adds them when the request needs them
//!   ([`opt_in_headers`]).
//! - A missing key returns 403, not 401. On the typed `/v1/systemone`
//!   ingress s1 turns that 403 into 401 with a message that names
//!   `s1 auth set` ([`map_missing_key`]). `/v1/raw` returns the 403 unchanged.
//!
//! Baseten Model APIs (the baselabs decision models) speak the same format
//! with a bearer key, so they use this client too. Baseten docs ask for a
//! string or a `{"messages": [...]}` state; with `compat.string_state` s1
//! sends any other JSON state as text ([`stringify_state`], on the typed
//! ingress only). A bad key
//! returns 403 `{"error": ...}`, passed through unchanged.

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use bytes::Bytes;

use super::{Backend, BackendContext, BackendRequest, ProviderResponse, copy_headers};
use crate::catalog::{Images, Model, Provider};
use crate::error::S1Error;
use crate::protocol::{Protocol, ProtocolId, RouteInfo, systemone::Systemone};

pub const IMAGES_HEADER: &str = "typesafe-danger-zone-images";
pub const LONG_CONTEXT_HEADER: &str = crate::router::LONG_CONTEXT_HEADER;
/// Set on a response whose status s1 changed; holds the provider's status.
pub const UPSTREAM_STATUS_HEADER: &str = "s1-upstream-status";

pub struct TypeSafe {
    name: String,
    provider: Provider,
    ctx: BackendContext,
}

impl TypeSafe {
    pub fn new(name: &str, provider: Provider, ctx: BackendContext) -> Self {
        Self {
            name: name.to_string(),
            provider,
            ctx,
        }
    }

    fn url(&self) -> String {
        format!(
            "{}/v1/systemone",
            self.provider.base_url.trim_end_matches('/')
        )
    }
}

/// Client headers forwarded to TypeSafe.
pub fn forward_request_header(name: &str) -> bool {
    matches!(name, "content-type" | "content-encoding") || name.starts_with("typesafe-")
}

/// TypeSafe response headers passed back to the client.
pub fn forward_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-type" | "content-encoding" | "location" | "retry-after" | "retry-after-ms"
    ) || name.starts_with("x-typesafe-")
        || name.starts_with("x-ratelimit-")
}

/// Opt-in headers the request needs and the client did not send.
pub fn opt_in_headers(model: Option<&Model>, info: &RouteInfo, sent: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    let yes = HeaderValue::from_static("true");
    let needs_images = info.assets > 0 || info.has_box;
    let preview = model.is_none_or(|m| m.images == Images::Preview);
    if needs_images && preview && !sent.contains_key(IMAGES_HEADER) {
        out.insert(HeaderName::from_static(IMAGES_HEADER), yes.clone());
    }
    // Jev limits the request (64k) and each question (32k: the state plus
    // that question); the opt-in raises both.
    let needs_long = |m: &Model| {
        m.context_tokens.is_some_and(|ctx| info.tokens_high() > ctx)
            || m.question_context_tokens
                .is_some_and(|ctx| info.question_tokens_high() > ctx)
    };
    if let Some(m) = model
        && m.long_context_tokens.is_some()
        && needs_long(m)
        && !sent.contains_key(LONG_CONTEXT_HEADER)
    {
        out.insert(HeaderName::from_static(LONG_CONTEXT_HEADER), yes);
    }
    out
}

/// For a model with `compat.string_state` (Baseten): a `state` that is not
/// a string or a `{"messages": [...]}` conversation is sent as JSON text,
/// indented by 2: the form the baselabs models were trained on. Baseten
/// takes other JSON too but reads it in another form (live, 2026-10-22: the
/// answers differ). The typed ingress calls this; `/v1/raw` sends the
/// client's bytes unchanged.
///
/// Every other top-level member keeps its exact bytes. Inside the state,
/// key order and strings are kept; numbers are printed again (`1e2` becomes
/// `100.0`, an integer beyond 64 bits loses precision). `Ok(None)`: the body
/// needs no change. `Err`: the state is nested deeper than s1 can read.
pub fn stringify_state(body: &[u8]) -> Result<Option<Bytes>, String> {
    use crate::protocol::TopLevel;
    let Ok(members) = serde_json::from_slice::<TopLevel<'_>>(body) else {
        return Ok(None);
    };
    let Some((_, raw)) = members.0.iter().find(|(k, _)| k == "state") else {
        return Ok(None);
    };
    let state: serde_json::Value = match serde_json::from_str(raw.get()) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("recursion limit") => {
            return Err("`state` is nested too deeply to send as text (at most 127 levels)".into());
        }
        Err(_) => return Ok(None),
    };
    let conversation = state
        .as_object()
        .is_some_and(|o| o.len() == 1 && o.get("messages").is_some_and(|m| m.is_array()));
    if state.is_string() || state.is_null() || conversation {
        return Ok(None);
    }
    let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(&text).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(body.len() + text.len());
    out.push(b'{');
    for (i, (k, v)) in members.0.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(serde_json::to_string(k).expect("key").as_bytes());
        out.push(b':');
        if k == "state" {
            out.extend_from_slice(text.as_bytes());
        } else {
            out.extend_from_slice(v.get().as_bytes());
        }
    }
    out.push(b'}');
    Ok(Some(Bytes::from(out)))
}

/// TypeSafe answers 403 (not 401) when no key arrives. Make it a clear 401.
pub fn map_missing_key(resp: ProviderResponse, auth_entry: &str) -> ProviderResponse {
    if resp.status != StatusCode::FORBIDDEN {
        return resp;
    }
    let detail: Option<serde_json::Value> = serde_json::from_slice(&resp.body).ok();
    let detail = detail.as_ref().and_then(|v| v.get("detail"));
    if detail
        .and_then(|d| d.get("error_type"))
        .and_then(|t| t.as_str())
        != Some("authentication_error")
    {
        return resp;
    }
    let upstream = detail
        .and_then(|d| d.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let body = serde_json::json!({
        "detail": {
            "error_type": "authentication_error",
            "message": format!(
                "TypeSafe got no API key (HTTP 403: {upstream}). Set one with `s1 auth set {auth_entry}` or TYPESAFE_API_KEY."
            ),
        }
    });
    let mut headers = resp.headers;
    headers.insert(
        HeaderName::from_static(UPSTREAM_STATUS_HEADER),
        HeaderValue::from_static("403"),
    );
    ProviderResponse {
        status: StatusCode::UNAUTHORIZED,
        headers,
        body: Bytes::from(body.to_string()),
    }
}

#[async_trait]
impl Backend for TypeSafe {
    fn name(&self) -> &str {
        &self.name
    }

    fn protocols(&self) -> &[ProtocolId] {
        &[ProtocolId::Systemone]
    }

    fn caps(&self, model: &str) -> Option<&Model> {
        self.provider.models.iter().find(|m| m.id == model)
    }

    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse, S1Error> {
        let auth_entry = self.provider.auth.as_deref().unwrap_or(&self.name);
        let key = self.ctx.credential(&req, &self.name, auth_entry)?;
        let mut headers = HeaderMap::new();
        copy_headers(&req.headers, &mut headers, forward_request_header);
        let model = self
            .provider
            .models
            .iter()
            .find(|m| m.upstream_id() == req.upstream_model);
        if let Ok(info) = Systemone.inspect(&req.body) {
            headers.extend(opt_in_headers(model, &info, &headers));
        }
        if !headers.contains_key(header::CONTENT_TYPE) {
            headers.insert(
                header::CONTENT_TYPE,
                "application/json".parse().expect("valid header"),
            );
        }
        let resp = self
            .ctx
            .http
            .post(self.url())
            .headers(headers)
            .bearer_auth(key.key.expose())
            .body(req.body)
            .send()
            .await
            .map_err(|e| {
                S1Error::Unreachable(format!("cannot reach {}: {}", self.name, e.without_url()))
            })?;
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut out_headers = HeaderMap::new();
        copy_headers(resp.headers(), &mut out_headers, forward_response_header);
        let body = resp.bytes().await.map_err(|e| {
            S1Error::Unreachable(format!(
                "{} closed the response early: {}",
                self.name,
                e.without_url()
            ))
        })?;
        Ok(ProviderResponse {
            status,
            headers: out_headers,
            body,
        })
    }

    fn typed_response(
        &self,
        _upstream_model: &str,
        resp: ProviderResponse,
    ) -> Result<ProviderResponse, S1Error> {
        Ok(map_missing_key(
            resp,
            self.provider.auth.as_deref().unwrap_or(&self.name),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn jev() -> Model {
        Catalog::builtin()
            .model("typesafe/jev-latest")
            .unwrap()
            .model
            .clone()
    }

    fn state_of(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["state"].clone()
    }

    fn stringified(body: &[u8]) -> Bytes {
        stringify_state(body).unwrap().expect("state converted")
    }

    #[test]
    fn stringify_state_sends_json_states_as_indented_text() {
        let body =
            r#"{"model":"m","state":{"b":1,"a":"é"},"questions":{"q2":{},"q1":{}}}"#.as_bytes();
        let out = stringified(body);
        assert_eq!(state_of(&out), "{\n  \"b\": 1,\n  \"a\": \"é\"\n}");
        // Other members keep their exact bytes (and so question and option
        // order).
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"model":"m","state":"{\n  \"b\": 1,\n  \"a\": \"é\"\n}","questions":{"q2":{},"q1":{}}}"#
        );
        let spaced = br#"{ "questions" : {"q": {"options": [ "b" , "a" ]}}, "state": [1,2] }"#;
        let out = stringified(spaced);
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"questions":{"q": {"options": [ "b" , "a" ]}},"state":"[\n  1,\n  2\n]"}"#
        );
        assert_eq!(state_of(&stringified(br#"{"state":7}"#)), "7");
    }

    #[test]
    fn stringify_state_keeps_text_and_conversations() {
        let none = |b: &[u8]| stringify_state(b).unwrap().is_none();
        assert!(none(br#"{"state":"text"}"#));
        assert!(none(br#"{"state":null}"#));
        assert!(none(br#"{"questions":{}}"#));
        assert!(none(
            br#"{"state":{"messages":[{"role":"user","content":"hi"}]}}"#
        ));
        // Not a bare conversation: one more key, or `messages` not a list.
        assert!(!none(br#"{"state":{"messages":[],"x":1}}"#));
        assert!(!none(br#"{"state":{"messages":"hi"}}"#));
        assert!(none(b"not json"));
    }

    #[test]
    fn stringify_state_refuses_a_state_too_deep_to_read() {
        let deep = format!(r#"{{"state":{}1{}}}"#, "[".repeat(200), "]".repeat(200));
        assert!(stringify_state(deep.as_bytes()).is_err());
        let ok = format!(r#"{{"state":{}1{}}}"#, "[".repeat(100), "]".repeat(100));
        assert!(stringify_state(ok.as_bytes()).unwrap().is_some());
    }

    #[test]
    fn images_header_added_for_assets_and_box() {
        let m = jev();
        let none = HeaderMap::new();
        let plain = RouteInfo {
            input_bytes: 100,
            ..Default::default()
        };
        assert!(opt_in_headers(Some(&m), &plain, &none).is_empty());
        let assets = RouteInfo {
            assets: 1,
            ..plain.clone()
        };
        assert_eq!(
            opt_in_headers(Some(&m), &assets, &none)[IMAGES_HEADER],
            "true"
        );
        let boxq = RouteInfo {
            has_box: true,
            ..plain.clone()
        };
        assert_eq!(
            opt_in_headers(Some(&m), &boxq, &none)[IMAGES_HEADER],
            "true"
        );
        // Not twice.
        let mut sent = HeaderMap::new();
        sent.insert(IMAGES_HEADER, HeaderValue::from_static("true"));
        assert!(opt_in_headers(Some(&m), &assets, &sent).is_empty());
    }

    #[test]
    fn long_context_header_added_when_input_needs_it() {
        let m = jev();
        let short = RouteInfo {
            input_bytes: 90_000,
            ..Default::default()
        };
        assert!(opt_in_headers(Some(&m), &short, &HeaderMap::new()).is_empty());
        let long = RouteInfo {
            input_bytes: 300_000,
            ..Default::default()
        };
        assert_eq!(
            opt_in_headers(Some(&m), &long, &HeaderMap::new())[LONG_CONTEXT_HEADER],
            "true"
        );
    }

    #[test]
    fn long_context_header_added_when_one_question_needs_it() {
        let m = jev();
        // About 40k tokens: under the 65,536 request limit, but state plus
        // one question is over Jev's 32k per-question limit.
        let big_state = RouteInfo {
            input_bytes: 120_000,
            questions: 2,
            questions_bytes: 200,
            max_question_bytes: 100,
            ..Default::default()
        };
        assert_eq!(
            opt_in_headers(Some(&m), &big_state, &HeaderMap::new())[LONG_CONTEXT_HEADER],
            "true"
        );
        // Same size, but spread over many small questions: each question
        // sees only the state and itself.
        let many_questions = RouteInfo {
            input_bytes: 120_000,
            questions: 1_000,
            questions_bytes: 110_000,
            max_question_bytes: 110,
            ..Default::default()
        };
        assert!(opt_in_headers(Some(&m), &many_questions, &HeaderMap::new()).is_empty());
    }

    #[test]
    fn missing_key_403_becomes_clear_401() {
        let body = br#"{"detail":{"error_type":"authentication_error","message":"Must supply an API key! Check your request and try again."}}"#;
        let resp = map_missing_key(
            ProviderResponse {
                status: StatusCode::FORBIDDEN,
                headers: HeaderMap::new(),
                body: Bytes::from_static(body),
            },
            "typesafe",
        );
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
        assert_eq!(resp.headers[UPSTREAM_STATUS_HEADER], "403");
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["detail"]["error_type"], "authentication_error");
        let msg = v["detail"]["message"].as_str().unwrap();
        assert!(msg.contains("s1 auth set typesafe"), "{msg}");
        assert!(msg.contains("403"), "{msg}");

        // Other 403s and other statuses pass through.
        let other = map_missing_key(
            ProviderResponse {
                status: StatusCode::FORBIDDEN,
                headers: HeaderMap::new(),
                body: Bytes::from_static(br#"{"detail":{"error_type":"permission_error"}}"#),
            },
            "typesafe",
        );
        assert_eq!(other.status, StatusCode::FORBIDDEN);
    }
}
