//! The Cloudflare Clairic format: `POST /v1/extract`. Canonical (and only)
//! format of the Extract family; a preview API pinned by the fixtures in
//! `tests/fixtures/clairic/`.
//!
//! `inspect` reads only what routing and the size checks need: `model`,
//! the number of `fields`, whether `state` is a string, and the images
//! (count and decoded size, from the base64 length; nothing is decoded).
//!
//! s1 refuses before the call what Clairic surely refuses (live, 2026-10-20):
//! a state that is not a string, an empty state without images, no
//! `fields`, and remote image URLs (`invalid_request`); more than
//! [`MAX_IMAGES`] images (`unsupported`); an image over [`MAX_IMAGE_BYTES`]
//! or images over [`MAX_IMAGES_TOTAL_BYTES`] in total (`too_long`).

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
use crate::flatten::col;

pub struct Clairic;

/// Most images in one request.
pub const MAX_IMAGES: u32 = 4;
/// Largest decoded image.
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest decoded size of all images together.
pub const MAX_IMAGES_TOTAL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    model: Cow<'a, str>,
    #[serde(default, borrow)]
    state: Option<&'a RawValue>,
    #[serde(default, borrow)]
    fields: Option<HashMap<Cow<'a, str>, IgnoredAny>>,
    #[serde(default)]
    images: Option<Vec<Image>>,
}

/// One `images` item: a data URL string or `{content_type, base64}`.
#[derive(Default)]
struct Image {
    /// Bytes of the payload in the request (the whole data URL, or the
    /// `base64` member).
    raw: usize,
    /// Decoded bytes, from the base64 length.
    decoded: usize,
    /// A string that is not a data URL (Clairic takes no remote URLs).
    remote: bool,
}

/// Decoded length of base64 text (padding and a missing tail allowed).
pub fn base64_decoded_len(b64: &str) -> usize {
    let b64 = b64.trim_end();
    let pad = b64.bytes().rev().take_while(|&b| b == b'=').count().min(2);
    (b64.len() * 3 / 4).saturating_sub(pad)
}

fn data_url(s: &str) -> Image {
    let is_data = s
        .as_bytes()
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"data:"));
    let payload = s.split_once(',').map_or("", |(_, p)| p);
    Image {
        raw: s.len(),
        decoded: base64_decoded_len(payload),
        remote: !is_data,
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Image;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a data URL or an object with `content_type` and `base64`")
            }
            fn visit_str<E>(self, s: &str) -> Result<Image, E> {
                Ok(data_url(s))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Image, A::Error> {
                let mut out = Image::default();
                while let Some(k) = m.next_key::<Cow<'de, str>>()? {
                    if k == "base64" {
                        let b: Cow<'de, str> = m.next_value()?;
                        out.raw = b.len();
                        out.decoded = base64_decoded_len(&b);
                    } else {
                        m.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(out)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Image, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Image::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// Answer types with one value and the scalar columns.
const SCALAR: [&str; 6] = ["text", "number", "date", "time", "datetime", "duration"];

impl Protocol for Clairic {
    fn id(&self) -> ProtocolId {
        ProtocolId::Clairic
    }

    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError> {
        let head: Head<'_> = serde_json::from_slice(body)
            .map_err(|e| InspectError(format!("cannot read Clairic request: {e}")))?;
        let fields = match &head.fields {
            Some(f) if !f.is_empty() => f.len(),
            _ => {
                return Err(InspectError(
                    "Clairic request needs a non-empty `fields` object".into(),
                ));
            }
        };
        let images = head.images.unwrap_or_default();
        if images.iter().any(|i| i.remote) {
            return Err(InspectError(
                "Clairic takes images only as embedded data URLs (`data:image/png;base64,...`) or `{content_type, base64}`, not remote URLs".into(),
            ));
        }
        match head.state.map(RawValue::get) {
            Some(s) if !s.starts_with('"') => {
                return Err(InspectError(
                    "Clairic takes only a string `state`; send structured data as JSON text".into(),
                ));
            }
            None | Some("\"\"") if images.is_empty() => {
                return Err(InspectError(
                    "Clairic needs a non-empty `state` when no images are sent".into(),
                ));
            }
            _ => {}
        }
        let image_bytes = images.iter().map(|i| i.raw).sum::<usize>();
        Ok(RouteInfo {
            model: head.model.into_owned(),
            input_bytes: body.len().saturating_sub(image_bytes),
            image_bytes,
            questions: u32::try_from(fields).unwrap_or(u32::MAX),
            images: u32::try_from(images.len()).unwrap_or(u32::MAX),
            largest_image_bytes: images.iter().map(|i| i.decoded).max().unwrap_or(0),
            images_decoded_bytes: images.iter().map(|i| i.decoded).sum(),
            ..RouteInfo::default()
        })
    }

    fn check(&self, info: &RouteInfo) -> Result<(), S1Error> {
        if info.images > MAX_IMAGES {
            return Err(S1Error::Unsupported(format!(
                "Clairic takes at most {MAX_IMAGES} images, not {}",
                info.images
            )));
        }
        if info.largest_image_bytes > MAX_IMAGE_BYTES {
            return Err(S1Error::TooLong(format!(
                "an image is {} bytes; Clairic takes at most {MAX_IMAGE_BYTES} bytes per image",
                info.largest_image_bytes
            )));
        }
        if info.images_decoded_bytes > MAX_IMAGES_TOTAL_BYTES {
            return Err(S1Error::TooLong(format!(
                "images are {} bytes in total; Clairic takes at most {MAX_IMAGES_TOTAL_BYTES}",
                info.images_decoded_bytes
            )));
        }
        Ok(())
    }

    /// Columns per field (spec "Flat columns"):
    /// scalar (`text`, `number`, `date`, `time`, `datetime`, `duration`)
    /// `f`, `f__conf`, `f__p_null`, `f__guess`, `f__span`;
    /// `estimate` and `forecast` `f`, `f__median`, `f__p10`, `f__p90`
    /// (arrays for a forecast); `list` `f`, `f__conf`, `f__spans`.
    /// Any other answer is kept whole as `f`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError> {
        let v: Value = serde_json::from_slice(resp)
            .map_err(|e| InspectError(format!("cannot read Clairic response: {e}")))?;
        let answers = v
            .get("answers")
            .and_then(Value::as_object)
            .ok_or_else(|| InspectError("Clairic response has no `answers` object".into()))?;
        let mut cols = Vec::new();
        for (name, a) in answers {
            let ty = a.get("type").and_then(Value::as_str).unwrap_or("");
            let field = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
            let suffixes: &[(&str, &str)] = match ty {
                t if SCALAR.contains(&t) => &[
                    ("conf", "confidence"),
                    ("p_null", "p_null"),
                    ("guess", "guess"),
                    ("span", "span"),
                ],
                "estimate" | "forecast" => &[("median", "median"), ("p10", "p10"), ("p90", "p90")],
                "list" => &[("conf", "confidence"), ("spans", "spans")],
                _ => {
                    cols.push((name.clone(), a.clone()));
                    continue;
                }
            };
            cols.push((name.clone(), field("value")));
            for (suffix, key) in suffixes {
                cols.push((col(name, suffix), field(key)));
            }
        }
        Ok(cols)
    }

    /// The Workers AI error envelope, with the s1 code:
    /// `{"result": {}, "success": false, "errors": [{"code", "message"}], "messages": []}`.
    fn error_body(&self, err: &S1Error) -> Bytes {
        Bytes::from(
            serde_json::json!({
                "result": {},
                "success": false,
                "errors": [{"code": err.code(), "message": err.to_string()}],
                "messages": [],
            })
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatten::{is_column_of, validate_names};
    use proptest::prelude::*;
    use serde_json::json;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/clairic/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// The `result` of a recorded envelope.
    fn result(name: &str) -> Vec<u8> {
        let v: Value = serde_json::from_slice(&fixture(name)).unwrap();
        assert_eq!(v["success"], true, "{name}");
        serde_json::to_vec(&v["result"]).unwrap()
    }

    fn inspect(v: Value) -> Result<RouteInfo, InspectError> {
        Clairic.inspect(&serde_json::to_vec(&v).unwrap())
    }

    #[test]
    fn inspect_reads_model_fields_and_state_size() {
        let body = fixture("invoice.request.json");
        let info = Clairic.inspect(&body).unwrap();
        assert_eq!(info.model, "clairic");
        assert_eq!(info.questions, 6);
        assert_eq!(info.input_bytes, body.len());
        assert_eq!((info.images, info.image_bytes), (0, 0));
        assert_eq!(Clairic.check(&info), Ok(()));
        assert_eq!(Clairic.family(), crate::protocol::Family::Extract);
    }

    #[test]
    fn inspect_counts_images_by_decoded_size() {
        let body = fixture("image.request.json");
        let info = Clairic.inspect(&body).unwrap();
        assert_eq!(info.images, 1);
        // The 1x1 PNG is 70 bytes.
        assert_eq!(info.largest_image_bytes, 70);
        assert_eq!(info.images_decoded_bytes, 70);
        assert_eq!(info.input_bytes, body.len() - info.image_bytes);

        let b64 = "A".repeat(4_000);
        let info = inspect(json!({
            "model": "extract",
            "state": "x",
            "fields": {"f": {"type": "text"}},
            "images": [format!("DATA:image/png;base64,{b64}"), {"content_type": "image/png", "base64": "AAAA"}]
        }))
        .unwrap();
        assert_eq!(info.images, 2);
        assert_eq!(info.largest_image_bytes, 3_000);
        assert_eq!(info.images_decoded_bytes, 3_003);
        assert!(info.input_bytes < 200, "{}", info.input_bytes);
        assert_eq!(info.image_tokens(), 2 * 1_027);
    }

    #[test]
    fn non_ascii_image_strings_are_remote_not_a_panic() {
        for s in ["ééé", "dataé", "daté:", "é"] {
            let err = inspect(json!({
                "model": "m",
                "state": "x",
                "fields": {"f": {"type": "text"}},
                "images": [s]
            }))
            .unwrap_err();
            assert!(err.0.contains("remote URLs"), "{s}: {}", err.0);
        }
    }

    #[test]
    fn base64_lengths() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("AAAA"), 3);
        assert_eq!(base64_decoded_len("AAA="), 2);
        assert_eq!(base64_decoded_len("AA=="), 1);
        assert_eq!(base64_decoded_len("AAA"), 2);
        assert_eq!(base64_decoded_len("AA"), 1);
    }

    #[test]
    fn inspect_refuses_what_clairic_refuses() {
        let fields = json!({"f": {"type": "text"}});
        for (body, needle) in [
            (
                json!({"model": "m", "state": {"a": 1}, "fields": fields}),
                "string",
            ),
            (
                json!({"model": "m", "state": ["a"], "fields": fields}),
                "string",
            ),
            (
                json!({"model": "m", "state": "", "fields": fields}),
                "non-empty `state`",
            ),
            (json!({"model": "m", "fields": fields}), "non-empty `state`"),
            (json!({"model": "m", "state": "x"}), "fields"),
            (json!({"model": "m", "state": "x", "fields": {}}), "fields"),
            (
                json!({"model": "m", "state": "x", "fields": fields, "images": ["https://example.com/a.png"]}),
                "remote",
            ),
            (json!({"state": "x", "fields": fields}), "model"),
        ] {
            let err = inspect(body.clone()).unwrap_err();
            assert!(err.0.contains(needle), "{body}: {err}");
        }
        assert!(Clairic.inspect(b"nope").is_err());
        // Images without state are fine.
        let ok = json!({"model": "m", "state": "", "fields": fields, "images": ["data:image/png;base64,AAAA"]});
        assert!(inspect(ok).is_ok());
    }

    #[test]
    fn check_enforces_image_limits() {
        let info = |images: u32, largest: usize, total: usize| RouteInfo {
            images,
            largest_image_bytes: largest,
            images_decoded_bytes: total,
            ..RouteInfo::default()
        };
        assert!(
            Clairic
                .check(&info(4, MAX_IMAGE_BYTES, 2 * MAX_IMAGE_BYTES))
                .is_ok()
        );
        assert_eq!(
            Clairic.check(&info(5, 10, 50)).unwrap_err().code(),
            "unsupported"
        );
        assert_eq!(
            Clairic
                .check(&info(1, MAX_IMAGE_BYTES + 1, MAX_IMAGE_BYTES + 1))
                .unwrap_err()
                .code(),
            "too_long"
        );
        assert_eq!(
            Clairic
                .check(&info(3, MAX_IMAGE_BYTES, MAX_IMAGES_TOTAL_BYTES + 1))
                .unwrap_err()
                .code(),
            "too_long"
        );
    }

    #[test]
    fn flatten_recorded_invoice() {
        let cols = Clairic.flatten(&result("invoice.response.json")).unwrap();
        let names: Vec<&str> = cols.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "invoice_number",
                "invoice_number__conf",
                "invoice_number__p_null",
                "invoice_number__guess",
                "invoice_number__span",
                "due_date",
                "due_date__conf",
                "due_date__p_null",
                "due_date__guess",
                "due_date__span",
                "total",
                "total__conf",
                "total__p_null",
                "total__guess",
                "total__span",
                "contacts",
                "contacts__conf",
                "contacts__spans",
                "po_number",
                "po_number__conf",
                "po_number__p_null",
                "po_number__guess",
                "po_number__span",
                "days_to_pay",
                "days_to_pay__median",
                "days_to_pay__p10",
                "days_to_pay__p90",
            ]
        );
        let m: serde_json::Map<String, Value> = cols.into_iter().collect();
        assert_eq!(m["invoice_number"], "INV-2291");
        assert_eq!(m["invoice_number__span"], json!([8, 16]));
        assert_eq!(m["due_date"], "2026-10-02");
        assert_eq!(m["due_date__span"], Value::Null);
        assert_eq!(m["total"], json!(1080));
        assert_eq!(m["contacts"], json!(["Dana Ruiz", "Priya Shah"]));
        assert_eq!(m["contacts__spans"], json!([[120, 129], [131, 141]]));
        // A null value still carries its guess.
        assert_eq!(m["po_number"], Value::Null);
        assert_eq!(m["po_number__guess"], "INV-2291");
        assert!(m["po_number__p_null"].as_f64().unwrap() > 0.99);
        assert_eq!(m["days_to_pay"], json!(31));
        assert_eq!(m["days_to_pay__median"], json!(31));
        assert_eq!(m["days_to_pay__p10"], json!(24));
        assert_eq!(m["days_to_pay__p90"], json!(37));
    }

    #[test]
    fn flatten_recorded_types_and_image() {
        let m: serde_json::Map<String, Value> = Clairic
            .flatten(&result("types.response.json"))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(m["start"], "14:30:00");
        assert_eq!(m["when"], "2026-10-21T14:30:00");
        assert_eq!(m["length"], json!(90));
        assert!(m.contains_key("length__conf"));
        // Forecast: passthrough of the per-step arrays.
        assert_eq!(m["sales_next"], json!([162, 172, 182]));
        assert_eq!(m["sales_next__p10"], json!([152, 161, 171]));
        assert_eq!(m["growth"], json!(10));
        assert_eq!(m["growth__p90"], json!(14));
        // No quantile or state_text columns.
        assert!(m.keys().all(|k| !k.contains("quantiles")));
        assert_eq!(m.len(), 3 * 5 + 2 * 4);

        let m: serde_json::Map<String, Value> = Clairic
            .flatten(&result("image.response.json"))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(m["color"], "Red");
        assert_eq!(m["color__span"], Value::Null);
    }

    #[test]
    fn recorded_responses_keep_the_preview_format() {
        // Top level and answer keys the flat columns and the cache rely on.
        for name in [
            "invoice.response.json",
            "types.response.json",
            "image.response.json",
            "truncated.response.json",
        ] {
            let v: Value = serde_json::from_slice(&result(name)).unwrap();
            assert_eq!(v["model"], "clairic", "{name}");
            assert!(v["truncated"].is_boolean(), "{name}");
            for k in ["input_tokens", "output_tokens", "passes", "state_tokens"] {
                assert!(v["usage"][k].is_u64(), "{name}: usage.{k}");
            }
            for (field, a) in v["answers"].as_object().unwrap() {
                assert!(a["type"].is_string(), "{name}: {field}");
                assert!(a.get("value").is_some(), "{name}: {field}");
            }
        }
        let t: Value = serde_json::from_slice(&result("truncated.response.json")).unwrap();
        assert_eq!(t["truncated"], true);
        let types: Value = serde_json::from_slice(&result("types.response.json")).unwrap();
        assert!(types["state_text"].is_string());
        assert_eq!(
            types["answers"]["growth"]["quantiles"]
                .as_array()
                .unwrap()
                .len(),
            99
        );
        assert_eq!(
            types["answers"]["sales_next"]["steps"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn flatten_keeps_unknown_answers_whole() {
        let m: serde_json::Map<String, Value> = Clairic
            .flatten(br#"{"answers":{"odd":{"type":"new","x":1}}}"#)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(m["odd"]["x"], 1);
        assert!(Clairic.flatten(b"{}").is_err());
    }

    #[test]
    fn error_body_uses_the_workers_ai_envelope() {
        let body = Clairic.error_body(&S1Error::TooLong("big".into()));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["errors"][0]["code"], "too_long");
        assert_eq!(v["errors"][0]["message"], "big");
    }

    fn answer(kind: u8) -> Value {
        match kind % 5 {
            0 => {
                json!({"type": "text", "value": "a", "guess": "a", "p_null": 0.1, "confidence": 0.9, "span": [0, 1]})
            }
            1 => json!({"type": "number", "value": 1, "p_null": 0.1, "confidence": 0.9}),
            2 => {
                json!({"type": "estimate", "value": 3, "mean": 3.1, "median": 3, "p10": 1, "p90": 5, "p_below": 0.0, "p_above": 0.0})
            }
            3 => json!({"type": "list", "value": ["a"], "spans": [[0, 1]], "confidence": 0.9}),
            _ => {
                json!({"type": "forecast", "value": [1, 2], "median": [1, 2], "p10": [0, 1], "p90": [2, 3], "steps": []})
            }
        }
    }

    proptest! {
        /// Flatten: column names are unique for any valid set of field names.
        #[test]
        fn flatten_column_names_are_unique(
            fields in proptest::collection::btree_map("[A-Za-z_][A-Za-z0-9_.-]{0,8}", any::<u8>(), 1..10),
        ) {
            let fields: Vec<_> = fields
                .into_iter()
                .filter(|(n, _)| validate_names([n.as_str()]).is_ok())
                .collect();
            let answers: serde_json::Map<String, Value> =
                fields.iter().map(|(n, k)| (n.clone(), answer(*k))).collect();
            let resp = json!({"model": "clairic", "answers": answers, "truncated": false, "usage": {}});
            let cols = Clairic.flatten(&serde_json::to_vec(&resp).unwrap()).unwrap();
            let mut seen = std::collections::HashSet::new();
            for (c, _) in &cols {
                prop_assert!(seen.insert(c.clone()), "duplicate column {}", c);
                prop_assert!(fields.iter().any(|(f, _)| is_column_of(c, f)));
            }
        }
    }
}
