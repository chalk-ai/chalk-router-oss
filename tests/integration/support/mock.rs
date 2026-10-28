//! Shared plumbing for the ephemeral mock upstream servers integration tests
//! spin up: bind a loopback port, serve an [`axum::Router`], and return its
//! base URL. Every mock server used to hand-roll
//! `TcpListener::bind` + `local_addr` + `tokio::spawn(axum::serve(...))` +
//! URL formatting; `serve` is that boilerplate, factored once.
//!
//! `TcpListener::bind(..).await` only returns once the socket is bound and
//! listening, so a caller never needs to sleep after `serve` returns before
//! sending it a request — the accept queue holds any request that arrives
//! before the spawned task's first `poll`.

use axum::Router;
use tokio::net::TcpListener;

/// Bind an ephemeral loopback port, serve `app` on it in a background task,
/// and return its base URL (e.g. `http://127.0.0.1:54321`).
pub async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serve");
    });
    format!("http://{addr}")
}
