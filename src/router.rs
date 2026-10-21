//! Routing by the `model` field (spec "Model catalog and routing").
//!
//! - The endpoint fixes the family; a model of another family is `wrong_family`.
//! - An id (`provider/model`, or a bare model name that is unique in the
//!   family) resolves to one backend. Its capabilities must fit, else
//!   `too_long` or `unsupported`.
//! - An alias resolves to an ordered list. The first entry that is available
//!   and whose capabilities fit (context, images, option and level counts)
//!   wins. An entry whose circuit breaker is open is unavailable. A
//!   throttled entry (provider pause, queue, or no room under its limits)
//!   is skipped only when the alias sets `spill = true`; if every fitting
//!   entry is throttled, the first one takes the request and it queues.
//! - An entry may speak another format of the same family when s1 can
//!   translate between them ([`crate::translate::can`]); [`Route::api`] is
//!   the format the backend gets.

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
    /// Format the backend speaks; the ingress translates when it differs.
    pub api: ProtocolId,
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
    /// Ok if the model can take a request now (backend, a usable key, and
    /// a closed breaker). The error says why not; its code should be
    /// `backend_unavailable`.
    pub available: &'a dyn Fn(&ModelRef<'_>) -> Result<(), S1Error>,
    /// True if a request to the model would wait now. Used only for aliases
    /// with `spill = true`.
    pub throttled: &'a dyn Fn(&ModelRef<'_>) -> bool,
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
        // A family with no model at all (forecast in v1) has no backend.
        let family = ingress.family();
        if !self.catalog.models().any(|m| m.family() == family) {
            return Err(S1Error::BackendUnavailable(format!(
                "no {0} backend is configured; add a `{ingress}` provider to models.toml",
                family.as_str()
            )));
        }
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
        let spill = self.catalog.aliases.get(alias).is_some_and(|a| a.spill);
        let mut first_misfit: Option<S1Error> = None;
        let mut first_throttled: Option<&ModelRef<'_>> = None;
        let mut any_unavailable = false;
        // Shortest breaker cool-down among unavailable entries, and whether
        // every unavailable entry was skipped for an open breaker.
        let mut breaker_wait: Option<u64> = None;
        let mut all_breaker = true;
        let mut skipped = Vec::new();
        for m in entries {
            match self.check(ingress, m, info, flags, Estimate::High) {
                Ok(()) if spill && (self.throttled)(m) => {
                    first_throttled.get_or_insert(m);
                }
                Ok(()) => return self.finish(m),
                Err(S1Error::WrongFamily(_)) => {}
                Err(e) if e.code() == "backend_unavailable" => {
                    match &e {
                        S1Error::BreakerOpen {
                            retry_after_secs, ..
                        } => {
                            let secs = *retry_after_secs;
                            breaker_wait = Some(breaker_wait.map_or(secs, |w| w.min(secs)));
                        }
                        _ => all_breaker = false,
                    }
                    any_unavailable = true;
                    skipped.push(format!("{}: {e}", m.id()));
                }
                Err(e) => {
                    skipped.push(format!("{}: {e}", m.id()));
                    first_misfit.get_or_insert(e);
                }
            }
        }
        // Every fitting entry is throttled: queue on the first.
        if let Some(m) = first_throttled {
            return self.finish(m);
        }
        if any_unavailable {
            let message = format!(
                "no model of alias `{alias}` can take this request now ({})",
                skipped.join("; ")
            );
            // Every unavailable entry has an open breaker: tell the client
            // when the first one cools down.
            return Err(match breaker_wait.filter(|_| all_breaker) {
                Some(retry_after_secs) => S1Error::BreakerOpen {
                    message,
                    retry_after_secs,
                },
                None => S1Error::BackendUnavailable(message),
            });
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
        let api = m.api();
        if !crate::translate::can(ingress, api) {
            return Err(S1Error::Unsupported(format!(
                "model `{}` speaks `{api}`; translation from `{ingress}` is not in this build",
                m.id(),
            )));
        }
        let Some(backend) = self.backends.get(m.provider_name) else {
            return Err(S1Error::BackendUnavailable(format!(
                "provider `{}` has no backend in this build",
                m.provider_name
            )));
        };
        if !backend.protocols().contains(&api) {
            return Err(S1Error::Unsupported(format!(
                "provider `{}` does not speak `{api}`",
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
            api: m.api(),
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
    // Translated toward OpenAI, every asset reference is one image.
    let info = &*info.toward(m.api());
    if m.api() == ProtocolId::OpenaiDecisions
        && info
            .openai_images
            .is_some_and(|n| n as usize > crate::translate::decide::MAX_OPENAI_IMAGES)
    {
        return Err(S1Error::Unsupported(format!(
            "`{id}` takes at most {} images (each asset reference counts once)",
            crate::translate::decide::MAX_OPENAI_IMAGES
        )));
    }
    if model.compat_flag("truncates", false) {
        // The provider cuts long input to fit and says so in the response
        // (Clairic `truncated: true`); only its own size pre-check refuses.
        let tokens = info.tokens_low();
        if let Some(max) = model
            .compat
            .get("max_request_tokens")
            .and_then(toml::Value::as_integer)
            .and_then(|v| u64::try_from(v).ok())
            && tokens > max
        {
            return Err(S1Error::TooLong(format!(
                "request is about {tokens} tokens; `{id}` refuses more than {max}"
            )));
        }
    } else {
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
        // A per-question limit with no opt-in to raise it (Baseten: 32k
        // for the state plus one question). Jev's opt-in covers it.
        if let Some(ctx) = model.question_context_tokens
            && model.long_context_tokens.is_none()
        {
            let tokens = match est {
                Estimate::High => info.question_tokens_high(),
                Estimate::Low => info.question_tokens_low(),
            };
            if tokens > ctx {
                return Err(S1Error::TooLong(format!(
                    "one question sees about {tokens} tokens; `{id}` takes {ctx} per question"
                )));
            }
        }
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
        route_full(c, info, available, &|_| false)
    }

    fn route_full(
        c: &Catalog,
        info: &RouteInfo,
        available: &dyn Fn(&ModelRef<'_>) -> bool,
        throttled: &dyn Fn(&ModelRef<'_>) -> bool,
    ) -> Result<Route, S1Error> {
        route_in(ProtocolId::Systemone, c, info, available, throttled)
    }

    fn route_in(
        ingress: ProtocolId,
        c: &Catalog,
        info: &RouteInfo,
        available: &dyn Fn(&ModelRef<'_>) -> bool,
        throttled: &dyn Fn(&ModelRef<'_>) -> bool,
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
            throttled,
        }
        .route(ingress, info, RequestFlags::default())
    }

    fn route(c: &Catalog, info: &RouteInfo) -> Result<String, S1Error> {
        route_with(c, info, &|_| true).map(|r| r.model_id)
    }

    fn route_forecast(c: &Catalog, model: &str) -> Result<Route, S1Error> {
        let reg = registry(c);
        Router {
            catalog: c,
            backends: &reg,
            available: &|_| Ok(()),
            throttled: &|_| false,
        }
        .route(ProtocolId::Tsfm, &info(model), RequestFlags::default())
    }

    #[test]
    fn forecast_without_a_backend_is_unavailable() {
        let c = Catalog::builtin();
        for model in [
            "forecast",
            "amazon/chronos-bolt-small",
            "typesafe/jev-latest",
        ] {
            let e = route_forecast(&c, model).unwrap_err();
            assert_eq!(e.code(), "backend_unavailable", "{model}: {e}");
            assert_eq!(e.status().as_u16(), 503);
        }
        // A tsfm provider in the overlay makes the family routable.
        let c = Catalog::with_overlay_str(
            "[providers.tsfm]\nbase_url = \"http://127.0.0.1:9\"\napi = \"tsfm\"\n\
             [[providers.tsfm.models]]\nid = \"chronos\"\nupstream = \"amazon/chronos-bolt-small\"\n\
             [aliases.forecast]\nmodels = [\"tsfm/chronos\"]\n",
        )
        .unwrap();
        let r = route_forecast(&c, "forecast").unwrap();
        assert_eq!(r.model_id, "tsfm/chronos");
        assert_eq!(r.upstream, "amazon/chronos-bolt-small");
        let e = route_forecast(&c, "typesafe/jev-latest").unwrap_err();
        assert_eq!(e.code(), "wrong_family");
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
        // Same family, other format: translated.
        let r = route_with(&c, &info("openai/gpt-6-luna"), &|_| true).unwrap();
        assert_eq!(r.api, ProtocolId::OpenaiDecisions);
        assert_eq!(r.upstream, "gpt-6-luna");
    }

    #[test]
    fn openai_ingress_routes_through_translation() {
        let c = Catalog::builtin();
        let all = |_: &ModelRef<'_>| true;
        let none = |_: &ModelRef<'_>| false;
        let r = |i: &RouteInfo| route_in(ProtocolId::OpenaiDecisions, &c, i, &all, &none);
        // The decide alias reaches Clef (systemone) through translation.
        let clef = r(&info("decide")).unwrap();
        assert_eq!(clef.model_id, "workers-ai/clef-flash");
        assert_eq!(clef.api, ProtocolId::Systemone);
        // Images from `input` are systemone assets: Clef takes none.
        let assets = RouteInfo {
            assets: 1,
            ..info("decide")
        };
        assert_eq!(r(&assets).unwrap().model_id, "typesafe/jev-latest");
        // Native OpenAI needs no translation.
        let native = r(&info("openai/gpt-6-luna")).unwrap();
        assert_eq!(native.api, ProtocolId::OpenaiDecisions);
        // More than 10 score levels: no systemone model takes them.
        let levels = RouteInfo {
            max_levels: Some(11),
            min_levels: Some(11),
            ..info("decide")
        };
        assert_eq!(r(&levels).unwrap_err().code(), "unsupported");
        // Other families stay wrong.
        let e = r(&info("workers-ai/clairic")).unwrap_err();
        assert_eq!(e.code(), "wrong_family");
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
    fn baseten_limits_each_question_not_the_request() {
        let c = Catalog::builtin();
        // Four questions of about 20k tokens each: about 80k in all, but
        // each prompt (state plus one question) is about 20k.
        let wide = RouteInfo {
            input_bytes: 330_000,
            questions: 4,
            questions_bytes: 320_000,
            max_question_bytes: 80_000,
            ..info("baseten/mammoth-1")
        };
        assert!(route(&c, &wide).is_ok());
        // One question of about 35k tokens: over 32,768.
        let deep = RouteInfo {
            input_bytes: 140_000,
            questions_bytes: 1_000,
            max_question_bytes: 1_000,
            ..info("baseten/mammoth-1")
        };
        let e = route(&c, &deep).unwrap_err();
        assert_eq!(e.code(), "too_long");
        assert!(e.to_string().contains("per question"), "{e}");
        // Jev's long-context opt-in covers its own per-question limit.
        let jev = RouteInfo {
            model: "typesafe/jev-latest".into(),
            ..deep
        };
        assert!(route(&c, &jev).is_ok());
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

    #[test]
    fn spill_skips_throttled_entries_only_when_the_alias_allows() {
        let flash_throttled = |m: &ModelRef<'_>| m.model.id == "clef-flash";
        let all = |_: &ModelRef<'_>| true;
        // Default alias: spill = false, so the throttled first entry queues.
        let c = Catalog::builtin();
        let r = route_full(&c, &info("decide"), &all, &flash_throttled).unwrap();
        assert_eq!(r.model_id, "workers-ai/clef-flash");
        // spill = true: the next entry takes it.
        let c = Catalog::with_overlay_str(
            "[aliases.decide]\nmodels = [\"workers-ai/clef-flash\", \"typesafe/jev-latest\"]\nspill = true",
        )
        .unwrap();
        let r = route_full(&c, &info("decide"), &all, &flash_throttled).unwrap();
        assert_eq!(r.model_id, "typesafe/jev-latest");
        // Every entry throttled: queue on the first.
        let r = route_full(&c, &info("decide"), &all, &|_| true).unwrap();
        assert_eq!(r.model_id, "workers-ai/clef-flash");
        // A named model is never spilled.
        let r = route_full(&c, &info("workers-ai/clef-flash"), &all, &|_| true).unwrap();
        assert_eq!(r.model_id, "workers-ai/clef-flash");
    }

    #[test]
    fn open_breaker_makes_an_entry_unavailable() {
        let c = Catalog::builtin();
        let reg = registry(&c);
        let available = |m: &ModelRef<'_>| {
            if m.provider_name == "workers-ai" {
                Err(S1Error::BreakerOpen {
                    message: "open".into(),
                    retry_after_secs: 5,
                })
            } else {
                Ok(())
            }
        };
        let r = Router {
            catalog: &c,
            backends: &reg,
            available: &available,
            throttled: &|_| false,
        };
        let got = r
            .route(
                ProtocolId::Systemone,
                &info("decide"),
                RequestFlags::default(),
            )
            .unwrap();
        assert_eq!(got.model_id, "typesafe/jev-latest");
        let e = r
            .route(
                ProtocolId::Systemone,
                &info("workers-ai/clef"),
                RequestFlags::default(),
            )
            .unwrap_err();
        assert_eq!(e.code(), "backend_unavailable");
        assert_eq!(e.retry_after(), Some(5));
    }

    #[test]
    fn alias_with_only_open_breakers_keeps_retry_after() {
        let c = Catalog::with_overlay_str(
            "[aliases.cf]\nmodels = [\"workers-ai/clef-flash\", \"workers-ai/clef\"]",
        )
        .unwrap();
        let reg = registry(&c);
        let run = |available: &dyn Fn(&ModelRef<'_>) -> Result<(), S1Error>, model: &str| {
            Router {
                catalog: &c,
                backends: &reg,
                available,
                throttled: &|_| false,
            }
            .route(ProtocolId::Systemone, &info(model), RequestFlags::default())
            .unwrap_err()
        };
        // Each entry has its own cool-down: the shortest one wins.
        let breakers = |m: &ModelRef<'_>| {
            Err(S1Error::BreakerOpen {
                message: "open".into(),
                retry_after_secs: if m.model.id == "clef-flash" { 9 } else { 4 },
            })
        };
        let e = run(&breakers, "cf");
        assert_eq!(e.code(), "backend_unavailable");
        assert_eq!(e.status().as_u16(), 503);
        assert_eq!(e.retry_after(), Some(4), "{e}");
        // Default alias: clef-flash breaker open, typesafe also unavailable
        // through its breaker.
        let e = run(&breakers, "decide");
        assert_eq!(e.retry_after(), Some(4), "{e}");
        // One entry is down for another reason: no promise of a retry time.
        let mixed = |m: &ModelRef<'_>| {
            if m.model.id == "clef-flash" {
                Err(S1Error::BreakerOpen {
                    message: "open".into(),
                    retry_after_secs: 9,
                })
            } else {
                Err(S1Error::BackendUnavailable("no key".into()))
            }
        };
        let e = run(&mixed, "cf");
        assert_eq!(e.code(), "backend_unavailable");
        assert_eq!(e.retry_after(), None, "{e}");
    }

    // Review R3-4.2: toward OpenAI each asset reference is one image.
    #[test]
    fn asset_references_count_per_occurrence_toward_openai() {
        let c = Catalog::with_overlay_str(
            "[aliases.mix]\nmodels = [\"openai/gpt-6-luna\", \"typesafe/jev-latest\"]\n",
        )
        .unwrap();
        let refs = |n: u32| RouteInfo {
            assets: 1,
            openai_images: Some(n),
            ..info("mix")
        };
        assert_eq!(route(&c, &refs(128)).unwrap(), "openai/gpt-6-luna");
        // Over OpenAI's 128 images: the alias skips it, a named model fails.
        assert_eq!(route(&c, &refs(129)).unwrap(), "typesafe/jev-latest");
        let named = RouteInfo {
            model: "openai/gpt-6-luna".into(),
            ..refs(129)
        };
        assert_eq!(route(&c, &named).unwrap_err().code(), "unsupported");
        // Context and cost: one image allowance per occurrence.
        let i = refs(128);
        assert_eq!(
            i.toward(ProtocolId::OpenaiDecisions).image_tokens(),
            128 * crate::protocol::TOKENS_PER_IMAGE_MAX
        );
        assert_eq!(
            i.toward(ProtocolId::Systemone).image_tokens(),
            crate::protocol::TOKENS_PER_IMAGE_MAX
        );
    }

    fn route_extract(c: &Catalog, info: &RouteInfo) -> Result<Route, S1Error> {
        let reg = registry(c);
        Router {
            catalog: c,
            backends: &reg,
            available: &|_| Ok(()),
            throttled: &|_| false,
        }
        .route(ProtocolId::Clairic, info, RequestFlags::default())
    }

    #[test]
    fn extract_routes_to_clairic_and_not_to_decide_models() {
        let c = Catalog::builtin();
        for name in ["extract", "clairic", "workers-ai/clairic"] {
            let r = route_extract(&c, &info(name)).unwrap();
            assert_eq!(r.model_id, "workers-ai/clairic", "{name}");
            assert_eq!(r.upstream, "clairic");
        }
        let e = route_extract(&c, &info("workers-ai/clef")).unwrap_err();
        assert_eq!(e.code(), "wrong_family", "{e}");
        let e = route(&c, &info("extract")).unwrap_err();
        assert_eq!(e.code(), "wrong_family", "{e}");
        // Clairic takes an `images` array.
        let with_images = RouteInfo {
            images: 4,
            ..info("extract")
        };
        assert!(route_extract(&c, &with_images).is_ok());
    }

    #[test]
    fn clairic_truncates_long_state_and_refuses_only_past_its_precheck() {
        let c = Catalog::builtin();
        // About 100k tokens by any estimate: over the 65,536 context, but
        // Clairic cuts the state and flags `truncated`.
        let long = RouteInfo {
            input_bytes: 400_000,
            ..info("extract")
        };
        assert!(route_extract(&c, &long).is_ok());
        let named = RouteInfo {
            input_bytes: 400_000,
            ..info("workers-ai/clairic")
        };
        assert!(route_extract(&c, &named).is_ok());
        // Workers AI refuses when `bytes / 4` exceeds 131,072.
        let huge = RouteInfo {
            input_bytes: 4 * 131_072 + 8,
            ..info("extract")
        };
        let e = route_extract(&c, &huge).unwrap_err();
        assert_eq!(e.code(), "too_long", "{e}");
    }
}
