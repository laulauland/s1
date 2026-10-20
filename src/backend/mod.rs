//! Provider endpoints. One module per provider; [`build`] is the one place
//! to register a new one.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;

use crate::catalog::{Catalog, Model, ModelRef, Provider};
use crate::error::S1Error;
use crate::protocol::ProtocolId;
use crate::secrets::{ApiKey, KeyResolver};

pub mod typesafe;
pub mod workers_ai;

/// One call to a provider.
#[derive(Debug, Clone)]
pub struct BackendRequest {
    /// Model id sent to the provider (the catalog `upstream`).
    pub upstream_model: String,
    /// Request bytes; sent unchanged. On `/v1/raw` these are the client's
    /// bytes; on a typed ingress `model` holds the upstream id.
    pub body: Bytes,
    /// Client headers the provider should see (content type, encoding,
    /// provider opt-in headers). Never the client's `Authorization`.
    pub headers: HeaderMap,
}

/// The provider's answer, unchanged.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: StatusCode,
    /// Headers worth passing back (content type/encoding, rate limits, request id).
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// One provider endpoint.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Provider name in the catalog.
    fn name(&self) -> &str;
    /// Formats it accepts natively.
    fn protocols(&self) -> &[ProtocolId];
    /// Capabilities of a model, from the catalog.
    fn caps(&self, model: &str) -> Option<&Model>;
    /// Check that a resolved key holds everything [`Backend::call`] needs
    /// (for example a Cloudflare account id). Fails with `backend_unavailable`.
    fn check_key(&self, _key: &ApiKey) -> Result<(), S1Error> {
        Ok(())
    }
    /// The body `model` for a typed request to `upstream_model` (the
    /// catalog `upstream` id). Default: the id itself. `/v1/raw` never
    /// rewrites the body.
    fn body_model(&self, upstream_model: &str) -> String {
        upstream_model.to_string()
    }
    /// Send the request; return the provider response unchanged (status,
    /// body, and the forwarded headers). `/v1/raw` returns this as is.
    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse, S1Error>;
    /// Adjust a response for the typed ingress: provider quirks such as
    /// TypeSafe's 403 for a missing key (a clear 401) or the Workers AI
    /// envelope (removed; overflow and truncation become `too_long`).
    /// `upstream_model` is the id that [`Backend::call`] sent.
    /// `/v1/raw` never calls this.
    fn typed_response(
        &self,
        _upstream_model: &str,
        resp: ProviderResponse,
    ) -> Result<ProviderResponse, S1Error> {
        Ok(resp)
    }
}

/// Shared context for building backends.
#[derive(Clone, Debug)]
pub struct BackendContext {
    pub http: reqwest::Client,
    pub keys: KeyResolver,
}

impl BackendContext {
    pub fn new(keys: KeyResolver) -> Self {
        let http = reqwest::Client::builder()
            // Raw passthrough returns the provider answer unchanged, 3xx included.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("build HTTP client");
        Self { http, keys }
    }

    /// Resolve a key or fail with `backend_unavailable`.
    pub fn key(&self, provider: &str, auth_entry: &str) -> Result<ApiKey, S1Error> {
        match self.keys.resolve(auth_entry) {
            Ok(Some(k)) => Ok(k),
            Ok(None) => Err(S1Error::BackendUnavailable(format!(
                "no API key for provider `{provider}`; run `s1 auth set {auth_entry}`"
            ))),
            Err(e) => Err(S1Error::BackendUnavailable(e.to_string())),
        }
    }
}

/// Build the backend for one catalog provider, if this build supports it.
pub fn build(name: &str, provider: &Provider, ctx: &BackendContext) -> Option<Arc<dyn Backend>> {
    match (name, provider.api) {
        // Stage 4 registers `openai`; stage 5 adds Clairic to `workers-ai`.
        ("workers-ai", _) => Some(Arc::new(workers_ai::WorkersAi::new(
            name,
            provider.clone(),
            ctx.clone(),
        ))),
        (_, ProtocolId::Systemone) => Some(Arc::new(typesafe::TypeSafe::new(
            name,
            provider.clone(),
            ctx.clone(),
        ))),
        _ => None,
    }
}

/// All backends this build supports, by provider name.
#[derive(Clone, Default)]
pub struct Registry {
    backends: BTreeMap<String, Arc<dyn Backend>>,
}

impl Registry {
    pub fn from_catalog(catalog: &Catalog, ctx: &BackendContext) -> Self {
        let backends = catalog
            .providers
            .iter()
            .filter_map(|(name, p)| build(name, p, ctx).map(|b| (name.clone(), b)))
            .collect();
        Self { backends }
    }

    pub fn get(&self, provider: &str) -> Option<&Arc<dyn Backend>> {
        self.backends.get(provider)
    }

    /// Ok if the model's provider has a backend and a usable key now.
    /// Router and `/v1/models` use this so that "available" means
    /// "`call` will not fail for lack of credentials".
    pub fn ready(&self, keys: &KeyResolver, m: &ModelRef<'_>) -> Result<(), S1Error> {
        let backend = self.get(m.provider_name).ok_or_else(|| {
            S1Error::BackendUnavailable(format!(
                "provider `{}` has no backend in this build",
                m.provider_name
            ))
        })?;
        let key = match keys.resolve(m.auth_entry()) {
            Ok(Some(k)) => k,
            Ok(None) => {
                return Err(S1Error::BackendUnavailable(format!(
                    "no API key for provider `{}`; run `s1 auth set {}`",
                    m.provider_name,
                    m.auth_entry()
                )));
            }
            Err(e) => return Err(S1Error::BackendUnavailable(e.to_string())),
        };
        backend.check_key(&key)
    }

    pub fn insert(&mut self, backend: Arc<dyn Backend>) {
        self.backends.insert(backend.name().to_string(), backend);
    }
}

/// Copy the named headers from `src` into `dst`.
pub(crate) fn copy_headers(src: &HeaderMap, dst: &mut HeaderMap, keep: impl Fn(&str) -> bool) {
    for (name, value) in src {
        if keep(name.as_str()) {
            dst.append(name.clone(), value.clone());
        }
    }
}
