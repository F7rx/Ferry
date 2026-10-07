//! A sender driven by hand: exact prepare-upload offers, partial uploads and
//! re-offers, over the same mutual TLS a real Ferry sender uses.

use bytes::Bytes;
use ferry_core::client::{ClientError, PeerAddress, PeerClient, PrepareResult};
use ferry_core::events::EngineEvent;
use ferry_core::identity::Identity;
use ferry_core::model::*;
use ferry_core::proto::*;
use ferry_core::{Engine, EngineConfig, Settings};
use indexmap::IndexMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

pub struct RawSender {
    pub identity: Identity,
    pub client: PeerClient,
}

impl RawSender {
    pub fn new(receiver: &Engine) -> RawSender {
        Self::with_identity(Identity::generate().unwrap(), receiver)
    }

    pub fn with_identity(identity: Identity, receiver: &Engine) -> RawSender {
        let client = client_for(&identity, receiver);
        RawSender { identity, client }
    }

    /// Talks to `receiver` from now on (a restarted receiver has a new port).
    pub fn retarget(&mut self, receiver: &Engine) {
        self.client = client_for(&self.identity, receiver);
    }

    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint.clone()
    }

    pub fn offer(&self, files: &[FileDto], transfer_id: Option<&str>) -> PrepareUploadRequest {
        PrepareUploadRequest {
            info: DeviceDto {
                alias: "Raw sender".into(),
                version: PROTOCOL_VERSION.into(),
                device_model: None,
                device_type: Some(DeviceKind::Desktop),
                fingerprint: self.identity.fingerprint.clone(),
                port: None,
                protocol: Some(Protocol::Https),
                download: false,
                ferry: Some(FerryHint::ours()),
            },
            files: files.iter().map(|f| (f.id.clone(), f.clone())).collect::<IndexMap<_, _>>(),
            ferry: transfer_id.map(|t| FerryPrepareRequest { transfer_id: t.to_string() }),
        }
    }

    /// prepare-upload, waiting out the receiver's request rate limit.
    pub async fn prepare(&self, request: &PrepareUploadRequest) -> Result<Option<PrepareUploadResponse>, ClientError> {
        loop {
            match self.client.prepare_upload(request, None, Duration::from_secs(60), &CancellationToken::new()).await {
                Err(ClientError::Status { status: 429, .. }) => tokio::time::sleep(Duration::from_millis(300)).await,
                Ok(PrepareResult::Accepted(r)) => return Ok(Some(r)),
                Ok(PrepareResult::NothingAccepted) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Expects the offer to be accepted.
    pub async fn accepted(&self, request: &PrepareUploadRequest) -> PrepareUploadResponse {
        self.prepare(request).await.expect("prepare-upload failed").expect("nothing accepted")
    }

    /// Uploads `data` (the bytes from `offset` on). Returns the HTTP status.
    pub async fn upload(&self, session: &PrepareUploadResponse, file_id: &str, offset: Option<u64>, data: &[u8]) -> u16 {
        let token = session.files.get(file_id).unwrap_or_else(|| panic!("no token for {file_id}"));
        let (tx, rx) = mpsc::channel(8);
        let chunks: Vec<Bytes> = data.chunks(256 * 1024).map(Bytes::copy_from_slice).collect();
        tokio::spawn(async move {
            for c in chunks {
                if tx.send(Ok(c)).await.is_err() {
                    break;
                }
            }
        });
        let result = self
            .client
            .upload(&session.session_id, file_id, token, offset, rx, Arc::new(AtomicU64::new(0)), &CancellationToken::new())
            .await;
        match result {
            Ok(_) => 200,
            Err(ClientError::Status { status, .. }) => status,
            Err(e) => panic!("upload failed: {e}"),
        }
    }
}

fn client_for(identity: &Identity, receiver: &Engine) -> PeerClient {
    let addr = PeerAddress { host: "127.0.0.1".into(), port: receiver.port(), protocol: Protocol::Https };
    PeerClient::new(identity, addr, Some(receiver.fingerprint())).unwrap()
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

pub fn file_dto(id: &str, name: &str, size: u64, sha256: Option<String>) -> FileDto {
    FileDto {
        id: id.into(),
        file_name: name.into(),
        size,
        file_type: "application/octet-stream".into(),
        sha256,
        preview: None,
        metadata: None,
    }
}

/// A receiver with its own data folder, so it can be restarted as the same device.
pub async fn start_receiver(data_dir: &Path, save_dir: &Path, tweak: impl FnOnce(&mut Settings)) -> Arc<Engine> {
    super::init_tracing();
    let mut settings = Settings { alias: "Receiver".into(), port: 0, save_dir: Some(save_dir.to_path_buf()), ..Settings::default() };
    tweak(&mut settings);
    Engine::start(EngineConfig { data_dir: Some(data_dir.to_path_buf()), settings_override: Some(settings), discovery: false })
        .await
        .unwrap()
}

/// Answers every accept prompt with `decision` and counts the prompts.
pub fn respond_all(engine: &Arc<Engine>, decision: Decision) -> Arc<AtomicUsize> {
    let prompts = Arc::new(AtomicUsize::new(0));
    let counter = prompts.clone();
    let engine = engine.clone();
    let mut events = engine.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(EngineEvent::IncomingRequest { request }) if request.text.is_none() => {
                    counter.fetch_add(1, Ordering::SeqCst);
                    engine.respond(&request.id, decision.clone());
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    });
    prompts
}

/// Forwards the ids of accept prompts, for tests that answer them one by one.
pub fn prompts(engine: &Arc<Engine>) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut events = engine.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(EngineEvent::IncomingRequest { request }) if request.text.is_none() => {
                    if tx.send(request.id).is_err() {
                        return;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    });
    rx
}

pub async fn next_prompt(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(super::T, rx.recv()).await.expect("no accept prompt").expect("event stream closed")
}
