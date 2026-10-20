//! The TypeSafe System One (Jev) format: `POST /v1/systemone`.
//! Canonical format for the Decide family.

use std::borrow::Cow;

use bytes::Bytes;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::{InspectError, Protocol, ProtocolId, RouteInfo};
use crate::error::S1Error;

pub struct Systemone;

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    model: Cow<'a, str>,
    #[serde(flatten)]
    _rest: std::collections::HashMap<Cow<'a, str>, IgnoredAny>,
}

impl Protocol for Systemone {
    fn id(&self) -> ProtocolId {
        ProtocolId::Systemone
    }

    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError> {
        let head: Head<'_> = serde_json::from_slice(body)
            .map_err(|e| InspectError(format!("cannot read systemone request: {e}")))?;
        Ok(RouteInfo {
            model: head.model.into_owned(),
            input_bytes: body.len(),
        })
    }

    fn flatten(&self, _resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError> {
        // Implemented with `--flat` in stage 2.
        Err(InspectError("flatten is not implemented yet".into()))
    }

    /// TypeSafe error shape: `{"detail": {"error_type": ..., "message": ...}}`.
    fn error_body(&self, err: &S1Error) -> Bytes {
        Bytes::from(
            serde_json::json!({
                "detail": {"error_type": err.code(), "message": err.to_string()}
            })
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_reads_model_only() {
        let body = br#"{"model":"jev-latest","state":{"a":[1,2,{"b":null}]},"questions":{"billing":{"type":"noul","instructions":"Is this message about billing?"}}}"#;
        let info = Systemone.inspect(body).unwrap();
        assert_eq!(info.model, "jev-latest");
        assert_eq!(info.input_bytes, body.len());
    }

    #[test]
    fn inspect_handles_escaped_model() {
        let info = Systemone
            .inspect(br#"{"state":"x","model":"jev-latest"}"#)
            .unwrap();
        assert_eq!(info.model, "jev-latest");
    }

    #[test]
    fn inspect_rejects_missing_model_and_bad_json() {
        assert!(Systemone.inspect(br#"{"state":"x"}"#).is_err());
        assert!(Systemone.inspect(b"not json").is_err());
    }

    #[test]
    fn error_body_uses_typesafe_shape() {
        let body = Systemone.error_body(&S1Error::BackendUnavailable("down".into()));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["detail"]["error_type"], "backend_unavailable");
        assert_eq!(v["detail"]["message"], "down");
    }
}
