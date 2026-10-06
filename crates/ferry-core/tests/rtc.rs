//! Native ↔ native WebRTC transfers through a real (in-process) ferry-signal:
//! files (a large one and a nested folder), text, decline, partial accept,
//! cancel by either side, receiving switched off, trusted auto-accept, an
//! impostor identity, and private link rooms with the right and wrong secret.
//! Every received byte goes through the normal safety path; no `.ferrypart`
//! may remain.

mod common;

use common::*;
use ferry_core::events::EngineEvent;
use ferry_core::model::*;
use ferry_core::rtc::identity::RtcIdentity;
use ferry_core::rtc::peer::{ConnectOptions, Connector, ConnectorConfig};
use ferry_core::rtc::protocol::DeviceInfo;
use ferry_core::rtc::signaling::{ClientInfoOut, SignalingClient, SignalingConfig, SignalingEvent};
use ferry_core::settings::AutoAccept;
use ferry_core::{SendItem, Settings, Target};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const LONG: Duration = Duration::from_secs(240);

async fn signal_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config =
        ferry_signal::Config { limits: ferry_signal::Limits { max_conns_per_group: 64, ..Default::default() }, ..Default::default() };
    tokio::spawn(ferry_signal::serve(listener, config));
    format!("ws://{addr}/v1/ws")
}

async fn rtc_peer(alias: &str, url: &str, tweak: impl FnOnce(&mut Settings)) -> Peer {
    let url = url.to_string();
    let peer = peer_with(alias, move |s| {
        s.signaling_url = Some(url);
        s.stun_servers = vec![];
        tweak(s);
    })
    .await;
    peer.engine.set_webrtc_loopback(true);
    peer
}

/// The WebRTC device id of `alias`, once it is online.
async fn find(peer: &Peer, alias: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(d) = peer.engine.devices().into_iter().find(|d| d.alias == alias && d.online && d.id.starts_with("rtc:")) {
                return d.id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{alias} never appeared"))
}

async fn absent(peer: &Peer, alias: &str, wait: Duration) -> bool {
    tokio::time::sleep(wait).await;
    !peer.engine.devices().iter().any(|d| d.alias == alias && d.online)
}

fn sha(path: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
}

fn no_parts(dir: &Path) {
    let parts: Vec<String> = files_in(dir).into_iter().filter(|f| f.ends_with(".ferrypart")).collect();
    assert!(parts.is_empty(), "leftover part files: {parts:?}");
}

async fn send(from: &Peer, to: &str, items: Vec<SendItem>) -> Vec<String> {
    from.engine.send(vec![Target::Device { id: to.to_string() }], items).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_folders_and_text_arrive_intact() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let bob = find(&a, "Bob").await;
    let alice = find(&b, "Alice").await;
    let device = a.engine.devices().into_iter().find(|d| d.id == bob).unwrap();
    assert!(device.verified && device.is_ferry && device.online);
    assert_eq!(device.address.as_deref(), Some("WebRTC"));

    let src = tempfile::tempdir().unwrap();
    let big = write_file(src.path(), "big.bin", &pattern(52 * 1024 * 1024 + 7, 1));
    let folder = src.path().join("Album");
    write_file(&folder, "2026/one.jpg", &pattern(300_000, 2));
    write_file(&folder, "2026/deep/two.txt", b"two");
    write_file(&folder, "empty.dat", b"");
    let mut requests = b.engine.subscribe();
    let responder = b.auto_respond(Decision::accept_all());

    let started = std::time::Instant::now();
    let ids = send(
        &a,
        &bob,
        vec![
            SendItem::Path { path: big.clone() },
            SendItem::Path { path: folder.clone() },
            SendItem::Text { text: "hello over WebRTC".into() },
        ],
    )
    .await;
    assert_eq!(ids.len(), 2, "one transfer for the text, one for the files");
    let mut outcomes = Vec::new();
    for id in &ids {
        outcomes.push(a.wait_final(id, LONG).await);
    }
    eprintln!("52 MiB + folder in {:?}", started.elapsed());
    for t in &outcomes {
        assert_eq!(t.state, TransferState::Completed, "{t:?}");
        let c = t.connection.as_ref().unwrap();
        assert_eq!((c.transport.as_str(), c.encrypted), ("webrtc", true));
    }
    let received = b.wait_received(LONG).await;
    assert_eq!(received.state, TransferState::Completed);
    assert_eq!(received.peer.id, alice);
    assert!(received.peer.verified);
    assert_eq!(received.connection.as_ref().unwrap().transport, "webrtc");

    // The text arrived as a message, the request listed the folder structure.
    let mut saw_text = false;
    let mut saw_files = false;
    while let Ok(e) = requests.try_recv() {
        if let EngineEvent::IncomingRequest { request } = e {
            match &request.text {
                Some(t) => saw_text |= t == "hello over WebRTC",
                None => {
                    saw_files = true;
                    assert!(request.files.iter().any(|f| f.name == "Album/2026/deep/two.txt"));
                    assert!(!request.trusted);
                }
            }
        }
    }
    assert!(saw_text && saw_files);

    assert_eq!(sha(&b.saved("big.bin")), sha(&big));
    assert_eq!(std::fs::read(b.saved("Album/2026/one.jpg")).unwrap(), pattern(300_000, 2));
    assert_eq!(std::fs::read(b.saved("Album/2026/deep/two.txt")).unwrap(), b"two");
    assert_eq!(std::fs::read(b.saved("Album/empty.dat")).unwrap(), b"");
    no_parts(b.save_dir.path());
    #[cfg(windows)]
    assert!(std::fs::read_to_string(format!("{}:Zone.Identifier", b.saved("big.bin").display())).unwrap().contains("ZoneId=3"));
    let history = b.engine.history(50, None, Some(Direction::Receive)).unwrap();
    assert!(history.iter().any(|h| h.name == "big.bin" && h.verified && h.peer_id == alice));
    assert!(history.iter().any(|h| h.kind == HistoryKind::Text));

    // Sending again reuses the open session; a second same-named file never replaces the first.
    let id = send(&a, &bob, vec![SendItem::Path { path: folder.join("2026/deep/two.txt") }]).await.remove(0);
    assert_eq!(a.wait_final(&id, LONG).await.state, TransferState::Completed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(b.saved("two.txt")).unwrap(), b"two");
    responder.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_and_partial_accept() {
    let url = signal_server().await;
    let a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let bob = find(&a, "Bob").await;
    let src = tempfile::tempdir().unwrap();
    let x = write_file(src.path(), "x.bin", &pattern(10_000, 3));
    let y = write_file(src.path(), "y.bin", &pattern(20_000, 4));

    let responder = b.auto_respond(Decision::decline());
    let id = send(&a, &bob, vec![SendItem::Path { path: x.clone() }]).await.remove(0);
    let mut a = a;
    let t = a.wait_final(&id, LONG).await;
    assert_eq!(t.state, TransferState::Declined);
    responder.abort();

    // Accept only y.bin (find its id in the request).
    let engine = b.engine.clone();
    let mut events = b.engine.subscribe();
    let picker = tokio::spawn(async move {
        loop {
            if let Ok(EngineEvent::IncomingRequest { request }) = events.recv().await
                && request.text.is_none()
            {
                let y = request.files.iter().find(|f| f.name == "y.bin").unwrap().id.clone();
                engine.respond(&request.id, Decision { accept: Some(vec![y]), ..Default::default() });
                return;
            }
        }
    });
    let id = send(&a, &bob, vec![SendItem::Path { path: x }, SendItem::Path { path: y.clone() }]).await.remove(0);
    let t = a.wait_final(&id, LONG).await;
    picker.await.unwrap();
    assert_eq!(t.state, TransferState::Completed);
    let files = a.engine.transfer_files(&id).unwrap();
    assert_eq!(files.iter().find(|f| f.name == "x.bin").unwrap().state, FileState::Skipped);
    assert_eq!(files.iter().find(|f| f.name == "y.bin").unwrap().state, FileState::Done);
    let r = b.wait_received(LONG).await;
    assert_eq!(r.file_count, 1);
    assert_eq!(files_in(b.save_dir.path()), vec!["y.bin"]);
    assert_eq!(sha(&b.saved("y.bin")), sha(&y));
}

async fn cancel_case(by_sender: bool) {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let bob = find(&a, "Bob").await;
    let src = tempfile::tempdir().unwrap();
    let big = write_file(src.path(), "huge.bin", &pattern(120 * 1024 * 1024, 5));
    let responder = b.auto_respond(Decision::accept_all());
    let id = send(&a, &bob, vec![SendItem::Path { path: big }]).await.remove(0);
    // Wait until bytes flow on the receiving side.
    let incoming = b.wait_transfer(LONG, |t| t.direction == Direction::Receive && t.bytes_done > 2 * 1024 * 1024).await;
    if by_sender {
        assert!(a.engine.cancel(&id));
    } else {
        assert!(b.engine.cancel(&incoming.id));
    }
    let sent = a.wait_final(&id, LONG).await;
    let received = b.wait_final(&incoming.id, LONG).await;
    assert_eq!(sent.state, TransferState::Cancelled, "{sent:?}");
    assert_eq!(received.state, TransferState::Cancelled, "{received:?}");
    let (local, remote) = if by_sender { (&sent, &received) } else { (&received, &sent) };
    assert_eq!(local.error.as_ref().unwrap().code, "cancelled");
    assert_eq!(remote.error.as_ref().unwrap().code, "cancelled_by_peer");
    tokio::time::sleep(Duration::from_millis(800)).await;
    no_parts(b.save_dir.path());
    assert!(files_in(b.save_dir.path()).is_empty());
    responder.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_cancels_mid_transfer() {
    cancel_case(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_cancels_mid_transfer() {
    cancel_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiving_switched_off() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let b = rtc_peer("Bob", &url, |s| s.receive_enabled = false).await;
    let bob = find(&a, "Bob").await;
    let src = tempfile::tempdir().unwrap();
    let f = write_file(src.path(), "f.txt", b"nope");
    let id = send(&a, &bob, vec![SendItem::Path { path: f }]).await.remove(0);
    let t = a.wait_final(&id, LONG).await;
    assert_eq!(t.state, TransferState::Failed);
    assert_eq!(t.error.unwrap().code, "rejected");
    assert!(files_in(b.save_dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_verified_keys_skip_the_prompt() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |s| s.auto_accept = AutoAccept::Trusted).await;
    let bob = find(&a, "Bob").await;
    let alice = find(&b, "Alice").await;
    let device = b.engine.set_device_flags(&alice, Some(true), None, None, None).unwrap().unwrap();
    assert!(device.trusted);
    let src = tempfile::tempdir().unwrap();
    let f = write_file(src.path(), "trusted.bin", &pattern(2_000_000, 6));
    let id = send(&a, &bob, vec![SendItem::Path { path: f.clone() }]).await.remove(0);
    assert_eq!(a.wait_final(&id, LONG).await.state, TransferState::Completed);
    b.wait_received(LONG).await;
    assert_eq!(sha(&b.saved("trusted.bin")), sha(&f));
    // Nobody was asked.
    while let Ok(e) = b.events.try_recv() {
        assert!(!matches!(e, EngineEvent::IncomingRequest { .. }), "trusted sender was prompted");
    }
    // The trust is bound to the key: the device stays trusted after it reconnects.
    assert!(b.engine.devices().iter().any(|d| d.id == alice && d.trusted));
}

/// A raw client that announces `claimed_key` but holds another identity.
async fn impostor(url: &str, alias: &str, claimed_key: &str, room: Option<(&str, Option<Vec<u8>>)>) -> tokio::task::JoinHandle<()> {
    let identity = Arc::new(RtcIdentity::generate());
    let info = ClientInfoOut {
        alias: alias.into(),
        device_model: None,
        device_type: Some("desktop".into()),
        token: "x".into(),
        public_key: claimed_key.into(),
        nearby: room.as_ref().map(|_| false),
    };
    let (client, mut events) = SignalingClient::start(SignalingConfig::new(url, info));
    let room_secret = room.as_ref().and_then(|(_, s)| s.clone());
    if let Some((id, _)) = &room {
        client.join_room(id).unwrap();
    }
    let mut config =
        ConnectorConfig::new(identity, DeviceInfo { alias: alias.into(), device_type: "desktop".into(), platform: "test".into() });
    config.include_loopback = true;
    let connector = Connector::new(client, config);
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let Some(incoming) = connector.handle_signal(&ev) {
                let c = connector.clone();
                let secret = room_secret.clone();
                tokio::spawn(async move {
                    let _ = c.accept(incoming, ConnectOptions { expected_peer_key: None, room_secret: secret, ice_servers: vec![] }).await;
                });
            }
            if matches!(ev, SignalingEvent::State(_)) {
                continue;
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unexpected_identity_key_fails_before_any_byte() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    // "Mallory" claims a key it does not hold.
    let victim = RtcIdentity::generate();
    let _m = impostor(&url, "Mallory", victim.public_key(), None).await;
    let mallory = find(&a, "Mallory").await;
    assert_eq!(mallory, format!("rtc:{}", victim.public_key()));
    let src = tempfile::tempdir().unwrap();
    let f = write_file(src.path(), "secret.txt", b"for the real key holder only");
    let id = send(&a, &mallory, vec![SendItem::Path { path: f }]).await.remove(0);
    let t = a.wait_final(&id, LONG).await;
    assert_eq!(t.state, TransferState::Failed);
    let err = t.error.unwrap();
    assert_eq!(err.code, "auth", "{err:?}");
    assert_eq!(t.bytes_done, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_link_rooms() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let c = rtc_peer("Carol", &url, |_| {}).await;
    for p in [&a, &b, &c] {
        p.engine.set_signaling_nearby(false);
    }
    assert!(absent(&b, "Alice", Duration::from_millis(1500)).await, "not nearby: invisible without a link");

    let room = a.engine.create_room();
    assert!(room.id.starts_with("r:") && room.id.len() == 24);
    let (_, secret) = room.link.split_once("#room=").unwrap();
    assert_eq!(secret.len(), 22);
    // Bob opens the link: they meet.
    let joined = b.engine.join_room(&room.link).unwrap();
    assert_eq!(joined.id, room.id);
    let alice = find(&b, "Alice").await;
    let bob = find(&a, "Bob").await;
    let dev = a.engine.devices().into_iter().find(|d| d.id == bob).unwrap();
    assert_eq!(dev.address.as_deref(), Some("via private link"));
    a.wait_event(Duration::from_secs(10), |e| matches!(e, EngineEvent::RoomUpdated { room: r } if r.peers == 1)).await;

    // Carol has a link with another secret: a different room, nobody there.
    let other = c.engine.join_room(&format!("http://x/#room={}", ferry_core::rtc::b64::encode(&[9u8; 16]))).unwrap();
    assert_ne!(other.id, room.id);
    assert!(absent(&c, "Alice", Duration::from_millis(1000)).await);
    assert!(c.engine.join_room("not a link").is_err());

    // A transfer through the room (the session proves the room secret).
    let responder = a.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let f = write_file(src.path(), "via-link.bin", &pattern(1_500_000, 7));
    let id = send(&b, &alice, vec![SendItem::Path { path: f.clone() }]).await.remove(0);
    assert_eq!(b.wait_final(&id, LONG).await.state, TransferState::Completed);
    a.wait_received(LONG).await;
    assert_eq!(sha(&a.saved("via-link.bin")), sha(&f));
    responder.abort();

    // Someone who knows the room id (the server does) but not the secret fails the MAC.
    let _d = impostor(&url, "Dave", RtcIdentity::generate().public_key(), Some((&room.id, Some(vec![1u8; 16])))).await;
    let dave = find(&a, "Dave").await;
    let g = write_file(src.path(), "not-for-dave.txt", b"secret");
    let id = send(&a, &dave, vec![SendItem::Path { path: g }]).await.remove(0);
    let t = a.wait_final(&id, LONG).await;
    assert_eq!(t.state, TransferState::Failed);
    assert_eq!(t.error.unwrap().code, "auth");

    assert!(b.engine.leave_room(&room.id));
    assert!(absent(&b, "Alice", Duration::from_millis(1500)).await);
    assert!(b.engine.rooms().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_transfers_resume_where_they_stopped() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let bob = find(&a, "Bob").await;
    let src = tempfile::tempdir().unwrap();
    let big = write_file(src.path(), "resume.bin", &pattern(40 * 1024 * 1024, 8));
    let responder = b.auto_respond(Decision::accept_all());
    let id = send(&a, &bob, vec![SendItem::Path { path: big.clone() }]).await.remove(0);
    let incoming = b.wait_transfer(LONG, |t| t.direction == Direction::Receive && t.bytes_done > 6 * 1024 * 1024).await;
    // The receiver's connection drops (signaling and sessions restart).
    b.engine.set_webrtc_loopback(true);
    let reconnecting = a.wait_transfer(LONG, |t| t.id == id && t.state == TransferState::Reconnecting).await;
    assert_eq!(reconnecting.error.unwrap().code, "connection_lost");
    let sent = a.wait_final(&id, LONG).await;
    assert_eq!(sent.state, TransferState::Completed, "{sent:?}");
    let received = b.wait_final(&incoming.id, LONG).await;
    assert_eq!(received.state, TransferState::Completed, "{received:?}");
    assert_eq!(files_in(b.save_dir.path()), vec!["resume.bin"], "one file, continued in place");
    assert_eq!(sha(&b.saved("resume.bin")), sha(&big));
    // No second prompt for the resumed transfer, and history lists it once.
    let history = b.engine.history(50, None, Some(Direction::Receive)).unwrap();
    assert_eq!(history.iter().filter(|h| h.name == "resume.bin").count(), 1);
    responder.abort();
}
