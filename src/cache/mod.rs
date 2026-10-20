//! Response cache (spec "Cache key" and "Cache"):
//!
//! - [`key`]: blake3 over family, resolved model version, and canonical JSON.
//! - [`store`]: the `redb` file in `~/.cache/s1/` with a size cap and LRU.
//! - [`Versions`]: the last resolved model version per s1 model id,
//!   persisted in `~/.local/state/s1/versions.json`, so a `-latest` model
//!   does not return stale answers after an update.
//!
//! Only successful responses on typed ingresses are cached. Clients skip
//! the lookup with `Cache-Control: no-cache` (the fresh answer is stored)
//! or skip the cache entirely with `Cache-Control: no-store`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};

pub mod key;
pub mod store;

pub use key::Key;
pub use store::{Cache, CacheError, Entry, Stats};

/// Response header: `hit`, `miss`, `refresh` (lookup skipped, answer
/// stored), or `bypass` (cache not used).
pub const CACHE_HEADER: &str = "s1-cache";

/// `[cache]` in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub enabled: bool,
    /// Size cap of the stored responses (default 1 GiB).
    pub max_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 1 << 30,
        }
    }
}

/// How one request uses the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Look up, store on a miss.
    Normal,
    /// Do not look up; store the fresh answer.
    Refresh,
    /// Neither look up nor store.
    Off,
}

impl Mode {
    pub fn from_headers(h: &HeaderMap) -> Self {
        let mut mode = Mode::Normal;
        for v in h.get_all(header::CACHE_CONTROL) {
            let Ok(v) = v.to_str() else { continue };
            for d in v.split(',').map(|d| d.trim().to_ascii_lowercase()) {
                match d.as_str() {
                    "no-store" => return Mode::Off,
                    "no-cache" => mode = Mode::Refresh,
                    _ => {}
                }
            }
        }
        mode
    }

    pub fn lookup(self) -> bool {
        self == Mode::Normal
    }

    pub fn store(self) -> bool {
        self != Mode::Off
    }
}

/// The top-level `model` of a provider response: the resolved version.
pub fn resolved_model(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct M<'a> {
        #[serde(borrow, default)]
        model: Option<Cow<'a, str>>,
    }
    serde_json::from_slice::<M<'_>>(body)
        .ok()?
        .model
        .map(Cow::into_owned)
        .filter(|m| !m.is_empty())
}

/// Last resolved model version per s1 model id.
pub struct Versions {
    path: Option<PathBuf>,
    map: Mutex<BTreeMap<String, String>>,
}

impl Versions {
    /// In memory only (tests, or no state dir).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            map: Mutex::new(BTreeMap::new()),
        }
    }

    /// Load `path` if it exists; changes are written back to it.
    pub fn load(path: &Path) -> Self {
        let map = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: Some(path.to_path_buf()),
            map: Mutex::new(map),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The version to use for `model_id`: the last one seen, else the id.
    pub fn current(&self, model_id: &str) -> String {
        self.map()
            .get(model_id)
            .cloned()
            .unwrap_or_else(|| model_id.to_string())
    }

    pub fn get(&self, model_id: &str) -> Option<String> {
        self.map().get(model_id).cloned()
    }

    /// Record a version seen in a response; persist when it changed.
    pub fn record(&self, model_id: &str, version: &str) {
        let mut map = self.map();
        if map.get(model_id).map(String::as_str) == Some(version) {
            return;
        }
        let old = map.insert(model_id.to_string(), version.to_string());
        tracing::info!(
            event = "model_version",
            model = %model_id,
            version = %version,
            previous = old.as_deref().unwrap_or(""),
            "resolved model version"
        );
        if let Some(path) = &self.path {
            let text = serde_json::to_vec_pretty(&*map).expect("versions serialize");
            if let Err(e) = write_atomic(path, &text) {
                tracing::warn!(error = %e, path = %path.display(), "cannot save model versions");
            }
        }
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.map().clone()
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_selects_the_mode() {
        let mut h = HeaderMap::new();
        assert_eq!(Mode::from_headers(&h), Mode::Normal);
        h.insert(
            header::CACHE_CONTROL,
            "max-age=0, No-Cache".parse().unwrap(),
        );
        assert_eq!(Mode::from_headers(&h), Mode::Refresh);
        h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        assert_eq!(Mode::from_headers(&h), Mode::Off);
        assert!(!Mode::Off.store() && !Mode::Refresh.lookup() && Mode::Refresh.store());
    }

    #[test]
    fn resolved_model_reads_the_top_level_model() {
        assert_eq!(
            resolved_model(br#"{"answers":{},"model":"jev-1.13.0"}"#).as_deref(),
            Some("jev-1.13.0")
        );
        assert_eq!(resolved_model(br#"{"answers":{}}"#), None);
        assert_eq!(resolved_model(b"nope"), None);
    }

    #[test]
    fn versions_persist_and_default_to_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/versions.json");
        let v = Versions::load(&path);
        assert_eq!(v.current("typesafe/jev-latest"), "typesafe/jev-latest");
        v.record("typesafe/jev-latest", "jev-1.13.0");
        assert_eq!(v.current("typesafe/jev-latest"), "jev-1.13.0");
        let again = Versions::load(&path);
        assert_eq!(
            again.get("typesafe/jev-latest").as_deref(),
            Some("jev-1.13.0")
        );
        again.record("typesafe/jev-latest", "jev-1.14.0");
        assert_eq!(
            Versions::load(&path).current("typesafe/jev-latest"),
            "jev-1.14.0"
        );
    }
}
