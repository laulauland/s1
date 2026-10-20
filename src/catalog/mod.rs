//! Model catalog: a compiled-in catalog for the v1 backends merged with the
//! user overlay `~/.config/s1/models.toml`.
//!
//! Merge rules: tables merge key by key (overlay wins); provider `models`
//! arrays merge by `id` (same id: fields override; new id: appended); other
//! arrays, alias `models` included, are replaced. Unknown keys are kept
//! (later stages read e.g. `[limits.*]`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use toml::Value;

use crate::limits::Limits;
use crate::protocol::{Family, ProtocolId};

const BUILTIN: &str = include_str!("builtin.toml");

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
    #[error("catalog: {0}")]
    Invalid(String),
}

/// How a model takes images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Images {
    #[default]
    None,
    Yes,
    /// Needs an opt-in header.
    Preview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// Id sent to the provider. Defaults to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Per-model protocol; defaults to the provider's `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ProtocolId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_options: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_levels: Option<u32>,
    /// Fewest choice options the model accepts (Clef: 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_options: Option<u32>,
    /// Fewest score levels the model accepts (Clef: 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_levels: Option<u32>,
    /// Most questions in one request (Clef: 64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_questions: Option<u32>,
    /// Context one question may use without the long-context opt-in (Jev:
    /// 32,768; the long context covers questions too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_context_tokens: Option<u64>,
    /// Context with the provider's long-context opt-in (Jev: 262,144 with
    /// `TypeSafe-Danger-Zone-Long-Context: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_tokens: Option<u64>,
    #[serde(default)]
    pub images: Images,
    #[serde(default)]
    pub compat: BTreeMap<String, Value>,
}

impl Model {
    /// A boolean `compat` flag, or `default` when unset.
    pub fn compat_flag(&self, key: &str, default: bool) -> bool {
        self.compat
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(default)
    }

    /// Id sent to the provider.
    pub fn upstream_id(&self) -> &str {
        self.upstream.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    pub base_url: String,
    pub api: ProtocolId,
    /// Entry name in `auth.json`. Defaults to the provider name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alias {
    pub models: Vec<String>,
    /// Use later entries when the first is throttled, not only when unavailable.
    #[serde(default)]
    pub spill: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub aliases: BTreeMap<String, Alias>,
    /// `[limits.<provider>]` tables; read with [`Catalog::limits_for`].
    #[serde(default)]
    pub limits: BTreeMap<String, Value>,
}

/// A model with its provider.
#[derive(Debug, Clone, Copy)]
pub struct ModelRef<'a> {
    pub provider_name: &'a str,
    pub provider: &'a Provider,
    pub model: &'a Model,
}

impl ModelRef<'_> {
    /// `provider/model`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider_name, self.model.id)
    }
    pub fn api(&self) -> ProtocolId {
        self.model.api.unwrap_or(self.provider.api)
    }
    pub fn family(&self) -> Family {
        self.api().family()
    }
    pub fn upstream(&self) -> &str {
        self.model.upstream_id()
    }
    pub fn auth_entry(&self) -> &str {
        self.provider.auth.as_deref().unwrap_or(self.provider_name)
    }
}

impl Catalog {
    /// The compiled-in catalog only.
    pub fn builtin() -> Self {
        Self::from_value(builtin_value()).expect("builtin catalog is valid")
    }

    /// Built-in catalog merged with an overlay file, if it exists.
    pub fn load(overlay: &Path) -> Result<Self, CatalogError> {
        let text = match std::fs::read_to_string(overlay) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(CatalogError::Io {
                    path: overlay.display().to_string(),
                    source,
                });
            }
        };
        match text {
            None => Ok(Self::builtin()),
            Some(t) => Self::with_overlay_str(&t).map_err(|e| match e {
                CatalogError::Parse { message, .. } => CatalogError::Parse {
                    path: overlay.display().to_string(),
                    message,
                },
                other => other,
            }),
        }
    }

    /// Built-in catalog merged with overlay TOML text.
    pub fn with_overlay_str(overlay: &str) -> Result<Self, CatalogError> {
        let overlay: Value = toml::from_str(overlay).map_err(|e| CatalogError::Parse {
            path: "models.toml".into(),
            message: e.to_string(),
        })?;
        let mut base = builtin_value();
        merge(&mut base, overlay);
        Self::from_value(base)
    }

    fn from_value(v: Value) -> Result<Self, CatalogError> {
        let cat: Catalog = v
            .try_into()
            .map_err(|e: toml::de::Error| CatalogError::Parse {
                path: "models.toml".into(),
                message: e.to_string(),
            })?;
        cat.validate()?;
        Ok(cat)
    }

    fn validate(&self) -> Result<(), CatalogError> {
        for (name, p) in &self.providers {
            if name.contains('/') {
                return Err(CatalogError::Invalid(format!(
                    "provider name `{name}` must not contain `/`"
                )));
            }
            let mut seen = std::collections::HashSet::new();
            for m in &p.models {
                if !seen.insert(&m.id) {
                    return Err(CatalogError::Invalid(format!(
                        "provider `{name}` lists model `{}` twice",
                        m.id
                    )));
                }
            }
        }
        for provider in self.limits.keys() {
            self.limits_for(provider)?;
        }
        for (alias, a) in &self.aliases {
            if a.models.is_empty() {
                return Err(CatalogError::Invalid(format!(
                    "alias `{alias}` has no models"
                )));
            }
            for target in &a.models {
                if self.model(target).is_none() && !self.aliases.contains_key(target) {
                    return Err(CatalogError::Invalid(format!(
                        "alias `{alias}` names unknown model `{target}`"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Limits for one provider: its `[limits.<provider>]` table over the
    /// defaults ([`Limits::default`]).
    pub fn limits_for(&self, provider: &str) -> Result<Limits, CatalogError> {
        match self.limits.get(provider) {
            None => Ok(Limits::default()),
            Some(v) => Limits::from_toml(v)
                .map_err(|e| CatalogError::Invalid(format!("[limits.{provider}]: {e}"))),
        }
    }

    /// All models, sorted by `provider/model`.
    pub fn models(&self) -> impl Iterator<Item = ModelRef<'_>> {
        self.providers.iter().flat_map(|(name, p)| {
            p.models.iter().map(move |m| ModelRef {
                provider_name: name,
                provider: p,
                model: m,
            })
        })
    }

    /// Look up a `provider/model` id.
    pub fn model(&self, id: &str) -> Option<ModelRef<'_>> {
        let (provider_name, model_id) = id.split_once('/')?;
        let (provider_name, provider) = self.providers.get_key_value(provider_name)?;
        let model = provider.models.iter().find(|m| m.id == model_id)?;
        Some(ModelRef {
            provider_name,
            provider,
            model,
        })
    }

    /// Look up a model of one provider by catalog id or upstream id.
    pub fn provider_model(&self, provider: &str, model: &str) -> Option<ModelRef<'_>> {
        let (provider_name, p) = self.providers.get_key_value(provider)?;
        let m = p.models.iter().find(|m| m.id == model).or_else(|| {
            p.models
                .iter()
                .find(|m| m.upstream.as_deref() == Some(model))
        })?;
        Some(ModelRef {
            provider_name,
            provider: p,
            model: m,
        })
    }

    /// `GET /v1/models` body. `state` gives the live state per provider.
    pub fn to_models_json(&self, state: impl Fn(&ModelRef<'_>) -> ModelState) -> Json {
        let data: Vec<Json> = self
            .models()
            .map(|m| {
                let s = state(&m);
                json!({
                    "id": m.id(),
                    "object": "model",
                    "owned_by": m.provider_name,
                    "provider": m.provider_name,
                    "name": m.model.id,
                    "upstream": m.upstream(),
                    "api": m.api(),
                    "family": m.family(),
                    "base_url": m.provider.base_url,
                    "capabilities": {
                        "context_tokens": m.model.context_tokens,
                        "max_options": m.model.max_options,
                        "max_levels": m.model.max_levels,
                        "min_options": m.model.min_options,
                        "min_levels": m.model.min_levels,
                        "max_questions": m.model.max_questions,
                        "long_context_tokens": m.model.long_context_tokens,
                        "images": m.model.images,
                    },
                    "compat": m.model.compat,
                    "state": {
                        "available": s.available,
                        "throttled_until": s.throttled_until,
                        "breaker_open": s.breaker_open,
                    },
                })
            })
            .collect();
        json!({ "object": "list", "data": data, "aliases": self.aliases })
    }
}

/// Live state for one model, shown by `GET /v1/models`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelState {
    pub available: bool,
    pub throttled_until: Option<String>,
    pub breaker_open: bool,
}

fn builtin_value() -> Value {
    toml::from_str(BUILTIN).expect("builtin catalog parses")
}

/// Deep merge `overlay` into `base` (see module docs).
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, ov) in o {
                match b.get_mut(&k) {
                    Some(bv) if k == "models" => merge_models(bv, ov),
                    Some(bv) => merge(bv, ov),
                    None => {
                        b.insert(k, ov);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn merge_models(base: &mut Value, overlay: Value) {
    // Only provider model lists (tables with `id`, on both sides) merge by id.
    // Alias model lists (strings) are replaced, so an empty overlay list
    // empties the alias and validation rejects it.
    let (Value::Array(b), Value::Array(o)) = (&mut *base, &overlay) else {
        *base = overlay;
        return;
    };
    let is_record = |m: &Value| m.get("id").is_some_and(Value::is_str);
    if !b.iter().all(is_record) || !o.iter().all(is_record) {
        *base = overlay;
        return;
    }
    for om in o {
        let id = om.get("id").and_then(Value::as_str);
        let existing = id.and_then(|id| {
            b.iter_mut()
                .find(|bm| bm.get("id").and_then(Value::as_str) == Some(id))
        });
        match existing {
            Some(bm) => merge(bm, om.clone()),
            None => b.push(om.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn builtin_has_the_four_v1_backends() {
        let c = Catalog::builtin();
        let jev = c.model("typesafe/jev-latest").unwrap();
        assert_eq!(jev.api(), ProtocolId::Systemone);
        assert_eq!(jev.upstream(), "jev-latest");
        assert_eq!(jev.model.images, Images::Preview);
        assert_eq!(
            c.model("workers-ai/clef").unwrap().api(),
            ProtocolId::Systemone
        );
        assert_eq!(
            c.model("workers-ai/clairic").unwrap().api(),
            ProtocolId::Clairic
        );
        assert_eq!(
            c.model("openai/gpt-6-luna").unwrap().api(),
            ProtocolId::OpenaiDecisions
        );
        assert_eq!(c.aliases["decide"].models[1], "typesafe/jev-latest");
        assert!(!c.aliases["decide"].spill);
    }

    #[test]
    fn overlay_adds_providers_and_models() {
        let c = Catalog::with_overlay_str(
            r#"
            [providers.local]
            base_url = "http://127.0.0.1:9000"
            api = "systemone"
            [[providers.local.models]]
            id = "tiny"

            [[providers.typesafe.models]]
            id = "jev-1.13.0"
            context_tokens = 65536
            "#,
        )
        .unwrap();
        let tiny = c.model("local/tiny").unwrap();
        assert_eq!(tiny.upstream(), "tiny");
        assert_eq!(tiny.auth_entry(), "local");
        assert!(c.model("typesafe/jev-1.13.0").is_some());
        assert!(c.model("typesafe/jev-latest").is_some(), "built-ins stay");
    }

    #[test]
    fn overlay_overrides_fields() {
        let c = Catalog::with_overlay_str(
            r#"
            [providers.typesafe]
            base_url = "http://127.0.0.1:1234"
            [[providers.typesafe.models]]
            id = "jev-latest"
            context_tokens = 262144
            [aliases.decide]
            models = ["typesafe/jev-latest"]
            spill = true
            "#,
        )
        .unwrap();
        let jev = c.model("typesafe/jev-latest").unwrap();
        assert_eq!(jev.provider.base_url, "http://127.0.0.1:1234");
        assert_eq!(jev.model.context_tokens, Some(262144));
        assert_eq!(jev.model.max_options, Some(255), "unset fields stay");
        assert_eq!(c.aliases["decide"].models, vec!["typesafe/jev-latest"]);
        assert!(c.aliases["decide"].spill);
    }

    #[test]
    fn overlay_errors_are_reported() {
        assert!(Catalog::with_overlay_str("[providers.typesafe").is_err());
        assert!(
            Catalog::with_overlay_str("[providers.typesafe]\napi = \"nope\"").is_err(),
            "unknown protocol"
        );
        let err = Catalog::with_overlay_str("[aliases.x]\nmodels = [\"no/such\"]").unwrap_err();
        assert!(err.to_string().contains("no/such"));
    }

    #[test]
    fn overlay_alias_arrays_replace_and_empty_is_an_error() {
        let err = Catalog::with_overlay_str("[aliases.decide]\nmodels = []\n").unwrap_err();
        assert!(err.to_string().contains("decide"), "{err}");
        let c = Catalog::with_overlay_str("[providers.typesafe]\nmodels = []\n").unwrap();
        assert!(
            c.model("typesafe/jev-latest").is_some(),
            "provider model lists merge by id"
        );
    }

    #[test]
    fn limits_come_from_the_catalog() {
        let c = Catalog::builtin();
        let ts = c.limits_for("typesafe").unwrap();
        assert!(ts.rpm.is_some() && ts.tpm.is_some(), "placeholders are set");
        let c = Catalog::with_overlay_str("[limits.typesafe]\nrpm = 5\n").unwrap();
        assert_eq!(c.limits_for("typesafe").unwrap().rpm, Some(5));
        assert_eq!(
            c.limits_for("typesafe").unwrap().tpm,
            ts.tpm,
            "other built-in fields stay"
        );
        assert_eq!(c.limits_for("nobody").unwrap(), Limits::default());
        let err = Catalog::with_overlay_str("[limits.typesafe]\nmax_in_flight = 0\n").unwrap_err();
        assert!(err.to_string().contains("limits.typesafe"), "{err}");
    }

    #[test]
    fn missing_overlay_file_gives_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let c = Catalog::load(&dir.path().join("models.toml")).unwrap();
        assert_eq!(c, Catalog::builtin());
    }

    #[test]
    fn models_json_lists_every_model_with_state() {
        let c = Catalog::builtin();
        let v = c.to_models_json(|m| ModelState {
            available: m.provider_name == "typesafe",
            ..Default::default()
        });
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), c.models().count());
        let jev = data
            .iter()
            .find(|m| m["id"] == "typesafe/jev-latest")
            .unwrap();
        assert_eq!(jev["api"], "systemone");
        assert_eq!(jev["family"], "decide");
        assert_eq!(jev["capabilities"]["max_levels"], 10);
        assert_eq!(jev["state"]["available"], true);
        assert_eq!(jev["state"]["breaker_open"], false);
        assert!(v["aliases"]["decide"]["models"].is_array());
    }

    proptest! {
        /// Overlay models are added or override; built-in models never vanish.
        #[test]
        fn merge_keeps_builtin_models_and_adds_overlay(
            ids in proptest::collection::btree_set("[a-z][a-z0-9-]{0,8}", 0..6),
            ctx in 1u64..1_000_000,
        ) {
            let mut overlay = String::new();
            for id in &ids {
                overlay.push_str(&format!(
                    "[[providers.typesafe.models]]\nid = \"{id}\"\ncontext_tokens = {ctx}\n"
                ));
            }
            let c = Catalog::with_overlay_str(&overlay).unwrap();
            for m in Catalog::builtin().models() {
                prop_assert!(c.model(&m.id()).is_some());
            }
            for id in &ids {
                let m = c.model(&format!("typesafe/{id}")).unwrap();
                prop_assert_eq!(m.model.context_tokens, Some(ctx));
            }
        }
    }
}
