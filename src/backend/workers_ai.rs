//! Cloudflare Workers AI: `POST {base_url}/accounts/{account}/ai/run/@cf/cloudflare/{model}`
//! with a bearer API token. Serves Clef (systemone) now; Clairic (stage 5)
//! uses the same URL and envelope.
//!
//! [`Backend::call`] only selects the URL path from the upstream id; the
//! request and response bytes are unchanged, so `/v1/raw` is a pure
//! passthrough. The typed ingress does the rest in
//! [`Backend::typed_response`].
//!
//! Quirks (live calls, 2026-10-20; fixtures in `tests/fixtures/workers-ai/`):
//! - The model is in the URL path and in the body `model` selector, and they
//!   must match (else 422, code 3030). The typed ingress sets the body
//!   selector from the catalog `upstream` id (`clef` or
//!   `@cf/cloudflare/clef` both give `clef`, see [`model_ids`]); on `/v1/raw` the client's body
//!   `model` picks the path.
//! - Responses come in the v4 envelope `{result, success, errors, messages}`.
//!   On the typed ingress the client gets `result` only, and errors are
//!   rendered in the model's wire format.
//! - Long input: the docs say text `state` is silently truncated. Live,
//!   Clef rejected it instead: 413 code 5021 when `bytes / 4` exceeds the
//!   context, 422 code 3030 "Request requires N tokens". Both map to
//!   `too_long` on the typed ingress. The `bytes / 4` pre-check does not
//!   count image data (a 325 KB body with a 244 KB JPEG passed clef-flash).
//!   Before the call, the router refuses text `bytes / 4 > context`
//!   (images counted per image, see [`crate::protocol::RouteInfo`]). A
//!   success whose `usage.input_tokens` reaches the context is treated as
//!   truncated (`too_long`), for models with `compat.silent_truncation`.

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{Backend, BackendContext, BackendRequest, ProviderResponse, copy_headers};
use crate::catalog::{Model, Provider};
use crate::error::S1Error;
use crate::protocol::ProtocolId;
use crate::secrets::ApiKey;

/// Model path prefix for Cloudflare-hosted models.
pub const MODEL_PATH_PREFIX: &str = "@cf/cloudflare/";

pub struct WorkersAi {
    name: String,
    provider: Provider,
    ctx: BackendContext,
}

impl WorkersAi {
    pub fn new(name: &str, provider: Provider, ctx: BackendContext) -> Self {
        Self {
            name: name.to_string(),
            provider,
            ctx,
        }
    }

    fn model_by_upstream(&self, upstream: &str) -> Option<&Model> {
        self.provider
            .models
            .iter()
            .find(|m| m.upstream_id() == upstream)
    }

    fn protocol_of(&self, model: Option<&Model>) -> ProtocolId {
        model.and_then(|m| m.api).unwrap_or(self.provider.api)
    }

    /// The account id from the key, or `backend_unavailable` if missing or empty.
    fn account_id(&self, key: &ApiKey) -> Result<String, S1Error> {
        let auth_entry = self.provider.auth.as_deref().unwrap_or(&self.name);
        key.account_id.clone().filter(|a| !a.is_empty()).ok_or_else(|| {
            S1Error::BackendUnavailable(format!(
                "no Cloudflare account id for provider `{}`; set CLOUDFLARE_ACCOUNT_ID or run `s1 auth set {auth_entry} --account-id <id>`",
                self.name
            ))
        })
    }
}

/// URL path id and body selector for a catalog `upstream` id.
/// `clef` -> (`@cf/cloudflare/clef`, `clef`); `@cf/x/y` -> (`@cf/x/y`, `y`).
pub fn model_ids(upstream: &str) -> (String, String) {
    if upstream.starts_with('@') {
        let selector = upstream.rsplit('/').next().unwrap_or(upstream);
        (upstream.to_string(), selector.to_string())
    } else {
        (
            format!("{MODEL_PATH_PREFIX}{upstream}"),
            upstream.to_string(),
        )
    }
}

pub fn run_url(base_url: &str, account_id: &str, path_id: &str) -> String {
    format!(
        "{}/accounts/{account_id}/ai/run/{path_id}",
        base_url.trim_end_matches('/')
    )
}

/// Workers AI response headers passed back to the client.
pub fn forward_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "location"
            | "retry-after"
            | "ratelimit"
            | "ratelimit-policy"
            | "cf-ai-req-id"
            | "cf-ray"
    )
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CfError {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: String,
}

/// The envelope, opened.
#[derive(Debug)]
pub enum Unwrapped {
    /// `success: true`: the `result` bytes, unchanged.
    Result(Bytes),
    /// `success: false`.
    Errors(Vec<CfError>),
    /// Not an envelope (for example an HTML error page): pass through.
    Opaque,
}

pub fn unwrap_envelope(body: &[u8]) -> Unwrapped {
    let Ok(env) = serde_json::from_slice::<Envelope<'_>>(body) else {
        return Unwrapped::Opaque;
    };
    match (env.success, env.result) {
        (Some(true), Some(r)) => Unwrapped::Result(Bytes::copy_from_slice(r.get().as_bytes())),
        (Some(false), _) => Unwrapped::Errors(env.errors),
        _ => Unwrapped::Opaque,
    }
}

/// True if the errors say the input is over the model's context.
pub fn is_context_overflow(status: StatusCode, errors: &[CfError]) -> bool {
    errors.iter().any(|e| {
        e.code == Some(5021)
            || (e.code == Some(3006) && status == StatusCode::PAYLOAD_TOO_LARGE)
            || (e.code == Some(3030)
                && e.message.contains("Request requires")
                && e.message.contains("tokens"))
    })
}

/// `usage.input_tokens` of a decide or extract result.
fn input_tokens(result: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct R {
        usage: Option<U>,
    }
    #[derive(Deserialize)]
    struct U {
        input_tokens: Option<u64>,
    }
    serde_json::from_slice::<R>(result)
        .ok()?
        .usage?
        .input_tokens
}

/// Provider error in the systemone shape, with Cloudflare's codes kept.
fn systemone_error(status: StatusCode, errors: &[CfError]) -> Bytes {
    let error_type = match status.as_u16() {
        401 | 403 => "authentication_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    let message = errors
        .iter()
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Bytes::from(
        serde_json::json!({
            "detail": {
                "error_type": error_type,
                "message": format!("Workers AI: {message}"),
                "provider_errors": errors,
            }
        })
        .to_string(),
    )
}

#[async_trait]
impl Backend for WorkersAi {
    fn name(&self) -> &str {
        &self.name
    }

    fn protocols(&self) -> &[ProtocolId] {
        &[ProtocolId::Systemone]
    }

    fn caps(&self, model: &str) -> Option<&Model> {
        self.provider.models.iter().find(|m| m.id == model)
    }

    fn check_key(&self, key: &ApiKey) -> Result<(), S1Error> {
        self.account_id(key).map(|_| ())
    }

    /// The selector: `@cf/cloudflare/clef` gives `clef`.
    fn body_model(&self, upstream_model: &str) -> String {
        model_ids(upstream_model).1
    }

    /// Model path from the body-selected upstream id; the body and the
    /// response are not changed (the typed ingress already set the body
    /// `model` to the upstream id, which is the selector).
    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse, S1Error> {
        let auth_entry = self.provider.auth.as_deref().unwrap_or(&self.name);
        let key = self.ctx.credential(&req, &self.name, auth_entry)?;
        let account = self.account_id(&key)?;
        let (path_id, _) = model_ids(&req.upstream_model);

        let mut headers = HeaderMap::new();
        copy_headers(&req.headers, &mut headers, |n| n == "content-type");
        headers
            .entry(header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/json"));
        let resp = self
            .ctx
            .http
            .post(run_url(&self.provider.base_url, &account, &path_id))
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

    /// Remove the envelope; map context overflow and truncation to
    /// `too_long`; render other errors in the model's wire format.
    fn typed_response(
        &self,
        upstream_model: &str,
        resp: ProviderResponse,
    ) -> Result<ProviderResponse, S1Error> {
        let model = self.model_by_upstream(upstream_model);
        let ProviderResponse {
            status,
            mut headers,
            body: raw,
        } = resp;
        let json_type = HeaderValue::from_static("application/json");
        match unwrap_envelope(&raw) {
            Unwrapped::Result(result) => {
                if let (Some(m), Some(used)) = (model, input_tokens(&result))
                    && m.compat_flag("silent_truncation", false)
                    && m.context_tokens.is_some_and(|ctx| used >= ctx)
                {
                    return Err(S1Error::TooLong(format!(
                        "`{}/{upstream_model}` used {used} input tokens, its whole context; the input was probably truncated",
                        self.name
                    )));
                }
                headers.insert(header::CONTENT_TYPE, json_type);
                Ok(ProviderResponse {
                    status,
                    headers,
                    body: result,
                })
            }
            Unwrapped::Errors(errors) if is_context_overflow(status, &errors) => {
                let msg = errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(S1Error::TooLong(format!(
                    "input is over the context of `{}/{upstream_model}`: {msg}",
                    self.name
                )))
            }
            Unwrapped::Errors(errors) => {
                let body = match self.protocol_of(model) {
                    ProtocolId::Systemone => systemone_error(status, &errors),
                    // Stage 5 renders Clairic errors; until then keep the envelope.
                    _ => raw,
                };
                headers.insert(header::CONTENT_TYPE, json_type);
                Ok(ProviderResponse {
                    status,
                    headers,
                    body,
                })
            }
            Unwrapped::Opaque => Ok(ProviderResponse {
                status,
                headers,
                body: raw,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/workers-ai/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn key_without_account_id_is_not_usable() {
        use crate::secrets::{KeyResolver, KeySource, Secret};
        let provider = crate::catalog::Catalog::builtin().providers["workers-ai"].clone();
        let ctx = BackendContext::new(KeyResolver::new("/nonexistent/auth.json".into(), |_| None));
        let b = WorkersAi::new("workers-ai", provider, ctx);
        let key = |account: Option<&str>| ApiKey {
            key: Secret::new("t"),
            account_id: account.map(str::to_string),
            source: KeySource::Env,
        };
        assert!(b.check_key(&key(Some("acc"))).is_ok());
        for missing in [None, Some("")] {
            let e = b.check_key(&key(missing)).unwrap_err();
            assert_eq!(e.code(), "backend_unavailable");
            assert!(e.to_string().contains("CLOUDFLARE_ACCOUNT_ID"), "{e}");
        }
    }

    #[test]
    fn model_ids_put_model_in_path_and_body() {
        assert_eq!(
            model_ids("clef"),
            ("@cf/cloudflare/clef".to_string(), "clef".to_string())
        );
        assert_eq!(
            model_ids("@cf/cloudflare/clef-flash"),
            (
                "@cf/cloudflare/clef-flash".to_string(),
                "clef-flash".to_string()
            )
        );
        assert_eq!(
            run_url(
                "https://api.cloudflare.com/client/v4/",
                "acc",
                "@cf/cloudflare/clef"
            ),
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/run/@cf/cloudflare/clef"
        );
    }

    #[test]
    fn envelope_is_removed_on_success() {
        let Unwrapped::Result(r) = unwrap_envelope(&fixture("clef.response.json")) else {
            panic!("expected a result");
        };
        let v: serde_json::Value = serde_json::from_slice(&r).unwrap();
        assert_eq!(v["model"], "clef-flash");
        assert_eq!(v["answers"]["team"]["choice"], "technical");
        assert!(v.get("success").is_none());
        assert_eq!(input_tokens(&r), Some(346));
    }

    #[test]
    fn overflow_errors_are_detected() {
        for (name, status) in [
            ("too_many_tokens.422.json", 422),
            ("context_estimate.413.json", 413),
        ] {
            let Unwrapped::Errors(e) = unwrap_envelope(&fixture(name)) else {
                panic!("{name}: expected errors");
            };
            assert!(
                is_context_overflow(StatusCode::from_u16(status).unwrap(), &e),
                "{name}"
            );
        }
        for (name, status) in [
            ("model_mismatch.422.json", 422),
            ("extra_field.422.json", 422),
            ("invalid_token.401.json", 401),
        ] {
            let Unwrapped::Errors(e) = unwrap_envelope(&fixture(name)) else {
                panic!("{name}: expected errors");
            };
            assert!(
                !is_context_overflow(StatusCode::from_u16(status).unwrap(), &e),
                "{name}"
            );
        }
        assert!(matches!(unwrap_envelope(b"<html>"), Unwrapped::Opaque));
    }

    #[test]
    fn errors_render_in_systemone_shape() {
        let Unwrapped::Errors(e) = unwrap_envelope(&fixture("invalid_token.401.json")) else {
            panic!()
        };
        let v: serde_json::Value =
            serde_json::from_slice(&systemone_error(StatusCode::UNAUTHORIZED, &e)).unwrap();
        assert_eq!(v["detail"]["error_type"], "authentication_error");
        assert_eq!(v["detail"]["provider_errors"][0]["code"], 10000);
        assert!(
            v["detail"]["message"]
                .as_str()
                .unwrap()
                .contains("Authentication error")
        );
    }
}
