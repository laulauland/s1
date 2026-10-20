//! Bearer-token check for the TCP listener. The Unix socket needs none.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use crate::secrets::Secret;

/// Wrap a router so every request needs `Authorization: Bearer <s1-local>`.
pub fn require_bearer(app: Router, token: Secret) -> Router {
    app.layer(middleware::from_fn_with_state(token, check))
}

async fn check(State(token): State<Secret>, req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(given.as_bytes(), token.expose().as_bytes()) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(unauthorized_body(req.uri().path())),
    )
        .into_response()
}

/// The 401 body in the format of the endpoint's ingress (spec: errors in
/// each ingress's own format), else the generic s1 envelope.
fn unauthorized_body(path: &str) -> serde_json::Value {
    const CODE: &str = "unauthorized";
    const MESSAGE: &str = "missing or wrong bearer token; see `s1 auth token`";
    match path {
        "/v1/systemone" => serde_json::json!({
            "detail": {"error_type": CODE, "message": MESSAGE}
        }),
        "/v1/decisions" => serde_json::json!({
            "error": {
                "message": MESSAGE,
                "type": "authentication_error",
                "param": null,
                "code": CODE
            }
        }),
        _ => serde_json::json!({"error": {"code": CODE, "message": MESSAGE}}),
    }
}

/// Constant-time compare (`subtle`); unequal lengths are unequal.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn compares_tokens() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
