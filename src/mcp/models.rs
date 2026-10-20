//! The `models` tool: the merged catalog, aliases, and live state, as
//! `GET /v1/models` returns them.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::tools::{CallCtx, Tool, ToolOutput};
use crate::catalog::Catalog;

pub struct Models;

#[async_trait]
impl Tool for Models {
    fn name(&self) -> &'static str {
        "models"
    }

    fn definition(&self, _catalog: &Catalog) -> Value {
        json!({
            "name": "models",
            "title": "Models and aliases",
            "description": "Lists the s1 models (id, family, capabilities, and state: available, \
                            throttled_until, breaker_open) and the aliases. Use an id or an alias \
                            as `template.model` in decide and extract.",
            "inputSchema": {"type": "object", "additionalProperties": false},
            "outputSchema": {
                "type": "object",
                "properties": {
                    "object": {"type": "string"},
                    "data": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "family": {"type": "string"},
                                "capabilities": {"type": "object"},
                                "state": {"type": "object"},
                            },
                            "required": ["id", "family", "state"],
                        },
                    },
                    "aliases": {"type": "object"},
                },
                "required": ["data", "aliases"],
            },
        })
    }

    async fn call(&self, ctx: CallCtx, args: Value) -> ToolOutput {
        if args.as_object().is_some_and(|a| !a.is_empty()) {
            return ToolOutput::error("invalid_request", "`models` takes no arguments");
        }
        ToolOutput::Ok(crate::daemon::models::models_json(&ctx.state))
    }
}
