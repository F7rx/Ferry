//! The engine's outbound event stream. Shells subscribe once and render from
//! it; anything missed (a lagging subscriber) can be re-read via snapshots.

use crate::model::{DeviceSummary, HistoryEntry, IncomingRequest, LocalDevice, RoomInfo, SignalingStatus, TransferFile, TransferSummary};
use crate::pairing::PairingRequest;
use crate::util::now_ms;
use indexmap::IndexMap;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EngineEvent {
    /// The local device's announced identity or addresses changed.
    LocalDeviceChanged {
        device: LocalDevice,
    },
    /// A device appeared or changed (online state, alias, trust...).
    DeviceUpdated {
        device: DeviceSummary,
    },
    /// A device was forgotten (expired, not trusted).
    DeviceRemoved {
        id: String,
    },
    /// A peer wants to send something; answer with `Engine::respond`.
    IncomingRequest {
        request: IncomingRequest,
    },
    /// A pending request disappeared without an answer from us.
    IncomingRequestClosed {
        id: String,
        reason: String,
    },
    /// Progress / state of a transfer (coalesced to ≤ 10 Hz per transfer).
    TransferUpdated {
        transfer: TransferSummary,
    },
    /// Per-file changes (state transitions; progress of the active files).
    TransferFilesUpdated {
        id: String,
        files: Vec<TransferFile>,
    },
    TransferRemoved {
        id: String,
    },
    HistoryAdded {
        entry: HistoryEntry,
    },
    /// Receiving is on/off or the server failed (port in use...).
    ServerStatus {
        running: bool,
        port: u16,
        error: Option<String>,
    },
    /// A browser link was opened or its activity changed.
    BrowserShareUpdated {
        share: crate::browser::BrowserShareInfo,
    },
    BrowserShareRemoved {
        id: String,
    },
    /// Something the user should know that isn't tied to a transfer.
    Notice {
        level: NoticeLevel,
        code: String,
        message: String,
    },
    /// A pairing QR code we showed was used (`device`) or expired/cancelled.
    PairingOfferClosed {
        id: String,
        device: Option<DeviceSummary>,
    },
    /// Another device asks to pair by code comparison; the user decides.
    PairingRequest {
        request: crate::pairing::PairingRequest,
    },
    /// That prompt was answered, timed out or withdrawn.
    PairingRequestClosed {
        id: String,
    },
    /// A code-comparison pairing has an outcome: ours (`id` from
    /// `start_code_pairing`), or one we confirmed (`id` of the request).
    PairingFinished {
        id: String,
        outcome: crate::pairing::PairingOutcome,
        device: Option<DeviceSummary>,
        error: Option<crate::error::ErrorInfo>,
    },
    /// A private link room was created/joined or its peer count changed.
    RoomUpdated {
        room: RoomInfo,
    },
    /// A private link room was left.
    RoomRemoved {
        id: String,
    },
    /// The signaling connection (WebRTC) changed state.
    SignalingStatus {
        status: SignalingStatus,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EngineEvent>,
    open: Arc<Mutex<OpenState>>,
}

/// What the event stream says is open right now: the prompts waiting for the
/// user and the last server status. Updated inside `emit`, so a snapshot
/// restores exactly what a lagging subscriber missed.
#[derive(Default)]
struct OpenState {
    requests: IndexMap<String, IncomingRequest>,
    pairing: IndexMap<String, PairingRequest>,
    server: Option<ServerStatus>,
}

/// The receiving server's state, as last reported by `ServerStatus`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub running: bool,
    pub port: u16,
    pub error: Option<String>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(2048);
        Self { tx, open: Arc::default() }
    }

    pub fn emit(&self, event: EngineEvent) {
        self.track(&event);
        // No subscribers is fine (e.g. during tests); lagging ones resync.
        let _ = self.tx.send(event);
    }

    fn track(&self, event: &EngineEvent) {
        let mut open = self.open.lock().unwrap();
        match event {
            // A message (text) needs no answer, so it is never pending.
            EngineEvent::IncomingRequest { request } if request.text.is_none() => {
                open.requests.insert(request.id.clone(), request.clone());
            }
            EngineEvent::IncomingRequestClosed { id, .. } => {
                open.requests.shift_remove(id);
            }
            EngineEvent::PairingRequest { request } => {
                open.pairing.insert(request.id.clone(), request.clone());
            }
            EngineEvent::PairingRequestClosed { id } => {
                open.pairing.shift_remove(id);
            }
            EngineEvent::ServerStatus { running, port, error } => {
                open.server = Some(ServerStatus { running: *running, port: *port, error: error.clone() });
            }
            _ => {}
        }
    }

    /// Incoming requests still waiting for a decision, oldest first.
    pub fn pending_requests(&self) -> Vec<IncomingRequest> {
        let now = now_ms();
        self.open.lock().unwrap().requests.values().filter(|r| r.expires_at_ms > now).cloned().collect()
    }

    /// Pairing prompts still waiting for an answer, oldest first.
    pub fn pairing_requests(&self) -> Vec<PairingRequest> {
        let now = now_ms();
        self.open.lock().unwrap().pairing.values().filter(|r| r.expires_at_ms > now).cloned().collect()
    }

    /// The last reported server status (None until the server first reports).
    pub fn server_status(&self) -> Option<ServerStatus> {
        self.open.lock().unwrap().server.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceKind, PeerRef};

    fn peer() -> PeerRef {
        PeerRef { id: "p".into(), alias: "Phone".into(), device_kind: DeviceKind::Mobile, device_model: None, verified: true }
    }

    fn request(id: &str, text: Option<&str>, expires_at_ms: u64) -> EngineEvent {
        EngineEvent::IncomingRequest {
            request: IncomingRequest {
                id: id.into(),
                peer: peer(),
                files: Vec::new(),
                total_bytes: 0,
                text: text.map(str::to_string),
                received_at_ms: now_ms(),
                trusted: false,
                default_save_dir: String::new(),
                expires_at_ms,
            },
        }
    }

    fn pairing(id: &str, expires_at_ms: u64) -> EngineEvent {
        EngineEvent::PairingRequest { request: PairingRequest { id: id.into(), peer: peer(), code: "123 456".into(), expires_at_ms } }
    }

    #[test]
    fn tracks_open_prompts_until_closed() {
        let bus = EventBus::new();
        let later = now_ms() + 60_000;
        bus.emit(request("a", None, later));
        bus.emit(request("b", None, later));
        // Duplicates replace, they don't add.
        bus.emit(request("a", None, later));
        bus.emit(pairing("x", later));
        let ids: Vec<String> = bus.pending_requests().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(bus.pairing_requests().len(), 1);

        bus.emit(EngineEvent::IncomingRequestClosed { id: "a".into(), reason: "answered".into() });
        bus.emit(EngineEvent::PairingRequestClosed { id: "x".into() });
        let ids: Vec<String> = bus.pending_requests().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["b"]);
        assert!(bus.pairing_requests().is_empty());
    }

    #[test]
    fn messages_and_expired_prompts_are_not_pending() {
        let bus = EventBus::new();
        bus.emit(request("msg", Some("hello"), now_ms()));
        bus.emit(request("old", None, now_ms().saturating_sub(1)));
        bus.emit(pairing("old", now_ms().saturating_sub(1)));
        assert!(bus.pending_requests().is_empty());
        assert!(bus.pairing_requests().is_empty());
    }

    #[test]
    fn remembers_the_last_server_status() {
        let bus = EventBus::new();
        assert_eq!(bus.server_status(), None);
        bus.emit(EngineEvent::ServerStatus { running: true, port: 53317, error: None });
        bus.emit(EngineEvent::ServerStatus { running: false, port: 53317, error: Some("port in use".into()) });
        assert_eq!(bus.server_status(), Some(ServerStatus { running: false, port: 53317, error: Some("port in use".into()) }));
    }
}
