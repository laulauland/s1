//! Route registration: the one place where endpoints are added.
//! Later stages add a `.merge(<module>::routes())` line each.

use axum::Router;
use axum::routing::get;

use super::reload::Shared;
use super::{AppState, cache, ingress, models, raw, reload, status};

/// The router over one fixed state (nothing reloads it).
pub fn router(state: AppState) -> Router {
    router_for(Shared::new(state))
}

/// The router over a reloadable state.
pub fn router_for(shared: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(models::routes())
        .merge(status::routes())
        .merge(cache::routes())
        .merge(raw::routes())
        .merge(ingress::routes())
        .merge(reload::routes())
        .merge(crate::mcp::routes())
        .with_state(shared)
}

/// Whether systemd passed the listeners (see [`super::activation`]).
#[derive(Clone, Copy)]
pub struct SocketActivated(pub bool);

async fn health(
    activated: Option<axum::Extension<SocketActivated>>,
) -> axum::Json<serde_json::Value> {
    let activated = activated.is_some_and(|a| a.0.0);
    axum::Json(serde_json::json!({
        "ok": true,
        "pid": std::process::id(),
        "socket_activated": activated,
    }))
}
