//! `POST /cache/clear`: remove every cached response (`s1 util cache clear`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/cache/clear", post(clear))
}

async fn clear(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let Some(cache) = st.cache.clone() else {
        return (
            StatusCode::OK,
            Json(json!({"cleared": 0, "enabled": false})),
        );
    };
    match tokio::task::spawn_blocking(move || cache.clear()).await {
        Ok(Ok(before)) => {
            tracing::info!(
                event = "cache_clear",
                entries = before.entries,
                bytes = before.bytes,
                "cache cleared"
            );
            (
                StatusCode::OK,
                Json(json!({"cleared": before.entries, "bytes": before.bytes, "enabled": true})),
            )
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": "internal", "message": e.to_string()}})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": "internal", "message": e.to_string()}})),
        ),
    }
}
