//! The daemon: one axum router served on a Unix socket (file permissions give
//! access control) and on 127.0.0.1 TCP (bearer token from `auth.json`).
//! Requests on the socket carry an [`Origin::UnixSocket`] extension; only
//! they may send image file paths.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::backend::{BackendContext, Registry};
use crate::cache::{Cache, Versions};
use crate::catalog::{Catalog, ModelRef};
use crate::config::Config;
use crate::images::Origin;
use crate::paths::Paths;
use crate::protect::Protection;
use crate::queue::{Bounds, Scheduler};
use crate::secrets::{ApiKey, KeyResolver, Secret};

mod auth;
mod cache;
mod decode;
pub(crate) mod ingress;
pub(crate) mod models;
pub(crate) mod raw;
mod routes;
mod status;

pub use raw::MAX_BODY;
pub use routes::router;

/// Shared state of all handlers.
#[derive(Clone)]
pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub backends: Registry,
    pub keys: KeyResolver,
    /// Limits, queues, and breakers per backend and key.
    pub protection: Arc<Protection>,
    /// Response cache; `None` when disabled.
    pub cache: Option<Arc<Cache>>,
    /// Last resolved model versions (part of the cache key).
    pub versions: Arc<Versions>,
    /// Which image URLs s1 fetches toward systemone (public HTTPS only by
    /// default).
    pub image_fetch: crate::translate::fetch::FetchPolicy,
}

impl AppState {
    /// Default queue bounds, no cache, versions kept in memory.
    pub fn new(catalog: Catalog, keys: KeyResolver) -> Self {
        let ctx = BackendContext::new(keys.clone());
        let backends = Registry::from_catalog(&catalog, &ctx);
        let protection = Arc::new(Protection::new(&catalog, Bounds::default()));
        Self {
            catalog: Arc::new(catalog),
            backends,
            keys,
            protection,
            cache: None,
            versions: Arc::new(Versions::in_memory()),
            image_fetch: crate::translate::fetch::FetchPolicy::PUBLIC_HTTPS,
        }
    }

    /// Replace the queue bounds (drops schedulers made so far).
    pub fn with_bounds(mut self, bounds: Bounds) -> Self {
        self.protection = Arc::new(Protection::new(&self.catalog, bounds));
        self
    }

    pub fn with_cache(mut self, cache: Arc<Cache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Replace the image URL fetch policy (tests serve images on loopback).
    pub fn with_image_fetch(mut self, policy: crate::translate::fetch::FetchPolicy) -> Self {
        self.image_fetch = policy;
        self
    }

    pub fn with_versions(mut self, versions: Versions) -> Self {
        self.versions = Arc::new(versions);
        self
    }

    /// State for the daemon: config sizes, the cache file, and the
    /// persisted model versions.
    pub fn from_config(
        catalog: Catalog,
        keys: KeyResolver,
        config: &Config,
        paths: &Paths,
    ) -> Result<Self, DaemonError> {
        let mut st = Self::new(catalog, keys)
            .with_bounds(config.queue)
            .with_versions(Versions::load(&paths.versions_file()));
        if config.cache.enabled {
            let cache = Cache::open(&paths.cache_file(), config.cache.max_bytes)
                .map_err(|e| DaemonError::Setup(e.to_string()))?;
            st = st.with_cache(Arc::new(cache));
        }
        Ok(st)
    }

    /// The `auth.json` entry of a provider.
    fn auth_entry<'a>(&'a self, provider: &'a str) -> &'a str {
        self.catalog
            .providers
            .get(provider)
            .and_then(|p| p.auth.as_deref())
            .unwrap_or(provider)
    }

    /// The scheduler of a provider and its resolved key, made on first use;
    /// `None` if no key resolves.
    pub(crate) fn guard(&self, provider: &str) -> Option<Arc<Scheduler>> {
        self.guard_with_key(provider).map(|(g, _)| g)
    }

    /// The scheduler of a provider and the key it belongs to, resolved once.
    /// Requests carry this key to the backend
    /// ([`BackendRequest::credential`](crate::backend::BackendRequest)), so
    /// a key that changes while they wait does not move them to another
    /// key's limits.
    pub(crate) fn guard_with_key(&self, provider: &str) -> Option<(Arc<Scheduler>, ApiKey)> {
        let key = self
            .keys
            .resolve(self.auth_entry(provider))
            .ok()
            .flatten()?;
        Some((self.protection.guard(provider, key.key.expose()), key))
    }

    /// The scheduler of a provider and its key, only if it already exists.
    pub(crate) fn existing_guard(&self, provider: &str) -> Option<Arc<Scheduler>> {
        let key = self
            .keys
            .resolve(self.auth_entry(provider))
            .ok()
            .flatten()?;
        self.protection.existing(provider, key.key.expose())
    }

    /// Ok if the model can take a request now: a backend, a usable key, and
    /// a closed breaker.
    pub(crate) fn available(&self, m: &ModelRef<'_>) -> Result<(), crate::error::S1Error> {
        self.backends.ready(&self.keys, m)?;
        match self.existing_guard(m.provider_name) {
            Some(g) => g.check_breaker(),
            None => Ok(()),
        }
    }

    /// True if a new request to the model would wait.
    pub(crate) fn throttled(&self, m: &ModelRef<'_>) -> bool {
        self.existing_guard(m.provider_name)
            .is_some_and(|g| g.throttled())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("s1 daemon is already running on {0}")]
    AlreadyRunning(PathBuf),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Setup(String),
}

/// Contents of `daemon.json` in the state dir.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonInfo {
    pub pid: u32,
    pub socket: PathBuf,
    pub tcp: SocketAddr,
}

/// What to serve and where.
pub struct ServerOptions {
    pub socket: PathBuf,
    pub tcp: SocketAddr,
    pub state: AppState,
    pub token: Secret,
}

/// A server running in this process.
pub struct RunningServer {
    pub socket: PathBuf,
    pub tcp: SocketAddr,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
    lifecycle: Lifecycle,
}

impl RunningServer {
    /// Stop accepting, finish open requests, remove the socket file (only
    /// the one this server made) and its `daemon.json`, then release the
    /// lifecycle lock.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
        self.lifecycle.release(&self.socket);
    }
}

/// Bind both listeners and serve the shared router.
pub async fn serve(opts: ServerOptions) -> Result<RunningServer, DaemonError> {
    serve_inner(opts, None).await
}

/// As [`serve`], and write `daemon.json` to `info_file` before the socket
/// appears, so a client that sees the socket also sees the metadata.
pub async fn serve_with_info(
    opts: ServerOptions,
    info_file: &Path,
) -> Result<RunningServer, DaemonError> {
    serve_inner(opts, Some(info_file.to_path_buf())).await
}

async fn serve_inner(
    opts: ServerOptions,
    info_file: Option<PathBuf>,
) -> Result<RunningServer, DaemonError> {
    let mut lifecycle = Lifecycle::acquire(&opts.socket)?;
    let tcp = TcpListener::bind(opts.tcp)
        .await
        .map_err(|e| DaemonError::Io(format!("cannot listen on {}: {e}", opts.tcp)))?;
    let tcp_addr = tcp
        .local_addr()
        .map_err(|e| DaemonError::Io(e.to_string()))?;
    if let Some(file) = info_file {
        let info = DaemonInfo {
            pid: std::process::id(),
            socket: opts.socket.clone(),
            tcp: tcp_addr,
        };
        write_info(&file, &info)?;
        lifecycle.info_file = Some(file);
    }
    let unix = lifecycle.publish(&opts.socket)?;

    let app: Router = router(opts.state);
    let tcp_app = auth::require_bearer(app.clone(), opts.token);
    // Only the socket may name local image files (see `crate::images`).
    let app = app.layer(Extension(Origin::UnixSocket));
    let (stop, rx) = watch::channel(false);

    let mut rx1 = rx.clone();
    let unix_task = tokio::spawn(async move {
        axum::serve(unix, app)
            .with_graceful_shutdown(async move {
                let _ = rx1.wait_for(|s| *s).await;
            })
            .await
    });
    let mut rx2 = rx;
    let tcp_task = tokio::spawn(async move {
        axum::serve(tcp, tcp_app)
            .with_graceful_shutdown(async move {
                let _ = rx2.wait_for(|s| *s).await;
            })
            .await
    });
    Ok(RunningServer {
        socket: opts.socket,
        tcp: tcp_addr,
        stop,
        tasks: vec![unix_task, tcp_task],
        lifecycle,
    })
}

/// One daemon's claim on a socket path: an exclusive `flock` on
/// `<socket>.lock`, held from the stale-socket probe until shutdown, and the
/// identity of the socket inode this daemon made.
struct Lifecycle {
    _lock: std::fs::File,
    /// (device, inode) of the socket this daemon bound.
    socket_id: Option<(u64, u64)>,
    /// `daemon.json` written by this daemon.
    info_file: Option<PathBuf>,
}

fn io(what: &str, path: &Path) -> impl FnOnce(std::io::Error) -> DaemonError {
    let path = path.display().to_string();
    let what = what.to_string();
    move |e| DaemonError::Io(format!("{what} {path}: {e}"))
}

fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

impl Lifecycle {
    /// Take the lock, then clear a stale socket. Another live daemon (or
    /// one still starting) holds the lock: `AlreadyRunning`.
    fn acquire(path: &Path) -> Result<Self, DaemonError> {
        use std::os::fd::AsRawFd;
        let setup = |e: crate::secrets::SecretsError| DaemonError::Setup(e.to_string());
        // The socket dir must be ours and private before anything in it is
        // trusted: an existing dir of this user is set back to 0700, one of
        // another user is refused.
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            crate::secrets::ensure_private_dir(dir).map_err(setup)?;
        }
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let lock = crate::secrets::open_lock_file(&lock_path).map_err(setup)?;
        // SAFETY: flock on a valid, owned file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(DaemonError::AlreadyRunning(path.to_path_buf()));
            }
            return Err(io("cannot lock", &lock_path)(e));
        }
        if crate::secrets::check_own_file(path).map_err(setup)? {
            // A daemon without the lock (an older build) may still serve it.
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(DaemonError::AlreadyRunning(path.to_path_buf()));
            }
            std::fs::remove_file(path).map_err(io("cannot remove stale", path))?;
        }
        Ok(Self {
            _lock: lock,
            socket_id: None,
            info_file: None,
        })
    }

    /// Bind the socket in a fresh 0700 directory next to `path`, make it
    /// 0600, and only then rename it into place: no other user can connect
    /// at any moment, whatever the umask.
    fn publish(&mut self, path: &Path) -> Result<UnixListener, DaemonError> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let tmp_dir = dir.join(format!(".s1-bind-{}", crate::secrets::random_hex(6)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&tmp_dir)
            .map_err(io("cannot create", &tmp_dir))?;
        let tmp_sock = tmp_dir.join("s");
        let result = (|| {
            let listener = std::os::unix::net::UnixListener::bind(&tmp_sock)
                .map_err(io("cannot listen on", path))?;
            std::fs::set_permissions(&tmp_sock, std::fs::Permissions::from_mode(0o600))
                .map_err(io("cannot chmod", &tmp_sock))?;
            std::fs::rename(&tmp_sock, path).map_err(io("cannot move socket to", path))?;
            listener
                .set_nonblocking(true)
                .map_err(io("cannot listen on", path))?;
            UnixListener::from_std(listener).map_err(io("cannot listen on", path))
        })();
        let _ = std::fs::remove_file(&tmp_sock);
        let _ = std::fs::remove_dir(&tmp_dir);
        let listener = result?;
        self.socket_id = file_id(path);
        Ok(listener)
    }

    /// Remove the socket only if it is still the inode this daemon made, and
    /// `daemon.json` only if it still names this process. The lock goes when
    /// `self` drops, after both.
    fn release(self, path: &Path) {
        if self.socket_id.is_some() && file_id(path) == self.socket_id {
            let _ = std::fs::remove_file(path);
        }
        if let Some(file) = &self.info_file
            && read_info_file(file).is_some_and(|i| i.pid == std::process::id())
        {
            let _ = std::fs::remove_file(file);
        }
    }
}

fn write_info(file: &Path, info: &DaemonInfo) -> Result<(), DaemonError> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(io("cannot create", dir))?;
    }
    std::fs::write(
        file,
        serde_json::to_vec_pretty(info).expect("serialize daemon info"),
    )
    .map_err(io("cannot write", file))
}

/// Run the daemon in the foreground until SIGTERM or SIGINT.
pub async fn run(paths: Paths) -> Result<(), DaemonError> {
    let config =
        Config::load(&paths.config_file()).map_err(|e| DaemonError::Setup(e.to_string()))?;
    let tcp = config.tcp_addr().map_err(DaemonError::Setup)?;
    // Logs, daemon.json, and (without XDG_RUNTIME_DIR) the socket live here.
    crate::secrets::ensure_private_dir(&paths.state_dir)
        .map_err(|e| DaemonError::Setup(e.to_string()))?;
    let _guard =
        crate::logging::init(&paths.log_dir(), &config.log_level).map_err(DaemonError::Setup)?;

    let keys = KeyResolver::from_process(&paths);
    let token = keys
        .ensure_local_token()
        .map_err(|e| DaemonError::Setup(e.to_string()))?;
    let catalog =
        Catalog::load(&paths.models_file()).map_err(|e| DaemonError::Setup(e.to_string()))?;
    let models = catalog.models().count();
    let state = AppState::from_config(catalog, keys, &config, &paths)?;
    let server = serve_with_info(
        ServerOptions {
            socket: paths.socket.clone(),
            tcp,
            state,
            token,
        },
        &paths.daemon_file(),
    )
    .await?;

    tracing::info!(
        event = "daemon_start",
        pid = std::process::id(),
        socket = %server.socket.display(),
        tcp = %server.tcp,
        models,
        "s1 daemon started"
    );

    wait_for_signal().await;
    tracing::info!(event = "daemon_stop", "s1 daemon stopping");
    server.shutdown().await;
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Read `daemon.json`, if present.
pub fn read_info(paths: &Paths) -> Option<DaemonInfo> {
    read_info_file(&paths.daemon_file())
}

fn read_info_file(file: &Path) -> Option<DaemonInfo> {
    let text = std::fs::read(file).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Make a request id: `req_` + 16 hex digits.
pub(crate) fn request_id() -> String {
    format!("req_{}", crate::secrets::random_hex(8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;

    fn fifo_lock(with_reader: bool) {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("run");
        std::fs::create_dir(&sock_dir).unwrap();
        let socket = sock_dir.join("s1.sock");
        let lock_path = sock_dir.join("s1.sock.lock");
        let c = std::ffi::CString::new(lock_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo(3) with a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let _reader = with_reader.then(|| {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&lock_path)
                .unwrap()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Lifecycle::acquire(&socket).map(|_| ()));
        });
        let res = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Lifecycle::acquire hung on a FIFO lock path");
        assert!(
            matches!(res, Err(DaemonError::Setup(_)) | Err(DaemonError::Io(_))),
            "{res:?}"
        );
    }

    #[test]
    fn fifo_socket_lock_without_reader_is_an_error_not_a_hang() {
        fifo_lock(false);
    }

    #[test]
    fn fifo_socket_lock_with_reader_is_refused() {
        fifo_lock(true);
    }
}
