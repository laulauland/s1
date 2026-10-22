//! The accept loop. `axum::serve` drains open requests at shutdown, but
//! hyper closes a connection that is idle at that moment, and a connection
//! whose first request has not arrived yet counts as idle. Under socket
//! activation that loses a request: the client connected (to this daemon,
//! not to systemd's backlog) just before the stop. Here such a connection
//! may send its first request within [`FIRST_REQUEST_GRACE`] after the stop;
//! it is answered, then closed.
//!
//! A kept-alive connection that already had a request and is idle at the
//! stop is closed at once (HTTP keep-alive rules: the client opens a new
//! connection, which systemd queues for the next daemon).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::serve::Listener;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;

/// How long a connection accepted before the stop may take to send its
/// first request.
pub const FIRST_REQUEST_GRACE: Duration = Duration::from_secs(5);

/// Serve `app` on `listener` until `stop` is true, then stop accepting and
/// wait for every connection to finish.
pub async fn serve_until<L: Listener>(
    mut listener: L,
    app: Router,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut conns = JoinSet::new();
    loop {
        let io = tokio::select! {
            (io, _) = listener.accept() => io,
            _ = stop.wait_for(|s| *s) => break,
        };
        conns.spawn(connection(io, app.clone(), stop.clone()));
        while conns.try_join_next().is_some() {}
    }
    drop(listener);
    while conns.join_next().await.is_some() {}
    Ok(())
}

async fn connection<I>(io: I, app: Router, mut stop: watch::Receiver<bool>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let had_request = Arc::new(AtomicBool::new(false));
    let first = Arc::new(Notify::new());
    let service = {
        let (had_request, first) = (had_request.clone(), first.clone());
        hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
            if !had_request.swap(true, Ordering::SeqCst) {
                first.notify_one();
            }
            tower::ServiceExt::oneshot(app.clone(), req)
        })
    };
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        _ = conn.as_mut() => return,
        _ = stop.wait_for(|s| *s) => {}
    }
    if !had_request.load(Ordering::SeqCst) {
        tokio::select! {
            _ = conn.as_mut() => return,
            _ = first.notified() => {}
            _ = tokio::time::sleep(FIRST_REQUEST_GRACE) => {}
        }
    }
    // A request in flight finishes (with `Connection: close`); an idle
    // connection closes now.
    conn.as_mut().graceful_shutdown();
    let _ = conn.await;
}
