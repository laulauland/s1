//! List tools (`decide`, `extract`): one request template plus 1 to
//! [`MAX_ITEMS`] states. Each state becomes one typed-ingress request in the
//! bulk lane, so it gets the same routing, cache, limits, retries, and
//! breaker as `/v1/systemone` and `/v1/extract`. Each item gets its own
//! `ok` and `error`; one failed item never fails the call.
//!
//! Before it starts, a call estimates its run time from the backend's drain
//! rate. When the estimate is over the client timeout, the call is refused
//! with a suggested chunk size and a retry time.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::tools::{CallCtx, Tool, ToolOutput, family_models};
use crate::catalog::Catalog;
use crate::daemon::ingress;
use crate::protocol::{self, ProtocolId};
use crate::queue::{Estimate, LANE_HEADER, Lane, Scheduler};
use crate::router::{RequestFlags, Router as ModelRouter};

/// Most states per call (the spec's first guess; see "Open questions").
pub const MAX_ITEMS: usize = 1_000;
/// Client timeout when the call does not name one. Most MCP clients give a
/// tool call 60 s (the TypeScript SDK default).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Share of the client timeout a call may plan to use.
const BUDGET_SHARE: f64 = 0.8;
/// Items in flight when the backend has no scheduler yet (no key).
const DEFAULT_WINDOW: usize = 8;
/// Most bytes of item request bodies held at once. Bodies are built when
/// their item starts; one body always runs, however large.
const MAX_INFLIGHT_BYTES: usize = 64 << 20;
/// Least time between two progress notifications.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Folds one state item into a copy of the template.
pub type BuildFn = fn(&mut Map<String, Value>, &Map<String, Value>) -> Result<(), String>;

/// A list tool for one wire format.
pub struct ListTool {
    pub name: &'static str,
    pub title: &'static str,
    pub protocol: ProtocolId,
    /// What the tool does; the item limit and the models are appended.
    pub summary: &'static str,
    /// JSON Schema of the `template` argument.
    pub template_schema: fn() -> Value,
    /// JSON Schema of one state item.
    pub item_schema: fn() -> Value,
    /// Extra properties of one result item (besides id, ok, answers, error).
    pub result_extra: fn() -> Map<String, Value>,
    /// Fold one state item into the template.
    pub build: BuildFn,
    /// Pick the result fields from a successful response.
    pub pick: fn(&Value, &mut Map<String, Value>),
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn definition(&self, catalog: &Catalog) -> Value {
        let models = family_models(catalog, self.protocol.family());
        let description = format!(
            "{} Takes 1 to {MAX_ITEMS} states per call and answers each one separately: \
             results come back in input order as {{id, ok, answers | error}}, and one failed \
             item never fails the call. Lists are meant for use from code (a script or a \
             programmatic tool call); calling this tool directly with many items fills your \
             context. Long calls send progress notifications when the request has a \
             progressToken. A call whose estimated run time is over `timeout_secs` (default \
             {DEFAULT_TIMEOUT_SECS}) is refused with `suggested_chunk_size` and \
             `retry_after_secs` (or `required_timeout_secs` when not even one item fits). Models and aliases for `template.model`: {}.",
            self.summary,
            models.join(", ")
        );
        json!({
            "name": self.name,
            "title": self.title,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "template": (self.template_schema)(),
                    "states": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ITEMS,
                        "items": (self.item_schema)(),
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The caller's timeout for this call in seconds; s1 refuses \
                                        the call up front if it would take longer.",
                    },
                },
                "required": ["template", "states"],
                "additionalProperties": false,
            },
            "outputSchema": output_schema((self.result_extra)()),
        })
    }

    async fn call(&self, ctx: CallCtx, args: Value) -> ToolOutput {
        run(self, ctx, args).await
    }
}

/// `{results: [{id, ok, answers?, error?, model?, ...extra}]}`; `answers`
/// is required when `ok` is true, `error` when it is false.
fn output_schema(extra: Map<String, Value>) -> Value {
    let mut props = json!({
        "id": {"description": "The item's `id` (its index when it had none)."},
        "ok": {"type": "boolean"},
        "answers": {"type": "object", "description": "The provider's `answers` object."},
        "model": {"type": "string", "description": "The s1 model that answered."},
        "error": {
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "message": {"type": "string"},
                "status": {"type": "integer"},
            },
            "required": ["code", "message"],
        },
    });
    let obj = props.as_object_mut().expect("object");
    obj.extend(extra);
    json!({
        "type": "object",
        "properties": {
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": props,
                    "required": ["id", "ok"],
                    // A success carries `answers`; a failure carries `error`.
                    "if": {"properties": {"ok": {"const": true}}, "required": ["ok"]},
                    "then": {"required": ["answers"]},
                    "else": {"required": ["error"]},
                },
            },
        },
        "required": ["results"],
    })
}

/// One state turned into a request body, or the reason it could not be.
struct Prepared {
    id: Value,
    body: Result<Bytes, String>,
}

fn prepare(tool: &ListTool, template: &Map<String, Value>, i: usize, item: &Value) -> Prepared {
    let Some(obj) = item.as_object() else {
        return Prepared {
            id: json!(i),
            body: Err("each state item must be an object {id, state}".into()),
        };
    };
    let id = obj.get("id").cloned().unwrap_or_else(|| json!(i));
    if !obj.contains_key("state") {
        return Prepared {
            id,
            body: Err("the item has no `state`".into()),
        };
    }
    let mut body = template.clone();
    let body = (tool.build)(&mut body, obj)
        .map(|()| Bytes::from(serde_json::to_vec(&Value::Object(body)).expect("JSON serializes")));
    Prepared { id, body }
}

/// Headers of one item request: JSON in the bulk lane.
pub(crate) fn item_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(LANE_HEADER, HeaderValue::from_static(Lane::Bulk.as_str()));
    h
}

/// The result item for one finished request.
fn item_result(tool: &ListTool, id: Value, status: u16, model: Option<&str>, body: &[u8]) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id);
    if (200..300).contains(&status) {
        match serde_json::from_slice::<Value>(body) {
            Ok(v) if v.get("answers").is_some_and(Value::is_object) => {
                m.insert("ok".into(), json!(true));
                m.insert("answers".into(), v["answers"].clone());
                if let Some(model) = model {
                    m.insert("model".into(), json!(model));
                }
                (tool.pick)(&v, &mut m);
                return Value::Object(m);
            }
            _ => {
                m.insert("ok".into(), json!(false));
                m.insert(
                    "error".into(),
                    json!({"code": "bad_response", "message": "the provider response has no `answers` object", "status": status}),
                );
                return Value::Object(m);
            }
        }
    }
    m.insert("ok".into(), json!(false));
    m.insert(
        "error".into(),
        crate::cli::stream::reply_error(status, body),
    );
    Value::Object(m)
}

fn item_error(id: Value, code: &str, message: &str) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message}})
}

/// Arguments of a list call.
struct Args {
    template: Map<String, Value>,
    states: Vec<Value>,
    timeout_secs: u64,
}

fn parse_args(args: Value) -> Result<Args, String> {
    let Value::Object(mut a) = args else {
        return Err("arguments must be an object {template, states}".into());
    };
    if let Some(k) = a
        .keys()
        .find(|k| !["template", "states", "timeout_secs"].contains(&k.as_str()))
    {
        return Err(format!("unknown argument `{k}`"));
    }
    let template = match a.remove("template") {
        Some(Value::Object(t)) => t,
        _ => return Err("`template` must be a request object without the state".into()),
    };
    let states = match a.remove("states") {
        Some(Value::Array(s)) => s,
        _ => return Err("`states` must be an array of {id, state}".into()),
    };
    if states.is_empty() || states.len() > MAX_ITEMS {
        return Err(format!(
            "`states` holds {} items; send 1 to {MAX_ITEMS} per call",
            states.len()
        ));
    }
    let timeout_secs = match a.remove("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(v) => match v.as_u64() {
            Some(t) if t >= 1 => t,
            _ => return Err("`timeout_secs` must be a positive integer".into()),
        },
    };
    Ok(Args {
        template,
        states,
        timeout_secs,
    })
}

/// Why a call is refused before it starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Refusal {
    pub estimated_secs: u64,
    pub suggested_chunk_size: usize,
    pub retry_after_secs: u64,
    /// Set when not even one item fits the budget, now or once the backend
    /// is idle: the `timeout_secs` that lets one item run now.
    pub required_timeout_secs: Option<u64>,
}

/// The estimate of one backend group for a prefix of a call's routed items:
/// the group, its estimate, and how many of the prefix's items it holds.
type GroupEstimate = (usize, Estimate, usize);

/// Refuse a call of `items` routed items if they would not finish within
/// the budget. `ests(k)` gives the estimate of every group that holds one
/// of the first `k` routed items. The suggested chunk is the longest prefix
/// whose own costs fit (every shorter prefix fits too), so a client that
/// resubmits the first `suggested_chunk_size` items is not refused again,
/// however uneven the item costs are. Returns the refusal and the slowest
/// group.
fn plan(
    items: usize,
    timeout_secs: u64,
    ests: impl Fn(usize) -> Vec<GroupEstimate>,
) -> Option<(Refusal, usize)> {
    let budget = timeout_secs as f64 * BUDGET_SHARE;
    let (slow, worst) = ests(items)
        .into_iter()
        .map(|(g, e, _)| (g, e.secs))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if worst <= budget + 1e-9 {
        return None;
    }
    let estimated_secs = worst.ceil() as u64;
    let fits_now = |k: usize| ests(k).iter().all(|(_, e, _)| e.secs <= budget + 1e-9);
    // Once the backlog has drained (and the pause is over), the backend is
    // idle.
    let fits_idle = |k: usize| {
        ests(k)
            .iter()
            .all(|(_, e, n)| *n as f64 <= budget * e.rate + 1e-9)
    };
    let longest = |fits: &dyn Fn(usize) -> bool, max: usize| {
        (1..=max).take_while(|&k| fits(k)).last().unwrap_or(0)
    };
    let fit = |chunk: usize| chunk.min(items.saturating_sub(1)).max(1);
    let now = longest(&fits_now, items.saturating_sub(1));
    if now >= 1 {
        return Some((
            Refusal {
                estimated_secs,
                suggested_chunk_size: fit(now),
                retry_after_secs: 0,
                required_timeout_secs: None,
            },
            slow,
        ));
    }
    let idle = longest(&fits_idle, items);
    if idle >= 1 {
        let chunk = fit(idle);
        let drain = ests(chunk)
            .iter()
            .map(|(_, e, _)| e.drain_secs())
            .fold(0.0, f64::max);
        return Some((
            Refusal {
                estimated_secs,
                suggested_chunk_size: chunk,
                retry_after_secs: (drain - 1e-9).ceil().max(1.0) as u64,
                required_timeout_secs: None,
            },
            slow,
        ));
    }
    // Not even the first item fits an idle backend: no chunk size helps,
    // only a longer timeout. Name the one that lets it run now.
    let one = ests(1)
        .iter()
        .map(|(_, e, _)| (e.backlog + 1) as f64 / e.rate + e.pause_secs)
        .fold(0.0, f64::max);
    Some((
        Refusal {
            estimated_secs,
            suggested_chunk_size: 1,
            retry_after_secs: 0,
            required_timeout_secs: Some(((one / BUDGET_SHARE) - 1e-9).ceil().max(1.0) as u64),
        },
        slow,
    ))
}

/// [`plan`] for `items` equal items on one backend.
#[cfg(test)]
pub(crate) fn check_budget(e: &Estimate, items: usize, timeout_secs: u64) -> Option<Refusal> {
    let at = |k: usize| Estimate {
        secs: (e.backlog + k) as f64 / e.rate + e.pause_secs,
        ..*e
    };
    plan(items, timeout_secs, |k| vec![(0, at(k), k)]).map(|(r, _)| r)
}

/// One routed item of a call, in call order.
struct Routed {
    /// Index into the call's backend guards.
    group: usize,
    tokens: u64,
}

/// The backend guard and input tokens of one item body, if it routes.
/// Items that fail inspection, the protocol's checks, or routing are left
/// out of the estimate; they fail fast with their own error when they run.
fn route_item(
    tool: &ListTool,
    st: &crate::daemon::AppState,
    body: &[u8],
) -> Option<(Arc<Scheduler>, u64)> {
    let p = protocol::by_id(tool.protocol)?;
    let info = p.inspect(body).ok()?;
    p.check(&info).ok()?;
    let available = |m: &crate::catalog::ModelRef<'_>| st.available(m);
    let throttled = |m: &crate::catalog::ModelRef<'_>| st.throttled(m);
    let route = ModelRouter {
        catalog: &st.catalog,
        backends: &st.backends,
        available: &available,
        throttled: &throttled,
    }
    .route(tool.protocol, &info, RequestFlags::default())
    .ok()?;
    let g = st.guard(&route.provider)?;
    Some((g, info.tokens_high()))
}

/// Check the routed items against the budget, per backend. The call is
/// refused when any backend would not finish in time.
fn check_groups(
    guards: &[Arc<Scheduler>],
    routed: &[Routed],
    timeout_secs: u64,
) -> Option<(Refusal, usize, String)> {
    let ests = |k: usize| {
        let mut sums = vec![(0usize, 0u64); guards.len()];
        for r in &routed[..k] {
            let s = &mut sums[r.group];
            s.0 += 1;
            s.1 = s.1.saturating_add(r.tokens);
        }
        sums.into_iter()
            .enumerate()
            .filter(|(_, (n, _))| *n > 0)
            .map(|(g, (n, tokens))| (g, guards[g].estimate(n, tokens.div_ceil(n as u64)), n))
            .collect()
    };
    let (r, slow) = plan(routed.len(), timeout_secs, ests)?;
    Some((r, routed.len(), guards[slow].provider().to_string()))
}

fn refusal_output(r: &Refusal, items: usize, provider: &str, timeout_secs: u64) -> ToolOutput {
    let advice = match r.required_timeout_secs {
        Some(t) => format!("not even one item fits; call with `timeout_secs` of at least {t}"),
        None if r.retry_after_secs == 0 => format!(
            "send at most {} items per call (now)",
            r.suggested_chunk_size
        ),
        None => format!(
            "send at most {} items per call (in {} s)",
            r.suggested_chunk_size, r.retry_after_secs
        ),
    };
    let mut e = json!({
        "code": "queue_full",
        "message": format!(
            "{items} items would take about {} s on `{provider}`, over the {timeout_secs} s \
             timeout; {advice}",
            r.estimated_secs
        ),
        "items": items,
        "estimated_secs": r.estimated_secs,
        "timeout_secs": timeout_secs,
        "suggested_chunk_size": r.suggested_chunk_size,
        "retry_after_secs": r.retry_after_secs,
    });
    if let Some(t) = r.required_timeout_secs {
        e["required_timeout_secs"] = json!(t);
    }
    ToolOutput::Err(e)
}

/// True if a body of `len` bytes may start next to `running` items that
/// hold `in_flight` bytes. One item always runs, however large.
fn admit(running: usize, in_flight: usize, len: usize) -> bool {
    running == 0 || in_flight.saturating_add(len) <= MAX_INFLIGHT_BYTES
}

async fn run(tool: &ListTool, ctx: CallCtx, args: Value) -> ToolOutput {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(msg) => return ToolOutput::error("invalid_request", msg),
    };
    let n = args.states.len();
    let st = ctx.state.clone();
    let mut results: Vec<Option<Value>> = vec![None; n];

    // Estimate from every item. Each body is built, inspected, and dropped
    // at once: holding all of them would cost items x template bytes.
    let mut jobs = VecDeque::new();
    let mut guards: Vec<Arc<Scheduler>> = Vec::new();
    let mut routed: Vec<Routed> = Vec::new();
    for (i, s) in args.states.iter().enumerate() {
        let p = prepare(tool, &args.template, i, s);
        match p.body {
            Ok(body) => {
                jobs.push_back(i);
                if let Some((g, tokens)) = route_item(tool, &st, &body) {
                    let group = match guards.iter().position(|x| Arc::ptr_eq(x, &g)) {
                        Some(x) => x,
                        None => {
                            guards.push(g);
                            guards.len() - 1
                        }
                    };
                    routed.push(Routed { group, tokens });
                }
            }
            Err(msg) => results[i] = Some(item_error(p.id, "invalid_request", &msg)),
        }
    }
    if let Some((r, items, provider)) = check_groups(&guards, &routed, args.timeout_secs) {
        return refusal_output(&r, items, &provider, args.timeout_secs);
    }

    let mut done = results.iter().filter(|r| r.is_some()).count();
    let mut last_report: Option<Instant> = None;
    let mut set = JoinSet::new();
    let mut running = HashMap::new();
    let mut bytes_in_flight = 0usize;
    // A built body that waits for room under MAX_INFLIGHT_BYTES.
    let mut next: Option<(usize, Value, Bytes)> = None;
    loop {
        let window = guards
            .iter()
            .map(|g| g.cap().max(1) as usize)
            .max()
            .unwrap_or(DEFAULT_WINDOW);
        while set.len() < window {
            // Build the body only when the item is about to start.
            let (i, id, body) = match next.take() {
                Some(b) => b,
                None => {
                    let Some(i) = jobs.pop_front() else { break };
                    let p = prepare(tool, &args.template, i, &args.states[i]);
                    match p.body {
                        Ok(b) => (i, p.id, b),
                        Err(msg) => {
                            results[i] = Some(item_error(p.id, "invalid_request", &msg));
                            done += 1;
                            continue;
                        }
                    }
                }
            };
            if !admit(set.len(), bytes_in_flight, body.len()) {
                next = Some((i, id, body));
                break;
            }
            let len = body.len();
            bytes_in_flight += len;
            let st = st.clone();
            let pid = tool.protocol;
            let origin = ctx.origin;
            let h = set.spawn(async move {
                let resp =
                    ingress::handle(State(st), pid, origin, item_headers(), Body::from(body)).await;
                let status = resp.status().as_u16();
                let model = resp
                    .headers()
                    .get(crate::daemon::raw::H_MODEL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                (status, model, bytes)
            });
            running.insert(h.id(), (i, id, len));
        }
        let Some(joined) = set.join_next_with_id().await else {
            break;
        };
        let (task, outcome) = match joined {
            Ok((task, out)) => (task, Ok(out)),
            Err(e) => (e.id(), Err(e.to_string())),
        };
        let Some((i, id, len)) = running.remove(&task) else {
            continue;
        };
        bytes_in_flight -= len;
        results[i] = Some(match outcome {
            Ok((status, model, bytes)) => item_result(tool, id, status, model.as_deref(), &bytes),
            Err(msg) => item_error(id, "internal", &msg),
        });
        done += 1;
        if let Some(p) = &ctx.progress {
            if p.closed() {
                // The client closed the stream: stop (dropping the set aborts).
                return ToolOutput::error("cancelled", "the client went away");
            }
            if done < n && last_report.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                p.report(done as u64, n as u64, &format!("{done}/{n} items"));
                last_report = Some(Instant::now());
            }
        }
    }
    let results: Vec<Value> = results
        .into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or_else(|| item_error(json!(i), "internal", "no result")))
        .collect();
    ToolOutput::Ok(json!({ "results": results }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::LANE_HEADER;

    fn est(rate: f64, backlog: usize, pause: f64, items: usize) -> Estimate {
        Estimate {
            rate,
            backlog,
            pause_secs: pause,
            secs: (backlog + items) as f64 / rate + pause,
        }
    }

    #[test]
    fn budget_allows_calls_that_fit() {
        assert_eq!(check_budget(&est(10.0, 0, 0.0, 100), 100, 60), None);
        // 48 s of 60 is the budget.
        assert_eq!(check_budget(&est(1.0, 0, 0.0, 48), 48, 60), None);
        assert!(check_budget(&est(1.0, 0, 0.0, 49), 49, 60).is_some());
    }

    #[test]
    fn refusal_suggests_a_chunk_that_fits_now() {
        let r = check_budget(&est(1.0, 3, 0.0, 100), 100, 10).unwrap();
        assert_eq!(r.estimated_secs, 103);
        // 8 s budget, 3 waiting: 5 more fit now.
        assert_eq!(r.suggested_chunk_size, 5);
        assert_eq!(r.retry_after_secs, 0);
    }

    #[test]
    fn refusal_waits_for_the_backlog_when_nothing_fits() {
        let r = check_budget(&est(1.0, 20, 2.0, 5), 5, 10).unwrap();
        assert_eq!(r.retry_after_secs, 22);
        // Once idle (backlog and pause gone): 8 s at 1/s.
        assert_eq!(r.suggested_chunk_size, 4, "fewer than the call's 5 items");
        assert_eq!(r.required_timeout_secs, None);
    }

    #[test]
    fn refusal_names_the_timeout_when_one_item_cannot_fit() {
        // 0.1 per second: one item takes 10 s, more than 80 % of 1 s.
        let r = check_budget(&est(0.1, 0, 0.0, 3), 3, 1).unwrap();
        assert_eq!(r.suggested_chunk_size, 1);
        assert_eq!(r.retry_after_secs, 0);
        // 10 s is 80 % of 12.5 s.
        assert_eq!(r.required_timeout_secs, Some(13));
        // A 1-item call gets the same answer, not "send 1 item".
        let r = check_budget(&est(0.1, 0, 0.0, 1), 1, 1).unwrap();
        assert_eq!(r.required_timeout_secs, Some(13));
    }

    proptest::proptest! {
        #[test]
        fn recommendations_fit_after_their_retry_delay(
            rate in 0.01f64..100.0,
            backlog in 0usize..500,
            pause in proptest::prop_oneof![proptest::strategy::Just(0.0f64), 0.0f64..120.0],
            items in 1usize..1000,
            timeout in 1u64..600,
        ) {
            let e = est(rate, backlog, pause, items);
            if let Some(r) = check_budget(&e, items, timeout) {
                proptest::prop_assert!(r.suggested_chunk_size >= 1);
                proptest::prop_assert!(r.suggested_chunk_size < items || items == 1);
                let chunk = r.suggested_chunk_size;
                if let Some(t) = r.required_timeout_secs {
                    // One item fits now with the named timeout.
                    proptest::prop_assert_eq!(chunk, 1);
                    proptest::prop_assert_eq!(r.retry_after_secs, 0);
                    proptest::prop_assert!(t > timeout);
                    let one = est(rate, backlog, pause, 1);
                    proptest::prop_assert!(check_budget(&one, 1, t).is_none());
                } else if r.retry_after_secs == 0 {
                    let after = est(rate, backlog, pause, chunk);
                    proptest::prop_assert!(check_budget(&after, chunk, timeout).is_none());
                } else {
                    // After the delay the backlog and the pause are gone.
                    proptest::prop_assert!(r.retry_after_secs as f64 + 1e-9 >= e.drain_secs());
                    let after = est(rate, 0, 0.0, chunk);
                    proptest::prop_assert!(check_budget(&after, chunk, timeout).is_none());
                }
            }
        }
    }

    /// Estimates of a TPM-bound backend model: per group a TPM, an
    /// in-flight cap, a backlog and a pause; items have uneven costs.
    fn tpm_ests(
        groups: &[(u64, f64, usize, f64)],
        items: &[(usize, u64)],
        k: usize,
    ) -> Vec<GroupEstimate> {
        let mut sums = vec![(0usize, 0u64); groups.len()];
        for &(g, t) in &items[..k] {
            sums[g].0 += 1;
            sums[g].1 += t;
        }
        sums.into_iter()
            .enumerate()
            .filter(|(_, (n, _))| *n > 0)
            .map(|(g, (n, tokens))| {
                let (tpm, cap, backlog, pause) = groups[g];
                let each = tokens.div_ceil(n as u64).max(1);
                let rate = cap.min(tpm as f64 / 60.0 / each as f64);
                let secs = (backlog + n) as f64 / rate + pause;
                (
                    g,
                    Estimate {
                        rate,
                        backlog,
                        pause_secs: pause,
                        secs,
                    },
                    n,
                )
            })
            .collect()
    }

    fn group_strategy() -> impl proptest::strategy::Strategy<Value = (u64, f64, usize, f64)> {
        (
            1_000u64..10_000_000,
            1.0f64..16.0,
            0usize..50,
            proptest::prop_oneof![proptest::strategy::Just(0.0f64), 0.0f64..30.0],
        )
    }

    proptest::proptest! {
        #[test]
        fn uneven_prefix_recommendations_fit_when_resubmitted(
            groups in proptest::collection::vec(group_strategy(), 1..3),
            raw in proptest::collection::vec(
                (0usize..3, proptest::prop_oneof![1u64..50, 1u64..200_000]),
                1..300,
            ),
            timeout in 1u64..120,
        ) {
            let items: Vec<(usize, u64)> =
                raw.into_iter().map(|(g, t)| (g % groups.len(), t)).collect();
            let n = items.len();
            let Some((r, _)) = plan(n, timeout, |k| tpm_ests(&groups, &items, k)) else {
                return Ok(());
            };
            let chunk = r.suggested_chunk_size;
            proptest::prop_assert!(chunk >= 1 && (chunk < n || n == 1));
            if let Some(t) = r.required_timeout_secs {
                proptest::prop_assert_eq!(chunk, 1);
                proptest::prop_assert!(t > timeout);
                proptest::prop_assert!(plan(1, t, |k| tpm_ests(&groups, &items, k)).is_none());
            } else if r.retry_after_secs == 0 {
                let again = plan(chunk, timeout, |k| tpm_ests(&groups, &items, k));
                proptest::prop_assert!(again.is_none(), "{:?}", again);
            } else {
                // After the delay the backlog and the pause are gone.
                let idle: Vec<_> = groups.iter().map(|&(t, c, _, _)| (t, c, 0, 0.0)).collect();
                for (_, e, _) in tpm_ests(&groups, &items, chunk) {
                    proptest::prop_assert!(r.retry_after_secs as f64 + 1e-9 >= e.drain_secs());
                }
                let again = plan(chunk, timeout, |k| tpm_ests(&idle, &items, k));
                proptest::prop_assert!(again.is_none(), "{:?}", again);
            }
        }
    }
    #[test]
    fn item_bodies_in_flight_stay_under_the_byte_bound() {
        const MIB: usize = 1 << 20;
        // One body always runs, even one over the bound.
        assert!(admit(0, 0, 300 * MIB));
        // 8 MiB bodies: 8 fit in 64 MiB, the 9th waits.
        let mut in_flight = 0;
        let mut running = 0;
        while admit(running, in_flight, 8 * MIB) {
            running += 1;
            in_flight += 8 * MIB;
            assert!(running <= 1_000, "unbounded");
        }
        assert_eq!(running, 8);
        assert!(in_flight <= MAX_INFLIGHT_BYTES);
    }

    #[test]
    fn items_go_in_the_bulk_lane() {
        let h = item_headers();
        assert_eq!(Lane::from_headers(&h), Lane::Bulk);
        assert_eq!(h[LANE_HEADER], "bulk");
        assert_eq!(h[header::CONTENT_TYPE], "application/json");
    }

    #[test]
    fn args_are_checked() {
        let t = json!({"model": "decide"});
        assert!(parse_args(json!({"template": t, "states": []})).is_err());
        let many: Vec<Value> = (0..=MAX_ITEMS)
            .map(|i| json!({"id": i, "state": "x"}))
            .collect();
        let e = parse_args(json!({"template": t, "states": many}))
            .err()
            .unwrap();
        assert!(e.contains("1001"), "{e}");
        assert!(parse_args(json!({"template": "x", "states": [{"state": 1}]})).is_err());
        assert!(parse_args(json!({"template": t, "states": [{"state": 1}], "extra": 1})).is_err());
        assert!(
            parse_args(json!({"template": t, "states": [{"state": 1}], "timeout_secs": 0}))
                .is_err()
        );
        let ok = parse_args(json!({"template": t, "states": [{"state": 1}], "timeout_secs": 5}))
            .unwrap();
        assert_eq!(ok.timeout_secs, 5);
        let ok = parse_args(json!({"template": t, "states": [{"state": 1}]})).unwrap();
        assert_eq!(ok.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn item_results_follow_the_spec_shape() {
        let tool = &super::super::decide::TOOL;
        let ok = item_result(
            tool,
            json!("a"),
            200,
            Some("workers-ai/clef"),
            br#"{"model":"clef","answers":{"q":{"type":"noul","noul":0.9}},"usage":{}}"#,
        );
        assert_eq!(ok["id"], "a");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["answers"]["q"]["noul"], 0.9);
        assert_eq!(ok["model"], "workers-ai/clef");
        assert!(ok.get("error").is_none());
        let err = item_result(
            tool,
            json!(2),
            413,
            None,
            br#"{"detail":{"error_type":"too_long","message":"too long"}}"#,
        );
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"]["code"], "too_long");
        assert_eq!(err["error"]["status"], 413);
        assert!(err.get("answers").is_none());
        let bad = item_result(tool, json!(3), 200, None, b"{}");
        assert_eq!(bad["error"]["code"], "bad_response");
    }
}
