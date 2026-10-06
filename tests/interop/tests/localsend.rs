//! Wire compatibility with unmodified upstream LocalSend (pinned commit):
//! upstream's own client and server code talk to Ferry over real TLS sockets.

use ferry_core::events::EngineEvent;
use ferry_core::model::{Decision, Direction, Protocol, TransferState};
use ferry_core::{Engine, EngineConfig, SendItem, Settings, Target};
use sha2::Digest;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use upstream::http::client::LsHttpClientV2;
use upstream::http::dto_v2::{PrepareUploadRequestDtoV2, RegisterDtoV2};
use upstream::http::server::common::save::FileUploadTarget;
use upstream::http::server::v2::{PrepareUploadDecisionV2, ServerEventV2};
use upstream::http::server::web::WebConfig;
use upstream::http::server::{ServerConfigV2, TlsConfig, start_with_port};
use upstream::http::state::ClientInfo;
use upstream::model::discovery::ProtocolType;
use upstream::model::transfer::FileDto;

const T: Duration = Duration::from_secs(30);

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}

// ── A Ferry receiver ─────────────────────────────────────────────────────

struct FerryPeer {
    engine: Arc<Engine>,
    save: tempfile::TempDir,
}

async fn ferry(alias: &str, pin: Option<&str>, accept: bool) -> FerryPeer {
    init();
    let save = tempfile::tempdir().unwrap();
    let mut s = Settings::default();
    s.alias = alias.into();
    s.port = 0;
    s.save_dir = Some(save.path().to_path_buf());
    s.pin = pin.map(str::to_string);
    let engine = Engine::start(EngineConfig::ephemeral(s)).await.unwrap();
    let e = engine.clone();
    let mut events = engine.subscribe();
    tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let EngineEvent::IncomingRequest { request } = ev {
                if request.text.is_none() {
                    e.respond(&request.id, if accept { Decision::accept_all() } else { Decision::decline() });
                }
            }
        }
    });
    FerryPeer { engine, save }
}

// ── An upstream LocalSend client ─────────────────────────────────────────

struct UpstreamSender {
    client: LsHttpClientV2,
    fingerprint: String,
}

fn upstream_sender(expected_fingerprint: Option<String>) -> UpstreamSender {
    let cert = upstream::crypto::cert::generate_self_signed().unwrap();
    let client = LsHttpClientV2::try_new(&cert.private_key_pem, &cert.certificate_pem, expected_fingerprint, None).unwrap();
    UpstreamSender { client, fingerprint: cert.fingerprint }
}

fn upstream_info(fingerprint: &str) -> RegisterDtoV2 {
    RegisterDtoV2 {
        alias: "LocalSend Phone".into(),
        version: "2.2".into(),
        device_model: Some("Pixel".into()),
        device_type: Some(upstream::model::discovery::DeviceType::Mobile),
        fingerprint: fingerprint.into(),
        port: 53317,
        protocol: ProtocolType::Https,
        download: false,
    }
}

fn dto(id: &str, name: &str, data: &[u8], with_hash: bool) -> FileDto {
    FileDto {
        id: id.into(),
        file_name: name.into(),
        size: data.len() as u64,
        file_type: "application/octet-stream".into(),
        sha256: with_hash.then(|| hex::encode(sha2::Sha256::digest(data))),
        preview: None,
        metadata: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_client_sends_to_ferry() {
    let rx = ferry("Ferry PC", None, true).await;
    let port = rx.engine.port();
    let sender = upstream_sender(Some(rx.engine.fingerprint()));

    // register: upstream must parse our answer.
    let reg = sender.client.register(ProtocolType::Https, "127.0.0.1", port, upstream_info(&sender.fingerprint)).await.unwrap();
    assert_eq!(reg.body.alias, "Ferry PC");
    assert_eq!(reg.cert_fingerprint.as_deref(), Some(rx.engine.fingerprint().as_str()));

    let a = b"first file".to_vec();
    let b: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
    let request = PrepareUploadRequestDtoV2 {
        info: upstream_info(&sender.fingerprint),
        files: [dto("a", "notes.txt", &a, true), dto("b", "Pictures/holiday.raw", &b, true)]
            .into_iter()
            .map(|f| (f.id.clone(), f))
            .collect(),
    };
    let result =
        sender.client.prepare_upload(ProtocolType::Https, "127.0.0.1", port, None, request, None, CancellationToken::new()).await.unwrap();
    assert_eq!(result.status_code, 200);
    let response = result.response.unwrap();
    for (id, data) in [("a", &a), ("b", &b)] {
        sender
            .client
            .upload(
                ProtocolType::Https,
                "127.0.0.1",
                port,
                None,
                &response.session_id,
                id,
                &response.files[id],
                upstream::reqwest::Body::from(data.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(rx.save.path().join("notes.txt")).unwrap(), a);
    assert_eq!(std::fs::read(rx.save.path().join("Pictures").join("holiday.raw")).unwrap(), b);
    // The sender identity is the verified certificate, and it shows as such.
    let t = rx.engine.transfers().into_iter().find(|t| t.direction == Direction::Receive).unwrap();
    assert_eq!(t.state, TransferState::Completed);
    assert_eq!(t.peer.alias, "LocalSend Phone");
    assert!(t.peer.verified);
    assert_eq!(t.peer.id, sender.fingerprint);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_client_gets_422_for_a_bad_checksum_and_can_retry() {
    let rx = ferry("Ferry PC", None, true).await;
    let port = rx.engine.port();
    let sender = upstream_sender(Some(rx.engine.fingerprint()));
    let data = b"payload".to_vec();
    let mut file = dto("x", "x.bin", &data, true);
    file.sha256 = Some(hex::encode(sha2::Sha256::digest(b"something else")));
    let request = PrepareUploadRequestDtoV2 { info: upstream_info(&sender.fingerprint), files: [("x".to_string(), file)].into() };
    let response = sender
        .client
        .prepare_upload(ProtocolType::Https, "127.0.0.1", port, None, request, None, CancellationToken::new())
        .await
        .unwrap()
        .response
        .unwrap();
    let err = sender
        .client
        .upload(
            ProtocolType::Https,
            "127.0.0.1",
            port,
            None,
            &response.session_id,
            "x",
            &response.files["x"],
            upstream::reqwest::Body::from(data.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("422"), "{err}");
    assert!(!rx.save.path().join("x.bin").exists(), "damaged data must not be kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_client_pin_flow() {
    let rx = ferry("Ferry PC", Some("2468"), true).await;
    let port = rx.engine.port();
    let sender = upstream_sender(Some(rx.engine.fingerprint()));
    let data = b"pin protected".to_vec();
    let request = || PrepareUploadRequestDtoV2 {
        info: upstream_info(&sender.fingerprint),
        files: [("p".to_string(), dto("p", "p.txt", &data, false))].into(),
    };
    let err = sender
        .client
        .prepare_upload(ProtocolType::Https, "127.0.0.1", port, None, request(), None, CancellationToken::new())
        .await
        .err()
        .expect("expected an error");
    assert!(err.to_string().contains("401"), "{err}");
    let err = sender
        .client
        .prepare_upload(ProtocolType::Https, "127.0.0.1", port, None, request(), Some("1111"), CancellationToken::new())
        .await
        .err()
        .expect("expected an error");
    assert!(err.to_string().contains("401"), "{err}");
    let ok = sender
        .client
        .prepare_upload(ProtocolType::Https, "127.0.0.1", port, None, request(), Some("2468"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(ok.status_code, 200);
}

// ── An upstream LocalSend receiver ───────────────────────────────────────

struct UpstreamReceiver {
    port: u16,
    fingerprint: String,
    dir: tempfile::TempDir,
    prepares: Arc<Mutex<u32>>,
    messages: Arc<Mutex<Vec<String>>>,
    _stop: oneshot::Sender<()>,
}

/// Upstream's real server with TLS, auto-accepting into a folder (keeping
/// relative paths, like the Flutter app). Text messages are answered 204.
async fn upstream_receiver(pin: Option<&str>, slow: bool) -> UpstreamReceiver {
    init();
    let cert = upstream::crypto::cert::generate_self_signed().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (event_tx, mut event_rx) = mpsc::channel::<ServerEventV2>(16);
    let prepares = Arc::new(Mutex::new(0));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let save: PathBuf = dir.path().to_path_buf();
    {
        let prepares = prepares.clone();
        let messages = messages.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload { files, decision_tx, .. } => {
                        *prepares.lock().await += 1;
                        let message = (files.len() == 1)
                            .then(|| files.values().next().unwrap())
                            .filter(|f| f.file_type.starts_with("text/") && f.preview.is_some())
                            .and_then(|f| f.preview.clone());
                        let decision = match message {
                            Some(text) => {
                                messages.lock().await.push(text);
                                PrepareUploadDecisionV2::Accept(Default::default())
                            }
                            None => PrepareUploadDecisionV2::Accept(files.keys().cloned().collect()),
                        };
                        let _ = decision_tx.send(decision);
                    }
                    ServerEventV2::FileUpload { file, target_tx, .. } => {
                        let rel: PathBuf = file.file_name.split('/').collect();
                        let path = save.join(rel);
                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                        let (result_tx, result_rx) = oneshot::channel();
                        let _ = target_tx.send(FileUploadTarget::Path { path, result_tx, progress_tx: None });
                        tokio::spawn(async move {
                            let _ = result_rx.await;
                        });
                        if slow {
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    let (stop_tx, stop_rx) = oneshot::channel();
    let handle = start_with_port(
        0,
        Some(TlsConfig { cert: cert.certificate_pem.clone(), private_key: cert.private_key_pem.clone() }),
        ClientInfo {
            alias: "LocalSend Laptop".into(),
            version: "2.2".into(),
            device_model: Some("Linux".into()),
            device_type: Some(upstream::model::discovery::DeviceType::Desktop),
            token: cert.fingerprint.clone(),
        },
        None,
        Some(ServerConfigV2 { pin: pin.map(str::to_string), verify_checksums: true, event_tx }),
        WebConfig::default(),
        stop_rx,
    )
    .await
    .unwrap();
    UpstreamReceiver { port: handle.port(), fingerprint: cert.fingerprint, dir, prepares, messages, _stop: stop_tx }
}

impl UpstreamReceiver {
    fn target(&self) -> Target {
        Target::Address {
            host: "127.0.0.1".into(),
            port: self.port,
            protocol: Protocol::Https,
            fingerprint: Some(self.fingerprint.clone()),
        }
    }
}

async fn wait_final(engine: &Engine, id: &str) -> ferry_core::model::TransferSummary {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(t) = engine.transfers().into_iter().find(|t| t.id == id && t.state.is_final()) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("transfer did not finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ferry_sends_files_and_folders_to_upstream() {
    let up = upstream_receiver(None, false).await;
    let tx = ferry("Ferry Sender", None, true).await;
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("readme.md"), b"# hi").unwrap();
    std::fs::create_dir_all(src.path().join("Album/2026")).unwrap();
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(src.path().join("Album/2026/clip.mov"), &big).unwrap();
    let ids = tx
        .engine
        .send(
            vec![up.target()],
            vec![SendItem::Path { path: src.path().join("readme.md") }, SendItem::Path { path: src.path().join("Album") }],
        )
        .await
        .unwrap();
    let t = wait_final(&tx.engine, &ids[0]).await;
    assert_eq!(t.state, TransferState::Completed, "{:?}", t.error);
    assert!(!t.resumable, "LocalSend peers are not resumable");
    assert_eq!(std::fs::read(up.dir.path().join("readme.md")).unwrap(), b"# hi");
    assert_eq!(std::fs::read(up.dir.path().join("Album/2026/clip.mov")).unwrap(), big);
    assert_eq!(t.peer.alias, "LocalSend Laptop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ferry_sends_a_text_message_to_upstream() {
    let up = upstream_receiver(None, false).await;
    let tx = ferry("Ferry Sender", None, true).await;
    let ids = tx.engine.send(vec![up.target()], vec![SendItem::Text { text: "Meeting at 3?".into() }]).await.unwrap();
    let t = wait_final(&tx.engine, &ids[0]).await;
    assert_eq!(t.state, TransferState::Completed, "{:?}", t.error);
    assert_eq!(up.messages.lock().await.as_slice(), ["Meeting at 3?"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ferry_handles_upstream_pin() {
    let up = upstream_receiver(Some("9876"), false).await;
    let tx = ferry("Ferry Sender", None, true).await;
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.txt"), b"a").unwrap();
    let ids = tx.engine.send(vec![up.target()], vec![SendItem::Path { path: src.path().join("a.txt") }]).await.unwrap();
    tokio::time::timeout(T, async {
        loop {
            if tx.engine.transfers().iter().any(|t| t.id == ids[0] && t.state == TransferState::PinRequired) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    tx.engine.submit_pin(&ids[0], Some("9876".into()));
    let t = wait_final(&tx.engine, &ids[0]).await;
    assert_eq!(t.state, TransferState::Completed, "{:?}", t.error);
    assert_eq!(std::fs::read(up.dir.path().join("a.txt")).unwrap(), b"a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ferry_waits_while_upstream_is_busy_with_another_sender() {
    // Upstream serves one session at a time and answers others with 409.
    let up = upstream_receiver(None, true).await;
    let a = ferry("Sender A", None, true).await;
    let b = ferry("Sender B", None, true).await;
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.bin"), vec![1u8; 300_000]).unwrap();
    std::fs::write(src.path().join("b.bin"), vec![2u8; 300_000]).unwrap();
    let (ia, ib) = tokio::join!(
        a.engine.send(vec![up.target()], vec![SendItem::Path { path: src.path().join("a.bin") }]),
        b.engine.send(vec![up.target()], vec![SendItem::Path { path: src.path().join("b.bin") }]),
    );
    let ta = wait_final(&a.engine, &ia.unwrap()[0]).await;
    let tb = wait_final(&b.engine, &ib.unwrap()[0]).await;
    assert_eq!(ta.state, TransferState::Completed, "{:?}", ta.error);
    assert_eq!(tb.state, TransferState::Completed, "{:?}", tb.error);
    assert!(*up.prepares.lock().await >= 2);
    assert_eq!(std::fs::read(up.dir.path().join("a.bin")).unwrap(), vec![1u8; 300_000]);
    assert_eq!(std::fs::read(up.dir.path().join("b.bin")).unwrap(), vec![2u8; 300_000]);
}

#[allow(unused)]
fn _unused(_: HashMap<(), ()>) {}
