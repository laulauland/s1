//! Route registration: the one place where endpoints are added.
//! Later stages add a `.merge(<module>::routes())` line each.

use axum::Router;
use axum::routing::get;

use super::{AppState, models, raw};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(models::routes())
        .merge(raw::routes())
        .with_state(state)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"ok": true, "pid": std::process::id()}))
}
