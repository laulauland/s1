//! `s1 auth set|list|token`. Never prints key values (except `token`, which
//! prints the local bearer token on purpose, for MCP client setup).

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use clap::Subcommand;

use crate::paths::Paths;
use crate::secrets::{KeyResolver, LOCAL_TOKEN_ENTRY, Secret};

#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    /// Store a provider key (prompt without echo, or read stdin).
    Set {
        /// Provider name, e.g. typesafe, workers-ai, openai.
        provider: String,
        /// Cloudflare account id (workers-ai).
        #[arg(long)]
        account_id: Option<String>,
    },
    /// Show which providers have keys and where they come from.
    List,
    /// Print the local bearer token for TCP clients (made on first use).
    Token,
}

pub fn run(cmd: AuthCmd, paths: &Paths) -> Result<ExitCode, String> {
    let keys = KeyResolver::from_process(paths);
    match cmd {
        AuthCmd::Set {
            provider,
            account_id,
        } => {
            let key = read_key(&provider)?;
            if key.is_empty() {
                return Err("empty key; nothing written".into());
            }
            keys.update(|f| f.set_api_key(&provider, &Secret::new(key), account_id.as_deref()))
                .map_err(|e| e.to_string())?;
            eprintln!(
                "saved key for `{provider}` in {}",
                keys.auth_path().display()
            );
            Ok(ExitCode::SUCCESS)
        }
        AuthCmd::List => {
            let list = keys.list().map_err(|e| e.to_string())?;
            if list.is_empty() {
                println!("no keys; run `s1 auth set <provider>`");
            }
            for k in list {
                let note = if k.provider == LOCAL_TOKEN_ENTRY {
                    " (local bearer token)"
                } else {
                    ""
                };
                println!(
                    "{}  {}  {}{note}",
                    k.provider,
                    k.entry_type,
                    k.source.as_str()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        AuthCmd::Token => {
            let t = keys.ensure_local_token().map_err(|e| e.to_string())?;
            println!("{}", t.expose());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn read_key(provider: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{provider} API key: "))
            .map(|k| k.trim().to_string())
            .map_err(|e| format!("cannot read key: {e}"))
    } else {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        Ok(s.trim().to_string())
    }
}
