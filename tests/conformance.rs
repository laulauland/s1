//! SDK conformance: the official OpenAI and TypeSafe Python SDKs, unchanged,
//! with base URL = s1 (TCP, local token as the key) and mock providers.
//! The Python side is a uv project in `conformance/`. Opt-in, as it needs
//! `uv` and (on first run) network access to install the SDKs:
//!
//! ```sh
//! S1_CONFORMANCE=1 cargo test --test conformance -- --ignored --nocapture
//! ```
//!
//! With `S1_LIVE=1` as well, `official_sdks_live` runs the same calls
//! through s1 to the real providers (keys from the env or `~/.s1/auth.json`,
//! read only) and checks shapes only.

mod common;

use std::process::Command;

use common::{CF_ACCOUNT, CF_TOKEN, Mocks, OPENAI_KEY, TEST_KEY};
use s1::catalog::Catalog;
use s1::daemon::{AppState, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "conformance: needs S1_CONFORMANCE=1 and uv"]
async fn official_sdks_work_with_base_url_s1() {
    if std::env::var("S1_CONFORMANCE").as_deref() != Ok("1") {
        eprintln!("skip: S1_CONFORMANCE is not 1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    keys.update(|f| {
        f.set_api_key("typesafe", &Secret::new(TEST_KEY), None);
        f.set_api_key("workers-ai", &Secret::new(CF_TOKEN), Some(CF_ACCOUNT));
        f.set_api_key("openai", &Secret::new(OPENAI_KEY), None);
    })
    .unwrap();
    let token = keys.ensure_local_token().unwrap();
    let token_text = token.expose().to_string();
    let mocks = Mocks::start().await;
    let catalog = Catalog::with_overlay_str(&mocks.overlay()).unwrap();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(catalog, keys),
        token,
    })
    .await
    .unwrap();
    let out = run_sdks(&server, token_text, false).await;
    server.shutdown().await;
    // The TypeSafe SDK compresses bodies over 1 KiB; providers got them plain.
    let plain = mocks
        .jev
        .all()
        .iter()
        .chain(mocks.clef.all().iter())
        .chain(mocks.luna.all().iter())
        .all(|s| s.headers.get("content-encoding").is_none());
    assert!(out, "conformance failed");
    assert!(plain, "a provider got a compressed body");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "conformance: needs S1_CONFORMANCE=1, S1_LIVE=1, uv and real keys"]
async fn official_sdks_live() {
    if std::env::var("S1_CONFORMANCE").as_deref() != Ok("1")
        || std::env::var("S1_LIVE").as_deref() != Ok("1")
    {
        eprintln!("skip: S1_CONFORMANCE and S1_LIVE must both be 1");
        return;
    }
    let real = KeyResolver::from_process(&Paths::from_env());
    let mut env: Vec<(&'static str, String)> = Vec::new();
    for p in ["typesafe", "workers-ai", "openai"] {
        let Ok(Some(k)) = real.resolve(p) else {
            eprintln!("skip: no key for {p}");
            return;
        };
        match p {
            "typesafe" => env.push(("TYPESAFE_API_KEY", k.key.expose().to_string())),
            "openai" => env.push(("OPENAI_API_KEY", k.key.expose().to_string())),
            _ => {
                let Some(acc) = k.account_id.clone() else {
                    eprintln!("skip: no Cloudflare account id");
                    return;
                };
                env.push(("CLOUDFLARE_API_TOKEN", k.key.expose().to_string()));
                env.push(("CLOUDFLARE_ACCOUNT_ID", acc));
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), move |name| {
        env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
    });
    let token = keys.ensure_local_token().unwrap();
    let token_text = token.expose().to_string();
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token,
    })
    .await
    .unwrap();
    let ok = run_sdks(&server, token_text, true).await;
    server.shutdown().await;
    assert!(ok, "live conformance failed");
}

/// Run `conformance.py` against `server` over TCP. True on success.
async fn run_sdks(server: &s1::daemon::RunningServer, token: String, live: bool) -> bool {
    let base = format!("http://{}", server.tcp);
    let project = format!("{}/conformance", env!("CARGO_MANIFEST_DIR"));
    let out = tokio::task::spawn_blocking(move || {
        Command::new("uv")
            .args(["run", "--frozen", "--quiet", "conformance.py"])
            .current_dir(&project)
            // Keep SDK settings from the user's shell out of the run.
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("TYPESAFE_BASE_URL")
            .env("S1_BASE_URL", base)
            .env("S1_TOKEN", token)
            .env("S1_CONFORMANCE_LIVE", if live { "1" } else { "0" })
            .output()
    })
    .await
    .unwrap()
    .expect("run uv (is it installed?)");
    println!("{}", String::from_utf8_lossy(&out.stdout));
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    out.status.success()
}
