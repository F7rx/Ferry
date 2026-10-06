//! Resumable transfers: network loss, receiver restart, pause/resume.
//! Each test checks the bytes on the wire to prove the transfer *continued*
//! instead of starting over.

mod common;

use common::proxy::Proxy;
use common::*;
use ferry_core::events::EngineEvent;
use ferry_core::model::*;
use ferry_core::{Engine, EngineConfig, SendItem, Settings, Target};
use std::net::SocketAddr;
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

fn via(proxy: &Proxy, fingerprint: String) -> Target {
    Target::Address { host: "127.0.0.1".into(), port: proxy.port, protocol: Protocol::Https, fingerprint: Some(fingerprint) }
}

fn backend(engine: &Engine) -> SocketAddr {
    format!("127.0.0.1:{}", engine.port()).parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_drop_continues_from_the_last_byte() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let proxy = Proxy::start(backend(&rx.engine)).await;
    proxy.set_rate(40 * MIB);

    let src = tempfile::tempdir().unwrap();
    let data = pattern(40 * MIB as usize, 11);
    let path = write_file(src.path(), "movie.mkv", &data);
    let ids = tx.engine.send(vec![via(&proxy, rx.engine.fingerprint())], vec![SendItem::Path { path }]).await.unwrap();

    tx.wait_transfer(T, |t| t.id == ids[0] && t.state == TransferState::Transferring && t.bytes_done > MIB).await;
    proxy.cut_after(14 * MIB);
    let lost = tx.wait_transfer(T, |t| t.id == ids[0] && t.state == TransferState::Reconnecting).await;
    assert!(lost.error.as_ref().unwrap().message.contains("Waiting for"), "{:?}", lost.error);
    assert!(lost.resumable);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    proxy.up();
    let done = tx.wait_final(&ids[0], Duration::from_secs(60)).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.error);
    rx.wait_received(T).await;
    assert!(std::fs::read(rx.saved("movie.mkv")).unwrap() == data);

    let wire = proxy.upstream_bytes();
    assert!(wire < data.len() as u64 * 115 / 100, "resent too much: {wire} bytes on the wire for {} bytes", data.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_restart_resumes_from_disk_without_asking_again() {
    init_tracing();
    let data_dir = tempfile::tempdir().unwrap();
    let save_dir = tempfile::tempdir().unwrap();
    let settings = || Settings { alias: "Receiver".into(), port: 0, save_dir: Some(save_dir.path().to_path_buf()), ..Settings::default() };
    let config = || EngineConfig { data_dir: Some(data_dir.path().to_path_buf()), settings_override: Some(settings()), discovery: false };

    let rx1 = Engine::start(config()).await.unwrap();
    let fingerprint = rx1.fingerprint();
    {
        let engine = rx1.clone();
        let mut events = engine.subscribe();
        tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                if let EngineEvent::IncomingRequest { request } = e {
                    engine.respond(&request.id, Decision::accept_all());
                }
            }
        });
    }
    let mut tx = peer("Sender").await;
    let proxy = Proxy::start(backend(&rx1)).await;
    proxy.set_rate(12 * MIB);

    let src = tempfile::tempdir().unwrap();
    let data = pattern(40 * MIB as usize, 12);
    let path = write_file(src.path(), "backup.tar", &data);
    let ids = tx.engine.send(vec![via(&proxy, fingerprint.clone())], vec![SendItem::Path { path }]).await.unwrap();

    // Long enough for at least one durable checkpoint (every 2 s).
    tx.wait_transfer(Duration::from_secs(60), |t| t.id == ids[0] && t.bytes_done > 30 * MIB).await;
    proxy.down();
    rx1.shutdown().await;
    drop(rx1);

    // Same identity (same data dir), new process, new port.
    let rx2 = Engine::start(config()).await.unwrap();
    assert_eq!(rx2.fingerprint(), fingerprint);
    let mut rx2_events = rx2.subscribe();
    proxy.set_backend(backend(&rx2));
    proxy.up();

    let done = tx.wait_final(&ids[0], Duration::from_secs(90)).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.error);
    assert!(std::fs::read(save_dir.path().join("backup.tar")).unwrap() == data);
    // The restarted receiver continued on its own: no new accept prompt.
    while let Ok(e) = rx2_events.try_recv() {
        assert!(!matches!(e, EngineEvent::IncomingRequest { .. }), "resumed transfer must not prompt again");
    }
    let wire = proxy.upstream_bytes();
    assert!(wire < data.len() as u64 * 150 / 100, "resent too much: {wire} bytes on the wire for {} bytes", data.len());
    // Only the finished file remains.
    assert_eq!(files_in(save_dir.path()), vec!["backup.tar"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_by_user() {
    let mut rx = peer("Receiver").await;
    let mut tx = peer("Sender").await;
    rx.auto_respond(Decision::accept_all());
    let proxy = Proxy::start(backend(&rx.engine)).await;
    proxy.set_rate(16 * MIB);
    let src = tempfile::tempdir().unwrap();
    let data = pattern(32 * MIB as usize, 13);
    let path = write_file(src.path(), "photos.zip", &data);
    let ids = tx.engine.send(vec![via(&proxy, rx.engine.fingerprint())], vec![SendItem::Path { path }]).await.unwrap();

    tx.wait_transfer(T, |t| t.id == ids[0] && t.bytes_done > 4 * MIB).await;
    assert!(tx.engine.pause(&ids[0]));
    tx.wait_transfer(T, |t| t.id == ids[0] && t.state == TransferState::Paused).await;
    let at_pause = proxy.upstream_bytes();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(proxy.upstream_bytes() - at_pause < MIB, "paused transfer kept sending");

    assert!(tx.engine.resume(&ids[0]));
    let done = tx.wait_final(&ids[0], Duration::from_secs(60)).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.error);
    rx.wait_received(T).await;
    assert!(std::fs::read(rx.saved("photos.zip")).unwrap() == data);
    assert!(proxy.upstream_bytes() < data.len() as u64 * 115 / 100);
}
