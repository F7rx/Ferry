//! Types shared with the UI (serialized as camelCase JSON). These are the
//! contract between ferry-core and every shell (Tauri, CLI, tests).

use crate::error::ErrorInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// LocalSend device categories (protocol §7.1). Unknown values map to `Desktop`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Mobile,
    #[default]
    Desktop,
    Web,
    Headless,
    Server,
}

impl From<localsend::model::discovery::DeviceType> for DeviceKind {
    fn from(value: localsend::model::discovery::DeviceType) -> Self {
        use localsend::model::discovery::DeviceType as T;
        match value {
            T::Mobile => DeviceKind::Mobile,
            T::Desktop => DeviceKind::Desktop,
            T::Web => DeviceKind::Web,
            T::Headless => DeviceKind::Headless,
            T::Server => DeviceKind::Server,
        }
    }
}

impl From<DeviceKind> for localsend::model::discovery::DeviceType {
    fn from(value: DeviceKind) -> Self {
        use localsend::model::discovery::DeviceType as T;
        match value {
            DeviceKind::Mobile => T::Mobile,
            DeviceKind::Desktop => T::Desktop,
            DeviceKind::Web => T::Web,
            DeviceKind::Headless => T::Headless,
            DeviceKind::Server => T::Server,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Https,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Https => "https",
        }
    }
}

impl From<localsend::model::discovery::ProtocolType> for Protocol {
    fn from(value: localsend::model::discovery::ProtocolType) -> Self {
        match value {
            localsend::model::discovery::ProtocolType::Http => Protocol::Http,
            localsend::model::discovery::ProtocolType::Https => Protocol::Https,
        }
    }
}

impl From<Protocol> for localsend::model::discovery::ProtocolType {
    fn from(value: Protocol) -> Self {
        match value {
            Protocol::Http => localsend::model::discovery::ProtocolType::Http,
            Protocol::Https => localsend::model::discovery::ProtocolType::Https,
        }
    }
}

/// How sure we are about who a peer is (see docs/04-threat-model.md).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PeerIdentity {
    /// Proven by mutual TLS: the fingerprint of the certificate the peer used.
    Verified { fingerprint: String },
    /// HTTPS without a client certificate.
    Unverified,
    /// LocalSend's unencrypted mode; nothing is proven.
    PlainHttp,
}

impl PeerIdentity {
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            PeerIdentity::Verified { fingerprint } => Some(fingerprint),
            _ => None,
        }
    }

    pub fn is_verified(&self) -> bool {
        matches!(self, PeerIdentity::Verified { .. })
    }
}

/// This device, as shown in the UI and announced to peers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDevice {
    pub alias: String,
    pub fingerprint: String,
    pub device_kind: DeviceKind,
    pub device_model: Option<String>,
    pub port: u16,
    pub protocol: Protocol,
    /// Addresses we listen on, best first (for QR codes and manual entry).
    pub addresses: Vec<String>,
    /// Short code users can compare (first 4 groups of the fingerprint).
    pub short_id: String,
    pub app_version: String,
}

/// A peer device as the UI sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSummary {
    /// Stable id: the verified certificate fingerprint, or `http:<ip>:<port>`
    /// for unauthenticated legacy peers (which can never be trusted).
    pub id: String,
    pub alias: String,
    pub device_model: Option<String>,
    pub device_kind: DeviceKind,
    pub verified: bool,
    pub protocol: Protocol,
    /// Advertises Ferry extensions (hint until confirmed over TLS).
    pub is_ferry: bool,
    pub trusted: bool,
    pub favorite: bool,
    /// Paired as one of "My devices".
    pub mine: bool,
    pub online: bool,
    pub last_seen_ms: u64,
    /// `host:port` of the best known channel.
    pub address: Option<String>,
    pub ip_version: Option<u8>,
    pub rtt_ms: Option<u32>,
    /// The user's own name for this device, overriding `alias` in the UI.
    pub custom_alias: Option<String>,
    /// Exposes LocalSend's browser download API.
    pub download: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferState {
    /// Expanding folders / connecting.
    Preparing,
    /// Waiting for the receiver to accept (or for a PIN).
    WaitingForAcceptance,
    PinRequired,
    Transferring,
    /// Paused by the user (Ferry peers only).
    Paused,
    /// Connection lost; retrying automatically.
    Reconnecting,
    /// Finishing integrity checks.
    Verifying,
    Completed,
    /// Some files finished, others failed.
    CompletedWithErrors,
    Declined,
    Cancelled,
    Failed,
}

impl TransferState {
    pub fn is_final(self) -> bool {
        matches!(
            self,
            TransferState::Completed
                | TransferState::CompletedWithErrors
                | TransferState::Declined
                | TransferState::Cancelled
                | TransferState::Failed
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileState {
    Pending,
    Transferring,
    Verifying,
    Done,
    Failed,
    /// Not accepted by the receiver.
    Skipped,
    Cancelled,
}

impl FileState {
    pub fn is_final(self) -> bool {
        matches!(self, FileState::Done | FileState::Failed | FileState::Skipped | FileState::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRef {
    pub id: String,
    pub alias: String,
    pub device_kind: DeviceKind,
    pub device_model: Option<String>,
    pub verified: bool,
}

/// The transport a transfer is actually using (shown on the transfer card).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    /// `lan` | `webrtc` | `browser`
    pub transport: String,
    pub encrypted: bool,
    pub ip_version: Option<u8>,
    /// WebRTC through a TURN relay (still end-to-end encrypted).
    pub relayed: bool,
    pub address: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferFile {
    pub id: String,
    /// Relative name, `/`-separated for files inside folders.
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub state: FileState,
    pub bytes_done: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    /// Where a received file was saved / where a sent file came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Lightweight transfer snapshot sent with every progress update.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferSummary {
    pub id: String,
    pub direction: Direction,
    /// Set for group drops: all transfers of one drop share it.
    pub drop_id: Option<String>,
    pub peer: PeerRef,
    pub state: TransferState,
    pub file_count: u32,
    pub files_done: u32,
    pub total_bytes: u64,
    pub bytes_done: u64,
    /// Smoothed throughput in bytes/s.
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub connection: Option<ConnectionInfo>,
    /// Pause/resume and reconnect-after-restart are available.
    pub resumable: bool,
    /// The first file's name (cards show "photo.jpg and 7 more").
    pub title: String,
    /// For text messages.
    pub text: Option<String>,
    pub error: Option<ErrorInfo>,
    /// Root folder (receive) when everything landed in one place.
    pub save_dir: Option<String>,
}

/// A transfer request waiting for the user's decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingRequest {
    pub id: String,
    pub peer: PeerRef,
    pub files: Vec<IncomingFile>,
    pub total_bytes: u64,
    /// Text message content (no files to save).
    pub text: Option<String>,
    pub received_at_ms: u64,
    /// The sender is a trusted device (accept would be one tap).
    pub trusted: bool,
    pub default_save_dir: String,
    /// Expires (auto-declines) at this time.
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
}

/// The user's answer to an [`IncomingRequest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    /// `None` = accept everything; `Some(ids)` = accept a subset (may be empty).
    pub accept: Option<Vec<String>>,
    pub decline: bool,
    /// Also trust this device from now on (only effective for verified peers).
    pub trust: bool,
    pub save_dir: Option<PathBuf>,
}

impl Decision {
    pub fn accept_all() -> Self {
        Self::default()
    }

    pub fn decline() -> Self {
        Self { decline: true, ..Self::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryKind {
    File,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryStatus {
    Completed,
    Failed,
    Cancelled,
}

/// One item of history / the Inbox (metadata only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    pub transfer_id: String,
    pub direction: Direction,
    pub peer_id: String,
    pub peer_alias: String,
    pub peer_kind: DeviceKind,
    pub kind: HistoryKind,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub path: Option<String>,
    /// Only stored when the user enabled keeping message text.
    pub text: Option<String>,
    pub timestamp_ms: u64,
    pub status: HistoryStatus,
    /// SHA-256 was compared end to end.
    pub verified: bool,
}

/// A private link room (WebRTC): devices that opened the same link see each
/// other on any network. The secret travels only in the link's fragment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomInfo {
    /// Server-side room id (`r:` + 22 base64url characters).
    pub id: String,
    /// `…/#room=<base64url secret>`: whoever has it can join.
    pub link: String,
    /// Other devices in the room right now.
    pub peers: u32,
    pub created_at_ms: u64,
}

/// The signaling connection used for WebRTC transfers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalingStatus {
    /// `None` when no signaling server is configured.
    pub url: Option<String>,
    /// `off` | `connecting` | `open` | `closed`
    pub state: String,
    pub error: Option<String>,
    /// This device's WebRTC identity key (base64url Ed25519).
    pub identity_key: String,
}
