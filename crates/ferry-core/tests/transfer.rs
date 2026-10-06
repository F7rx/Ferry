//! End-to-end transfers between two engines over real TLS on loopback.

mod common;

use common::*;
use ferry_core::SendItem;
use ferry_core::events::EngineEvent;
use ferry_core::model::*;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sends_files_and_folders() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());

    let src = tempfile::tempdir().unwrap();
    let a = write_file(src.path(), "hello.txt", b"hello world");
    let album = src.path().join("Album");
    write_file(&album, "one.jpg", &pattern(200_000, 1));
    write_file(&album, "nested/two.bin", &pattern(1_500_000, 2));

    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path: a }, SendItem::Path { path: album }]).await.unwrap();
    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Completed, "{:?}", sent.error);
    assert_eq!(sent.file_count, 3);
    assert_eq!(sent.bytes_done, sent.total_bytes);
    assert!(sent.resumable, "Ferry peers negotiate resumable transfers");

    let received = rx.wait_received(T).await;
    assert_eq!(received.state, TransferState::Completed);
    assert_eq!(files_in(rx.save_dir.path()), vec!["Album/nested/two.bin", "Album/one.jpg", "hello.txt"]);
    assert_eq!(std::fs::read(rx.saved("Album/nested/two.bin")).unwrap(), pattern(1_500_000, 2));
    assert_eq!(std::fs::read(rx.saved("hello.txt")).unwrap(), b"hello world");
    // Mark-of-the-Web on both write paths (in-memory small file, part file).
    #[cfg(windows)]
    for name in ["hello.txt", "Album/nested/two.bin"] {
        let tag = std::fs::read_to_string(format!("{}:Zone.Identifier", rx.saved(name).display())).unwrap();
        assert!(tag.contains("ZoneId=3"), "{name}: {tag}");
    }

    // History on both sides.
    let rh = rx.engine.history(10, None, Some(Direction::Receive)).unwrap();
    assert_eq!(rh.len(), 3);
    assert!(rh.iter().all(|h| h.status == HistoryStatus::Completed));
    let th = tx.engine.history(10, None, Some(Direction::Send)).unwrap();
    assert_eq!(th.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_a_large_file_with_matching_hash() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let data = pattern(64 * 1024 * 1024 + 17, 7);
    let path = write_file(src.path(), "big.bin", &data);

    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path }]).await.unwrap();
    let sent = tx.wait_final(&ids[0], Duration::from_secs(120)).await;
    assert_eq!(sent.state, TransferState::Completed, "{:?}", sent.error);
    rx.wait_received(T).await;
    let got = std::fs::read(rx.saved("big.bin")).unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data);
    // No part files left behind.
    assert_eq!(files_in(rx.save_dir.path()), vec!["big.bin"]);
    let hist = rx.engine.history(1, None, None).unwrap();
    assert!(hist[0].verified, "receiver should have been told the hashes match");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_is_reported_to_sender() {
    let rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::decline());
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "x.txt", b"x");
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path }]).await.unwrap();
    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Declined);
    assert!(files_in(rx.save_dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_acceptance_skips_the_rest() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    let src = tempfile::tempdir().unwrap();
    let a = write_file(src.path(), "keep.txt", b"keep");
    let b = write_file(src.path(), "skip.txt", b"skip");
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path: a }, SendItem::Path { path: b }]).await.unwrap();

    let request = match rx.wait_event(T, |e| matches!(e, EngineEvent::IncomingRequest { .. })).await {
        EngineEvent::IncomingRequest { request } => request,
        _ => unreachable!(),
    };
    assert_eq!(request.files.len(), 2);
    assert_eq!(request.peer.alias, "Sender");
    assert!(request.peer.verified);
    let keep = request.files.iter().find(|f| f.name == "keep.txt").unwrap().id.clone();
    rx.engine.respond(&request.id, Decision { accept: Some(vec![keep]), ..Default::default() });

    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Completed);
    assert_eq!(sent.file_count, 2);
    assert_eq!(files_in(rx.save_dir.path()), vec!["keep.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_messages_are_delivered_without_files() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Text { text: "https://example.com/hello".into() }]).await.unwrap();
    let event = rx.wait_event(T, |e| matches!(e, EngineEvent::IncomingRequest { request } if request.text.is_some())).await;
    let EngineEvent::IncomingRequest { request } = event else { unreachable!() };
    assert_eq!(request.text.as_deref(), Some("https://example.com/hello"));
    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Completed);
    assert!(files_in(rx.save_dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_is_requested_and_checked() {
    let mut rx = peer_with("Receiver", |s| s.pin = Some("4711".into())).await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "secret.txt", b"psst");
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path }]).await.unwrap();

    let t = tx.wait_transfer(T, |t| t.id == ids[0] && t.state == TransferState::PinRequired).await;
    assert_eq!(t.error.unwrap().code, "pin_required");
    tx.engine.submit_pin(&ids[0], Some("0000".into()));
    let t = tx
        .wait_transfer(T, |t| {
            t.id == ids[0] && t.state == TransferState::PinRequired && t.error.as_ref().is_some_and(|e| e.code == "pin_invalid")
        })
        .await;
    assert_eq!(t.state, TransferState::PinRequired);
    tx.engine.submit_pin(&ids[0], Some("4711".into()));
    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Completed, "{:?}", sent.error);
    rx.wait_received(T).await;
    assert_eq!(std::fs::read(rx.saved("secret.txt")).unwrap(), b"psst");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_names_never_overwrite() {
    let rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    std::fs::write(rx.saved("report.pdf"), b"original").unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "report.pdf", b"incoming");
    for _ in 0..2 {
        let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path: path.clone() }]).await.unwrap();
        assert_eq!(tx.wait_final(&ids[0], T).await.state, TransferState::Completed);
    }
    assert_eq!(std::fs::read(rx.saved("report.pdf")).unwrap(), b"original");
    assert_eq!(std::fs::read(rx.saved("report (2).pdf")).unwrap(), b"incoming");
    assert_eq!(std::fs::read(rx.saved("report (3).pdf")).unwrap(), b"incoming");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_senders_are_both_served() {
    let rx = peer("Receiver").await;
    let mut a = peer("Alice").await;
    let mut b = peer("Bob").await;
    rx.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let pa = write_file(src.path(), "a.bin", &pattern(3_000_000, 3));
    let pb = write_file(src.path(), "b.bin", &pattern(3_000_000, 4));
    let (ia, ib) = tokio::join!(
        a.engine.send(vec![rx.target()], vec![SendItem::Path { path: pa }]),
        b.engine.send(vec![rx.target()], vec![SendItem::Path { path: pb }]),
    );
    let (ia, ib) = (ia.unwrap(), ib.unwrap());
    assert_eq!(a.wait_final(&ia[0], T).await.state, TransferState::Completed);
    assert_eq!(b.wait_final(&ib[0], T).await.state, TransferState::Completed);
    assert_eq!(files_in(rx.save_dir.path()), vec!["a.bin", "b.bin"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_cancel_cleans_up_receiver() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "huge.bin", &pattern(96 * 1024 * 1024, 5));
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path }]).await.unwrap();
    tx.wait_transfer(T, |t| t.id == ids[0] && t.bytes_done > 1_000_000).await;
    assert!(tx.engine.cancel(&ids[0]));
    assert_eq!(tx.wait_final(&ids[0], T).await.state, TransferState::Cancelled);
    let received = rx.wait_received(T).await;
    assert_eq!(received.state, TransferState::Cancelled);
    assert_eq!(received.error.unwrap().code, "cancelled_by_peer");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(files_in(rx.save_dir.path()).is_empty(), "part files must be removed: {:?}", files_in(rx.save_dir.path()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_cancel_stops_sender() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "huge.bin", &pattern(96 * 1024 * 1024, 6));
    let ids = tx.engine.send(vec![rx.target()], vec![SendItem::Path { path }]).await.unwrap();
    let incoming = rx.wait_transfer(T, |t| t.direction == Direction::Receive && t.bytes_done > 1_000_000).await;
    assert!(rx.engine.cancel(&incoming.id));
    let sent = tx.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Cancelled, "{:?}", sent.error);
    assert_eq!(sent.error.unwrap().code, "cancelled_by_peer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn group_drop_reaches_every_device_independently() {
    let laptop = peer("Laptop").await;
    let phone = peer("Phone").await;
    let tablet = peer("Tablet").await;
    laptop.auto_respond(Decision::accept_all());
    phone.auto_respond(Decision::accept_all());
    // One device declining must not affect the others.
    tablet.auto_respond(Decision::decline());

    let mut tx = peer("Sender").await;
    let src = tempfile::tempdir().unwrap();
    let data = pattern(12 * 1024 * 1024, 21);
    let path = write_file(src.path(), "video.mp4", &data);
    let ids = tx.engine.send(vec![laptop.target(), phone.target(), tablet.target()], vec![SendItem::Path { path }]).await.unwrap();
    assert_eq!(ids.len(), 3);

    let mut finals = Vec::new();
    for id in &ids {
        finals.push(tx.wait_final(id, Duration::from_secs(60)).await);
    }
    let drop_ids: std::collections::HashSet<_> =
        finals.iter().map(|t| t.drop_id.clone().expect("group transfers share a drop id")).collect();
    assert_eq!(drop_ids.len(), 1, "one drop id for the whole group");
    let by_peer = |alias: &str| finals.iter().find(|t| t.peer.alias == alias).unwrap().state;
    assert_eq!(by_peer("Laptop"), TransferState::Completed);
    assert_eq!(by_peer("Phone"), TransferState::Completed);
    assert_eq!(by_peer("Tablet"), TransferState::Declined);
    assert!(std::fs::read(laptop.saved("video.mp4")).unwrap() == data);
    assert!(std::fs::read(phone.saved("video.mp4")).unwrap() == data);
    assert!(files_in(tablet.save_dir.path()).is_empty());
}
