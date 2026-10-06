//! A TCP proxy for fault injection: throttles bandwidth, cuts connections
//! after N bytes (Wi-Fi drop), refuses connections while "down", and can be
//! re-pointed at a different backend (receiver restarted on a new port).
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

pub struct Proxy {
    pub port: u16,
    state: Arc<State>,
}

struct State {
    backend: Mutex<SocketAddr>,
    down: AtomicBool,
    /// Total client→server bytes forwarded.
    upstream_bytes: AtomicU64,
    /// Cut everything once upstream_bytes passes this (0 = never).
    cut_at: AtomicU64,
    /// Bytes per second per direction (0 = unlimited).
    rate: AtomicU64,
    conns: Mutex<Vec<AbortHandle>>,
}

impl Proxy {
    pub async fn start(backend: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State {
            backend: Mutex::new(backend),
            down: AtomicBool::new(false),
            upstream_bytes: AtomicU64::new(0),
            cut_at: AtomicU64::new(0),
            rate: AtomicU64::new(0),
            conns: Mutex::new(Vec::new()),
        });
        let s = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else { return };
                if s.down.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let backend = *s.backend.lock().unwrap();
                let s2 = s.clone();
                let task = tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(backend).await else { return };
                    let _ = client.set_nodelay(true);
                    let _ = server.set_nodelay(true);
                    let (cr, cw) = client.into_split();
                    let (sr, sw) = server.into_split();
                    let up = pump(cr, sw, s2.clone(), true);
                    let down = pump(sr, cw, s2.clone(), false);
                    tokio::select! { _ = up => {}, _ = down => {} }
                });
                s.conns.lock().unwrap().push(task.abort_handle());
            }
        });
        Proxy { port, state }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.state.rate.store(bytes_per_sec, Ordering::SeqCst);
    }

    /// Simulates the network dropping once this many upstream bytes passed.
    pub fn cut_after(&self, bytes: u64) {
        self.state.cut_at.store(self.state.upstream_bytes.load(Ordering::SeqCst) + bytes, Ordering::SeqCst);
    }

    /// Kills all connections and refuses new ones until [`Proxy::up`].
    pub fn down(&self) {
        self.state.down.store(true, Ordering::SeqCst);
        for c in self.state.conns.lock().unwrap().drain(..) {
            c.abort();
        }
    }

    pub fn up(&self) {
        self.state.cut_at.store(0, Ordering::SeqCst);
        self.state.down.store(false, Ordering::SeqCst);
    }

    pub fn is_down(&self) -> bool {
        self.state.down.load(Ordering::SeqCst)
    }

    pub fn set_backend(&self, backend: SocketAddr) {
        *self.state.backend.lock().unwrap() = backend;
    }

    pub fn upstream_bytes(&self) -> u64 {
        self.state.upstream_bytes.load(Ordering::SeqCst)
    }
}

async fn pump(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, state: Arc<State>, upstream: bool) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if upstream {
            let total = state.upstream_bytes.fetch_add(n as u64, Ordering::SeqCst) + n as u64;
            let cut_at = state.cut_at.load(Ordering::SeqCst);
            if cut_at > 0 && total >= cut_at {
                // The "Wi-Fi" goes away: drop everything, refuse reconnects.
                state.down.store(true, Ordering::SeqCst);
                for c in state.conns.lock().unwrap().drain(..) {
                    c.abort();
                }
                return;
            }
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
        let rate = state.rate.load(Ordering::SeqCst);
        if rate > 0 {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / rate as f64)).await;
        }
    }
}
