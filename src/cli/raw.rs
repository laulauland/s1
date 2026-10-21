//! `s1 util raw <protocol> < body.json`: pure passthrough through the daemon.

use std::io::{Read, Write};
use std::process::ExitCode;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use bytes::Bytes;
use clap::Args;

use super::client::{ensure_daemon, request};
use crate::paths::Paths;

#[derive(Debug, Args)]
pub struct RawArgs {
    /// Wire format: systemone, openai-decisions, clairic, tsfm.
    pub protocol: String,
    /// Provider name from the catalog (default per protocol; systemone: typesafe).
    #[arg(long)]
    pub provider: Option<String>,
    /// Extra request header, `Name: value` (repeatable), e.g.
    /// `-H 'TypeSafe-Danger-Zone-Images: true'`.
    #[arg(short = 'H', long = "header")]
    pub headers: Vec<String>,
    /// Print the status line and response headers to stderr.
    #[arg(short = 'i', long)]
    pub include: bool,
}

pub fn run(args: RawArgs, paths: &Paths) -> Result<ExitCode, String> {
    let mut body = Vec::new();
    std::io::stdin()
        .read_to_end(&mut body)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    for h in &args.headers {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| format!("header `{h}` must look like `Name: value`"))?;
        let name = HeaderName::try_from(name.trim()).map_err(|e| format!("header `{h}`: {e}"))?;
        let value =
            HeaderValue::try_from(value.trim()).map_err(|e| format!("header `{h}`: {e}"))?;
        headers.insert(name, value);
    }
    let mut path = format!("/v1/raw/{}", args.protocol);
    if let Some(p) = &args.provider {
        path.push_str(&format!("?provider={p}"));
    }
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::POST,
        &path,
        headers,
        Bytes::from(body),
    ))?;
    if args.include {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "HTTP {}", reply.status);
        for (k, v) in &reply.headers {
            let _ = writeln!(err, "{k}: {}", v.to_str().unwrap_or("<binary>"));
        }
    }
    let mut out = std::io::stdout().lock();
    out.write_all(&reply.body)
        .and_then(|_| out.flush())
        .map_err(|e| format!("cannot write stdout: {e}"))?;
    Ok(if reply.status.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
