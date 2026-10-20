//! Translation between formats of one family (stage 4: OpenAI Decisions and
//! systemone). Stage 1 defines the trait only.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::error::S1Error;
use crate::protocol::ProtocolId;

/// How question names map between formats, so answers return in request
/// order with the client's names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameMap {
    /// Target-format name -> client name (`None` for an unnamed question).
    pub names: BTreeMap<String, Option<String>>,
    /// Target-format names in client request order.
    pub order: Vec<String>,
}

/// A translated request plus the map needed to translate its response.
#[derive(Debug, Clone)]
pub struct Translated {
    pub body: Bytes,
    pub map: NameMap,
}

/// Only between formats of one family.
pub trait Translate: Send + Sync {
    fn request(&self, from: ProtocolId, to: ProtocolId, body: &[u8])
    -> Result<Translated, S1Error>;
    fn response(
        &self,
        from: ProtocolId,
        to: ProtocolId,
        body: &[u8],
        map: &NameMap,
    ) -> Result<Bytes, S1Error>;
}
