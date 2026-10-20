//! The TypeSafe System One (Jev) format: `POST /v1/systemone`.
//! Canonical format for the Decide family.
//!
//! `inspect` reads only what routing needs with borrowed parsing: strings
//! are borrowed where possible, and `state`, instructions, and criteria
//! values are skipped (`IgnoredAny`) or counted, never built.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use bytes::Bytes;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_json::value::RawValue;

use super::{InspectError, Protocol, ProtocolId, RouteInfo};
use crate::error::S1Error;
use crate::flatten::{col, prob_col};

pub struct Systemone;

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    model: Cow<'a, str>,
    #[serde(default, borrow)]
    /// Raw, so each question's size is known; read as [`QuestionHead`].
    questions: Option<HashMap<Cow<'a, str>, &'a RawValue>>,
    #[serde(default)]
    assets: Option<Media>,
    /// Clef extension: an array of images.
    #[serde(default)]
    images: Option<Media>,
}

#[derive(Deserialize)]
struct QuestionHead<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Option<Cow<'a, str>>,
    /// `null` gives `None`; see [`Count`] for the flag.
    #[serde(default)]
    instructions: Option<Count>,
    #[serde(default)]
    criteria: Option<Count>,
}

/// Number of members of an object or array; 0 for anything else. The
/// flag is false for `null` and for a blank string (Clef rejects those
/// as `instructions`; live, 2026-10-20), true for anything else.
/// Values are skipped, not built.
struct Count(u32, bool);

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Count;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Count, A::Error> {
                let mut n = 0u32;
                while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                    n = n.saturating_add(1);
                }
                Ok(Count(n, true))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Count, A::Error> {
                let mut n = 0u32;
                while s.next_element::<IgnoredAny>()?.is_some() {
                    n = n.saturating_add(1);
                }
                Ok(Count(n, true))
            }
            fn visit_str<E>(self, s: &str) -> Result<Count, E> {
                Ok(Count(0, !s.trim().is_empty()))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Count, E> {
                Ok(Count(0, true))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Count, E> {
                Ok(Count(0, true))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Count, E> {
                Ok(Count(0, true))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Count, E> {
                Ok(Count(0, true))
            }
            fn visit_unit<E>(self) -> Result<Count, E> {
                Ok(Count(0, false))
            }
            fn visit_none<E>(self) -> Result<Count, E> {
                Ok(Count(0, false))
            }
        }
        d.deserialize_any(V)
    }
}

/// Image payloads: an `assets` map or an `images` array. Counts members
/// and sums the length of their base64 data; values are not built.
#[derive(Default)]
struct Media {
    count: u32,
    bytes: usize,
}

impl<'de> Deserialize<'de> for Media {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Media;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Media, A::Error> {
                let mut out = Media::default();
                while let Some((_, p)) = m.next_entry::<IgnoredAny, Payload>()? {
                    out.count = out.count.saturating_add(1);
                    out.bytes = out.bytes.saturating_add(p.0);
                }
                Ok(out)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Media, A::Error> {
                let mut out = Media::default();
                while let Some(p) = s.next_element::<Payload>()? {
                    out.count = out.count.saturating_add(1);
                    out.bytes = out.bytes.saturating_add(p.0);
                }
                Ok(out)
            }
            fn visit_str<E>(self, _: &str) -> Result<Media, E> {
                Ok(Media::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Media, E> {
                Ok(Media::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Media, E> {
                Ok(Media::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Media, E> {
                Ok(Media::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Media, E> {
                Ok(Media::default())
            }
            fn visit_unit<E>(self) -> Result<Media, E> {
                Ok(Media::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// Byte length of one image's data: a data URL string, or the
/// `data_base64` (systemone asset) or `base64` (Clef image) member of an
/// object. 0 for anything else.
struct Payload(usize);

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Payload;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, s: &str) -> Result<Payload, E> {
                Ok(Payload(s.len()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Payload, A::Error> {
                let mut n = 0usize;
                while let Some(k) = m.next_key::<Cow<'de, str>>()? {
                    if k == "data_base64" || k == "base64" {
                        n = n.saturating_add(m.next_value::<Payload>()?.0);
                    } else {
                        m.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Payload(n))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Payload, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Payload(0))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Payload, E> {
                Ok(Payload(0))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Payload, E> {
                Ok(Payload(0))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Payload, E> {
                Ok(Payload(0))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Payload, E> {
                Ok(Payload(0))
            }
            fn visit_unit<E>(self) -> Result<Payload, E> {
                Ok(Payload(0))
            }
        }
        d.deserialize_any(V)
    }
}

/// A question name Clef accepts: `[A-Za-z0-9_.-]`, 1 to 100 characters.
fn strict_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn widen(slot: &mut Option<u32>, n: u32, pick: fn(u32, u32) -> u32) {
    *slot = Some(slot.map_or(n, |s| pick(s, n)));
}

impl Protocol for Systemone {
    fn id(&self) -> ProtocolId {
        ProtocolId::Systemone
    }

    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError> {
        let head: Head<'_> = serde_json::from_slice(body)
            .map_err(|e| InspectError(format!("cannot read systemone request: {e}")))?;
        let assets = head.assets.unwrap_or_default();
        let images = head.images.unwrap_or_default();
        // Images cost tokens per image, not per base64 byte.
        let image_bytes = assets.bytes.saturating_add(images.bytes);
        let mut info = RouteInfo {
            model: head.model.into_owned(),
            input_bytes: body.len().saturating_sub(image_bytes),
            image_bytes,
            assets: assets.count,
            images: images.count,
            ..RouteInfo::default()
        };
        for (name, raw) in head.questions.iter().flatten() {
            let q: QuestionHead<'_> = serde_json::from_str(raw.get())
                .map_err(|e| InspectError(format!("cannot read question `{name}`: {e}")))?;
            info.questions += 1;
            let len = raw.get().len();
            info.questions_bytes = info.questions_bytes.saturating_add(len);
            info.max_question_bytes = info.max_question_bytes.max(len);
            if !q.instructions.as_ref().is_some_and(|i| i.1) {
                info.missing_instructions = true;
            }
            if !strict_name(name) {
                info.loose_question_names = true;
            }
            let n = q.criteria.as_ref().map_or(0, |c| c.0);
            match q.ty.as_deref() {
                Some("choice") => {
                    widen(&mut info.max_options, n, u32::max);
                    widen(&mut info.min_options, n, u32::min);
                }
                Some("score") => {
                    widen(&mut info.max_levels, n, u32::max);
                    widen(&mut info.min_levels, n, u32::min);
                }
                Some("box") => info.has_box = true,
                _ => {}
            }
        }
        Ok(info)
    }

    /// Columns per answer (spec "Flat columns"):
    /// choice `q`, `q__conf`, `q__p__<option>`; score `q`, `q__conf`,
    /// `q__p__<level>`; noul `q`; box `q__top|bottom|left|right`, `q__conf`.
    /// Any other answer (for example a per-question error) is kept whole as `q`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError> {
        let v: Value = serde_json::from_slice(resp)
            .map_err(|e| InspectError(format!("cannot read systemone response: {e}")))?;
        let answers = v
            .get("answers")
            .and_then(Value::as_object)
            .ok_or_else(|| InspectError("systemone response has no `answers` object".into()))?;
        let mut cols = Vec::new();
        for (name, a) in answers {
            let ty = a.get("type").and_then(Value::as_str).unwrap_or("");
            let field = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
            match ty {
                "noul" => cols.push((name.clone(), field("noul"))),
                "choice" | "score" => {
                    cols.push((name.clone(), field(ty)));
                    cols.push((col(name, "conf"), field("confidence")));
                    if let Some(p) = a.get("probabilities").and_then(Value::as_object) {
                        for (opt, prob) in p {
                            cols.push((prob_col(name, opt), prob.clone()));
                        }
                    }
                }
                "box" => {
                    let b = a.get("box");
                    for side in ["top", "bottom", "left", "right"] {
                        let x = b.and_then(|b| b.get(side)).cloned().unwrap_or(Value::Null);
                        cols.push((col(name, side), x));
                    }
                    cols.push((col(name, "conf"), field("confidence")));
                }
                _ => cols.push((name.clone(), a.clone())),
            }
        }
        Ok(cols)
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
    use crate::flatten::validate_names;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn inspect_reads_model_only() {
        let body = br#"{"model":"jev-latest","state":{"a":[1,2,{"b":null}]},"questions":{"billing":{"type":"noul","instructions":"Is this message about billing?"}}}"#;
        let info = Systemone.inspect(body).unwrap();
        assert_eq!(info.model, "jev-latest");
        assert_eq!(info.input_bytes, body.len());
        assert_eq!(info.questions, 1);
        assert_eq!(info.max_options, None);
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
    fn inspect_counts_options_levels_images_and_box() {
        let body = json!({
            "model": "decide",
            "state": "photo-1",
            "questions": {
                "tone": {"type": "choice", "criteria": {"a": "A", "b": null, "c": {"x": 1}}},
                "pick": {"type": "choice", "criteria": {"y": "Y"}},
                "urgency": {"type": "score", "criteria": ["low", "mid", "high", "max"]},
                "where": {"type": "box", "instructions": "Find it."},
                "yes": {"type": "noul"}
            },
            "assets": {"photo-1": {"type": "image", "format": "png", "data_base64": "AA=="}},
            "images": ["data:image/png;base64,AA==", "data:image/png;base64,AA=="]
        });
        let info = Systemone
            .inspect(serde_json::to_vec(&body).unwrap().as_slice())
            .unwrap();
        assert_eq!(info.questions, 5);
        assert_eq!((info.max_options, info.min_options), (Some(3), Some(1)));
        assert_eq!((info.max_levels, info.min_levels), (Some(4), Some(4)));
        assert_eq!(info.assets, 1);
        assert_eq!(info.images, 2);
        assert!(info.has_box);
    }

    #[test]
    fn inspect_leaves_image_payloads_out_of_input_bytes() {
        let big = "A".repeat(2_000_000);
        let body = json!({
            "model": "decide",
            "state": "photo-1",
            "questions": {"q": {"type": "noul", "instructions": "Red?"}},
            "assets": {"photo-1": {"type": "image", "format": "jpeg", "data_base64": big}},
            "images": [
                format!("data:image/jpeg;base64,{big}"),
                {"content_type": "image/png", "base64": big},
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let info = Systemone.inspect(&bytes).unwrap();
        let data_url = "data:image/jpeg;base64,".len() + big.len();
        assert_eq!(info.image_bytes, 2 * big.len() + data_url);
        assert_eq!(info.input_bytes, bytes.len() - info.image_bytes);
        assert!(info.input_bytes < 400, "{}", info.input_bytes);
        assert_eq!((info.assets, info.images), (1, 2));
        // Three images at most 1,027 tokens each, plus the text.
        assert_eq!(info.image_tokens(), 3 * 1_027);
        assert!(info.tokens_high() < 3 * 1_027 + 200);
        assert!(info.tokens_low() < 100);
    }
    #[test]
    fn inspect_notes_missing_instructions_and_loose_names() {
        let check = |v: Value| Systemone.inspect(&serde_json::to_vec(&v).unwrap()).unwrap();
        let ok = check(json!({"model": "m", "state": "x", "questions": {
            "a.b-c_1": {"type": "noul", "instructions": "Q?"},
            "o": {"type": "noul", "instructions": {"q": "Q?", "data": [1]}},
            "p": {"type": "noul", "instructions": ["Q?"]},
            "n": {"type": "noul", "instructions": 3},
            "e": {"type": "noul", "instructions": {}}
        }}));
        assert!(!ok.missing_instructions && !ok.loose_question_names);
        for q in [
            json!({"type": "noul"}),
            json!({"type": "noul", "instructions": null}),
            json!({"type": "noul", "instructions": ""}),
            json!({"type": "noul", "instructions": " \n "}),
        ] {
            let i = check(json!({"model": "m", "state": "x", "questions": {"q": q}}));
            assert!(i.missing_instructions, "{q}");
        }
        for name in ["has space", "ünï", &"x".repeat(101), ""] {
            let i = check(json!({"model": "m", "state": "x", "questions": {
                name: {"type": "noul", "instructions": "Q?"}
            }}));
            assert!(i.loose_question_names, "{name}");
        }
        let i = check(json!({"model": "m", "state": "x", "questions": {
            "x".repeat(100): {"type": "noul", "instructions": "Q?"}
        }}));
        assert!(!i.loose_question_names);
    }

    #[test]
    fn inspect_measures_questions() {
        let body = br#"{"model":"m","state":"abc","questions":{"a":{"type":"noul","instructions":"Q?"},"b":{"type":"noul","instructions":"Longer question?"}}}"#;
        let info = Systemone.inspect(body).unwrap();
        let a = r#"{"type":"noul","instructions":"Q?"}"#.len();
        let b = r#"{"type":"noul","instructions":"Longer question?"}"#.len();
        assert_eq!(info.max_question_bytes, b);
        assert_eq!(info.questions_bytes, a + b);
    }

    #[test]
    fn error_body_uses_typesafe_shape() {
        let body = Systemone.error_body(&S1Error::BackendUnavailable("down".into()));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["detail"]["error_type"], "backend_unavailable");
        assert_eq!(v["detail"]["message"], "down");
    }

    #[test]
    fn flatten_follows_the_spec_table() {
        // Recorded Clef response (envelope removed) for the doc example.
        let resp = br#"{"model":"clef-flash","answers":{"urgent":{"type":"noul","noul":0.9351},"team":{"type":"choice","choice":"technical","probabilities":{"billing":0.0651,"technical":0.9131,"sales":0.0218},"confidence":0.7576},"severity":{"type":"score","score":2.681,"legend":{"0":"No impact","1":"Minor","2":"Major","3":"Critical"},"probabilities":{"0":0.0189,"1":0.018,"2":0.2263,"3":0.7368},"confidence":0.4597}},"usage":{"input_tokens":346,"output_tokens":0}}"#;
        let cols = Systemone.flatten(resp).unwrap();
        let names: Vec<&str> = cols.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "urgent",
                "team",
                "team__conf",
                "team__p__billing",
                "team__p__technical",
                "team__p__sales",
                "severity",
                "severity__conf",
                "severity__p__0",
                "severity__p__1",
                "severity__p__2",
                "severity__p__3",
            ]
        );
        let m: serde_json::Map<String, Value> = cols.into_iter().collect();
        assert_eq!(m["urgent"], json!(0.9351));
        assert_eq!(m["team"], json!("technical"));
        assert_eq!(m["team__conf"], json!(0.7576));
        assert_eq!(m["severity"], json!(2.681));
        assert_eq!(m["severity__p__3"], json!(0.7368));
    }

    #[test]
    fn flatten_recorded_jev_response() {
        let path = format!(
            "{}/tests/fixtures/typesafe/triage.response.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let resp = std::fs::read(path).unwrap();
        let m: serde_json::Map<String, Value> =
            Systemone.flatten(&resp).unwrap().into_iter().collect();
        assert_eq!(m["urgent"], json!(0.97));
        assert_eq!(m["team"], json!("technical"));
        assert_eq!(m["team__conf"], json!(1.0));
        assert_eq!(m["severity"], json!(2.99));
        assert_eq!(m["severity__p__3"], json!(0.99));
        assert_eq!(m.len(), 12);
    }

    #[test]
    fn flatten_box_and_unknown_answers() {
        let resp = br#"{"model":"jev-preview","answers":{"red":{"type":"box","box":{"top":0.2,"bottom":0.66,"left":0.19,"right":0.68},"confidence":null},"odd":{"type":"error","error":{"code":"refusal"}}},"usage":{"input_tokens":285,"output_tokens":20}}"#;
        let m: serde_json::Map<String, Value> =
            Systemone.flatten(resp).unwrap().into_iter().collect();
        assert_eq!(m["red__top"], json!(0.2));
        assert_eq!(m["red__right"], json!(0.68));
        assert_eq!(m["red__conf"], Value::Null);
        assert_eq!(m["odd"]["error"]["code"], "refusal");
        assert!(Systemone.flatten(b"{}").is_err());
    }

    fn answer(kind: u8, options: &[String]) -> Value {
        match kind % 4 {
            0 => json!({"type": "noul", "noul": 0.5}),
            1 => {
                let p: serde_json::Map<String, Value> =
                    options.iter().map(|o| (o.clone(), json!(0.1))).collect();
                json!({"type": "choice", "choice": options[0], "confidence": 0.3, "probabilities": p})
            }
            2 => {
                let p: serde_json::Map<String, Value> = (0..options.len())
                    .map(|i| (i.to_string(), json!(0.1)))
                    .collect();
                json!({"type": "score", "score": 1.0, "confidence": 0.3, "legend": {}, "probabilities": p})
            }
            _ => {
                json!({"type": "box", "box": {"top": 0, "bottom": 1, "left": 0, "right": 1}, "confidence": null})
            }
        }
    }

    proptest! {
        /// Flatten: column names are unique for any valid set of question
        /// names (no `__`) and any option names.
        #[test]
        fn flatten_column_names_are_unique(
            questions in proptest::collection::btree_map(
                "[a-z_]{1,6}",
                (any::<u8>(), proptest::collection::btree_set("[a-z_]{0,6}", 1..5)),
                1..8,
            ),
        ) {
            let questions: Vec<_> = questions
                .into_iter()
                .filter(|(n, _)| validate_names([n.as_str()]).is_ok())
                .collect();
            let mut answers = serde_json::Map::new();
            for (name, (kind, opts)) in &questions {
                let opts: Vec<String> = opts.iter().cloned().collect();
                answers.insert(name.clone(), answer(*kind, &opts));
            }
            let resp = json!({"model": "m", "answers": answers, "usage": {}});
            let cols = Systemone.flatten(&serde_json::to_vec(&resp).unwrap()).unwrap();
            let mut seen = std::collections::HashSet::new();
            for (name, _) in &cols {
                prop_assert!(seen.insert(name.clone()), "duplicate column {}", name);
            }
            // Every column belongs to a question.
            for (c, _) in &cols {
                prop_assert!(questions.iter().any(|(q, _)| crate::flatten::is_column_of(c, q)));
            }
        }
    }
}
