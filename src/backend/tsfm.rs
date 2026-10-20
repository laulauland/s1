//! A TSFM.ai-compatible forecast server: `POST {base_url}/v1/forecast` with
//! a bearer key (TSFM.ai itself, or a self-hosted server such as Sidus).
//!
//! The compiled-in catalog has no forecast provider; this backend is used
//! only for providers with `api = "tsfm"` in `models.toml`. It passes the
//! request and response through unchanged, so the quantile levels the
//! server produced are the levels the client gets.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode, header};

use super::{Backend, BackendContext, BackendRequest, ProviderResponse, copy_headers};
use crate::catalog::{Model, Provider};
use crate::error::S1Error;
use crate::protocol::ProtocolId;

pub struct Tsfm {
    name: String,
    provider: Provider,
    ctx: BackendContext,
}

impl Tsfm {
    pub fn new(name: &str, provider: Provider, ctx: BackendContext) -> Self {
        Self {
            name: name.to_string(),
            provider,
            ctx,
        }
    }

    fn url(&self) -> String {
        format!(
            "{}{}",
            self.provider.base_url.trim_end_matches('/'),
            crate::protocol::tsfm::ENDPOINT
        )
    }
}

/// TSFM response headers passed back to the client (rate limits per the
/// TSFM docs: `x-ratelimit-*` and `retry-after`).
pub fn forward_response_header(name: &str) -> bool {
    matches!(name, "content-type" | "content-encoding" | "retry-after")
        || name.starts_with("x-ratelimit-")
        || name == "x-request-id"
}

#[async_trait]
impl Backend for Tsfm {
    fn name(&self) -> &str {
        &self.name
    }

    fn protocols(&self) -> &[ProtocolId] {
        &[ProtocolId::Tsfm]
    }

    fn caps(&self, model: &str) -> Option<&Model> {
        self.provider.models.iter().find(|m| m.id == model)
    }

    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse, S1Error> {
        let auth_entry = self.provider.auth.as_deref().unwrap_or(&self.name);
        let key = self.ctx.credential(&req, &self.name, auth_entry)?;
        let mut headers = HeaderMap::new();
        copy_headers(&req.headers, &mut headers, |n| {
            matches!(n, "content-type" | "content-encoding")
        });
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
}
