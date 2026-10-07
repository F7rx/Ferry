//! Helpers for engine-to-engine tests on loopback.
#![allow(dead_code)]

use ferry_core::events::EngineEvent;
use ferry_core::model::*;
use ferry_core::{Engine, EngineConfig, Settings, Target};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub struct Peer {
    pub engine: Arc<Engine>,
    pub save_dir: tempfile::TempDir,
    pub events: broadcast::Receiver<EngineEvent>,
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}

pub async fn peer(alias: &str) -> Peer {
    peer_with(alias, |_| {}).await
}

pub async fn peer_with(alias: &str, tweak: impl FnOnce(&mut Settings)) -> Peer {
    init_tracing();
    let save_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings {
        alias: alias.to_string(),
        port: 0, // any free port
        save_dir: Some(save_dir.path().to_path_buf()),
        ..Settings::default()
    };
    tweak(&mut settings);
    let engine = Engine::start(EngineConfig::ephemeral(settings)).await.unwrap();
    let events = engine.subscribe();
    Peer { engine, save_dir, events }
}

impl Peer {
    pub fn target(&self) -> Target {
        Target::Address {
            host: "127.0.0.1".into(),
            port: self.engine.port(),
            protocol: Protocol::Https,
            fingerprint: Some(self.engine.fingerprint()),
        }
    }

    /// Answers every incoming request with `decision`.
    pub fn auto_respond(&self, decision: Decision) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        let mut events = engine.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(EngineEvent::IncomingRequest { request }) if request.text.is_none() => {
                        engine.respond(&request.id, decision.clone());
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        })
    }

    /// Waits until transfer `id` reaches a final state.
    pub async fn wait_final(&mut self, id: &str, timeout: Duration) -> TransferSummary {
        self.wait_transfer(timeout, |t| t.id == id && t.state.is_final()).await
    }

    pub async fn wait_transfer(&mut self, timeout: Duration, pred: impl Fn(&TransferSummary) -> bool) -> TransferSummary {
        // Already there?
        if let Some(t) = self.engine.transfers().into_iter().find(|t| pred(t)) {
            return t;
        }
        tokio::time::timeout(timeout, async {
            loop {
                match self.events.recv().await {
                    Ok(EngineEvent::TransferUpdated { transfer }) if pred(&transfer) => return transfer,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(t) = self.engine.transfers().into_iter().find(|t| pred(t)) {
                            return t;
                        }
                    }
                    Err(e) => panic!("event stream closed: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            let seen: Vec<String> = self
                .engine
                .transfers()
                .iter()
                .map(|t| {
                    format!("{:?} {:?} {}/{} {:?}", t.direction, t.state, t.bytes_done, t.total_bytes, t.error.as_ref().map(|e| &e.code))
                })
                .collect();
            panic!("timed out waiting for transfer state; transfers now: {seen:?}")
        })
    }

    pub async fn wait_event(&mut self, timeout: Duration, pred: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
        tokio::time::timeout(timeout, async {
            loop {
                match self.events.recv().await {
                    Ok(e) if pred(&e) => return e,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("event stream closed: {e}"),
                }
            }
        })
        .await
        .expect("timed out waiting for event")
    }

    /// The receive-side transfer (there is one per session).
    pub async fn wait_received(&mut self, timeout: Duration) -> TransferSummary {
        self.wait_transfer(timeout, |t| t.direction == Direction::Receive && t.state.is_final()).await
    }

    pub fn saved(&self, rel: &str) -> PathBuf {
        self.save_dir.path().join(rel)
    }
}

pub fn write_file(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Deterministic pseudo-random content (incompressible enough for tests).
pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

pub fn files_in(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

pub const T: Duration = Duration::from_secs(30);

pub mod proxy;
pub mod raw;
