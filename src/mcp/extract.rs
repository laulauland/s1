//! The `extract` tool: a Clairic template plus a list of states.

use serde_json::{Map, Value, json};

use super::list::ListTool;
use crate::protocol::ProtocolId;

pub static TOOL: ListTool = ListTool {
    name: "extract",
    title: "Extract (Clairic)",
    protocol: ProtocolId::Clairic,
    summary: "Extracts Clairic fields (text, number, date, list, estimate, ...) from each \
              state with a System One extract model.",
    template_schema,
    item_schema,
    result_extra,
    build,
    pick,
};

fn template_schema() -> Value {
    json!({
        "type": "object",
        "description": "A Clairic request body without `state`: `model` (an id or an alias), \
                        `fields`, and optional `reference_date` and `options`. Each item's \
                        `state` (and `images`) is added to it.",
        "properties": {
            "model": {"type": "string"},
            "fields": {"type": "object"},
        },
        "required": ["model", "fields"],
    })
}

fn item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": ["string", "integer"], "description": "Echoed in the result; defaults to the index."},
            "state": {"description": "The text to read. Any other JSON value is sent as JSON text."},
            "images": {
                "type": "array",
                "maxItems": 4,
                "items": {"type": "string", "description": "A data URL (data:image/...;base64,...)."},
            },
        },
        "required": ["state"],
    })
}

fn result_extra() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "truncated".into(),
        json!({"type": "boolean", "description": "True when Clairic cut a long state to fit."}),
    );
    m
}

fn build(body: &mut Map<String, Value>, item: &Map<String, Value>) -> Result<(), String> {
    // Clairic takes only a string state (like `s1 extract --state-field`).
    let state = match &item["state"] {
        Value::String(s) => Value::String(s.clone()),
        other => Value::String(other.to_string()),
    };
    body.insert("state".into(), state);
    match item.get("images") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => {
            body.insert("images".into(), Value::Array(a.clone()));
        }
        Some(_) => return Err("`images` must be an array of data URL strings".into()),
    }
    Ok(())
}

fn pick(resp: &Value, out: &mut Map<String, Value>) {
    if let Some(t) = resp.get("truncated").and_then(Value::as_bool) {
        out.insert("truncated".into(), json!(t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_states_become_json_text_and_images_pass() {
        let mut body = Map::new();
        let item = json!({"state": {"a": 1}, "images": ["data:image/png;base64,AAAA"]});
        build(&mut body, item.as_object().unwrap()).unwrap();
        assert_eq!(body["state"], r#"{"a":1}"#);
        assert_eq!(body["images"][0], "data:image/png;base64,AAAA");
        let bad = json!({"state": "x", "images": "nope"});
        assert!(build(&mut Map::new(), bad.as_object().unwrap()).is_err());
    }
}
