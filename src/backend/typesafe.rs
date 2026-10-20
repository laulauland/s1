//! TypeSafe Jev: `POST {base_url}/v1/systemone` with a bearer key.
//!
//! Quirks: images need `TypeSafe-Danger-Zone-Images: true`, long context needs
//! `TypeSafe-Danger-Zone-Long-Context: true` (both passed through from the
//! client); a missing key returns 403, not 401.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode, header};

use super::{Backend, BackendContext, BackendRequest, ProviderResponse, copy_headers};
use crate::catalog::{Model, Provider};
use crate::error::S1Error;
use crate::protocol::ProtocolId;

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
        let key = self.ctx.key(&self.name, auth_entry)?;
        let mut headers = HeaderMap::new();
        copy_headers(&req.headers, &mut headers, forward_request_header);
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
                S1Error::BackendUnavailable(format!(
                    "cannot reach {}: {}",
                    self.name,
                    e.without_url()
                ))
            })?;
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut out_headers = HeaderMap::new();
        copy_headers(resp.headers(), &mut out_headers, forward_response_header);
        let body = resp.bytes().await.map_err(|e| {
            S1Error::BackendUnavailable(format!(
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
