//! `s1 decide -t tpl.json "text"` (single) and `... < lines.jsonl` (streaming).
//!
//! A template is a normal systemone request body without the `state`. If it
//! has no `model`, s1 uses the `decide` alias.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, Method, header};
use bytes::Bytes;
use clap::Args;
use serde_json::{Map, Value};

use super::client::{ensure_daemon, request};
use super::stream::{self, Job, Keep, SendFn, StateMode, StreamOpts};
use crate::flatten::validate_names;
use crate::paths::Paths;
use crate::protocol::{self, Protocol, ProtocolId};
use crate::queue::{LANE_HEADER, Lane};

/// Alias used when neither the template nor `-m` names a model.
pub const DEFAULT_MODEL: &str = "decide";

#[derive(Debug, Args)]
pub struct DecideArgs {
    /// Request template: a systemone body without `state`.
    #[arg(short = 't', long = "template")]
    pub template: PathBuf,
    /// Text to decide on. Without it, s1 reads JSON lines from stdin.
    pub text: Option<String>,
    /// Model id or alias (overrides the template's `model`).
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// Read JSON lines from stdin even when stdin is a terminal.
    #[arg(long)]
    pub jsonl: bool,
    /// Write flat columns (`q`, `q__conf`, `q__p__<option>`).
    #[arg(long)]
    pub flat: bool,
    /// Input field that holds the line id (passed through).
    #[arg(long, default_value = "id")]
    pub id_field: String,
    /// Input field to send as the state (default: the whole line).
    #[arg(long)]
    pub state_field: Option<String>,
    /// Input field with an image path (or a list of paths).
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

/// Endpoint for a Decide format.
pub fn endpoint(pid: ProtocolId) -> &'static str {
    match pid {
        ProtocolId::OpenaiDecisions => "/v1/decisions",
        _ => "/v1/systemone",
    }
}

/// Read and check a template. Returns the body without `state` and the
/// question names.
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
    let names: Vec<String> = match t.get("questions") {
        Some(Value::Object(q)) if !q.is_empty() => q.keys().cloned().collect(),
        _ => return Err("needs a non-empty `questions` object".into()),
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

fn json_headers(no_cache: bool, lane: Lane) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if no_cache {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    if lane == Lane::Bulk {
        h.insert(LANE_HEADER, HeaderValue::from_static("bulk"));
    }
    h
}

pub fn run(args: DecideArgs, paths: &Paths) -> Result<ExitCode, String> {
    let pid = ProtocolId::Systemone;
    let proto = protocol::by_id(pid).ok_or("systemone is not in this build")?;
    let (template, names) = load_template(&args.template, args.model.as_deref())?;
    let streaming = args.text.is_none() && (args.jsonl || !std::io::stdin().is_terminal());
    let Some(text) = args.text.clone().filter(|_| !streaming) else {
        if !streaming {
            return Err("give the text to decide on, or pipe JSON lines to stdin".into());
        }
        return run_stream(args, paths, template, names, pid, proto);
    };

    let mut body = template;
    body.insert("state".into(), Value::String(text));
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::POST,
        endpoint(pid),
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
    args: DecideArgs,
    paths: &Paths,
    template: Map<String, Value>,
    names: Vec<String>,
    pid: ProtocolId,
    proto: &'static dyn Protocol,
) -> Result<ExitCode, String> {
    if args.flat && names.contains(&args.id_field) {
        return Err(format!(
            "question `{}` has the same name as the id field; rename it or pass --id-field",
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
        state_mode: StateMode::Value,
        answer_names: names,
    });
    ensure_daemon(paths)?;
    let socket = paths.socket.clone();
    let path = endpoint(pid);
    let no_cache = args.no_cache;
    let send: SendFn = Arc::new(move |body: Bytes, lane: Lane| {
        let socket = socket.clone();
        Box::pin(async move {
            request(
                &socket,
                Method::POST,
                path,
                json_headers(no_cache, lane),
                body,
            )
            .await
        })
    });
    let outcome = super::runtime()?.block_on(async {
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        stream::run(stdin, &mut stdout, job, &opts, send).await
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
            r#"{"state":"old","questions":{"urgent":{"type":"noul"},"topic":{"type":"choice","criteria":{"a":null,"b":null}}}}"#,
            None,
        )
        .unwrap();
        assert_eq!(t["model"], "decide");
        assert!(t.get("state").is_none());
        assert_eq!(names, ["urgent", "topic"]);
        let (t, _) = parse_template(
            r#"{"model":"jev-latest","questions":{"q":{"type":"noul"}}}"#,
            Some("workers-ai/clef"),
        )
        .unwrap();
        assert_eq!(t["model"], "workers-ai/clef");
    }

    #[test]
    fn templates_with_separator_names_are_rejected() {
        let err =
            parse_template(r#"{"questions":{"bad__name":{"type":"noul"}}}"#, None).unwrap_err();
        assert!(err.contains("bad__name"), "{err}");
        assert!(parse_template(r#"{"questions":{}}"#, None).is_err());
        assert!(parse_template("[]", None).is_err());
    }
}
