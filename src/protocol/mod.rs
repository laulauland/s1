//! Wire formats. One module per format; [`by_id`] is the one place to
//! register a new one.

use std::fmt;
use std::str::FromStr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::S1Error;

pub mod clairic;
pub mod openai_decisions;
pub mod systemone;
pub mod tsfm;

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

/// What routing needs from a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteInfo {
    /// The `model` field as sent (an id or an alias).
    pub model: String,
    /// Request size in bytes, without image payloads (see `image_bytes`).
    pub input_bytes: usize,
    /// Bytes of image payloads (systemone `assets.*.data_base64`, Clef
    /// and Clairic `images` items). Images are counted per image, not per byte.
    pub image_bytes: usize,
    /// Number of questions (or fields).
    pub questions: u32,
    /// Bytes of all question bodies, and of the largest one (systemone;
    /// 0 if not measured). For per-question context limits.
    pub questions_bytes: usize,
    pub max_question_bytes: usize,
    /// Most and fewest options over all choice questions (`None`: no choice question).
    pub max_options: Option<u32>,
    pub min_options: Option<u32>,
    /// Most and fewest levels over all score questions (`None`: no score question).
    pub max_levels: Option<u32>,
    pub min_levels: Option<u32>,
    /// Image assets referenced by placeholders (systemone `assets`).
    pub assets: u32,
    /// Images sent as an array (Clef `images` extension, Clairic `images`).
    pub images: u32,
    /// Images a systemone request with `assets` becomes toward OpenAI
    /// Decisions: one per asset reference plus `images`, counted up to one
    /// over the OpenAI limit (`None`: not measured, or no `assets`). See
    /// [`RouteInfo::toward`].
    pub openai_images: Option<u32>,
    /// A question type that only some models have (systemone `box`).
    pub has_box: bool,
    /// A question whose `instructions` are missing, null, or a blank
    /// string. Clef rejects those.
    pub missing_instructions: bool,
    /// A question name outside `[A-Za-z0-9_.-]`, 1 to 100 characters
    /// (the names Clef accepts).
    pub loose_question_names: bool,
    /// Decoded bytes of the largest image (Clairic `images`; 0 if not counted).
    pub largest_image_bytes: usize,
    /// Decoded bytes of all images (Clairic `images`; 0 if not counted).
    pub images_decoded_bytes: usize,
}

/// Bytes per token for a conservative estimate (real text with JSON
/// overhead measured about 3.2 bytes per token on Clef).
pub const BYTES_PER_TOKEN_SAFE: usize = 3;
/// Bytes per token that providers use for their own pre-check
/// (Workers AI rejects when `bytes / 4` exceeds the context).
pub const BYTES_PER_TOKEN_LENIENT: usize = 4;

/// Most input tokens one image costs. Clef: images are resized to at most
/// about 1 MP, `(w / 32) x (h / 32)` tokens capped at 1,024, plus 3 marker
/// tokens. Jev bills images as input tokens too; s1 uses the same cap.
pub const TOKENS_PER_IMAGE_MAX: u64 = 1_024 + 3;

impl RouteInfo {
    /// The facts as a backend that speaks `api` gets the request: toward
    /// OpenAI Decisions each asset reference is one image (spec
    /// "Translation rules"), so a systemone asset referenced many times
    /// costs, and counts against context, once per reference.
    pub fn toward(&self, api: ProtocolId) -> std::borrow::Cow<'_, RouteInfo> {
        match self.openai_images {
            Some(n) if api == ProtocolId::OpenaiDecisions => std::borrow::Cow::Owned(RouteInfo {
                assets: n.saturating_sub(self.images),
                ..self.clone()
            }),
            _ => std::borrow::Cow::Borrowed(self),
        }
    }
    /// Input tokens of the images, at most [`TOKENS_PER_IMAGE_MAX`] each.
    pub fn image_tokens(&self) -> u64 {
        u64::from(self.assets.saturating_add(self.images)) * TOKENS_PER_IMAGE_MAX
    }
    /// Input tokens, estimated high (used to choose among alias entries).
    /// Text bytes plus the image cap for each image.
    pub fn tokens_high(&self) -> u64 {
        self.input_bytes.div_ceil(BYTES_PER_TOKEN_SAFE) as u64 + self.image_tokens()
    }
    /// Input tokens one question sees, estimated high: the request
    /// without the other questions (state, images, and the largest
    /// question). Jev limits this per question.
    pub fn question_tokens_high(&self) -> u64 {
        let bytes = self
            .input_bytes
            .saturating_sub(self.questions_bytes)
            .saturating_add(self.max_question_bytes);
        bytes.div_ceil(BYTES_PER_TOKEN_SAFE) as u64 + self.image_tokens()
    }
    /// Input tokens, estimated low (a request above this surely fails).
    /// Text bytes only: an image can cost as little as 64 tokens, and the
    /// Workers AI pre-check does not count image bytes (live, 2026-10-20).
    pub fn tokens_low(&self) -> u64 {
        (self.input_bytes / BYTES_PER_TOKEN_LENIENT) as u64
    }
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
    /// Limits of the format itself, checked after `inspect` on a typed
    /// ingress (for example Clairic's image count and sizes).
    fn check(&self, _info: &RouteInfo) -> Result<(), S1Error> {
        Ok(())
    }
    /// Turn a response into flat columns for `--flat`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError>;
    /// Render an s1 error in this format.
    fn error_body(&self, err: &S1Error) -> Bytes;
}

/// The protocol implementation for an id, if this build has one.
pub fn by_id(id: ProtocolId) -> Option<&'static dyn Protocol> {
    match id {
        ProtocolId::Systemone => Some(&systemone::Systemone),
        ProtocolId::OpenaiDecisions => Some(&openai_decisions::OpenaiDecisions),
        ProtocolId::Clairic => Some(&clairic::Clairic),
        ProtocolId::Tsfm => Some(&tsfm::Tsfm),
    }
}

/// The top-level `model` string of a JSON object body, read without
/// looking at any other member. `None` when the body is not an object or
/// has no string `model`.
pub fn top_level_model(body: &[u8]) -> Option<String> {
    let members: TopLevel<'_> = serde_json::from_slice(body).ok()?;
    let (_, v) = members.0.iter().find(|(k, _)| k == "model")?;
    serde_json::from_str::<String>(v.get()).ok()
}

/// Set the top-level `model` field of a JSON object body to `model`.
///
/// The other members keep their exact bytes (only the whitespace between
/// top-level members may change). Returns the body unchanged when the
/// field already has this value.
pub fn replace_model(body: &Bytes, model: &str) -> Result<Bytes, InspectError> {
    use serde_json::value::RawValue;
    let members: TopLevel<'_> = serde_json::from_slice(body)
        .map_err(|e| InspectError(format!("request is not a JSON object: {e}")))?;
    let current = members.0.iter().find(|(k, _)| k == "model");
    if let Some((_, v)) = current
        && serde_json::from_str::<&str>(v.get()).ok() == Some(model)
    {
        return Ok(body.clone());
    }
    let model_json = serde_json::to_string(model).expect("string serializes");
    let mut out = Vec::with_capacity(body.len() + model.len());
    out.push(b'{');
    let mut first = true;
    let mut push = |out: &mut Vec<u8>, k: &str, v: &str| {
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend_from_slice(serde_json::to_string(k).expect("key").as_bytes());
        out.push(b':');
        out.extend_from_slice(v.as_bytes());
    };
    if current.is_none() {
        push(&mut out, "model", &model_json);
    }
    for (k, v) in &members.0 {
        let v: &RawValue = v;
        if k == "model" {
            push(&mut out, k, &model_json);
        } else {
            push(&mut out, k, v.get());
        }
    }
    out.push(b'}');
    Ok(Bytes::from(out))
}

/// Top-level members of a JSON object, in order, values unparsed.
pub(crate) struct TopLevel<'a>(
    pub(crate) Vec<(std::borrow::Cow<'a, str>, &'a serde_json::value::RawValue)>,
);

impl<'de: 'a, 'a> Deserialize<'de> for TopLevel<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = TopLevel<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(k) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    let v: &'de serde_json::value::RawValue = map.next_value()?;
                    out.push((k, v));
                }
                Ok(TopLevel(out))
            }
        }
        d.deserialize_map(V(std::marker::PhantomData))
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
    use proptest::prelude::*;

    #[test]
    fn top_level_model_ignores_other_members() {
        let body = br#"{"questions":{"q":null},"model":"clef","state":7}"#;
        assert_eq!(top_level_model(body).as_deref(), Some("clef"));
        assert_eq!(top_level_model(br#"{"model":3}"#), None);
        assert_eq!(top_level_model(br#"{"state":"x"}"#), None);
        assert_eq!(top_level_model(b"not json"), None);
    }

    #[test]
    fn replace_model_keeps_other_bytes() {
        let body = Bytes::from_static(
            br#"{"state": {"a" : [1, 2]}, "model":"decide","questions":{"q":{"type":"noul"}}}"#,
        );
        let out = replace_model(&body, "jev-latest").unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"state":{"a" : [1, 2]},"model":"jev-latest","questions":{"q":{"type":"noul"}}}"#
        );
        // Same value: bytes unchanged.
        let same = Bytes::from_static(br#"{ "model" : "clef" , "state":"x"}"#);
        assert_eq!(replace_model(&same, "clef").unwrap(), same);
        // Missing model: added first.
        let none = Bytes::from_static(br#"{"state":"x"}"#);
        assert_eq!(
            replace_model(&none, "clef").unwrap().as_ref(),
            br#"{"model":"clef","state":"x"}"#
        );
        assert!(replace_model(&Bytes::from_static(b"[1]"), "clef").is_err());
    }

    proptest! {
        #[test]
        fn replace_model_changes_only_the_model(
            state in "[ -~]{0,40}",
            from in "[a-z/-]{1,12}",
            to in "[a-z/-]{1,12}",
        ) {
            let v = serde_json::json!({"state": state, "model": from, "questions": {"q": {"type": "noul"}}});
            let body = Bytes::from(serde_json::to_vec(&v).unwrap());
            let out = replace_model(&body, &to).unwrap();
            let mut expected = v.clone();
            expected["model"] = Value::String(to.clone());
            let got: Value = serde_json::from_slice(&out).unwrap();
            prop_assert_eq!(got, expected);
        }
    }

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
