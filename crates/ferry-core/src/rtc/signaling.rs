//! Signaling client for `ferry-signal` / LocalSend's `/v1/ws` (05-protocol.md
//! §5.1), wire-compatible with `apps/app/src/lib/rtc/signaling.ts`.
//!
//! `wss://host/v1/ws?d=<base64url-nopad(JSON client info)>`, then JSON frames
//! `{"type":"SCREAMING_SNAKE", ...}` both ways. SDPs travel as
//! base64url-nopad(zlib(SDP)); ICE candidates as flat `RTCIceCandidateInit`
//! objects. Frames are processed strictly in order; malformed inbound frames
//! are ignored. The client reconnects with jittered exponential backoff, keeps
//! the connection alive (`PING` every 25 s, reconnect after 65 s of silence)
//! and re-joins its rooms after every `HELLO`.

use super::b64;
use super::protocol::{RtcError, js_len, q};
use futures_util::{SinkExt, StreamExt};
use indexmap::IndexSet;
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

pub const SIGNALING_VERSION: &str = "2.2";
pub const SIGNALING_CAPS: [&str; 3] = ["rooms", "trickle", "ferry-dc"];
/// Decompressed SDPs larger than this are rejected (decompression-bomb guard).
pub const MAX_SDP_BYTES: usize = 256 * 1024;
/// Server limits: encoded `sdp`, serialized ICE `candidate`, `sessionId`.
pub const MAX_ENCODED_SDP_BYTES: usize = 48 * 1024;
pub const MAX_CANDIDATE_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_ID_LENGTH: usize = 64;
const MAX_ALIAS_CHARS: usize = 64;
const MAX_DEVICE_MODEL_CHARS: usize = 64;
const MAX_TOKEN_CHARS: usize = 512;
/// Inbound frames larger than this are dropped by the WebSocket layer.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

// ── Client info (what we announce) ────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfoOut {
    pub alias: String,
    pub device_model: Option<String>,
    /// Sent upper-cased (`DESKTOP`, `WEB`, …).
    pub device_type: Option<String>,
    pub token: String,
    /// Identity public key, base64url.
    pub public_key: String,
    pub nearby: Option<bool>,
}

fn clip_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl ClientInfoOut {
    /// The JSON of `?d=` / `UPDATE.info` (member order as upstream LocalSend).
    pub fn to_json(&self) -> String {
        let mut o = format!("{{\"alias\":{},\"version\":{}", q(&clip_chars(&self.alias, MAX_ALIAS_CHARS)), q(SIGNALING_VERSION));
        if let Some(m) = &self.device_model {
            o.push_str(&format!(",\"deviceModel\":{}", q(&clip_chars(m, MAX_DEVICE_MODEL_CHARS))));
        }
        if let Some(t) = &self.device_type {
            o.push_str(&format!(",\"deviceType\":{}", q(&t.to_uppercase())));
        }
        o.push_str(&format!(",\"token\":{}", q(&clip_chars(&self.token, MAX_TOKEN_CHARS))));
        let caps: Vec<String> = SIGNALING_CAPS.iter().map(|c| q(c)).collect();
        o.push_str(&format!(",\"ext\":{{\"v\":1,\"caps\":[{}],\"key\":{}", caps.join(","), q(&self.public_key)));
        if let Some(n) = self.nearby {
            o.push_str(&format!(",\"nearby\":{n}"));
        }
        o.push_str("}}");
        o
    }
}

/// The connection URL: `url` with `?d=` carrying the client info.
pub fn connect_url(url: &str, info: &ClientInfoOut) -> Result<String, RtcError> {
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        return Err(RtcError::new("invalid", "the signaling URL must start with ws:// or wss://"));
    }
    let d = b64::encode(info.to_json().as_bytes());
    let (base, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let (path, query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let mut params: Vec<&str> =
        query.map(|q| q.split('&').filter(|p| !p.is_empty() && *p != "d" && !p.starts_with("d=")).collect()).unwrap_or_default();
    let dparam = format!("d={d}");
    params.push(&dparam);
    let mut out = format!("{path}?{}", params.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Ok(out)
}

/// `GET /v1/turn?peer=<own id>` for the server behind `url`.
pub fn turn_url(url: &str, client_id: &str) -> Option<String> {
    let base = url.split(['?', '#']).next()?;
    let (scheme, rest) = base.split_once("://")?;
    let http = match scheme {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix("/ws").map(|r| format!("{r}/turn")).unwrap_or_else(|| format!("{rest}/turn"));
    Some(format!("{http}://{rest}?peer={}", percent_encoding::utf8_percent_encode(client_id, percent_encoding::NON_ALPHANUMERIC)))
}

/// `r:` + 16 to 64 base64url characters (link/QR rooms) or `c:` + 6 digits.
pub fn is_valid_room_id(room: &str) -> bool {
    if let Some(rest) = room.strip_prefix("r:") {
        (16..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    } else if let Some(rest) = room.strip_prefix("c:") {
        rest.len() == 6 && rest.bytes().all(|b| b.is_ascii_digit())
    } else {
        false
    }
}

// ── SDP encoding ──────────────────────────────────────────────────────────

/// base64url-nopad(zlib-deflate(UTF-8 SDP)), as LocalSend sends it.
pub fn encode_sdp(sdp: &str) -> String {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing into a Vec cannot fail.
    let _ = enc.write_all(sdp.as_bytes());
    b64::encode(&enc.finish().unwrap_or_default())
}

/// Inverse of [`encode_sdp`]. Tolerates padding and the standard alphabet.
pub fn decode_sdp(encoded: &str) -> Result<String, String> {
    let normalized = encoded.trim_end_matches('=').replace('+', "-").replace('/', "_");
    let bytes = b64::decode(&normalized).ok_or("SDP is not base64url")?;
    let mut out = Vec::new();
    let mut reader = flate2::read::ZlibDecoder::new(bytes.as_slice()).take(MAX_SDP_BYTES as u64 + 1);
    reader.read_to_end(&mut out).map_err(|e| format!("corrupt SDP: {e}"))?;
    if out.len() > MAX_SDP_BYTES {
        return Err("SDP too large".into());
    }
    String::from_utf8(out).map_err(|_| "SDP is not UTF-8".to_string())
}

// ── Messages ──────────────────────────────────────────────────────────────

/// A peer as the server describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientInfo {
    pub id: String,
    pub alias: String,
    pub version: String,
    pub token: String,
    pub device_model: Option<String>,
    pub device_type: Option<String>,
    pub ext: Option<ClientExt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClientExt {
    pub v: Value,
    pub caps: Vec<String>,
    /// Identity public key, base64url.
    pub key: String,
    pub nearby: Option<bool>,
}

impl ClientInfo {
    pub fn key(&self) -> Option<&str> {
        self.ext.as_ref().map(|e| e.key.as_str())
    }

    fn to_value(&self) -> Value {
        let mut o = Map::new();
        o.insert("id".into(), json!(self.id));
        o.insert("alias".into(), json!(self.alias));
        o.insert("version".into(), json!(self.version));
        o.insert("token".into(), json!(self.token));
        if let Some(m) = &self.device_model {
            o.insert("deviceModel".into(), json!(m));
        }
        if let Some(t) = &self.device_type {
            o.insert("deviceType".into(), json!(t));
        }
        if let Some(e) = &self.ext {
            let mut x = Map::new();
            x.insert("v".into(), e.v.clone());
            x.insert("caps".into(), json!(e.caps));
            x.insert("key".into(), json!(e.key));
            if let Some(n) = e.nearby {
                x.insert("nearby".into(), json!(n));
            }
            o.insert("ext".into(), Value::Object(x));
        }
        Value::Object(o)
    }
}

/// An ICE candidate as a flat `RTCIceCandidateInit`. Outer `None` = member
/// absent, `Some(None)` = `null`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<Option<String>>,
    pub sdp_m_line_index: Option<Option<i64>>,
    pub username_fragment: Option<Option<String>>,
}

impl IceCandidate {
    fn to_json(&self) -> String {
        let mut o = format!("{{\"candidate\":{}", q(&self.candidate));
        if let Some(mid) = &self.sdp_mid {
            o.push_str(&format!(",\"sdpMid\":{}", mid.as_deref().map(q).unwrap_or_else(|| "null".into())));
        }
        if let Some(i) = &self.sdp_m_line_index {
            o.push_str(&format!(",\"sdpMLineIndex\":{}", i.map(|i| i.to_string()).unwrap_or_else(|| "null".into())));
        }
        if let Some(u) = &self.username_fragment {
            o.push_str(&format!(",\"usernameFragment\":{}", u.as_deref().map(q).unwrap_or_else(|| "null".into())));
        }
        o.push('}');
        o
    }

    fn to_value(&self) -> Value {
        serde_json::from_str(&self.to_json()).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerInfo {
    pub v: Value,
    pub caps: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalingState {
    Connecting,
    Open,
    Closed,
}

/// Everything the client reports, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum SignalingEvent {
    State(SignalingState),
    Hello {
        client: ClientInfo,
        peers: Vec<ClientInfo>,
        server: Option<ServerInfo>,
    },
    Join {
        peer: ClientInfo,
    },
    Update {
        peer: ClientInfo,
    },
    Left {
        peer_id: String,
    },
    Offer {
        peer: ClientInfo,
        session_id: String,
        sdp: String,
    },
    Answer {
        peer: ClientInfo,
        session_id: String,
        sdp: String,
    },
    /// `candidate: None` = end of candidates.
    Ice {
        peer: ClientInfo,
        session_id: String,
        candidate: Option<IceCandidate>,
    },
    Cancel {
        peer: ClientInfo,
        session_id: String,
    },
    RoomHello {
        room: String,
        peers: Vec<ClientInfo>,
    },
    RoomPeerJoined {
        room: String,
        peer: ClientInfo,
    },
    RoomPeerLeft {
        room: String,
        peer_id: String,
    },
    /// Server `ERROR` frames, plus local problems (`bad-sdp`, `connect`).
    Error {
        code: Value,
        message: String,
        session_id: Option<String>,
        room: Option<String>,
    },
}

impl SignalingEvent {
    /// `{type, payload}` as the reference's event emitter reports it (tests).
    pub fn to_reference_json(&self) -> Value {
        let peers = |p: &[ClientInfo]| Value::Array(p.iter().map(ClientInfo::to_value).collect());
        let (ty, payload) = match self {
            SignalingEvent::State(s) => ("state", json!(format!("{s:?}").to_lowercase())),
            SignalingEvent::Hello { client, peers: p, server } => {
                let mut o = json!({ "client": client.to_value(), "peers": peers(p) });
                if let Some(s) = server {
                    o["server"] = json!({ "v": s.v, "caps": s.caps });
                }
                ("hello", o)
            }
            SignalingEvent::Join { peer } => ("join", json!({ "peer": peer.to_value() })),
            SignalingEvent::Update { peer } => ("update", json!({ "peer": peer.to_value() })),
            SignalingEvent::Left { peer_id } => ("left", json!({ "peerId": peer_id })),
            SignalingEvent::Offer { peer, session_id, sdp } => {
                ("offer", json!({ "peer": peer.to_value(), "sessionId": session_id, "sdp": sdp }))
            }
            SignalingEvent::Answer { peer, session_id, sdp } => {
                ("answer", json!({ "peer": peer.to_value(), "sessionId": session_id, "sdp": sdp }))
            }
            SignalingEvent::Ice { peer, session_id, candidate } => (
                "ice",
                json!({ "peer": peer.to_value(), "sessionId": session_id, "candidate": candidate.as_ref().map(IceCandidate::to_value) }),
            ),
            SignalingEvent::Cancel { peer, session_id } => ("cancel", json!({ "peer": peer.to_value(), "sessionId": session_id })),
            SignalingEvent::RoomHello { room, peers: p } => ("roomHello", json!({ "room": room, "peers": peers(p) })),
            SignalingEvent::RoomPeerJoined { room, peer } => ("roomPeerJoined", json!({ "room": room, "peer": peer.to_value() })),
            SignalingEvent::RoomPeerLeft { room, peer_id } => ("roomPeerLeft", json!({ "room": room, "peerId": peer_id })),
            SignalingEvent::Error { code, message, session_id, room } => {
                let mut o = json!({ "code": code, "message": message });
                if let Some(s) = session_id {
                    o["sessionId"] = json!(s);
                }
                if let Some(r) = room {
                    o["room"] = json!(r);
                }
                ("error", o)
            }
        };
        json!({ "type": ty, "payload": payload })
    }
}

/// Outbound frames (member order as the reference sends them).
pub mod outbound {
    use super::{ClientInfoOut, IceCandidate, q};

    pub fn sdp(kind: &str, target: &str, session_id: &str, encoded: &str) -> String {
        format!("{{\"type\":{},\"target\":{},\"sessionId\":{},\"sdp\":{}}}", q(kind), q(target), q(session_id), q(encoded))
    }

    pub fn ice(target: &str, session_id: &str, candidate: Option<&IceCandidate>) -> String {
        let c = candidate.map(IceCandidate::to_json).unwrap_or_else(|| "null".into());
        format!("{{\"type\":\"ICE\",\"target\":{},\"sessionId\":{},\"candidate\":{c}}}", q(target), q(session_id))
    }

    pub fn cancel(target: &str, session_id: &str) -> String {
        format!("{{\"type\":\"CANCEL\",\"target\":{},\"sessionId\":{}}}", q(target), q(session_id))
    }

    pub fn room(kind: &str, room: &str) -> String {
        format!("{{\"type\":{},\"room\":{}}}", q(kind), q(room))
    }

    pub fn update(info: &ClientInfoOut) -> String {
        format!("{{\"type\":\"UPDATE\",\"info\":{}}}", info.to_json())
    }

    pub fn ping() -> String {
        "{\"type\":\"PING\"}".into()
    }
}

fn is_str(v: Option<&Value>, min: usize, max: usize) -> Option<&str> {
    match v {
        Some(Value::String(s)) => {
            let n = js_len(s);
            (n >= min && n <= max).then_some(s.as_str())
        }
        _ => None,
    }
}

fn parse_client(v: Option<&Value>) -> Option<ClientInfo> {
    let o = v?.as_object()?;
    let mut info = ClientInfo {
        id: is_str(o.get("id"), 1, 256)?.into(),
        alias: is_str(o.get("alias"), 0, 256)?.into(),
        version: is_str(o.get("version"), 0, 32)?.into(),
        token: is_str(o.get("token"), 0, 1024)?.into(),
        device_model: is_str(o.get("deviceModel"), 0, 256).map(Into::into),
        device_type: is_str(o.get("deviceType"), 0, 32).map(Into::into),
        ext: None,
    };
    if let Some(ext) = o.get("ext").and_then(Value::as_object)
        && let Some(v) = ext.get("v").filter(|v| v.is_number())
        && let Some(caps) = ext.get("caps").and_then(Value::as_array)
        && caps.len() <= 64
        && let Some(caps) = caps.iter().map(|c| is_str(Some(c), 0, 64).map(String::from)).collect::<Option<Vec<_>>>()
        && let Some(key) = is_str(ext.get("key"), 1, 256)
    {
        info.ext = Some(ClientExt { v: v.clone(), caps, key: key.into(), nearby: ext.get("nearby").and_then(Value::as_bool) });
    }
    Some(info)
}

fn parse_clients(v: &[Value]) -> Vec<ClientInfo> {
    v.iter().filter_map(|c| parse_client(Some(c))).collect()
}

/// `Some(None)` = end of candidates; `None` = invalid.
fn parse_candidate(v: Option<&Value>) -> Option<Option<IceCandidate>> {
    match v? {
        Value::Null => Some(None),
        Value::String(s) => (s.starts_with("candidate:") && js_len(s) <= MAX_CANDIDATE_BYTES)
            .then(|| Some(IceCandidate { candidate: s.clone(), sdp_m_line_index: Some(Some(0)), ..Default::default() })),
        Value::Object(o) => {
            let candidate = is_str(o.get("candidate"), 0, MAX_CANDIDATE_BYTES)?;
            let mut c = IceCandidate { candidate: candidate.into(), ..Default::default() };
            match o.get("sdpMid") {
                Some(Value::Null) => c.sdp_mid = Some(None),
                v => c.sdp_mid = is_str(v, 0, 256).map(|s| Some(s.to_string())),
            }
            match o.get("sdpMLineIndex") {
                Some(Value::Null) => c.sdp_m_line_index = Some(None),
                Some(Value::Number(n)) => {
                    let i = n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9e15).map(|f| f as i64));
                    c.sdp_m_line_index = i.map(Some);
                }
                _ => {}
            }
            match o.get("usernameFragment") {
                Some(Value::Null) => c.username_fragment = Some(None),
                v => c.username_fragment = is_str(v, 0, 256).map(|s| Some(s.to_string())),
            }
            Some(Some(c))
        }
        _ => None,
    }
}

/// Parses one inbound frame into the event it produces (if any).
pub fn parse_server_message(text: &str) -> Option<SignalingEvent> {
    let raw: Value = serde_json::from_str(text).ok()?;
    let o = raw.as_object()?;
    let ty = o.get("type")?.as_str()?;
    match ty {
        "HELLO" => {
            let client = parse_client(o.get("client"))?;
            let peers = o.get("peers")?.as_array()?;
            let server = o.get("server").and_then(Value::as_object).and_then(|s| {
                let v = s.get("v").filter(|v| v.is_number())?;
                let caps = s.get("caps")?.as_array()?;
                Some(ServerInfo { v: v.clone(), caps: caps.iter().filter_map(|c| c.as_str().map(String::from)).collect() })
            });
            Some(SignalingEvent::Hello { client, peers: parse_clients(peers), server })
        }
        "JOIN" => Some(SignalingEvent::Join { peer: parse_client(o.get("peer"))? }),
        "UPDATE" => Some(SignalingEvent::Update { peer: parse_client(o.get("peer"))? }),
        "LEFT" => Some(SignalingEvent::Left { peer_id: is_str(o.get("peerId"), 1, 256)?.into() }),
        "OFFER" | "ANSWER" => {
            let peer = parse_client(o.get("peer"))?;
            let session_id: String = is_str(o.get("sessionId"), 1, 256)?.into();
            let encoded = is_str(o.get("sdp"), 1, MAX_SDP_BYTES * 2)?;
            Some(match decode_sdp(encoded) {
                Err(message) => SignalingEvent::Error { code: json!("bad-sdp"), message, session_id: Some(session_id), room: None },
                Ok(sdp) if ty == "OFFER" => SignalingEvent::Offer { peer, session_id, sdp },
                Ok(sdp) => SignalingEvent::Answer { peer, session_id, sdp },
            })
        }
        "ICE" => {
            let peer = parse_client(o.get("peer"))?;
            let candidate = parse_candidate(o.get("candidate"))?;
            let session_id = is_str(o.get("sessionId"), 1, 256)?.into();
            Some(SignalingEvent::Ice { peer, session_id, candidate })
        }
        "CANCEL" => {
            Some(SignalingEvent::Cancel { peer: parse_client(o.get("peer"))?, session_id: is_str(o.get("sessionId"), 1, 256)?.into() })
        }
        "ROOM_HELLO" => Some(SignalingEvent::RoomHello {
            room: is_str(o.get("room"), 1, 256)?.into(),
            peers: parse_clients(o.get("peers")?.as_array()?),
        }),
        "ROOM_PEER_JOINED" => {
            Some(SignalingEvent::RoomPeerJoined { peer: parse_client(o.get("peer"))?, room: is_str(o.get("room"), 1, 256)?.into() })
        }
        "ROOM_PEER_LEFT" => Some(SignalingEvent::RoomPeerLeft {
            room: is_str(o.get("room"), 1, 256)?.into(),
            peer_id: is_str(o.get("peerId"), 1, 256)?.into(),
        }),
        "ERROR" => {
            let code = o.get("code").filter(|c| c.is_number() || c.is_string())?.clone();
            let message = o.get("message").and_then(Value::as_str).map(|m| super::protocol::clip_utf16(m, 1024)).unwrap_or_default();
            Some(SignalingEvent::Error {
                code,
                message,
                session_id: is_str(o.get("sessionId"), 1, 256).map(Into::into),
                room: is_str(o.get("room"), 1, 256).map(Into::into),
            })
        }
        _ => None,
    }
}

// ── The client ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct SignalingConfig {
    /// Endpoint, e.g. `wss://signal.example/v1/ws`.
    pub url: String,
    pub info: ClientInfoOut,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub ping_interval: Duration,
    pub idle_timeout: Duration,
}

impl SignalingConfig {
    pub fn new(url: impl Into<String>, info: ClientInfoOut) -> Self {
        SignalingConfig {
            url: url.into(),
            info,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(65),
        }
    }
}

enum Cmd {
    Send(String, oneshot::Sender<Result<(), RtcError>>),
    Join(String),
    Leave(String),
    Update(ClientInfoOut),
    Close,
}

#[derive(Default)]
struct Status {
    state: Option<SignalingState>,
    client: Option<ClientInfo>,
    server: Option<ServerInfo>,
    rooms: IndexSet<String>,
    last_error: Option<String>,
}

/// A running signaling connection. Dropping every handle closes it.
#[derive(Clone)]
pub struct SignalingClient {
    cmd: mpsc::UnboundedSender<Cmd>,
    status: Arc<Mutex<Status>>,
    url: String,
}

impl SignalingClient {
    /// Starts connecting (and reconnecting until [`SignalingClient::close`]).
    pub fn start(config: SignalingConfig) -> (SignalingClient, mpsc::UnboundedReceiver<SignalingEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let client = SignalingClient { cmd: cmd_tx, status: status.clone(), url: config.url.clone() };
        tokio::spawn(run(config, cmd_rx, ev_tx, status));
        (client, ev_rx)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn state(&self) -> SignalingState {
        self.status.lock().unwrap().state.unwrap_or(SignalingState::Closed)
    }

    /// This device as the server sees it (from the last `HELLO`).
    pub fn client(&self) -> Option<ClientInfo> {
        self.status.lock().unwrap().client.clone()
    }

    pub fn server(&self) -> Option<ServerInfo> {
        self.status.lock().unwrap().server.clone()
    }

    pub fn last_error(&self) -> Option<String> {
        self.status.lock().unwrap().last_error.clone()
    }

    pub fn joined_rooms(&self) -> Vec<String> {
        self.status.lock().unwrap().rooms.iter().cloned().collect()
    }

    async fn send(&self, text: String) -> Result<(), RtcError> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Send(text, tx)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }

    pub async fn send_offer(&self, target: &str, session_id: &str, sdp: &str) -> Result<(), RtcError> {
        self.send_sdp("OFFER", target, session_id, sdp).await
    }

    pub async fn send_answer(&self, target: &str, session_id: &str, sdp: &str) -> Result<(), RtcError> {
        self.send_sdp("ANSWER", target, session_id, sdp).await
    }

    async fn send_sdp(&self, kind: &str, target: &str, session_id: &str, sdp: &str) -> Result<(), RtcError> {
        check_session_id(session_id)?;
        let encoded = encode_sdp(sdp);
        if encoded.len() > MAX_ENCODED_SDP_BYTES {
            return Err(RtcError::new("too-large", "SDP too large for the signaling server"));
        }
        self.send(outbound::sdp(kind, target, session_id, &encoded)).await
    }

    /// Trickles one ICE candidate; `None` signals end-of-candidates.
    pub async fn send_ice(&self, target: &str, session_id: &str, candidate: Option<&IceCandidate>) -> Result<(), RtcError> {
        check_session_id(session_id)?;
        if candidate.is_some_and(|c| c.to_json().len() > MAX_CANDIDATE_BYTES) {
            return Err(RtcError::new("too-large", "ICE candidate too large"));
        }
        self.send(outbound::ice(target, session_id, candidate)).await
    }

    pub async fn send_cancel(&self, target: &str, session_id: &str) -> Result<(), RtcError> {
        check_session_id(session_id)?;
        self.send(outbound::cancel(target, session_id)).await
    }

    /// Joins a room now (when connected) and after every reconnect.
    pub fn join_room(&self, room: &str) -> Result<(), RtcError> {
        if !is_valid_room_id(room) {
            return Err(RtcError::new("invalid", "invalid room id"));
        }
        self.status.lock().unwrap().rooms.insert(room.to_string());
        let _ = self.cmd.send(Cmd::Join(room.to_string()));
        Ok(())
    }

    pub fn leave_room(&self, room: &str) {
        if self.status.lock().unwrap().rooms.shift_remove(room) {
            let _ = self.cmd.send(Cmd::Leave(room.to_string()));
        }
    }

    /// Updates the announced info (sent now when connected, used for reconnects).
    pub fn update(&self, info: ClientInfoOut) {
        let _ = self.cmd.send(Cmd::Update(info));
    }

    pub fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }

    /// Short-lived TURN credentials (`GET /v1/turn?peer=<own id>`) when the
    /// server advertised the `turn` capability; `Ok(None)` otherwise.
    pub async fn fetch_turn(&self) -> Result<Option<TurnCredentials>, RtcError> {
        let (Some(client), Some(server)) = (self.client(), self.server()) else { return Ok(None) };
        if !server.caps.iter().any(|c| c == "turn") {
            return Ok(None);
        }
        let url = turn_url(&self.url, &client.id).ok_or_else(|| RtcError::new("turn", "no TURN endpoint for this server"))?;
        // reqwest needs a process-wide rustls provider (the engine installs ring; keep it if set).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().map_err(|e| RtcError::new("turn", e.to_string()))?;
        let res = http.get(&url).send().await.map_err(|e| RtcError::new("turn", e.to_string()))?;
        if !res.status().is_success() {
            return Err(RtcError::new("turn", format!("TURN credentials unavailable (HTTP {})", res.status().as_u16())));
        }
        let body: Value = res.json().await.map_err(|e| RtcError::new("turn", e.to_string()))?;
        parse_turn(&body).map(Some).ok_or_else(|| RtcError::new("turn", "malformed TURN credentials"))
    }
}

/// `GET /v1/turn` answer: coturn `use-auth-secret` credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCredentials {
    pub ice_servers: Vec<TurnServer>,
    /// Validity in seconds.
    pub ttl: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// Validates a `/v1/turn` body like the reference's `parseTurn`.
pub fn parse_turn(value: &Value) -> Option<TurnCredentials> {
    let o = value.as_object()?;
    let ttl = o.get("ttl")?.as_f64()?;
    let mut ice_servers = Vec::new();
    for s in o.get("iceServers")?.as_array()? {
        let s = s.as_object()?;
        let urls: Vec<String> = match s.get("urls")? {
            Value::String(u) => vec![u.clone()],
            Value::Array(a) => a.iter().map(|u| u.as_str().map(String::from)).collect::<Option<_>>()?,
            _ => return None,
        };
        let ok =
            |u: &String| (1..=1024).contains(&js_len(u)) && (u.starts_with("turn:") || u.starts_with("turns:") || u.starts_with("stun:"));
        if urls.is_empty() || !urls.iter().all(ok) {
            return None;
        }
        ice_servers.push(TurnServer {
            urls,
            username: is_str(s.get("username"), 1, 1024)?.into(),
            credential: is_str(s.get("credential"), 1, 1024)?.into(),
        });
    }
    Some(TurnCredentials { ice_servers, ttl: ttl.max(0.0) as u64 })
}

fn closed() -> RtcError {
    RtcError::new("closed", "signaling is not connected")
}

fn check_session_id(id: &str) -> Result<(), RtcError> {
    let n = js_len(id);
    if !(1..=MAX_SESSION_ID_LENGTH).contains(&n) {
        return Err(RtcError::new("invalid", "invalid session id"));
    }
    Ok(())
}

fn set_state(status: &Mutex<Status>, events: &mpsc::UnboundedSender<SignalingEvent>, state: SignalingState) {
    let mut s = status.lock().unwrap();
    if s.state == Some(state) {
        return;
    }
    s.state = Some(state);
    if state != SignalingState::Open {
        s.client = None;
    }
    drop(s);
    let _ = events.send(SignalingEvent::State(state));
}

/// Waits `delay` unless a `Close` arrives; other commands are answered as offline.
async fn backoff(delay: Duration, cmds: &mut mpsc::UnboundedReceiver<Cmd>, info: &mut ClientInfoOut) -> bool {
    let until = Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => return true,
            cmd = cmds.recv() => match cmd {
                None | Some(Cmd::Close) => return false,
                Some(Cmd::Send(_, ack)) => { let _ = ack.send(Err(closed())); }
                Some(Cmd::Update(i)) => *info = i,
                Some(Cmd::Join(_) | Cmd::Leave(_)) => {}
            },
        }
    }
}

async fn run(
    config: SignalingConfig,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    events: mpsc::UnboundedSender<SignalingEvent>,
    status: Arc<Mutex<Status>>,
) {
    let mut info = config.info.clone();
    let mut attempt: u32 = 0;
    loop {
        set_state(&status, &events, SignalingState::Connecting);
        let url = match connect_url(&config.url, &info) {
            Ok(u) => u,
            Err(e) => {
                status.lock().unwrap().last_error = Some(e.message.clone());
                let _ = events.send(SignalingEvent::Error { code: json!("connect"), message: e.message, session_id: None, room: None });
                set_state(&status, &events, SignalingState::Closed);
                return;
            }
        };
        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let connect = tokio_tungstenite::connect_async_with_config(url.as_str(), Some(ws_config), true);
        let ws = tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(15), connect) => match r {
                Ok(Ok((ws, _))) => Some(ws),
                Ok(Err(e)) => { report(&status, &events, e.to_string()); None }
                Err(_) => { report(&status, &events, "connection timed out".into()); None }
            },
            cmd = cmds.recv() => match cmd {
                None | Some(Cmd::Close) => { set_state(&status, &events, SignalingState::Closed); return; }
                Some(Cmd::Send(_, ack)) => { let _ = ack.send(Err(closed())); None }
                Some(Cmd::Update(i)) => { info = i; None }
                Some(_) => None,
            },
        };
        if let Some(ws) = ws {
            status.lock().unwrap().last_error = None;
            set_state(&status, &events, SignalingState::Open);
            match connected(ws, &config, &mut info, &mut cmds, &events, &status, &mut attempt).await {
                Exit::Closed => {
                    set_state(&status, &events, SignalingState::Closed);
                    return;
                }
                Exit::Dropped => {}
            }
        }
        set_state(&status, &events, SignalingState::Closed);
        // Exponential backoff with "equal jitter": delay ∈ [d/2, d).
        let exp = config.initial_backoff.saturating_mul(1u32 << attempt.min(16)).min(config.max_backoff);
        let jitter: f64 = rand::random::<f64>();
        let delay = (exp / 2 + exp.mul_f64(jitter / 2.0)).min(config.max_backoff);
        attempt = attempt.saturating_add(1);
        if !backoff(delay, &mut cmds, &mut info).await {
            set_state(&status, &events, SignalingState::Closed);
            return;
        }
    }
}

fn report(status: &Mutex<Status>, events: &mpsc::UnboundedSender<SignalingEvent>, message: String) {
    tracing::debug!("signaling connect failed: {message}");
    status.lock().unwrap().last_error = Some(message.clone());
    let _ = events.send(SignalingEvent::Error { code: json!("connect"), message, session_id: None, room: None });
}

enum Exit {
    Closed,
    Dropped,
}

async fn connected<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    config: &SignalingConfig,
    info: &mut ClientInfoOut,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    events: &mpsc::UnboundedSender<SignalingEvent>,
    status: &Mutex<Status>,
    attempt: &mut u32,
) -> Exit
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut write, mut read) = ws.split();
    let mut greeted = false;
    let mut last_inbound = Instant::now();
    let mut tick = tokio::time::interval_at(Instant::now() + config.ping_interval, config.ping_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(Ok(msg)) = msg else { return Exit::Dropped };
                last_inbound = Instant::now();
                let Message::Text(text) = msg else {
                    if matches!(msg, Message::Close(_)) { return Exit::Dropped; }
                    continue;
                };
                let Some(event) = parse_server_message(text.as_str()) else { continue };
                if let SignalingEvent::Hello { client, server, .. } = &event {
                    greeted = true;
                    *attempt = 0;
                    let rooms: Vec<String> = {
                        let mut s = status.lock().unwrap();
                        s.client = Some(client.clone());
                        s.server = server.clone();
                        s.rooms.iter().cloned().collect()
                    };
                    for room in rooms {
                        if write.send(Message::text(outbound::room("ROOM_JOIN", &room))).await.is_err() {
                            return Exit::Dropped;
                        }
                    }
                }
                let _ = events.send(event);
            }
            cmd = cmds.recv() => match cmd {
                None | Some(Cmd::Close) => {
                    let _ = write.send(Message::Close(None)).await;
                    return Exit::Closed;
                }
                Some(Cmd::Send(text, ack)) => {
                    let r = write.send(Message::text(text)).await.map_err(|_| closed());
                    let failed = r.is_err();
                    let _ = ack.send(r);
                    if failed { return Exit::Dropped; }
                }
                Some(Cmd::Join(room)) => {
                    if greeted && write.send(Message::text(outbound::room("ROOM_JOIN", &room))).await.is_err() {
                        return Exit::Dropped;
                    }
                }
                Some(Cmd::Leave(room)) => {
                    if greeted && write.send(Message::text(outbound::room("ROOM_LEAVE", &room))).await.is_err() {
                        return Exit::Dropped;
                    }
                }
                Some(Cmd::Update(i)) => {
                    *info = i;
                    if write.send(Message::text(outbound::update(info))).await.is_err() {
                        return Exit::Dropped;
                    }
                }
            },
            _ = tick.tick() => {
                let ferry_server = status.lock().unwrap().server.is_some();
                if ferry_server && last_inbound.elapsed() > config.idle_timeout {
                    // The server pongs our pings; silence means a dead connection.
                    return Exit::Dropped;
                }
                // Plain LocalSend servers don't know PING; they get empty frames.
                let frame = if ferry_server { outbound::ping() } else { String::new() };
                if write.send(Message::text(frame)).await.is_err() {
                    return Exit::Dropped;
                }
            }
        }
    }
}
