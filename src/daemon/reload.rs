//! Hot reload of the `models.toml` overlay. The daemon keeps one
//! [`AppState`] snapshot (catalog, backends, protection); each request
//! takes the current one when it starts and keeps it to the end. A reload
//! builds and checks a new snapshot first, then swaps it in; a bad file
//! changes nothing and its error shows in the log, the `/reload` answer,
//! and `/status`.
//!
//! Triggers: a change to the file (polled; editors that replace the file
//! and deleting it count), SIGHUP, and `POST /reload` on the Unix socket
//! (`s1 util daemon reload`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::AppState;
use crate::catalog::Catalog;
use crate::images::Origin;

/// How often the watcher looks at `models.toml`. A change is loaded once
/// two looks in a row see the same file, so a save in progress is not read.
pub const POLL_EVERY: Duration = Duration::from_millis(500);

/// The router state: the current snapshot behind a lock, swapped on reload.
#[derive(Clone)]
pub struct Shared(Arc<Live>);

struct Live {
    /// The snapshot and its catalog status, swapped together, so `/status`
    /// never shows one generation's backends with another's catalog.
    current: RwLock<Generation>,
    /// One reload at a time.
    reloading: Mutex<()>,
}

struct Generation {
    state: AppState,
    status: CatalogStatus,
}

/// Handlers that take `State<AppState>` get the snapshot current when the
/// request starts.
impl FromRef<Shared> for AppState {
    fn from_ref(s: &Shared) -> AppState {
        s.snapshot()
    }
}

/// Catalog state in `/status`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogStatus {
    /// The overlay file; `null` for a fixed catalog.
    pub file: Option<PathBuf>,
    pub models: usize,
    /// Unix seconds of the load in use.
    pub loaded_at: u64,
    /// Reloads that changed the catalog since start.
    pub reloads: u64,
    /// Why the newest reload failed; `null` once a reload succeeds.
    pub last_error: Option<String>,
    pub last_error_at: Option<u64>,
}

/// Result of one reload.
#[derive(Debug, Clone, Serialize)]
pub struct ReloadOutcome {
    /// False if the file gives the catalog already in use.
    pub changed: bool,
    pub models: usize,
    /// `provider/model` ids that are new or gone.
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn model_ids(c: &Catalog) -> std::collections::BTreeSet<String> {
    c.models().map(|m| m.id()).collect()
}

impl Shared {
    pub fn new(state: AppState) -> Self {
        state
            .versions
            .set_targets(super::model_targets(&state.catalog));
        let status = CatalogStatus {
            file: state.models_file.clone(),
            models: state.catalog.models().count(),
            loaded_at: now(),
            reloads: 0,
            last_error: None,
            last_error_at: None,
        };
        Self(Arc::new(Live {
            current: RwLock::new(Generation { state, status }),
            reloading: Mutex::new(()),
        }))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Generation> {
        self.0.current.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Generation> {
        self.0.current.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The snapshot in use now.
    pub fn snapshot(&self) -> AppState {
        self.read().state.clone()
    }

    pub fn status(&self) -> CatalogStatus {
        self.read().status.clone()
    }

    /// The snapshot and its catalog status, from one generation.
    pub fn generation(&self) -> (AppState, CatalogStatus) {
        let g = self.read();
        (g.state.clone(), g.status.clone())
    }

    /// Load `models.toml` again and, if it is valid and different, swap the
    /// new catalog in. Errors leave the old catalog in use.
    pub fn reload(&self) -> Result<ReloadOutcome, String> {
        let _one = self.0.reloading.lock().unwrap_or_else(|e| e.into_inner());
        let old = self.snapshot();
        let Some(file) = old.models_file.clone() else {
            return Err(
                "this daemon has a fixed catalog; there is no models.toml to reload".into(),
            );
        };
        let catalog = match Catalog::load(&file) {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    event = "catalog_reload_failed",
                    file = %file.display(),
                    error = %msg,
                    "models.toml not reloaded; the previous catalog stays in use"
                );
                let st = &mut self.write().status;
                st.last_error = Some(msg.clone());
                st.last_error_at = Some(now());
                return Err(msg);
            }
        };
        let models = catalog.models().count();
        let changed = catalog != *old.catalog;
        let (before, after) = (model_ids(&old.catalog), model_ids(&catalog));
        let added: Vec<String> = after.difference(&before).cloned().collect();
        let removed: Vec<String> = before.difference(&after).cloned().collect();
        // Built outside the lock: requests keep reading the old snapshot.
        let next = changed.then(|| old.with_catalog(catalog));
        let mut g = self.write();
        if let Some(next) = next {
            g.state = next;
            g.status.models = models;
            g.status.loaded_at = now();
            g.status.reloads += 1;
            tracing::info!(
                event = "catalog_reload",
                file = %file.display(),
                models,
                added = ?added,
                removed = ?removed,
                "models.toml reloaded"
            );
        }
        g.status.last_error = None;
        g.status.last_error_at = None;
        drop(g);
        Ok(ReloadOutcome {
            changed,
            models,
            added,
            removed,
        })
    }
}

/// What identifies a version of the file: `None` when it is missing.
type Fingerprint = Option<(u64, u64, i64, i64, u64)>;

fn fingerprint(file: &Path) -> Fingerprint {
    use std::os::unix::fs::MetadataExt;
    // Follows symlinks: a symlinked models.toml reloads when its target
    // changes.
    std::fs::metadata(file)
        .ok()
        .map(|m| (m.dev(), m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

/// Poll `models.toml` every `models_poll` and reload when it changed and
/// then stayed the same for one more poll. `None` when there is nothing to
/// watch.
pub(crate) fn spawn_watcher(
    shared: Shared,
    mut stop: watch::Receiver<bool>,
) -> Option<JoinHandle<std::io::Result<()>>> {
    let st = shared.snapshot();
    let (file, every) = (st.models_file?, st.models_poll?);
    Some(tokio::spawn(async move {
        // Unknown at first: the file may have changed since the catalog
        // loaded, so the first stable look reloads (a no-op if it did not).
        let mut loaded: Option<Fingerprint> = None;
        let mut seen = fingerprint(&file);
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.wait_for(|s| *s) => return Ok(()),
                _ = tick.tick() => {}
            }
            let now = fingerprint(&file);
            if loaded != Some(now) && now == seen {
                // The outcome is logged and kept for /status.
                let s = shared.clone();
                let _ = tokio::task::spawn_blocking(move || s.reload()).await;
                loaded = Some(now);
            }
            seen = now;
        }
    }))
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/reload", post(reload))
}

/// `POST /reload`: only on the Unix socket.
async fn reload(
    State(shared): State<Shared>,
    origin: Option<Extension<Origin>>,
) -> (StatusCode, Json<Value>) {
    if !matches!(origin, Some(Extension(Origin::UnixSocket))) {
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": {"code": "forbidden", "message": "reload only on the Unix socket"}}),
            ),
        );
    }
    match tokio::task::spawn_blocking(move || shared.reload()).await {
        Ok(Ok(outcome)) => (
            StatusCode::OK,
            Json(serde_json::to_value(outcome).expect("outcome serializes")),
        ),
        Ok(Err(message)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": {"code": "invalid_catalog", "message": message}})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": "internal", "message": e.to_string()}})),
        ),
    }
}
