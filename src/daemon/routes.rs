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
        .with_state(state)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"ok": true, "pid": std::process::id()}))
}
