//! `GET /status`: per backend and key, queue depth per lane, current rate,
//! in-flight cap, limits, `throttled_until`, and breaker state; plus cache
//! size. Keys appear only as a hash prefix (`k_…`).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/status", get(status))
}

async fn status(State(st): State<AppState>) -> Json<Value> {
    // Every provider with a backend and a key, so idle backends show too.
    let providers: Vec<String> = st.catalog.providers.keys().cloned().collect();
    let backends: Vec<Value> = providers
        .iter()
        .filter(|p| st.backends.get(p).is_some())
        .filter_map(|p| st.guard(p))
        .map(|g| serde_json::to_value(g.snapshot()).expect("snapshot serializes"))
        .collect();
    let cache = st.cache.as_ref().map(|c| c.stats());
    Json(json!({
        "pid": std::process::id(),
        "backends": backends,
        "cache": cache,
    }))
}
