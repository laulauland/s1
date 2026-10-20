//! Provider error bodies in the client's format, so each ingress returns
//! errors in its own format (spec "Errors"). The HTTP status is kept.
//!
//! - systemone: `{"detail": {"error_type", "message"}}` (TypeSafe; also the
//!   shape s1 gives Workers AI errors), or FastAPI's `{"detail": [...]}`.
//! - OpenAI: `{"error": {"message", "type", "param", "code"}}`.

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::error::S1Error;
use crate::protocol::ProtocolId;

/// What an error body says, format-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// Most specific code (`error.code`, else `error.type`, else `detail.error_type`).
    pub code: Option<String>,
    /// Error class (`error.type` or `detail.error_type`).
    pub kind: Option<String>,
    pub message: String,
    pub param: Option<String>,
}

/// OpenAI `error.type` for an HTTP status.
pub fn openai_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 => "service_unavailable_error",
        400..=499 => "invalid_request_error",
        _ => "server_error",
    }
}

/// OpenAI `error.type` for an s1 error.
pub fn openai_type_of(err: &S1Error) -> &'static str {
    openai_type(err.status())
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

/// Read an error body in either format.
pub fn read(status: StatusCode, body: &[u8]) -> ProviderError {
    let v: Option<Value> = serde_json::from_slice(body).ok();
    let fallback = || ProviderError {
        code: None,
        kind: None,
        message: format!(
            "provider returned HTTP {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&body[..body.len().min(500)]).trim()
        ),
        param: None,
    };
    let Some(v) = v else {
        return fallback();
    };
    if let Some(e) = v.get("error").filter(|e| e.is_object()) {
        let kind = str_of(e.get("type"));
        return ProviderError {
            code: str_of(e.get("code")).or_else(|| kind.clone()),
            kind,
            message: str_of(e.get("message")).unwrap_or_default(),
            param: str_of(e.get("param")),
        };
    }
    match v.get("detail") {
        Some(Value::Object(d)) => {
            let kind = str_of(d.get("error_type"));
            ProviderError {
                code: kind.clone(),
                kind,
                message: str_of(d.get("message")).unwrap_or_default(),
                param: None,
            }
        }
        Some(Value::Array(items)) => {
            let message = items
                .iter()
                .map(|i| {
                    let loc = i
                        .get("loc")
                        .and_then(Value::as_array)
                        .map(|l| {
                            l.iter()
                                .map(|p| match p {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(".")
                        })
                        .unwrap_or_default();
                    let msg = i.get("msg").and_then(Value::as_str).unwrap_or("");
                    if loc.is_empty() {
                        msg.to_string()
                    } else {
                        format!("{loc}: {msg}")
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            ProviderError {
                code: Some("invalid_request_error".into()),
                kind: Some("invalid_request_error".into()),
                message,
                param: None,
            }
        }
        Some(Value::String(s)) => ProviderError {
            code: None,
            kind: None,
            message: s.clone(),
            param: None,
        },
        _ => fallback(),
    }
}

/// Render an error in `format`.
pub fn render(format: ProtocolId, status: StatusCode, e: &ProviderError) -> Bytes {
    let v = match format {
        ProtocolId::OpenaiDecisions => json!({
            "error": {
                "message": e.message,
                "type": e.kind.clone().unwrap_or_else(|| openai_type(status).to_string()),
                "param": e.param,
                "code": e.code,
            }
        }),
        _ => json!({
            "detail": {
                "error_type": e.code.clone().or_else(|| e.kind.clone()).unwrap_or_else(|| openai_type(status).to_string()),
                "message": e.message,
            }
        }),
    };
    Bytes::from(v.to_string())
}

/// A provider error body (in any Decide format) as a body in `to`.
pub fn translate(to: ProtocolId, status: StatusCode, body: &[u8]) -> Bytes {
    render(to, status, &read(status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn json(b: &[u8]) -> Value {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn typesafe_errors_in_openai_shape() {
        let out = translate(
            ProtocolId::OpenaiDecisions,
            StatusCode::UNAUTHORIZED,
            &fixture("typesafe/invalid_key.401.json"),
        );
        let v = json(&out);
        assert_eq!(v["error"]["type"], "authentication_error");
        assert_eq!(v["error"]["code"], "authentication_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Cannot authenticate")
        );
        assert_eq!(v["error"]["param"], Value::Null);
        let validation =
            br#"{"detail":[{"loc":["body","state"],"msg":"Field required","type":"missing"}]}"#;
        let v = json(&translate(
            ProtocolId::OpenaiDecisions,
            StatusCode::UNPROCESSABLE_ENTITY,
            validation,
        ));
        assert_eq!(v["error"]["message"], "body.state: Field required");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn openai_errors_in_systemone_shape() {
        let v = json(&translate(
            ProtocolId::Systemone,
            StatusCode::BAD_REQUEST,
            &fixture("openai/one_choice.400.json"),
        ));
        assert_eq!(v["detail"]["error_type"], "array_below_min_length");
        assert!(
            v["detail"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Invalid 'questions[0].choices'")
        );
        let v = json(&translate(
            ProtocolId::Systemone,
            StatusCode::BAD_GATEWAY,
            b"<html>bad gateway</html>",
        ));
        assert_eq!(v["detail"]["error_type"], "server_error");
        assert!(v["detail"]["message"].as_str().unwrap().contains("502"));
    }
}
