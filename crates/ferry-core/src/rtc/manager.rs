//! WebRTC in the engine: presence from the signaling server, private link
//! rooms, sessions per peer, and transfers in both directions through the same
//! paths as LAN transfers (transfer registry, decision prompts, the receive
//! safety path: sanitize → `.ferrypart` → no-replace rename → Mark-of-the-Web →
//! history).
//!
//! Devices met through signaling get the id `rtc:<identity key>`. Every
//! session pins that key (`expected_peer_key`), so the id is a cryptographic
//! identity: it can be trusted like a verified LAN device, and only explicit
//! trust on it ever skips the accept prompt.

use super::b64;
use super::identity::RtcIdentity;
use super::peer::{ConnectOptions, Connected, Connector, ConnectorConfig, IncomingConnection};
use super::protocol::{self, DeviceInfo, FileMeta, MAX_FILES, MAX_TEXT_BYTES, RtcError, file_name_problem};
use super::session::*;
use super::signaling::{ClientInfo, ClientInfoOut, SignalingClient, SignalingConfig, SignalingEvent, SignalingState};
use super::transcript::room_id_from_secret;
use crate::db::{InboundFileRecord, InboundRecord, NewHistoryEntry};
use crate::error::{ErrorInfo, Result};
use crate::events::{EngineEvent, NoticeLevel};
use crate::fsutil::part::{self, PartSpec, PartWriter};
use crate::fsutil::sanitize::{SafeRelativePath, sanitize_relative_path};
use crate::fsutil::{PART_SUFFIX, motw, space, unique};
use crate::model::{
    ConnectionInfo, Decision, DeviceKind, Direction, FileState, HistoryKind, HistoryStatus, IncomingFile, IncomingRequest, PeerRef,
    RoomInfo, SignalingStatus, TransferFile, TransferState,
};
use crate::proto::{DeviceDto, FerryHint};
use crate::receive::{MAX_SESSIONS, MAX_SESSIONS_PER_PEER, SessionSlot, SlotTable};
use crate::send::{OutFile, SendItem, build_manifest};
use crate::settings::AutoAccept;
use crate::shared::{Shared, platform_name};
use crate::transfer::{NewTransfer, TransferEntry};
use crate::util::{message_history, now_ms, random_token};
use async_trait::async_trait;
use bytes::Bytes;
use indexmap::{IndexMap, IndexSet};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Device ids of WebRTC peers: `rtc:` + base64url identity key.
pub const DEVICE_PREFIX: &str = "rtc:";
const MAX_PENDING: usize = 16;
const MAX_PENDING_PER_PEER: usize = 3;
/// Messages remembered by (peer, transfer id), so a re-sent or resumed
/// transfer doesn't show its text twice.
const MAX_DELIVERED: usize = 256;
/// Receives with files per peer (identity key) and overall, from admission
/// (while the user decides) until they end: the same caps as LAN sessions.
const MAX_RECEIVES_PER_PEER: usize = MAX_SESSIONS_PER_PEER;
const MAX_RECEIVES: usize = MAX_SESSIONS;
/// Messages admitted per peer / overall within MESSAGE_WINDOW; more are declined.
const MAX_MESSAGES_PER_PEER: usize = 10;
const MAX_MESSAGES: usize = 30;
const MESSAGE_WINDOW: Duration = Duration::from_secs(60);
/// Sender reconnect attempts after an interruption.
const RETRY_DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];
/// How long a receiver waits for an interrupted sender to come back.
const RECEIVER_WAIT: Duration = Duration::from_secs(3 * 60);
/// Interrupted receives can be resumed this long.
const RESUME_TTL: Duration = Duration::from_secs(24 * 3600);
const SYNC_THRESHOLD: u64 = 8 * 1024 * 1024;
/// Presence is re-confirmed this often (the directory marks silent devices offline).
const PRESENCE_REFRESH: Duration = Duration::from_secs(30);
/// Codes that end a send; anything else (closed, timeout, webrtc, offline, gone) is retried.
const FATAL: [&str; 12] = [
    "invalid",
    "invalid-state",
    "auth",
    "protocol",
    "rejected",
    "too-large",
    "source",
    "no-sink",
    "internal",
    "overrun",
    "no-resume",
    "cancelled",
];

pub fn device_id(key: &str) -> String {
    format!("{DEVICE_PREFIX}{key}")
}

pub fn key_of(device_id: &str) -> Option<&str> {
    device_id.strip_prefix(DEVICE_PREFIX)
}

fn kind_of(device_type: Option<&str>) -> DeviceKind {
    match device_type.unwrap_or("").to_ascii_lowercase().as_str() {
        "mobile" => DeviceKind::Mobile,
        "desktop" => DeviceKind::Desktop,
        "headless" => DeviceKind::Headless,
        "server" => DeviceKind::Server,
        "web" => DeviceKind::Web,
        _ => DeviceKind::Web,
    }
}

fn kind_name(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Mobile => "mobile",
        DeviceKind::Desktop => "desktop",
        DeviceKind::Web => "web",
        DeviceKind::Headless => "headless",
        DeviceKind::Server => "server",
    }
}

fn webrtc_connection(relayed: Option<bool>, address: Option<String>) -> ConnectionInfo {
    let ip_version = address
        .as_deref()
        .and_then(|a| a.rsplit_once(':'))
        .and_then(|(ip, _)| ip.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .map(|ip| if ip.is_ipv6() { 6 } else { 4 });
    ConnectionInfo { transport: "webrtc".into(), encrypted: true, ip_version, relayed, address }
}

fn err_info(e: &RtcError) -> ErrorInfo {
    ErrorInfo::new(&e.code.replace('-', "_"), e.message.clone())
}

// ── State ─────────────────────────────────────────────────────────────────

struct Room {
    secret: Vec<u8>,
    link: String,
    created_ms: u64,
    peers: HashSet<String>,
}

struct Running {
    signaling: SignalingClient,
    connector: Arc<Connector>,
    stop: CancellationToken,
}

/// One received file of a WebRTC transfer.
struct InFile {
    meta: FileMeta,
    rel: SafeRelativePath,
    dest_dir: PathBuf,
    part_path: Option<PathBuf>,
    final_path: Option<PathBuf>,
}

/// An accepted incoming transfer: where its files go and how far they got.
struct Receive {
    key: String,
    transfer_id: String,
    entry: Arc<TransferEntry>,
    peer: PeerRef,
    save_dir: PathBuf,
    files: Mutex<IndexMap<String, InFile>>,
    session: Mutex<Option<PeerSession>>,
    /// Bumped when an interrupted transfer resumes (stale waiters stop).
    generation: std::sync::atomic::AtomicU64,
    interrupted_at: Mutex<Option<Instant>>,
    /// Counts against the receive caps while the transfer is live or waiting
    /// for its sender to come back; see [`SessionSlot`].
    slot: Mutex<Option<SessionSlot<String>>>,
}

struct Outgoing {
    entry: Arc<TransferEntry>,
    cancel: CancellationToken,
    session: Mutex<Option<PeerSession>>,
}

struct PendingRequest {
    key: String,
    transfer_id: String,
    decision: Option<oneshot::Sender<Decision>>,
}

#[derive(Default)]
struct State {
    /// Peers on the signaling server, by identity key.
    present: HashMap<String, ClientInfo>,
    /// Every connection seen, by client id. A device that reconnects is
    /// briefly there twice under one key; when one connection leaves, the
    /// other keeps it present.
    clients: HashMap<String, ClientInfo>,
    /// Present in our nearby group (else only through rooms).
    nearby: HashSet<String>,
    rooms: IndexMap<String, Room>,
    sessions: HashMap<String, PeerSession>,
    /// How each session is connected (relay, remote address), by session id.
    connections: HashMap<String, ConnectionInfo>,
    /// Outgoing transfers by id (= ferry-dc transferId).
    outgoing: HashMap<String, Arc<Outgoing>>,
    /// Incoming transfers by (peer key, transferId).
    receives: HashMap<(String, String), Arc<Receive>>,
    pending: HashMap<String, PendingRequest>,
    signaling_state: Option<SignalingState>,
    /// Messages already shown, by (peer key, transferId), oldest first.
    delivered: IndexSet<(String, String)>,
    /// When recent messages were admitted, by peer key.
    message_times: HashMap<String, VecDeque<Instant>>,
}

impl State {
    /// Registers an offer waiting for its user unless the peer (or everyone
    /// together) is at the cap. The earlier check in `on_offer` only saves
    /// work; this one, made under the same lock as the insert, is what holds
    /// when offers from several peers arrive at once.
    fn add_pending(&mut self, id: &str, request: PendingRequest) -> bool {
        let mine = self.pending.values().filter(|p| p.key == request.key).count();
        if self.pending.len() >= MAX_PENDING || mine >= MAX_PENDING_PER_PEER {
            return false;
        }
        self.pending.insert(id.to_string(), request);
        true
    }

    /// Admits a message from `key` at `now` unless it (or everyone together)
    /// sent too many within the last minute.
    fn admit_message(&mut self, key: &str, now: Instant) -> bool {
        let mut total = 0;
        self.message_times.retain(|_, times| {
            while times.front().is_some_and(|t| now.saturating_duration_since(*t) >= MESSAGE_WINDOW) {
                times.pop_front();
            }
            total += times.len();
            !times.is_empty()
        });
        let mine = self.message_times.get(key).map_or(0, VecDeque::len);
        if mine >= MAX_MESSAGES_PER_PEER || total >= MAX_MESSAGES {
            return false;
        }
        self.message_times.entry(key.to_string()).or_default().push_back(now);
        true
    }

    /// Notes that the message of (`key`, `transfer_id`) is shown; false if it was already.
    fn first_delivery(&mut self, key: &str, transfer_id: &str) -> bool {
        if !self.delivered.insert((key.to_string(), transfer_id.to_string())) {
            return false;
        }
        if self.delivered.len() > MAX_DELIVERED {
            self.delivered.shift_remove_index(0);
        }
        true
    }
}

pub struct RtcManager {
    shared: Arc<Shared>,
    identity: Arc<RtcIdentity>,
    me: Weak<RtcManager>,
    state: Mutex<State>,
    running: Mutex<Option<Running>>,
    /// Serializes start/stop: two overlapping restarts would otherwise each
    /// replace `running`, leaving one signaling connection orphaned but live.
    lifecycle: Mutex<()>,
    connect_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    nearby: std::sync::atomic::AtomicBool,
    loopback: std::sync::atomic::AtomicBool,
    /// Receive slots in use, by peer key.
    slots: Arc<Mutex<SlotTable<String>>>,
}

impl RtcManager {
    pub fn new(shared: Arc<Shared>) -> Result<Arc<Self>> {
        let identity = Arc::new(RtcIdentity::from_pkcs8_pem(&shared.identity.signing_key_pem).map_err(|e| ErrorInfo::new("identity", e))?);
        Ok(Arc::new_cyclic(|me| RtcManager {
            shared,
            identity,
            me: me.clone(),
            state: Mutex::new(State::default()),
            running: Mutex::new(None),
            lifecycle: Mutex::new(()),
            connect_locks: tokio::sync::Mutex::new(HashMap::new()),
            nearby: std::sync::atomic::AtomicBool::new(true),
            loopback: std::sync::atomic::AtomicBool::new(false),
            slots: Arc::new(Mutex::new(SlotTable::default())),
        }))
    }

    fn arc(&self) -> Option<Arc<RtcManager>> {
        self.me.upgrade()
    }

    pub fn identity_key(&self) -> &str {
        self.identity.public_key()
    }

    /// Whether this device is shown to peers on the same network (else only in rooms).
    pub fn set_nearby(&self, nearby: bool) {
        self.nearby.store(nearby, Ordering::Relaxed);
    }

    /// Also gather loopback ICE candidates (same-machine tests without a network).
    pub fn set_include_loopback(&self, include: bool) {
        self.loopback.store(include, Ordering::Relaxed);
    }

    fn client_info(&self) -> ClientInfoOut {
        let s = self.shared.settings.get();
        ClientInfoOut {
            alias: s.alias.clone(),
            device_model: self.shared.device_model(),
            device_type: Some(kind_name(self.shared.device_kind()).to_string()),
            token: random_token(),
            public_key: self.identity.public_key().to_string(),
            nearby: (!self.nearby.load(Ordering::Relaxed)).then_some(false),
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            alias: self.shared.settings.get().alias,
            device_type: kind_name(self.shared.device_kind()).into(),
            platform: platform_name().into(),
        }
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Connects to `settings.signaling_url` (no-op without one). Restarts a
    /// running connection.
    pub fn start(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap();
        self.stop_running();
        let Some(this) = self.arc() else { return };
        let settings = self.shared.settings.get();
        let Some(url) = settings.signaling_url.clone().filter(|u| !u.trim().is_empty()) else {
            self.emit_status();
            return;
        };
        let mut config = SignalingConfig::new(url.trim(), self.client_info());
        config.initial_backoff = Duration::from_millis(500);
        let (signaling, events) = SignalingClient::start(config);
        let mut cc = ConnectorConfig::new(self.identity.clone(), self.device_info());
        cc.sink = Some(Arc::new(EngineSink { manager: self.me.clone() }));
        cc.ice_servers = ice_servers(&settings.stun_servers);
        cc.include_loopback = self.loopback.load(Ordering::Relaxed);
        let decision = Duration::from_secs(settings.decision_timeout_secs.clamp(10, 3600) + 5);
        cc.tune = Arc::new(move |o: &mut SessionOptions| o.decision_timeout = Some(decision));
        let connector = Connector::new(signaling.clone(), cc);
        let stop = CancellationToken::new();
        *self.running.lock().unwrap() = Some(Running { signaling: signaling.clone(), connector: connector.clone(), stop: stop.clone() });
        let rooms: Vec<String> = self.state.lock().unwrap().rooms.keys().cloned().collect();
        for room in rooms {
            let _ = signaling.join_room(&room);
        }
        tokio::spawn(this.clone().signaling_loop(events, connector, stop.clone()));
        tokio::spawn(this.refresh_loop(stop));
        self.emit_status();
    }

    /// Disconnects from signaling and closes every session.
    pub fn stop(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap();
        self.stop_running();
    }

    fn stop_running(&self) {
        let running = self.running.lock().unwrap().take();
        if let Some(r) = running {
            r.stop.cancel();
            r.connector.dispose();
            r.signaling.close();
        }
        let sessions: Vec<PeerSession> = self.state.lock().unwrap().sessions.drain().map(|(_, s)| s).collect();
        for s in sessions {
            s.close(Some("signaling stopped"));
        }
        self.clear_presence();
        self.state.lock().unwrap().signaling_state = None;
    }

    /// Applies changed settings (alias / kind → re-announce; URL / STUN → reconnect).
    pub fn settings_changed(&self, previous: &crate::Settings) {
        let now = self.shared.settings.get();
        if previous.signaling_url != now.signaling_url
            || previous.stun_servers != now.stun_servers
            || previous.decision_timeout_secs != now.decision_timeout_secs
        {
            self.start();
        } else if (previous.alias != now.alias || previous.device_kind != now.device_kind || previous.device_model != now.device_model)
            && let Some(r) = self.running.lock().unwrap().as_ref()
        {
            r.signaling.update(self.client_info());
            r.connector.set_device(self.device_info());
        }
    }

    pub fn status(&self) -> SignalingStatus {
        let url = self.shared.settings.get().signaling_url.filter(|u| !u.trim().is_empty());
        let running = self.running.lock().unwrap();
        let (state, error) = match running.as_ref() {
            None => ("off", None),
            Some(r) => (
                match r.signaling.state() {
                    SignalingState::Connecting => "connecting",
                    SignalingState::Open => "open",
                    SignalingState::Closed => "closed",
                },
                r.signaling.last_error(),
            ),
        };
        SignalingStatus { url, state: state.into(), error, identity_key: self.identity.public_key().into() }
    }

    fn emit_status(&self) {
        self.shared.events.emit(EngineEvent::SignalingStatus { status: self.status() });
    }

    fn connector(&self) -> Option<Arc<Connector>> {
        self.running.lock().unwrap().as_ref().map(|r| r.connector.clone())
    }

    async fn refresh_loop(self: Arc<Self>, stop: CancellationToken) {
        let mut tick = tokio::time::interval(PRESENCE_REFRESH);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => {
                    let keys: Vec<String> = self.state.lock().unwrap().present.keys().cloned().collect();
                    for k in keys { self.announce(&k); }
                    self.prune_resumes();
                }
            }
        }
    }

    // ── Signaling events / presence ─────────────────────────────────────

    async fn signaling_loop(
        self: Arc<Self>,
        mut events: mpsc::UnboundedReceiver<SignalingEvent>,
        connector: Arc<Connector>,
        stop: CancellationToken,
    ) {
        loop {
            let event = tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                e = events.recv() => e,
            };
            let Some(event) = event else { return };
            // A stopped connection's last events (its own Closed, a late Hello)
            // must not touch the shared presence its replacement already filled.
            if stop.is_cancelled() {
                return;
            }
            if let Some(incoming) = connector.handle_signal(&event) {
                self.on_incoming_connection(&connector, incoming);
                continue;
            }
            match event {
                SignalingEvent::State(state) => {
                    self.state.lock().unwrap().signaling_state = Some(state);
                    if state != SignalingState::Open {
                        self.clear_presence();
                    }
                    self.emit_status();
                }
                SignalingEvent::Hello { peers, .. } => {
                    let old: Vec<String> = {
                        let mut st = self.state.lock().unwrap();
                        st.clients.clear();
                        st.nearby.drain().collect()
                    };
                    for k in old {
                        self.drop_key(&k);
                    }
                    for p in peers {
                        self.seen(p, None);
                    }
                    self.emit_status();
                }
                SignalingEvent::Join { peer } => self.seen(peer, None),
                SignalingEvent::Update { peer } => {
                    let room = {
                        let st = self.state.lock().unwrap();
                        peer.key()
                            .filter(|k| !st.nearby.contains(*k))
                            .and_then(|k| st.rooms.iter().find(|(_, r)| r.peers.contains(k)).map(|(id, _)| id.clone()))
                    };
                    self.seen(peer, room);
                }
                SignalingEvent::Left { peer_id } => self.gone(&peer_id, None),
                SignalingEvent::RoomHello { room, peers } => {
                    let old: Vec<String> = match self.state.lock().unwrap().rooms.get_mut(&room) {
                        Some(r) => r.peers.drain().collect(),
                        None => continue,
                    };
                    for k in old {
                        self.drop_key(&k);
                    }
                    for p in peers {
                        self.seen(p, Some(room.clone()));
                    }
                    self.emit_room(&room);
                }
                // The guard's lock is released before `seen` takes it again.
                SignalingEvent::RoomPeerJoined { room, peer } if self.state.lock().unwrap().rooms.contains_key(&room) => {
                    self.seen(peer, Some(room));
                }
                SignalingEvent::RoomPeerLeft { room, peer_id } => self.gone(&peer_id, Some(&room)),
                SignalingEvent::Error { code, message, room, .. } => {
                    tracing::debug!("signaling error {code}: {message}");
                    if let Some(room) = room {
                        self.shared.events.emit(EngineEvent::Notice {
                            level: NoticeLevel::Warning,
                            code: "room_error".into(),
                            message: format!("Couldn't use the private link ({room}): {message}"),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// A peer is present (nearby, or in `room`).
    fn seen(&self, peer: ClientInfo, room: Option<String>) {
        let Some(key) = peer.key().map(str::to_string) else { return }; // only Ferry clients speak ferry-dc/1
        if key == self.identity.public_key() || b64::decode(&key).is_none() {
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            match &room {
                Some(id) => match st.rooms.get_mut(id) {
                    Some(r) => {
                        r.peers.insert(key.clone());
                    }
                    None => return,
                },
                None => {
                    st.nearby.insert(key.clone());
                }
            }
            st.clients.insert(peer.id.clone(), peer.clone());
            st.present.insert(key.clone(), peer);
        }
        self.announce(&key);
        if let Some(room) = room {
            self.emit_room(&room);
        }
    }

    fn gone(&self, client_id: &str, room: Option<&str>) {
        let (keys, moved) = {
            let mut st = self.state.lock().unwrap();
            st.clients.remove(client_id);
            let mut keys = Vec::new();
            let mut moved = Vec::new();
            let left: Vec<String> = st.present.iter().filter(|(_, p)| p.id == client_id).map(|(k, _)| k.clone()).collect();
            for k in left {
                // Still connected another way (a reconnect overlapping the old
                // connection): carry on with that one.
                let other = st.clients.values().find(|c| c.key() == Some(k.as_str())).cloned();
                match other {
                    Some(c) => {
                        st.present.insert(k.clone(), c);
                        moved.push(k);
                    }
                    None => keys.push(k),
                }
            }
            for k in &keys {
                match room {
                    Some(r) => {
                        if let Some(r) = st.rooms.get_mut(r) {
                            r.peers.remove(k);
                        }
                    }
                    None => {
                        st.nearby.remove(k);
                    }
                }
            }
            (keys, moved)
        };
        for k in keys {
            self.drop_key(&k);
        }
        for k in moved {
            self.announce(&k);
        }
        if let Some(r) = room {
            self.emit_room(r);
        }
    }

    /// Forgets `key` unless it is still reachable another way.
    fn drop_key(&self, key: &str) {
        let still = {
            let mut st = self.state.lock().unwrap();
            let still = st.nearby.contains(key) || st.rooms.values().any(|r| r.peers.contains(key));
            if !still {
                st.present.remove(key);
            }
            still
        };
        if still {
            self.announce(key);
        } else {
            self.shared.devices.remote_gone(&device_id(key));
        }
    }

    fn clear_presence(&self) {
        let (keys, rooms) = {
            let mut st = self.state.lock().unwrap();
            st.nearby.clear();
            st.clients.clear();
            for r in st.rooms.values_mut() {
                r.peers.clear();
            }
            (st.present.drain().map(|(k, _)| k).collect::<Vec<_>>(), st.rooms.keys().cloned().collect::<Vec<_>>())
        };
        for k in keys {
            self.shared.devices.remote_gone(&device_id(&k));
        }
        for r in rooms {
            self.emit_room(&r);
        }
    }

    fn announce(&self, key: &str) {
        let (peer, label) = {
            let st = self.state.lock().unwrap();
            let Some(peer) = st.present.get(key).cloned() else { return };
            let via_room = !st.nearby.contains(key) && st.rooms.values().any(|r| r.peers.contains(key));
            (peer, via_room.then(|| "via private link".to_string()))
        };
        let dto = DeviceDto {
            alias: peer.alias.clone(),
            version: peer.version.clone(),
            device_model: peer.device_model.clone(),
            device_type: Some(kind_of(peer.device_type.as_deref())),
            fingerprint: device_id(key),
            port: None,
            protocol: None,
            download: false,
            ferry: Some(FerryHint { v: 1, caps: vec!["webrtc".into()] }),
        };
        self.shared.devices.observe_remote(&device_id(key), dto, label.or_else(|| Some("WebRTC".into())));
    }

    fn room_of(&self, key: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().rooms.values().find(|r| r.peers.contains(key)).map(|r| r.secret.clone())
    }

    fn peer_ref(&self, key: &str, remote: Option<&RemotePeer>) -> PeerRef {
        let id = device_id(key);
        match self.shared.devices.get(&id) {
            Some(d) => PeerRef {
                id,
                alias: d.custom_alias.unwrap_or(d.alias),
                device_kind: d.device_kind,
                device_model: d.device_model,
                verified: true,
            },
            None => PeerRef {
                id,
                alias: remote
                    .map(|r| r.device.alias.chars().filter(|c| !c.is_control()).take(64).collect::<String>())
                    .filter(|a| !a.trim().is_empty())
                    .unwrap_or_else(|| "Unknown device".into()),
                device_kind: kind_of(remote.map(|r| r.device.device_type.as_str())),
                device_model: None,
                verified: true,
            },
        }
    }

    // ── Rooms ────────────────────────────────────────────────────────────

    fn link_base(&self) -> String {
        let url = self.shared.settings.get().signaling_url.unwrap_or_default();
        let base = url.split(['?', '#']).next().unwrap_or("");
        match base.split_once("://") {
            Some((scheme, rest)) => {
                let host = rest.split('/').next().unwrap_or("");
                format!("{}://{host}", if scheme == "wss" { "https" } else { "http" })
            }
            None => "https://ferry.invalid".into(),
        }
    }

    fn room_info(id: &str, r: &Room) -> RoomInfo {
        RoomInfo { id: id.to_string(), link: r.link.clone(), peers: r.peers.len() as u32, created_at_ms: r.created_ms }
    }

    fn emit_room(&self, id: &str) {
        let info = self.state.lock().unwrap().rooms.get(id).map(|r| Self::room_info(id, r));
        if let Some(room) = info {
            self.shared.events.emit(EngineEvent::RoomUpdated { room });
        }
    }

    fn join_secret(&self, secret: Vec<u8>) -> RoomInfo {
        let id = room_id_from_secret(&secret);
        let created = {
            let mut st = self.state.lock().unwrap();
            if st.rooms.contains_key(&id) {
                false
            } else {
                let link = format!("{}/#room={}", self.link_base(), b64::encode(&secret));
                st.rooms.insert(id.clone(), Room { secret, link, created_ms: now_ms(), peers: HashSet::new() });
                true
            }
        };
        if created && let Some(r) = self.running.lock().unwrap().as_ref() {
            let _ = r.signaling.join_room(&id);
        }
        self.emit_room(&id);
        let st = self.state.lock().unwrap();
        Self::room_info(&id, &st.rooms[&id])
    }

    /// A new private link (random 128-bit secret).
    pub fn create_room(&self) -> RoomInfo {
        self.join_secret(crate::util::random_bytes::<16>().to_vec())
    }

    /// Joins a link (`…#room=<secret>`) or a bare secret.
    pub fn join_room(&self, link: &str) -> Result<RoomInfo> {
        let input = link.trim();
        let secret_text = ["#room=", "&room=", "?room="]
            .iter()
            .find_map(|p| input.find(p).map(|i| &input[i + p.len()..]))
            .or_else(|| input.strip_prefix("room="))
            .unwrap_or(input);
        let secret_text: String = secret_text.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        match b64::decode(&secret_text) {
            Some(secret) if (16..=64).contains(&secret.len()) => Ok(self.join_secret(secret)),
            _ => Err(ErrorInfo::new("bad_link", "That isn't a Ferry link.").into()),
        }
    }

    pub fn leave_room(&self, id: &str) -> bool {
        let Some(room) = self.state.lock().unwrap().rooms.shift_remove(id) else { return false };
        if let Some(r) = self.running.lock().unwrap().as_ref() {
            r.signaling.leave_room(id);
        }
        for k in room.peers {
            self.drop_key(&k);
        }
        self.shared.events.emit(EngineEvent::RoomRemoved { id: id.to_string() });
        true
    }

    pub fn rooms(&self) -> Vec<RoomInfo> {
        self.state.lock().unwrap().rooms.iter().map(|(id, r)| Self::room_info(id, r)).collect()
    }

    // ── Sessions ─────────────────────────────────────────────────────────

    fn on_incoming_connection(&self, connector: &Arc<Connector>, incoming: IncomingConnection) {
        let Some(key) = incoming.peer.key().map(str::to_string) else {
            connector.reject(&incoming);
            return;
        };
        if !self.shared.settings.get().receive_enabled || key == self.identity.public_key() {
            connector.reject(&incoming);
            return;
        }
        let Some(this) = self.arc() else { return };
        let connector = connector.clone();
        let room_secret = self.room_of(&key);
        tokio::spawn(async move {
            let options = ConnectOptions {
                expected_peer_key: Some(key.clone()),
                room_secret,
                ice_servers: turn_servers(connector.signaling()).await,
            };
            match connector.accept(incoming, options).await {
                Ok(connected) => {
                    this.adopt(&key, connected);
                }
                Err(e) => tracing::debug!("incoming WebRTC connection failed: {e}"),
            }
        });
    }

    /// Takes over an authenticated session: keeps it for `key` and handles its events.
    fn adopt(self: &Arc<Self>, key: &str, connected: Connected) -> PeerSession {
        let session = connected.session.clone();
        {
            let mut st = self.state.lock().unwrap();
            st.connections.insert(session.session_id().to_string(), webrtc_connection(connected.relayed, connected.remote_address.clone()));
            let replace = st.sessions.get(key).is_none_or(|s| s.state() != SessionState::Ready);
            if replace {
                st.sessions.insert(key.to_string(), session.clone());
            }
        }
        tokio::spawn(self.clone().drive(key.to_string(), session.clone(), connected.events, connected.relayed, connected.remote_address));
        session
    }

    /// An authenticated session for `key`: the open one, or a new connection.
    async fn session_for(self: &Arc<Self>, key: &str, fresh: bool) -> std::result::Result<(PeerSession, ConnectionInfo), RtcError> {
        let lock = self.connect_locks.lock().await.entry(key.to_string()).or_default().clone();
        let _guard = lock.lock().await;
        if !fresh {
            let st = self.state.lock().unwrap();
            if let Some(s) = st.sessions.get(key).filter(|s| s.state() == SessionState::Ready) {
                let info = st.connections.get(s.session_id()).cloned().unwrap_or_else(|| webrtc_connection(None, None));
                return Ok((s.clone(), info));
            }
        }
        let (client_id, connector) = {
            let st = self.state.lock().unwrap();
            (st.present.get(key).map(|p| p.id.clone()), self.connector())
        };
        let (Some(client_id), Some(connector)) = (client_id, connector) else {
            return Err(RtcError::new("offline", "the device is offline"));
        };
        let options = ConnectOptions {
            expected_peer_key: Some(key.to_string()),
            room_secret: self.room_of(key),
            ice_servers: turn_servers(connector.signaling()).await,
        };
        let connected = connector.connect(&client_id, options).await?;
        let info = webrtc_connection(connected.relayed, connected.remote_address.clone());
        Ok((self.adopt(key, connected), info))
    }

    /// Handles one session's events until it closes.
    async fn drive(
        self: Arc<Self>,
        key: String,
        session: PeerSession,
        mut events: mpsc::UnboundedReceiver<SessionEvent>,
        relayed: Option<bool>,
        address: Option<String>,
    ) {
        let connection = webrtc_connection(relayed, address);
        while let Some(event) = events.recv().await {
            match event {
                SessionEvent::Offer(offer) => {
                    let this = self.clone();
                    let (key, connection) = (key.clone(), connection.clone());
                    tokio::spawn(async move { this.on_offer(key, offer, connection).await });
                }
                SessionEvent::Accepted { transfer_id, files, offsets } => {
                    if let Some(out) = self.outgoing(&transfer_id) {
                        let mut accepted: HashSet<String> = files.iter().cloned().collect();
                        accepted.insert("text".into()); // a message is not an offered file
                        out.entry.skip_files(&accepted);
                        for id in &files {
                            let offset = offsets.iter().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0);
                            out.entry.set_file_state(id, FileState::Transferring, None);
                            out.entry.reset_file_progress(id, offset);
                        }
                        out.entry.set_error(None);
                        out.entry.set_state(TransferState::Transferring);
                    }
                }
                SessionEvent::Progress { transfer_id, direction: super::session::Direction::Send, file_id, bytes, .. } => {
                    if let Some(out) = self.outgoing(&transfer_id) {
                        out.entry.reset_file_progress(&file_id, bytes);
                    }
                }
                SessionEvent::Progress { .. } => {}
                SessionEvent::FileComplete { transfer_id, direction, file_id, ok, error, .. } => {
                    let entry = match direction {
                        super::session::Direction::Send => self.outgoing(&transfer_id).map(|o| o.entry.clone()),
                        super::session::Direction::Receive => self.receive(&key, &transfer_id).map(|r| r.entry.clone()),
                    };
                    if let Some(entry) = entry {
                        if ok {
                            if direction == super::session::Direction::Send {
                                entry.set_file_state(&file_id, FileState::Done, None);
                                if let Some(size) = entry.files().iter().find(|f| f.id == file_id).map(|f| f.size) {
                                    entry.reset_file_progress(&file_id, size);
                                }
                            }
                        } else {
                            entry.set_file_state(
                                &file_id,
                                FileState::Failed,
                                Some(ErrorInfo::new("file_failed", error.unwrap_or_else(|| "The file didn't arrive intact.".into()))),
                            );
                        }
                    }
                }
                SessionEvent::Done { transfer_id, direction: super::session::Direction::Receive, .. } => {
                    if let Some(r) = self.take_receive(&key, &transfer_id) {
                        let state = r.entry.conclude();
                        tracing::info!("WebRTC transfer {} from {} finished: {state:?}", r.entry.id, r.peer.alias);
                        let _ = self.shared.db.delete_inbound(&device_id(&key), &transfer_id);
                    }
                }
                SessionEvent::Done { .. } => {}
                SessionEvent::Cancelled { transfer_id, direction: super::session::Direction::Receive, by_remote, interrupted, reason } => {
                    self.withdraw_pending(&key, &transfer_id, reason.as_deref().unwrap_or("cancelled"));
                    if let Some(r) = self.receive(&key, &transfer_id) {
                        if interrupted {
                            self.receive_interrupted(&r);
                        } else {
                            self.take_receive(&key, &transfer_id);
                            self.discard_parts(&r);
                            r.entry.fail(
                                TransferState::Cancelled,
                                Some(if by_remote { ErrorInfo::cancelled_by_peer() } else { ErrorInfo::cancelled() }),
                            );
                        }
                    }
                }
                SessionEvent::Cancelled { .. } => {}
                SessionEvent::Ready(_) | SessionEvent::Error { .. } => {}
                SessionEvent::Closed { .. } => break,
            }
        }
        let mut st = self.state.lock().unwrap();
        st.connections.remove(session.session_id());
        if st.sessions.get(&key).is_some_and(|s| s.session_id() == session.session_id()) {
            st.sessions.remove(&key);
        }
    }

    fn outgoing(&self, id: &str) -> Option<Arc<Outgoing>> {
        self.state.lock().unwrap().outgoing.get(id).cloned()
    }

    fn receive(&self, key: &str, transfer_id: &str) -> Option<Arc<Receive>> {
        self.state.lock().unwrap().receives.get(&(key.to_string(), transfer_id.to_string())).cloned()
    }

    /// Removes an incoming transfer and frees its receive slot.
    fn take_receive(&self, key: &str, transfer_id: &str) -> Option<Arc<Receive>> {
        let r = self.state.lock().unwrap().receives.remove(&(key.to_string(), transfer_id.to_string()));
        if let Some(r) = &r {
            r.slot.lock().unwrap().take();
        }
        r
    }

    // ── Receiving ────────────────────────────────────────────────────────

    async fn on_offer(self: Arc<Self>, key: String, offer: IncomingOffer, connection: ConnectionInfo) {
        let settings = self.shared.settings.get();
        if !settings.receive_enabled {
            let _ = offer.decline().await;
            return;
        }
        let id = device_id(&key);
        let peer = self.peer_ref(&key, Some(&offer.peer));
        let trust = self.shared.devices.trust(&id);

        // A message: shown right away, nothing to accept (LocalSend semantics).
        if offer.files.is_empty() {
            let Some(text) = offer.text.clone() else {
                let _ = offer.accept(Some(vec![]), &[]).await;
                return;
            };
            let admitted = {
                let mut st = self.state.lock().unwrap();
                let repeat = st.delivered.contains(&(key.clone(), offer.transfer_id.clone()));
                repeat || st.admit_message(&key, Instant::now())
            };
            if !admitted {
                let _ = offer.decline().await;
                return;
            }
            if offer.accept(Some(vec![]), &[]).await.is_ok() {
                self.deliver_message(&key, &peer, &offer.transfer_id, text, trust.trusted);
            }
            return;
        }

        // An interrupted transfer comes back: continue without asking again.
        // Its text (if any) was shown when it was first accepted.
        if let Some(r) = self.receive(&key, &offer.transfer_id) {
            self.resume_receive(r, offer).await;
            return;
        }

        // Admitted from here on: the slot is held while the user decides and
        // then by the transfer until it ends; every early return frees it.
        let Some(slot) = SessionSlot::reserve(&self.slots, key.clone(), MAX_RECEIVES, MAX_RECEIVES_PER_PEER) else {
            let _ = offer.decline().await;
            return;
        };

        let (pending_all, pending_peer) = {
            let st = self.state.lock().unwrap();
            (st.pending.len(), st.pending.values().filter(|p| p.key == key).count())
        };
        if pending_all >= MAX_PENDING || pending_peer >= MAX_PENDING_PER_PEER {
            let _ = offer.decline().await;
            return;
        }
        let save_dir = settings.save_dir();
        let total: u64 = offer.files.iter().map(|f| f.size).sum();
        let _ = std::fs::create_dir_all(&save_dir);
        if let Ok(available) = space::available_space(&save_dir)
            && total.saturating_add(64 * 1024 * 1024) > available
        {
            let info = ErrorInfo::disk_full(total, available);
            self.shared.events.emit(EngineEvent::Notice {
                level: NoticeLevel::Warning,
                code: "disk_full".into(),
                message: format!("Declined a transfer from {}: {}", peer.alias, info.message),
            });
            let _ = offer.decline().await;
            return;
        }

        let auto = match settings.auto_accept {
            AutoAccept::Off => false,
            AutoAccept::MyDevices => trust.mine,
            AutoAccept::Trusted => trust.trusted,
        };
        let decision = if auto {
            Decision::accept_all()
        } else {
            let request_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            let request = PendingRequest { key: key.clone(), transfer_id: offer.transfer_id.clone(), decision: Some(tx) };
            if !self.state.lock().unwrap().add_pending(&request_id, request) {
                let _ = offer.decline().await;
                return;
            }
            let timeout = Duration::from_secs(settings.decision_timeout_secs.clamp(10, 3600));
            self.shared.events.emit(EngineEvent::IncomingRequest {
                request: IncomingRequest {
                    id: request_id.clone(),
                    peer: peer.clone(),
                    files: offer
                        .files
                        .iter()
                        .map(|f| IncomingFile {
                            id: f.id.clone(),
                            name: sanitize_relative_path(&f.name).map(|r| r.display()).unwrap_or_else(|_| f.name.clone()),
                            size: f.size,
                            mime: f.mime.clone(),
                        })
                        .collect(),
                    total_bytes: total,
                    // Text that comes with files is shown once they are accepted.
                    text: None,
                    received_at_ms: now_ms(),
                    trusted: trust.trusted,
                    default_save_dir: save_dir.display().to_string(),
                    expires_at_ms: now_ms() + timeout.as_millis() as u64,
                },
            });
            let answer = tokio::time::timeout(timeout, rx).await;
            let reason = match &answer {
                Ok(Ok(_)) => "answered",
                Ok(Err(_)) => "cancelled",
                Err(_) => "expired",
            };
            if self.state.lock().unwrap().pending.remove(&request_id).is_some() || reason == "answered" {
                self.shared.events.emit(EngineEvent::IncomingRequestClosed { id: request_id, reason: reason.into() });
            }
            match answer {
                Ok(Ok(d)) => d,
                Ok(Err(_)) => return, // withdrawn by the peer (already closed)
                Err(_) => {
                    let _ = offer.decline().await;
                    return;
                }
            }
        };
        if decision.decline {
            let _ = offer.decline().await;
            return;
        }
        let ids: Vec<String> = match &decision.accept {
            None => offer.files.iter().map(|f| f.id.clone()).collect(),
            Some(list) => offer.files.iter().filter(|f| list.contains(&f.id)).map(|f| f.id.clone()).collect(),
        };
        if decision.trust {
            let _ = self.shared.devices.update_flags(&id, Some(true), None, None, None);
        }
        if ids.is_empty() {
            // Nothing accepted: like a decline, the text isn't shown.
            let _ = offer.accept(Some(vec![]), &[]).await;
            return;
        }
        let save_dir = decision.save_dir.clone().unwrap_or(save_dir);
        let accepted: Vec<FileMeta> = offer.files.iter().filter(|f| ids.contains(&f.id)).cloned().collect();
        let receive = match self.create_receive(&key, &offer.transfer_id, &peer, accepted, save_dir, connection).await {
            Ok(r) => r,
            Err(err) => {
                self.shared.events.emit(EngineEvent::Notice {
                    level: NoticeLevel::Error,
                    code: "save_folder".into(),
                    message: format!("Couldn't prepare the save folder: {err}"),
                });
                let _ = offer.decline().await;
                return;
            }
        };
        *receive.slot.lock().unwrap() = Some(slot);
        *receive.session.lock().unwrap() = Some(offer.session().clone());
        match offer.accept(Some(ids), &[]).await {
            // Accepted, even in part: the message that came with the files arrives too.
            Ok(()) => {
                if let Some(text) = offer.text.clone() {
                    self.deliver_message(&key, &peer, &offer.transfer_id, text, trust.trusted);
                }
            }
            Err(e) => {
                self.take_receive(&key, &offer.transfer_id);
                receive.entry.fail(TransferState::Failed, Some(err_info(&e)));
            }
        }
    }

    /// Shows a message once per (peer, transfer) and records it as the
    /// history settings allow.
    fn deliver_message(&self, key: &str, peer: &PeerRef, transfer_id: &str, text: String, trusted: bool) {
        if !self.state.lock().unwrap().first_delivery(key, transfer_id) {
            return;
        }
        let settings = self.shared.settings.get();
        let id = uuid::Uuid::new_v4().to_string();
        self.shared.events.emit(EngineEvent::IncomingRequest {
            request: IncomingRequest {
                id: id.clone(),
                peer: peer.clone(),
                files: Vec::new(),
                total_bytes: 0,
                text: Some(text.clone()),
                received_at_ms: now_ms(),
                trusted,
                default_save_dir: String::new(),
                expires_at_ms: now_ms(),
            },
        });
        if settings.history_enabled {
            let (name, kept) = message_history(&text, settings.keep_message_text);
            let entry = NewHistoryEntry {
                transfer_id: id,
                direction: Direction::Receive,
                peer_id: peer.id.clone(),
                peer_alias: peer.alias.clone(),
                peer_kind: peer.device_kind,
                kind: HistoryKind::Text,
                name,
                size: text.len() as u64,
                mime: "text/plain".into(),
                path: None,
                text: kept,
                timestamp_ms: now_ms(),
                status: HistoryStatus::Completed,
                verified: true,
            };
            if let Ok(entry) = self.shared.db.add_history(&entry) {
                self.shared.events.emit(EngineEvent::HistoryAdded { entry });
            }
        }
    }

    /// Answers the UI's decision on a pending request.
    pub fn respond(&self, request_id: &str, decision: Decision) -> bool {
        let tx = self.state.lock().unwrap().pending.get_mut(request_id).and_then(|p| p.decision.take());
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// The sender withdrew (or the session closed before the decision).
    fn withdraw_pending(&self, key: &str, transfer_id: &str, reason: &str) {
        let ids: Vec<String> = {
            let mut st = self.state.lock().unwrap();
            let ids: Vec<String> =
                st.pending.iter().filter(|(_, p)| p.key == key && p.transfer_id == transfer_id).map(|(id, _)| id.clone()).collect();
            for id in &ids {
                st.pending.remove(id);
            }
            ids
        };
        for id in ids {
            self.shared.events.emit(EngineEvent::IncomingRequestClosed { id, reason: reason.into() });
        }
    }

    async fn create_receive(
        &self,
        key: &str,
        transfer_id: &str,
        peer: &PeerRef,
        files: Vec<FileMeta>,
        save_dir: PathBuf,
        connection: ConnectionInfo,
    ) -> std::io::Result<Arc<Receive>> {
        let dir = save_dir.clone();
        let metas = files.clone();
        // Each transfer's top-level folders get their own (unique) directory.
        let (planned, save_root) = tokio::task::spawn_blocking(move || -> std::io::Result<(IndexMap<String, InFile>, Option<PathBuf>)> {
            std::fs::create_dir_all(&dir)?;
            let mut folders: HashMap<String, PathBuf> = HashMap::new();
            let mut out = IndexMap::new();
            for meta in metas {
                let rel = sanitize_relative_path(&meta.name).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let dest_dir = match rel.parents().split_first() {
                    None => dir.clone(),
                    Some((top, rest)) => {
                        let top_dir = match folders.get(top) {
                            Some(d) => d.clone(),
                            None => {
                                let d = unique::create_unique_dir(&dir, top)?;
                                folders.insert(top.clone(), d.clone());
                                d
                            }
                        };
                        rest.iter().fold(top_dir, |p, c| p.join(c))
                    }
                };
                out.insert(meta.id.clone(), InFile { meta, rel, dest_dir, part_path: None, final_path: None });
            }
            let root = if folders.len() == 1 && out.values().all(|f| !f.rel.parents().is_empty()) {
                folders.values().next().cloned()
            } else {
                Some(dir)
            };
            Ok((out, root))
        })
        .await
        .map_err(std::io::Error::other)??;
        let entry = self.shared.transfers.create(NewTransfer {
            id: uuid::Uuid::new_v4().to_string(),
            direction: Direction::Receive,
            drop_id: None,
            peer: peer.clone(),
            files: planned
                .iter()
                .map(|(id, f)| TransferFile {
                    id: id.clone(),
                    name: f.rel.display(),
                    size: f.meta.size,
                    mime: f.meta.mime.clone(),
                    state: FileState::Pending,
                    bytes_done: 0,
                    error: None,
                    path: None,
                })
                .collect(),
            state: TransferState::Transferring,
            resumable: true,
            pausable: false,
            text: None,
            save_dir: save_root.map(|p| p.display().to_string()),
            connection: Some(connection),
        });
        let receive = Arc::new(Receive {
            key: key.to_string(),
            transfer_id: transfer_id.to_string(),
            entry,
            peer: peer.clone(),
            save_dir,
            files: Mutex::new(planned),
            session: Mutex::new(None),
            generation: Default::default(),
            interrupted_at: Mutex::new(None),
            slot: Mutex::new(None),
        });
        self.state.lock().unwrap().receives.insert((key.to_string(), transfer_id.to_string()), receive.clone());
        Ok(receive)
    }

    /// The session closed under an accepted transfer: keep the partial files
    /// and wait for the sender to come back with the same transfer id.
    fn receive_interrupted(&self, r: &Arc<Receive>) {
        *r.interrupted_at.lock().unwrap() = Some(Instant::now());
        let generation = r.generation.fetch_add(1, Ordering::SeqCst) + 1;
        r.entry.set_state(TransferState::Reconnecting);
        r.entry.set_error(Some(
            ErrorInfo::new("connection_lost", format!("Waiting for {} to reconnect…", r.peer.alias))
                .with_hint("It continues where it stopped."),
        ));
        self.persist_receive(r);
        let r = r.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECEIVER_WAIT).await;
            if r.generation.load(Ordering::SeqCst) == generation && r.entry.state() == TransferState::Reconnecting {
                // Kept for a later resume, but no longer counted as live.
                r.slot.lock().unwrap().take();
                r.entry.fail(
                    TransferState::Failed,
                    Some(
                        ErrorInfo::new("connection_lost", "The connection was lost.")
                            .with_hint(format!("When {} sends again, it continues where it stopped.", r.peer.alias)),
                    ),
                );
            }
        });
    }

    /// Records partial files so they are cleaned up even after a restart.
    fn persist_receive(&self, r: &Receive) {
        let files = r.files.lock().unwrap();
        let record = InboundRecord {
            peer_fingerprint: device_id(&r.key),
            transfer_id: r.transfer_id.clone(),
            peer_alias: r.peer.alias.clone(),
            created_ms: r.entry.started_at_ms,
            updated_ms: now_ms(),
            files: files
                .iter()
                .filter_map(|(id, f)| {
                    let part = f.part_path.as_ref()?;
                    Some(InboundFileRecord {
                        file_id: id.clone(),
                        rel_name: f.rel.display(),
                        size: f.meta.size,
                        mime: f.meta.mime.clone(),
                        part_path: part.display().to_string(),
                        final_path: f.final_path.as_ref().map(|p| p.display().to_string()),
                        offset: std::fs::metadata(part).map(|m| m.len()).unwrap_or(0),
                        done: f.final_path.is_some(),
                        sha256: None,
                        attempts: 0,
                    })
                })
                .collect(),
            // Only kept for cleanup: WebRTC transfers resume from memory.
            save_root: Some(r.save_dir.display().to_string()),
            display_root: None,
            manifest: false,
            declined: Vec::new(),
        };
        if !record.files.is_empty() {
            let _ = self.shared.db.save_inbound(&record);
        }
    }

    async fn resume_receive(&self, r: Arc<Receive>, offer: IncomingOffer) {
        // The re-offer must describe the same files.
        let same = r
            .files
            .lock()
            .unwrap()
            .iter()
            .all(|(id, f)| offer.files.iter().any(|o| &o.id == id && o.size == f.meta.size && o.name == f.meta.name));
        if !same {
            let _ = offer.decline().await;
            return;
        }
        let (ids, offsets) = {
            let files = r.files.lock().unwrap();
            let mut offsets = Vec::new();
            for (id, f) in files.iter() {
                let stored = match (&f.final_path, &f.part_path) {
                    (Some(_), _) => f.meta.size,
                    (None, Some(p)) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0).min(f.meta.size),
                    _ => 0,
                };
                if stored > 0 {
                    offsets.push((id.clone(), stored));
                }
            }
            (files.keys().cloned().collect::<Vec<_>>(), offsets)
        };
        tracing::info!("Resuming WebRTC transfer {} from {} at {offsets:?}", r.transfer_id, r.peer.alias);
        r.generation.fetch_add(1, Ordering::SeqCst);
        *r.interrupted_at.lock().unwrap() = None;
        {
            // Live again: counted again where there is room. It was admitted
            // already, so full caps don't stop the sender's resend.
            let mut slot = r.slot.lock().unwrap();
            if slot.is_none() {
                *slot = SessionSlot::reserve(&self.slots, r.key.clone(), MAX_RECEIVES, MAX_RECEIVES_PER_PEER);
            }
        }
        *r.session.lock().unwrap() = Some(offer.session().clone());
        for (id, off) in &offsets {
            r.entry.reset_file_progress(id, *off);
        }
        r.entry.set_error(None);
        r.entry.set_state(TransferState::Transferring);
        if offer.accept(Some(ids.clone()), &offsets).await.is_err() {
            // The partial data can't be used: start over.
            let _ = offer.accept(Some(ids), &[]).await;
        }
    }

    fn discard_parts(&self, r: &Receive) {
        let parts: Vec<PathBuf> =
            r.files.lock().unwrap().values().filter(|f| f.final_path.is_none()).filter_map(|f| f.part_path.clone()).collect();
        let _ = self.shared.db.delete_inbound(&device_id(&r.key), &r.transfer_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            for p in parts {
                let _ = tokio::fs::remove_file(p).await;
            }
        });
    }

    fn prune_resumes(&self) {
        let stale: Vec<Arc<Receive>> = {
            let mut st = self.state.lock().unwrap();
            let keys: Vec<(String, String)> = st
                .receives
                .iter()
                .filter(|(_, r)| r.interrupted_at.lock().unwrap().is_some_and(|t| t.elapsed() > RESUME_TTL))
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter().filter_map(|k| st.receives.remove(k)).collect()
        };
        for r in stale {
            r.slot.lock().unwrap().take();
            self.discard_parts(&r);
        }
    }

    // ── Sending ──────────────────────────────────────────────────────────

    /// Starts sending `items` to WebRTC devices (`rtc:` ids). One transfer per
    /// target for the files, plus one per text message.
    pub async fn send(&self, targets: Vec<String>, items: Vec<SendItem>, drop_id: Option<String>) -> Result<Vec<String>> {
        let Some(this) = self.arc() else { return Ok(vec![]) };
        let (texts, paths): (Vec<_>, Vec<_>) = items.into_iter().partition(|i| matches!(i, SendItem::Text { .. }));
        let mut texts_out = Vec::new();
        for item in texts {
            let SendItem::Text { text } = item else { continue };
            if protocol::q(&text).len() > MAX_TEXT_BYTES {
                return Err(
                    ErrorInfo::new("text_too_long", "That text is too long to send as a message over the internet (60 KB max).").into()
                );
            }
            texts_out.push(text);
        }
        let files = if paths.is_empty() {
            Vec::new()
        } else {
            let paths: Vec<PathBuf> =
                paths.into_iter().filter_map(|i| if let SendItem::Path { path } = i { Some(path) } else { None }).collect();
            let files = tokio::task::spawn_blocking(move || build_manifest(&paths)).await.map_err(ErrorInfo::internal)??;
            if files.is_empty() {
                return Err(ErrorInfo::new("nothing_to_send", "The selection contains no files.").into());
            }
            if files.len() > MAX_FILES {
                return Err(ErrorInfo::new(
                    "too_many_files",
                    format!("Too many files for one transfer over the internet (more than {MAX_FILES})."),
                )
                .into());
            }
            if let Some((f, problem)) = files.iter().find_map(|f| file_name_problem(&f.name).map(|p| (f, p))) {
                return Err(ErrorInfo::new("bad_name", format!("\"{}\" can't be sent: {problem}.", f.name)).into());
            }
            files
        };
        let mut ids = Vec::new();
        for target in &targets {
            let Some(key) = key_of(target) else { continue };
            for text in &texts_out {
                let file = OutFile {
                    id: "text".into(),
                    name: "Message".into(),
                    path: None,
                    size: text.len() as u64,
                    mime: "text/plain".into(),
                    modified: None,
                    text: Some(text.clone()),
                };
                ids.push(this.start_send(key, vec![file], drop_id.clone()));
            }
            if !files.is_empty() {
                ids.push(this.start_send(key, files.clone(), drop_id.clone()));
            }
        }
        Ok(ids)
    }

    fn start_send(self: &Arc<Self>, key: &str, files: Vec<OutFile>, drop_id: Option<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let text = files.first().and_then(|f| f.text.clone());
        let entry = self.shared.transfers.create(NewTransfer {
            id: id.clone(),
            direction: Direction::Send,
            drop_id,
            peer: self.peer_ref(key, None),
            files: files
                .iter()
                .map(|f| TransferFile {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    size: f.size,
                    mime: f.mime.clone(),
                    state: FileState::Pending,
                    bytes_done: 0,
                    error: None,
                    path: f.path.as_ref().map(|p| p.display().to_string()),
                })
                .collect(),
            state: TransferState::Preparing,
            resumable: true,
            pausable: false,
            text: text.clone(),
            save_dir: None,
            connection: Some(webrtc_connection(None, None)),
        });
        let out = Arc::new(Outgoing { entry, cancel: self.shared.shutdown.child_token(), session: Mutex::new(None) });
        self.state.lock().unwrap().outgoing.insert(id.clone(), out.clone());
        let this = self.clone();
        let key = key.to_string();
        let tid = id.clone();
        tokio::spawn(async move {
            this.run_send(&key, &tid, &out, files, text).await;
            this.finish_history(&out);
        });
        id
    }

    async fn run_send(self: &Arc<Self>, key: &str, transfer_id: &str, out: &Arc<Outgoing>, files: Vec<OutFile>, text: Option<String>) {
        let alias = out.entry.peer().alias;
        let request_files: Vec<OutgoingFile> = files
            .iter()
            .filter(|f| f.text.is_none())
            .filter_map(|f| {
                let path = f.path.clone()?;
                let modified = f.modified.and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
                Some(OutgoingFile {
                    meta: FileMeta { id: f.id.clone(), name: f.name.clone(), size: f.size, mime: f.mime.clone(), modified },
                    source: Arc::new(PathSource::new(path)),
                })
            })
            .collect();
        let mut attempt = 0usize;
        loop {
            if out.cancel.is_cancelled() {
                return;
            }
            let fresh = attempt > 0;
            let connected = tokio::select! {
                r = self.session_for(key, fresh) => r,
                _ = out.cancel.cancelled() => return,
            };
            if let Err(e) = &connected
                && matches!(e.code.as_str(), "cancelled" | "rejected" | "auth")
            {
                // The peer refused the connection (receiving off) or isn't who it claims.
                let info = if e.code == "auth" {
                    ErrorInfo::new("auth", format!("Couldn't verify {alias}: {}", e.message))
                        .with_hint("The device's identity didn't match. Nothing was sent.")
                } else {
                    ErrorInfo::new("rejected", format!("{alias} isn't accepting transfers right now."))
                };
                out.entry.fail(TransferState::Failed, Some(info));
                return;
            }
            let outcome = match connected {
                Ok((session, connection)) => {
                    *out.session.lock().unwrap() = Some(session.clone());
                    out.entry.set_connection(connection);
                    out.entry.set_error(None);
                    out.entry.set_state(if out.entry.bytes_done() > 0 {
                        TransferState::Transferring
                    } else {
                        TransferState::WaitingForAcceptance
                    });
                    let request =
                        TransferRequest { transfer_id: transfer_id.to_string(), files: request_files.clone(), text: text.clone() };
                    // Cancel from another task: the send future must keep being polled,
                    // since it may hold the data channel's write queue while blocked on
                    // backpressure (awaiting a write here instead would deadlock).
                    let watcher = {
                        let (cancel, session, tid) = (out.cancel.clone(), session.clone(), transfer_id.to_string());
                        tokio::spawn(async move {
                            cancel.cancelled().await;
                            session.cancel(Some("cancelled"), Some(&tid)).await;
                        })
                    };
                    let result = session.send_transfer(request).await;
                    if out.cancel.is_cancelled() {
                        let _ = tokio::time::timeout(Duration::from_secs(2), watcher).await;
                        return;
                    }
                    watcher.abort();
                    result
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok(o) if o.declined => {
                    out.entry.fail(TransferState::Declined, Some(ErrorInfo::declined()));
                    return;
                }
                Ok(o) => {
                    if text.is_some() && request_files.is_empty() {
                        out.entry.set_file_state("text", FileState::Done, None);
                        out.entry.reset_file_progress("text", text.as_ref().map(|t| t.len() as u64).unwrap_or(0));
                    }
                    for id in &o.completed {
                        out.entry.set_file_state(id, FileState::Done, None);
                    }
                    let state = if o.failed.is_empty() {
                        TransferState::Completed
                    } else if o.completed.is_empty() {
                        TransferState::Failed
                    } else {
                        TransferState::CompletedWithErrors
                    };
                    if state == TransferState::Completed {
                        out.entry.set_state(state);
                    } else {
                        out.entry.fail(state, Some(ErrorInfo::new("files_failed", format!("{} file(s) failed.", o.failed.len()))));
                    }
                    return;
                }
                Err(e) if e.code == "cancelled" => {
                    let info = if e.message == "timeout" {
                        ErrorInfo::new("no_answer", format!("{alias} didn't answer in time."))
                    } else {
                        ErrorInfo::cancelled_by_peer()
                    };
                    out.entry.fail(TransferState::Cancelled, Some(info));
                    return;
                }
                Err(e) if FATAL.contains(&e.code.as_str()) => {
                    let info = if e.code == "auth" {
                        ErrorInfo::new("auth", format!("Couldn't verify {alias}: {}", e.message))
                            .with_hint("The device's identity didn't match. Nothing was sent.")
                    } else {
                        err_info(&e)
                    };
                    out.entry.fail(TransferState::Failed, Some(info));
                    return;
                }
                Err(e) => {
                    // Lost or unreachable: reconnect and continue where it stopped.
                    if attempt >= RETRY_DELAYS.len() || (attempt == 0 && e.code == "offline") {
                        out.entry.fail(
                            TransferState::Failed,
                            Some(if e.code == "offline" {
                                ErrorInfo::unreachable(&alias)
                            } else {
                                ErrorInfo::new("connection_lost", format!("Couldn't reach {alias} again: {}", e.message))
                            }),
                        );
                        return;
                    }
                    out.entry.set_state(TransferState::Reconnecting);
                    out.entry.set_error(Some(
                        ErrorInfo::new("connection_lost", "Connection lost. Reconnecting…").with_hint("It continues where it stopped."),
                    ));
                    let delay = Duration::from_secs(RETRY_DELAYS[attempt]);
                    attempt += 1;
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = out.cancel.cancelled() => return,
                    }
                }
            }
        }
    }

    fn finish_history(&self, out: &Outgoing) {
        self.state.lock().unwrap().outgoing.remove(&out.entry.id);
        let settings = self.shared.settings.get();
        if !settings.history_enabled {
            return;
        }
        let peer = out.entry.peer();
        let state = out.entry.state();
        let text = out.entry.summary().text;
        for file in out.entry.files() {
            let status = match file.state {
                FileState::Done => HistoryStatus::Completed,
                FileState::Skipped => continue,
                _ if state == TransferState::Cancelled => HistoryStatus::Cancelled,
                _ if state == TransferState::Declined => continue,
                _ => HistoryStatus::Failed,
            };
            let is_text = file.id == "text" && text.is_some();
            let (name, kept) = match &text {
                Some(t) if is_text => message_history(t, settings.keep_message_text),
                _ => (file.name.clone(), None),
            };
            let entry = NewHistoryEntry {
                transfer_id: out.entry.id.clone(),
                direction: Direction::Send,
                peer_id: peer.id.clone(),
                peer_alias: peer.alias.clone(),
                peer_kind: peer.device_kind,
                kind: if is_text { HistoryKind::Text } else { HistoryKind::File },
                name,
                size: file.size,
                mime: file.mime.clone(),
                path: file.path.clone(),
                text: kept,
                timestamp_ms: now_ms(),
                status,
                verified: status == HistoryStatus::Completed,
            };
            if let Ok(e) = self.shared.db.add_history(&entry) {
                self.shared.events.emit(EngineEvent::HistoryAdded { entry: e });
            }
        }
    }

    /// Cancels a WebRTC transfer in either direction (by transfer entry id).
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(out) = self.outgoing(id) {
            if out.entry.state().is_final() {
                return false;
            }
            out.cancel.cancel();
            out.entry.fail(TransferState::Cancelled, Some(ErrorInfo::cancelled()));
            return true;
        }
        let receive = self.state.lock().unwrap().receives.values().find(|r| r.entry.id == id).cloned();
        let Some(r) = receive else { return false };
        let session = r.session.lock().unwrap().clone();
        self.take_receive(&r.key, &r.transfer_id);
        self.discard_parts(&r);
        r.entry.fail(TransferState::Cancelled, Some(ErrorInfo::cancelled()));
        if let Some(session) = session {
            let tid = r.transfer_id.clone();
            tokio::spawn(async move { session.cancel(Some("cancelled"), Some(&tid)).await });
        }
        true
    }

    // ── The receive sink (called by sessions) ───────────────────────────

    async fn open_file(&self, file: &FileMeta, offset: u64, ctx: &SinkContext) -> std::result::Result<Box<dyn SinkWriter>, String> {
        let r = self.receive(&ctx.peer_key, &ctx.transfer_id).ok_or("transfer not accepted")?;
        let (dest_dir, name, existing, final_path) = {
            let files = r.files.lock().unwrap();
            let f = files.get(&file.id).ok_or("file not accepted")?;
            (f.dest_dir.clone(), f.rel.file_name().to_string(), f.part_path.clone(), f.final_path.clone())
        };
        // Resuming a file that was already committed: nothing to write.
        if let Some(path) = final_path {
            if offset != file.size {
                return Err("file already received".into());
            }
            return Ok(Box::new(DoneWriter { path }));
        }
        let root = r.save_dir.clone();
        let dir = dest_dir.clone();
        let inside = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
            std::fs::create_dir_all(&dir)?;
            // Defense in depth: the resolved folder must stay inside the save folder.
            Ok(std::fs::canonicalize(&dir)?.starts_with(std::fs::canonicalize(&root)?))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not create folder: {e}"))?;
        if !inside {
            return Err("invalid path".into());
        }
        let part_path = match (offset, existing) {
            (o, Some(p)) if o > 0 => p,
            (0, Some(p)) => {
                let _ = tokio::fs::remove_file(&p).await;
                dest_dir.join(format!("{name}.{}{PART_SUFFIX}", &random_token()[..6]))
            }
            (0, None) => dest_dir.join(format!("{name}.{}{PART_SUFFIX}", &random_token()[..6])),
            _ => return Err("no partial data to resume".into()),
        };
        if file.size > offset
            && let Ok(available) = space::available_space(&dest_dir)
            && file.size - offset > available
        {
            return Err(ErrorInfo::disk_full(file.size - offset, available).message);
        }
        let writer = PartWriter::start(PartSpec { path: part_path.clone(), offset, expected_len: file.size, checkpoint_every: None })
            .await
            .map_err(|e| format!("could not write file: {e}"))?;
        if let Some(f) = r.files.lock().unwrap().get_mut(&file.id) {
            f.part_path = Some(part_path.clone());
        }
        // Tagged before any byte is visible; the stream moves with the rename.
        let tag = part_path.clone();
        let _ = tokio::task::spawn_blocking(move || motw::mark_from_network(&tag)).await;
        r.entry.set_file_state(&file.id, FileState::Transferring, None);
        r.entry.reset_file_progress(&file.id, offset);
        let progress = r.entry.file_progress(&file.id);
        Ok(Box::new(EngineWriter {
            writer: Some(writer),
            receive: r,
            file_id: file.id.clone(),
            name,
            dest_dir,
            part_path,
            size: file.size,
            modified: file.modified,
            progress,
            shared: self.shared.clone(),
        }))
    }

    async fn hash_prefix(&self, file: &FileMeta, offset: u64, ctx: &SinkContext) -> std::result::Result<Sha256, String> {
        let r = self.receive(&ctx.peer_key, &ctx.transfer_id).ok_or("transfer not accepted")?;
        let path = {
            let files = r.files.lock().unwrap();
            let f = files.get(&file.id).ok_or("file not accepted")?;
            f.final_path.clone().or_else(|| f.part_path.clone()).ok_or("nothing stored")?
        };
        tokio::task::spawn_blocking(move || -> std::io::Result<Sha256> {
            let mut f = std::fs::File::open(&path)?;
            let mut h = Sha256::new();
            let mut left = offset;
            let mut buf = vec![0u8; 1 << 20];
            while left > 0 {
                let n = f.read(&mut buf[..(left.min(1 << 20)) as usize])?;
                if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "stored prefix is shorter than the resume offset"));
                }
                h.update(&buf[..n]);
                left -= n as u64;
            }
            Ok(h)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }
}

/// Short-lived TURN credentials for one connection, when the server offers them.
async fn turn_servers(signaling: &SignalingClient) -> Vec<webrtc::ice_transport::ice_server::RTCIceServer> {
    match signaling.fetch_turn().await {
        Ok(Some(turn)) => turn
            .ice_servers
            .into_iter()
            .map(|s| webrtc::ice_transport::ice_server::RTCIceServer { urls: s.urls, username: s.username, credential: s.credential })
            .collect(),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::debug!("no TURN credentials: {e}");
            Vec::new()
        }
    }
}

fn ice_servers(stun: &[String]) -> Vec<webrtc::ice_transport::ice_server::RTCIceServer> {
    let urls: Vec<String> =
        stun.iter().map(|s| s.trim().to_string()).filter(|s| s.starts_with("stun:") || s.starts_with("stuns:")).collect();
    if urls.is_empty() {
        return Vec::new();
    }
    vec![webrtc::ice_transport::ice_server::RTCIceServer { urls, ..Default::default() }]
}

// ── Sink and writers ──────────────────────────────────────────────────────

struct EngineSink {
    manager: Weak<RtcManager>,
}

#[async_trait]
impl FileSink for EngineSink {
    async fn open(&self, file: &FileMeta, offset: u64, ctx: &SinkContext) -> std::result::Result<Box<dyn SinkWriter>, String> {
        self.manager.upgrade().ok_or("engine stopped")?.open_file(file, offset, ctx).await
    }

    fn can_resume(&self) -> bool {
        true
    }

    async fn hash_prefix(&self, file: &FileMeta, offset: u64, ctx: &SinkContext) -> std::result::Result<Sha256, String> {
        self.manager.upgrade().ok_or("engine stopped")?.hash_prefix(file, offset, ctx).await
    }
}

/// A file already committed in an earlier session of the same transfer.
struct DoneWriter {
    path: PathBuf,
}

#[async_trait]
impl SinkWriter for DoneWriter {
    async fn write(&mut self, _chunk: Bytes) -> std::result::Result<(), String> {
        Err(format!("{} is already complete", self.path.display()))
    }
    async fn close(self: Box<Self>) -> std::result::Result<(), String> {
        Ok(())
    }
    async fn abort(self: Box<Self>, _reason: AbortReason) {}
}

struct EngineWriter {
    writer: Option<PartWriter>,
    receive: Arc<Receive>,
    file_id: String,
    name: String,
    dest_dir: PathBuf,
    part_path: PathBuf,
    size: u64,
    modified: Option<i64>,
    progress: Option<Arc<std::sync::atomic::AtomicU64>>,
    shared: Arc<Shared>,
}

#[async_trait]
impl SinkWriter for EngineWriter {
    async fn write(&mut self, chunk: Bytes) -> std::result::Result<(), String> {
        let n = chunk.len() as u64;
        let w = self.writer.as_mut().ok_or("writer closed")?;
        w.write(chunk).await.map_err(|e| e.to_string())?;
        if let Some(p) = &self.progress {
            p.fetch_add(n, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn close(mut self: Box<Self>) -> std::result::Result<(), String> {
        let writer = self.writer.take().ok_or("writer closed")?;
        let outcome = writer.finish().await.map_err(|e| e.to_string())?;
        if outcome.len != self.size {
            return Err("size mismatch on disk".into());
        }
        self.receive.entry.set_file_state(&self.file_id, FileState::Verifying, None);
        let (part, dir, name, size) = (self.part_path.clone(), self.dest_dir.clone(), self.name.clone(), self.size);
        let modified = self.modified.filter(|m| *m >= 0).map(|m| std::time::UNIX_EPOCH + Duration::from_millis(m as u64));
        // Durable, then visible under its final (never colliding) name.
        let final_path = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
            if size >= SYNC_THRESHOLD {
                part::sync_file(&part)?;
            }
            motw::mark_from_network(&part);
            let final_path = unique::rename_unique(&part, &dir, &name)?;
            part::set_times(&final_path, modified, None);
            Ok(final_path)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not save file: {e}"))?;
        let r = &self.receive;
        let (rel, mime) = {
            let mut files = r.files.lock().unwrap();
            let f = files.get_mut(&self.file_id);
            match f {
                Some(f) => {
                    f.final_path = Some(final_path.clone());
                    (f.rel.display(), f.meta.mime.clone())
                }
                None => (self.name.clone(), String::new()),
            }
        };
        if self.shared.settings.get().history_enabled {
            let entry = NewHistoryEntry {
                transfer_id: r.entry.id.clone(),
                direction: Direction::Receive,
                peer_id: r.peer.id.clone(),
                peer_alias: r.peer.alias.clone(),
                peer_kind: r.peer.device_kind,
                kind: HistoryKind::File,
                name: rel,
                size: self.size,
                mime,
                path: Some(final_path.display().to_string()),
                text: None,
                timestamp_ms: now_ms(),
                status: HistoryStatus::Completed,
                // The session compared SHA-256 end to end before committing.
                verified: true,
            };
            if let Ok(e) = self.shared.db.add_history(&entry) {
                self.shared.events.emit(EngineEvent::HistoryAdded { entry: e });
            }
        }
        r.entry.update_file(&self.file_id, |f| {
            f.state = FileState::Done;
            f.error = None;
            f.path = Some(final_path.display().to_string());
        });
        r.entry.reset_file_progress(&self.file_id, self.size);
        Ok(())
    }

    async fn abort(mut self: Box<Self>, reason: AbortReason) {
        let len = match self.writer.take() {
            Some(w) => w.finish().await.map(|o| o.len).unwrap_or(0),
            None => 0,
        };
        match reason {
            // Resumable: keep what is on disk.
            AbortReason::Closed | AbortReason::Timeout => {
                self.receive.entry.reset_file_progress(&self.file_id, len);
                self.receive.entry.set_file_state(&self.file_id, FileState::Pending, None);
            }
            _ => {
                let _ = tokio::fs::remove_file(&self.part_path).await;
                if let Some(f) = self.receive.files.lock().unwrap().get_mut(&self.file_id) {
                    f.part_path = None;
                }
                let state = if reason == AbortReason::Cancelled { FileState::Cancelled } else { FileState::Failed };
                let error = match reason {
                    AbortReason::Integrity => Some(ErrorInfo::checksum_mismatch(&self.name)),
                    AbortReason::Overrun => Some(ErrorInfo::new("protocol", "The sender sent more data than announced.")),
                    AbortReason::Error => Some(ErrorInfo::new("file_failed", "The file couldn't be written.")),
                    _ => None,
                };
                self.receive.entry.set_file_state(&self.file_id, state, error);
            }
        }
    }
}

/// Reads a file on disk in blocks (one handle, positioned reads).
struct PathSource {
    path: PathBuf,
    file: Arc<Mutex<Option<std::fs::File>>>,
}

impl PathSource {
    fn new(path: PathBuf) -> Self {
        PathSource { path, file: Arc::new(Mutex::new(None)) }
    }
}

#[async_trait]
impl FileSource for PathSource {
    async fn read(&self, start: u64, end: u64) -> std::io::Result<Bytes> {
        let (path, file) = (self.path.clone(), self.file.clone());
        tokio::task::spawn_blocking(move || -> std::io::Result<Bytes> {
            let mut guard = file.lock().unwrap();
            if guard.is_none() {
                *guard = Some(std::fs::File::open(&path)?);
            }
            let Some(f) = guard.as_mut() else { return Err(std::io::Error::other("file not open")) };
            f.seek(SeekFrom::Start(start))?;
            let mut buf = vec![0u8; (end - start) as usize];
            f.read_exact(&mut buf)?;
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(key: &str) -> PendingRequest {
        PendingRequest { key: key.into(), transfer_id: "t".into(), decision: None }
    }

    #[test]
    fn pending_offers_are_capped_per_peer_and_in_total() {
        let mut st = State::default();
        for i in 0..MAX_PENDING_PER_PEER {
            assert!(st.add_pending(&format!("a{i}"), request("A")));
        }
        assert!(!st.add_pending("a-extra", request("A")), "per-peer cap");
        for n in MAX_PENDING_PER_PEER..MAX_PENDING {
            assert!(st.add_pending(&format!("p{n}"), request(&format!("P{}", n / MAX_PENDING_PER_PEER))));
        }
        assert!(!st.add_pending("fresh", request("NEW")), "global cap");
        st.pending.remove("a0");
        assert!(st.add_pending("fresh", request("NEW")), "a freed place is reused");
        assert_eq!(st.pending.len(), MAX_PENDING);
    }

    #[test]
    fn messages_are_rate_limited_per_peer_and_in_total() {
        let mut st = State::default();
        let start = Instant::now();
        for _ in 0..MAX_MESSAGES_PER_PEER {
            assert!(st.admit_message("A", start));
        }
        assert!(!st.admit_message("A", start), "per-peer limit");
        let mut n = MAX_MESSAGES_PER_PEER;
        let mut peer = 0;
        while n < MAX_MESSAGES {
            assert!(st.admit_message(&format!("P{}", peer / MAX_MESSAGES_PER_PEER), start), "{n}");
            peer += 1;
            n += 1;
        }
        assert!(!st.admit_message("NEW", start), "global limit");
        // A minute later the window has moved on.
        let later = start + MESSAGE_WINDOW;
        assert!(st.admit_message("A", later));
        assert!(st.admit_message("NEW", later));
        assert_eq!(st.message_times.values().map(VecDeque::len).sum::<usize>(), 2, "old times are forgotten");
    }
}
