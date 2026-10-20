//! MCP tools: the [`Tool`] trait and the one registration list ([`all`]).
//!
//! A new tool is a new module with a `Tool` value plus one line in [`all`]
//! (`forecast` was added this way in stage 7).

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{decide, extract, forecast, models};
use crate::catalog::Catalog;
use crate::daemon::AppState;
use crate::images::Origin;

/// How long clients may reuse a `tools/list` result. The list changes only
/// when the daemon restarts with another catalog.
pub const TOOLS_TTL_MS: u64 = 3_600_000;

/// Everything a tool call may use.
pub struct CallCtx {
    pub state: AppState,
    /// The listener of the MCP request (only the Unix socket may name files).
    pub origin: Origin,
    /// Set when the client asked for progress (`_meta.progressToken`).
    pub progress: Option<Progress>,
}

/// Sends `notifications/progress` on the response stream of the request.
#[derive(Clone)]
pub struct Progress {
    pub(super) token: Value,
    pub(super) tx: mpsc::UnboundedSender<super::Event>,
}

impl Progress {
    /// Report `done` of `total`; `done` must grow with each call.
    pub fn report(&self, done: u64, total: u64, message: &str) {
        let msg = super::jsonrpc::progress(&self.token, done, total, message);
        let _ = self.tx.send(super::Event::Notify(msg));
    }

    /// True when the client has gone away (it closed the response stream).
    pub fn closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Result of a tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutput {
    /// Structured content; it conforms to the tool's `outputSchema`.
    Ok(Value),
    /// A tool execution error (`isError: true`): an s1 error object
    /// `{code, message, ...}` the caller can read and act on.
    Err(Value),
}

impl ToolOutput {
    /// An error with an s1 code and a message.
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        ToolOutput::Err(json!({"code": code, "message": message.into()}))
    }

    /// The `tools/call` result body (without the shared `_meta`).
    pub fn to_result(&self) -> Value {
        match self {
            ToolOutput::Ok(v) => json!({
                "resultType": "complete",
                "content": [{"type": "text", "text": v.to_string()}],
                "structuredContent": v,
                "isError": false,
            }),
            ToolOutput::Err(e) => json!({
                "resultType": "complete",
                "content": [{"type": "text", "text": json!({"error": e}).to_string()}],
                "isError": true,
            }),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    /// The `tools/list` entry: `name`, `title`, `description`,
    /// `inputSchema`, `outputSchema`. It may depend only on the catalog.
    fn definition(&self, catalog: &Catalog) -> Value;
    async fn call(&self, ctx: CallCtx, args: Value) -> ToolOutput;
}

/// Every tool, in `tools/list` order.
static ALL: [&dyn Tool; 4] = [
    &decide::TOOL,
    &extract::TOOL,
    &forecast::Forecast,
    &models::Models,
];

pub fn all() -> &'static [&'static dyn Tool] {
    &ALL
}

pub fn find(name: &str) -> Option<&'static dyn Tool> {
    all().iter().copied().find(|t| t.name() == name)
}

/// The `tools/list` result: static for one catalog, cacheable by anyone
/// (`cacheScope: public`); `_meta["s1/catalogVersion"]` names the version.
pub fn list_result(catalog: &Catalog) -> Value {
    let tools: Vec<Value> = all().iter().map(|t| t.definition(catalog)).collect();
    let version = blake3::hash(
        serde_json::to_string(&tools)
            .expect("tools serialize")
            .as_bytes(),
    )
    .to_hex()[..16]
        .to_string();
    json!({
        "resultType": "complete",
        "tools": tools,
        "ttlMs": TOOLS_TTL_MS,
        "cacheScope": "public",
        "_meta": {"s1/catalogVersion": version},
    })
}

/// Model ids and aliases of a family, for tool descriptions.
pub fn family_models(catalog: &Catalog, family: crate::protocol::Family) -> Vec<String> {
    let mut out: Vec<String> = catalog
        .aliases
        .iter()
        .filter(|(_, a)| {
            a.models
                .first()
                .and_then(|m| catalog.model(m))
                .is_some_and(|m| m.family() == family)
        })
        .map(|(name, _)| name.clone())
        .collect();
    out.extend(
        catalog
            .models()
            .filter(|m| m.family() == family)
            .map(|m| m.id()),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_schemas_and_a_valid_name() {
        let cat = Catalog::builtin();
        let list = list_result(&cat);
        let names: Vec<&str> = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["decide", "extract", "forecast", "models"]);
        for t in list["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            assert!(t["outputSchema"].is_object(), "{t}");
            assert!(t["description"].as_str().unwrap().len() > 20);
            let n = t["name"].as_str().unwrap();
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            );
        }
        assert_eq!(list["cacheScope"], "public");
        assert_eq!(list["ttlMs"], TOOLS_TTL_MS);
    }

    #[test]
    fn list_is_static_per_catalog_version() {
        let a = list_result(&Catalog::builtin());
        let b = list_result(&Catalog::builtin());
        assert_eq!(a, b);
        let other = Catalog::with_overlay_str(
            "[aliases.fast]\nmodels = [\"workers-ai/clef-flash\"]\nspill = false\n",
        )
        .unwrap();
        let c = list_result(&other);
        assert_ne!(
            a["_meta"]["s1/catalogVersion"],
            c["_meta"]["s1/catalogVersion"]
        );
        let decide = c["tools"][0]["description"].as_str().unwrap();
        assert!(decide.contains("fast"), "{decide}");
        assert!(decide.contains("workers-ai/clef-flash"), "{decide}");
        assert!(!decide.contains("workers-ai/clairic"), "{decide}");
    }

    #[test]
    fn results_carry_text_and_structured_content() {
        let ok = ToolOutput::Ok(json!({"results": []})).to_result();
        assert_eq!(ok["isError"], false);
        assert_eq!(ok["resultType"], "complete");
        assert_eq!(ok["structuredContent"], json!({"results": []}));
        let text: Value = serde_json::from_str(ok["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, json!({"results": []}));
        let err = ToolOutput::error("invalid_request", "nope").to_result();
        assert_eq!(err["isError"], true);
        assert!(err.get("structuredContent").is_none());
        assert!(
            err["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid_request")
        );
    }
}
