//! Wire formats. One module per format; [`by_id`] is the one place to
//! register a new one.

use std::fmt;
use std::str::FromStr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::S1Error;

pub mod systemone;

/// Model family. The endpoint fixes the family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Decide,
    Extract,
    Forecast,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Decide => "decide",
            Family::Extract => "extract",
            Family::Forecast => "forecast",
        }
    }
}

/// A wire format name, as used in the catalog `api` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolId {
    #[serde(rename = "systemone")]
    Systemone,
    #[serde(rename = "openai-decisions")]
    OpenaiDecisions,
    #[serde(rename = "clairic")]
    Clairic,
    #[serde(rename = "tsfm")]
    Tsfm,
}

impl ProtocolId {
    pub const ALL: [ProtocolId; 4] = [
        ProtocolId::Systemone,
        ProtocolId::OpenaiDecisions,
        ProtocolId::Clairic,
        ProtocolId::Tsfm,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProtocolId::Systemone => "systemone",
            ProtocolId::OpenaiDecisions => "openai-decisions",
            ProtocolId::Clairic => "clairic",
            ProtocolId::Tsfm => "tsfm",
        }
    }

    pub fn family(self) -> Family {
        match self {
            ProtocolId::Systemone | ProtocolId::OpenaiDecisions => Family::Decide,
            ProtocolId::Clairic => Family::Extract,
            ProtocolId::Tsfm => Family::Forecast,
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProtocolId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ProtocolId::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unknown protocol `{s}`"))
    }
}

/// What routing needs from a request. Later stages add fields
/// (tokens, images, option counts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteInfo {
    /// The `model` field as sent (an id or an alias).
    pub model: String,
    /// Request size in bytes.
    pub input_bytes: usize,
}

/// A request body the protocol cannot read.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct InspectError(pub String);

/// One wire format (systemone, openai-decisions, clairic, tsfm).
pub trait Protocol: Send + Sync {
    fn id(&self) -> ProtocolId;
    fn family(&self) -> Family {
        self.id().family()
    }
    /// Read only what routing needs; keep the body as bytes.
    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError>;
    /// Turn a response into flat columns for `--flat`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError>;
    /// Render an s1 error in this format.
    fn error_body(&self, err: &S1Error) -> Bytes;
}

/// The protocol implementation for an id, if this build has one.
pub fn by_id(id: ProtocolId) -> Option<&'static dyn Protocol> {
    match id {
        ProtocolId::Systemone => Some(&systemone::Systemone),
        _ => None,
    }
}

/// Fallback error body for formats without an implementation yet.
pub fn generic_error_body(err: &S1Error) -> Bytes {
    Bytes::from(
        serde_json::json!({"error": {"code": err.code(), "message": err.to_string()}}).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_map_to_families() {
        for id in ProtocolId::ALL {
            assert_eq!(id.as_str().parse::<ProtocolId>().unwrap(), id);
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, format!("\"{}\"", id.as_str()));
        }
        assert_eq!(ProtocolId::OpenaiDecisions.family(), Family::Decide);
        assert_eq!(ProtocolId::Clairic.family(), Family::Extract);
        assert_eq!(ProtocolId::Tsfm.family(), Family::Forecast);
        assert!("nope".parse::<ProtocolId>().is_err());
    }
}
