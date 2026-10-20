//! Cache key (spec "Cache key"):
//! `blake3(family, resolved model version, canonical JSON of the request
//! without the model field)`.
//!
//! Canonical JSON: object keys sorted (by their UTF-8 bytes), no
//! insignificant whitespace, strings re-escaped the one way `serde_json`
//! writes them, numbers exactly as given. The JSON is streamed into the
//! hasher, never built as a string. Image paths are expanded into inline
//! data before the key is made, so the key covers the image content.

use std::borrow::Cow;

use serde_json::value::RawValue;

use crate::protocol::Family;

/// A cache key: 32 bytes of blake3.
pub type Key = [u8; 32];

/// Bumped when the key layout changes, so old entries stop matching.
const KEY_VERSION: &[u8] = b"s1-cache-key-v1";

/// Deepest nesting the key accepts (serde_json's own default limit).
pub const MAX_DEPTH: usize = 128;

/// Where canonical JSON goes.
pub trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl Sink for blake3::Hasher {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

/// Write `raw` as canonical JSON. `skip` names top-level keys to leave out.
///
/// One pass over the (already validated) text builds a borrowed tree, with
/// nesting deeper than [`MAX_DEPTH`] refused before it is entered, so a
/// deeply nested member (such as `state`, which inspection does not read)
/// cannot overflow the stack or make the work grow with depth.
pub fn write_canonical(raw: &RawValue, skip: &[&str], out: &mut impl Sink) -> Result<(), String> {
    let mut scan = Scan {
        text: raw.get(),
        at: 0,
    };
    let mut node = scan.value(0)?;
    scan.space();
    if scan.at != scan.text.len() {
        return Err("trailing characters after JSON value".into());
    }
    if let Node::Object(members) = &mut node {
        members.retain(|(k, _)| !skip.contains(&k.as_ref()));
    }
    node.write(out);
    Ok(())
}

/// A JSON value borrowed from the request text.
enum Node<'a> {
    /// Members in key order (sorted once parsed).
    Object(Vec<(Cow<'a, str>, Node<'a>)>),
    Array(Vec<Node<'a>>),
    String(Cow<'a, str>),
    /// Numbers, booleans, null: exactly as given.
    Literal(&'a str),
}

impl Node<'_> {
    fn write(&self, out: &mut impl Sink) {
        match self {
            Node::Object(members) => {
                out.put(b"{");
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.put(b",");
                    }
                    write_string(k, out);
                    out.put(b":");
                    v.write(out);
                }
                out.put(b"}");
            }
            Node::Array(items) => {
                out.put(b"[");
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.put(b",");
                    }
                    v.write(out);
                }
                out.put(b"]");
            }
            Node::String(s) => write_string(s, out),
            Node::Literal(t) => out.put(t.as_bytes()),
        }
    }
}

/// A recursive-descent reader over JSON text. Recursion is bounded by
/// [`MAX_DEPTH`].
struct Scan<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Scan<'a> {
    fn space(&mut self) {
        let b = self.text.as_bytes();
        while self.at < b.len() && matches!(b[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.text.as_bytes().get(self.at).copied()
    }

    fn expect(&mut self, want: u8) -> Result<(), String> {
        match self.peek() {
            Some(b) if b == want => {
                self.at += 1;
                Ok(())
            }
            _ => Err(format!("expected `{}` at byte {}", want as char, self.at)),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node<'a>, String> {
        match self.peek() {
            Some(b'{') | Some(b'[') if depth >= MAX_DEPTH => Err(format!(
                "JSON nested too deep (more than {MAX_DEPTH} levels)"
            )),
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                } else {
                    loop {
                        let k = self.string()?;
                        self.expect(b':')?;
                        members.push((k, self.value(depth + 1)?));
                        match self.peek() {
                            Some(b',') => self.at += 1,
                            Some(b'}') => {
                                self.at += 1;
                                break;
                            }
                            _ => return Err(format!("expected `,` or `}}` at byte {}", self.at)),
                        }
                    }
                }
                members.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                Ok(Node::Object(members))
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                if self.peek() == Some(b']') {
                    self.at += 1;
                } else {
                    loop {
                        items.push(self.value(depth + 1)?);
                        match self.peek() {
                            Some(b',') => self.at += 1,
                            Some(b']') => {
                                self.at += 1;
                                break;
                            }
                            _ => return Err(format!("expected `,` or `]` at byte {}", self.at)),
                        }
                    }
                }
                Ok(Node::Array(items))
            }
            Some(b'"') => Ok(Node::String(self.string()?)),
            Some(_) => {
                let b = self.text.as_bytes();
                let start = self.at;
                while self.at < b.len()
                    && !matches!(
                        b[self.at],
                        b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    self.at += 1;
                }
                Ok(Node::Literal(&self.text[start..self.at]))
            }
            None => Err("empty JSON value".into()),
        }
    }

    /// A string token, unescaped by serde_json (the slice is only the token,
    /// so the work stays linear).
    fn string(&mut self) -> Result<Cow<'a, str>, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("expected a string at byte {}", self.at));
        }
        let b = self.text.as_bytes();
        let start = self.at;
        let mut i = start + 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        if i >= b.len() {
            return Err("unterminated string".into());
        }
        self.at = i + 1;
        serde_json::from_str(&self.text[start..self.at]).map_err(|e| e.to_string())
    }
}

fn write_string(s: &str, out: &mut impl Sink) {
    out.put(
        serde_json::to_string(s)
            .expect("strings serialize")
            .as_bytes(),
    );
}

/// Canonical JSON text of a body (for tests and debugging).
pub fn canonical(body: &[u8], skip: &[&str]) -> Result<String, String> {
    let raw: &RawValue = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    write_canonical(raw, skip, &mut out)?;
    Ok(String::from_utf8(out).expect("canonical JSON is UTF-8"))
}

/// The key of a request body. `extra` holds request facts outside the body
/// that change the answer (for example the long-context opt-in).
pub fn key(
    family: Family,
    version: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> Result<Key, String> {
    let raw: &RawValue = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut h = blake3::Hasher::new();
    for part in [KEY_VERSION, family.as_str().as_bytes(), version.as_bytes()] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    for (k, v) in extra {
        h.update(&(k.len() as u64).to_le_bytes());
        h.update(k.as_bytes());
        h.update(&(v.len() as u64).to_le_bytes());
        h.update(v.as_bytes());
    }
    h.update(b"\0body\0");
    write_canonical(raw, &["model"], &mut h)?;
    Ok(*h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Map, Value, json};

    #[test]
    fn canonical_sorts_keys_and_keeps_numbers() {
        let body =
            r#"{ "z": 1.50, "model": "decide", "a": {"y": [1e3, true, null], "b": "caf\u00e9"} }"#;
        assert_eq!(
            canonical(body.as_bytes(), &["model"]).unwrap(),
            r#"{"a":{"b":"café","y":[1e3,true,null]},"z":1.50}"#
        );
    }

    #[test]
    fn model_and_family_and_version_matter_as_specified() {
        let a = br#"{"model":"decide","state":"x","questions":{"q":{"type":"noul"}}}"#;
        let b = br#"{"model":"typesafe/jev-latest","state":"x","questions":{"q":{"type":"noul"}}}"#;
        let k = |body: &[u8], v: &str| key(Family::Decide, v, body, &[]).unwrap();
        assert_eq!(
            k(a, "jev-1.13.0"),
            k(b, "jev-1.13.0"),
            "model field is not in the key"
        );
        assert_ne!(k(a, "jev-1.13.0"), k(a, "jev-1.14.0"));
        assert_ne!(
            key(Family::Decide, "v", a, &[]).unwrap(),
            key(Family::Extract, "v", a, &[]).unwrap()
        );
        assert_ne!(
            key(Family::Decide, "v", a, &[]).unwrap(),
            key(Family::Decide, "v", a, &[("long_context", "true")]).unwrap()
        );
    }

    #[test]
    fn image_content_is_in_the_key() {
        let img = |data: &str| {
            serde_json::to_vec(&json!({
                "model": "m", "state": "p1",
                "questions": {"q": {"type": "noul"}},
                "assets": {"p1": {"type": "image", "format": "png", "data_base64": data}}
            }))
            .unwrap()
        };
        let k = |b: &[u8]| key(Family::Decide, "v", b, &[]).unwrap();
        assert_ne!(k(&img("AAAA")), k(&img("AAAB")));
        assert_eq!(k(&img("AAAA")), k(&img("AAAA")));
    }

    /// Deep nesting (for example in `state`, which inspection skips) is an
    /// error, not a stack overflow, and costs no more than one pass. Runs on
    /// a thread with a tokio-worker-sized stack.
    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let deep = |n: usize| {
            format!(
                r#"{{"model":"decide","questions":{{"q":{{"type":"noul"}}}},"state":{}{}}}"#,
                "[".repeat(n),
                "]".repeat(n)
            )
        };
        let (shallow, too_deep) = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let k = |b: String| key(Family::Decide, "v", b.as_bytes(), &[]);
                (k(deep(MAX_DEPTH - 2)), k(deep(200_000)))
            })
            .unwrap()
            .join()
            .expect("no stack overflow");
        assert!(shallow.is_ok(), "{shallow:?}");
        let err = too_deep.unwrap_err();
        assert!(err.contains("deep"), "{err}");
    }

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|n| json!(n)),
            "[a-z \\\\\"é]{0,8}".prop_map(Value::String),
        ];
        leaf.prop_recursive(12, 96, 5, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::btree_map("[a-z_]{1,6}", inner, 0..5)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// The same value with every object's keys in reverse order.
    fn reorder(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, v) in m.iter().rev() {
                    out.insert(k.clone(), reorder(v));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(reorder).collect()),
            other => other.clone(),
        }
    }

    fn request(state: &Value, questions: &Map<String, Value>) -> Value {
        json!({"model": "decide", "state": state, "questions": questions})
    }

    fn questions(names: &[String], options: &[String]) -> Map<String, Value> {
        names
            .iter()
            .map(|n| {
                let criteria: Map<String, Value> =
                    options.iter().map(|o| (o.clone(), json!(format!("about {o}")))).collect();
                (n.clone(), json!({"type": "choice", "instructions": format!("{n}?"), "criteria": criteria}))
            })
            .collect()
    }

    /// The same value with every object's keys sorted (an independent
    /// reference for the canonical form; the generator has no floats).
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&m[k])))
                        .collect(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }

    proptest! {
        /// The canonical form is the sorted, compact serialization.
        #[test]
        fn canonical_matches_sorted_compact_json(v in json_value()) {
            let pretty = serde_json::to_vec_pretty(&v).unwrap();
            prop_assert_eq!(
                canonical(&pretty, &[]).unwrap(),
                serde_json::to_string(&sorted(&v)).unwrap()
            );
        }

        /// Key order and whitespace do not change the key; a change to a
        /// question, an option, or the state does.
        #[test]
        fn key_ignores_layout_but_not_content(
            state in json_value(),
            names in proptest::collection::btree_set("[a-z]{1,6}", 1..4),
            options in proptest::collection::btree_set("[a-z]{1,6}", 2..5),
            pick in any::<proptest::sample::Index>(),
        ) {
            let names: Vec<String> = names.into_iter().collect();
            let options: Vec<String> = options.into_iter().collect();
            let qs = questions(&names, &options);
            let req = request(&state, &qs);
            let k = |v: &Value, pretty: bool| {
                let bytes = if pretty { serde_json::to_vec_pretty(v) } else { serde_json::to_vec(v) }.unwrap();
                key(Family::Decide, "jev-1.13.0", &bytes, &[]).unwrap()
            };
            let base = k(&req, false);
            prop_assert_eq!(base, k(&req, true));
            prop_assert_eq!(base, k(&reorder(&req), true));

            // Rename one question.
            let victim = pick.get(&names).clone();
            let mut renamed = qs.clone();
            let q = renamed.shift_remove(&victim).unwrap();
            renamed.insert(format!("{victim}_x"), q);
            prop_assert_ne!(base, k(&request(&state, &renamed), false));

            // Change one option's description.
            let mut changed = qs.clone();
            let opt = pick.get(&options).clone();
            changed[&victim]["criteria"][&opt] = json!("something else");
            prop_assert_ne!(base, k(&request(&state, &changed), false));

            // Drop one option.
            let mut fewer = qs.clone();
            fewer[&victim]["criteria"].as_object_mut().unwrap().shift_remove(&opt);
            prop_assert_ne!(base, k(&request(&state, &fewer), false));

            // Change the state.
            prop_assert_ne!(base, k(&request(&json!([state.clone(), 1]), &qs), false));
        }
    }
}
