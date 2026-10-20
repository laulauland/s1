//! `s1 extract -t tpl.json "text"` or `... < doc.txt` (single), and
//! `... < lines.jsonl` (streaming).
//!
//! A template is a normal Clairic request body without the `state`. If it
//! has no `model`, s1 uses the `extract` alias.
//!
//! Input: a text argument is the state; `-` reads the whole of stdin as the
//! state. Without an argument, stdin is JSON lines when `--jsonl` is given
//! or when its first non-blank line is a JSON object; otherwise the whole
//! of stdin is one text (so `s1 extract -t t.json < invoice.txt` works).
//! In streaming mode a state that is not a string is sent as JSON text,
//! because Clairic takes only a string.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use axum::http::Method;
use bytes::Bytes;
use clap::Args;
use serde_json::{Map, Value};
use tokio::io::AsyncReadExt;

use super::client::{ensure_daemon, request};
use super::decide::json_headers;
use super::stream::{self, Job, Keep, SendFn, StateMode, StreamOpts};
use crate::flatten::validate_names;
use crate::paths::Paths;
use crate::protocol::{self, Protocol, ProtocolId};
use crate::queue::Lane;

/// Alias used when neither the template nor `-m` names a model.
pub const DEFAULT_MODEL: &str = "extract";
/// Endpoint of the Extract family.
pub const ENDPOINT: &str = "/v1/extract";

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Request template: a Clairic body without `state`.
    #[arg(short = 't', long = "template")]
    pub template: std::path::PathBuf,
    /// Text to extract from; `-` reads all of stdin as the text. Without
    /// it, s1 reads stdin (JSON lines, or one text).
    pub text: Option<String>,
    /// Model id or alias (overrides the template's `model`).
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// Read JSON lines from stdin, even when stdin is a terminal.
    #[arg(long)]
    pub jsonl: bool,
    /// Write flat columns (`f`, `f__conf`, `f__p_null`, `f__guess`, `f__span`; estimates `f__median`, `f__p10`, `f__p90`).
    #[arg(long)]
    pub flat: bool,
    /// Input field that holds the line id (passed through).
    #[arg(long, default_value = "id")]
    pub id_field: String,
    /// Input field to send as the state (default: the whole line, as JSON text).
    #[arg(long)]
    pub state_field: Option<String>,
    /// Input field with an image path (or a list of paths), sent inline
    /// as data URLs in `images` (PNG or JPEG).
    #[arg(long)]
    pub image_field: Option<String>,
    /// Input fields to copy to the output: `a,b` or `*` for the whole line.
    #[arg(long)]
    pub keep: Option<String>,
    /// Write results in completion order, not input order.
    #[arg(long)]
    pub unordered: bool,
    /// Most requests in flight at once in streaming mode (default: follow
    /// the backend's current cap).
    #[arg(short = 'j', long)]
    pub max_in_flight: Option<usize>,
    /// Skip the cache lookup (the fresh answer is still stored).
    #[arg(long)]
    pub no_cache: bool,
}

/// Read and check a template. Returns the body without `state` and the
/// field names.
pub fn load_template(
    path: &Path,
    model: Option<&str>,
) -> Result<(Map<String, Value>, Vec<String>), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read template {}: {e}", path.display()))?;
    parse_template(&text, model).map_err(|e| format!("template {}: {e}", path.display()))
}

pub fn parse_template(
    text: &str,
    model: Option<&str>,
) -> Result<(Map<String, Value>, Vec<String>), String> {
    let mut t = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => m,
        Ok(_) => return Err("must be a JSON object".into()),
        Err(e) => return Err(format!("not JSON: {e}")),
    };
    let names: Vec<String> = match t.get("fields") {
        Some(Value::Object(f)) if !f.is_empty() => f.keys().cloned().collect(),
        _ => return Err("needs a non-empty `fields` object".into()),
    };
    validate_names(names.iter().map(String::as_str))?;
    t.shift_remove("state");
    match model {
        Some(m) => {
            t.insert("model".into(), Value::String(m.into()));
        }
        None => {
            t.entry("model")
                .or_insert_with(|| Value::String(DEFAULT_MODEL.into()));
        }
    }
    Ok((t, names))
}

/// What stdin holds, decided from its first non-blank line.
#[derive(Debug, PartialEq, Eq)]
pub enum Stdin {
    /// JSON lines; `head` is what was read so far (it starts the stream).
    Lines { head: String },
    /// One text; `head` is its start.
    Text { head: String },
}

/// Read up to the first non-blank line and decide what stdin holds.
pub fn sniff(input: &mut impl BufRead) -> Result<Stdin, String> {
    let mut head = String::new();
    loop {
        let start = head.len();
        let n = input
            .read_line(&mut head)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        let line = head[start..].trim();
        if n == 0 || !line.is_empty() {
            let lines = matches!(serde_json::from_str::<Value>(line), Ok(Value::Object(_)));
            return Ok(if lines {
                Stdin::Lines { head }
            } else {
                Stdin::Text { head }
            });
        }
    }
}

pub fn run(args: ExtractArgs, paths: &Paths) -> Result<ExitCode, String> {
    let proto = protocol::by_id(ProtocolId::Clairic).ok_or("clairic is not in this build")?;
    let (template, names) = load_template(&args.template, args.model.as_deref())?;
    let read_all = |head: String| -> Result<String, String> {
        let mut text = head;
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        Ok(text)
    };
    let text = match args.text.as_deref() {
        Some("-") => read_all(String::new())?,
        Some(t) => t.to_string(),
        None if args.jsonl => {
            return run_stream(args, paths, template, names, proto, String::new());
        }
        None if std::io::stdin().is_terminal() => {
            return Err(
                "give the text to extract from, pipe a document, or pipe JSON lines to stdin"
                    .into(),
            );
        }
        None => {
            // The lock ends here: the rest of stdin is read through new handles.
            let sniffed = sniff(&mut std::io::stdin().lock())?;
            match sniffed {
                Stdin::Lines { head } => {
                    return run_stream(args, paths, template, names, proto, head);
                }
                Stdin::Text { head } => read_all(head)?,
            }
        }
    };
    if text.trim().is_empty() {
        return Err("the text to extract from is empty".into());
    }
    run_single(&args, paths, template, proto, text)
}

fn run_single(
    args: &ExtractArgs,
    paths: &Paths,
    template: Map<String, Value>,
    proto: &'static dyn Protocol,
    text: String,
) -> Result<ExitCode, String> {
    let mut body = template;
    body.insert("state".into(), Value::String(text));
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::POST,
        ENDPOINT,
        json_headers(args.no_cache, Lane::Interactive),
        Bytes::from(serde_json::to_vec(&Value::Object(body)).expect("JSON")),
    ))?;
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| format!("cannot write stdout: {e}");
    if reply.status.is_success() && args.flat {
        let cols = proto
            .flatten(&reply.body)
            .map_err(|e| format!("cannot flatten the response: {e}"))?;
        let line = Value::Object(crate::flatten::to_object(cols)).to_string();
        writeln!(out, "{line}").map_err(io)?;
    } else {
        out.write_all(&reply.body).map_err(io)?;
        if !reply.body.ends_with(b"\n") {
            out.write_all(b"\n").map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    Ok(if reply.status.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_stream(
    args: ExtractArgs,
    paths: &Paths,
    template: Map<String, Value>,
    names: Vec<String>,
    proto: &'static dyn Protocol,
    head: String,
) -> Result<ExitCode, String> {
    if args.flat && names.contains(&args.id_field) {
        return Err(format!(
            "field `{}` has the same name as the id field; rename it or pass --id-field",
            args.id_field
        ));
    }
    let opts = StreamOpts {
        id_field: args.id_field,
        state_field: args.state_field,
        image_field: args.image_field,
        keep: Keep::parse(args.keep.as_deref()),
        unordered: args.unordered,
        flat: args.flat,
        max_in_flight: args.max_in_flight,
    };
    let job = Arc::new(Job {
        template,
        protocol: proto,
        state_mode: StateMode::JsonText,
        answer_names: names,
    });
    ensure_daemon(paths)?;
    let socket = paths.socket.clone();
    let no_cache = args.no_cache;
    let send: SendFn = Arc::new(move |body: Bytes, lane: Lane| {
        let socket = socket.clone();
        Box::pin(async move {
            request(
                &socket,
                Method::POST,
                ENDPOINT,
                json_headers(no_cache, lane),
                body,
            )
            .await
        })
    });
    let outcome = super::runtime()?.block_on(async {
        // The sniffed lines first, then the rest of stdin (std keeps what it
        // buffered; tokio's stdin reads through the same handle).
        let input =
            AsyncReadExt::chain(std::io::Cursor::new(head.into_bytes()), tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        stream::run(
            tokio::io::BufReader::new(input),
            &mut stdout,
            job,
            &opts,
            send,
        )
        .await
    })?;
    if outcome.failed > 0 {
        eprintln!("s1: {} of {} lines failed", outcome.failed, outcome.lines);
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_gets_default_model_and_loses_state() {
        let (t, names) = parse_template(
            r#"{"state":"old","fields":{"invoice_number":{"type":"text"},"total":{"type":"number"}}}"#,
            None,
        )
        .unwrap();
        assert_eq!(t["model"], "extract");
        assert!(t.get("state").is_none());
        assert_eq!(names, ["invoice_number", "total"]);
        let (t, _) = parse_template(
            r#"{"model":"clairic","fields":{"f":{"type":"text"}}}"#,
            Some("workers-ai/clairic"),
        )
        .unwrap();
        assert_eq!(t["model"], "workers-ai/clairic");
    }

    #[test]
    fn templates_need_fields_without_separator_names() {
        let err = parse_template(r#"{"fields":{"bad__name":{"type":"text"}}}"#, None).unwrap_err();
        assert!(err.contains("bad__name"), "{err}");
        assert!(parse_template(r#"{"fields":{}}"#, None).is_err());
        assert!(parse_template(r#"{"questions":{"q":{}}}"#, None).is_err());
        assert!(parse_template("[]", None).is_err());
    }

    #[test]
    fn sniff_tells_json_lines_from_text() {
        let sniffed = |s: &str| sniff(&mut std::io::Cursor::new(s.as_bytes().to_vec())).unwrap();
        assert_eq!(
            sniffed("\n{\"id\":1}\n{\"id\":2}\n"),
            Stdin::Lines {
                head: "\n{\"id\":1}\n".into()
            }
        );
        assert_eq!(
            sniffed("Invoice INV-1\nTotal 4\n"),
            Stdin::Text {
                head: "Invoice INV-1\n".into()
            }
        );
        // JSON that is not an object is text, and so is an empty input.
        assert!(matches!(sniffed("[1, 2]\n"), Stdin::Text { .. }));
        assert!(matches!(sniffed("{not json\n"), Stdin::Text { .. }));
        assert_eq!(sniffed(""), Stdin::Text { head: "".into() });
    }
}
