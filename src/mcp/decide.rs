//! The `decide` tool: a systemone template plus a list of states.

use serde_json::{Map, Value, json};

use super::list::ListTool;
use crate::protocol::ProtocolId;

pub static TOOL: ListTool = ListTool {
    name: "decide",
    title: "Decide (System One)",
    protocol: ProtocolId::Systemone,
    summary: "Answers systemone questions (noul, choice, score) about each state with a \
              System One decide model.",
    template_schema,
    item_schema,
    result_extra: Map::new,
    build,
    pick: |_, _| {},
};

fn template_schema() -> Value {
    json!({
        "type": "object",
        "description": "A systemone request body without `state`: `model` (an id or an alias) \
                        and `questions`. Each item's `state` is added to it.",
        "properties": {
            "model": {"type": "string"},
            "questions": {"type": "object"},
        },
        "required": ["model", "questions"],
    })
}

fn item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": ["string", "integer"], "description": "Echoed in the result; defaults to the index."},
            "state": {"description": "What the model reads: a string or any JSON value."},
        },
        "required": ["state"],
    })
}

fn build(body: &mut Map<String, Value>, item: &Map<String, Value>) -> Result<(), String> {
    body.insert("state".into(), item["state"].clone());
    Ok(())
}
