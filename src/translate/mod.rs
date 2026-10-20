//! Translation between formats of one family (spec "Translation rules").
//!
//! - [`decide`]: systemone <-> OpenAI Decisions, requests and answers.
//! - [`errors`]: provider error bodies rendered in the client's format.
//! - [`fetch`]: image URLs fetched by s1 (only toward systemone).
//!
//! [`translator`] is the one place to register a pair of formats.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::Value;

use crate::error::S1Error;
use crate::protocol::ProtocolId;

pub mod decide;
pub mod errors;
pub mod fetch;

/// How question names map between formats, so answers return in request
/// order with the client's names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NameMap {
    /// Target-format name -> client name (`None` for an unnamed question).
    pub names: BTreeMap<String, Option<String>>,
    /// Target-format names in client request order.
    pub order: Vec<String>,
    /// Target-format name -> the client's score levels, lowest first, as the
    /// client wrote them (OpenAI `label`s, or systemone `criteria` items).
    /// Answers carry them back unchanged.
    pub levels: BTreeMap<String, Vec<Value>>,
}

impl NameMap {
    /// Add a question; `client` is `None` for an unnamed one.
    pub fn push(&mut self, target: &str, client: Option<String>) {
        self.names.insert(target.to_string(), client);
        self.order.push(target.to_string());
    }

    /// The client name of a target-format question.
    pub fn client(&self, target: &str) -> Option<&str> {
        self.names.get(target).and_then(|n| n.as_deref())
    }
}

/// A translated request plus the map needed to translate its response.
#[derive(Debug, Clone)]
pub struct Translated {
    pub body: Bytes,
    pub map: NameMap,
}

/// Only between formats of one family.
pub trait Translate: Send + Sync {
    /// A client request in `from` as a request in `to`.
    fn request(&self, from: ProtocolId, to: ProtocolId, body: &[u8])
    -> Result<Translated, S1Error>;
    /// A successful provider response in `from` as a response in `to`.
    fn response(
        &self,
        from: ProtocolId,
        to: ProtocolId,
        body: &[u8],
        map: &NameMap,
    ) -> Result<Bytes, S1Error>;
}

/// The translator between two formats, if this build has one.
pub fn translator(from: ProtocolId, to: ProtocolId) -> Option<&'static dyn Translate> {
    use ProtocolId::{OpenaiDecisions, Systemone};
    match (from, to) {
        (Systemone, OpenaiDecisions) | (OpenaiDecisions, Systemone) => Some(&decide::Decide),
        _ => None,
    }
}

/// True if a request in `from` can reach a backend that speaks `to`.
pub fn can(from: ProtocolId, to: ProtocolId) -> bool {
    from == to || translator(from, to).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_decide_formats_translate() {
        use ProtocolId::*;
        assert!(can(Systemone, OpenaiDecisions));
        assert!(can(OpenaiDecisions, Systemone));
        assert!(can(Clairic, Clairic));
        assert!(!can(Systemone, Clairic));
        assert!(!can(OpenaiDecisions, Tsfm));
        assert!(translator(Systemone, Systemone).is_none());
    }
}
