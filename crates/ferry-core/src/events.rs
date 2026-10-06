//! The engine's outbound event stream. Shells subscribe once and render from
//! it; anything missed (a lagging subscriber) can be re-read via snapshots.

use crate::model::{DeviceSummary, HistoryEntry, IncomingRequest, LocalDevice, RoomInfo, SignalingStatus, TransferFile, TransferSummary};
use serde::Serialize;
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
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(2048);
        Self { tx }
    }

    pub fn emit(&self, event: EngineEvent) {
        // No subscribers is fine (e.g. during tests); lagging ones resync.
        let _ = self.tx.send(event);
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
