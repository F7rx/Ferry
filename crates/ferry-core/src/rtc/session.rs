//! `PeerSession`: one authenticated `ferry-dc/1` data channel (05-protocol.md
//! §5.2), behaving like `apps/app/src/lib/rtc/session.ts`.
//!
//! Works over any [`Transport`] (a WebRTC data channel, or an in-memory pair in
//! tests). Both sides send `hello` as soon as the channel is open, compute the
//! transcript T over both DTLS fingerprints as *they* observed them, exchange
//! `auth` signatures (plus a room MAC in secret rooms) and only then accept
//! transfers. Each direction runs one transfer at a time; both directions may
//! run concurrently.
//!
//! Tasks: a *reader* takes frames off the channel and answers liveness,
//! cancellation and sender-side replies (`answer`, `file-ack`, `progress`)
//! right away; everything else goes, in order, to a *worker* that runs the
//! handshake and the receiving side (the only place that touches the sink);
//! a *timer* pings and enforces the timeouts. Sending runs in the caller's
//! task ([`PeerSession::send_transfer`]).
//!
//! Memory stays bounded: senders read 1 MiB blocks (one block of read-ahead),
//! pause while the channel buffers more than 1 MiB and keep at most 16 MiB of
//! file bytes beyond the receiver's last `progress` in flight. Receivers fail
//! the session when a peer ignores that window or floods control frames.

use super::b64;
use super::identity::{RtcIdentity, verify};
use super::protocol::*;
use super::transcript::*;
use async_trait::async_trait;
use bytes::Bytes;
use indexmap::{IndexMap, IndexSet};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const READ_BLOCK: u64 = 1024 * 1024;

static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A cancellation token with an identity (one per transfer).
#[derive(Clone)]
struct Token {
    seq: u64,
    inner: CancellationToken,
}

impl Token {
    fn new() -> Self {
        Token { seq: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed), inner: CancellationToken::new() }
    }

    fn cancel(&self) {
        self.inner.cancel()
    }

    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn cancelled(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
        self.inner.cancelled()
    }
}

impl Default for Token {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
const DRAIN_POLL: Duration = Duration::from_millis(250);
/// Control frames waiting for the worker (UTF-16 units, as the reference).
const MAX_QUEUED_CONTROL: usize = 16 * 1024 * 1024;

// ── Transport ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Frame {
    Text(String),
    Binary(Bytes),
}

/// The subset of a data channel a session uses.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, frame: Frame) -> Result<(), String>;
    /// Bytes queued but not yet sent (`RTCDataChannel.bufferedAmount`).
    fn buffered_amount(&self) -> usize;
    /// Notified when `buffered_amount` drops below [`BUFFER_LOW_WATER`].
    fn buffered_low(&self) -> &Notify;
    /// The next inbound frame; `None` once the channel is closed. One reader.
    async fn recv(&self) -> Option<Frame>;
    async fn close(&self);
}

// ── Sources and sinks ─────────────────────────────────────────────────────

/// Bytes of a file to send. `read` returns exactly `end - start` bytes.
#[async_trait]
pub trait FileSource: Send + Sync + 'static {
    async fn read(&self, start: u64, end: u64) -> std::io::Result<Bytes>;
}

#[derive(Clone)]
pub struct OutgoingFile {
    pub meta: FileMeta,
    pub source: Arc<dyn FileSource>,
}

pub struct TransferRequest {
    /// 1 to 256 printable ASCII characters, unique within a session. Offer the
    /// same id again in a *new* session to resume.
    pub transfer_id: String,
    pub files: Vec<OutgoingFile>,
    /// At most [`MAX_TEXT_BYTES`] as a JSON string.
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferOutcome {
    pub transfer_id: String,
    pub declined: bool,
    /// Accepted files the receiver verified and committed.
    pub completed: Vec<String>,
    pub failed: Vec<String>,
    /// Offered files the receiver did not accept (all of them when declined).
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkContext {
    pub transfer_id: String,
    /// The verified peer's public key (base64url).
    pub peer_key: String,
}

/// Why a writer is aborted. Sinks keep partial data for `Closed` and `Timeout`
/// (resumable) and may discard it for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortReason {
    Cancelled,
    Closed,
    Timeout,
    Integrity,
    Overrun,
    Error,
}

#[async_trait]
pub trait SinkWriter: Send {
    async fn write(&mut self, chunk: Bytes) -> Result<(), String>;
    /// Commits the file. Only called after size and SHA-256 were verified.
    async fn close(self: Box<Self>) -> Result<(), String>;
    async fn abort(self: Box<Self>, reason: AbortReason);
}

#[async_trait]
pub trait FileSink: Send + Sync + 'static {
    /// Opens a writer positioned at `offset` (anything beyond it is dropped).
    async fn open(&self, file: &FileMeta, offset: u64, ctx: &SinkContext) -> Result<Box<dyn SinkWriter>, String>;
    /// Whether [`FileSink::hash_prefix`] works (needed to accept offsets > 0).
    fn can_resume(&self) -> bool {
        false
    }
    /// A hasher fed with exactly the first `offset` bytes already stored.
    async fn hash_prefix(&self, _file: &FileMeta, _offset: u64, _ctx: &SinkContext) -> Result<Sha256, String> {
        Err("this sink cannot resume".into())
    }
}

// ── Events ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePeer {
    pub alg: Alg,
    /// Raw public key, base64url. Verified: the peer proved possession of it.
    pub key: String,
    pub device: DeviceInfo,
    pub caps: Vec<String>,
    pub transcript: [u8; 32],
    /// 6-digit first-contact verification code derived from T.
    pub short_code: String,
    /// A room MAC was required and verified.
    pub room_verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Local,
    Remote,
    Timeout,
    Auth,
    Protocol,
    Error,
}

/// What a session reports. Per transfer the sender sees `Accepted` →
/// `Progress`* → `FileComplete`* → `Done`, or `Cancelled`; the receiver sees
/// `Offer` → `Progress`* → `FileComplete`* → `Done`, or `Cancelled`.
#[derive(Debug)]
pub enum SessionEvent {
    Ready(RemotePeer),
    Offer(IncomingOffer),
    Accepted {
        transfer_id: String,
        files: Vec<String>,
        offsets: Vec<(String, u64)>,
    },
    Progress {
        transfer_id: String,
        direction: Direction,
        file_id: String,
        bytes: u64,
        total: u64,
    },
    FileComplete {
        transfer_id: String,
        direction: Direction,
        file_id: String,
        ok: bool,
        sha256: Option<String>,
        error: Option<String>,
    },
    Done {
        transfer_id: String,
        direction: Direction,
        completed: Vec<String>,
        failed: Vec<String>,
        skipped: Vec<String>,
    },
    /// `interrupted`: the session closed under the transfer (resumable).
    Cancelled {
        transfer_id: String,
        direction: Direction,
        reason: Option<String>,
        by_remote: bool,
        interrupted: bool,
    },
    Error {
        code: String,
        message: String,
        remote: bool,
    },
    Closed {
        reason: CloseReason,
        message: Option<String>,
    },
}

/// An offer waiting for a decision (accept a subset, or decline).
#[derive(Clone)]
pub struct IncomingOffer {
    pub transfer_id: String,
    pub files: Vec<FileMeta>,
    pub text: Option<String>,
    pub peer: RemotePeer,
    /// When it is cancelled if nobody decides.
    pub expires_at: Option<Instant>,
    session: PeerSession,
}

impl std::fmt::Debug for IncomingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingOffer").field("transfer_id", &self.transfer_id).field("files", &self.files.len()).finish_non_exhaustive()
    }
}

impl IncomingOffer {
    /// Accepts `ids` (default: all), resuming each from `offsets` (default 0).
    pub async fn accept(&self, ids: Option<Vec<String>>, offsets: &[(String, u64)]) -> Result<(), RtcError> {
        self.session.accept_offer(&self.transfer_id, ids, offsets).await
    }

    pub async fn decline(&self) -> Result<(), RtcError> {
        self.session.decline_offer(&self.transfer_id).await
    }

    pub fn session(&self) -> &PeerSession {
        &self.session
    }
}

// ── Options ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SessionOptions {
    pub role: Role,
    /// Signaling session id (bound into the transcript).
    pub session_id: String,
    /// Normalized DTLS fingerprints from the local / remote SDP.
    pub local_fingerprint: String,
    pub remote_fingerprint: String,
    pub identity: Arc<RtcIdentity>,
    pub device: DeviceInfo,
    pub caps: Vec<String>,
    /// Secret of a link/QR room: both sides must prove it (HMAC over T).
    pub room_secret: Option<Vec<u8>>,
    /// Pin the peer's public key (base64url).
    pub expected_peer_key: Option<String>,
    /// Remote SDP `a=max-message-size`; unknown → 16 KiB chunks.
    pub max_message_size: Option<MaxMessageSize>,
    /// Where received files go. Without a sink only text-only offers can be accepted.
    pub sink: Option<Arc<dyn FileSink>>,
    pub ping_interval: Duration,
    /// Close when nothing arrived from the peer for this long.
    pub timeout: Duration,
    pub handshake_timeout: Duration,
    /// Cancel incoming offers nobody decided on within this time (None = never).
    pub decision_timeout: Option<Duration>,
    /// Minimum interval between `Progress` events per direction.
    pub progress_interval: Duration,
}

impl SessionOptions {
    pub fn new(
        role: Role,
        session_id: impl Into<String>,
        local_fp: String,
        remote_fp: String,
        identity: Arc<RtcIdentity>,
        device: DeviceInfo,
    ) -> Self {
        SessionOptions {
            role,
            session_id: session_id.into(),
            local_fingerprint: local_fp,
            remote_fingerprint: remote_fp,
            identity,
            device,
            caps: Vec::new(),
            room_secret: None,
            expected_peer_key: None,
            max_message_size: None,
            sink: None,
            ping_interval: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(30),
            decision_timeout: Some(Duration::from_secs(300)),
            progress_interval: Duration::from_millis(100),
        }
    }
}

// ── State ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Handshake,
    Ready,
    Closed,
}

#[derive(Clone)]
enum ReadyState {
    Pending,
    Ready(RemotePeer),
    Failed(RtcError),
}

struct FileAck {
    ok: bool,
    sha256: Option<String>,
    error: Option<String>,
}

struct Answered {
    declined: bool,
}

struct Out {
    transfer_id: String,
    sizes: HashMap<String, u64>,
    accepted: IndexSet<String>,
    offsets: Vec<(String, u64)>,
    answered: bool,
    answer_tx: Option<oneshot::Sender<Answered>>,
    acks: HashMap<String, oneshot::Sender<FileAck>>,
    ended: HashSet<String>,
    aborted: Option<RtcError>,
    cancel: Token,
    sent: u64,
    acked: u64,
}

struct Inc {
    transfer_id: String,
    files: HashMap<String, FileMeta>,
    metas: Vec<FileMeta>,
    text: Option<String>,
    offer_bytes: usize,
    total_size: u64,
    announced: bool,
    answered: bool,
    accepted: IndexMap<String, u64>,
    finished: HashSet<String>,
    completed: Vec<String>,
    failed: Vec<String>,
    current: Option<String>,
    cancel: Token,
    processed: u64,
    reported: u64,
}

struct State {
    phase: SessionState,
    close_reason: Option<CloseReason>,
    local_key: Option<Vec<u8>>,
    remote_hello: Option<Hello>,
    transcript: Option<[u8; 32]>,
    peer: Option<RemotePeer>,
    used_ids: HashSet<String>,
    sends: HashMap<String, Token>,
    out: Option<Out>,
    inc: Option<Inc>,
    discard: Option<String>,
    last_progress: HashMap<Direction, Instant>,
}

enum Work {
    Control(Control, usize),
    Binary(Bytes),
}

struct Inner {
    me: std::sync::Weak<Inner>,
    opts: SessionOptions,
    transport: Arc<dyn Transport>,
    events: mpsc::UnboundedSender<SessionEvent>,
    nonce: [u8; 32],
    room_key: Option<[u8; 32]>,
    chunk_size: usize,
    st: Mutex<State>,
    closed: CancellationToken,
    ready: watch::Sender<ReadyState>,
    send_lock: tokio::sync::Mutex<()>,
    wake: Notify,
    last_inbound: Mutex<Instant>,
    queued_binary: AtomicUsize,
    queued_control: AtomicUsize,
    work: mpsc::UnboundedSender<Work>,
}

/// A handle to one session (cheap to clone).
#[derive(Clone)]
pub struct PeerSession {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PeerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSession").field("session_id", &self.inner.opts.session_id).finish_non_exhaustive()
    }
}

/// Lengths of the binary frames a sender produces for one file from `offset`:
/// 1 MiB read blocks, each cut into `chunk`-sized frames.
pub fn chunk_plan(size: u64, offset: u64, chunk: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut block = offset;
    while block < size {
        let end = size.min(block + READ_BLOCK);
        let mut o = block;
        while o < end {
            let n = (end - o).min(chunk as u64);
            out.push(n as usize);
            o += n;
        }
        block = end;
    }
    out
}

fn closed_error(reason: CloseReason, message: &str) -> RtcError {
    let code = match reason {
        CloseReason::Timeout => "timeout",
        CloseReason::Auth => "auth",
        CloseReason::Protocol => "protocol",
        _ => "closed",
    };
    RtcError::new(code, message)
}

/// Removes a send slot when `send_transfer` returns.
struct SlotGuard<'a> {
    inner: &'a Inner,
    id: String,
    token: Token,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.inner.st.lock().unwrap();
        if st.sends.get(&self.id).is_some_and(|t| t == &self.token) {
            st.sends.remove(&self.id);
        }
    }
}

impl PeerSession {
    /// Starts a session on an open channel; the handshake begins immediately.
    pub fn start(opts: SessionOptions, transport: Arc<dyn Transport>) -> (PeerSession, mpsc::UnboundedReceiver<SessionEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let (work_tx, work_rx) = mpsc::unbounded_channel();
        let room_key = opts.room_secret.as_deref().map(derive_room_key);
        let chunk_size = chunk_size_for(opts.max_message_size);
        let inner = Arc::new_cyclic(|me| Inner {
            me: me.clone(),
            transport,
            events,
            nonce: crate::util::random_bytes::<32>(),
            room_key,
            chunk_size,
            st: Mutex::new(State {
                phase: SessionState::Handshake,
                close_reason: None,
                local_key: None,
                remote_hello: None,
                transcript: None,
                peer: None,
                used_ids: HashSet::new(),
                sends: HashMap::new(),
                out: None,
                inc: None,
                discard: None,
                last_progress: HashMap::new(),
            }),
            closed: CancellationToken::new(),
            ready: watch::channel(ReadyState::Pending).0,
            send_lock: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            last_inbound: Mutex::new(Instant::now()),
            queued_binary: AtomicUsize::new(0),
            queued_control: AtomicUsize::new(0),
            work: work_tx,
            opts,
        });
        tokio::spawn(reader(inner.clone()));
        tokio::spawn(worker(inner.clone(), work_rx));
        tokio::spawn(timer(inner.clone()));
        (PeerSession { inner }, rx)
    }

    pub fn session_id(&self) -> &str {
        &self.inner.opts.session_id
    }

    pub fn role(&self) -> Role {
        self.inner.opts.role
    }

    pub fn state(&self) -> SessionState {
        self.inner.st.lock().unwrap().phase
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    /// The authenticated peer, once ready.
    pub fn peer(&self) -> Option<RemotePeer> {
        self.inner.st.lock().unwrap().peer.clone()
    }

    /// Binary frame size used when sending.
    pub fn chunk_size(&self) -> usize {
        self.inner.chunk_size
    }

    /// Resolves once the peer is authenticated; fails when the handshake fails
    /// or the session closes first.
    pub async fn ready(&self) -> Result<RemotePeer, RtcError> {
        let mut rx = self.inner.ready.subscribe();
        loop {
            match &*rx.borrow_and_update() {
                ReadyState::Ready(p) => return Ok(p.clone()),
                ReadyState::Failed(e) => return Err(e.clone()),
                ReadyState::Pending => {}
            }
            if rx.changed().await.is_err() {
                return Err(RtcError::new("closed", "session closed"));
            }
        }
    }

    /// Resolves when the session has closed.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await
    }

    pub fn close(&self, message: Option<&str>) {
        self.inner.shutdown(CloseReason::Local, message.map(str::to_string));
    }

    /// Cancels `transfer_id` (running, queued or incoming), or everything.
    pub async fn cancel(&self, reason: Option<&str>, transfer_id: Option<&str>) {
        let inner = &self.inner;
        let matches = |id: &str| transfer_id.is_none_or(|t| t == id);
        let mut frames = Vec::new();
        {
            let mut st = inner.st.lock().unwrap();
            let running = st.out.as_ref().map(|o| o.transfer_id.clone());
            if let Some(id) = &running
                && matches(id)
                && let Some(frame) = inner.abort_outgoing(&mut st, RtcError::new("cancelled", reason.unwrap_or("cancelled")), false, reason)
            {
                frames.push(frame);
            }
            let queued: Vec<(String, Token)> = st
                .sends
                .iter()
                .filter(|(id, _)| Some(*id) != running.as_ref() && matches(id))
                .map(|(i, t)| (i.clone(), t.clone()))
                .collect();
            for (id, token) in queued {
                if token.is_cancelled() {
                    continue;
                }
                token.cancel();
                inner.emit(SessionEvent::Cancelled {
                    transfer_id: id,
                    direction: Direction::Send,
                    reason: reason.map(str::to_string),
                    by_remote: false,
                    interrupted: false,
                });
            }
            if let Some(id) = st.inc.as_ref().map(|i| i.transfer_id.clone())
                && matches(&id)
                && let Some(frame) = inner.abort_incoming(&mut st, false, reason)
            {
                frames.push(frame);
            }
        }
        for f in frames {
            let _ = inner.send_control(&f).await;
        }
    }

    /// Offers files and/or text and streams the accepted files. One outgoing
    /// transfer runs at a time (calls queue). Fails with `cancelled`,
    /// `invalid`, `too-large`, `source`, or the session's close code.
    pub async fn send_transfer(&self, request: TransferRequest) -> Result<TransferOutcome, RtcError> {
        let inner = &*self.inner;
        let id = request.transfer_id.clone();
        if !is_valid_id(&id) {
            return Err(RtcError::new("invalid", "invalid transferId"));
        }
        let token = Token::new();
        {
            let mut st = inner.st.lock().unwrap();
            if !st.used_ids.insert(id.clone()) {
                return Err(RtcError::new("invalid", format!("transferId {} was already used in this session", q(&id))));
            }
            st.sends.insert(id.clone(), token.clone());
        }
        let _slot = SlotGuard { inner, id: id.clone(), token: token.clone() };
        let _turn = tokio::select! {
            g = inner.send_lock.lock() => g,
            _ = token.cancelled() => return Err(inner.outgoing_error(&token)),
            _ = inner.closed.cancelled() => return Err(inner.outgoing_error(&token)),
        };
        let ready = tokio::select! {
            r = self.ready() => r,
            _ = token.cancelled() => return Err(inner.outgoing_error(&token)),
        };
        ready?;
        if token.is_cancelled() || inner.closed.is_cancelled() {
            return Err(inner.outgoing_error(&token));
        }
        self.inner.run_transfer(request, token).await
    }

    async fn accept_offer(&self, transfer_id: &str, ids: Option<Vec<String>>, offsets: &[(String, u64)]) -> Result<(), RtcError> {
        let inner = &self.inner;
        let frames = {
            let mut st = inner.st.lock().unwrap();
            inner.pending(&st, transfer_id)?;
            let Some(inc) = st.inc.as_ref() else { return Err(not_pending()) };
            let list = ids.unwrap_or_else(|| inc.metas.iter().map(|m| m.id.clone()).collect());
            let mut accepted: IndexMap<String, u64> = IndexMap::new();
            let mut wire = Vec::new();
            for id in &list {
                let Some(meta) = inc.files.get(id) else {
                    return Err(RtcError::new("invalid", format!("unknown file id {}", q(id))));
                };
                if accepted.contains_key(id) {
                    return Err(RtcError::new("invalid", format!("duplicate file id {}", q(id))));
                }
                let offset = offsets.iter().rev().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0);
                if offset > meta.size {
                    return Err(RtcError::new("invalid", format!("invalid offset for {}", q(id))));
                }
                accepted.insert(id.clone(), offset);
                if offset > 0 {
                    wire.push((id.clone(), offset));
                }
            }
            if !accepted.is_empty() && inner.opts.sink.is_none() {
                return Err(RtcError::new("no-sink", "no file sink configured"));
            }
            if !wire.is_empty() && !inner.opts.sink.as_ref().is_some_and(|s| s.can_resume()) {
                return Err(RtcError::new("no-resume", "the sink cannot read back prefixes, so it cannot resume"));
            }
            let keys: Vec<String> = accepted.keys().cloned().collect();
            let frames = encode_answer(transfer_id, &keys, &wire, false)?;
            let Some(inc) = st.inc.as_mut() else { return Err(not_pending()) };
            inc.accepted = accepted;
            inc.answered = true;
            frames
        };
        for f in frames {
            inner.send_raw(f).await?;
        }
        Ok(())
    }

    async fn decline_offer(&self, transfer_id: &str) -> Result<(), RtcError> {
        let inner = &self.inner;
        {
            let mut st = inner.st.lock().unwrap();
            inner.pending(&st, transfer_id)?;
            if let Some(inc) = st.inc.as_mut() {
                inc.answered = true;
            }
            st.inc = None;
        }
        let msg = Control::Answer(Answer { transfer_id: transfer_id.into(), accept: vec![], offsets: vec![], declined: true, more: false });
        inner.send_control(&msg).await
    }
}

fn not_pending() -> RtcError {
    RtcError::new("invalid-state", "the offer is no longer pending")
}

impl Inner {
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    async fn send_raw(&self, text: String) -> Result<(), RtcError> {
        if self.closed.is_cancelled() {
            return Err(RtcError::new("closed", "data channel is not open"));
        }
        self.write(Frame::Text(text)).await
    }

    /// A write that gives up once the session closes: a write to a data channel
    /// that died underneath it can otherwise wait forever. Never abandoned while
    /// the session is open, since a half-written message breaks the channel.
    async fn write(&self, frame: Frame) -> Result<(), RtcError> {
        tokio::select! {
            r = self.transport.send(frame) => r.map_err(|e| RtcError::new("closed", format!("data channel is not open: {e}"))),
            _ = self.closed.cancelled() => Err(RtcError::new("closed", "data channel is not open")),
        }
    }

    async fn send_control(&self, msg: &Control) -> Result<(), RtcError> {
        self.send_raw(encode_control(msg)?).await
    }

    fn touch(&self) {
        *self.last_inbound.lock().unwrap() = Instant::now();
    }

    /// Throttled per direction; the final event of a file is always emitted.
    fn progress(&self, st: &mut State, direction: Direction, transfer_id: &str, file_id: &str, bytes: u64, total: u64) {
        let now = Instant::now();
        if bytes < total && st.last_progress.get(&direction).is_some_and(|t| now.duration_since(*t) < self.opts.progress_interval) {
            return;
        }
        st.last_progress.insert(direction, now);
        self.emit(SessionEvent::Progress { transfer_id: transfer_id.into(), direction, file_id: file_id.into(), bytes, total });
    }

    /// Reports a fatal error to the peer, then closes.
    async fn fail(&self, err: RtcError) {
        if self.closed.is_cancelled() {
            return;
        }
        tracing::debug!(session = %self.opts.session_id, "ferry-dc session failed: {err}");
        let frame = error_frame(&err.code, &err.message);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.send_control(&frame)).await;
        self.emit(SessionEvent::Error { code: err.code.clone(), message: err.message.clone(), remote: false });
        let reason = match err.code.as_str() {
            "auth" => CloseReason::Auth,
            "protocol" | "overrun" | "too-large" => CloseReason::Protocol,
            _ => CloseReason::Error,
        };
        self.shutdown(reason, Some(err.message));
    }

    fn shutdown(&self, reason: CloseReason, message: Option<String>) {
        let text = message.clone().unwrap_or_else(|| format!("session closed ({reason:?})").to_lowercase());
        if !self.closed.is_cancelled() {
            tracing::debug!(session = %self.opts.session_id, role = ?self.opts.role, ?reason, "ferry-dc session closing: {text}");
        }
        let err = closed_error(reason, &text);
        {
            let mut st = self.st.lock().unwrap();
            if st.phase == SessionState::Closed {
                return;
            }
            st.phase = SessionState::Closed;
            st.close_reason = Some(reason);
            self.ready.send_if_modified(|r| {
                if matches!(r, ReadyState::Pending) {
                    *r = ReadyState::Failed(err.clone());
                    true
                } else {
                    false
                }
            });
            let by_remote = reason == CloseReason::Remote;
            if let Some(out) = st.out.as_mut()
                && out.aborted.is_none()
            {
                out.aborted = Some(err.clone());
                out.cancel.cancel();
                self.emit(SessionEvent::Cancelled {
                    transfer_id: out.transfer_id.clone(),
                    direction: Direction::Send,
                    reason: Some(text.clone()),
                    by_remote,
                    interrupted: true,
                });
            }
            for token in st.sends.values() {
                token.cancel();
            }
            if let Some(inc) = st.inc.take() {
                inc.cancel.cancel();
                if inc.announced {
                    self.emit(SessionEvent::Cancelled {
                        transfer_id: inc.transfer_id,
                        direction: Direction::Receive,
                        reason: Some(text.clone()),
                        by_remote,
                        interrupted: true,
                    });
                }
            }
        }
        // A sender blocked on backpressure re-checks and sees the close.
        self.closed.cancel();
        self.wake.notify_waiters();
        let transport = self.transport.clone();
        tokio::spawn(async move { transport.close().await });
        self.emit(SessionEvent::Closed { reason, message });
    }

    /// The error a cancelled or closed outgoing request fails with.
    fn outgoing_error(&self, token: &Token) -> RtcError {
        let st = self.st.lock().unwrap();
        if let Some(out) = st.out.as_ref().filter(|o| &o.cancel == token)
            && let Some(e) = &out.aborted
        {
            return e.clone();
        }
        if let Some(reason) = st.close_reason {
            return closed_error(reason, "session closed");
        }
        if token.is_cancelled() {
            return RtcError::new("cancelled", "cancelled");
        }
        RtcError::new("closed", "session closed")
    }

    /// Marks the running outgoing transfer aborted; returns the `cancel` frame to send.
    fn abort_outgoing(&self, st: &mut State, err: RtcError, by_remote: bool, reason: Option<&str>) -> Option<Control> {
        let out = st.out.as_mut()?;
        if out.aborted.is_some() {
            return None;
        }
        out.aborted = Some(err);
        out.cancel.cancel();
        self.wake.notify_waiters();
        self.emit(SessionEvent::Cancelled {
            transfer_id: out.transfer_id.clone(),
            direction: Direction::Send,
            reason: reason.map(str::to_string),
            by_remote,
            interrupted: false,
        });
        (!by_remote)
            .then(|| Control::Cancel { transfer_id: out.transfer_id.clone(), reason: reason.map(|r| clip_utf16(r, MAX_REASON_LENGTH)) })
    }

    /// Ends the incoming transfer; returns the `cancel` frame to send.
    fn abort_incoming(&self, st: &mut State, by_remote: bool, reason: Option<&str>) -> Option<Control> {
        let inc = st.inc.take()?;
        inc.cancel.cancel();
        st.discard = Some(inc.transfer_id.clone());
        if inc.announced {
            self.emit(SessionEvent::Cancelled {
                transfer_id: inc.transfer_id.clone(),
                direction: Direction::Receive,
                reason: reason.map(str::to_string),
                by_remote,
                interrupted: false,
            });
        }
        (!by_remote).then(|| Control::Cancel { transfer_id: inc.transfer_id, reason: reason.map(|r| clip_utf16(r, MAX_REASON_LENGTH)) })
    }

    fn pending(&self, st: &State, transfer_id: &str) -> Result<(), RtcError> {
        match &st.inc {
            Some(inc) if inc.transfer_id == transfer_id && !inc.answered && inc.announced && st.phase == SessionState::Ready => Ok(()),
            _ => Err(not_pending()),
        }
    }

    // ── Reader: fast path ────────────────────────────────────────────────

    async fn on_text(&self, text: String) -> Result<(), RtcError> {
        let msg = parse_control(&text)?;
        match &msg {
            Control::Ping => {
                // Never block the reader on our own write queue.
                if let Some(inner) = self.me.upgrade() {
                    tokio::spawn(async move {
                        let _ = inner.send_control(&Control::Pong).await;
                    });
                }
                return Ok(());
            }
            Control::Pong => return Ok(()),
            Control::Cancel { transfer_id, reason } => {
                if self.on_remote_cancel(transfer_id, reason.as_deref()) {
                    return Ok(());
                }
            }
            Control::Answer(a) => return self.on_answer(a),
            Control::FileAck { id, ok, sha256, error } => return self.on_file_ack(id, *ok, sha256.clone(), error.clone()),
            Control::Progress { transfer_id, bytes } => return self.on_progress(transfer_id, *bytes),
            Control::Error { code, message } => {
                self.emit(SessionEvent::Error { code: code.clone(), message: message.clone(), remote: true });
                self.shutdown(if code == "auth" { CloseReason::Auth } else { CloseReason::Remote }, Some(message.clone()));
                return Ok(());
            }
            _ => {}
        }
        let cost = js_len(&text);
        if self.queued_control.fetch_add(cost, Ordering::Relaxed) + cost > MAX_QUEUED_CONTROL {
            return Err(RtcError::protocol("too many queued control messages"));
        }
        let _ = self.work.send(Work::Control(msg, cost));
        Ok(())
    }

    fn on_binary_frame(&self, data: Bytes) -> Result<(), RtcError> {
        if data.len() > MAX_BINARY_FRAME {
            return Err(RtcError::protocol("binary frame too large"));
        }
        // A compliant sender never has more than RECV_WINDOW unreported bytes
        // in flight, so this bounds the receive queue (one frame of slack).
        let size = data.len();
        if self.queued_binary.fetch_add(size, Ordering::Relaxed) + size > RECV_WINDOW as usize + MAX_BINARY_FRAME {
            return Err(RtcError::protocol("the peer ignored the flow-control window"));
        }
        let _ = self.work.send(Work::Binary(data));
        Ok(())
    }

    fn on_remote_cancel(&self, transfer_id: &str, reason: Option<&str>) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.out.as_ref().is_some_and(|o| o.transfer_id == transfer_id) {
            self.abort_outgoing(&mut st, RtcError::new("cancelled", reason.unwrap_or("cancelled by the peer")), true, reason);
            return true;
        }
        if st.inc.as_ref().is_some_and(|i| i.transfer_id == transfer_id) {
            self.abort_incoming(&mut st, true, reason);
            return true;
        }
        false
    }

    fn on_answer(&self, a: &Answer) -> Result<(), RtcError> {
        let mut st = self.st.lock().unwrap();
        if st.phase != SessionState::Ready {
            return Err(RtcError::protocol("answer before authentication"));
        }
        let Some(out) = st.out.as_mut() else { return Ok(()) };
        // Answers for other transfers are stale (sent before our cancel arrived).
        if out.transfer_id != a.transfer_id || out.answered || out.aborted.is_some() {
            return Ok(());
        }
        if a.declined {
            if !out.accepted.is_empty() {
                return Err(RtcError::protocol("declining answer after accepting answer frames"));
            }
            out.answered = true;
            if let Some(tx) = out.answer_tx.take() {
                let _ = tx.send(Answered { declined: true });
            }
            return Ok(());
        }
        for id in &a.accept {
            let Some(size) = out.sizes.get(id).copied() else {
                return Err(RtcError::protocol(format!("answer accepts unknown file {}", q(id))));
            };
            if !out.accepted.insert(id.clone()) {
                return Err(RtcError::protocol(format!("answer accepts {} twice", q(id))));
            }
            let offset = a.offset_of(id);
            if offset > size {
                return Err(RtcError::protocol(format!("offset beyond the end of {}", q(id))));
            }
            if offset > 0 {
                out.offsets.push((id.clone(), offset));
            }
        }
        if a.more {
            return Ok(());
        }
        out.answered = true;
        if let Some(tx) = out.answer_tx.take() {
            let _ = tx.send(Answered { declined: false });
        }
        Ok(())
    }

    fn on_file_ack(&self, id: &str, ok: bool, sha256: Option<String>, error: Option<String>) -> Result<(), RtcError> {
        let mut st = self.st.lock().unwrap();
        if st.phase != SessionState::Ready {
            return Err(RtcError::protocol("file-ack before authentication"));
        }
        let Some(out) = st.out.as_mut() else { return Ok(()) };
        if !out.acks.contains_key(id) {
            return Ok(()); // stale ack from a cancelled transfer
        }
        if !out.ended.contains(id) {
            return Err(RtcError::protocol(format!("file-ack for {} before its file-end", q(id))));
        }
        if let Some(tx) = out.acks.remove(id) {
            let _ = tx.send(FileAck { ok, sha256, error });
        }
        Ok(())
    }

    fn on_progress(&self, transfer_id: &str, bytes: u64) -> Result<(), RtcError> {
        let mut st = self.st.lock().unwrap();
        if st.phase != SessionState::Ready {
            return Err(RtcError::protocol("progress before authentication"));
        }
        let Some(out) = st.out.as_mut() else { return Ok(()) };
        if out.transfer_id != transfer_id || out.aborted.is_some() {
            return Ok(());
        }
        if bytes > out.sent {
            return Err(RtcError::protocol("progress beyond the bytes sent"));
        }
        if bytes > out.acked {
            out.acked = bytes;
            self.wake.notify_waiters();
        }
        Ok(())
    }

    // ── Worker: handshake ────────────────────────────────────────────────

    async fn send_hello(&self) -> Result<(), RtcError> {
        let identity = &self.opts.identity;
        let key = identity.public_key().to_string();
        self.st.lock().unwrap().local_key = Some(identity.public_key_raw().to_vec());
        let d = &self.opts.device;
        let hello = Control::Hello(Hello {
            alg: identity.alg(),
            key,
            nonce: b64::encode(&self.nonce),
            device: DeviceInfo {
                alias: clip_utf16(&d.alias, MAX_ALIAS_LENGTH),
                device_type: clip_utf16(&d.device_type, MAX_DEVICE_TYPE_LENGTH),
                platform: clip_utf16(&d.platform, MAX_PLATFORM_LENGTH),
            },
            caps: self.opts.caps.iter().take(MAX_CAPS).map(|c| clip_utf16(c, MAX_CAP_LENGTH)).collect(),
        });
        self.send_control(&hello).await
    }

    async fn on_hello(&self, hello: Hello) -> Result<(), RtcError> {
        let auth = {
            let mut st = self.st.lock().unwrap();
            if st.remote_hello.is_some() {
                return Err(RtcError::protocol("duplicate hello"));
            }
            let Some(local_key) = st.local_key.clone() else {
                return Err(RtcError::new("internal", "local hello not sent"));
            };
            let remote_nonce = b64::decode(&hello.nonce).unwrap_or_default();
            if remote_nonce == self.nonce {
                return Err(RtcError::new("auth", "reflected handshake"));
            }
            if self.opts.expected_peer_key.as_ref().is_some_and(|k| *k != hello.key) {
                return Err(RtcError::new("auth", "unexpected peer key"));
            }
            let remote_key = b64::decode(&hello.key).unwrap_or_default();
            let offerer = self.opts.role == Role::Offerer;
            let (lf, rf) = (self.opts.local_fingerprint.as_str(), self.opts.remote_fingerprint.as_str());
            let t = transcript_hash(&TranscriptParts {
                session_id: &self.opts.session_id,
                fp_offerer: if offerer { lf } else { rf },
                fp_answerer: if offerer { rf } else { lf },
                nonce_offerer: if offerer { &self.nonce } else { &remote_nonce },
                nonce_answerer: if offerer { &remote_nonce } else { &self.nonce },
                key_offerer: if offerer { &local_key } else { &remote_key },
                key_answerer: if offerer { &remote_key } else { &local_key },
            });
            st.transcript = Some(t);
            st.remote_hello = Some(hello);
            Control::Auth { sig: self.opts.identity.sign(&auth_payload(self.opts.role, &t)), mac: self.room_key.map(|k| room_mac(&k, &t)) }
        };
        self.send_control(&auth).await
    }

    fn on_auth(&self, sig: &str, mac: Option<&str>) -> Result<(), RtcError> {
        let mut st = self.st.lock().unwrap();
        let (Some(hello), Some(t)) = (st.remote_hello.clone(), st.transcript) else {
            return Err(RtcError::protocol("auth before hello"));
        };
        let mut ok = verify(hello.alg, &hello.key, &auth_payload(self.opts.role.other(), &t), sig);
        if ok && let Some(key) = &self.room_key {
            ok = mac.is_some_and(|m| verify_room_mac(key, &t, m));
        }
        if !ok {
            return Err(RtcError::new("auth", "peer authentication failed"));
        }
        if st.phase != SessionState::Handshake {
            return Ok(());
        }
        st.phase = SessionState::Ready;
        let peer = RemotePeer {
            alg: hello.alg,
            key: hello.key.clone(),
            device: hello.device.clone(),
            caps: hello.caps.clone(),
            transcript: t,
            short_code: short_code(&t),
            room_verified: self.room_key.is_some(),
        };
        st.peer = Some(peer.clone());
        drop(st);
        self.ready.send_replace(ReadyState::Ready(peer.clone()));
        self.emit(SessionEvent::Ready(peer));
        Ok(())
    }

    // ── Worker: receiving ────────────────────────────────────────────────

    fn on_offer(self: &Arc<Self>, offer: Offer, cost: usize) -> Result<Option<IncomingOffer>, RtcError> {
        let mut st = self.st.lock().unwrap();
        let continuing = st.inc.as_ref().is_some_and(|i| !i.announced && !i.answered && i.transfer_id == offer.transfer_id);
        if continuing {
            if offer.text.is_some() {
                return Err(RtcError::protocol("text in an offer continuation"));
            }
        } else {
            if st.inc.is_some() {
                return Err(RtcError::protocol("offer while another incoming transfer is active"));
            }
            st.discard = None;
            st.inc = Some(Inc {
                transfer_id: offer.transfer_id.clone(),
                files: HashMap::new(),
                metas: Vec::new(),
                text: offer.text.clone(),
                offer_bytes: 0,
                total_size: 0,
                announced: false,
                answered: false,
                accepted: IndexMap::new(),
                finished: HashSet::new(),
                completed: Vec::new(),
                failed: Vec::new(),
                current: None,
                cancel: Token::new(),
                processed: 0,
                reported: 0,
            });
        }
        let Some(inc) = st.inc.as_mut() else { return Ok(None) };
        inc.offer_bytes += cost;
        if inc.offer_bytes > MAX_OFFER_BYTES {
            return Err(RtcError::protocol("offer too large"));
        }
        if inc.metas.len() + offer.files.len() > MAX_FILES {
            return Err(RtcError::protocol(format!("too many files (max {MAX_FILES})")));
        }
        for f in offer.files {
            if inc.files.contains_key(&f.id) {
                return Err(RtcError::protocol(format!("duplicate file id {}", q(&f.id))));
            }
            inc.total_size += f.size;
            if inc.total_size > MAX_SAFE_INTEGER {
                return Err(RtcError::protocol("total size too large"));
            }
            inc.files.insert(f.id.clone(), f.clone());
            inc.metas.push(f);
        }
        if offer.more {
            return Ok(None);
        }
        inc.announced = true;
        let Some(peer) = st.peer.clone() else { return Err(RtcError::protocol("offer before authentication")) };
        let Some(inc) = st.inc.as_ref() else { return Ok(None) };
        let expires_at = self.opts.decision_timeout.map(|d| Instant::now() + d);
        Ok(Some(IncomingOffer {
            transfer_id: inc.transfer_id.clone(),
            files: inc.metas.clone(),
            text: inc.text.clone(),
            peer,
            expires_at,
            session: PeerSession { inner: self.clone() },
        }))
    }

    fn expire_offer(&self, transfer_id: &str, cancel: &Token) -> Option<Control> {
        let mut st = self.st.lock().unwrap();
        let inc = st.inc.as_ref()?;
        if inc.transfer_id != transfer_id || &inc.cancel != cancel || inc.answered || st.phase != SessionState::Ready {
            return None;
        }
        self.abort_incoming(&mut st, false, Some("timeout"))
    }
}

/// The receiving side of the file currently being streamed to us.
struct Cur {
    transfer_id: String,
    meta: FileMeta,
    received: u64,
    hasher: Sha256,
    writer: Option<Box<dyn SinkWriter>>,
    error: Option<String>,
    cancel: Token,
}

async fn abort_cur(cur: &mut Option<Cur>, reason: AbortReason) {
    if let Some(mut c) = cur.take()
        && let Some(w) = c.writer.take()
    {
        w.abort(reason).await;
    }
}

async fn reader(inner: Arc<Inner>) {
    loop {
        let frame = tokio::select! {
            f = inner.transport.recv() => f,
            _ = inner.closed.cancelled() => return,
        };
        let Some(frame) = frame else {
            inner.shutdown(CloseReason::Remote, Some("data channel closed".into()));
            return;
        };
        inner.touch();
        let result = match frame {
            Frame::Text(text) => inner.on_text(text).await,
            Frame::Binary(data) => inner.on_binary_frame(data),
        };
        if let Err(err) = result {
            inner.fail(err).await;
            return;
        }
    }
}

async fn timer(inner: Arc<Inner>) {
    let opts = &inner.opts;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + opts.ping_interval, opts.ping_interval);
    let step = (opts.timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
    let mut check = tokio::time::interval(step);
    let handshake_deadline = Instant::now() + opts.handshake_timeout;
    loop {
        tokio::select! {
            _ = inner.closed.cancelled() => return,
            _ = ping.tick() => {
                // Off this loop: a ping stuck on a dead channel must not hold up
                // the timeout check below, which is what closes that channel.
                let inner = inner.clone();
                tokio::spawn(async move { let _ = inner.send_control(&Control::Ping).await; });
            }
            _ = check.tick() => {
                if inner.last_inbound.lock().unwrap().elapsed() > opts.timeout {
                    inner.shutdown(CloseReason::Timeout, Some("no frames from the peer".into()));
                    return;
                }
                if inner.st.lock().unwrap().phase == SessionState::Handshake && Instant::now() > handshake_deadline {
                    inner.shutdown(CloseReason::Timeout, Some("handshake timed out".into()));
                    return;
                }
            }
        }
    }
}

async fn worker(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Work>) {
    let mut cur: Option<Cur> = None;
    if let Err(err) = inner.send_hello().await {
        inner.fail(err).await;
        return;
    }
    loop {
        let cur_cancel = cur.as_ref().map(|c| c.cancel.clone()).unwrap_or_default();
        let has_cur = cur.is_some();
        let item = tokio::select! {
            item = rx.recv() => item,
            _ = cur_cancel.cancelled(), if has_cur => {
                // The transfer was cancelled (or the session closed) mid-file.
                let reason = close_abort_reason(&inner);
                abort_cur(&mut cur, reason).await;
                continue;
            }
            _ = inner.closed.cancelled() => None,
        };
        let Some(item) = item else {
            abort_cur(&mut cur, close_abort_reason(&inner)).await;
            return;
        };
        let result = match item {
            Work::Control(msg, cost) => {
                let r = handle_control(&inner, &mut cur, msg, cost).await;
                inner.queued_control.fetch_sub(cost, Ordering::Relaxed);
                r
            }
            Work::Binary(data) => {
                let size = data.len();
                let r = on_binary(&inner, &mut cur, data).await;
                inner.queued_binary.fetch_sub(size, Ordering::Relaxed);
                r
            }
        };
        if let Err(err) = result {
            if err.code == "overrun" {
                abort_cur(&mut cur, AbortReason::Overrun).await;
            } else {
                abort_cur(&mut cur, AbortReason::Error).await;
            }
            inner.fail(err).await;
            return;
        }
    }
}

/// What a writer interrupted by a close is aborted with (resumable data is kept).
fn close_abort_reason(inner: &Inner) -> AbortReason {
    match inner.st.lock().unwrap().close_reason {
        Some(CloseReason::Timeout) => AbortReason::Timeout,
        Some(_) => AbortReason::Closed,
        None => AbortReason::Cancelled,
    }
}

async fn handle_control(inner: &Arc<Inner>, cur: &mut Option<Cur>, msg: Control, cost: usize) -> Result<(), RtcError> {
    let phase = inner.st.lock().unwrap().phase;
    if phase == SessionState::Closed {
        return Ok(());
    }
    if phase == SessionState::Handshake {
        return match msg {
            Control::Hello(h) => inner.on_hello(h).await,
            Control::Auth { sig, mac } => inner.on_auth(&sig, mac.as_deref()),
            other => Err(RtcError::protocol(format!("unexpected \"{}\" before authentication", other.kind()))),
        };
    }
    match msg {
        Control::Offer(offer) => {
            let Some(incoming) = inner.on_offer(offer, cost)? else { return Ok(()) };
            let transfer_id = incoming.transfer_id.clone();
            let cancel = inner.st.lock().unwrap().inc.as_ref().map(|i| i.cancel.clone()).unwrap_or_default();
            if let Some(timeout) = inner.opts.decision_timeout {
                let weak = Arc::downgrade(inner);
                let id = transfer_id.clone();
                let token = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(timeout) => {}
                        _ = token.cancelled() => return,
                    }
                    let Some(inner) = weak.upgrade() else { return };
                    if let Some(frame) = inner.expire_offer(&id, &token) {
                        let _ = inner.send_control(&frame).await;
                    }
                });
            }
            if inner.events.send(SessionEvent::Offer(incoming)).is_err() {
                // Nobody can decide: never auto-accept.
                let _ = (PeerSession { inner: inner.clone() }).decline_offer(&transfer_id).await;
            }
            Ok(())
        }
        Control::File { id, offset } => on_file_start(inner, cur, &id, offset).await,
        Control::FileEnd { id, sha256 } => on_file_end(inner, cur, &id, &sha256).await,
        Control::Done { transfer_id } => on_done(inner, &transfer_id),
        Control::Cancel { transfer_id, reason } => {
            inner.on_remote_cancel(&transfer_id, reason.as_deref()); // stale when it still names nothing
            Ok(())
        }
        Control::Hello(_) => Err(RtcError::protocol("duplicate hello")),
        other => Err(RtcError::protocol(format!("unexpected \"{}\" message", other.kind()))),
    }
}

async fn on_file_start(inner: &Arc<Inner>, cur: &mut Option<Cur>, id: &str, offset: u64) -> Result<(), RtcError> {
    // A writer left over from a cancelled transfer goes first.
    if cur.as_ref().is_some_and(|c| c.cancel.is_cancelled()) {
        abort_cur(cur, AbortReason::Cancelled).await;
    }
    let (meta, ctx, cancel) = {
        let mut st = inner.st.lock().unwrap();
        let discarding = st.discard.is_some();
        let peer_key = st.peer.as_ref().map(|p| p.key.clone()).unwrap_or_default();
        let Some(inc) = st.inc.as_mut() else {
            if discarding {
                return Ok(());
            }
            return Err(RtcError::protocol("file without an accepted transfer"));
        };
        if !inc.answered || inc.current.is_some() {
            return Err(RtcError::protocol("unexpected file message"));
        }
        let Some(expected) = inc.accepted.get(id).copied().filter(|_| !inc.finished.contains(id)) else {
            return Err(RtcError::protocol(format!("file {} was not accepted", q(id))));
        };
        if offset != expected {
            return Err(RtcError::protocol(format!("file {} starts at the wrong offset", q(id))));
        }
        inc.current = Some(id.to_string());
        let meta = inc.files.get(id).cloned().ok_or_else(|| RtcError::protocol("unknown file"))?;
        (meta, SinkContext { transfer_id: inc.transfer_id.clone(), peer_key }, inc.cancel.clone())
    };
    let mut c = Cur {
        transfer_id: ctx.transfer_id.clone(),
        meta: meta.clone(),
        received: offset,
        hasher: Sha256::new(),
        writer: None,
        error: None,
        cancel: cancel.clone(),
    };
    let open = async {
        let Some(sink) = inner.opts.sink.clone() else { return Err("no file sink configured".to_string()) };
        if offset > 0 {
            c.hasher = sink.hash_prefix(&meta, offset, &ctx).await?;
        }
        sink.open(&meta, offset, &ctx).await
    };
    let opened = tokio::select! {
        r = open => Some(r),
        _ = cancel.cancelled() => None,
    };
    match opened {
        None => return Ok(()),
        Some(Ok(writer)) => {
            if cancel.is_cancelled() {
                writer.abort(close_abort_reason(inner)).await;
                return Ok(());
            }
            c.writer = Some(writer);
        }
        // The file fails, the transfer goes on: its bytes are consumed and nacked.
        Some(Err(e)) => c.error = Some(e),
    }
    *cur = Some(c);
    Ok(())
}

async fn on_binary(inner: &Arc<Inner>, cur: &mut Option<Cur>, chunk: Bytes) -> Result<(), RtcError> {
    let current_matches = {
        let st = inner.st.lock().unwrap();
        match (&st.inc, cur.as_ref()) {
            (Some(inc), Some(c)) => inc.transfer_id == c.transfer_id && inc.current.as_deref() == Some(c.meta.id.as_str()),
            (None, _) if st.discard.is_some() => return Ok(()), // tail of a cancelled transfer
            _ => false,
        }
    };
    let Some(c) = cur.as_mut().filter(|_| current_matches) else {
        return Err(RtcError::protocol("binary frame outside a file"));
    };
    let len = chunk.len() as u64;
    if c.received + len > c.meta.size {
        return Err(RtcError::new("overrun", format!("file {} exceeds its declared size", q(&c.meta.id))));
    }
    c.hasher.update(&chunk);
    c.received += len;
    if c.error.is_none()
        && let Some(writer) = c.writer.as_mut()
    {
        let written = tokio::select! {
            r = writer.write(chunk) => Some(r),
            _ = c.cancel.cancelled() => None,
        };
        match written {
            None => return Ok(()),
            Some(Ok(())) => {}
            Some(Err(e)) => {
                c.error = Some(e);
                if let Some(w) = c.writer.take() {
                    w.abort(AbortReason::Error).await;
                }
            }
        }
    }
    let report = {
        let mut st = inner.st.lock().unwrap();
        let Some(inc) = st.inc.as_mut().filter(|i| i.transfer_id == c.transfer_id) else { return Ok(()) };
        inc.processed += len;
        let processed = inc.processed;
        let report = (processed - inc.reported >= PROGRESS_STEP).then(|| {
            inc.reported = processed;
            Control::Progress { transfer_id: inc.transfer_id.clone(), bytes: processed }
        });
        let (tid, fid, received, size) = (c.transfer_id.clone(), c.meta.id.clone(), c.received, c.meta.size);
        inner.progress(&mut st, Direction::Receive, &tid, &fid, received, size);
        report
    };
    if let Some(msg) = report {
        let _ = inner.send_control(&msg).await;
    }
    Ok(())
}

async fn on_file_end(inner: &Arc<Inner>, cur: &mut Option<Cur>, id: &str, sha256: &str) -> Result<(), RtcError> {
    {
        let mut st = inner.st.lock().unwrap();
        let discarding = st.discard.is_some();
        let Some(inc) = st.inc.as_mut() else {
            if discarding {
                return Ok(());
            }
            return Err(RtcError::protocol("file-end without a transfer"));
        };
        let open = cur.as_ref().is_some_and(|c| c.meta.id == id && c.transfer_id == inc.transfer_id) && inc.current.as_deref() == Some(id);
        if !open {
            return Err(RtcError::protocol("file-end for a file that is not open"));
        }
        inc.current = None;
        inc.finished.insert(id.to_string());
    }
    let Some(mut c) = cur.take() else { return Ok(()) };
    let digest = hex::encode(c.hasher.finalize_reset());
    let mut error = c.error.clone();
    if error.is_none() && c.received != c.meta.size {
        error = Some("size mismatch".into());
    }
    if error.is_none() && digest != sha256 {
        error = Some("sha256 mismatch".into());
    }
    let writer = c.writer.take();
    if error.is_some() {
        if let Some(w) = writer {
            w.abort(if c.error.is_some() { AbortReason::Error } else { AbortReason::Integrity }).await;
        }
    } else if let Some(w) = writer {
        if let Err(e) = w.close().await {
            error = Some(format!("commit failed: {e}"));
        }
    } else {
        error = Some("file was not opened".into());
    }
    let (transfer_id, ok) = {
        let mut st = inner.st.lock().unwrap();
        let Some(inc) = st.inc.as_mut().filter(|i| i.transfer_id == c.transfer_id) else { return Ok(()) };
        let ok = error.is_none();
        if ok {
            inc.completed.push(id.to_string())
        } else {
            inc.failed.push(id.to_string())
        }
        (inc.transfer_id.clone(), ok)
    };
    let error = error.map(|e| clip_utf16(&e, MAX_REASON_LENGTH));
    inner.send_control(&Control::FileAck { id: id.to_string(), ok, sha256: Some(digest.clone()), error: error.clone() }).await?;
    inner.emit(SessionEvent::FileComplete {
        transfer_id,
        direction: Direction::Receive,
        file_id: id.to_string(),
        ok,
        sha256: Some(digest),
        error,
    });
    Ok(())
}

fn on_done(inner: &Inner, transfer_id: &str) -> Result<(), RtcError> {
    let mut st = inner.st.lock().unwrap();
    let discarding = st.discard.is_some();
    let Some(inc) = st.inc.as_ref() else {
        if discarding {
            return Ok(()); // crossed our cancel
        }
        return Err(RtcError::protocol("done without a transfer"));
    };
    if inc.transfer_id != transfer_id {
        return Err(RtcError::protocol("done for another transfer"));
    }
    if !inc.answered || inc.current.is_some() {
        return Err(RtcError::protocol("unexpected done"));
    }
    let Some(mut inc) = st.inc.take() else { return Ok(()) };
    let missing: Vec<String> = inc.accepted.keys().filter(|id| !inc.finished.contains(*id)).cloned().collect();
    for id in missing {
        inc.failed.push(id.clone());
        inner.emit(SessionEvent::FileComplete {
            transfer_id: inc.transfer_id.clone(),
            direction: Direction::Receive,
            file_id: id,
            ok: false,
            sha256: None,
            error: Some("not sent".into()),
        });
    }
    let skipped = inc.metas.iter().filter(|m| !inc.accepted.contains_key(&m.id)).map(|m| m.id.clone()).collect();
    inc.cancel.cancel();
    inner.emit(SessionEvent::Done {
        transfer_id: inc.transfer_id,
        direction: Direction::Receive,
        completed: inc.completed,
        failed: inc.failed,
        skipped,
    });
    Ok(())
}

// ── Sending ───────────────────────────────────────────────────────────────

impl Inner {
    async fn run_transfer(self: &Arc<Self>, request: TransferRequest, token: Token) -> Result<TransferOutcome, RtcError> {
        let metas: Vec<FileMeta> = request.files.iter().map(|f| f.meta.clone()).collect();
        validate_metas(&metas).map_err(|e| RtcError::new("invalid", e.message))?;
        if let Some(text) = &request.text
            && q(text).len() > MAX_TEXT_BYTES
        {
            return Err(RtcError::new("too-large", format!("text exceeds {MAX_TEXT_BYTES} bytes")));
        }
        let transfer_id = request.transfer_id.clone();
        let frames = encode_offer(&transfer_id, &metas, request.text.as_deref())?;
        let (answer_tx, answer_rx) = oneshot::channel();
        {
            let mut st = self.st.lock().unwrap();
            if st.phase != SessionState::Ready {
                return Err(RtcError::new("closed", "session closed"));
            }
            st.out = Some(Out {
                transfer_id: transfer_id.clone(),
                sizes: metas.iter().map(|m| (m.id.clone(), m.size)).collect(),
                accepted: IndexSet::new(),
                offsets: Vec::new(),
                answered: false,
                answer_tx: Some(answer_tx),
                acks: HashMap::new(),
                ended: HashSet::new(),
                aborted: None,
                cancel: token.clone(),
                sent: 0,
                acked: 0,
            });
        }
        let result = self.stream_transfer(&request, &metas, frames, answer_rx, &token).await;
        let result = match result {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                let mut frame = None;
                let aborted = {
                    let mut st = self.st.lock().unwrap();
                    let mine = st.out.as_ref().is_some_and(|o| o.cancel == token);
                    let aborted = st.out.as_ref().filter(|_| mine).and_then(|o| o.aborted.clone());
                    if aborted.is_none() && mine && st.phase == SessionState::Ready && !self.closed.is_cancelled() {
                        if err.code == "closed" {
                            drop(st);
                            self.shutdown(CloseReason::Remote, Some("data channel closed".into()));
                            None
                        } else {
                            // A local failure (e.g. the source became unreadable): tell the peer.
                            frame = self.abort_outgoing(&mut st, err.clone(), false, Some(&err.message));
                            Some(err.clone())
                        }
                    } else {
                        aborted
                    }
                };
                if let Some(f) = frame {
                    let _ = self.send_control(&f).await;
                }
                Err(aborted.unwrap_or(err))
            }
        };
        let mut st = self.st.lock().unwrap();
        if st.out.as_ref().is_some_and(|o| o.cancel == token) {
            st.out = None;
        }
        result
    }

    /// Runs `fut` unless the transfer is aborted or the session closes.
    async fn race<T>(&self, token: &Token, fut: impl std::future::Future<Output = T>) -> Result<T, RtcError> {
        tokio::select! {
            v = fut => Ok(v),
            _ = token.cancelled() => Err(self.outgoing_error(token)),
            _ = self.closed.cancelled() => Err(self.outgoing_error(token)),
        }
    }

    fn check_outgoing(&self, token: &Token) -> Result<(), RtcError> {
        if token.is_cancelled() || self.closed.is_cancelled() {
            return Err(self.outgoing_error(token));
        }
        Ok(())
    }

    async fn stream_transfer(
        self: &Arc<Self>,
        request: &TransferRequest,
        metas: &[FileMeta],
        frames: Vec<String>,
        answer_rx: oneshot::Receiver<Answered>,
        token: &Token,
    ) -> Result<TransferOutcome, RtcError> {
        let transfer_id = request.transfer_id.clone();
        for frame in frames {
            self.check_outgoing(token)?;
            self.send_raw(frame).await?;
        }
        let answer = self.race(token, answer_rx).await?.map_err(|_| self.outgoing_error(token))?;
        let order: Vec<String> = metas.iter().map(|m| m.id.clone()).collect();
        if answer.declined {
            return Ok(TransferOutcome { transfer_id, declined: true, completed: vec![], failed: vec![], skipped: order });
        }
        let (accepted, offsets) = {
            let st = self.st.lock().unwrap();
            let out = st.out.as_ref().ok_or_else(|| self.outgoing_error(token))?;
            (order.iter().filter(|id| out.accepted.contains(*id)).cloned().collect::<Vec<_>>(), out.offsets.clone())
        };
        let skipped: Vec<String> = order.iter().filter(|id| !accepted.contains(id)).cloned().collect();
        self.emit(SessionEvent::Accepted { transfer_id: transfer_id.clone(), files: accepted.clone(), offsets: offsets.clone() });

        let results: Arc<Mutex<(Vec<String>, Vec<String>)>> = Arc::default();
        let mut acks = Vec::new();
        for id in &accepted {
            let Some(file) = request.files.iter().find(|f| &f.meta.id == id) else { continue };
            let offset = offsets.iter().rev().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0);
            let (ack_tx, ack_rx) = oneshot::channel();
            if let Some(out) = self.st.lock().unwrap().out.as_mut() {
                out.acks.insert(id.clone(), ack_tx);
            }
            let digest = self.stream_file(token, file, offset).await?;
            let inner = self.clone();
            let results = results.clone();
            let (tid, fid) = (transfer_id.clone(), id.clone());
            acks.push(tokio::spawn(async move {
                let Ok(ack) = ack_rx.await else { return };
                let ok = ack.ok && ack.sha256.as_ref().is_none_or(|h| *h == digest);
                let error = (!ok).then(|| {
                    if ack.ok {
                        "the receiver computed a different SHA-256".to_string()
                    } else {
                        ack.error.unwrap_or_else(|| "rejected by the receiver".into())
                    }
                });
                {
                    let mut r = results.lock().unwrap();
                    if ok { r.0.push(fid.clone()) } else { r.1.push(fid.clone()) }
                }
                inner.emit(SessionEvent::FileComplete {
                    transfer_id: tid,
                    direction: Direction::Send,
                    file_id: fid,
                    ok,
                    sha256: Some(digest),
                    error,
                });
            }));
        }
        for ack in acks {
            self.race(token, ack).await?.map_err(|e| RtcError::new("internal", e.to_string()))?;
        }
        self.check_outgoing(token)?;
        self.send_control(&Control::Done { transfer_id: transfer_id.clone() }).await?;
        let (mut completed, mut failed) = results.lock().unwrap().clone();
        let index = |id: &String| order.iter().position(|o| o == id);
        completed.sort_by_key(index);
        failed.sort_by_key(index);
        self.emit(SessionEvent::Done {
            transfer_id: transfer_id.clone(),
            direction: Direction::Send,
            completed: completed.clone(),
            failed: failed.clone(),
            skipped: skipped.clone(),
        });
        Ok(TransferOutcome { transfer_id, declined: false, completed, failed, skipped })
    }

    async fn read(&self, token: &Token, file: &OutgoingFile, start: u64, end: u64) -> Result<Bytes, RtcError> {
        let where_ = format!("{} [{start}, {end})", q(&file.meta.id));
        match self.race(token, file.source.read(start, end)).await? {
            Ok(b) if b.len() as u64 == end - start => Ok(b),
            Ok(_) => Err(RtcError::new("source", format!("could not read {where_}"))),
            Err(e) => Err(RtcError::new("source", format!("could not read {where_}: {e}"))),
        }
    }

    /// Streams one file from `offset`; returns the full-file SHA-256 (hex).
    async fn stream_file(self: &Arc<Self>, token: &Token, file: &OutgoingFile, offset: u64) -> Result<String, RtcError> {
        let id = &file.meta.id;
        let size = file.meta.size;
        self.send_control(&Control::File { id: id.clone(), offset }).await?;
        let mut hasher = Sha256::new();
        // Resume: hash the prefix the receiver already has.
        let mut pos = 0;
        while pos < offset {
            let end = offset.min(pos + READ_BLOCK);
            hasher.update(self.read(token, file, pos, end).await?);
            pos = end;
        }
        let transfer_id = token_transfer(self, token).unwrap_or_default();
        let mut pos = offset;
        // One block of read-ahead.
        let spawn_read = |start: u64| {
            let source = file.source.clone();
            let end = size.min(start + READ_BLOCK);
            tokio::spawn(async move { (start, end, source.read(start, end).await) })
        };
        let mut next = (pos < size).then(|| spawn_read(pos));
        while let Some(handle) = next.take() {
            let (start, end, data) = self.race(token, handle).await?.map_err(|e| RtcError::new("source", e.to_string()))?;
            let block = match data {
                Ok(b) if b.len() as u64 == end - start => b,
                Ok(_) => return Err(RtcError::new("source", format!("could not read {} [{start}, {end})", q(id)))),
                Err(e) => return Err(RtcError::new("source", format!("could not read {} [{start}, {end}): {e}", q(id)))),
            };
            if end < size {
                next = Some(spawn_read(end));
            }
            let mut o = 0;
            while o < block.len() {
                let chunk = block.slice(o..block.len().min(o + self.chunk_size));
                o += chunk.len();
                self.clear_to_send(token, chunk.len() as u64).await?;
                hasher.update(&chunk);
                {
                    let mut st = self.st.lock().unwrap();
                    if let Some(out) = st.out.as_mut() {
                        out.sent += chunk.len() as u64;
                    }
                }
                pos += chunk.len() as u64;
                if let Err(e) = self.write(Frame::Binary(chunk)).await {
                    return Err(if self.closed.is_cancelled() { self.outgoing_error(token) } else { e });
                }
                let mut st = self.st.lock().unwrap();
                self.progress(&mut st, Direction::Send, &transfer_id, id, pos, size);
            }
        }
        let digest = hex::encode(hasher.finalize());
        self.send_control(&Control::FileEnd { id: id.clone(), sha256: digest.clone() }).await?;
        if let Some(out) = self.st.lock().unwrap().out.as_mut() {
            out.ended.insert(id.clone());
        }
        Ok(digest)
    }

    /// Waits until `size` more bytes may be sent: channel buffer drained below
    /// the mark and flow-control window open.
    async fn clear_to_send(&self, token: &Token, size: u64) -> Result<(), RtcError> {
        if self.transport.buffered_amount() > BUFFER_HIGH_WATER {
            loop {
                let low = self.transport.buffered_low().notified();
                tokio::pin!(low);
                low.as_mut().enable();
                if self.transport.buffered_amount() <= BUFFER_LOW_WATER {
                    break;
                }
                self.check_outgoing(token)?;
                self.race(token, async {
                    tokio::select! {
                        _ = low => {}
                        _ = tokio::time::sleep(DRAIN_POLL) => {}
                    }
                })
                .await?;
            }
        }
        loop {
            let woken = self.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            let open = {
                let st = self.st.lock().unwrap();
                let Some(out) = st.out.as_ref() else { return Err(self.outgoing_error(token)) };
                out.sent + size <= out.acked + RECV_WINDOW
            };
            if open {
                break;
            }
            self.check_outgoing(token)?;
            self.race(token, async {
                tokio::select! {
                    _ = woken => {}
                    _ = tokio::time::sleep(DRAIN_POLL) => {}
                }
            })
            .await?;
        }
        self.check_outgoing(token)
    }
}

fn token_transfer(inner: &Inner, token: &Token) -> Option<String> {
    inner.st.lock().unwrap().out.as_ref().filter(|o| &o.cancel == token).map(|o| o.transfer_id.clone())
}
