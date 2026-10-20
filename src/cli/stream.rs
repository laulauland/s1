//! Streaming filter mode (spec "Streaming filter mode"): one JSON line in,
//! one JSON line out, with a window of requests in flight.
//!
//! Format-neutral: a [`Job`] holds the request template, the wire format
//! (for `--flat`), and how the state is placed, so `s1 extract` (stage 5)
//! reuses this engine.
//!
//! Window: the first [`INTERACTIVE_FIRST`] requests go in the interactive
//! lane, the rest in the bulk lane. The window starts at
//! [`INITIAL_WINDOW`] and then follows the backend's current in-flight cap
//! (the `s1-capacity` reply header), never above `-j` when given. Stdin is
//! read only when the window has room, so a full window blocks the
//! producer of the pipe. The window counts requests in flight and replies
//! held back for an earlier line (ordered output), so one slow line also
//! stops stdin reads. Each reply is written as soon as it can be, also
//! while stdin stays open.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;

use super::client::Reply;
use crate::flatten::is_column_of;
use crate::images::path_asset;
use crate::protocol::Protocol;
use crate::queue::Lane;

/// Requests sent in the interactive lane before the rest go in bulk.
pub const INTERACTIVE_FIRST: usize = 4;
/// Window before the first reply tells the backend's cap.
pub const INITIAL_WINDOW: usize = 4;
/// Largest window when `-j` is not given.
pub const MAX_WINDOW: usize = 256;
/// Reply header with the backend's current in-flight cap.
pub const CAPACITY_HEADER: &str = "s1-capacity";

/// What the output carries besides the id and the answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Keep {
    #[default]
    Nothing,
    Fields(Vec<String>),
    /// The whole input line.
    All,
}

impl Keep {
    /// `--keep body,customer` or `--keep '*'`.
    pub fn parse(spec: Option<&str>) -> Self {
        match spec.map(str::trim) {
            None | Some("") => Keep::Nothing,
            Some("*") => Keep::All,
            Some(s) => Keep::Fields(
                s.split(',')
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect(),
            ),
        }
    }
}

/// How the state goes into the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateMode {
    /// As a JSON value (systemone takes strings, objects, and arrays).
    Value,
    /// As JSON text (Clairic takes only a string).
    JsonText,
    /// As OpenAI Decisions `input`: JSON text; with images, one user
    /// message of an `input_text` part and `input_image` data URLs (read
    /// here, as OpenAI-format bodies have no path assets).
    OpenaiInput,
}

#[derive(Debug, Clone)]
pub struct StreamOpts {
    pub id_field: String,
    pub state_field: Option<String>,
    pub image_field: Option<String>,
    pub keep: Keep,
    pub unordered: bool,
    pub flat: bool,
    /// Upper bound of the window (`-j`); `None`: follow the backend's cap.
    pub max_in_flight: Option<usize>,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            id_field: "id".into(),
            state_field: None,
            image_field: None,
            keep: Keep::Nothing,
            unordered: false,
            flat: false,
            max_in_flight: None,
        }
    }
}

/// One kind of request, made per input line.
pub struct Job {
    /// Request body without the state.
    pub template: Map<String, Value>,
    pub protocol: &'static dyn Protocol,
    pub state_mode: StateMode,
    /// Question or field names (their flat columns are `name` and `name__*`).
    pub answer_names: Vec<String>,
}

/// Sends one request body to the daemon in a lane.
pub type SendFn = Arc<
    dyn Fn(Bytes, Lane) -> Pin<Box<dyn Future<Output = Result<Reply, String>> + Send>>
        + Send
        + Sync,
>;

/// The window of requests in flight.
#[derive(Debug, Clone, Copy)]
struct Window {
    size: usize,
    limit: usize,
}

impl Window {
    fn new(limit: Option<usize>) -> Self {
        let limit = limit.unwrap_or(MAX_WINDOW).max(1);
        Self {
            size: INITIAL_WINDOW.min(limit),
            limit,
        }
    }

    /// Follow the cap a reply reports.
    fn observe(&mut self, res: &Result<Reply, String>) {
        let cap = res
            .as_ref()
            .ok()
            .and_then(|r| r.headers.get(CAPACITY_HEADER))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok());
        if let Some(cap) = cap {
            self.size = cap.clamp(1, self.limit);
        }
    }
}

/// Result of a whole stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    pub lines: usize,
    pub failed: usize,
}

/// Per-line context carried to the output.
struct LineCtx {
    seq: usize,
    id: Value,
    kept: Map<String, Value>,
}

/// Run the stream until EOF. Returns `Err` only for a fatal problem (a kept
/// field that would overwrite an answer column, or I/O); lines that fail
/// are written as error lines and counted.
pub async fn run<R, W>(
    input: R,
    out: &mut W,
    job: Arc<Job>,
    opts: &StreamOpts,
    send: SendFn,
) -> Result<Outcome, String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    check_id_field(&job, opts)?;
    let mut lines = input.lines();
    let mut set: JoinSet<(LineCtx, Result<Reply, String>)> = JoinSet::new();
    let mut writer = Writer {
        out,
        unordered: opts.unordered,
        next: 0,
        pending: BTreeMap::new(),
        outcome: Outcome::default(),
    };
    let mut seq = 0usize;
    let mut sent = 0usize;
    let mut fatal = None;
    let mut window = Window::new(opts.max_in_flight);
    let mut eof = false;

    // Write each reply as soon as it arrives; read stdin only while the
    // window has room. Replies that wait in the reorder buffer for an
    // earlier line count against the window too, so a stalled head line
    // stops stdin reads (and the pipe blocks the producer).
    loop {
        let room = set.len() + writer.pending.len() < window.size;
        tokio::select! {
            biased;
            Some(done) = set.join_next(), if !set.is_empty() => {
                let (ctx, res) = done.map_err(|e| format!("request task failed: {e}"))?;
                window.observe(&res);
                let (text, ok) = finish(&ctx, res, &job, opts);
                writer.push(ctx.seq, text, ok).await?;
            }
            line = lines.next_line(), if room && !eof => {
                let Some(line) = line.map_err(|e| format!("cannot read stdin: {e}"))? else {
                    eof = true;
                    continue;
                };
                if line.trim().is_empty() {
                    continue;
                }
                let n = seq;
                seq += 1;
                match prepare(&line, n, &job, opts) {
                    Prepared::Send(ctx, body) => {
                        let lane = if sent < INTERACTIVE_FIRST {
                            Lane::Interactive
                        } else {
                            Lane::Bulk
                        };
                        sent += 1;
                        let send = send.clone();
                        set.spawn(async move { (ctx, send(body, lane).await) });
                    }
                    Prepared::Failed(ctx, error) => {
                        let text = error_line(&ctx, error, opts);
                        writer.push(n, text, false).await?;
                    }
                    Prepared::Fatal(msg) => {
                        fatal = Some(msg);
                        break;
                    }
                }
            }
            // Nothing in flight and nothing left to read.
            else => break,
        }
    }
    while let Some(done) = set.join_next().await {
        let (ctx, res) = done.map_err(|e| format!("request task failed: {e}"))?;
        let (text, ok) = finish(&ctx, res, &job, opts);
        writer.push(ctx.seq, text, ok).await?;
    }
    writer.flush().await?;
    match fatal {
        Some(msg) => Err(msg),
        None => Ok(writer.outcome),
    }
}

enum Prepared {
    Send(LineCtx, Bytes),
    Failed(LineCtx, Value),
    Fatal(String),
}

fn local_error(code: &str, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

fn prepare(line: &str, seq: usize, job: &Job, opts: &StreamOpts) -> Prepared {
    let obj = match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(o)) => o,
        Ok(_) => {
            let ctx = LineCtx {
                seq,
                id: Value::Null,
                kept: Map::new(),
            };
            return Prepared::Failed(
                ctx,
                local_error("invalid_input", "input line is not a JSON object"),
            );
        }
        Err(e) => {
            let ctx = LineCtx {
                seq,
                id: Value::Null,
                kept: Map::new(),
            };
            return Prepared::Failed(
                ctx,
                local_error("invalid_input", format!("input line is not JSON: {e}")),
            );
        }
    };
    let id = obj.get(&opts.id_field).cloned().unwrap_or(Value::Null);
    let kept: Map<String, Value> = match &opts.keep {
        Keep::Nothing => Map::new(),
        Keep::All => obj
            .iter()
            .filter(|(k, _)| **k != opts.id_field)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Keep::Fields(fields) => fields
            .iter()
            .filter(|f| **f != opts.id_field)
            .filter_map(|f| obj.get(f).map(|v| (f.clone(), v.clone())))
            .collect(),
    };
    if let Some(k) = kept.keys().find(|k| collides(k, job, opts)) {
        return Prepared::Fatal(format!(
            "line {}: kept field `{k}` has the same name as a new answer column; s1 does not overwrite it (rename the question or drop the field from --keep)",
            seq + 1
        ));
    }
    let ctx = LineCtx { seq, id, kept };
    match build_body(&obj, job, opts) {
        Ok(body) => Prepared::Send(ctx, body),
        Err(e) => Prepared::Failed(ctx, local_error("invalid_input", e)),
    }
}

/// The id field must not share a name with a key s1 writes: `error`, and
/// `response` (or with `--flat` an answer column).
fn check_id_field(job: &Job, opts: &StreamOpts) -> Result<(), String> {
    let id = opts.id_field.as_str();
    if id == "error" || collides(id, job, opts) {
        return Err(format!(
            "--id-field `{id}` has the same name as a key s1 writes (`error`, `response`, or with --flat an answer column); pick another id field"
        ));
    }
    Ok(())
}

/// True if kept field `k` would overwrite something s1 writes.
fn collides(k: &str, job: &Job, opts: &StreamOpts) -> bool {
    if opts.flat {
        job.answer_names.iter().any(|q| is_column_of(k, q))
    } else {
        k == "response"
    }
}

fn build_body(obj: &Map<String, Value>, job: &Job, opts: &StreamOpts) -> Result<Bytes, String> {
    let mut state = match &opts.state_field {
        Some(f) => obj
            .get(f)
            .cloned()
            .ok_or_else(|| format!("input line has no `{f}` field"))?,
        None => Value::Object(obj.clone()),
    };
    let mut body = job.template.clone();
    if job.state_mode == StateMode::OpenaiInput {
        let images = match opts.image_field.as_ref().and_then(|f| obj.get(f)) {
            Some(paths) => image_data_urls(paths)?,
            None => Vec::new(),
        };
        body.insert("input".into(), openai_input(state, images));
        return Ok(Bytes::from(
            serde_json::to_vec(&Value::Object(body)).expect("JSON serializes"),
        ));
    }
    if let Some(field) = &opts.image_field
        && let Some(paths) = obj.get(field)
    {
        let (refs, assets) = load_images(paths)?;
        if !matches!(state, Value::Object(_)) {
            let key = opts.state_field.clone().unwrap_or_else(|| "text".into());
            state = json!({ key: state });
        }
        state
            .as_object_mut()
            .expect("object")
            .insert(field.clone(), refs);
        let slot = body
            .entry("assets")
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(slot) = slot else {
            return Err("template `assets` must be an object".into());
        };
        slot.extend(assets);
    }
    let state = match job.state_mode {
        StateMode::Value => state,
        StateMode::JsonText | StateMode::OpenaiInput => match state {
            Value::String(s) => Value::String(s),
            other => Value::String(other.to_string()),
        },
    };
    body.insert("state".into(), state);
    Ok(Bytes::from(
        serde_json::to_vec(&Value::Object(body)).expect("JSON serializes"),
    ))
}

/// OpenAI Decisions `input`: the state as text, plus image parts.
fn openai_input(state: Value, images: Vec<String>) -> Value {
    let text = match state {
        Value::String(s) => s,
        other => other.to_string(),
    };
    if images.is_empty() {
        return Value::String(text);
    }
    let mut parts = vec![json!({"type": "input_text", "text": text})];
    parts.extend(
        images
            .into_iter()
            .map(|url| json!({"type": "input_image", "image_url": url})),
    );
    json!([{"role": "user", "content": parts}])
}

/// Image files named by a path or a list of paths, as data URLs.
fn image_data_urls(paths: &Value) -> Result<Vec<String>, String> {
    use base64::Engine;
    let list: Vec<&str> = match paths {
        Value::String(p) => vec![p.as_str()],
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().ok_or("image paths must be strings"))
            .collect::<Result<_, _>>()?,
        Value::Null => vec![],
        _ => return Err("image field must hold a path or a list of paths".into()),
    };
    list.iter()
        .map(|p| {
            let size = std::fs::metadata(p)
                .map_err(|e| format!("cannot read image {p}: {e}"))?
                .len();
            if size > crate::images::MAX_IMAGE_FILE {
                return Err(format!(
                    "image {p} is over {} bytes",
                    crate::images::MAX_IMAGE_FILE
                ));
            }
            let data = std::fs::read(p).map_err(|e| format!("cannot read image {p}: {e}"))?;
            let mime = match crate::images::image_format(&data) {
                Some("png") => "image/png",
                Some(_) => "image/jpeg",
                None => return Err(format!("image {p} is not PNG or JPEG")),
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            Ok(format!("data:{mime};base64,{b64}"))
        })
        .collect()
}

/// Path-valued assets for image files named by a path or a list of paths.
/// Returns the placeholder reference(s) to put in the state, and the
/// `assets` entries. The daemon reads the files (Unix socket only; see
/// [`crate::images`]), so the CLI and other local clients share one code path.
fn load_images(paths: &Value) -> Result<(Value, Map<String, Value>), String> {
    let list: Vec<&str> = match paths {
        Value::String(p) => vec![p.as_str()],
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().ok_or("image paths must be strings"))
            .collect::<Result<_, _>>()?,
        Value::Null => vec![],
        _ => return Err("image field must hold a path or a list of paths".into()),
    };
    let tag = crate::secrets::random_hex(6);
    let mut refs = Vec::new();
    let mut assets = Map::new();
    for (i, p) in list.iter().enumerate() {
        // The daemon has another working directory: send absolute paths.
        let abs = std::path::absolute(Path::new(p))
            .map_err(|e| format!("cannot resolve image path {p}: {e}"))?;
        let placeholder = format!("s1-image-{tag}-{i}");
        assets.insert(placeholder.clone(), path_asset(&abs));
        refs.push(Value::String(placeholder));
    }
    let refs = match (paths, refs.len()) {
        (Value::String(_), 1) => refs.pop().expect("one ref"),
        _ => Value::Array(refs),
    };
    Ok((refs, assets))
}

/// Error object for a failed HTTP reply: `{code, message, status}`.
pub fn reply_error(status: u16, body: &[u8]) -> Value {
    let v: Option<Value> = serde_json::from_slice(body).ok();
    let pick = |v: &Value| -> Option<(String, String)> {
        // systemone: {"detail": {"error_type", "message"}}; others: {"error": {"code"|"type", "message"}}.
        if let Some(d) = v.get("detail").filter(|d| d.is_object()) {
            let code = d.get("error_type").and_then(Value::as_str)?;
            let msg = d.get("message").and_then(Value::as_str).unwrap_or("");
            return Some((code.into(), msg.into()));
        }
        let e = v.get("error").filter(|e| e.is_object())?;
        let code = e
            .get("code")
            .or_else(|| e.get("type"))
            .and_then(Value::as_str)?;
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
        Some((code.into(), msg.into()))
    };
    let (code, message) = v.as_ref().and_then(pick).unwrap_or_else(|| {
        (
            format!("http_{status}"),
            String::from_utf8_lossy(body).chars().take(500).collect(),
        )
    });
    json!({"code": code, "message": message, "status": status})
}

fn error_line(ctx: &LineCtx, error: Value, opts: &StreamOpts) -> String {
    let mut m = Map::new();
    m.insert(opts.id_field.clone(), ctx.id.clone());
    for (k, v) in &ctx.kept {
        m.insert(k.clone(), v.clone());
    }
    // The error object wins over a kept field of the same name.
    m.insert("error".into(), error);
    Value::Object(m).to_string()
}

fn finish(
    ctx: &LineCtx,
    res: Result<Reply, String>,
    job: &Job,
    opts: &StreamOpts,
) -> (String, bool) {
    let reply = match res {
        Ok(r) => r,
        Err(e) => return (error_line(ctx, local_error("daemon_error", e), opts), false),
    };
    if !reply.status.is_success() {
        let err = reply_error(reply.status.as_u16(), &reply.body);
        return (error_line(ctx, err, opts), false);
    }
    let mut m = Map::new();
    m.insert(opts.id_field.clone(), ctx.id.clone());
    for (k, v) in &ctx.kept {
        m.insert(k.clone(), v.clone());
    }
    if opts.flat {
        match job.protocol.flatten(&reply.body) {
            Ok(cols) => m.extend(cols),
            Err(e) => {
                return (
                    error_line(ctx, local_error("bad_response", e.0), opts),
                    false,
                );
            }
        }
    } else {
        match serde_json::from_slice::<Value>(&reply.body) {
            Ok(v) => {
                m.insert("response".into(), v);
            }
            Err(e) => {
                return (
                    error_line(ctx, local_error("bad_response", e.to_string()), opts),
                    false,
                );
            }
        }
    }
    (Value::Object(m).to_string(), true)
}

struct Writer<'a, W> {
    out: &'a mut W,
    unordered: bool,
    next: usize,
    pending: BTreeMap<usize, String>,
    outcome: Outcome,
}

impl<W: AsyncWrite + Unpin> Writer<'_, W> {
    async fn push(&mut self, seq: usize, text: String, ok: bool) -> Result<(), String> {
        self.outcome.lines += 1;
        if !ok {
            self.outcome.failed += 1;
        }
        if self.unordered {
            return self.write(&text).await;
        }
        self.pending.insert(seq, text);
        while let Some(text) = self.pending.remove(&self.next) {
            self.write(&text).await?;
            self.next += 1;
        }
        Ok(())
    }

    async fn write(&mut self, text: &str) -> Result<(), String> {
        let io = |e: std::io::Error| format!("cannot write stdout: {e}");
        self.out.write_all(text.as_bytes()).await.map_err(io)?;
        self.out.write_all(b"\n").await.map_err(io)?;
        self.out.flush().await.map_err(io)
    }

    /// Write what is left (only after a fatal stop can gaps remain).
    async fn flush(&mut self) -> Result<(), String> {
        let rest = std::mem::take(&mut self.pending);
        for (_, text) in rest {
            self.write(&text).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::systemone::Systemone;
    use axum::http::{HeaderMap, StatusCode};
    use proptest::prelude::*;
    use std::time::Duration;

    fn job() -> Arc<Job> {
        let template = json!({
            "model": "decide",
            "questions": {
                "urgent": {"type": "noul", "instructions": "Urgent?"},
                "topic": {"type": "choice", "instructions": "Topic?", "criteria": {"billing": "B", "tech": "T"}}
            }
        });
        Arc::new(Job {
            template: template.as_object().unwrap().clone(),
            protocol: &Systemone,
            state_mode: StateMode::Value,
            answer_names: vec!["urgent".into(), "topic".into()],
        })
    }

    /// A fake daemon: answers from the state; `fail` in the state gives 413;
    /// `sleep:N` delays N ms.
    fn fake_send(seen: Arc<std::sync::Mutex<Vec<Value>>>) -> SendFn {
        Arc::new(move |body: Bytes, _lane: Lane| {
            let seen = seen.clone();
            Box::pin(async move {
                let req: Value = serde_json::from_slice(&body).unwrap();
                seen.lock().unwrap().push(req.clone());
                let state = req["state"].to_string();
                if let Some(ms) = state
                    .split("sleep:")
                    .nth(1)
                    .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|d| d.parse::<u64>().ok())
                {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                if state.contains("fail") {
                    return Ok(Reply {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        headers: HeaderMap::new(),
                        body: Bytes::from_static(
                            br#"{"detail":{"error_type":"too_long","message":"too long"}}"#,
                        ),
                    });
                }
                let noul = if state.contains("yes") { 0.9 } else { 0.1 };
                let body = json!({"model": "m", "answers": {
                    "urgent": {"type": "noul", "noul": noul},
                    "topic": {"type": "choice", "choice": "billing", "confidence": 0.5, "probabilities": {"billing": 0.75, "tech": 0.25}}
                }, "usage": {"input_tokens": 1, "output_tokens": 0}});
                Ok(Reply {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: Bytes::from(body.to_string()),
                })
            })
        })
    }

    async fn run_lines(
        input: &str,
        opts: &StreamOpts,
    ) -> (Result<Outcome, String>, Vec<Value>, Vec<Value>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = Vec::new();
        let res = run(
            input.as_bytes(),
            &mut out,
            job(),
            opts,
            fake_send(seen.clone()),
        )
        .await;
        let lines = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let sent = seen.lock().unwrap().clone();
        (res, lines, sent)
    }

    #[tokio::test]
    async fn whole_line_is_the_state_by_default() {
        let (res, out, sent) =
            run_lines("{\"id\":1,\"body\":\"yes\"}\n", &StreamOpts::default()).await;
        assert_eq!(
            res.unwrap(),
            Outcome {
                lines: 1,
                failed: 0
            }
        );
        assert_eq!(sent[0]["state"], json!({"id": 1, "body": "yes"}));
        assert_eq!(sent[0]["model"], "decide");
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["response"]["answers"]["urgent"]["noul"], 0.9);
        assert_eq!(out[0].as_object().unwrap().len(), 2, "id and response only");
    }

    #[tokio::test]
    async fn state_field_keep_and_flat() {
        let opts = StreamOpts {
            state_field: Some("body".into()),
            keep: Keep::parse(Some("customer, body")),
            flat: true,
            ..Default::default()
        };
        let (res, out, sent) = run_lines(
            "{\"id\":\"a\",\"body\":\"yes\",\"customer\":\"c1\",\"x\":1}\n",
            &opts,
        )
        .await;
        res.unwrap();
        assert_eq!(sent[0]["state"], "yes");
        let o = out[0].as_object().unwrap();
        let keys: Vec<&str> = o.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "customer",
                "body",
                "urgent",
                "topic",
                "topic__conf",
                "topic__p__billing",
                "topic__p__tech"
            ]
        );
        assert_eq!(o["urgent"], 0.9);
        assert_eq!(o["topic"], "billing");
    }

    #[tokio::test]
    async fn keep_star_copies_the_line_and_collisions_stop_the_stream() {
        let opts = StreamOpts {
            keep: Keep::All,
            flat: true,
            state_field: Some("body".into()),
            ..Default::default()
        };
        let input = "{\"id\":1,\"body\":\"yes\",\"earlier\":0.5}\n{\"id\":2,\"body\":\"no\",\"topic__conf\":0.9}\n{\"id\":3,\"body\":\"no\"}\n";
        let (res, out, sent) = run_lines(input, &opts).await;
        let err = res.unwrap_err();
        assert!(err.contains("topic__conf"), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert_eq!(out.len(), 1, "lines before the collision are written");
        assert_eq!(out[0]["earlier"], 0.5);
        assert_eq!(sent.len(), 1, "nothing sent from or after the bad line");

        // Without --flat only `response` can collide.
        let opts = StreamOpts {
            keep: Keep::All,
            ..Default::default()
        };
        let (res, _, _) = run_lines("{\"id\":1,\"topic\":\"x\",\"response\":1}\n", &opts).await;
        assert!(res.unwrap_err().contains("`response`"));
    }

    #[tokio::test]
    async fn id_fields_that_collide_with_output_keys_are_rejected() {
        for (id_field, flat) in [
            ("topic__conf", true),
            ("urgent", true),
            ("error", true),
            ("response", false),
            ("error", false),
        ] {
            let opts = StreamOpts {
                id_field: id_field.into(),
                flat,
                ..Default::default()
            };
            let (res, out, sent) = run_lines("{\"id\":1}\n", &opts).await;
            let err = res.unwrap_err();
            assert!(err.contains(id_field), "{id_field}: {err}");
            assert!(out.is_empty() && sent.is_empty(), "{id_field}");
        }
        // `response` is free with --flat; a question-like name without it.
        for (id_field, flat) in [("response", true), ("topic__conf", false)] {
            let opts = StreamOpts {
                id_field: id_field.into(),
                flat,
                ..Default::default()
            };
            let (res, _, _) = run_lines("{\"id\":1}\n", &opts).await;
            assert!(res.is_ok(), "{id_field}");
        }
    }

    #[tokio::test]
    async fn per_line_errors_do_not_stop_the_stream() {
        let opts = StreamOpts {
            id_field: "key".into(),
            keep: Keep::parse(Some("tag")),
            state_field: Some("body".into()),
            ..Default::default()
        };
        let input = "{\"key\":1,\"body\":\"yes\",\"tag\":\"t1\"}\nnot json\n{\"key\":3,\"body\":\"fail\",\"tag\":\"t3\"}\n\n{\"key\":4,\"tag\":\"t4\"}\n[1]\n";
        let (res, out, _) = run_lines(input, &opts).await;
        assert_eq!(
            res.unwrap(),
            Outcome {
                lines: 5,
                failed: 4
            }
        );
        assert_eq!(out.len(), 5, "blank lines are skipped");
        assert_eq!(out[0]["key"], 1);
        assert!(out[0].get("error").is_none());
        assert_eq!(out[1]["key"], Value::Null);
        assert_eq!(out[1]["error"]["code"], "invalid_input");
        assert_eq!(out[2]["key"], 3);
        assert_eq!(out[2]["tag"], "t3");
        assert_eq!(out[2]["error"]["code"], "too_long");
        assert_eq!(out[2]["error"]["status"], 413);
        assert_eq!(out[3]["error"]["message"], "input line has no `body` field");
        assert_eq!(out[3]["tag"], "t4");
        assert_eq!(out[4]["error"]["code"], "invalid_input");
    }

    #[tokio::test]
    async fn image_field_sends_absolute_paths_as_assets() {
        let opts = StreamOpts {
            state_field: Some("body".into()),
            image_field: Some("photo".into()),
            // One at a time, so `sent` is in input order.
            max_in_flight: Some(1),
            ..Default::default()
        };
        let input = format!(
            "{}\n{}\n{}\n",
            json!({"id": 1, "body": "yes", "photo": "/tmp/a.png"}),
            json!({"id": 2, "body": "yes", "photo": "rel/b.png"}),
            json!({"id": 3, "body": "yes", "photo": ["/x/1.jpg", "/x/2.png"]})
        );
        let (res, _, sent) = run_lines(&input, &opts).await;
        assert_eq!(res.unwrap().failed, 0);
        let state = sent[0]["state"].as_object().unwrap();
        assert_eq!(state["body"], "yes");
        let placeholder = state["photo"].as_str().unwrap();
        // The CLI never reads the file; the daemon does.
        assert_eq!(
            sent[0]["assets"][placeholder],
            json!({"type": "image", "s1_path": "/tmp/a.png"})
        );
        let placeholder = sent[1]["state"]["photo"].as_str().unwrap();
        let rel = sent[1]["assets"][placeholder]["s1_path"].as_str().unwrap();
        assert!(Path::new(rel).is_absolute(), "{rel}");
        assert!(rel.ends_with("rel/b.png"), "{rel}");
        let refs = sent[2]["state"]["photo"].as_array().unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(
            sent[2]["assets"][refs[1].as_str().unwrap()]["s1_path"],
            "/x/2.png"
        );
    }

    #[test]
    fn json_text_state_for_string_only_formats() {
        let mut j = Arc::try_unwrap(job()).ok().unwrap();
        j.state_mode = StateMode::JsonText;
        let obj = json!({"id": 1, "a": [1, 2]});
        let body = build_body(obj.as_object().unwrap(), &j, &StreamOpts::default()).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["state"], r#"{"id":1,"a":[1,2]}"#);
    }

    #[test]
    fn openai_input_is_text_and_images_are_data_urls() {
        let mut j = Arc::try_unwrap(job()).ok().unwrap();
        j.state_mode = StateMode::OpenaiInput;
        let obj = json!({"id": 1, "body": "hi"});
        let opts = StreamOpts {
            state_field: Some("body".into()),
            image_field: Some("photo".into()),
            ..Default::default()
        };
        let v: Value =
            serde_json::from_slice(&build_body(obj.as_object().unwrap(), &j, &opts).unwrap())
                .unwrap();
        assert_eq!(v["input"], "hi");
        assert!(v.get("state").is_none());

        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n0000").unwrap();
        let obj = json!({"id": 1, "body": "hi", "photo": png.to_str().unwrap()});
        let v: Value =
            serde_json::from_slice(&build_body(obj.as_object().unwrap(), &j, &opts).unwrap())
                .unwrap();
        let parts = &v["input"][0]["content"];
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(parts[0], json!({"type": "input_text", "text": "hi"}));
        assert!(
            parts[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        let gif = dir.path().join("a.gif");
        std::fs::write(&gif, b"GIF89a").unwrap();
        let obj = json!({"body": "hi", "photo": gif.to_str().unwrap()});
        assert!(build_body(obj.as_object().unwrap(), &j, &opts).is_err());
    }

    #[test]
    fn reply_errors_are_uniform() {
        let e = reply_error(
            503,
            br#"{"detail":{"error_type":"backend_unavailable","message":"down"}}"#,
        );
        assert_eq!(
            e,
            json!({"code": "backend_unavailable", "message": "down", "status": 503})
        );
        let e = reply_error(429, br#"{"error":{"code":"queue_full","message":"busy"}}"#);
        assert_eq!(e["code"], "queue_full");
        let e = reply_error(
            422,
            br#"{"detail":[{"loc":["body"],"msg":"x","type":"missing"}]}"#,
        );
        assert_eq!(e["code"], "http_422");
        assert_eq!(e["status"], 422);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        /// Streaming CLI: one output line per input line, same ids, in input
        /// order unless `--unordered` (then the same set).
        #[test]
        fn output_order_and_ids_match_input(
            items in proptest::collection::vec((0u64..15, any::<bool>(), any::<bool>()), 0..24),
            unordered in any::<bool>(),
            window in 1usize..6,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let input: String = items
                .iter()
                .enumerate()
                .map(|(i, (ms, fail, bad))| {
                    if *bad && *fail {
                        "garbage\n".to_string()
                    } else {
                        let body = format!("sleep:{ms} {}", if *fail { "fail" } else { "ok" });
                        format!("{}\n", json!({"id": format!("row-{i}"), "body": body}))
                    }
                })
                .collect();
            let opts = StreamOpts {
                state_field: Some("body".into()),
                unordered,
                max_in_flight: Some(window),
                flat: true,
                ..Default::default()
            };
            let (res, out, _) = rt.block_on(run_lines(&input, &opts));
            let outcome = res.unwrap();
            prop_assert_eq!(out.len(), items.len());
            prop_assert_eq!(outcome.lines, items.len());
            let expected: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, (_, fail, bad))| if *bad && *fail { Value::Null } else { json!(format!("row-{i}")) })
                .collect();
            let mut got: Vec<Value> = out.iter().map(|l| l["id"].clone()).collect();
            if unordered {
                let key = |v: &Value| v.to_string();
                let mut exp = expected.clone();
                exp.sort_by_key(key);
                got.sort_by_key(key);
                prop_assert_eq!(got, exp);
            } else {
                prop_assert_eq!(got, expected);
            }
            let failed = items.iter().filter(|(_, fail, _)| *fail).count();
            prop_assert_eq!(outcome.failed, failed);
        }
    }

    /// A daemon that reports cap `cap`, records lanes and concurrency, and
    /// answers after `ms`.
    #[derive(Default)]
    struct Probe {
        in_flight: usize,
        /// In flight when each call started (including itself), and its lane.
        starts: Vec<(usize, Lane)>,
    }

    fn probe_send(probe: Arc<std::sync::Mutex<Probe>>, cap: u32, ms: u64) -> SendFn {
        Arc::new(move |_body: Bytes, lane: Lane| {
            let probe = probe.clone();
            Box::pin(async move {
                {
                    let mut p = probe.lock().unwrap();
                    p.in_flight += 1;
                    let n = p.in_flight;
                    p.starts.push((n, lane));
                }
                tokio::time::sleep(Duration::from_millis(ms)).await;
                probe.lock().unwrap().in_flight -= 1;
                let mut headers = HeaderMap::new();
                headers.insert(CAPACITY_HEADER, cap.to_string().parse().unwrap());
                let body = json!({"model": "m", "answers": {
                    "urgent": {"type": "noul", "noul": 0.5},
                    "topic": {"type": "choice", "choice": "billing", "confidence": 0.5, "probabilities": {"billing": 0.5, "tech": 0.5}}
                }});
                Ok(Reply {
                    status: StatusCode::OK,
                    headers,
                    body: Bytes::from(body.to_string()),
                })
            })
        })
    }

    fn lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("{}\n", json!({"id": i, "body": "x"})))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn window_follows_the_backend_cap_and_lanes_switch_to_bulk() {
        let probe = Arc::new(std::sync::Mutex::new(Probe::default()));
        let opts = StreamOpts {
            state_field: Some("body".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let res = run(
            lines(20).as_bytes(),
            &mut out,
            job(),
            &opts,
            probe_send(probe.clone(), 2, 10),
        )
        .await
        .unwrap();
        assert_eq!(res.lines, 20);
        let p = probe.lock().unwrap();
        // Before any reply: the initial window.
        assert!(
            p.starts[..INITIAL_WINDOW]
                .iter()
                .all(|(n, _)| *n <= INITIAL_WINDOW)
        );
        // After the first reply reported cap 2, never more than 2.
        assert!(
            p.starts[INITIAL_WINDOW..].iter().all(|(n, _)| *n <= 2),
            "{:?}",
            p.starts
        );
        let lanes: Vec<Lane> = p.starts.iter().map(|(_, l)| *l).collect();
        assert!(
            lanes[..INTERACTIVE_FIRST]
                .iter()
                .all(|l| *l == Lane::Interactive)
        );
        assert!(lanes[INTERACTIVE_FIRST..].iter().all(|l| *l == Lane::Bulk));
    }

    #[tokio::test(start_paused = true)]
    async fn j_caps_the_window_below_the_backend_cap() {
        let probe = Arc::new(std::sync::Mutex::new(Probe::default()));
        let opts = StreamOpts {
            state_field: Some("body".into()),
            max_in_flight: Some(3),
            ..Default::default()
        };
        let mut out = Vec::new();
        run(
            lines(30).as_bytes(),
            &mut out,
            job(),
            &opts,
            probe_send(probe.clone(), 50, 10),
        )
        .await
        .unwrap();
        let p = probe.lock().unwrap();
        let max = p.starts.iter().map(|(n, _)| *n).max().unwrap();
        assert_eq!(max, 3, "cap 50 reported, -j 3 wins");
    }

    #[tokio::test]
    async fn a_full_window_stops_reading_stdin() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let send: SendFn = {
            let (gate, calls) = (gate.clone(), calls.clone());
            Arc::new(move |_body: Bytes, _lane: Lane| {
                let (gate, calls) = (gate.clone(), calls.clone());
                Box::pin(async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _p = gate.acquire().await.unwrap();
                    Ok(Reply {
                        status: StatusCode::OK,
                        headers: HeaderMap::new(),
                        body: Bytes::from_static(br#"{"answers":{}}"#),
                    })
                })
            })
        };
        // A small pipe: the producer blocks once the reader stops.
        let (mut producer, consumer) = tokio::io::duplex(256);
        let input = lines(500);
        let writer = tokio::spawn(async move {
            producer.write_all(input.as_bytes()).await.unwrap();
        });
        let opts = StreamOpts {
            state_field: Some("body".into()),
            max_in_flight: Some(2),
            ..Default::default()
        };
        let runner = tokio::spawn(async move {
            let mut out = Vec::new();
            run(
                tokio::io::BufReader::new(consumer),
                &mut out,
                job(),
                &opts,
                send,
            )
            .await
            .map(|o| o.lines)
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!writer.is_finished(), "the producer is blocked by the pipe");
        gate.add_permits(10_000);
        writer.await.unwrap();
        assert_eq!(runner.await.unwrap().unwrap(), 500);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 500);
    }

    /// Answers every request at once except request #0, which waits for
    /// `head`. Counts the requests sent.
    fn stalled_head_send(
        head: Arc<tokio::sync::Semaphore>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> SendFn {
        Arc::new(move |_body: Bytes, _lane: Lane| {
            let (head, calls) = (head.clone(), calls.clone());
            Box::pin(async move {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    let _p = head.acquire().await.unwrap();
                }
                Ok(Reply {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(br#"{"answers":{}}"#),
                })
            })
        })
    }

    #[tokio::test]
    async fn a_slow_first_line_stops_reading_stdin_in_ordered_mode() {
        let head = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let send = stalled_head_send(head.clone(), calls.clone());
        let (mut producer, consumer) = tokio::io::duplex(256);
        let input = lines(20_000);
        let writer = tokio::spawn(async move {
            producer.write_all(input.as_bytes()).await.unwrap();
        });
        let opts = StreamOpts {
            state_field: Some("body".into()),
            max_in_flight: Some(2),
            ..Default::default()
        };
        let runner = tokio::spawn(async move {
            let mut out = Vec::new();
            let o = run(
                tokio::io::BufReader::new(consumer),
                &mut out,
                job(),
                &opts,
                send,
            )
            .await
            .map(|o| o.lines);
            (o, out)
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Line 0 in flight, line 1 answered and held: the window (2) is full.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!writer.is_finished(), "the producer is blocked by the pipe");
        head.add_permits(1);
        writer.await.unwrap();
        let (o, out) = runner.await.unwrap();
        assert_eq!(o.unwrap(), 20_000);
        let ids: Vec<u64> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["id"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert!(ids.iter().copied().eq(0..20_000), "ordered output");
    }

    #[tokio::test]
    async fn a_reply_is_written_while_stdin_stays_open() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut producer, consumer) = tokio::io::duplex(4096);
        let (out_w, out_r) = tokio::io::duplex(4096);
        let opts = StreamOpts {
            state_field: Some("body".into()),
            ..Default::default()
        };
        let send = fake_send(seen);
        let runner = tokio::spawn(async move {
            let mut out_w = out_w;
            run(
                tokio::io::BufReader::new(consumer),
                &mut out_w,
                job(),
                &opts,
                send,
            )
            .await
            .map(|o| o.lines)
        });
        let mut out = tokio::io::BufReader::new(out_r).lines();
        for i in 0..3 {
            producer
                .write_all(format!("{}\n", json!({"id": i, "body": "x"})).as_bytes())
                .await
                .unwrap();
            // stdin stays open: the answer must still come out.
            let line = tokio::time::timeout(Duration::from_secs(2), out.next_line())
                .await
                .expect("reply held back while stdin is open")
                .unwrap()
                .unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["id"], i);
        }
        drop(producer);
        assert_eq!(runner.await.unwrap().unwrap(), 3);
    }

    #[tokio::test]
    async fn local_error_lines_count_against_the_window() {
        // Line 0 stalls; the rest are invalid lines. Held error lines fill
        // the window, so stdin reads stop.
        let head = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let send = stalled_head_send(head.clone(), calls.clone());
        let (mut producer, consumer) = tokio::io::duplex(256);
        let input = format!(
            "{}\n{}",
            json!({"id": 0, "body": "x"}),
            "not json\n".repeat(20_000)
        );
        let writer = tokio::spawn(async move {
            producer.write_all(input.as_bytes()).await.unwrap();
        });
        let opts = StreamOpts {
            state_field: Some("body".into()),
            max_in_flight: Some(4),
            ..Default::default()
        };
        let runner = tokio::spawn(async move {
            let mut out = Vec::new();
            run(
                tokio::io::BufReader::new(consumer),
                &mut out,
                job(),
                &opts,
                send,
            )
            .await
            .map(|o| o.lines)
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!writer.is_finished(), "the producer is blocked by the pipe");
        head.add_permits(1);
        writer.await.unwrap();
        assert_eq!(runner.await.unwrap().unwrap(), 20_001);
    }
}
