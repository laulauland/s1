//! OpenAI Decisions: `POST {base_url}/decisions` with a bearer key
//! (`base_url` is `https://api.openai.com/v1`).
//!
//! Quirks: public beta (no beta header is needed, live 2026-10-20); any
//! question may come back as `type: refusal` while the others get answers.
//! The response is not changed on any ingress: a systemone client gets
//! refusals as per-question errors through [`crate::translate`].

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

use super::{Backend, BackendContext, BackendRequest, ProviderResponse, copy_headers};
use crate::catalog::{Model, Provider};
use crate::error::S1Error;
use crate::protocol::ProtocolId;

pub struct OpenAi {
    name: String,
    provider: Provider,
    ctx: BackendContext,
}

impl OpenAi {
    pub fn new(name: &str, provider: Provider, ctx: BackendContext) -> Self {
        Self {
            name: name.to_string(),
            provider,
            ctx,
        }
    }

    pub fn url(&self) -> String {
        decisions_url(&self.provider.base_url)
    }
}

/// `{base_url}/decisions`.
pub fn decisions_url(base_url: &str) -> String {
    format!("{}/decisions", base_url.trim_end_matches('/'))
}

/// Client headers forwarded to OpenAI. Never `Authorization` or the
/// client's organization and project (the key belongs to s1).
pub fn forward_request_header(name: &str) -> bool {
    matches!(
        name,
        "content-type" | "content-encoding" | "x-client-request-id" | "idempotency-key"
    )
}

/// OpenAI response headers passed back to the client.
pub fn forward_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "content-encoding"
            | "location"
            | "retry-after"
            | "retry-after-ms"
            | "x-request-id"
            | "openai-processing-ms"
            | "openai-version"
    ) || name.starts_with("x-ratelimit-")
}

#[async_trait]
impl Backend for OpenAi {
    fn name(&self) -> &str {
        &self.name
    }

    fn protocols(&self) -> &[ProtocolId] {
        &[ProtocolId::OpenaiDecisions]
    }

    fn caps(&self, model: &str) -> Option<&Model> {
        self.provider.models.iter().find(|m| m.id == model)
    }

    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse, S1Error> {
        let auth_entry = self.provider.auth.as_deref().unwrap_or(&self.name);
        let key = self.ctx.credential(&req, &self.name, auth_entry)?;
        let mut headers = HeaderMap::new();
        copy_headers(&req.headers, &mut headers, forward_request_header);
        headers
            .entry(header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/json"));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_header_filters() {
        assert_eq!(
            decisions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/decisions"
        );
        assert!(forward_request_header("content-type"));
        assert!(!forward_request_header("authorization"));
        assert!(!forward_request_header("openai-organization"));
        assert!(!forward_request_header("typesafe-danger-zone-images"));
        assert!(forward_response_header("x-ratelimit-remaining-tokens"));
        assert!(forward_response_header("x-request-id"));
        assert!(forward_response_header("location"));
        assert!(!forward_response_header("set-cookie"));
    }
}
