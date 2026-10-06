//! `ferry-signal`: the WebRTC signaling server for Ferry.
//!
//! The WebSocket endpoint `GET /v1/ws?d=<base64url(JSON client info)>` is
//! wire-compatible with LocalSend's signaling server: legacy clients see the
//! exact upstream message set (`HELLO`, `JOIN`, `UPDATE`, `LEFT`, `OFFER`,
//! `ANSWER`, `ERROR`). Clients that send an `ext` object additionally get
//! rooms, trickle ICE, `CANCEL`, `PING`/`PONG` and TURN credentials
//! (`GET /v1/turn`). See `README.md` and `docs/05-protocol.md` §5.1.
//!
//! Design notes:
//! - All peer topology (connections, IP groups, rooms) lives in one
//!   [`std::sync::RwLock`] that is only ever held for short, non-async critical
//!   sections. Fan-out uses `try_send` on bounded per-connection queues, so a
//!   slow peer never blocks anybody: when its queue overflows it is
//!   disconnected (close code [`close::SLOW_CONSUMER`]).
//! - Every inbound frame (valid or not) is metered by a per-connection token
//!   bucket; connection attempts and concurrent connections are limited per
//!   IP group (IPv4 address / IPv6 /64).
//! - `X-Forwarded-For` is honoured only from configured trusted proxies.
//! - The server only relays signaling: it never sees, stores or forwards
//!   file contents, and keeps everything (peers, rooms, rate-limit buckets)
//!   in memory only.

mod config;
mod conn;
mod http;
mod hub;
mod limit;
mod net;
mod protocol;
mod turn;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt;
use tokio::net::TcpListener;

pub use config::{Config, ConfigError, Limits, TurnConfig};

/// WebSocket close codes sent by the server.
pub mod close {
    /// The server is shutting down.
    pub const GOING_AWAY: u16 = 1001;
    /// Too many invalid or oversized messages.
    pub const POLICY_VIOLATION: u16 = 1008;
    /// A frame exceeded the maximum WebSocket message size.
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    /// No frame (not even a pong) was received within the idle timeout.
    pub const IDLE_TIMEOUT: u16 = 4000;
    /// The client did not read its messages fast enough.
    pub const SLOW_CONSUMER: u16 = 4008;
    /// The client exceeded the per-connection frame rate limit.
    pub const RATE_LIMITED: u16 = 4029;

    pub(crate) fn reason(code: u16) -> &'static str {
        match code {
            GOING_AWAY => "server shutting down",
            POLICY_VIOLATION => "too many invalid messages",
            MESSAGE_TOO_BIG => "frame too large",
            IDLE_TIMEOUT => "idle timeout",
            SLOW_CONSUMER => "too slow",
            RATE_LIMITED => "rate limited",
            _ => "",
        }
    }
}

/// How often stale per-IP-group rate-limit state is purged.
const PURGE_INTERVAL: Duration = Duration::from_secs(60);
/// Time WebSocket connections get to finish their close handshake on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Serves the signaling server on `listener` until the process exits.
pub async fn serve(listener: TcpListener, config: Config) -> std::io::Result<()> {
    serve_with_shutdown(listener, config, std::future::pending()).await
}

/// Serves the signaling server on `listener` until `shutdown` resolves.
///
/// On shutdown the listener is closed, every WebSocket connection is closed
/// with [`close::GOING_AWAY`] (queued messages are flushed first; clients
/// reconnect) and the future resolves once they are gone, or after a few
/// seconds at most.
pub async fn serve_with_shutdown<F>(listener: TcpListener, config: Config, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let state = Arc::new(http::AppState::new(config));

    let purge = {
        let limiter = Arc::downgrade(&state.limiter);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PURGE_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                match limiter.upgrade() {
                    Some(limiter) => limiter.purge(),
                    None => return,
                }
            }
        })
    };

    let listener = listener.tap_io(|tcp| {
        // Signaling messages are small and latency-sensitive.
        let _ = tcp.set_nodelay(true);
    });
    let app = http::router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>();
    let signal = Arc::clone(&state);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            signal.shutdown.send_replace(true);
        })
        .await;
    // `axum::serve` stops tracking a connection once it is upgraded to a
    // WebSocket: close those here (no-op if the signal already did).
    state.shutdown.send_replace(true);
    let open = state.shutdown.receiver_count();
    if open > 0 {
        tracing::info!(open, "waiting for WebSocket connections to close");
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, state.shutdown.closed()).await.is_err() {
        tracing::warn!(open = state.shutdown.receiver_count(), "WebSocket connections did not close in time");
    }
    purge.abort();
    result
}
