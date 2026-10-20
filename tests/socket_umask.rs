//! The Unix socket must never be reachable by others, not even for a moment
//! after `bind` under a permissive umask. Own test binary: it changes the
//! process umask.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use s1::catalog::Catalog;
use s1::daemon::{AppState, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_is_private_from_creation_under_permissive_umask() {
    // SAFETY: umask(2) has no memory-safety preconditions.
    unsafe { libc::umask(0) };
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    // An existing socket dir that others can access: the daemon must make it
    // private before it binds there.
    std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        paths.socket.parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let keys = KeyResolver::new(paths.auth_file(), |_| None);

    let stop = Arc::new(AtomicBool::new(false));
    let socket = paths.socket.clone();
    let stop2 = stop.clone();
    let watcher = std::thread::spawn(move || {
        let mut bad = Vec::new();
        let mut observed = 0usize;
        while !stop2.load(Ordering::Relaxed) {
            if let Ok(m) = std::fs::symlink_metadata(&socket) {
                observed += 1;
                let mode = m.permissions().mode() & 0o777;
                if mode != 0o600 {
                    bad.push(mode);
                }
            }
        }
        (observed, bad)
    });

    for _ in 0..200 {
        let server = serve(ServerOptions {
            socket: paths.socket.clone(),
            tcp: "127.0.0.1:0".parse().unwrap(),
            state: AppState::new(Catalog::builtin(), keys.clone()),
            token: Secret::new("t"),
        })
        .await
        .unwrap();
        server.shutdown().await;
    }
    stop.store(true, Ordering::Relaxed);
    let (observed, bad) = watcher.join().unwrap();
    // The watcher must have seen published sockets, or it proved nothing.
    assert!(observed > 0, "the watcher never saw a published socket");
    let dir_mode = std::fs::metadata(paths.socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "socket dir mode {dir_mode:o}");
    assert!(
        bad.is_empty(),
        "socket was visible with modes {:?}",
        bad.iter()
            .take(5)
            .map(|m| format!("{m:o}"))
            .collect::<Vec<_>>()
    );
}
