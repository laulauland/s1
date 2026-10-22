//! Route registration: the one place where endpoints are added.
//! Later stages add a `.merge(<module>::routes())` line each.

use axum::Router;
use axum::routing::get;

use super::{AppState, cache, ingress, models, raw, status};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(models::routes())
        .merge(status::routes())
        .merge(cache::routes())
        .merge(raw::routes())
        .merge(ingress::routes())
        .merge(crate::mcp::routes())
        .with_state(state)
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
