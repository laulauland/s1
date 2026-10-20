//! `s1 status`: per backend and key, queue depth per lane, current rate,
//! in-flight cap, limits, `throttled_until`, and breaker state; plus cache
//! size (`GET /status`).

use std::process::ExitCode;

use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use clap::Args;
use serde_json::Value;

use super::client::{ensure_daemon, request};
use crate::paths::Paths;

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Print the `GET /status` JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: StatusArgs, paths: &Paths) -> Result<ExitCode, String> {
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::GET,
        "/status",
        HeaderMap::new(),
        Bytes::new(),
    ))?;
    if !reply.status.is_success() {
        return Err(format!(
            "daemon returned {}: {}",
            reply.status,
            String::from_utf8_lossy(&reply.body)
        ));
    }
    let v: Value =
        serde_json::from_slice(&reply.body).map_err(|e| format!("bad daemon reply: {e}"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&v).expect("serialize"));
    } else {
        print!("{}", render(&v));
    }
    Ok(ExitCode::SUCCESS)
}

fn num(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn render(v: &Value) -> String {
    let header = [
        "BACKEND",
        "KEY",
        "QUEUE I/B",
        "IN FLIGHT",
        "CAP",
        "REQ/MIN",
        "TOK/MIN",
        "RPM",
        "TPM",
        "BREAKER",
        "THROTTLED UNTIL",
    ];
    let mut rows: Vec<Vec<String>> = vec![header.iter().map(|s| s.to_string()).collect()];
    for b in v["backends"].as_array().into_iter().flatten() {
        rows.push(vec![
            num(&b["provider"]),
            num(&b["key"]),
            format!(
                "{}/{}",
                num(&b["queue"]["interactive"]),
                num(&b["queue"]["bulk"])
            ),
            num(&b["in_flight"]),
            format!("{}/{}", num(&b["cap"]), num(&b["limits"]["max_in_flight"])),
            num(&b["rate"]["requests_last_minute"]),
            num(&b["rate"]["tokens_last_minute"]),
            num(&b["limits"]["rpm"]),
            num(&b["limits"]["tpm"]),
            num(&b["breaker"]["state"]),
            num(&b["throttled_until"]),
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in &rows {
        let line: Vec<String> = r
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    if rows.len() == 1 {
        out.push_str("(no backend has a key; see `s1 auth list`)\n");
    }
    match &v["cache"] {
        Value::Null => out.push_str("\ncache: off\n"),
        c => out.push_str(&format!(
            "\ncache: {} entries, {} response bytes, file {} of {} bytes\n",
            num(&c["entries"]),
            num(&c["bytes"]),
            num(&c["file_bytes"]),
            num(&c["max_bytes"])
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_shows_queue_cap_and_breaker() {
        let v = json!({
            "backends": [{
                "provider": "typesafe", "key": "k_01234567",
                "queue": {"interactive": 2, "bulk": 7, "tokens": 10, "max_requests": 1024, "max_tokens": 1},
                "in_flight": 3, "cap": 16,
                "rate": {"requests_last_minute": 40, "tokens_last_minute": 900, "completed_last_minute": 37},
                "limits": {"rpm": 600, "tpm": null, "max_in_flight": 32, "adaptive": true},
                "throttled_until": null,
                "breaker": {"state": "open", "open_until": "2026-10-20T10:00:00Z", "recent_failures": 5}
            }],
            "cache": {"entries": 3, "bytes": 900, "max_bytes": 1073741824, "file_bytes": 1056768}
        });
        let t = render(&v);
        assert!(t.contains("typesafe"), "{t}");
        assert!(t.contains("2/7"), "{t}");
        assert!(t.contains("16/32"), "{t}");
        assert!(t.contains("open"), "{t}");
        assert!(t.contains("cache: 3 entries"), "{t}");
        assert!(t.contains("file 1056768 of 1073741824 bytes"), "{t}");
        let t = render(&json!({"backends": [], "cache": null}));
        assert!(t.contains("no backend has a key"), "{t}");
        assert!(t.contains("cache: off"), "{t}");
    }
}
