//! The OpenAI Decisions format: `POST /v1/decisions`.
//!
//! Request: `model`, `input` (a string, or user messages with `input_text`
//! and `input_image` parts), `questions` (an ordered list; `name` optional;
//! `predicate`, `choice` with `choices`, `score` with `levels`). Response:
//! `answers` in question order; any answer may be `type: refusal`.
//!
//! `inspect` reads only what routing needs with borrowed parsing; the text
//! of the input and choices is skipped, and image data is counted, never
//! built. Question names and instructions are read for the Clef limits.

use std::borrow::Cow;
use std::fmt;

use bytes::Bytes;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use super::{InspectError, Protocol, ProtocolId, RouteInfo};
use crate::error::S1Error;
use crate::flatten::{col, prob_col};
use crate::translate::decide::{BOOL_FALSE, BOOL_TRUE};
use crate::translate::errors::openai_type_of;

pub struct OpenaiDecisions;

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    model: Cow<'a, str>,
    #[serde(default)]
    input: Option<Input>,
    #[serde(default)]
    questions: Option<Vec<QuestionHead<'a>>>,
}

#[derive(Deserialize)]
struct QuestionHead<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(default)]
    name: Option<Value>,
    #[serde(default)]
    instructions: Option<Value>,
    #[serde(default)]
    choices: Option<Len>,
    #[serde(default)]
    levels: Option<Len>,
}

/// Length of a list; values are skipped.
struct Len(u32);

impl<'de> Deserialize<'de> for Len {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Len;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Len, A::Error> {
                let mut n = 0u32;
                while s.next_element::<IgnoredAny>()?.is_some() {
                    n = n.saturating_add(1);
                }
                Ok(Len(n))
            }
        }
        d.deserialize_seq(V)
    }
}

/// Images in an `input`: their count and the bytes of their URLs.
#[derive(Default)]
struct Input {
    images: u32,
    image_bytes: usize,
}

impl Input {
    fn add(&mut self, other: Input) {
        self.images = self.images.saturating_add(other.images);
        self.image_bytes = self.image_bytes.saturating_add(other.image_bytes);
    }
}

/// Walks any JSON value; every object member named `image_url` with a
/// string value is one image.
impl<'de> Deserialize<'de> for Input {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Input;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Input, A::Error> {
                let mut out = Input::default();
                while let Some(i) = s.next_element::<Input>()? {
                    out.add(i);
                }
                Ok(out)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Input, A::Error> {
                let mut out = Input::default();
                while let Some(k) = m.next_key::<Cow<'de, str>>()? {
                    if k == "image_url" {
                        let url: StrLen = m.next_value()?;
                        out.images = out.images.saturating_add(1);
                        out.image_bytes = out.image_bytes.saturating_add(url.0);
                    } else {
                        out.add(m.next_value::<Input>()?);
                    }
                }
                Ok(out)
            }
            fn visit_str<E>(self, _: &str) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_unit<E>(self) -> Result<Input, E> {
                Ok(Input::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// Length of a string, read without copying it.
struct StrLen(usize);

impl<'de> Deserialize<'de> for StrLen {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrLen;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E>(self, s: &str) -> Result<StrLen, E> {
                Ok(StrLen(s.len()))
            }
        }
        d.deserialize_str(V)
    }
}

fn widen(slot: &mut Option<u32>, n: u32, pick: fn(u32, u32) -> u32) {
    *slot = Some(slot.map_or(n, |s| pick(s, n)));
}

/// Column name of a question: its `name`, or `q<index>` when unnamed (the
/// same names a systemone backend sees).
pub fn question_column(i: usize, name: Option<&str>) -> String {
    name.map_or_else(|| format!("q{i}"), str::to_string)
}

/// Column text of a choice value, one-to-one: booleans use the systemone
/// encoding (`bool:true`, `bool:false`), other non-strings are
/// `json:<compact JSON>`, and a string is itself unless it starts with a
/// reserved prefix (`bool:`, `json:`, `str:`), then it is `str:<string>`.
/// So `"true"` and `true`, or `"1"` and `1`, get different columns.
fn value_column(v: &Value) -> String {
    match v {
        Value::Bool(true) => BOOL_TRUE.into(),
        Value::Bool(false) => BOOL_FALSE.into(),
        Value::String(s) if ["bool:", "json:", "str:"].iter().any(|p| s.starts_with(p)) => {
            format!("str:{s}")
        }
        Value::String(s) => s.clone(),
        other => format!("json:{other}"),
    }
}

impl Protocol for OpenaiDecisions {
    fn id(&self) -> ProtocolId {
        ProtocolId::OpenaiDecisions
    }

    fn inspect(&self, body: &[u8]) -> Result<RouteInfo, InspectError> {
        let head: Head<'_> = serde_json::from_slice(body)
            .map_err(|e| InspectError(format!("cannot read OpenAI Decisions request: {e}")))?;
        let input = head.input.unwrap_or_default();
        // Images become systemone `assets` when translated; a native OpenAI
        // backend takes them as they are.
        let mut info = RouteInfo {
            model: head.model.into_owned(),
            input_bytes: body.len().saturating_sub(input.image_bytes),
            image_bytes: input.image_bytes,
            assets: input.images,
            ..RouteInfo::default()
        };
        // Clef limits, read from the questions as a systemone backend
        // would get them.
        let translated: Vec<Value> = head
            .questions
            .iter()
            .flatten()
            .map(|q| json!({"type": q.ty, "name": q.name, "instructions": q.instructions}))
            .collect();
        (info.missing_instructions, info.loose_question_names) =
            crate::translate::decide::systemone_question_facts(&translated);
        for q in head.questions.iter().flatten() {
            info.questions += 1;
            match q.ty.as_deref() {
                Some("choice") => {
                    let n = q.choices.as_ref().map_or(0, |c| c.0);
                    widen(&mut info.max_options, n, u32::max);
                    widen(&mut info.min_options, n, u32::min);
                }
                Some("score") => {
                    let n = q.levels.as_ref().map_or(0, |c| c.0);
                    widen(&mut info.max_levels, n, u32::max);
                    widen(&mut info.min_levels, n, u32::min);
                }
                _ => {}
            }
        }
        Ok(info)
    }

    /// Columns per answer (spec "Flat columns"): predicate `q`; choice `q`,
    /// `q__conf`, `q__p__<value>`; score `q`, `q__conf`, `q__p__<level>`.
    /// Unnamed questions are `q<index>`. A refusal is kept whole as `q`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>, InspectError> {
        let v: Value = serde_json::from_slice(resp)
            .map_err(|e| InspectError(format!("cannot read OpenAI Decisions response: {e}")))?;
        let answers = v.get("answers").and_then(Value::as_array).ok_or_else(|| {
            InspectError("OpenAI Decisions response has no `answers` list".into())
        })?;
        let mut cols = Vec::new();
        for (i, a) in answers.iter().enumerate() {
            let name = question_column(i, a.get("name").and_then(Value::as_str));
            let field = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
            match a.get("type").and_then(Value::as_str).unwrap_or("") {
                "predicate" => cols.push((name, field("probability"))),
                ty @ ("choice" | "score") => {
                    cols.push((name.clone(), field(ty)));
                    cols.push((col(&name, "conf"), field("confidence")));
                    for p in a
                        .get("probabilities")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let v = p.get("value").unwrap_or(&Value::Null);
                        // Score values are level indexes: `q__p__<level>`.
                        let key = match v {
                            Value::Number(n) if ty == "score" => n.to_string(),
                            _ => value_column(v),
                        };
                        cols.push((prob_col(&name, &key), field_of(p, "probability")));
                    }
                }
                _ => cols.push((name, a.clone())),
            }
        }
        Ok(cols)
    }

    /// OpenAI error shape: `{"error": {"message", "type", "param", "code"}}`
    /// with the s1 code in `code`.
    fn error_body(&self, err: &S1Error) -> Bytes {
        Bytes::from(
            json!({
                "error": {
                    "message": err.to_string(),
                    "type": openai_type_of(err),
                    "param": null,
                    "code": err.code(),
                }
            })
            .to_string(),
        )
    }
}

fn field_of(v: &Value, k: &str) -> Value {
    v.get(k).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatten::validate_names;
    use proptest::prelude::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn inspect_recorded_request() {
        let body = fixture("openai/triage.request.json");
        let info = OpenaiDecisions.inspect(&body).unwrap();
        assert_eq!(info.model, "gpt-6-luna");
        assert_eq!(info.questions, 3);
        assert_eq!((info.max_options, info.min_options), (Some(3), Some(3)));
        assert_eq!((info.max_levels, info.min_levels), (Some(3), Some(3)));
        assert_eq!(info.assets, 0);
        assert_eq!(info.input_bytes, body.len());
    }

    // Review R2-4.2: the Clef limits come from the questions as
    // translated to systemone.
    #[test]
    fn inspect_notes_missing_instructions_and_loose_names() {
        let check = |qs: Value| {
            let body = json!({"model": "decide", "input": "x", "questions": qs});
            OpenaiDecisions
                .inspect(&serde_json::to_vec(&body).unwrap())
                .unwrap()
        };
        let ok = check(json!([
            {"type": "predicate", "name": "a.b-c_1", "instructions": "Q?"},
            {"type": "predicate", "instructions": "Q?\n\nTrue if: rain"},
            {"type": "choice", "instructions": "Q?", "choices": [{"value": "a"}]},
            {"type": "score", "name": "x".repeat(100), "instructions": "Q?", "levels": [{"label": "l"}]}
        ]));
        assert!(!ok.missing_instructions && !ok.loose_question_names);
        for q in [
            json!({"type": "predicate"}),
            json!({"type": "predicate", "instructions": null}),
            json!({"type": "predicate", "instructions": " "}),
            json!({"type": "predicate", "instructions": "True if: rain"}),
            json!({"type": "predicate", "instructions": "False if: sun"}),
            json!({"type": "choice", "choices": [{"value": "a"}]}),
            json!({"type": "score", "instructions": "", "levels": [{"label": "l"}]}),
        ] {
            let i = check(json!([q]));
            assert!(i.missing_instructions, "{q}");
            assert!(!i.loose_question_names, "{q}");
        }
        for name in ["a b", "ünï", &"x".repeat(101), ""] {
            let i = check(json!([{"type": "predicate", "name": name, "instructions": "Q?"}]));
            assert!(i.loose_question_names, "{name}");
        }
    }

    #[test]
    fn inspect_counts_images_per_image() {
        let big = "A".repeat(1_000_000);
        let body = serde_json::to_vec(&json!({
            "model": "decide",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Look."},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{big}")},
                {"type": "input_image", "image_url": "https://example.com/a.jpg", "detail": "low"}
            ]}],
            "questions": [{"type": "predicate", "instructions": "Damaged?"}]
        }))
        .unwrap();
        let info = OpenaiDecisions.inspect(&body).unwrap();
        assert_eq!(info.assets, 2);
        assert!(info.input_bytes < 400, "{}", info.input_bytes);
        assert_eq!(info.image_tokens(), 2 * 1_027);
    }

    #[test]
    fn inspect_rejects_missing_model() {
        assert!(OpenaiDecisions.inspect(br#"{"input":"x"}"#).is_err());
        assert!(OpenaiDecisions.inspect(b"[]").is_err());
    }

    #[test]
    fn error_body_uses_openai_shape_with_s1_code() {
        let b = OpenaiDecisions.error_body(&S1Error::QueueFull {
            message: "full".into(),
            retry_after_secs: 2,
        });
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(
            v,
            json!({"error": {"message": "full", "type": "rate_limit_error", "param": null, "code": "queue_full"}})
        );
        let v: Value =
            serde_json::from_slice(&OpenaiDecisions.error_body(&S1Error::Unsupported("no".into())))
                .unwrap();
        assert_eq!(v["error"]["code"], "unsupported");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn flatten_recorded_response() {
        let cols = OpenaiDecisions
            .flatten(&fixture("openai/triage.response.json"))
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "billing",
                "q1",
                "q1__conf",
                "q1__p__billing",
                "q1__p__technical",
                "q1__p__bool:true",
                "urgency",
                "urgency__conf",
                "urgency__p__0",
                "urgency__p__1",
                "urgency__p__2",
            ]
        );
        let m: serde_json::Map<String, Value> = cols.into_iter().collect();
        assert_eq!(m["billing"], json!(1.0));
        assert_eq!(m["q1"], json!("billing"));
        assert_eq!(m["urgency"], json!(1.19));
        let refusal = OpenaiDecisions
            .flatten(&fixture("openai/refusal.response.json"))
            .unwrap();
        assert_eq!(refusal[0].0, "q0");
        assert_eq!(refusal[0].1["type"], "refusal");
        let typed = OpenaiDecisions
            .flatten(&fixture("openai/typed_choice.response.json"))
            .unwrap();
        let m: serde_json::Map<String, Value> = typed.into_iter().collect();
        assert_eq!(m["q0__p__true"], json!(0.52));
        assert_eq!(m["q0__p__bool:true"], json!(0.48));
    }

    // Review B7: typed choice values get distinct columns.
    #[test]
    fn typed_choice_value_columns_are_injective() {
        let values = [
            json!(true),
            json!("bool:true"),
            json!(false),
            json!("bool:false"),
            json!(1),
            json!("1"),
            json!(null),
            json!("null"),
            json!("json:1"),
            json!("str:x"),
            json!("x"),
        ];
        let probs: Vec<Value> = values
            .iter()
            .map(|v| json!({"value": v, "probability": 0.1}))
            .collect();
        let resp = json!({"model": "m", "answers": [{"type": "choice", "name": "c",
            "choice": true, "confidence": 0.5, "probabilities": probs}]});
        let cols = OpenaiDecisions
            .flatten(&serde_json::to_vec(&resp).unwrap())
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|(k, _)| k.as_str()).skip(2).collect();
        assert_eq!(
            names,
            [
                "c__p__bool:true",
                "c__p__str:bool:true",
                "c__p__bool:false",
                "c__p__str:bool:false",
                "c__p__json:1",
                "c__p__1",
                "c__p__json:null",
                "c__p__null",
                "c__p__str:json:1",
                "c__p__str:str:x",
                "c__p__x",
            ]
        );
    }

    proptest! {
        /// Column names are unique for any valid set of question names.
        #[test]
        fn flatten_column_names_are_unique(
            names in proptest::collection::btree_set("[a-z_]{1,6}", 1..6),
            unnamed in 0usize..3,
            picks in proptest::collection::btree_set(0usize..14, 1..14),
            extra in "(bool:|json:|str:)?[a-z0-9:]{0,6}",
        ) {
            // Distinct choice values of any JSON type, including strings
            // that look like the reserved column prefixes.
            let pool = [
                json!(true), json!(false), json!("true"), json!("bool:true"),
                json!("bool:false"), json!(1), json!("1"), json!(1.5), json!(null),
                json!("null"), json!("json:1"), json!("str:bool:true"), json!(""),
                json!("str:"),
            ];
            let mut values: Vec<Value> = picks.iter().map(|&i| pool[i].clone()).collect();
            if !values.contains(&json!(extra)) {
                values.push(json!(extra));
            }
            let probs: Vec<Value> = values
                .iter()
                .map(|v| json!({"value": v, "probability": 0.1}))
                .collect();
            let names: Vec<String> = names
                .into_iter()
                .filter(|n| validate_names([n.as_str()]).is_ok() && !n.starts_with('q'))
                .collect();
            let mut answers: Vec<Value> = names
                .iter()
                .map(|n| json!({"type": "choice", "name": n, "choice": true, "confidence": 0.5,
                    "probabilities": probs}))
                .collect();
            for _ in 0..unnamed {
                answers.push(json!({"type": "predicate", "name": null, "probability": 0.1}));
            }
            let resp = json!({"model": "m", "answers": answers});
            let cols = OpenaiDecisions.flatten(&serde_json::to_vec(&resp).unwrap()).unwrap();
            let mut seen = std::collections::HashSet::new();
            for (c, _) in &cols {
                prop_assert!(seen.insert(c.clone()), "duplicate column {}", c);
            }
        }
    }
}
