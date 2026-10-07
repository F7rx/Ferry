//! Signaling ↔ WebRTC glue (the reference's `peer.ts`), on webrtc-rs.
//!
//! The offerer creates the ordered, reliable `ferry/1` data channel; the
//! answerer answers. ICE trickles both ways: outgoing candidates are held back
//! until our OFFER/ANSWER is on the wire, remote ones are queued until the
//! remote description is set. Chrome's mDNS (`<uuid>.local`) host candidates
//! are resolved by webrtc-rs's mDNS querier; peer-reflexive candidates cover
//! the rest. Once the channel is open, both SDP fingerprints and the remote
//! `max-message-size` go to a [`PeerSession`], and `connect`/`accept` resolve
//! only when the peer is authenticated.

use super::identity::RtcIdentity;
use super::protocol::*;
use super::session::*;
use super::signaling::{ClientInfo, IceCandidate, SignalingClient, SignalingEvent};
use super::transcript::{Role, extract_fingerprint};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data::data_channel::DataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

const MAX_QUEUED_ICE: usize = 64;
/// Read buffer: the largest frame a peer may send (256 KiB) plus slack.
const READ_BUFFER: usize = MAX_BINARY_FRAME + 4096;

// ── The data channel as a session transport ───────────────────────────────

pub struct WebRtcChannel {
    dc: Arc<DataChannel>,
    pc: Arc<RTCPeerConnection>,
    low: Arc<Notify>,
    buf: tokio::sync::Mutex<Vec<u8>>,
    closed: AtomicBool,
}

impl WebRtcChannel {
    fn new(dc: Arc<DataChannel>, pc: Arc<RTCPeerConnection>) -> Arc<Self> {
        let low = Arc::new(Notify::new());
        dc.set_buffered_amount_low_threshold(BUFFER_LOW_WATER);
        let notify = low.clone();
        dc.on_buffered_amount_low(Box::new(move || {
            notify.notify_waiters();
            Box::pin(async {})
        }));
        Arc::new(WebRtcChannel { dc, pc, low, buf: tokio::sync::Mutex::new(vec![0; READ_BUFFER]), closed: AtomicBool::new(false) })
    }
}

#[async_trait]
impl Transport for WebRtcChannel {
    async fn send(&self, frame: Frame) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("closed".into());
        }
        let (data, is_string) = match frame {
            Frame::Text(t) => (Bytes::from(t), true),
            Frame::Binary(b) => (b, false),
        };
        self.dc.write_data_channel(&data, is_string).await.map(|_| ()).map_err(|e| e.to_string())
    }

    fn buffered_amount(&self) -> usize {
        self.dc.buffered_amount()
    }

    fn buffered_low(&self) -> &Notify {
        &self.low
    }

    async fn recv(&self) -> Option<Frame> {
        let mut buf = self.buf.lock().await;
        match self.dc.read_data_channel(&mut buf).await {
            Ok((0, false)) | Err(_) => None,
            Ok((n, true)) => Some(Frame::Text(String::from_utf8_lossy(&buf[..n]).into_owned())),
            Ok((n, false)) => Some(Frame::Binary(Bytes::copy_from_slice(&buf[..n]))),
        }
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Let the last frames (an `error` before a fatal close) leave first.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = self.dc.close().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = self.pc.close().await;
    }
}

// ── Connector ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ConnectorConfig {
    pub identity: Arc<RtcIdentity>,
    pub device: DeviceInfo,
    pub sink: Option<Arc<dyn FileSink>>,
    /// STUN/TURN servers (`stun:`/`turn:` URLs).
    pub ice_servers: Vec<RTCIceServer>,
    /// Offer → authenticated session deadline.
    pub connect_timeout: Duration,
    /// Also gather loopback candidates (same-machine tests without a network).
    pub include_loopback: bool,
    /// Session tuning applied to every session.
    pub tune: Arc<dyn Fn(&mut SessionOptions) + Send + Sync>,
}

impl ConnectorConfig {
    pub fn new(identity: Arc<RtcIdentity>, device: DeviceInfo) -> Self {
        ConnectorConfig {
            identity,
            device,
            sink: None,
            ice_servers: Vec::new(),
            connect_timeout: Duration::from_secs(30),
            include_loopback: false,
            tune: Arc::new(|_| {}),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// Secret of a link/QR room; both sides must prove it.
    pub room_secret: Option<Vec<u8>>,
    /// Expected identity key of the peer (base64url).
    pub expected_peer_key: Option<String>,
    /// More ICE servers for this connection (short-lived TURN credentials).
    pub ice_servers: Vec<RTCIceServer>,
}

/// A peer wants to connect: [`Connector::accept`] or [`Connector::reject`] it.
#[derive(Clone, Debug)]
pub struct IncomingConnection {
    pub peer: ClientInfo,
    pub session_id: String,
    sdp: String,
}

/// An established session plus how it is connected.
pub struct Connected {
    pub session: PeerSession,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    pub peer_client_id: String,
    pub role: Role,
    /// Through a TURN relay; `None` when the selected candidate pair is unknown.
    pub relayed: Option<bool>,
    /// The remote candidate's address (`ip:port`), when known.
    pub remote_address: Option<String>,
}

/// An attempt's failure signal and reason.
struct Failed(CancellationToken, Arc<Mutex<Option<RtcError>>>);

struct Entry {
    pc: Option<Arc<RTCPeerConnection>>,
    remote_set: bool,
    ice_in: Vec<Option<IceCandidate>>,
    answer: Option<oneshot::Sender<String>>,
    failed: CancellationToken,
    failure: Arc<Mutex<Option<RtcError>>>,
    session: Option<PeerSession>,
    mid: String,
}

pub struct Connector {
    signaling: SignalingClient,
    config: Mutex<ConnectorConfig>,
    entries: Mutex<HashMap<(String, String), Entry>>,
}

fn key(peer: &str, session: &str) -> (String, String) {
    (peer.to_string(), session.to_string())
}

fn webrtc_err(e: impl std::fmt::Display) -> RtcError {
    RtcError::new("webrtc", e.to_string())
}

impl Connector {
    pub fn new(signaling: SignalingClient, config: ConnectorConfig) -> Arc<Self> {
        Arc::new(Connector { signaling, config: Mutex::new(config), entries: Mutex::new(HashMap::new()) })
    }

    /// The device info sent in `hello` by sessions created from now on.
    pub fn set_device(&self, device: DeviceInfo) {
        self.config.lock().unwrap().device = device;
    }

    pub fn set_ice_servers(&self, servers: Vec<RTCIceServer>) {
        self.config.lock().unwrap().ice_servers = servers;
    }

    pub fn signaling(&self) -> &SignalingClient {
        &self.signaling
    }

    /// Routes signaling relays (OFFER/ANSWER/ICE/CANCEL) to attempts.
    /// An OFFER for a new session is returned for the caller to decide.
    pub fn handle_signal(self: &Arc<Self>, event: &SignalingEvent) -> Option<IncomingConnection> {
        match event {
            SignalingEvent::Offer { peer, session_id, sdp } => {
                let mut entries = self.entries.lock().unwrap();
                let k = key(&peer.id, session_id);
                if entries.contains_key(&k) {
                    return None; // duplicate
                }
                entries.insert(k, Entry::new());
                Some(IncomingConnection { peer: peer.clone(), session_id: session_id.clone(), sdp: sdp.clone() })
            }
            SignalingEvent::Answer { peer, session_id, sdp } => {
                if let Some(tx) = self.entries.lock().unwrap().get_mut(&key(&peer.id, session_id)).and_then(|e| e.answer.take()) {
                    let _ = tx.send(sdp.clone());
                }
                None
            }
            SignalingEvent::Ice { peer, session_id, candidate } => {
                let pc = {
                    let mut entries = self.entries.lock().unwrap();
                    let entry = entries.get_mut(&key(&peer.id, session_id))?;
                    match (&entry.pc, entry.remote_set) {
                        (Some(pc), true) => Some((pc.clone(), entry.mid.clone())),
                        _ => {
                            if entry.ice_in.len() < MAX_QUEUED_ICE {
                                entry.ice_in.push(candidate.clone());
                            }
                            None
                        }
                    }
                };
                if let (Some((pc, mid)), Some(c)) = (pc, candidate.clone()) {
                    tokio::spawn(async move { add_ice(&pc, &c, &mid).await });
                }
                None
            }
            SignalingEvent::Cancel { peer, session_id } => {
                self.fail(&peer.id, session_id, RtcError::new("cancelled", "the peer cancelled"), false);
                None
            }
            _ => None,
        }
    }

    /// Declines an incoming connection (tells the peer via `CANCEL`).
    pub fn reject(&self, incoming: &IncomingConnection) {
        self.fail(&incoming.peer.id, &incoming.session_id, RtcError::new("rejected", "connection rejected"), true);
    }

    /// Ends an attempt (or closes its session); `notify` sends `CANCEL` unless established.
    fn fail(&self, peer: &str, session_id: &str, err: RtcError, notify: bool) {
        let entry = self.entries.lock().unwrap().remove(&key(peer, session_id));
        let Some(mut entry) = entry else { return };
        let established = entry.session.as_ref().is_some_and(|s| s.state() == SessionState::Ready);
        *entry.failure.lock().unwrap() = Some(err.clone());
        entry.failed.cancel();
        if notify && !established {
            let signaling = self.signaling.clone();
            let (p, s) = (peer.to_string(), session_id.to_string());
            tokio::spawn(async move {
                let _ = signaling.send_cancel(&p, &s).await;
            });
        }
        if let Some(session) = entry.session.take() {
            session.close(Some(&err.message));
        } else if let Some(pc) = entry.pc.take() {
            tokio::spawn(async move {
                let _ = pc.close().await;
            });
        }
    }

    /// Stops every pending or open session.
    pub fn dispose(&self) {
        let keys: Vec<(String, String)> = self.entries.lock().unwrap().keys().cloned().collect();
        for (p, s) in keys {
            self.fail(&p, &s, RtcError::new("closed", "connector disposed"), true);
        }
    }

    async fn new_pc(
        self: &Arc<Self>,
        peer: &str,
        session_id: &str,
        extra_ice: &[RTCIceServer],
    ) -> Result<(Arc<RTCPeerConnection>, mpsc::UnboundedSender<Option<IceCandidate>>, oneshot::Sender<String>), RtcError> {
        let config = self.config.lock().unwrap().clone();
        let mut ice_servers = config.ice_servers.clone();
        ice_servers.extend(extra_ice.iter().cloned());
        let mut se = SettingEngine::default();
        se.detach_data_channels();
        se.set_include_loopback_candidate(config.include_loopback);
        // Link-local addresses can't reach a peer and only produce bind errors.
        se.set_ip_filter(Box::new(|ip: std::net::IpAddr| match ip {
            std::net::IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_unspecified(),
            // Global unicast (2000::/3) and unique local (fc00::/7) only.
            std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xe000) == 0x2000 || (v6.segments()[0] & 0xfe00) == 0xfc00,
        }));
        let api = APIBuilder::new().with_setting_engine(se).build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration { ice_servers, ..Default::default() }).await.map_err(webrtc_err)?);
        // Outgoing candidates wait for our OFFER/ANSWER (the gate carries the m-line's mid).
        let (ice_tx, mut ice_rx) = mpsc::unbounded_channel::<Option<IceCandidate>>();
        let (gate_tx, gate_rx) = oneshot::channel::<String>();
        let signaling = self.signaling.clone();
        let (p, s) = (peer.to_string(), session_id.to_string());
        tokio::spawn(async move {
            let Ok(mid) = gate_rx.await else { return };
            while let Some(c) = ice_rx.recv().await {
                let c = c.map(|mut c| {
                    c.sdp_mid = Some(Some(mid.clone()));
                    c
                });
                let _ = signaling.send_ice(&p, &s, c.as_ref()).await;
            }
        });
        let tx = ice_tx.clone();
        pc.on_ice_candidate(Box::new(move |c| {
            let candidate = c.and_then(|c| c.to_json().ok()).map(|init| IceCandidate {
                candidate: init.candidate,
                sdp_mid: None,
                sdp_m_line_index: Some(Some(0)),
                username_fragment: None,
            });
            let _ = tx.send(candidate);
            Box::pin(async {})
        }));
        let weak = Arc::downgrade(self);
        let (p, s) = (peer.to_string(), session_id.to_string());
        pc.on_peer_connection_state_change(Box::new(move |state| {
            if state == RTCPeerConnectionState::Failed
                && let Some(this) = weak.upgrade()
            {
                this.fail(&p, &s, RtcError::new("webrtc", "WebRTC connection failed"), true);
            }
            Box::pin(async {})
        }));
        if let Some(entry) = self.entries.lock().unwrap().get_mut(&key(peer, session_id)) {
            entry.pc = Some(pc.clone());
        } else {
            let _ = pc.close().await;
            return Err(RtcError::new("closed", "connection attempt ended"));
        }
        Ok((pc, ice_tx, gate_tx))
    }

    /// Marks the remote description set and adds the queued candidates.
    async fn flush_ice(&self, peer: &str, session_id: &str, pc: &RTCPeerConnection, mid: &str) {
        let queued = {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(&key(peer, session_id)) else { return };
            entry.remote_set = true;
            entry.mid = mid.to_string();
            std::mem::take(&mut entry.ice_in)
        };
        for c in queued.into_iter().flatten() {
            add_ice(pc, &c, mid).await;
        }
    }

    /// Runs `fut` unless the attempt fails (peer CANCEL, WebRTC failure) first.
    async fn race<T>(
        &self,
        failed: &Failed,
        _peer: &str,
        _session_id: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T, RtcError> {
        tokio::select! {
            v = fut => Ok(v),
            _ = failed.0.cancelled() => Err(failed.1.lock().unwrap().clone().unwrap_or_else(|| RtcError::new("cancelled", "the connection attempt ended"))),
        }
    }

    /// Connects to a signaling peer as the offerer; resolves once authenticated.
    pub async fn connect(self: &Arc<Self>, target: &str, options: ConnectOptions) -> Result<Connected, RtcError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let failed = {
            let mut entries = self.entries.lock().unwrap();
            let entry = Entry::new();
            let failed = Failed(entry.failed.clone(), entry.failure.clone());
            entries.insert(key(target, &session_id), entry);
            failed
        };
        let timeout = self.config.lock().unwrap().connect_timeout;
        let attempt = self.offer(target, &session_id, &options, &failed);
        let result = match tokio::time::timeout(timeout, attempt).await {
            Ok(r) => r,
            Err(_) => Err(RtcError::new("timeout", "connection timed out")),
        };
        if let Err(err) = &result {
            self.fail(target, &session_id, err.clone(), true);
        }
        result
    }

    async fn offer(
        self: &Arc<Self>,
        target: &str,
        session_id: &str,
        options: &ConnectOptions,
        failed: &Failed,
    ) -> Result<Connected, RtcError> {
        let (pc, _ice, gate) = self.new_pc(target, session_id, &options.ice_servers).await?;
        let (answer_tx, answer_rx) = oneshot::channel();
        if let Some(e) = self.entries.lock().unwrap().get_mut(&key(target, session_id)) {
            e.answer = Some(answer_tx);
        }
        let dc = pc
            .create_data_channel(DC_LABEL, Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() }))
            .await
            .map_err(webrtc_err)?;
        let (open_tx, open_rx) = oneshot::channel::<Arc<DataChannel>>();
        let open_tx = Mutex::new(Some(open_tx));
        let dc2 = dc.clone();
        dc.on_open(Box::new(move || {
            let tx = open_tx.lock().unwrap().take();
            Box::pin(async move {
                if let (Ok(raw), Some(tx)) = (dc2.detach().await, tx) {
                    let _ = tx.send(raw);
                }
            })
        }));
        let offer = self.race(failed, target, session_id, pc.create_offer(None)).await?.map_err(webrtc_err)?;
        self.race(failed, target, session_id, pc.set_local_description(offer)).await?.map_err(webrtc_err)?;
        let local_sdp = pc.local_description().await.map(|d| d.sdp).ok_or_else(|| RtcError::new("webrtc", "no local description"))?;
        let mid = sdp_mid(&local_sdp);
        self.race(failed, target, session_id, self.signaling.send_offer(target, session_id, &local_sdp)).await??;
        let _ = gate.send(mid.clone());
        let remote_sdp = self.race(failed, target, session_id, answer_rx).await?.map_err(|_| RtcError::new("closed", "no answer"))?;
        let answer = RTCSessionDescription::answer(remote_sdp.clone()).map_err(webrtc_err)?;
        self.race(failed, target, session_id, pc.set_remote_description(answer)).await?.map_err(webrtc_err)?;
        self.flush_ice(target, session_id, &pc, &mid).await;
        let raw =
            self.race(failed, target, session_id, open_rx).await?.map_err(|_| RtcError::new("webrtc", "data channel did not open"))?;
        self.establish(target, session_id, Role::Offerer, pc, raw, &local_sdp, &remote_sdp, options, failed).await
    }

    /// Answers an incoming connection; resolves once the peer is authenticated.
    pub async fn accept(self: &Arc<Self>, incoming: IncomingConnection, options: ConnectOptions) -> Result<Connected, RtcError> {
        let (peer, session_id) = (incoming.peer.id.clone(), incoming.session_id.clone());
        let Some(failed) = self.entries.lock().unwrap().get(&key(&peer, &session_id)).map(|e| Failed(e.failed.clone(), e.failure.clone()))
        else {
            return Err(RtcError::new("invalid-state", "connection request is no longer pending"));
        };
        let timeout = self.config.lock().unwrap().connect_timeout;
        let result = match tokio::time::timeout(timeout, self.answer(&incoming, &options, &failed)).await {
            Ok(r) => r,
            Err(_) => Err(RtcError::new("timeout", "connection timed out")),
        };
        if let Err(err) = &result {
            self.fail(&peer, &session_id, err.clone(), true);
        }
        result
    }

    async fn answer(
        self: &Arc<Self>,
        incoming: &IncomingConnection,
        options: &ConnectOptions,
        failed: &Failed,
    ) -> Result<Connected, RtcError> {
        let (peer, session_id) = (incoming.peer.id.as_str(), incoming.session_id.as_str());
        let (pc, _ice, gate) = self.new_pc(peer, session_id, &options.ice_servers).await?;
        let (open_tx, open_rx) = oneshot::channel::<Arc<DataChannel>>();
        let open_tx = Arc::new(Mutex::new(Some(open_tx)));
        pc.on_data_channel(Box::new(move |dc| {
            let open_tx = open_tx.clone();
            Box::pin(async move {
                let reliable = dc.ordered() && dc.max_retransmits().is_none() && dc.max_packet_lifetime().is_none();
                if dc.label() != DC_LABEL || !reliable || open_tx.lock().unwrap().is_none() {
                    let _ = dc.close().await;
                    return;
                }
                let dc2 = dc.clone();
                dc.on_open(Box::new(move || {
                    Box::pin(async move {
                        let tx = open_tx.lock().unwrap().take();
                        if let (Ok(raw), Some(tx)) = (dc2.detach().await, tx) {
                            let _ = tx.send(raw);
                        }
                    })
                }));
            })
        }));
        let offer = RTCSessionDescription::offer(incoming.sdp.clone()).map_err(webrtc_err)?;
        self.race(failed, peer, session_id, pc.set_remote_description(offer)).await?.map_err(webrtc_err)?;
        let mid = sdp_mid(&incoming.sdp);
        self.flush_ice(peer, session_id, &pc, &mid).await;
        let answer = self.race(failed, peer, session_id, pc.create_answer(None)).await?.map_err(webrtc_err)?;
        self.race(failed, peer, session_id, pc.set_local_description(answer)).await?.map_err(webrtc_err)?;
        let local_sdp = pc.local_description().await.map(|d| d.sdp).ok_or_else(|| RtcError::new("webrtc", "no local description"))?;
        self.race(failed, peer, session_id, self.signaling.send_answer(peer, session_id, &local_sdp)).await??;
        let _ = gate.send(sdp_mid(&local_sdp));
        let raw = self.race(failed, peer, session_id, open_rx).await?.map_err(|_| RtcError::new("webrtc", "data channel did not open"))?;
        self.establish(peer, session_id, Role::Answerer, pc, raw, &local_sdp, &incoming.sdp, options, failed).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn establish(
        self: &Arc<Self>,
        peer: &str,
        session_id: &str,
        role: Role,
        pc: Arc<RTCPeerConnection>,
        raw: Arc<DataChannel>,
        local_sdp: &str,
        remote_sdp: &str,
        options: &ConnectOptions,
        failed: &Failed,
    ) -> Result<Connected, RtcError> {
        let (Some(local_fp), Some(remote_fp)) = (extract_fingerprint(local_sdp), extract_fingerprint(remote_sdp)) else {
            return Err(RtcError::new("webrtc", "SDP without a DTLS fingerprint"));
        };
        let config = self.config.lock().unwrap().clone();
        let mut opts = SessionOptions::new(role, session_id, local_fp, remote_fp, config.identity.clone(), config.device.clone());
        opts.sink = config.sink.clone();
        opts.max_message_size = Some(parse_max_message_size(remote_sdp));
        opts.room_secret = options.room_secret.clone();
        opts.expected_peer_key = options.expected_peer_key.clone();
        (config.tune)(&mut opts);
        let transport = WebRtcChannel::new(raw, pc.clone());
        let (session, events) = PeerSession::start(opts, transport);
        if let Some(entry) = self.entries.lock().unwrap().get_mut(&key(peer, session_id)) {
            entry.session = Some(session.clone());
        } else {
            session.close(Some("connection attempt ended"));
            return Err(RtcError::new("closed", "connection attempt ended"));
        }
        // The entry lives as long as the session (late candidates, CANCEL).
        let weak = Arc::downgrade(self);
        let (p, s, watched) = (peer.to_string(), session_id.to_string(), session.clone());
        tokio::spawn(async move {
            watched.closed().await;
            if let Some(this) = weak.upgrade() {
                this.entries.lock().unwrap().remove(&key(&p, &s));
            }
        });
        self.race(failed, peer, session_id, session.ready()).await??;
        let (relayed, remote_address) = selected_pair(&pc).await;
        Ok(Connected { session, events, peer_client_id: peer.to_string(), role, relayed, remote_address })
    }
}

impl Entry {
    fn new() -> Self {
        Entry {
            pc: None,
            remote_set: false,
            ice_in: Vec::new(),
            answer: None,
            failed: CancellationToken::new(),
            failure: Arc::new(Mutex::new(None)),
            session: None,
            mid: "0".into(),
        }
    }
}

/// The first `a=mid:` of an SDP (data-channel SDPs have one m-line).
fn sdp_mid(sdp: &str) -> String {
    sdp.lines().find_map(|l| l.trim_end().strip_prefix("a=mid:").map(str::to_string)).unwrap_or_else(|| "0".into())
}

async fn add_ice(pc: &RTCPeerConnection, c: &IceCandidate, mid: &str) {
    if c.candidate.is_empty() {
        return; // end-of-candidates marker
    }
    let init = RTCIceCandidateInit {
        candidate: c.candidate.clone(),
        sdp_mid: Some(c.sdp_mid.clone().flatten().unwrap_or_else(|| mid.to_string())),
        sdp_mline_index: c.sdp_m_line_index.flatten().and_then(|i| u16::try_from(i).ok()).or(Some(0)),
        username_fragment: c.username_fragment.clone().flatten(),
    };
    if let Err(e) = pc.add_ice_candidate(init).await {
        tracing::debug!("ignoring ICE candidate {}: {e}", c.candidate);
    }
}

/// Whether the selected candidate pair goes through a TURN relay (`None` when
/// there is no selected pair or its candidate types can't be read), and the
/// remote address.
async fn selected_pair(pc: &RTCPeerConnection) -> (Option<bool>, Option<String>) {
    let pair = pc.sctp().transport().ice_transport().get_selected_candidate_pair().await;
    match pair {
        Some(pair) => route_of(&pair.to_string()),
        None => (None, None),
    }
}

/// [`selected_pair`] from the pair's display text:
/// "(local) <proto> <type> <addr>:<port> <-> (remote) <proto> <type> <addr>:<port>".
fn route_of(text: &str) -> (Option<bool>, Option<String>) {
    let (local, remote) = text.split_once(" <-> ").unwrap_or((text, ""));
    let typ = |s: &str| s.split_whitespace().nth(2).unwrap_or("").to_string();
    let (local_type, remote_type) = (typ(local), typ(remote));
    let relayed = (!local_type.is_empty() && !remote_type.is_empty()).then(|| local_type == "relay" || remote_type == "relay");
    // "<addr>:<port>" (a related address may follow without a separator).
    let address = remote.split_whitespace().nth(3).and_then(|a| {
        let colon = a.rfind(':')?;
        let port: String = a[colon + 1..].chars().take_while(|c| c.is_ascii_digit()).take(5).collect();
        (!port.is_empty()).then(|| format!("{}:{port}", &a[..colon]))
    });
    (relayed, address)
}

#[cfg(test)]
mod tests {
    use super::route_of;

    #[test]
    fn route_is_known_only_from_both_candidate_types() {
        let direct = "(local) udp host 192.168.1.2:5000 <-> (remote) udp srflx 203.0.113.9:6000";
        assert_eq!(route_of(direct), (Some(false), Some("203.0.113.9:6000".into())));
        let relayed = "(local) udp relay 198.51.100.1:7000 related 192.168.1.2:5000 <-> (remote) udp host 10.0.0.3:9";
        assert_eq!(route_of(relayed).0, Some(true));
        assert_eq!(route_of("(local) udp host 1.2.3.4:5 <-> (remote) udp relay [2001:db8::1]:3478").0, Some(true));
        assert_eq!(route_of(""), (None, None), "nothing to read: unknown, not direct");
        assert_eq!(route_of("(local) udp host 1.2.3.4:5").0, None);
    }
}
