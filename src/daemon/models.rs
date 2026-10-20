//! `GET /v1/models`: merged catalog, aliases, capabilities, and live state
//! (`available`, `throttled_until`, `breaker_open`) per backend and key.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use super::AppState;
use crate::catalog::ModelState;
use crate::limits::headers::rfc3339;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/models", get(list))
}

async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(st.catalog.to_models_json(|m| {
        let guard = st.existing_guard(m.provider_name);
        let breaker_open = guard.as_ref().is_some_and(|g| g.breaker_open());
        ModelState {
            available: st.backends.ready(&st.keys, m).is_ok() && !breaker_open,
            throttled_until: guard
                .as_ref()
                .and_then(|g| g.throttled_until())
                .map(rfc3339),
            breaker_open,
        }
    }))
}
