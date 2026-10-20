//! `s1 models`: the merged catalog from the daemon.

use std::process::ExitCode;

use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use clap::Args;
use serde_json::Value;

use super::client::{ensure_daemon, request};
use crate::paths::Paths;

#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// Print the `GET /v1/models` JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: ModelsArgs, paths: &Paths) -> Result<ExitCode, String> {
    ensure_daemon(paths)?;
    let reply = super::runtime()?.block_on(request(
        &paths.socket,
        Method::GET,
        "/v1/models",
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
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", render_table(&v));
    Ok(ExitCode::SUCCESS)
}

fn render_table(v: &Value) -> String {
    let mut rows = vec![[
        "MODEL".to_string(),
        "API".to_string(),
        "FAMILY".to_string(),
        "AVAILABLE".to_string(),
    ]];
    for m in v["data"].as_array().into_iter().flatten() {
        let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
        rows.push([
            s("id"),
            s("api"),
            s("family"),
            if m["state"]["available"] == Value::Bool(true) {
                "yes".into()
            } else {
                "no".into()
            },
        ]);
    }
    let widths: Vec<usize> = (0..4)
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
    if let Some(aliases) = v["aliases"].as_object()
        && !aliases.is_empty()
    {
        out.push_str("\nALIAS  MODELS\n");
        for (name, a) in aliases {
            let models: Vec<&str> = a["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let spill = if a["spill"] == Value::Bool(true) {
                " (spill)"
            } else {
                ""
            };
            out.push_str(&format!("{name}  {}{spill}\n", models.join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_lists_models_and_aliases() {
        let v = crate::catalog::Catalog::builtin().to_models_json(|_| Default::default());
        let t = render_table(&v);
        assert!(t.contains("typesafe/jev-latest"));
        assert!(t.contains("decide  workers-ai/clef-flash, typesafe/jev-latest"));
    }
}
