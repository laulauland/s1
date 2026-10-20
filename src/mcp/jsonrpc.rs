//! JSON-RPC 2.0 messages as MCP 2026-07-28 uses them over Streamable HTTP:
//! one request or notification per POST, never a batch.

use axum::http::StatusCode;
use serde_json::{Map, Value, json};

/// Invalid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// Not a JSON-RPC request object (also batches and client responses).
pub const INVALID_REQUEST: i64 = -32600;
/// Unknown method (HTTP 404).
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Missing `_meta` fields, unknown tool (spec "Statelessness", "Tools").
pub const INVALID_PARAMS: i64 = -32602;
/// MCP: a required header is missing or differs from the body.
pub const HEADER_MISMATCH: i64 = -32020;
/// MCP: the request names a protocol version this server lacks.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// A JSON-RPC error plus the HTTP status it goes with.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub http: StatusCode,
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(http: StatusCode, code: i64, message: impl Into<String>) -> Self {
        Self {
            http,
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, PARSE_ERROR, msg)
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, INVALID_REQUEST, msg)
    }

    pub fn header(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, HEADER_MISMATCH, msg)
    }

    /// The JSON-RPC error object for request `id`.
    pub fn to_message(&self, id: &Value) -> Value {
        let mut err = json!({"code": self.code, "message": self.message});
        if let Some(d) = &self.data {
            err["data"] = d.clone();
        }
        json!({"jsonrpc": "2.0", "id": id, "error": err})
    }
}

/// What one POST body holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Map<String, Value>,
    },
    Notification {
        method: String,
    },
}

fn reject(id: Value, e: RpcError) -> Box<(Value, RpcError)> {
    Box::new((id, e))
}

/// Read one JSON-RPC message. `Err` carries the request id when it is known.
pub fn parse(body: &[u8]) -> Result<Incoming, Box<(Value, RpcError)>> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| reject(Value::Null, RpcError::parse(format!("parse error: {e}"))))?;
    let Value::Object(mut obj) = v else {
        let msg = if v.is_array() {
            "batches are not supported; send one JSON-RPC message per POST"
        } else {
            "a JSON-RPC message must be an object"
        };
        return Err(reject(Value::Null, RpcError::invalid_request(msg)));
    };
    let id = obj.remove("id");
    if let Some(id) = &id
        && !(id.is_string() || id.is_i64() || id.is_u64())
    {
        return Err(reject(
            Value::Null,
            RpcError::invalid_request("`id` must be a string or an integer"),
        ));
    }
    let id_or_null = id.clone().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(reject(
            id_or_null,
            RpcError::invalid_request("`jsonrpc` must be \"2.0\""),
        ));
    }
    let Some(method) = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let msg = if obj.contains_key("result") || obj.contains_key("error") {
            "this server sends no requests, so it takes no responses"
        } else {
            "`method` must be a string"
        };
        return Err(reject(id_or_null, RpcError::invalid_request(msg)));
    };
    let params = match obj.remove("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(reject(
                id_or_null,
                RpcError::new(
                    StatusCode::BAD_REQUEST,
                    INVALID_PARAMS,
                    "`params` must be an object",
                ),
            ));
        }
    };
    Ok(match id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method },
    })
}

/// A JSON-RPC success message.
pub fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A `notifications/progress` message.
pub fn progress(token: &Value, progress: u64, total: u64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {
            "progressToken": token,
            "progress": progress,
            "total": total,
            "message": message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications() {
        let r = parse(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"x"}}"#)
            .unwrap();
        let Incoming::Request { id, method, params } = r else {
            panic!("request expected")
        };
        assert_eq!(id, json!(1));
        assert_eq!(method, "tools/list");
        assert_eq!(params["cursor"], "x");
        assert_eq!(
            parse(br#"{"jsonrpc":"2.0","method":"notifications/x"}"#).unwrap(),
            Incoming::Notification {
                method: "notifications/x".into()
            }
        );
    }

    #[test]
    fn rejects_bad_messages() {
        let cases: [(&[u8], i64); 7] = [
            (b"{", PARSE_ERROR),
            (b"[]", INVALID_REQUEST),
            (b"3", INVALID_REQUEST),
            (
                br#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#,
                INVALID_REQUEST,
            ),
            (br#"{"jsonrpc":"1.0","id":1,"method":"m"}"#, INVALID_REQUEST),
            (br#"{"jsonrpc":"2.0","id":1,"result":{}}"#, INVALID_REQUEST),
            (
                br#"{"jsonrpc":"2.0","id":1,"method":"m","params":[1]}"#,
                INVALID_PARAMS,
            ),
        ];
        for (body, code) in cases {
            let (_, e) = *parse(body).unwrap_err();
            assert_eq!(e.code, code, "{}", String::from_utf8_lossy(body));
            assert_eq!(e.http, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn error_messages_carry_data() {
        let e = RpcError::new(
            StatusCode::BAD_REQUEST,
            UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version",
        )
        .with_data(json!({"supported": ["2026-07-28"], "requested": "1900-01-01"}));
        let m = e.to_message(&json!(1));
        assert_eq!(m["error"]["code"], -32022);
        assert_eq!(m["error"]["data"]["requested"], "1900-01-01");
        assert_eq!(m["id"], 1);
    }
}
