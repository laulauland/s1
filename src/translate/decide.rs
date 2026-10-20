//! systemone <-> OpenAI Decisions (spec "Translation rules: OpenAI
//! Decisions and systemone").
//!
//! | Topic | Rule |
//! | --- | --- |
//! | Question list | Unnamed OpenAI question `i` is systemone `q<i>`; answers return in request order |
//! | Yes/no | `predicate` <-> `noul`; `probability` and `noul` are both P(yes) |
//! | Noul criteria | Appended to `instructions` toward OpenAI ([`TRUE_IF`], [`FALSE_IF`]) and read back from that exact suffix; client text is escaped ([`escape_criteria`]) |
//! | Choice values | Boolean `true` / `false` is the systemone name [`BOOL_TRUE`] / [`BOOL_FALSE`], in both directions |
//! | Choice descriptions | `description` <-> the value in the `criteria` map |
//! | Score levels | `label` + `description` joined toward systemone; more than 10 levels is `unsupported` |
//! | Images | `input_image` data URLs <-> `assets` with placeholders (URLs are fetched first, see [`super::fetch`]); a placeholder in a question is `unsupported` toward OpenAI |
//! | Non-string texts | Compact JSON text toward OpenAI |
//! | Refusal | A per-question error object for a systemone client ([`REFUSAL_CODE`]) |
//! | `box` | `unsupported` toward OpenAI |
//!
//! OpenAI-only request fields that do not change answers (`prompt_cache_key`,
//! `prompt_cache_options`, `safety_identifier`, image `detail`, cache
//! breakpoints) are dropped toward systemone.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::{NameMap, Translate, Translated};
use crate::error::S1Error;
use crate::protocol::ProtocolId;

/// Systemone choice name of the OpenAI boolean value `true`.
pub const BOOL_TRUE: &str = "bool:true";
/// Systemone choice name of the OpenAI boolean value `false`.
pub const BOOL_FALSE: &str = "bool:false";
/// `error.code` of the systemone answer that stands for an OpenAI refusal.
pub const REFUSAL_CODE: &str = "refusal";
/// Most score levels systemone takes.
pub const MAX_SYSTEMONE_LEVELS: usize = 10;
/// Noul `criteria.true`, appended to OpenAI `instructions`.
pub const TRUE_IF: &str = "True if: ";
/// Noul `criteria.false`, appended to OpenAI `instructions`.
pub const FALSE_IF: &str = "False if: ";
/// Between instructions and each appended criterion.
const PARA: &str = "\n\n";
/// Escape mark (U+200B ZERO WIDTH SPACE) between `True if` / `False if` and
/// `: ` in client text, so client text never reads as an appended
/// criterion. See [`escape_criteria`].
const ESC: char = '\u{200B}';
/// The words of [`TRUE_IF`] and [`FALSE_IF`] before `: `.
const CRITERIA_KEYS: [&str; 2] = ["True if", "False if"];
/// Most bytes of image data URLs one systemone request may expand into
/// toward OpenAI, summed over every image occurrence (one asset referenced
/// many times counts each time). Over it is `too_long`.
pub const MAX_EXPANDED_IMAGES: usize = crate::daemon::MAX_BODY;

pub struct Decide;

impl Translate for Decide {
    fn request(
        &self,
        from: ProtocolId,
        to: ProtocolId,
        body: &[u8],
    ) -> Result<Translated, S1Error> {
        match (from, to) {
            (ProtocolId::OpenaiDecisions, ProtocolId::Systemone) => openai_to_systemone(body),
            (ProtocolId::Systemone, ProtocolId::OpenaiDecisions) => systemone_to_openai(body),
            _ => Err(no_pair(from, to)),
        }
    }

    fn response(
        &self,
        from: ProtocolId,
        to: ProtocolId,
        body: &[u8],
        map: &NameMap,
    ) -> Result<Bytes, S1Error> {
        let out = match (from, to) {
            (ProtocolId::Systemone, ProtocolId::OpenaiDecisions) => {
                systemone_answers_to_openai(body, map)?
            }
            (ProtocolId::OpenaiDecisions, ProtocolId::Systemone) => {
                openai_answers_to_systemone(body, map)?
            }
            _ => return Err(no_pair(from, to)),
        };
        Ok(Bytes::from(serde_json::to_vec(&out).expect("JSON")))
    }
}

fn no_pair(from: ProtocolId, to: ProtocolId) -> S1Error {
    S1Error::Unsupported(format!("no translation from `{from}` to `{to}`"))
}

fn invalid(msg: impl Into<String>) -> S1Error {
    S1Error::InvalidRequest(msg.into())
}

fn object(body: &[u8], what: &str) -> Result<Map<String, Value>, S1Error> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(invalid(format!("{what} is not a JSON object"))),
        Err(e) => Err(invalid(format!("cannot read {what}: {e}"))),
    }
}

/// A JSON value as text: strings as they are, `null` as empty, anything
/// else as compact JSON.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Systemone choice name of an OpenAI choice value.
pub fn choice_name(v: &Value) -> Result<String, S1Error> {
    match v {
        Value::Bool(true) => Ok(BOOL_TRUE.into()),
        Value::Bool(false) => Ok(BOOL_FALSE.into()),
        Value::String(s) if s == BOOL_TRUE || s == BOOL_FALSE => {
            Err(S1Error::Unsupported(format!(
                "the string choice value `{s}` cannot be told apart from a boolean value in systemone; rename it"
            )))
        }
        Value::String(s) => Ok(s.clone()),
        other => Err(invalid(format!(
            "choice `value` must be a string or a boolean, not {other}"
        ))),
    }
}

/// OpenAI choice value of a systemone choice name.
pub fn choice_value(name: &str) -> Value {
    match name {
        BOOL_TRUE => Value::Bool(true),
        BOOL_FALSE => Value::Bool(false),
        s => Value::String(s.to_string()),
    }
}

/// Systemone name of a choice value in an answer (never fails).
fn answer_choice_name(v: &Value) -> String {
    match v {
        Value::Bool(true) => BOOL_TRUE.into(),
        Value::Bool(false) => BOOL_FALSE.into(),
        other => text(other),
    }
}

// ---------------------------------------------------------------------------
// Requests: OpenAI Decisions -> systemone
// ---------------------------------------------------------------------------

/// Rewrite every `True if` / `False if` + `n` [`ESC`] marks + `: ` in `s`
/// to have `n + 1` marks (`up`) or `n - 1` marks (not `up`; `n = 0` stays,
/// it is a real criterion marker).
fn shift_escapes(s: &str, up: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let hit = CRITERIA_KEYS.iter().find_map(|k| {
            let after = rest.strip_prefix(k)?;
            let marks = after.chars().take_while(|c| *c == ESC).count();
            after[marks * ESC.len_utf8()..]
                .starts_with(": ")
                .then_some((k, marks))
        });
        match hit {
            Some((k, marks)) => {
                let n = match (up, marks) {
                    (true, m) => m + 1,
                    (false, 0) => 0,
                    (false, m) => m - 1,
                };
                out.push_str(k);
                out.extend(std::iter::repeat_n(ESC, n));
                rest = &rest[k.len() + marks * ESC.len_utf8()..];
            }
            None => {
                let c = rest.chars().next().expect("not empty");
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// Client text made safe to sit next to appended criteria: each `True if: `
/// / `False if: ` gets one more [`ESC`] mark before the colon (and one that
/// already has marks gets another), so only s1's own markers have none.
pub fn escape_criteria(s: &str) -> String {
    shift_escapes(s, true)
}

/// Inverse of [`escape_criteria`].
pub fn unescape_criteria(s: &str) -> String {
    shift_escapes(s, false)
}

/// Noul instructions with the criteria appended (toward OpenAI). Client
/// text is escaped ([`escape_criteria`]), so the result reads back exactly.
pub fn join_criteria(instructions: &str, t: Option<&str>, f: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !instructions.is_empty() {
        parts.push(escape_criteria(instructions));
    }
    if let Some(t) = t {
        parts.push(format!("{TRUE_IF}{}", escape_criteria(t)));
    }
    if let Some(f) = f {
        parts.push(format!("{FALSE_IF}{}", escape_criteria(f)));
    }
    parts.join(PARA)
}

/// Inverse of [`join_criteria`]: instructions and the appended criteria.
/// Only unescaped markers at the start or after a blank line count.
pub fn split_criteria(s: &str) -> (String, Option<String>, Option<String>) {
    fn take(rest: &mut String, prefix: &str) -> Option<String> {
        let sep = format!("{PARA}{prefix}");
        if let Some(i) = rest.rfind(&sep) {
            let tail = rest[i + sep.len()..].to_string();
            rest.truncate(i);
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix(prefix) {
            let tail = tail.to_string();
            rest.clear();
            Some(tail)
        } else {
            None
        }
    }
    let mut rest = s.to_string();
    let f = take(&mut rest, FALSE_IF);
    let t = take(&mut rest, TRUE_IF);
    (
        unescape_criteria(&rest),
        t.as_deref().map(unescape_criteria),
        f.as_deref().map(unescape_criteria),
    )
}

/// Systemone names for OpenAI questions: the client's name when it is set
/// and not taken, else `q<index>` (or a free variant of it).
fn target_names(questions: &[Value]) -> Result<Vec<(String, Option<String>)>, S1Error> {
    let mut client = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        match q.get("name") {
            None | Some(Value::Null) => client.push(None),
            Some(Value::String(s)) => client.push(Some(s.clone())),
            Some(other) => {
                return Err(invalid(format!(
                    "questions[{i}].name must be a string, not {other}"
                )));
            }
        }
    }
    let reserved: HashSet<&str> = client.iter().flatten().map(String::as_str).collect();
    let mut taken: HashSet<String> = HashSet::new();
    // Next suffix to try per base name. A suffixed candidate that is not
    // free stays not free (`taken` only grows, `reserved` is fixed, and a
    // suffixed candidate is never the question's own name), so the search
    // for a base resumes where it stopped: linear in the questions.
    let mut next: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(questions.len());
    for (i, name) in client.iter().enumerate() {
        // Free: not given to an earlier question, and not a name some other
        // question asks for.
        let free =
            |c: &str| !taken.contains(c) && (!reserved.contains(c) || name.as_deref() == Some(c));
        let base = name.clone().unwrap_or_else(|| format!("q{i}"));
        let target = if free(&base) {
            base
        } else {
            let k = next.entry(base.clone()).or_insert(1);
            loop {
                let c = format!("{base}_{k}");
                *k += 1;
                if free(&c) {
                    break c;
                }
            }
        };
        taken.insert(target.clone());
        out.push((target, name.clone()));
    }
    Ok(out)
}

/// Routing facts of OpenAI questions as a systemone backend sees them
/// after [`openai_to_systemone`]: `(missing_instructions,
/// loose_question_names)` (see [`crate::protocol::RouteInfo`]). A
/// predicate whose instructions hold only appended criteria has no
/// instructions in systemone. Questions that do not translate give no
/// facts; translation reports them.
pub fn systemone_question_facts(questions: &[Value]) -> (bool, bool) {
    let Ok(names) = target_names(questions) else {
        return (false, false);
    };
    let loose = names
        .iter()
        .any(|(target, _)| !crate::protocol::systemone::strict_name(target));
    let missing = questions.iter().any(|q| {
        let instructions = q.get("instructions").map(text).unwrap_or_default();
        let instr = match q.get("type").and_then(Value::as_str) {
            Some("predicate") => split_criteria(&instructions).0,
            _ => instructions,
        };
        instr.trim().is_empty()
    });
    (missing, loose)
}

/// Image placeholder that occurs in none of `texts`. The `<` and `>`
/// ends keep one placeholder from being a prefix of another
/// (`<s1-image-1>`, `<s1-image-10>`) and from matching across the edge
/// of the text next to it.
fn placeholder(i: usize, texts: &[&str]) -> String {
    let mut tag = 0usize;
    loop {
        let p = if tag == 0 {
            format!("<s1-image-{i}>")
        } else {
            format!("<s1-image-{tag}-{i}>")
        };
        if !texts.iter().any(|t| t.contains(&p)) {
            return p;
        }
        tag += 1;
    }
}

/// Every string in `v` (object keys and values, at any depth).
fn string_leaves<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| string_leaves(x, out)),
        Value::Object(m) => m.iter().for_each(|(k, x)| {
            out.push(k);
            string_leaves(x, out);
        }),
        _ => {}
    }
}

/// Format and base64 data of an image data URL (PNG or JPEG only).
pub fn parse_data_url(url: &str) -> Result<(&'static str, &str), S1Error> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| invalid("image is not a data URL"))?;
    let (meta, data) = rest
        .split_once(',')
        .ok_or_else(|| invalid("image data URL has no `,`"))?;
    let mut fields = meta.split(';');
    let mime = fields.next().unwrap_or("").to_ascii_lowercase();
    if !fields.any(|f| f.eq_ignore_ascii_case("base64")) {
        return Err(invalid("image data URL must be base64"));
    }
    let format = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        other => {
            return Err(S1Error::Unsupported(format!(
                "systemone takes PNG or JPEG images, not `{other}`"
            )));
        }
    };
    Ok((format, data))
}

/// `state` and `assets` from an OpenAI `input`. Adjacent text parts are
/// joined with a newline, messages with a blank line; each image becomes a
/// placeholder in the text (with no separator next to it, the inverse of
/// [`split_placeholders`]) and an asset. The same image data twice uses
/// one placeholder and one asset. Placeholders are chosen absent from the
/// input text and from every string in `others` (the questions), because
/// systemone may expand a placeholder in any of them.
fn input_to_state(input: &Value, others: &[Value]) -> Result<(Value, Map<String, Value>), S1Error> {
    let messages = match input {
        Value::String(s) => return Ok((Value::String(s.clone()), Map::new())),
        Value::Array(m) => m,
        _ => return Err(invalid("`input` must be a string or a list of messages")),
    };
    enum Part<'a> {
        Text(&'a str),
        Image(&'a str),
    }
    let mut blocks: Vec<Vec<Part<'_>>> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let ty = m.get("type").and_then(Value::as_str).unwrap_or("message");
        if ty != "message" || role != "user" {
            return Err(S1Error::Unsupported(format!(
                "input[{i}]: only user messages translate to systemone, not `{ty}` with role `{role}`"
            )));
        }
        let mut parts = Vec::new();
        match m.get("content") {
            Some(Value::String(s)) => parts.push(Part::Text(s)),
            Some(Value::Array(items)) => {
                for (j, p) in items.iter().enumerate() {
                    match p.get("type").and_then(Value::as_str) {
                        Some("input_text") => parts.push(Part::Text(
                            p.get("text").and_then(Value::as_str).unwrap_or(""),
                        )),
                        Some("input_image") => {
                            let url = p.get("image_url").and_then(Value::as_str).ok_or_else(|| {
                                S1Error::Unsupported(format!(
                                    "input[{i}].content[{j}]: only `image_url` images translate to systemone"
                                ))
                            })?;
                            parts.push(Part::Image(url));
                        }
                        other => {
                            return Err(S1Error::Unsupported(format!(
                                "input[{i}].content[{j}]: part type {other:?} does not translate to systemone"
                            )));
                        }
                    }
                }
            }
            _ => return Err(invalid(format!("input[{i}] has no `content`"))),
        }
        blocks.push(parts);
    }
    let mut texts: Vec<&str> = blocks
        .iter()
        .flatten()
        .filter_map(|p| match p {
            Part::Text(t) => Some(*t),
            Part::Image(_) => None,
        })
        .collect();
    others.iter().for_each(|v| string_leaves(v, &mut texts));
    let mut assets = Map::new();
    let mut seen: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut state_blocks = Vec::new();
    for parts in &blocks {
        let mut out = String::new();
        let mut last_text = false;
        for p in parts {
            match p {
                Part::Text(t) => {
                    if last_text {
                        out.push('\n');
                    }
                    out.push_str(t);
                    last_text = true;
                }
                Part::Image(url) => {
                    last_text = false;
                    if let Some(p) = seen.get(url) {
                        out.push_str(p);
                        continue;
                    }
                    let (format, data) = parse_data_url(url).map_err(|e| match e {
                        S1Error::InvalidRequest(m) if !url.starts_with("data:") => {
                            invalid(format!("image URL was not fetched before translation: {m}"))
                        }
                        other => other,
                    })?;
                    let p = placeholder(assets.len(), &texts);
                    assets.insert(
                        p.clone(),
                        json!({"type": "image", "format": format, "data_base64": data}),
                    );
                    out.push_str(&p);
                    seen.insert(url, p);
                }
            }
        }
        state_blocks.push(out);
    }
    Ok((Value::String(state_blocks.join(PARA)), assets))
}

fn openai_to_systemone(body: &[u8]) -> Result<Translated, S1Error> {
    let req = object(body, "OpenAI Decisions request")?;
    let model = req.get("model").cloned().unwrap_or(Value::Null);
    let input = req
        .get("input")
        .ok_or_else(|| invalid("request has no `input`"))?;
    let questions = req
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("`questions` must be a list"))?;
    let (state, assets) = input_to_state(input, questions)?;
    let names = target_names(questions)?;
    let mut map = NameMap::default();
    let mut out_q = Map::new();
    for (i, (q, (target, client))) in questions.iter().zip(names).enumerate() {
        let ty = q.get("type").and_then(Value::as_str).unwrap_or("");
        let instructions = q.get("instructions").map(text).unwrap_or_default();
        let mut o = Map::new();
        match ty {
            "predicate" => {
                let (instr, t, f) = split_criteria(&instructions);
                o.insert("type".into(), json!("noul"));
                if !instr.is_empty() {
                    o.insert("instructions".into(), Value::String(instr));
                }
                if t.is_some() || f.is_some() {
                    let mut c = Map::new();
                    if let Some(t) = t {
                        c.insert("true".into(), Value::String(t));
                    }
                    if let Some(f) = f {
                        c.insert("false".into(), Value::String(f));
                    }
                    o.insert("criteria".into(), Value::Object(c));
                }
            }
            "choice" => {
                let choices = q.get("choices").and_then(Value::as_array).ok_or_else(|| {
                    invalid(format!("questions[{i}]: a choice needs a `choices` list"))
                })?;
                let mut criteria = Map::new();
                for c in choices {
                    let name = choice_name(c.get("value").unwrap_or(&Value::Null))?;
                    let desc = match c.get("description") {
                        None | Some(Value::Null) => Value::Null,
                        Some(d) => Value::String(text(d)),
                    };
                    if criteria.insert(name.clone(), desc).is_some() {
                        return Err(invalid(format!(
                            "questions[{i}]: choice `{name}` appears twice"
                        )));
                    }
                }
                o.insert("type".into(), json!("choice"));
                if !instructions.is_empty() {
                    o.insert("instructions".into(), Value::String(instructions));
                }
                o.insert("criteria".into(), Value::Object(criteria));
            }
            "score" => {
                let levels = q.get("levels").and_then(Value::as_array).ok_or_else(|| {
                    invalid(format!("questions[{i}]: a score needs a `levels` list"))
                })?;
                if levels.len() > MAX_SYSTEMONE_LEVELS {
                    return Err(S1Error::Unsupported(format!(
                        "questions[{i}] has {} score levels; systemone takes at most {MAX_SYSTEMONE_LEVELS}",
                        levels.len()
                    )));
                }
                let mut labels = Vec::with_capacity(levels.len());
                let mut criteria = Vec::with_capacity(levels.len());
                for l in levels {
                    let label = l.get("label").map(text).unwrap_or_default();
                    let joined = match l.get("description").map(text) {
                        Some(d) if !d.is_empty() => format!("{label}: {d}"),
                        _ => label.clone(),
                    };
                    labels.push(Value::String(label));
                    criteria.push(Value::String(joined));
                }
                map.levels.insert(target.clone(), labels);
                o.insert("type".into(), json!("score"));
                if !instructions.is_empty() {
                    o.insert("instructions".into(), Value::String(instructions));
                }
                o.insert("criteria".into(), Value::Array(criteria));
            }
            other => {
                return Err(S1Error::Unsupported(format!(
                    "questions[{i}]: question type `{other}` does not translate to systemone"
                )));
            }
        }
        out_q.insert(target.clone(), Value::Object(o));
        map.push(&target, client);
    }
    let mut out = Map::new();
    out.insert("model".into(), model);
    out.insert("state".into(), state);
    out.insert("questions".into(), Value::Object(out_q));
    if !assets.is_empty() {
        out.insert("assets".into(), Value::Object(assets));
    }
    Ok(Translated {
        body: Bytes::from(serde_json::to_vec(&Value::Object(out)).expect("JSON")),
        map,
    })
}

// ---------------------------------------------------------------------------
// Requests: systemone -> OpenAI Decisions
// ---------------------------------------------------------------------------

/// What is left of a request's [`MAX_EXPANDED_IMAGES`] budget. Each image
/// occurrence is taken from it before its data URL is built. It also
/// counts images against [`MAX_OPENAI_IMAGES`].
struct ImageBudget {
    bytes: usize,
    images: usize,
}

impl ImageBudget {
    fn new() -> Self {
        Self {
            bytes: MAX_EXPANDED_IMAGES,
            images: 0,
        }
    }

    /// Take room for one image whose data URL has `data_len` base64 bytes
    /// (plus its `data:<mime>;base64,` prefix).
    fn take(&mut self, data_len: usize) -> Result<(), S1Error> {
        self.images += 1;
        if self.images > MAX_OPENAI_IMAGES {
            return Err(S1Error::Unsupported(format!(
                "the request has more than {MAX_OPENAI_IMAGES} images toward OpenAI Decisions (each asset reference counts once per occurrence)"
            )));
        }
        let n = data_len.saturating_add(DATA_URL_PREFIX);
        self.bytes = self.bytes.checked_sub(n).ok_or_else(|| {
            S1Error::TooLong(format!(
                "images expand to more than {} MiB toward OpenAI Decisions (each asset reference counts once per occurrence)",
                MAX_EXPANDED_IMAGES / (1024 * 1024)
            ))
        })?;
        Ok(())
    }
}

/// Bytes counted for the `data:<mime>;base64,` prefix of one image.
const DATA_URL_PREFIX: usize = 32;

/// One `input_image` occurrence of a systemone asset, taken from `budget`
/// before the data URL is built.
fn asset_image(name: &str, asset: &Value, budget: &mut ImageBudget) -> Result<Value, S1Error> {
    let len = asset
        .get("data_base64")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    budget.take(len)?;
    Ok(input_image(asset_data_url(name, asset)?))
}

/// Data URL of a systemone asset (`format` + `data_base64`).
fn asset_data_url(name: &str, asset: &Value) -> Result<String, S1Error> {
    if asset.get("part").is_some() {
        return Err(S1Error::Unsupported(format!(
            "asset `{name}` uses a multipart `part`; send `data_base64` to translate it"
        )));
    }
    if asset.get(crate::images::PATH_FIELD).is_some() {
        return Err(invalid(format!(
            "asset `{name}` names a local file; only the Unix socket may do that"
        )));
    }
    let data = asset
        .get("data_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("asset `{name}` has no `data_base64`")))?;
    let mime = match asset.get("format").and_then(Value::as_str) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => {
            // Detect from the first bytes.
            let head: String = data.chars().take(16).collect();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(head.as_bytes())
                .unwrap_or_default();
            match crate::images::image_format(&bytes) {
                Some("png") => "image/png",
                Some(_) => "image/jpeg",
                None => return Err(invalid(format!("asset `{name}` is not PNG or JPEG"))),
            }
        }
    };
    Ok(format!("data:{mime};base64,{data}"))
}

/// Bytes of a Clef `images` item's image data, before its data URL is built.
fn clef_image_len(item: &Value) -> usize {
    match item {
        Value::String(s) => s.len(),
        Value::Object(o) => o.get("base64").and_then(Value::as_str).map_or(0, str::len),
        _ => 0,
    }
}

/// Data URL of a Clef `images` item (a data URL string, or
/// `{content_type, base64}`).
fn clef_image_url(i: usize, item: &Value) -> Result<String, S1Error> {
    match item {
        Value::String(s) if s.starts_with("data:") => Ok(s.clone()),
        Value::Object(o) => {
            let ct = o
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = o
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("images[{i}] has no `base64`")))?;
            Ok(format!("data:{ct};base64,{data}"))
        }
        _ => Err(invalid(format!(
            "images[{i}] must be a data URL or {{content_type, base64}}"
        ))),
    }
}

fn input_image(url: String) -> Value {
    json!({"type": "input_image", "image_url": url})
}

fn input_text(t: &str) -> Value {
    json!({"type": "input_text", "text": t})
}

/// Split one string value at asset references, left to right, longest
/// match first: `Ok(text)` pieces and `Err(asset key)` references. Lazy,
/// so a caller that stops early (the image budget) scans no further.
fn split_refs<'s, 'k>(
    s: &'s str,
    assets: &'k Map<String, Value>,
) -> impl Iterator<Item = Result<&'s str, &'k String>> {
    let mut rest = s;
    // A reference found but not yet yielded (the text before it went first).
    let mut pending: Option<&'k String> = None;
    std::iter::from_fn(move || {
        if let Some(key) = pending.take() {
            return Some(Err(key));
        }
        if rest.is_empty() {
            return None;
        }
        let hit = assets
            .keys()
            .filter(|k| !k.is_empty())
            .filter_map(|k| rest.find(k.as_str()).map(|i| (i, k)))
            .min_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())));
        let Some((i, key)) = hit else {
            return Some(Ok(std::mem::take(&mut rest)));
        };
        let text = &rest[..i];
        rest = &rest[i + key.len()..];
        if i > 0 {
            pending = Some(key);
            Some(Ok(text))
        } else {
            Some(Err(key))
        }
    })
}

/// Most images in one OpenAI Decisions request ("at most 128 images are
/// allowed across all messages in one request").
pub const MAX_OPENAI_IMAGES: usize = super::fetch::MAX_IMAGES;

/// Images a systemone request becomes toward OpenAI Decisions: every
/// asset reference in `state` (one asset referenced many times counts
/// each time) plus Clef `images` items, counted up to one over
/// [`MAX_OPENAI_IMAGES`]. `None` when the request has no `assets` (the
/// image count routing already has is then right) or does not read
/// (translation reports that).
pub fn openai_image_occurrences(body: &[u8]) -> Option<u32> {
    #[derive(serde::Deserialize)]
    struct Head {
        #[serde(default)]
        state: Value,
        #[serde(default)]
        assets: Option<Map<String, Value>>,
        #[serde(default)]
        images: Option<Vec<serde::de::IgnoredAny>>,
    }
    let head: Head = serde_json::from_slice(body).ok()?;
    let assets = head.assets.filter(|a| !a.is_empty())?;
    let cap = MAX_OPENAI_IMAGES + 1;
    let mut n = head.images.map_or(0, |i| i.len()).min(cap);
    let mut strings = Vec::new();
    string_values(&head.state, &mut strings);
    'count: for s in strings {
        for piece in split_refs(s, &assets) {
            if n >= cap {
                break 'count;
            }
            if piece.is_err() {
                n += 1;
            }
        }
    }
    Some(n as u32)
}

/// Text and image parts of a systemone `state`. A string state is split at
/// asset references. A structured state goes as its compact JSON text, cut
/// at the references found inside its string values only (systemone does
/// not expand object keys), matched before JSON escaping.
fn split_placeholders(
    state: &Value,
    assets: &Map<String, Value>,
    budget: &mut ImageBudget,
) -> Result<(Vec<Value>, HashSet<String>), S1Error> {
    struct Out<'b> {
        parts: Vec<Value>,
        buf: String,
        used: HashSet<String>,
        budget: &'b mut ImageBudget,
    }
    impl Out<'_> {
        fn flush(&mut self) {
            if !self.buf.is_empty() {
                self.parts.push(input_text(&std::mem::take(&mut self.buf)));
            }
        }
    }
    fn escaped(s: &str) -> String {
        let q = serde_json::to_string(s).expect("JSON string");
        q[1..q.len() - 1].to_string()
    }
    fn walk(v: &Value, assets: &Map<String, Value>, o: &mut Out<'_>) -> Result<(), S1Error> {
        match v {
            Value::String(s) => {
                o.buf.push('"');
                for piece in split_refs(s, assets) {
                    match piece {
                        Ok(t) => o.buf.push_str(&escaped(t)),
                        Err(key) => {
                            o.flush();
                            o.parts.push(asset_image(key, &assets[key], o.budget)?);
                            o.used.insert(key.clone());
                        }
                    }
                }
                o.buf.push('"');
            }
            Value::Array(a) => {
                o.buf.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        o.buf.push(',');
                    }
                    walk(x, assets, o)?;
                }
                o.buf.push(']');
            }
            Value::Object(m) => {
                o.buf.push('{');
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        o.buf.push(',');
                    }
                    o.buf.push('"');
                    o.buf.push_str(&escaped(k));
                    o.buf.push_str("\":");
                    walk(x, assets, o)?;
                }
                o.buf.push('}');
            }
            other => o.buf.push_str(&other.to_string()),
        }
        Ok(())
    }
    let mut o = Out {
        parts: Vec::new(),
        buf: String::new(),
        used: HashSet::new(),
        budget,
    };
    match state {
        Value::String(s) => {
            for piece in split_refs(s, assets) {
                match piece {
                    Ok(t) => o.parts.push(input_text(t)),
                    Err(key) => {
                        o.parts.push(asset_image(key, &assets[key], o.budget)?);
                        o.used.insert(key.clone());
                    }
                }
            }
        }
        other => {
            walk(other, assets, &mut o)?;
            o.flush();
        }
    }
    Ok((o.parts, o.used))
}

/// String values (not object keys), at any depth.
fn string_values<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| string_values(x, out)),
        Value::Object(m) => m.values().for_each(|x| string_values(x, out)),
        _ => {}
    }
}

/// Texts of a systemone question where an asset placeholder may appear:
/// the string values in `instructions` and in the `criteria` values.
fn question_texts(q: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(i) = q.get("instructions") {
        string_values(i, &mut out);
    }
    match q.get("criteria") {
        Some(Value::Object(m)) => m.values().for_each(|v| string_values(v, &mut out)),
        Some(other) => string_values(other, &mut out),
        None => {}
    }
    out
}

fn systemone_to_openai(body: &[u8]) -> Result<Translated, S1Error> {
    let req = object(body, "systemone request")?;
    let model = req.get("model").cloned().unwrap_or(Value::Null);
    let state = req
        .get("state")
        .ok_or_else(|| invalid("request has no `state`"))?;
    let questions = req
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("`questions` must be an object"))?;
    let empty = Map::new();
    let assets = match req.get("assets") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(a)) => a,
        Some(_) => return Err(invalid("`assets` must be an object")),
    };
    let images = match req.get("images") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(a)) => a.as_slice(),
        Some(_) => return Err(invalid("`images` must be a list")),
    };
    let state_text = text(state);
    // OpenAI Decisions takes images only in the input: a placeholder in a
    // question would lose its image.
    for (name, q) in questions {
        let texts = question_texts(q);
        if let Some(key) = assets
            .keys()
            .filter(|k| !k.is_empty())
            .find(|k| texts.iter().any(|t| t.contains(k.as_str())))
        {
            return Err(S1Error::Unsupported(format!(
                "asset `{key}` is referenced in question `{name}`; OpenAI Decisions takes images only in the input, so put the placeholder only in `state`"
            )));
        }
    }
    let input = if assets.is_empty() && images.is_empty() {
        Value::String(state_text)
    } else {
        let mut budget = ImageBudget::new();
        let (mut parts, used) = split_placeholders(state, assets, &mut budget)?;
        if let Some(unused) = assets.keys().find(|k| !used.contains(*k)) {
            return Err(S1Error::Unsupported(format!(
                "asset `{unused}` is not referenced in `state`; OpenAI Decisions takes images only in the input"
            )));
        }
        for (i, item) in images.iter().enumerate() {
            budget.take(clef_image_len(item))?;
            parts.push(input_image(clef_image_url(i, item)?));
        }
        json!([{"role": "user", "content": parts}])
    };
    let mut map = NameMap::default();
    let mut out_q = Vec::with_capacity(questions.len());
    for (name, q) in questions {
        let ty = q.get("type").and_then(Value::as_str).unwrap_or("");
        let instructions = q.get("instructions").map(text).unwrap_or_default();
        let mut o = Map::new();
        match ty {
            "noul" => {
                let c = q.get("criteria");
                let side = |k: &str| c.and_then(|c| c.get(k)).filter(|v| !v.is_null()).map(text);
                let (t, f) = (side("true"), side("false"));
                o.insert("type".into(), json!("predicate"));
                o.insert("name".into(), json!(name));
                o.insert(
                    "instructions".into(),
                    Value::String(join_criteria(&instructions, t.as_deref(), f.as_deref())),
                );
            }
            "choice" => {
                let criteria = q
                    .get("criteria")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        invalid(format!("question `{name}`: `criteria` must be an object"))
                    })?;
                let choices: Vec<Value> = criteria
                    .iter()
                    .map(|(k, d)| {
                        let mut c = Map::new();
                        c.insert("value".into(), choice_value(k));
                        if !d.is_null() {
                            c.insert("description".into(), Value::String(text(d)));
                        }
                        Value::Object(c)
                    })
                    .collect();
                o.insert("type".into(), json!("choice"));
                o.insert("name".into(), json!(name));
                o.insert("instructions".into(), Value::String(instructions));
                o.insert("choices".into(), Value::Array(choices));
            }
            "score" => {
                let criteria = q.get("criteria").and_then(Value::as_array).ok_or_else(|| {
                    invalid(format!("question `{name}`: `criteria` must be a list"))
                })?;
                let levels: Vec<Value> =
                    criteria.iter().map(|l| json!({"label": text(l)})).collect();
                map.levels.insert(name.clone(), criteria.clone());
                o.insert("type".into(), json!("score"));
                o.insert("name".into(), json!(name));
                o.insert("instructions".into(), Value::String(instructions));
                o.insert("levels".into(), Value::Array(levels));
            }
            "box" => {
                return Err(S1Error::Unsupported(format!(
                    "question `{name}`: `box` questions need Jev; OpenAI Decisions has no box type"
                )));
            }
            other => {
                return Err(S1Error::Unsupported(format!(
                    "question `{name}`: type `{other}` does not translate to OpenAI Decisions"
                )));
            }
        }
        out_q.push(Value::Object(o));
        map.push(name, Some(name.clone()));
    }
    let out = json!({"model": model, "input": input, "questions": out_q});
    Ok(Translated {
        body: Bytes::from(serde_json::to_vec(&out).expect("JSON")),
        map,
    })
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn num(a: &Value, k: &str) -> Value {
    a.get(k).cloned().unwrap_or(Value::Null)
}

fn u64_of(v: Option<&Value>) -> u64 {
    v.and_then(Value::as_u64).unwrap_or(0)
}

/// Systemone answers (keyed by name) as an OpenAI `answers` list in
/// request order.
fn systemone_answers_to_openai(body: &[u8], map: &NameMap) -> Result<Value, S1Error> {
    let resp = object(body, "systemone response")?;
    let answers = resp
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("systemone response has no `answers` object"))?;
    let mut out = Vec::with_capacity(map.order.len());
    for target in &map.order {
        let name = map.client(target).map_or(Value::Null, |n| json!(n));
        let a = answers.get(target);
        let ty = a.and_then(|a| a.get("type")).and_then(Value::as_str);
        let item = match (a, ty) {
            (Some(a), Some("noul")) => {
                json!({"type": "predicate", "name": name, "probability": num(a, "noul")})
            }
            (Some(a), Some("choice")) => {
                let probs: Vec<Value> = a
                    .get("probabilities")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .map(|(k, p)| json!({"value": choice_value(k), "probability": p}))
                    .collect();
                let choice = a
                    .get("choice")
                    .and_then(Value::as_str)
                    .map_or(Value::Null, choice_value);
                json!({"type": "choice", "name": name, "choice": choice, "probabilities": probs, "confidence": num(a, "confidence")})
            }
            (Some(a), Some("score")) => {
                let labels = map.levels.get(target);
                let legend = a.get("legend");
                let mut probs: Vec<(u64, Value)> = a
                    .get("probabilities")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, p)| {
                        let idx: u64 = k.parse().ok()?;
                        let label = labels
                            .and_then(|l| l.get(idx as usize))
                            .or_else(|| legend.and_then(|l| l.get(k)))
                            .map(text)
                            .unwrap_or_default();
                        Some((idx, json!({"value": idx, "label": label, "probability": p})))
                    })
                    .collect();
                probs.sort_by_key(|(i, _)| *i);
                let probs: Vec<Value> = probs.into_iter().map(|(_, v)| v).collect();
                json!({"type": "score", "name": name, "score": num(a, "score"), "probabilities": probs, "confidence": num(a, "confidence")})
            }
            // An error, a missing answer, or a type OpenAI does not have:
            // no value is invented.
            _ => json!({"type": "refusal", "name": name}),
        };
        out.push(item);
    }
    let usage = resp.get("usage");
    let input = u64_of(usage.and_then(|u| u.get("input_tokens")));
    let output = u64_of(usage.and_then(|u| u.get("output_tokens")));
    Ok(json!({
        "model": num(&Value::Object(resp.clone()), "model"),
        "answers": out,
        "usage": {
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input + output,
        }
    }))
}

/// The per-question error object a systemone client gets for a refusal.
pub fn refusal_answer() -> Value {
    json!({
        "type": "error",
        "error": {
            "code": REFUSAL_CODE,
            "message": "OpenAI Decisions refused to answer this question",
        }
    })
}

/// OpenAI answers (a list in request order) as systemone answers keyed by
/// the client's names.
fn openai_answers_to_systemone(body: &[u8], map: &NameMap) -> Result<Value, S1Error> {
    let resp = object(body, "OpenAI Decisions response")?;
    let answers = resp
        .get("answers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("OpenAI Decisions response has no `answers` list"))?;
    let mut out = Map::new();
    for (i, target) in map.order.iter().enumerate() {
        let a = answers.get(i);
        let ty = a.and_then(|a| a.get("type")).and_then(Value::as_str);
        let item = match (a, ty) {
            (Some(a), Some("predicate")) => json!({"type": "noul", "noul": num(a, "probability")}),
            (Some(a), Some("choice")) => {
                let mut probs = Map::new();
                for p in a
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    probs.insert(
                        answer_choice_name(p.get("value").unwrap_or(&Value::Null)),
                        num(p, "probability"),
                    );
                }
                json!({
                    "type": "choice",
                    "choice": answer_choice_name(a.get("choice").unwrap_or(&Value::Null)),
                    "confidence": num(a, "confidence"),
                    "probabilities": probs,
                })
            }
            (Some(a), Some("score")) => {
                let levels = map.levels.get(target);
                let mut legend = Map::new();
                let mut probs = Map::new();
                for p in a
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(idx) = p.get("value").and_then(Value::as_u64) else {
                        continue;
                    };
                    let k = idx.to_string();
                    let label = levels
                        .and_then(|l| l.get(idx as usize))
                        .cloned()
                        .unwrap_or_else(|| num(p, "label"));
                    legend.insert(k.clone(), label);
                    probs.insert(k, num(p, "probability"));
                }
                json!({
                    "type": "score",
                    "score": num(a, "score"),
                    "confidence": num(a, "confidence"),
                    "legend": legend,
                    "probabilities": probs,
                })
            }
            (Some(_), Some("refusal")) => refusal_answer(),
            (Some(_), other) => json!({
                "type": "error",
                "error": {
                    "code": "unsupported",
                    "message": format!("OpenAI Decisions answer type {other:?} has no systemone form"),
                }
            }),
            (None, _) => json!({
                "type": "error",
                "error": {"code": "missing_answer", "message": "OpenAI Decisions returned no answer for this question"}
            }),
        };
        // The client's name, unless an earlier answer has it (OpenAI allows
        // duplicate names): then the unique target name, so no answer is lost.
        let key = match map.client(target) {
            Some(c) if !out.contains_key(c) => c.to_string(),
            _ => target.clone(),
        };
        out.insert(key, item);
    }
    let usage = resp.get("usage");
    Ok(json!({
        "model": num(&Value::Object(resp.clone()), "model"),
        "answers": out,
        "usage": {
            "input_tokens": u64_of(usage.and_then(|u| u.get("input_tokens"))),
            "output_tokens": u64_of(usage.and_then(|u| u.get("output_tokens"))),
        }
    }))
}

#[cfg(test)]
mod tests;
