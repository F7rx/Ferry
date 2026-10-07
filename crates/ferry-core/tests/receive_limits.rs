//! Receive admission: requests waiting for a decision, live sessions and
//! restored transfers all count against the same per-peer cap, so approving
//! several requests at once can never add up to more sessions than allowed.

mod common;

use common::raw::*;
use common::*;
use ferry_core::Engine;
use ferry_core::model::*;
use ferry_core::proto::PrepareUploadRequest;
use std::sync::Arc;
use std::time::Duration;

/// Live sessions one peer (IP address) may have (`MAX_SESSIONS_PER_PEER`).
const PER_PEER: usize = 8;
const SIZE: usize = 2 * 1024 * 1024 + 3;
const PREFIX: usize = 1024 * 1024 + 7;

fn small(sender: &RawSender, i: usize) -> PrepareUploadRequest {
    sender.offer(&[file_dto("f", &format!("note{i}.txt"), 10, None)], None)
}

async fn status(sender: &RawSender, offer: &PrepareUploadRequest) -> u16 {
    match sender.prepare(offer).await {
        Ok(Some(_)) => 200,
        Ok(None) => 204,
        Err(e) => e.status().unwrap_or_else(|| panic!("{e}")),
    }
}

fn spawn_prepare(sender: &Arc<RawSender>, offer: PrepareUploadRequest) -> tokio::task::JoinHandle<u16> {
    let sender = sender.clone();
    tokio::spawn(async move { status(&sender, &offer).await })
}

fn live_receives(engine: &Engine) -> Vec<TransferSummary> {
    engine.transfers().into_iter().filter(|t| t.direction == Direction::Receive && !t.state.is_final()).collect()
}

/// Asked and approved one after the other.
async fn approve(engine: &Arc<Engine>, prompts: &mut tokio::sync::mpsc::UnboundedReceiver<String>, sender: &Arc<RawSender>, i: usize) {
    let task = spawn_prepare(sender, small(sender, i));
    let id = next_prompt(prompts).await;
    assert!(engine.respond(&id, Decision::accept_all()));
    assert_eq!(task.await.unwrap(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_requests_hold_their_session_slot() {
    let rx = peer("Receiver").await;
    let mut prompts = prompts(&rx.engine);
    let sender = Arc::new(RawSender::new(&rx.engine));
    for i in 0..PER_PEER - 1 {
        approve(&rx.engine, &mut prompts, &sender, i).await;
    }

    // Three at once with one slot left: one is asked, the others turned away
    // right away instead of all being approved into ten sessions.
    let tasks: Vec<_> = (0..3).map(|i| spawn_prepare(&sender, small(&sender, 100 + i))).collect();
    let id = next_prompt(&mut prompts).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(prompts.try_recv().is_err(), "only one request may wait for the last slot");
    assert!(rx.engine.respond(&id, Decision::accept_all()));
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    results.sort();
    assert_eq!(results, [200, 409, 409]);
    assert_eq!(live_receives(&rx.engine).len(), PER_PEER);
    assert_eq!(status(&sender, &small(&sender, 200)).await, 409);

    // A declined request gives its slot back.
    let session = live_receives(&rx.engine)[0].id.clone();
    assert!(rx.engine.cancel(&session));
    let waiting = spawn_prepare(&sender, small(&sender, 300));
    let id = next_prompt(&mut prompts).await;
    assert_eq!(status(&sender, &small(&sender, 301)).await, 409, "the waiting request holds the slot");
    assert!(rx.engine.respond(&id, Decision::decline()));
    assert_eq!(waiting.await.unwrap(), 403);

    // So does one its sender withdraws.
    let waiting = spawn_prepare(&sender, small(&sender, 302));
    next_prompt(&mut prompts).await;
    sender.client.cancel(None).await;
    assert_eq!(waiting.await.unwrap(), 403);

    // And a session that finishes.
    let done_offer = sender.offer(&[file_dto("f", "done.bin", 4, None)], None);
    let task = {
        let sender = sender.clone();
        tokio::spawn(async move { sender.accepted(&done_offer).await })
    };
    let id = next_prompt(&mut prompts).await;
    assert!(rx.engine.respond(&id, Decision::accept_all()));
    let accepted = task.await.unwrap();
    assert_eq!(status(&sender, &small(&sender, 303)).await, 409);
    assert_eq!(sender.upload(&accepted, "f", None, b"done").await, 200);
    approve(&rx.engine, &mut prompts, &sender, 304).await;
    assert_eq!(live_receives(&rx.engine).len(), PER_PEER);
    assert_eq!(status(&sender, &small(&sender, 305)).await, 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_auto_accepts_stop_at_the_cap() {
    let data_dir = tempfile::tempdir().unwrap();
    let save_dir = tempfile::tempdir().unwrap();
    let identity = ferry_core::identity::Identity::generate().unwrap();
    // One of "my devices": accepted without asking.
    ferry_core::db::Db::open(&data_dir.path().join("ferry.db"))
        .unwrap()
        .upsert_device(&ferry_core::db::KnownDevice {
            fingerprint: identity.fingerprint.clone(),
            alias: "Raw sender".into(),
            custom_alias: None,
            device_model: None,
            device_kind: DeviceKind::Desktop,
            trusted: true,
            favorite: false,
            mine: true,
            last_address: None,
            last_port: None,
            last_protocol: None,
            last_seen_ms: 0,
            is_ferry: true,
        })
        .unwrap();
    let rx = start_receiver(data_dir.path(), save_dir.path(), |_| {}).await;
    let prompts = respond_all(&rx, Decision::decline());
    let sender = Arc::new(RawSender::with_identity(identity, &rx));

    let tasks: Vec<_> = (0..PER_PEER + 2).map(|i| spawn_prepare(&sender, small(&sender, i))).collect();
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 0, "accepted without asking");
    assert_eq!(results.iter().filter(|s| **s == 200).count(), PER_PEER, "{results:?}");
    assert_eq!(results.iter().filter(|s| **s == 409).count(), 2, "{results:?}");
    assert_eq!(live_receives(&rx).len(), PER_PEER);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_transfers_count_once_and_restore_waits_for_a_slot() {
    let data_dir = tempfile::tempdir().unwrap();
    let save_dir = tempfile::tempdir().unwrap();
    let rx = start_receiver(data_dir.path(), save_dir.path(), |_| {}).await;
    respond_all(&rx, Decision::accept_all());
    let mut sender = RawSender::new(&rx);
    let data = pattern(SIZE, 21);
    let tid = uuid::Uuid::new_v4().to_string();
    let resumable = sender.offer(&[file_dto("f", "big.bin", SIZE as u64, None)], Some(&tid));
    let session = sender.accepted(&resumable).await;
    assert_eq!(sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    for i in 0..PER_PEER - 1 {
        assert_eq!(status(&sender, &small(&sender, i)).await, 200);
    }
    assert_eq!(status(&sender, &small(&sender, 100)).await, 409);
    // Coming back to a session it already has takes no second slot.
    let again = sender.accepted(&resumable).await;
    assert_eq!(again.ferry.unwrap().offsets["f"], PREFIX as u64);

    // After a restart the transfer is restored only when there is room.
    rx.shutdown().await;
    let rx = start_receiver(data_dir.path(), save_dir.path(), |_| {}).await;
    respond_all(&rx, Decision::accept_all());
    sender.retarget(&rx);
    for i in 0..PER_PEER {
        assert_eq!(status(&sender, &small(&sender, 200 + i)).await, 200);
    }
    assert_eq!(status(&sender, &resumable).await, 409, "restoring may not exceed the cap");
    let one = live_receives(&rx)[0].id.clone();
    assert!(rx.cancel(&one));
    let restored = sender.accepted(&resumable).await;
    assert_eq!(restored.ferry.unwrap().offsets["f"], PREFIX as u64, "restored, not started over");
    assert_eq!(status(&sender, &small(&sender, 300)).await, 409, "the restored transfer holds its slot");
    assert_eq!(live_receives(&rx).len(), PER_PEER);
}
