// Derived from LocalSend (https://github.com/localsend/localsend, Apache-2.0); modified by the Ferry authors.
//! One WebSocket connection: a reader loop (this task) and a writer task.
//!
//! The writer owns the socket's sink and drains the bounded outbound queue;
//! nothing else ever awaits a socket write. The reader meters every inbound
//! frame, dispatches messages and enforces the idle timeout.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};
use uuid::Uuid;

use crate::close;
use crate::http::AppState;
use crate::hub::{self, ConnCtl, GONE, JoinError, NewPeer, Outbox, RouteError};
use crate::limit::{ConnSlot, TokenBucket};
use crate::net::IpGroup;
use crate::protocol::{
    ClientInfo, ClientMessage, MAX_SDP_BYTES, MAX_SESSION_ID_CHARS, RawClientInfo, ServerMessage, Violation, check_candidate, encode,
    error_json, is_valid_room_id, parse_client_message, too_long,
};

/// Time allowed for the close handshake (flush, close frame, peer's reply).
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Time allowed for writing the close frame itself.
const CLOSE_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// After an unreadable frame, keep the socket open briefly so the client can
/// read the queued `ERROR` and close frame before the connection is reset.
const BROKEN_LINGER: Duration = Duration::from_millis(500);
/// Inbound frames discarded while waiting for the peer's close reply.
const DRAIN_MAX_FRAMES: usize = 256;

/// Everything the upgrade handler resolved for a new connection.
pub(crate) struct ConnParams {
    pub id: Uuid,
    pub info: ClientInfo,
    pub group: IpGroup,
    pub ip_tag: String,
    pub slot: ConnSlot,
    /// [`AppState::shutdown`]; held until the connection has fully closed.
    pub shutdown: watch::Receiver<bool>,
}

/// Why the reader loop stopped.
enum End {
    /// The peer sent a close frame (tungstenite answers it).
    PeerClosed,
    /// The transport failed or ended without a close handshake.
    Transport,
    /// Close with `code` once the queued messages are flushed.
    Close(u16),
    /// Like `Close`, but the inbound side is unusable (oversized frame).
    Broken(u16),
    /// Disconnected through [`ConnCtl::kill`] (slow consumer, write failure).
    Killed,
}

impl End {
    fn label(&self) -> &'static str {
        match self {
            Self::PeerClosed => "closed by client",
            Self::Transport => "connection lost",
            Self::Close(code) | Self::Broken(code) => close::reason(*code),
            Self::Killed => "disconnected by server",
        }
    }
}

pub(crate) async fn run(socket: WebSocket, state: Arc<AppState>, params: ConnParams) {
    let ConnParams { id, info, group, ip_tag, slot, mut shutdown } = params;
    let limits = &state.config.limits;
    let ext = info.ext.is_some();
    let nearby_flag = info.ext.as_ref().and_then(|e| e.nearby);
    let nearby = nearby_flag != Some(false);
    let info = Arc::new(info);

    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel(limits.outbound_queue.max(1));
    let ctl = Arc::new(ConnCtl::new());
    let mut kill = ctl.subscribe();

    let total = state.hub.register(
        NewPeer { id, info: Arc::clone(&info), group, nearby, ext, tx: tx.clone(), ctl: Arc::clone(&ctl) },
        Some(&state.server_info),
    );
    tracing::info!(client = %id, ip = %ip_tag, ext, nearby, total, "client connected");
    tracing::debug!(client = %id, alias = %info.alias, version = %info.version, "client info");

    let mut writer = tokio::spawn(write_loop(sink, rx, Arc::clone(&ctl), limits.ping_interval, limits.write_timeout));

    let mut session = Session {
        id,
        info,
        group,
        ext,
        nearby_flag,
        tx,
        ctl: &ctl,
        state: &state,
        frames: TokenBucket::new(limits.frames_per_sec, limits.frame_burst),
        joins: TokenBucket::per_minute(limits.room_joins_per_minute),
        updates: TokenBucket::per_minute(limits.updates_per_minute),
        violations: 0,
    };
    let end = session.read_loop(&mut stream, &mut kill, &mut shutdown).await;
    // The writer reads the close code once its queue has drained, which can
    // happen as soon as the last sender is dropped: set it first.
    if let End::Close(code) | End::Broken(code) = end {
        ctl.set_graceful(code);
    }
    drop(session); // drops our queue sender
    // During shutdown everybody leaves at once: skip the LEFT fan-out.
    let notify = !*shutdown.borrow();
    let total = state.hub.unregister(id, notify); // drops the hub's queue sender

    let deadline = Instant::now() + CLOSE_GRACE;
    if matches!(end, End::PeerClosed | End::Close(_) | End::Killed) {
        // Keep reading until the close handshake completes so that unread
        // input does not turn our close into a connection reset.
        drain(&mut stream, deadline).await;
    }
    if tokio::time::timeout_at(deadline, &mut writer).await.is_err() {
        writer.abort();
    }
    if matches!(end, End::Broken(_)) {
        tokio::time::sleep(BROKEN_LINGER).await;
    }
    drop(stream);
    drop(slot);
    tracing::info!(client = %id, reason = end.label(), total, "client disconnected");
    drop(shutdown); // last: tells a shutting-down server this connection is gone
}

/// Drains the outbound queue into the socket and sends periodic pings.
async fn write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Utf8Bytes>,
    ctl: Arc<ConnCtl>,
    ping_interval: Duration,
    write_timeout: Duration,
) {
    let mut kill = ctl.subscribe();
    let mut ping = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let message = tokio::select! {
            biased;
            () = hub::killed(&mut kill) => break,
            item = rx.recv() => match item {
                Some(text) => Message::Text(text),
                None => break, // every sender is gone: the connection is closing
            },
            _ = ping.tick() => Message::Ping(Default::default()),
        };
        let sent = tokio::select! {
            biased;
            () = hub::killed(&mut kill) => break,
            sent = tokio::time::timeout(write_timeout, sink.send(message)) => sent,
        };
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                ctl.kill(GONE);
                return;
            }
            Err(_) => {
                ctl.kill(close::SLOW_CONSUMER);
                break;
            }
        }
    }

    let Some(code) = ctl.close_code() else {
        return;
    };
    let close = async {
        if code == 0 {
            // Complete a client-initiated close handshake.
            sink.close().await
        } else {
            sink.send(Message::Close(Some(CloseFrame { code, reason: Utf8Bytes::from_static(close::reason(code)) }))).await
        }
    };
    let _ = tokio::time::timeout(CLOSE_SEND_TIMEOUT, close).await;
}

/// Discards inbound frames until the stream ends or `deadline` passes.
async fn drain(stream: &mut SplitStream<WebSocket>, deadline: Instant) {
    for _ in 0..DRAIN_MAX_FRAMES {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(_))) => {}
            _ => return,
        }
    }
}

/// `true` for tungstenite's "message/frame too long" errors.
fn is_capacity_error(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(tungstenite::Error::Capacity(_)) = e.downcast_ref::<tungstenite::Error>() {
            return true;
        }
        source = e.source();
    }
    false
}

/// Payload of a relayed message.
#[derive(Clone, Copy)]
enum Relay<'a> {
    Offer(&'a str),
    Answer(&'a str),
    Ice(Option<&'a Value>),
    Cancel,
}

/// Per-connection state used by the reader loop.
struct Session<'a> {
    id: Uuid,
    info: Arc<ClientInfo>,
    group: IpGroup,
    ext: bool,
    /// `ext.nearby` as sent at connect time (fixed for the connection).
    nearby_flag: Option<bool>,
    tx: Outbox,
    ctl: &'a ConnCtl,
    state: &'a AppState,
    frames: TokenBucket,
    joins: TokenBucket,
    updates: TokenBucket,
    violations: u32,
}

impl Session<'_> {
    async fn read_loop(
        &mut self,
        stream: &mut SplitStream<WebSocket>,
        kill: &mut watch::Receiver<u16>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> End {
        let idle_timeout = self.state.config.limits.idle_timeout;
        let mut idle_deadline = Instant::now() + idle_timeout;
        loop {
            let frame = tokio::select! {
                biased;
                () = hub::killed(kill) => return End::Killed,
                // An error means the server state is gone: shutting down too.
                _ = shutdown.wait_for(|down| *down) => return End::Close(close::GOING_AWAY),
                () = tokio::time::sleep_until(idle_deadline) => return End::Close(close::IDLE_TIMEOUT),
                frame = stream.next() => frame,
            };
            let message = match frame {
                Some(Ok(message)) => message,
                Some(Err(err)) if is_capacity_error(&err) => {
                    self.push(error_json(413, "frame too large"));
                    return End::Broken(close::MESSAGE_TOO_BIG);
                }
                Some(Err(err)) => {
                    tracing::debug!(client = %self.id, "websocket error: {err}");
                    return End::Transport;
                }
                None => return End::Transport,
            };
            idle_deadline = Instant::now() + idle_timeout;

            if !self.frames.try_take() {
                self.push(error_json(429, "rate limit exceeded"));
                return End::Close(close::RATE_LIMITED);
            }
            let end = match message {
                Message::Text(text) => self.on_text(text.as_str()),
                Message::Binary(_) => self.violation(Violation::invalid("binary frames are not supported")),
                Message::Ping(_) | Message::Pong(_) => None,
                Message::Close(_) => return End::PeerClosed,
            };
            if let Some(end) = end {
                return end;
            }
        }
    }

    /// Queues a message for this client; a full queue means we are slow.
    fn push(&self, text: Utf8Bytes) {
        hub::deliver(&self.tx, self.ctl, text);
    }

    fn reply_error(&self, code: u16, message: &str, session_id: Option<&str>, room: Option<&str>) {
        self.push(encode(&ServerMessage::Error { code, message, session_id, room }));
    }

    /// Answers a bad message and counts it; too many close the connection.
    fn violation(&mut self, v: Violation) -> Option<End> {
        self.violation_with(v, None)
    }

    fn violation_with(&mut self, v: Violation, session_id: Option<&str>) -> Option<End> {
        self.reply_error(v.code, v.message, session_id, None);
        self.violations += 1;
        if self.violations >= self.state.config.limits.max_violations { Some(End::Close(close::POLICY_VIOLATION)) } else { None }
    }

    fn on_text(&mut self, text: &str) -> Option<End> {
        // LocalSend's web client keeps the connection alive with empty frames.
        if text.trim().is_empty() {
            return None;
        }
        let message = match parse_client_message(text) {
            Ok(message) => message,
            Err(v) => return self.violation(v),
        };
        if message.is_extension() && !self.ext {
            return self.violation(Violation::forbidden("extension messages require ext in the client info"));
        }
        match message {
            ClientMessage::Update { info } => self.on_update(info),
            ClientMessage::Offer { session_id, target, sdp } => self.relay(Relay::Offer(&sdp), target, &session_id),
            ClientMessage::Answer { session_id, target, sdp } => self.relay(Relay::Answer(&sdp), target, &session_id),
            ClientMessage::Ice { target, session_id, candidate } => self.relay(Relay::Ice(candidate.as_ref()), target, &session_id),
            ClientMessage::Cancel { target, session_id } => self.relay(Relay::Cancel, target, &session_id),
            ClientMessage::RoomJoin { room } => self.on_room_join(&room),
            ClientMessage::RoomLeave { room } => {
                if !is_valid_room_id(&room) {
                    return self.violation(Violation::invalid("invalid room id"));
                }
                self.state.hub.room_leave(self.id, &room);
                None
            }
            ClientMessage::Ping => {
                self.push(encode(&ServerMessage::Pong));
                None
            }
        }
    }

    fn on_update(&mut self, raw: RawClientInfo) -> Option<End> {
        if !self.updates.try_take() {
            self.reply_error(429, "too many updates", None, None);
            return None;
        }
        let mut info = match raw.validate(self.id) {
            Ok(info) => info,
            Err(v) => return self.violation(v),
        };
        // Whether a client gets extensions, and whether it is visible to its
        // IP group, is fixed when it connects.
        info.ext = if self.ext {
            match info.ext.take() {
                Some(mut ext) => {
                    ext.nearby = self.nearby_flag;
                    Some(ext)
                }
                None => self.info.ext.clone(),
            }
        } else {
            None
        };
        let info = Arc::new(info);
        self.info = Arc::clone(&info);
        self.state.hub.update(self.id, info);
        None
    }

    fn relay(&mut self, relay: Relay<'_>, target: Uuid, session_id: &str) -> Option<End> {
        if session_id.is_empty() {
            return self.violation(Violation::invalid("sessionId must not be empty"));
        }
        if too_long(session_id, MAX_SESSION_ID_CHARS) {
            return self.violation(Violation::too_large("sessionId too long"));
        }
        match relay {
            Relay::Offer(sdp) | Relay::Answer(sdp) if sdp.len() > MAX_SDP_BYTES => {
                return self.violation_with(Violation::too_large("sdp too large"), Some(session_id));
            }
            Relay::Ice(Some(candidate)) => {
                if let Err(v) = check_candidate(candidate) {
                    return self.violation_with(v, Some(session_id));
                }
            }
            _ => {}
        }

        let needs_ext = matches!(relay, Relay::Ice(_) | Relay::Cancel);
        match self.state.hub.route(self.id, target, needs_ext) {
            Ok(route) => {
                let peer = &*route.sender;
                let text = encode(&match relay {
                    Relay::Offer(sdp) => ServerMessage::Offer { peer, session_id, sdp },
                    Relay::Answer(sdp) => ServerMessage::Answer { peer, session_id, sdp },
                    Relay::Ice(candidate) => ServerMessage::Ice { peer, session_id, candidate },
                    Relay::Cancel => ServerMessage::Cancel { peer, session_id },
                });
                route.deliver(text);
            }
            Err(RouteError::NotFound) => {
                self.reply_error(404, "unknown target", Some(session_id), None);
            }
            Err(RouteError::Unsupported) => {
                self.reply_error(403, "target does not support this message", Some(session_id), None);
            }
        }
        None
    }

    fn on_room_join(&mut self, room: &str) -> Option<End> {
        let valid = is_valid_room_id(room);
        // Every attempt counts, valid or not (short codes must not be guessable).
        if !self.joins.try_take() {
            self.reply_error(429, "too many room joins", None, valid.then_some(room));
            return None;
        }
        if !valid {
            return self.violation(Violation::invalid("invalid room id"));
        }
        // Short codes are also limited per network, so that reconnecting
        // does not buy more guesses.
        if room.starts_with("c:") && !self.state.limiter.allow_code_join(self.group) {
            self.reply_error(429, "too many room joins", None, Some(room));
            return None;
        }
        let limits = &self.state.config.limits;
        match self.state.hub.room_join(self.id, room, limits) {
            Ok(()) => {}
            Err(JoinError::RoomFull) => self.reply_error(409, "room is full", None, Some(room)),
            Err(JoinError::TooManyRooms) => {
                self.reply_error(403, "too many rooms", None, Some(room));
            }
        }
        None
    }
}
