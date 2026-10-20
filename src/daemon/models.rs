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
    Json(st.catalog.to_models_json(|m| {
        let has_backend = st.backends.get(m.provider_name).is_some();
        let has_key = matches!(st.keys.resolve(m.auth_entry()), Ok(Some(_)));
        ModelState {
            available: has_backend && has_key,
            throttled_until: None,
            breaker_open: false,
        }
    }))
}
