//! Routing by the `model` field (spec "Model catalog and routing").
//!
//! - The endpoint fixes the family; a model of another family is `wrong_family`.
//! - An id (`provider/model`, or a bare model name that is unique in the
//!   family) resolves to one backend. Its capabilities must fit, else
//!   `too_long` or `unsupported`.
//! - An alias resolves to an ordered list. The first entry that is available
//!   and whose capabilities fit (context, images, option and level counts)
//!   wins. Throttle-based spill comes with the limiters (stage 3), through the
//!   `available` callback.
//! - Formats are not translated yet (stage 4): an entry must speak the
//!   ingress format natively.

use std::sync::Arc;

use crate::backend::{Backend, Registry};
use crate::catalog::{Catalog, Images, Model, ModelRef};
use crate::error::S1Error;
use crate::protocol::{ProtocolId, RouteInfo};

/// Opt-in header that raises Jev's context to its long-context size.
pub const LONG_CONTEXT_HEADER: &str = "typesafe-danger-zone-long-context";

/// Where a request goes.
#[derive(Clone)]
pub struct Route {
    pub backend: Arc<dyn Backend>,
    /// Provider name in the catalog.
    pub provider: String,
    /// `provider/model`.
    pub model_id: String,
    /// Id sent to the provider.
    pub upstream: String,
    pub model: Model,
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("model_id", &self.model_id)
            .field("upstream", &self.upstream)
            .finish()
    }
}

/// Request facts that are not in the body.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestFlags {
    /// The client opted in to long context.
    pub long_context: bool,
}

/// Inputs of one routing decision.
pub struct Router<'a> {
    pub catalog: &'a Catalog,
    pub backends: &'a Registry,
    /// Ok if the model can take a request now (backend and a usable key;
    /// later also not throttled and breaker closed). The error says why not
    /// and should be `backend_unavailable`.
    pub available: &'a dyn Fn(&ModelRef<'_>) -> Result<(), S1Error>,
}

/// How strictly to estimate the input size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimate {
    /// Choosing among alias entries: estimate high, prefer a roomier model.
    High,
    /// A named model: reject only what surely fails.
    Low,
}

impl Router<'_> {
    pub fn route(
        &self,
        ingress: ProtocolId,
        info: &RouteInfo,
        flags: RequestFlags,
    ) -> Result<Route, S1Error> {
        if let Some(entries) = self.expand_alias(&info.model)? {
            return self.pick(ingress, &info.model, &entries, info, flags);
        }
        let m = self.resolve_id(ingress, &info.model)?;
        self.check(ingress, &m, info, flags, Estimate::Low)?;
        self.finish(&m)
    }

    /// Alias targets in order, aliases of aliases expanded. `None` if `name`
    /// is not an alias.
    fn expand_alias(&self, name: &str) -> Result<Option<Vec<ModelRef<'_>>>, S1Error> {
        if !self.catalog.aliases.contains_key(name) {
            return Ok(None);
        }
        let mut out = Vec::new();
        let mut stack = vec![(name.to_string(), 0usize)];
        while let Some((n, depth)) = stack.pop() {
            if depth > 8 {
                return Err(S1Error::Unsupported(format!(
                    "alias `{name}` nests too deep"
                )));
            }
            match self.catalog.aliases.get(&n) {
                Some(a) => {
                    for t in a.models.iter().rev() {
                        stack.push((t.clone(), depth + 1));
                    }
                }
                None => {
                    if let Some(m) = self.catalog.model(&n) {
                        out.push(m);
                    }
                }
            }
        }
        Ok(Some(out))
    }

    /// A `provider/model` id, or a bare name unique among the models.
    fn resolve_id(&self, ingress: ProtocolId, name: &str) -> Result<ModelRef<'_>, S1Error> {
        if let Some(m) = self.catalog.model(name) {
            return Ok(m);
        }
        let matches: Vec<ModelRef<'_>> = self
            .catalog
            .models()
            .filter(|m| m.model.id == name || m.upstream() == name)
            .collect();
        // Prefer models of the endpoint's format, then of its family.
        let pick = matches
            .iter()
            .find(|m| m.api() == ingress)
            .or_else(|| matches.iter().find(|m| m.family() == ingress.family()))
            .or(matches.first());
        pick.copied().ok_or_else(|| {
            S1Error::Unsupported(format!(
                "unknown model `{name}`; see `s1 models` for ids and aliases"
            ))
        })
    }

    fn pick(
        &self,
        ingress: ProtocolId,
        alias: &str,
        entries: &[ModelRef<'_>],
        info: &RouteInfo,
        flags: RequestFlags,
    ) -> Result<Route, S1Error> {
        let mut first_misfit: Option<S1Error> = None;
        let mut any_unavailable = false;
        let mut skipped = Vec::new();
        for m in entries {
            match self.check(ingress, m, info, flags, Estimate::High) {
                Ok(()) => return self.finish(m),
                Err(S1Error::WrongFamily(_)) => {}
                Err(e @ S1Error::BackendUnavailable(_)) => {
                    any_unavailable = true;
                    skipped.push(format!("{}: {e}", m.id()));
                }
                Err(e) => {
                    skipped.push(format!("{}: {e}", m.id()));
                    first_misfit.get_or_insert(e);
                }
            }
        }
        if any_unavailable {
            return Err(S1Error::BackendUnavailable(format!(
                "no model of alias `{alias}` can take this request now ({})",
                skipped.join("; ")
            )));
        }
        // Every entry of the family is unfit: the request's problem, not an outage.
        Err(first_misfit.unwrap_or_else(|| {
            S1Error::WrongFamily(format!(
                "alias `{alias}` has no {0} model; this endpoint is for {0} requests",
                ingress.family().as_str()
            ))
        }))
    }

    /// Family, format, availability, then capabilities.
    fn check(
        &self,
        ingress: ProtocolId,
        m: &ModelRef<'_>,
        info: &RouteInfo,
        flags: RequestFlags,
        est: Estimate,
    ) -> Result<(), S1Error> {
        if m.family() != ingress.family() {
            return Err(S1Error::WrongFamily(format!(
                "model `{}` is a {} model; this endpoint is for {} requests",
                m.id(),
                m.family().as_str(),
                ingress.family().as_str()
            )));
        }
        if m.api() != ingress {
            return Err(S1Error::Unsupported(format!(
                "model `{}` speaks `{}`; translation from `{ingress}` is not in this build",
                m.id(),
                m.api()
            )));
        }
        let Some(backend) = self.backends.get(m.provider_name) else {
            return Err(S1Error::BackendUnavailable(format!(
                "provider `{}` has no backend in this build",
                m.provider_name
            )));
        };
        if !backend.protocols().contains(&ingress) {
            return Err(S1Error::Unsupported(format!(
                "provider `{}` does not speak `{ingress}`",
                m.provider_name
            )));
        }
        fits(m, info, flags, est)?;
        (self.available)(m)
    }

    fn finish(&self, m: &ModelRef<'_>) -> Result<Route, S1Error> {
        let backend = self.backends.get(m.provider_name).cloned().ok_or_else(|| {
            S1Error::BackendUnavailable(format!(
                "provider `{}` has no backend in this build",
                m.provider_name
            ))
        })?;
        Ok(Route {
            backend,
            provider: m.provider_name.to_string(),
            model_id: m.id(),
            upstream: m.upstream().to_string(),
            model: m.model.clone(),
        })
    }
}

/// Context a model offers for this request: its long context when the
/// client opted in or when the request needs it.
pub fn effective_context(model: &Model, tokens: u64, flags: RequestFlags) -> Option<u64> {
    let base = model.context_tokens?;
    match model.long_context_tokens {
        Some(long) if flags.long_context || tokens > base => Some(long),
        _ => Some(base),
    }
}

/// Do the model's capabilities fit the request?
pub fn fits(
    m: &ModelRef<'_>,
    info: &RouteInfo,
    flags: RequestFlags,
    est: Estimate,
) -> Result<(), S1Error> {
    let model = m.model;
    let id = m.id();
    let tokens = match est {
        Estimate::High => info.tokens_high(),
        Estimate::Low => info.tokens_low(),
    };
    if let Some(ctx) = effective_context(model, tokens, flags)
        && tokens > ctx
    {
        return Err(S1Error::TooLong(format!(
            "request is about {tokens} tokens; `{id}` takes {ctx}"
        )));
    }
    let unsupported = |what: String| Err(S1Error::Unsupported(format!("`{id}` {what}")));
    if info.assets > 0 && (model.images == Images::None || !model.compat_flag("assets", true)) {
        return unsupported("does not take image `assets`".into());
    }
    if info.images > 0
        && (model.images == Images::None || !model.compat_flag("images_array", false))
    {
        return unsupported("does not take an `images` array".into());
    }
    if info.has_box && !model.compat_flag("box", false) {
        return unsupported("does not take `box` questions".into());
    }
    if info.missing_instructions && model.compat_flag("instructions_required", false) {
        return unsupported("needs non-blank `instructions` in every question".into());
    }
    if info.loose_question_names && model.compat_flag("strict_question_names", false) {
        return unsupported(
            "takes only question names of 1 to 100 characters from `A-Z a-z 0-9 _ . -`".into(),
        );
    }
    if let (Some(n), Some(max)) = (info.max_options, model.max_options)
        && n > max
    {
        return unsupported(format!("takes at most {max} choice options, not {n}"));
    }
    if let (Some(n), Some(min)) = (info.min_options, model.min_options)
        && n < min
    {
        return unsupported(format!("needs at least {min} choice options, not {n}"));
    }
    if let (Some(n), Some(max)) = (info.max_levels, model.max_levels)
        && n > max
    {
        return unsupported(format!("takes at most {max} score levels, not {n}"));
    }
    if let (Some(n), Some(min)) = (info.min_levels, model.min_levels)
        && n < min
    {
        return unsupported(format!("needs at least {min} score levels, not {n}"));
    }
    if let Some(max) = model.max_questions
        && info.questions > max
    {
        return unsupported(format!(
            "takes at most {max} questions, not {}",
            info.questions
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendContext;
    use crate::secrets::KeyResolver;

    fn registry(c: &Catalog) -> Registry {
        let dir = std::env::temp_dir();
        let keys = KeyResolver::new(dir.join("s1-router-test-none/auth.json"), |_| None);
        Registry::from_catalog(c, &BackendContext::new(keys))
    }

    fn info(model: &str) -> RouteInfo {
        RouteInfo {
            model: model.into(),
            input_bytes: 200,
            questions: 1,
            ..Default::default()
        }
    }

    fn route_with(
        c: &Catalog,
        info: &RouteInfo,
        available: &dyn Fn(&ModelRef<'_>) -> bool,
    ) -> Result<Route, S1Error> {
        let available = |m: &ModelRef<'_>| {
            if available(m) {
                Ok(())
            } else {
                Err(S1Error::BackendUnavailable(format!(
                    "no API key for provider `{}`; run `s1 auth set {}`",
                    m.provider_name,
                    m.auth_entry()
                )))
            }
        };
        let reg = registry(c);
        Router {
            catalog: c,
            backends: &reg,
            available: &available,
        }
        .route(ProtocolId::Systemone, info, RequestFlags::default())
    }

    fn route(c: &Catalog, info: &RouteInfo) -> Result<String, S1Error> {
        route_with(c, info, &|_| true).map(|r| r.model_id)
    }

    #[test]
    fn ids_and_bare_names_resolve_to_one_backend() {
        let c = Catalog::builtin();
        assert_eq!(
            route(&c, &info("typesafe/jev-latest")).unwrap(),
            "typesafe/jev-latest"
        );
        assert_eq!(
            route(&c, &info("jev-latest")).unwrap(),
            "typesafe/jev-latest"
        );
        assert_eq!(route(&c, &info("clef")).unwrap(), "workers-ai/clef");
        let r = route_with(&c, &info("workers-ai/clef-flash"), &|_| true).unwrap();
        assert_eq!(r.upstream, "clef-flash");
        assert_eq!(r.provider, "workers-ai");
        assert!(matches!(
            route(&c, &info("nope/never")),
            Err(S1Error::Unsupported(_))
        ));
    }

    #[test]
    fn endpoint_fixes_the_family() {
        let c = Catalog::builtin();
        let e = route(&c, &info("workers-ai/clairic")).unwrap_err();
        assert_eq!(e.code(), "wrong_family", "{e}");
        let e = route(&c, &info("extract")).unwrap_err();
        assert_eq!(e.code(), "wrong_family", "{e}");
        // Same family, other format: needs translation (stage 4).
        let e = route(&c, &info("openai/gpt-6-luna")).unwrap_err();
        assert_eq!(e.code(), "unsupported", "{e}");
    }

    #[test]
    fn alias_takes_first_available_entry() {
        let c = Catalog::builtin();
        assert_eq!(route(&c, &info("decide")).unwrap(), "workers-ai/clef-flash");
        let only_ts = |m: &ModelRef<'_>| m.provider_name == "typesafe";
        assert_eq!(
            route_with(&c, &info("decide"), &only_ts).unwrap().model_id,
            "typesafe/jev-latest"
        );
        let e = route_with(&c, &info("decide"), &|_| false).unwrap_err();
        assert_eq!(e.code(), "backend_unavailable");
        assert!(e.to_string().contains("s1 auth set"), "{e}");
    }

    #[test]
    fn alias_skips_entries_whose_context_does_not_fit() {
        let c = Catalog::builtin();
        // About 30k tokens (high estimate): over clef-flash's 24,576.
        let big = RouteInfo {
            input_bytes: 90_000,
            ..info("decide")
        };
        assert_eq!(route(&c, &big).unwrap(), "typesafe/jev-latest");
        // A named model rejects only what surely fails (low estimate).
        let e = route(
            &c,
            &RouteInfo {
                input_bytes: 120_000,
                ..info("workers-ai/clef-flash")
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "too_long");
        assert!(
            route(
                &c,
                &RouteInfo {
                    input_bytes: 90_000,
                    ..info("workers-ai/clef-flash")
                }
            )
            .is_ok()
        );
        // Beyond every entry: too_long, not an outage.
        let huge = RouteInfo {
            input_bytes: 2_000_000,
            ..info("decide")
        };
        assert_eq!(route(&c, &huge).unwrap_err().code(), "too_long");
    }

    #[test]
    fn jev_long_context_extends_the_fit() {
        let c = Catalog::builtin();
        // 400 KB: over 65,536 tokens, under 262,144.
        let long = RouteInfo {
            input_bytes: 400_000,
            ..info("typesafe/jev-latest")
        };
        assert!(route(&c, &long).is_ok());
        assert_eq!(
            effective_context(
                c.model("typesafe/jev-latest").unwrap().model,
                10,
                RequestFlags { long_context: true }
            ),
            Some(262_144)
        );
        assert_eq!(
            effective_context(
                c.model("workers-ai/clef").unwrap().model,
                70_000,
                RequestFlags { long_context: true }
            ),
            Some(65_536)
        );
    }

    #[test]
    fn alias_skips_entries_without_needed_features() {
        let c = Catalog::builtin();
        let assets = RouteInfo {
            assets: 1,
            ..info("decide")
        };
        assert_eq!(route(&c, &assets).unwrap(), "typesafe/jev-latest");
        let one_option = RouteInfo {
            max_options: Some(1),
            min_options: Some(1),
            ..info("decide")
        };
        assert_eq!(route(&c, &one_option).unwrap(), "typesafe/jev-latest");
        let many_questions = RouteInfo {
            questions: 65,
            ..info("decide")
        };
        assert_eq!(route(&c, &many_questions).unwrap(), "typesafe/jev-latest");
        let images = RouteInfo {
            images: 2,
            ..info("decide")
        };
        assert_eq!(route(&c, &images).unwrap(), "workers-ai/clef-flash");
        // Box needs jev-preview; the default alias has none.
        let boxq = RouteInfo {
            has_box: true,
            ..info("decide")
        };
        assert_eq!(route(&c, &boxq).unwrap_err().code(), "unsupported");
        assert!(
            route(
                &c,
                &RouteInfo {
                    has_box: true,
                    ..info("typesafe/jev-preview")
                }
            )
            .is_ok()
        );
        let too_many = RouteInfo {
            max_options: Some(256),
            min_options: Some(256),
            ..info("typesafe/jev-latest")
        };
        assert_eq!(route(&c, &too_many).unwrap_err().code(), "unsupported");
        let levels = RouteInfo {
            max_levels: Some(11),
            min_levels: Some(11),
            ..info("decide")
        };
        assert_eq!(route(&c, &levels).unwrap_err().code(), "unsupported");
    }

    #[test]
    fn alias_skips_clef_for_missing_instructions_and_loose_names() {
        let c = Catalog::builtin();
        let no_instructions = RouteInfo {
            missing_instructions: true,
            ..info("decide")
        };
        assert_eq!(route(&c, &no_instructions).unwrap(), "typesafe/jev-latest");
        let loose = RouteInfo {
            loose_question_names: true,
            ..info("decide")
        };
        assert_eq!(route(&c, &loose).unwrap(), "typesafe/jev-latest");
        let e = route(
            &c,
            &RouteInfo {
                missing_instructions: true,
                ..info("workers-ai/clef")
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "unsupported");
        assert!(e.to_string().contains("instructions"), "{e}");
        let e = route(
            &c,
            &RouteInfo {
                loose_question_names: true,
                ..info("workers-ai/clef-flash")
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("question names"), "{e}");
    }

    /// A 2 MB photo is about 2.7 MB of base64. Counted as text it would be
    /// about 680k tokens; counted per image it fits.
    #[test]
    fn image_payloads_do_not_count_as_text_tokens() {
        use crate::protocol::{Protocol, systemone::Systemone};
        let c = Catalog::builtin();
        let photo = "A".repeat(2_700_000);
        let inspect = |v: serde_json::Value| Systemone.inspect(&serde_json::to_vec(&v).unwrap());
        let assets = inspect(serde_json::json!({
            "model": "typesafe/jev-latest",
            "state": "photo-1",
            "questions": {"red": {"type": "noul", "instructions": "Is it red?"}},
            "assets": {"photo-1": {"type": "image", "format": "jpeg", "data_base64": photo}}
        }))
        .unwrap();
        assert_eq!(route(&c, &assets).unwrap(), "typesafe/jev-latest");
        let alias = RouteInfo {
            model: "decide".into(),
            ..assets
        };
        assert_eq!(route(&c, &alias).unwrap(), "typesafe/jev-latest");
        let images = inspect(serde_json::json!({
            "model": "decide",
            "state": "Is it red?",
            "questions": {"red": {"type": "noul", "instructions": "Is it red?"}},
            "images": [format!("data:image/jpeg;base64,{photo}")]
        }))
        .unwrap();
        assert_eq!(route(&c, &images).unwrap(), "workers-ai/clef-flash");
        // Each image still costs up to 1,027 tokens: 24 images do not fit
        // clef-flash's 24,576, and Jev takes no `images` array.
        let many = RouteInfo {
            images: 24,
            ..images
        };
        let e = route(&c, &many).unwrap_err();
        assert_eq!(e.code(), "too_long", "{e}");
        assert!(e.to_string().contains("clef-flash"), "{e}");
    }
    #[test]
    fn nested_aliases_expand_in_order() {
        let c = Catalog::with_overlay_str(
            r#"
            [aliases.fast]
            models = ["workers-ai/clef-flash"]
            [aliases.chain]
            models = ["fast", "typesafe/jev-latest"]
            "#,
        )
        .unwrap();
        assert_eq!(route(&c, &info("chain")).unwrap(), "workers-ai/clef-flash");
        let no_cf = |m: &ModelRef<'_>| m.provider_name != "workers-ai";
        assert_eq!(
            route_with(&c, &info("chain"), &no_cf).unwrap().model_id,
            "typesafe/jev-latest"
        );
    }
}
