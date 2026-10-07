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
use ferry_core::rtc::protocol::{DeviceInfo, FileMeta, RtcError};
use ferry_core::rtc::session::{FileSource, OutgoingFile, PeerSession, TransferOutcome, TransferRequest};
use ferry_core::rtc::signaling::{ClientInfo, ClientInfoOut, SignalingClient, SignalingConfig, SignalingEvent};
use ferry_core::settings::AutoAccept;
use ferry_core::{SendItem, Settings, Target};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
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

/// The WebRTC device id of `alias`, once it is online. Generous: CI runs these
/// tests in parallel on two cores, and every peer generates its keys first.
async fn find(peer: &Peer, alias: &str) -> String {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(d) = peer.engine.devices().into_iter().find(|d| d.alias == alias && d.online && d.id.starts_with("rtc:")) {
                return d.id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let seen: Vec<String> = peer.engine.devices().iter().map(|d| format!("{} {} online={}", d.alias, d.id, d.online)).collect();
        let status = peer.engine.signaling_status();
        panic!("{alias} never appeared; signaling {} {:?}; devices {seen:?}", status.state, status.error)
    })
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
    let mut events = b.engine.subscribe();
    let requests = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            match events.recv().await {
                Ok(EngineEvent::IncomingRequest { request }) => {
                    seen.push(request);
                    if seen.len() == 2 {
                        return seen;
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("event stream closed: {e}"),
            }
        }
    });
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
    for request in tokio::time::timeout(LONG, requests).await.expect("both requests arrive").unwrap() {
        match &request.text {
            Some(t) => saw_text |= t == "hello over WebRTC",
            None => {
                saw_files = true;
                assert!(request.files.iter().any(|f| f.name == "Album/2026/deep/two.txt"));
                assert!(!request.trusted);
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
    // WebRTC transfers resume after a lost connection, but the user can't pause them.
    let sending = a.engine.transfers().into_iter().find(|t| t.id == id).unwrap();
    for t in [&sending, &incoming] {
        assert!(t.resumable && !t.can_pause && !t.can_resume, "{t:?}");
    }
    assert!(!a.engine.pause(&id) && !b.engine.pause(&incoming.id));
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
    let incoming = tokio::select! {
        t = b.wait_transfer(LONG, |t| t.direction == Direction::Receive && t.bytes_done > 6 * 1024 * 1024) => t,
        t = a.wait_transfer(LONG, |t| t.id == id && t.state.is_final()) => panic!("the send ended before reaching Bob: {t:?}"),
    };
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

/// Text entries in `peer`'s history once they are written (or none after a while).
async fn text_history(peer: &Peer, direction: Direction, expect: bool) -> Vec<HistoryEntry> {
    let read = || -> Vec<HistoryEntry> {
        peer.engine.history(50, None, Some(direction)).unwrap().into_iter().filter(|h| h.kind == HistoryKind::Text).collect()
    };
    for _ in 0..if expect { 100 } else { 10 } {
        if expect && !read().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    read()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_stays_present_while_a_second_connection_of_it_comes_and_goes() {
    let url = signal_server().await;
    let mut a = rtc_peer("Alice", &url, |_| {}).await;
    let b = rtc_peer("Bob", &url, |_| {}).await;
    let bob = find(&a, "Bob").await;
    // A second connection under Bob's key, as when a device reconnects before
    // the server has dropped its old connection...
    let key = b.engine.signaling_status().identity_key;
    let info = ClientInfoOut {
        alias: "Bob".into(),
        device_model: None,
        device_type: Some("desktop".into()),
        token: "second".into(),
        public_key: key,
        nearby: None,
    };
    let (extra, mut events) = SignalingClient::start(SignalingConfig::new(&url, info));
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), events.recv()).await.expect("connects").expect("events");
        if matches!(ev, SignalingEvent::Hello { .. }) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    // ...leaves again: Bob is still present and reachable on his first one.
    extra.close();
    assert!(!absent(&a, "Bob", Duration::from_millis(1500)).await, "Bob must stay present");
    let id = send(&a, &bob, vec![SendItem::Text { text: "still here".into() }]).await.remove(0);
    let sent = a.wait_final(&id, LONG).await;
    assert_eq!(sent.state, TransferState::Completed, "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_history_follows_the_privacy_settings() {
    let url = signal_server().await;
    let text = "the door code is 4711\nsecond line";
    for (history, keep) in [(false, false), (false, true), (true, false), (true, true)] {
        let tweak = move |s: &mut Settings| {
            s.history_enabled = history;
            s.keep_message_text = keep;
        };
        let (sender, receiver) = (format!("Alice {history} {keep}"), format!("Bob {history} {keep}"));
        let mut a = rtc_peer(&sender, &url, tweak).await;
        let mut b = rtc_peer(&receiver, &url, tweak).await;
        let bob = find(&a, &receiver).await;
        let id = send(&a, &bob, vec![SendItem::Text { text: text.into() }]).await.remove(0);
        // The sender's outcome first: when it fails, its error says why.
        let sent = a.wait_final(&id, LONG).await;
        assert_eq!(sent.state, TransferState::Completed, "{sent:?}");
        b.wait_event(LONG, |e| matches!(e, EngineEvent::IncomingRequest { request } if request.text.is_some())).await;
        for (who, direction) in [(&b, Direction::Receive), (&a, Direction::Send)] {
            let entries = text_history(who, direction, history).await;
            if !history {
                assert!(entries.is_empty(), "history off records nothing: {entries:?}");
                continue;
            }
            assert_eq!(entries.len(), 1, "{direction:?}: {entries:?}");
            let e = &entries[0];
            if keep {
                assert_eq!((e.name.as_str(), e.text.as_deref()), ("the door code is 4711", Some(text)), "{direction:?}");
            } else {
                assert_eq!((e.name.as_str(), e.text.as_deref()), ("Message", None), "{direction:?}: nothing of the text is kept");
            }
        }
    }
}

/// A bare `ferry-dc/1` sender (not an engine), for offers the engine never
/// makes itself, such as files and a message in one transfer.
struct RawSender {
    connector: Arc<Connector>,
    /// The receiver's client id and identity key.
    target: (String, String),
    /// This sender's device id at the receiver.
    device_id: String,
}

impl RawSender {
    async fn start(url: &str, alias: &str, target_alias: &str) -> RawSender {
        let identity = Arc::new(RtcIdentity::generate());
        let info = ClientInfoOut {
            alias: alias.into(),
            device_model: None,
            device_type: Some("desktop".into()),
            token: "x".into(),
            public_key: identity.public_key().into(),
            nearby: None,
        };
        let (client, mut events) = SignalingClient::start(SignalingConfig::new(url, info));
        let device_id = format!("rtc:{}", identity.public_key());
        let mut config =
            ConnectorConfig::new(identity, DeviceInfo { alias: alias.into(), device_type: "desktop".into(), platform: "test".into() });
        config.include_loopback = true;
        let connector = Connector::new(client, config);
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel::<ClientInfo>();
        let c = connector.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                c.handle_signal(&ev);
                match ev {
                    SignalingEvent::Hello { peers, .. } => peers.into_iter().for_each(|p| drop(tx.send(p))),
                    SignalingEvent::Join { peer } | SignalingEvent::Update { peer } => drop(tx.send(peer)),
                    _ => {}
                }
            }
        });
        let target = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let p = seen.recv().await.unwrap();
                if p.alias == target_alias
                    && let Some(key) = p.key()
                {
                    return (p.id.clone(), key.to_string());
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{target_alias} never appeared"));
        RawSender { connector, target, device_id }
    }

    /// A new authenticated session to the receiver.
    async fn session(&self) -> PeerSession {
        let options = ConnectOptions { expected_peer_key: Some(self.target.1.clone()), room_secret: None, ice_servers: vec![] };
        let mut connected = self.connector.connect(&self.target.0, options).await.unwrap();
        tokio::spawn(async move { while connected.events.recv().await.is_some() {} });
        connected.session
    }
}

struct Memory(Arc<Vec<u8>>);

#[async_trait::async_trait]
impl FileSource for Memory {
    async fn read(&self, start: u64, end: u64) -> std::io::Result<bytes::Bytes> {
        Ok(bytes::Bytes::copy_from_slice(&self.0[start as usize..end as usize]))
    }
}

fn memory_file(id: &str, data: &Arc<Vec<u8>>) -> OutgoingFile {
    OutgoingFile {
        meta: FileMeta {
            id: id.into(),
            name: format!("{id}.bin"),
            size: data.len() as u64,
            mime: "application/octet-stream".into(),
            modified: None,
        },
        source: Arc::new(Memory(data.clone())),
    }
}

type Sending = tokio::task::JoinHandle<Result<TransferOutcome, RtcError>>;

/// Offers `files` with `text` in one transfer.
fn offer(session: &PeerSession, id: &str, files: Vec<OutgoingFile>, text: &str) -> Sending {
    offer_with(session, id, files, Some(text))
}

fn offer_with(session: &PeerSession, id: &str, files: Vec<OutgoingFile>, text: Option<&str>) -> Sending {
    let (session, request) = (session.clone(), TransferRequest { transfer_id: id.into(), files, text: text.map(str::to_string) });
    tokio::spawn(async move { session.send_transfer(request).await })
}

/// A file whose bytes never come: its transfer stays live once accepted.
struct Stalled;

#[async_trait::async_trait]
impl FileSource for Stalled {
    async fn read(&self, _start: u64, _end: u64) -> std::io::Result<bytes::Bytes> {
        std::future::pending().await
    }
}

fn stalled_file() -> OutgoingFile {
    OutgoingFile {
        meta: FileMeta { id: "f".into(), name: "slow.bin".into(), size: 1 << 20, mime: "application/octet-stream".into(), modified: None },
        source: Arc::new(Stalled),
    }
}

/// Every message `peer` shows, in order.
fn messages(peer: &Peer) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (out, mut events) = (seen.clone(), peer.engine.subscribe());
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(EngineEvent::IncomingRequest { request }) if request.files.is_empty() => {
                    out.lock().unwrap().push(request.text.unwrap_or_default());
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    });
    seen
}

/// The next accept prompt (an offer with files).
async fn prompt(peer: &mut Peer) -> IncomingRequest {
    match peer.wait_event(LONG, |e| matches!(e, EngineEvent::IncomingRequest { request } if !request.files.is_empty())).await {
        EngineEvent::IncomingRequest { request } => request,
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_with_files_arrives_once_when_they_are_accepted() {
    let url = signal_server().await;
    let mut b = rtc_peer("Bob", &url, |_| {}).await;
    let shown = messages(&b);
    let raw = RawSender::start(&url, "Raw", "Bob").await;
    let small = Arc::new(pattern(5_000, 9));
    let big = Arc::new(pattern(40 * 1024 * 1024, 10));

    // Declined: the text never shows and nothing is recorded.
    let session = raw.session().await;
    let sending = offer(&session, "t-declined", vec![memory_file("small", &small)], "not for you");
    let request = prompt(&mut b).await;
    assert_eq!(request.text, None, "the prompt doesn't carry the text");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(shown.lock().unwrap().is_empty(), "nothing shown before the decision");
    b.engine.respond(&request.id, Decision::decline());
    assert!(sending.await.unwrap().unwrap().declined);

    // Accepted in part: the text shows once the decision is made.
    let files = vec![memory_file("big", &big), memory_file("small", &small)];
    let sending = offer(&session, "t-kept", files.clone(), "here you go");
    let request = prompt(&mut b).await;
    assert_eq!(request.text, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(shown.lock().unwrap().is_empty(), "neither the declined text nor the undecided one is shown");
    b.engine.respond(&request.id, Decision { accept: Some(vec!["big".into()]), ..Default::default() });
    let incoming = b.wait_transfer(LONG, |t| t.direction == Direction::Receive && t.bytes_done > 4 * 1024 * 1024).await;
    assert_eq!(*shown.lock().unwrap(), vec!["here you go".to_string()]);

    // The connection drops and the sender offers the transfer again: it
    // resumes without a prompt and without showing the text again.
    session.close(None);
    let _ = tokio::time::timeout(LONG, sending).await.expect("the interrupted send ends");
    b.wait_transfer(LONG, |t| t.id == incoming.id && t.state == TransferState::Reconnecting).await;
    let session = raw.session().await;
    let outcome = offer(&session, "t-kept", files.clone(), "here you go").await.unwrap().unwrap();
    assert_eq!(outcome.completed, vec!["big".to_string()], "{outcome:?}");
    assert_eq!(b.wait_final(&incoming.id, LONG).await.state, TransferState::Completed);
    assert_eq!(sha(&b.saved("big.bin")), hex::encode(Sha256::digest(big.as_slice())));

    // Offered once more after it finished: asked again, but its text was shown already.
    let session = raw.session().await;
    let sending = offer(&session, "t-kept", vec![memory_file("small", &small)], "here you go");
    let request = prompt(&mut b).await;
    b.engine.respond(&request.id, Decision::accept_all());
    assert!(!sending.await.unwrap().unwrap().declined);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*shown.lock().unwrap(), vec!["here you go".to_string()]);
    let history = b.engine.history(50, None, Some(Direction::Receive)).unwrap();
    let texts: Vec<&HistoryEntry> = history.iter().filter(|h| h.kind == HistoryKind::Text).collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].name == "Message" && texts[0].text.is_none(), "history follows the privacy settings: {texts:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_beyond_the_rate_limit_are_declined() {
    let url = signal_server().await;
    let b = rtc_peer("Bob", &url, |_| {}).await;
    let shown = messages(&b);
    let raw = RawSender::start(&url, "Raw", "Bob").await;
    let session = raw.session().await;
    for i in 0..10 {
        let outcome = offer_with(&session, &format!("m{i}"), vec![], Some(&format!("message {i}"))).await.unwrap().unwrap();
        assert!(!outcome.declined, "message {i} is within the limit");
    }
    let outcome = offer_with(&session, "m10", vec![], Some("one too many")).await.unwrap().unwrap();
    assert!(outcome.declined, "the 11th message within a minute is declined");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let shown = shown.lock().unwrap().clone();
    assert_eq!(shown.len(), 10, "{shown:?}");
    assert!(!shown.iter().any(|m| m == "one too many"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_receives_per_peer_are_capped() {
    let url = signal_server().await;
    let mut b = rtc_peer("Bob", &url, |s| s.auto_accept = AutoAccept::Trusted).await;
    let raw = RawSender::start(&url, "Raw", "Bob").await;
    assert_eq!(find(&b, "Raw").await, raw.device_id);
    b.engine.set_device_flags(&raw.device_id, Some(true), None, None, None).unwrap().unwrap();
    let engine = b.engine.clone();
    let live = move || engine.transfers().into_iter().filter(|t| t.direction == Direction::Receive && !t.state.is_final()).count();

    // Eight transfers that never finish are accepted; each needs its own session.
    let mut sessions = Vec::new();
    let mut sending = Vec::new();
    for i in 0..8 {
        let session = raw.session().await;
        sending.push(offer_with(&session, &format!("t{i}"), vec![stalled_file()], None));
        sessions.push(session);
        b.wait_transfer(LONG, |_| live() > i).await;
    }
    assert_eq!(live(), 8);

    // A ninth one is declined while they last.
    let session = raw.session().await;
    let outcome = offer_with(&session, "t8", vec![stalled_file()], None).await.unwrap().unwrap();
    assert!(outcome.declined, "{outcome:?}");
    assert_eq!(live(), 8);

    // Ending one frees its place.
    let first = b.engine.transfers().into_iter().find(|t| t.direction == Direction::Receive && !t.state.is_final()).unwrap();
    assert!(b.engine.cancel(&first.id));
    b.wait_final(&first.id, LONG).await;
    let session = raw.session().await;
    let _ninth = offer_with(&session, "t9", vec![stalled_file()], None);
    b.wait_transfer(LONG, |_| live() == 8).await;
    assert_eq!(live(), 8);
    drop((sessions, sending));
}
