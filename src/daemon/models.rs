//! `GET /v1/models`: merged catalog, aliases, capabilities, and state.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use super::AppState;
use crate::catalog::ModelState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/models", get(list))
}

async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(st.catalog.to_models_json(|m| ModelState {
        available: st.backends.ready(&st.keys, m).is_ok(),
        throttled_until: None,
        breaker_open: false,
    }))
}
