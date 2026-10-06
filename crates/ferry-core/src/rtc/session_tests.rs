//! Session behaviour over an in-memory channel pair (no WebRTC).

use super::identity::RtcIdentity;
use super::protocol::*;
use super::session::*;
use super::transcript::Role;
use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

type Transform = Box<dyn Fn(String) -> String + Send>;

/// One end of an in-memory data channel. `buffered_amount` counts bytes the
/// peer has not read yet; `blackhole` drops everything sent.
pub(crate) struct MemChannel {
    tx: mpsc::UnboundedSender<Frame>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Frame>>,
    /// Bytes this end sent that the other end hasn't read.
    sent_unread: Arc<AtomicUsize>,
    /// Bytes the other end sent that we haven't read.
    peer_unread: Arc<AtomicUsize>,
    low: Arc<Notify>,
    peer_low: Arc<Notify>,
    closed: Arc<AtomicBool>,
    pub blackhole: AtomicBool,
    pub max_buffered: AtomicUsize,
    /// Rewrites outgoing text frames (fault injection).
    pub transform: Mutex<Option<Transform>>,
}

pub(crate) fn mem_pair() -> (Arc<MemChannel>, Arc<MemChannel>) {
    let (atx, arx) = mpsc::unbounded_channel();
    let (btx, brx) = mpsc::unbounded_channel();
    let (a_unread, b_unread) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (a_low, b_low) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let a = MemChannel {
        tx: atx,
        rx: tokio::sync::Mutex::new(brx),
        sent_unread: a_unread.clone(),
        peer_unread: b_unread.clone(),
        low: a_low.clone(),
        peer_low: b_low.clone(),
        closed: closed.clone(),
        blackhole: AtomicBool::new(false),
        max_buffered: AtomicUsize::new(0),
        transform: Mutex::new(None),
    };
    let b = MemChannel {
        tx: btx,
        rx: tokio::sync::Mutex::new(arx),
        sent_unread: b_unread,
        peer_unread: a_unread,
        low: b_low,
        peer_low: a_low,
        closed,
        blackhole: AtomicBool::new(false),
        max_buffered: AtomicUsize::new(0),
        transform: Mutex::new(None),
    };
    (Arc::new(a), Arc::new(b))
}

fn frame_len(f: &Frame) -> usize {
    match f {
        Frame::Text(t) => t.len(),
        Frame::Binary(b) => b.len(),
    }
}

#[async_trait]
impl Transport for MemChannel {
    async fn send(&self, frame: Frame) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("closed".into());
        }
        if self.blackhole.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = match (frame, self.transform.lock().unwrap().as_ref()) {
            (Frame::Text(t), Some(f)) => Frame::Text(f(t)),
            (f, _) => f,
        };
        let n = self.sent_unread.fetch_add(frame_len(&frame), Ordering::SeqCst) + frame_len(&frame);
        self.max_buffered.fetch_max(n, Ordering::SeqCst);
        self.tx.send(frame).map_err(|_| "closed".to_string())
    }

    fn buffered_amount(&self) -> usize {
        self.sent_unread.load(Ordering::SeqCst)
    }

    fn buffered_low(&self) -> &Notify {
        &self.low
    }

    async fn recv(&self) -> Option<Frame> {
        let mut rx = self.rx.lock().await;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            tokio::select! {
                f = rx.recv() => {
                    let f = f?;
                    let before = self.peer_unread.fetch_sub(frame_len(&f), Ordering::SeqCst);
                    let after = before - frame_len(&f);
                    if after <= BUFFER_LOW_WATER && before > BUFFER_LOW_WATER {
                        self.peer_low.notify_waiters();
                    }
                    return Some(f);
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

// ── Memory sources and sinks ──────────────────────────────────────────────

struct MemSource(Bytes);

#[async_trait]
impl FileSource for MemSource {
    async fn read(&self, start: u64, end: u64) -> std::io::Result<Bytes> {
        Ok(self.0.slice(start as usize..end as usize))
    }
}

pub(crate) fn file(id: &str, data: &[u8]) -> OutgoingFile {
    OutgoingFile {
        meta: FileMeta {
            id: id.into(),
            name: format!("{id}.bin"),
            size: data.len() as u64,
            mime: "application/octet-stream".into(),
            modified: None,
        },
        source: Arc::new(MemSource(Bytes::copy_from_slice(data))),
    }
}

#[derive(Default)]
pub(crate) struct MemSink {
    pub files: Mutex<HashMap<String, Vec<u8>>>,
    pub committed: Mutex<Vec<String>>,
    pub aborted: Mutex<Vec<(String, AbortReason)>>,
    /// Delay per write (a slow disk).
    pub delay: Mutex<Option<Duration>>,
}

struct MemWriter {
    sink: Arc<MemSink>,
    id: String,
}

#[async_trait]
impl SinkWriter for MemWriter {
    async fn write(&mut self, chunk: Bytes) -> Result<(), String> {
        let delay = *self.sink.delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        self.sink.files.lock().unwrap().entry(self.id.clone()).or_default().extend_from_slice(&chunk);
        Ok(())
    }
    async fn close(self: Box<Self>) -> Result<(), String> {
        self.sink.committed.lock().unwrap().push(self.id.clone());
        Ok(())
    }
    async fn abort(self: Box<Self>, reason: AbortReason) {
        self.sink.aborted.lock().unwrap().push((self.id.clone(), reason));
    }
}

pub(crate) struct SharedSink(pub Arc<MemSink>);

#[async_trait]
impl FileSink for SharedSink {
    async fn open(&self, file: &FileMeta, offset: u64, _ctx: &SinkContext) -> Result<Box<dyn SinkWriter>, String> {
        let mut files = self.0.files.lock().unwrap();
        let data = files.entry(file.id.clone()).or_default();
        data.truncate(offset as usize);
        Ok(Box::new(MemWriter { sink: self.0.clone(), id: file.id.clone() }))
    }
    fn can_resume(&self) -> bool {
        true
    }
    async fn hash_prefix(&self, file: &FileMeta, offset: u64, _ctx: &SinkContext) -> Result<Sha256, String> {
        let files = self.0.files.lock().unwrap();
        let data = files.get(&file.id).ok_or("nothing stored")?;
        if (data.len() as u64) < offset {
            return Err("short".into());
        }
        let mut h = Sha256::new();
        h.update(&data[..offset as usize]);
        Ok(h)
    }
}

// ── Harness ───────────────────────────────────────────────────────────────

const FP_A: &str = "sha-256 AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA";
const FP_B: &str = "sha-256 BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB:BB";

pub(crate) struct Pair {
    pub a: PeerSession,
    pub b: PeerSession,
    pub _ea: mpsc::UnboundedReceiver<SessionEvent>,
    pub eb: mpsc::UnboundedReceiver<SessionEvent>,
    pub sink_b: Arc<MemSink>,
    pub cha: Arc<MemChannel>,
    pub chb: Arc<MemChannel>,
    pub id_a: Arc<RtcIdentity>,
    pub id_b: Arc<RtcIdentity>,
}

fn opts(role: Role, local: &str, remote: &str, id: Arc<RtcIdentity>, alias: &str) -> SessionOptions {
    let mut o = SessionOptions::new(
        role,
        "s1",
        local.into(),
        remote.into(),
        id,
        DeviceInfo { alias: alias.into(), device_type: "desktop".into(), platform: "test".into() },
    );
    o.progress_interval = Duration::ZERO;
    o
}

pub(crate) fn pair(tweak: impl FnOnce(&mut SessionOptions, &mut SessionOptions)) -> Pair {
    let (cha, chb) = mem_pair();
    let id_a = Arc::new(RtcIdentity::generate());
    let id_b = Arc::new(RtcIdentity::generate());
    let mut oa = opts(Role::Offerer, FP_A, FP_B, id_a.clone(), "Alice");
    let mut ob = opts(Role::Answerer, FP_B, FP_A, id_b.clone(), "Bob");
    let sink_b = Arc::new(MemSink::default());
    ob.sink = Some(Arc::new(SharedSink(sink_b.clone())));
    oa.sink = Some(Arc::new(SharedSink(Arc::new(MemSink::default()))));
    tweak(&mut oa, &mut ob);
    let (a, ea) = PeerSession::start(oa, cha.clone());
    let (b, eb) = PeerSession::start(ob, chb.clone());
    Pair { a, b, _ea: ea, eb, sink_b, cha, chb, id_a, id_b }
}

async fn next_offer(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> IncomingOffer {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("offer in time").expect("events") {
            SessionEvent::Offer(o) => return o,
            _ => continue,
        }
    }
}

async fn wait_event(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, f: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("event in time").expect("events");
        if f(&e) {
            return e;
        }
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i % 251) as u8).wrapping_add(seed)).collect()
}

fn req(id: &str, files: Vec<OutgoingFile>, text: Option<&str>) -> TransferRequest {
    TransferRequest { transfer_id: id.into(), files, text: text.map(str::to_string) }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn handshake_authenticates_both_peers() {
    let p = pair(|_, _| {});
    let (pa, pb) = tokio::join!(p.a.ready(), p.b.ready());
    let (pa, pb) = (pa.unwrap(), pb.unwrap());
    assert_eq!(pa.key, p.id_b.public_key());
    assert_eq!(pb.key, p.id_a.public_key());
    assert_eq!(pa.device.alias, "Bob");
    assert_eq!(pa.transcript, pb.transcript);
    assert_eq!(pa.short_code, pb.short_code);
    assert!(!pa.room_verified);
}

#[tokio::test]
async fn mitm_fingerprints_fail_authentication() {
    let p = pair(|a, _| a.remote_fingerprint = "sha-256 CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC:CC".into());
    let (ra, rb) = tokio::join!(p.a.ready(), p.b.ready());
    assert_eq!(ra.unwrap_err().code, "auth");
    assert_eq!(rb.unwrap_err().code, "auth");
}

#[tokio::test]
async fn room_secret_must_match() {
    let p = pair(|a, b| {
        a.room_secret = Some(vec![7; 16]);
        b.room_secret = Some(vec![7; 16]);
    });
    let (ra, rb) = tokio::join!(p.a.ready(), p.b.ready());
    assert!(ra.unwrap().room_verified && rb.unwrap().room_verified);

    let p = pair(|a, b| {
        a.room_secret = Some(vec![7; 16]);
        b.room_secret = Some(vec![8; 16]);
    });
    let (ra, rb) = tokio::join!(p.a.ready(), p.b.ready());
    assert_eq!(ra.unwrap_err().code, "auth");
    assert_eq!(rb.unwrap_err().code, "auth");

    let p = pair(|a, _| a.room_secret = Some(vec![7; 16]));
    assert_eq!(p.a.ready().await.unwrap_err().code, "auth");
}

#[tokio::test]
async fn pinned_key_mismatch_fails() {
    let p = pair(|a, _| a.expected_peer_key = Some(RtcIdentity::generate().public_key().into()));
    let err = p.a.ready().await.unwrap_err();
    assert_eq!(err.code, "auth");
    assert!(err.message.contains("unexpected peer key"));
    assert!(p.b.ready().await.is_err());
}

#[tokio::test]
async fn tampered_signature_is_rejected() {
    let p = pair(|_, _| {});
    *p.chb.transform.lock().unwrap() = Some(Box::new(|t: String| {
        if !t.contains("\"t\":\"auth\"") {
            return t;
        }
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        let mut sig = super::b64::decode(v["sig"].as_str().unwrap()).unwrap();
        sig[10] ^= 1;
        format!("{{\"t\":\"auth\",\"sig\":\"{}\"}}", super::b64::encode(&sig))
    }));
    assert_eq!(p.a.ready().await.unwrap_err().code, "auth");
    assert!(p.b.ready().await.is_err());
}

#[tokio::test]
async fn transfers_files_and_text_with_verification() {
    let mut p = pair(|_, _| {});
    let small = pattern(70_000, 1);
    let big = pattern(5 * 1024 * 1024 + 17, 2);
    let a = p.a.clone();
    let (s, b2) = (small.clone(), big.clone());
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("s", &s), file("b", &b2)], Some("hi"))).await });
    let offer = next_offer(&mut p.eb).await;
    assert_eq!(offer.text.as_deref(), Some("hi"));
    assert_eq!(offer.files.len(), 2);
    offer.accept(None, &[]).await.unwrap();
    let done = wait_event(&mut p.eb, |e| matches!(e, SessionEvent::Done { .. })).await;
    let SessionEvent::Done { completed, failed, .. } = done else { unreachable!() };
    assert_eq!((completed, failed), (vec!["s".to_string(), "b".to_string()], vec![]));
    let outcome = send.await.unwrap().unwrap();
    assert_eq!(outcome.completed, vec!["s", "b"]);
    let files = p.sink_b.files.lock().unwrap();
    assert_eq!(files["s"], small);
    assert_eq!(files["b"], big);
}

#[tokio::test]
async fn decline_and_partial_accept() {
    let mut p = pair(|_, _| {});
    let a = p.a.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("x", b"abc")], None)).await });
    next_offer(&mut p.eb).await.decline().await.unwrap();
    let out = send.await.unwrap().unwrap();
    assert!(out.declined);
    assert_eq!(out.skipped, vec!["x"]);

    let a = p.a.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t2", vec![file("x", b"abc"), file("y", b"defg")], None)).await });
    next_offer(&mut p.eb).await.accept(Some(vec!["y".into()]), &[]).await.unwrap();
    let out = send.await.unwrap().unwrap();
    assert_eq!((out.completed, out.skipped), (vec!["y".to_string()], vec!["x".to_string()]));
    assert!(!p.sink_b.files.lock().unwrap().contains_key("x"));
    // A transfer id can't be reused within a session.
    assert_eq!(p.a.send_transfer(req("t2", vec![], Some("again"))).await.unwrap_err().code, "invalid");
}

#[tokio::test]
async fn sender_cancel_and_receiver_cancel() {
    let mut p = pair(|_, _| {});
    *p.sink_b.delay.lock().unwrap() = Some(Duration::from_millis(2));
    let data = pattern(8 * 1024 * 1024, 3);
    let a = p.a.clone();
    let d = data.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("f", &d)], None)).await });
    next_offer(&mut p.eb).await.accept(None, &[]).await.unwrap();
    wait_event(&mut p.eb, |e| matches!(e, SessionEvent::Progress { bytes, .. } if *bytes > 100_000)).await;
    p.a.cancel(Some("user"), Some("t1")).await;
    assert_eq!(send.await.unwrap().unwrap_err().code, "cancelled");
    let ev = wait_event(&mut p.eb, |e| matches!(e, SessionEvent::Cancelled { .. })).await;
    assert!(matches!(ev, SessionEvent::Cancelled { by_remote: true, interrupted: false, .. }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(p.sink_b.aborted.lock().unwrap().iter().any(|(id, r)| id == "f" && *r == AbortReason::Cancelled));

    // The receiver cancels the next one; the session stays usable.
    let a = p.a.clone();
    let d = data.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t2", vec![file("g", &d)], None)).await });
    next_offer(&mut p.eb).await.accept(None, &[]).await.unwrap();
    wait_event(&mut p.eb, |e| matches!(e, SessionEvent::Progress { bytes, .. } if *bytes > 100_000)).await;
    p.b.cancel(None, Some("t2")).await;
    assert_eq!(send.await.unwrap().unwrap_err().code, "cancelled");
    *p.sink_b.delay.lock().unwrap() = None;
    let a = p.a.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t3", vec![file("h", b"ok")], None)).await });
    next_offer(&mut p.eb).await.accept(None, &[]).await.unwrap();
    assert_eq!(send.await.unwrap().unwrap().completed, vec!["h"]);
}

#[tokio::test]
async fn flow_control_bounds_memory_with_a_slow_receiver() {
    let mut p = pair(|_, _| {});
    *p.sink_b.delay.lock().unwrap() = Some(Duration::from_micros(300));
    let data = pattern(40 * 1024 * 1024, 4);
    let a = p.a.clone();
    let d = data.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("f", &d)], None)).await });
    next_offer(&mut p.eb).await.accept(None, &[]).await.unwrap();
    let out = tokio::time::timeout(Duration::from_secs(120), send).await.unwrap().unwrap().unwrap();
    assert_eq!(out.completed, vec!["f"]);
    // In flight never exceeds the window plus one frame (+ control slack).
    let max = p.cha.max_buffered.load(Ordering::SeqCst);
    assert!(max <= RECV_WINDOW as usize + 2 * LARGE_CHUNK + 64 * 1024, "max in flight {max}");
    assert_eq!(p.sink_b.files.lock().unwrap()["f"], data);
}

#[tokio::test]
async fn resumes_from_offsets_and_verifies_the_whole_file() {
    let mut p = pair(|_, _| {});
    let data = pattern(3 * 1024 * 1024 + 5, 5);
    // The receiver already holds the first 1 MiB + 3 bytes.
    p.sink_b.files.lock().unwrap().insert("f".into(), data[..1_048_579].to_vec());
    let a = p.a.clone();
    let d = data.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("f", &d)], None)).await });
    next_offer(&mut p.eb).await.accept(None, &[("f".into(), 1_048_579)]).await.unwrap();
    let out = send.await.unwrap().unwrap();
    assert_eq!(out.completed, vec!["f"]);
    assert_eq!(p.sink_b.files.lock().unwrap()["f"], data);
}

#[tokio::test]
async fn large_offers_are_split_and_reassembled() {
    let mut p = pair(|_, _| {});
    let files: Vec<OutgoingFile> = (0..3000)
        .map(|i| {
            let mut f = file(&format!("file-{i}"), b"");
            f.meta.name = format!("Holiday {}/IMG_{i:05} {}.jpg", i % 7, "\u{e4}".repeat(40));
            f
        })
        .collect();
    let a = p.a.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", files, Some("many"))).await });
    let offer = next_offer(&mut p.eb).await;
    assert_eq!(offer.files.len(), 3000);
    let ids: Vec<String> = offer.files.iter().map(|f| f.id.clone()).collect();
    offer.accept(Some(ids), &[]).await.unwrap();
    let out = send.await.unwrap().unwrap();
    assert_eq!(out.completed.len(), 3000);
}

#[tokio::test]
async fn undecided_offers_time_out() {
    let mut p = pair(|_, b| b.decision_timeout = Some(Duration::from_millis(200)));
    let a = p.a.clone();
    let send = tokio::spawn(async move { a.send_transfer(req("t1", vec![file("x", b"abc")], None)).await });
    let _offer = next_offer(&mut p.eb).await;
    let err = send.await.unwrap().unwrap_err();
    assert_eq!(err.code, "cancelled");
    assert_eq!(err.message, "timeout");
}

#[tokio::test]
async fn silence_closes_the_session() {
    let p = pair(|a, _| {
        a.timeout = Duration::from_millis(300);
        a.ping_interval = Duration::from_secs(60);
    });
    p.a.ready().await.unwrap();
    p.chb.blackhole.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), p.a.closed()).await.unwrap();
}

#[tokio::test]
async fn protocol_violations_close_both_ends() {
    let p = pair(|_, _| {});
    p.a.ready().await.unwrap();
    p.chb.send(Frame::Binary(Bytes::from_static(b"stray"))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), p.a.closed()).await.unwrap();
}
