//! Sending: one task per target device. A group drop is N such tasks sharing
//! one manifest; each has its own state, progress and retries.

use crate::client::{ClientError, PeerAddress, PeerClient, PrepareResult, describe_status};
use crate::db::NewHistoryEntry;
use crate::error::{ErrorInfo, Result};
use crate::events::EngineEvent;
use crate::fsutil::read::{hash_file_async, stream_file};
use crate::model::*;
use crate::proto::*;
use crate::shared::Shared;
use crate::transfer::{NewTransfer, TransferEntry};
use crate::util::now_ms;
use bytes::Bytes;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Concurrent upload streams across all transfers (protects the disk).
const GLOBAL_STREAMS: usize = 24;
/// Files below this are "small": many in parallel; verify skipped.
const SMALL_FILE: u64 = 1024 * 1024;
const MAX_FILE_ATTEMPTS: u32 = 3;
const BUSY_RETRY_FOR: Duration = Duration::from_secs(120);
/// How long a Ferry transfer keeps trying to reconnect before pausing.
const RECONNECT_WINDOW: Duration = Duration::from_secs(30 * 60);
const MAX_MANIFEST_FILES: usize = 100_000;

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SendItem {
    Path { path: PathBuf },
    Text { text: String },
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    /// A device from the directory (by id).
    Device { id: String },
    /// A manual address, optionally with the fingerprint we expect.
    Address { host: String, port: u16, protocol: Protocol, fingerprint: Option<String> },
}

#[derive(Clone, Debug)]
pub struct OutFile {
    pub id: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub size: u64,
    pub mime: String,
    pub modified: Option<SystemTime>,
    pub text: Option<String>,
}

struct Outgoing {
    id: String,
    target: Target,
    entry: Arc<TransferEntry>,
    files: Arc<Vec<OutFile>>,
    cancel: CancellationToken,
    /// Cancels the current attempt only (pause).
    attempt: Mutex<CancellationToken>,
    paused: Mutex<bool>,
    resume: Notify,
    pin_reply: Mutex<Option<oneshot::Sender<Option<String>>>>,
    transfer_id: String,
    remote_session: Mutex<Option<String>>,
    peer_fingerprint: Mutex<Option<String>>,
    done: Mutex<HashSet<String>>,
    attempts: Mutex<std::collections::HashMap<String, u32>>,
}

pub struct SendManager {
    shared: Arc<Shared>,
    outgoing: Mutex<std::collections::HashMap<String, Arc<Outgoing>>>,
    streams: Arc<Semaphore>,
    /// Signalled when a transfer lost its peer (discovery re-announces).
    pub lost_peer: Arc<Notify>,
}

impl SendManager {
    pub fn new(shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            outgoing: Mutex::new(Default::default()),
            streams: Arc::new(Semaphore::new(GLOBAL_STREAMS)),
            lost_peer: Arc::new(Notify::new()),
        })
    }

    /// Starts sending `items` to every target. Returns one transfer id per
    /// target (and per text item).
    pub async fn send(self: &Arc<Self>, targets: Vec<Target>, items: Vec<SendItem>) -> Result<Vec<String>> {
        let drop_id = (targets.len() > 1).then(|| uuid::Uuid::new_v4().to_string());
        self.send_with_drop(targets, items, drop_id).await
    }

    /// [`SendManager::send`] as part of a group drop spanning other transports.
    pub(crate) async fn send_with_drop(
        self: &Arc<Self>,
        targets: Vec<Target>,
        items: Vec<SendItem>,
        drop_id: Option<String>,
    ) -> Result<Vec<String>> {
        if targets.is_empty() || items.is_empty() {
            return Err(ErrorInfo::new("nothing_to_send", "Choose something to send and a device to send it to.").into());
        }
        let (texts, paths): (Vec<_>, Vec<_>) = items.into_iter().partition(|i| matches!(i, SendItem::Text { .. }));
        let mut ids = Vec::new();

        // Each text is its own LocalSend "message" request.
        for item in texts {
            let SendItem::Text { text } = item else { continue };
            if text.len() > 1024 * 1024 {
                return Err(ErrorInfo::new("text_too_long", "That text is too long to send as a message.").into());
            }
            let file = OutFile {
                id: uuid::Uuid::new_v4().to_string(),
                name: format!("{}.txt", uuid::Uuid::new_v4()),
                path: None,
                size: text.len() as u64,
                mime: "text/plain".into(),
                modified: None,
                text: Some(text),
            };
            let files = Arc::new(vec![file]);
            for target in &targets {
                ids.push(self.start(target.clone(), files.clone(), drop_id.clone()));
            }
        }

        if !paths.is_empty() {
            let paths: Vec<PathBuf> =
                paths.into_iter().filter_map(|i| if let SendItem::Path { path } = i { Some(path) } else { None }).collect();
            let files = tokio::task::spawn_blocking(move || build_manifest(&paths)).await.map_err(ErrorInfo::internal)??;
            if files.is_empty() {
                return Err(ErrorInfo::new("nothing_to_send", "The selection contains no files.").into());
            }
            let files = Arc::new(files);
            for target in &targets {
                ids.push(self.start(target.clone(), files.clone(), drop_id.clone()));
            }
        }
        Ok(ids)
    }

    fn start(self: &Arc<Self>, target: Target, files: Arc<Vec<OutFile>>, drop_id: Option<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let peer = self.target_peer_ref(&target);
        let text = files.first().and_then(|f| f.text.clone());
        let entry = self.shared.transfers.create(NewTransfer {
            id: id.clone(),
            direction: Direction::Send,
            drop_id,
            peer,
            files: files
                .iter()
                .map(|f| TransferFile {
                    id: f.id.clone(),
                    name: if f.text.is_some() { "Message".into() } else { f.name.clone() },
                    size: f.size,
                    mime: f.mime.clone(),
                    state: FileState::Pending,
                    bytes_done: 0,
                    error: None,
                    path: f.path.as_ref().map(|p| p.display().to_string()),
                })
                .collect(),
            state: TransferState::Preparing,
            resumable: false,
            text,
            save_dir: None,
            connection: None,
        });
        let out = Arc::new(Outgoing {
            id: id.clone(),
            target,
            entry,
            files,
            cancel: self.shared.shutdown.child_token(),
            attempt: Mutex::new(CancellationToken::new()),
            paused: Mutex::new(false),
            resume: Notify::new(),
            pin_reply: Mutex::new(None),
            transfer_id: uuid::Uuid::new_v4().to_string(),
            remote_session: Mutex::new(None),
            peer_fingerprint: Mutex::new(None),
            done: Mutex::new(HashSet::new()),
            attempts: Mutex::new(Default::default()),
        });
        self.outgoing.lock().unwrap().insert(id.clone(), out.clone());
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run(out.clone()).await;
            manager.finish_history(&out);
        });
        id
    }

    fn target_peer_ref(&self, target: &Target) -> PeerRef {
        match target {
            Target::Device { id } => match self.shared.devices.get(id) {
                Some(d) => PeerRef {
                    id: d.id.clone(),
                    alias: d.custom_alias.clone().unwrap_or(d.alias.clone()),
                    device_kind: d.device_kind,
                    device_model: d.device_model.clone(),
                    verified: d.verified,
                },
                None => PeerRef {
                    id: id.clone(),
                    alias: "Device".into(),
                    device_kind: DeviceKind::Desktop,
                    device_model: None,
                    verified: false,
                },
            },
            Target::Address { host, port, fingerprint, .. } => PeerRef {
                id: fingerprint.clone().unwrap_or_else(|| format!("{host}:{port}")),
                alias: format!("{host}:{port}"),
                device_kind: DeviceKind::Desktop,
                device_model: None,
                verified: fingerprint.is_some(),
            },
        }
    }

    // ── User actions ─────────────────────────────────────────────────────

    pub fn cancel(&self, id: &str) -> bool {
        let Some(out) = self.outgoing.lock().unwrap().get(id).cloned() else { return false };
        out.cancel.cancel();
        out.entry.fail(TransferState::Cancelled, Some(ErrorInfo::cancelled()));
        true
    }

    /// Pauses a resumable transfer (current uploads stop; data so far is kept).
    pub fn pause(&self, id: &str) -> bool {
        let Some(out) = self.outgoing.lock().unwrap().get(id).cloned() else { return false };
        if !out.entry.summary().resumable || out.entry.state().is_final() {
            return false;
        }
        *out.paused.lock().unwrap() = true;
        out.attempt.lock().unwrap().cancel();
        out.entry.set_state(TransferState::Paused);
        true
    }

    pub fn resume(&self, id: &str) -> bool {
        let Some(out) = self.outgoing.lock().unwrap().get(id).cloned() else { return false };
        let mut paused = out.paused.lock().unwrap();
        if !*paused {
            return false;
        }
        *paused = false;
        out.resume.notify_waiters();
        true
    }

    /// Supplies the PIN the receiver asked for (`None` gives up).
    pub fn submit_pin(&self, id: &str, pin: Option<String>) -> bool {
        let Some(out) = self.outgoing.lock().unwrap().get(id).cloned() else { return false };
        let tx = out.pin_reply.lock().unwrap().take();
        tx.is_some_and(|tx| tx.send(pin).is_ok())
    }

    /// The receiver told us it cancelled (it posted `/cancel` to our server).
    pub fn peer_cancelled(&self, identity: &PeerIdentity, session_id: Option<&str>) {
        let Some(session_id) = session_id else { return };
        let matching: Vec<Arc<Outgoing>> = self
            .outgoing
            .lock()
            .unwrap()
            .values()
            .filter(|o| o.remote_session.lock().unwrap().as_deref() == Some(session_id))
            .filter(|o| match (identity.fingerprint(), o.peer_fingerprint.lock().unwrap().as_deref()) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            })
            .cloned()
            .collect();
        for out in matching {
            out.cancel.cancel();
            out.entry.fail(TransferState::Cancelled, Some(ErrorInfo::cancelled_by_peer()));
        }
    }

    // ── The transfer task ────────────────────────────────────────────────

    async fn run(self: &Arc<Self>, out: Arc<Outgoing>) {
        let alias = out.entry.peer().alias;
        let result = self.run_inner(&out).await;
        if let Err(err) = result {
            if out.cancel.is_cancelled() {
                if let Some(session) = out.remote_session.lock().unwrap().clone() {
                    self.notify_cancel(&out, session);
                }
                return;
            }
            let info = match &err {
                SendError::Client(e) => describe_status(e, &alias),
                SendError::User(info) => info.clone(),
            };
            let state = match info.code.as_str() {
                "declined" => TransferState::Declined,
                "cancelled_by_peer" => TransferState::Cancelled,
                _ => TransferState::Failed,
            };
            out.entry.fail(state, Some(info));
        }
    }

    async fn run_inner(self: &Arc<Self>, out: &Arc<Outgoing>) -> std::result::Result<(), SendError> {
        let alias = out.entry.peer().alias;
        let (mut client, fingerprint) = self.connect(out).await?;
        *out.peer_fingerprint.lock().unwrap() = fingerprint.clone();
        let is_ferry = match (&fingerprint, client.addr.protocol) {
            (Some(fp), Protocol::Https) => {
                let ferry = client.hello().await.ok().flatten().is_some_and(|h| h.v >= 1 && h.caps.iter().any(|c| c == "resume"));
                self.shared.devices.set_ferry_confirmed(fp, ferry);
                ferry
            }
            _ => false,
        };
        out.entry.set_resumable(is_ferry);
        set_connection(&out.entry, &client.addr);

        let settings = self.shared.settings.get();
        let precompute = !is_ferry && settings.checksums_for_localsend;
        let hashes = if precompute { self.precompute_hashes(out).await? } else { Default::default() };

        let mut session = self.prepare(out, &client, is_ferry, &hashes, &alias).await?;
        loop {
            let Some(accepted) = session.take() else {
                // 204: a message was delivered, or the receiver accepted nothing.
                let is_message = out.files.iter().all(|f| f.text.is_some());
                if is_message {
                    for f in out.files.iter() {
                        out.entry.set_file_state(&f.id, FileState::Done, None);
                        out.entry.reset_file_progress(&f.id, f.size);
                    }
                    out.entry.set_state(TransferState::Completed);
                    return Ok(());
                }
                return Err(SendError::User(ErrorInfo::declined()));
            };
            let accepted_ids: HashSet<String> = accepted.files.keys().cloned().collect();
            if out.done.lock().unwrap().is_empty() {
                out.entry.skip_files(&accepted_ids);
            }
            *out.remote_session.lock().unwrap() = Some(accepted.session_id.clone());
            out.entry.set_error(None);
            out.entry.set_state(TransferState::Transferring);

            let attempt = out.cancel.child_token();
            *out.attempt.lock().unwrap() = attempt.clone();
            let outcome = self.upload_all(out, &client, &accepted, is_ferry, &hashes, &attempt).await;
            match outcome {
                UploadOutcome::Finished => {
                    let state = out.entry.conclude();
                    tracing::info!("Transfer {} to {alias} finished: {state:?}", out.id);
                    return Ok(());
                }
                UploadOutcome::Fatal(err) => return Err(err),
                UploadOutcome::Interrupted if out.cancel.is_cancelled() => return Err(SendError::User(ErrorInfo::cancelled())),
                UploadOutcome::Interrupted => {
                    if !is_ferry {
                        return Err(SendError::User(ErrorInfo::new("connection_lost", format!("Lost the connection to {alias}."))));
                    }
                    // Paused by the user, or the connection dropped: either way
                    // continue with the same transfer id when possible.
                    let (new_client, new_session) = self.reconnect(out, fingerprint.clone(), &hashes, &alias).await?;
                    client = new_client;
                    set_connection(&out.entry, &client.addr);
                    session = new_session;
                }
            }
        }
    }

    /// Finds a reachable channel and learns/pins the peer's identity.
    async fn connect(&self, out: &Arc<Outgoing>) -> std::result::Result<(PeerClient, Option<String>), SendError> {
        let alias = out.entry.peer().alias;
        let (candidates, fingerprint, device_id) = match &out.target {
            Target::Device { id } => {
                let fp = self.shared.devices.get(id).filter(|d| d.verified).map(|_| id.clone());
                (self.shared.devices.channels(id), fp, Some(id.clone()))
            }
            Target::Address { host, port, protocol, fingerprint } => {
                (vec![PeerAddress { host: host.clone(), port: *port, protocol: *protocol }], fingerprint.clone(), None)
            }
        };
        if candidates.is_empty() {
            return Err(SendError::User(ErrorInfo::unreachable(&alias)));
        }
        let dto = self.shared.device_dto();
        let mut last_err = None;
        for addr in candidates {
            if out.cancel.is_cancelled() {
                return Err(SendError::User(ErrorInfo::cancelled()));
            }
            let client = PeerClient::new(&self.shared.identity, addr.clone(), fingerprint.clone())
                .map_err(|e| SendError::User(ErrorInfo::internal(e)))?;
            let started = Instant::now();
            match client.register(&dto, Duration::from_secs(3)).await {
                Ok(result) => {
                    let rtt = started.elapsed().as_millis() as u32;
                    let identity = match (addr.protocol, &result.cert_fingerprint) {
                        (Protocol::Https, Some(fp)) => PeerIdentity::Verified { fingerprint: fp.clone() },
                        _ => PeerIdentity::PlainHttp,
                    };
                    let id = self.shared.devices.observe(crate::devices::Observation {
                        identity: identity.clone(),
                        addr: addr.clone(),
                        dto: result.device.clone(),
                        rtt_ms: Some(rtt),
                    });
                    out.entry.set_peer(self.shared.peer_ref(&identity, &result.device, &id));
                    let learned = identity.fingerprint().map(str::to_string);
                    // A manual address: pin what we just learned for everything after.
                    let client = if fingerprint.is_none() && learned.is_some() {
                        PeerClient::new(&self.shared.identity, addr, learned.clone())
                            .map_err(|e| SendError::User(ErrorInfo::internal(e)))?
                    } else {
                        client
                    };
                    return Ok((client, learned));
                }
                Err(err) => {
                    if let Some(id) = &device_id {
                        self.shared.devices.channel_failed(id, &addr);
                    }
                    if matches!(err, ClientError::IdentityMismatch) {
                        return Err(SendError::Client(err));
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(SendError::Client(last_err.unwrap_or(ClientError::Network("no address".into()))))
    }

    fn request_dto(&self, out: &Outgoing, is_ferry: bool, hashes: &std::collections::HashMap<String, String>) -> PrepareUploadRequest {
        let done = out.done.lock().unwrap().clone();
        let files: IndexMap<String, FileDto> = out
            .files
            .iter()
            .filter(|f| !done.contains(&f.id))
            .map(|f| {
                (
                    f.id.clone(),
                    FileDto {
                        id: f.id.clone(),
                        file_name: f.name.clone(),
                        size: f.size,
                        file_type: f.mime.clone(),
                        sha256: hashes.get(&f.id).cloned(),
                        preview: f.text.clone(),
                        metadata: f.modified.and_then(|m| {
                            let ts = time::OffsetDateTime::from(m).format(&time::format_description::well_known::Rfc3339).ok()?;
                            Some(FileMetadata { modified: Some(ts), accessed: None })
                        }),
                    },
                )
            })
            .collect();
        PrepareUploadRequest {
            info: self.shared.device_dto(),
            files,
            ferry: is_ferry.then(|| FerryPrepareRequest { transfer_id: out.transfer_id.clone() }),
        }
    }

    /// prepare-upload with PIN prompts and "receiver busy" patience.
    async fn prepare(
        &self,
        out: &Arc<Outgoing>,
        client: &PeerClient,
        is_ferry: bool,
        hashes: &std::collections::HashMap<String, String>,
        alias: &str,
    ) -> std::result::Result<Option<PrepareUploadResponse>, SendError> {
        let request = self.request_dto(out, is_ferry, hashes);
        let decision_timeout = Duration::from_secs(self.shared.settings.get().decision_timeout_secs.max(60) + 60);
        let mut pin: Option<String> = None;
        let busy_since = Instant::now();
        out.entry.set_state(TransferState::WaitingForAcceptance);
        loop {
            match client.prepare_upload(&request, pin.as_deref(), decision_timeout, &out.cancel).await {
                Ok(PrepareResult::Accepted(resp)) => {
                    if resp.files.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(resp));
                }
                Ok(PrepareResult::NothingAccepted) => return Ok(None),
                Err(ClientError::Status { status: 401, message, .. }) => {
                    let info =
                        if message.to_lowercase().contains("invalid") { ErrorInfo::pin_invalid() } else { ErrorInfo::pin_required() };
                    out.entry.set_error(Some(info));
                    out.entry.set_state(TransferState::PinRequired);
                    let (tx, rx) = oneshot::channel();
                    *out.pin_reply.lock().unwrap() = Some(tx);
                    let reply = tokio::select! {
                        r = rx => r.ok().flatten(),
                        _ = out.cancel.cancelled() => None,
                    };
                    match reply {
                        Some(p) => {
                            pin = Some(p);
                            out.entry.set_error(None);
                            out.entry.set_state(TransferState::WaitingForAcceptance);
                        }
                        None => return Err(SendError::User(ErrorInfo::cancelled())),
                    }
                }
                Err(ClientError::Status { status: 409, .. }) if busy_since.elapsed() < BUSY_RETRY_FOR => {
                    // LocalSend receivers handle one transfer at a time.
                    out.entry.set_error(Some(ErrorInfo::busy().with_hint(format!("Ferry will keep trying while {alias} is busy."))));
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                        _ = out.cancel.cancelled() => return Err(SendError::User(ErrorInfo::cancelled())),
                    }
                }
                Err(err) => return Err(SendError::Client(err)),
            }
        }
    }

    async fn upload_all(
        self: &Arc<Self>,
        out: &Arc<Outgoing>,
        client: &PeerClient,
        session: &PrepareUploadResponse,
        is_ferry: bool,
        precomputed: &std::collections::HashMap<String, String>,
        attempt: &CancellationToken,
    ) -> UploadOutcome {
        let parallel = self.shared.settings.get().parallel_files.max(1) as usize;
        let lane = Arc::new(Semaphore::new(parallel));
        let offsets = session.ferry.as_ref().map(|f| f.offsets.clone()).unwrap_or_default();
        let mut tasks = tokio::task::JoinSet::new();
        let (fatal_tx, mut fatal_rx) = mpsc::channel::<SendError>(1);
        let client = Arc::new(PeerClient::with_http(client.http_client(), client.addr.clone()));

        for file in out.files.iter() {
            let Some(token) = session.files.get(&file.id).cloned() else { continue };
            if out.done.lock().unwrap().contains(&file.id) {
                continue;
            }
            // Large files take half the lane so at most two run at once.
            let weight = if file.size < SMALL_FILE { 1 } else { (parallel as u32).div_ceil(2).max(1) };
            let permit = tokio::select! {
                p = lane.clone().acquire_many_owned(weight) => p.expect("semaphore closed"),
                _ = attempt.cancelled() => break,
                Some(err) = fatal_rx.recv() => { attempt.cancel(); return UploadOutcome::Fatal(err); }
            };
            let manager = self.clone();
            let out = out.clone();
            let client = client.clone();
            let file = file.clone();
            let session_id = session.session_id.clone();
            let offset = offsets.get(&file.id).copied().unwrap_or(0);
            let attempt = attempt.clone();
            let fatal_tx = fatal_tx.clone();
            let precomputed = precomputed.get(&file.id).cloned();
            tasks.spawn(async move {
                let _permit = permit;
                let _stream = manager.streams.clone().acquire_owned().await;
                let r = manager.upload_file(&out, &client, &session_id, &token, &file, offset, is_ferry, precomputed, &attempt).await;
                match r {
                    FileOutcome::Ok | FileOutcome::Skipped => true,
                    FileOutcome::Interrupted => false,
                    FileOutcome::Fatal(err) => {
                        let _ = fatal_tx.try_send(err);
                        attempt.cancel();
                        false
                    }
                }
            });
        }
        drop(fatal_tx);
        let mut all_ok = true;
        while let Some(r) = tasks.join_next().await {
            all_ok &= r.unwrap_or(false);
        }
        if let Ok(err) = fatal_rx.try_recv() {
            return UploadOutcome::Fatal(err);
        }
        if attempt.is_cancelled() || out.cancel.is_cancelled() || !all_ok {
            // Anything not done yet needs another attempt (or the user paused).
            let done = out.done.lock().unwrap().clone();
            let pending =
                out.files.iter().any(|f| session.files.contains_key(&f.id) && !done.contains(&f.id) && !self.file_failed(out, &f.id));
            if pending {
                return UploadOutcome::Interrupted;
            }
        }
        UploadOutcome::Finished
    }

    fn file_failed(&self, out: &Outgoing, id: &str) -> bool {
        out.entry.files().iter().any(|f| f.id == id && f.state == FileState::Failed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_file(
        &self,
        out: &Arc<Outgoing>,
        client: &PeerClient,
        session_id: &str,
        token: &str,
        file: &OutFile,
        mut offset: u64,
        is_ferry: bool,
        precomputed: Option<String>,
        attempt: &CancellationToken,
    ) -> FileOutcome {
        let entry = &out.entry;
        let alias = entry.peer().alias;
        loop {
            entry.set_file_state(&file.id, FileState::Transferring, None);
            entry.reset_file_progress(&file.id, offset);
            let progress = entry.file_progress(&file.id).unwrap_or_default();
            let (chunks, local_hash) = match (&file.path, &file.text) {
                // Small files: one read, hashed in memory (per-file overhead dominates).
                (Some(path), _) if offset == 0 && file.size < SMALL_FILE => {
                    let path = path.clone();
                    let read = tokio::task::spawn_blocking(move || {
                        use sha2::Digest;
                        let data = std::fs::read(&path)?;
                        let hash: [u8; 32] = sha2::Sha256::digest(&data).into();
                        Ok::<_, std::io::Error>((data, hash))
                    })
                    .await
                    .map_err(std::io::Error::other)
                    .and_then(|r| r);
                    let (tx, rx) = mpsc::channel(1);
                    let (htx, hrx) = oneshot::channel();
                    match read {
                        Ok((data, hash)) if data.len() as u64 == file.size => {
                            let _ = tx.try_send(Ok(Bytes::from(data)));
                            let _ = htx.send(Some(hash));
                        }
                        Ok(_) => {
                            let changed = std::io::Error::new(std::io::ErrorKind::InvalidData, "file changed size since it was selected");
                            let _ = tx.try_send(Err(changed));
                            let _ = htx.send(None);
                        }
                        Err(err) => {
                            let _ = tx.try_send(Err(err));
                            let _ = htx.send(None);
                        }
                    }
                    (rx, Some(hrx))
                }
                (Some(path), _) => {
                    let s = stream_file(path.clone(), offset, file.size);
                    (s.chunks, Some(s.sha256))
                }
                (None, Some(text)) => {
                    let (tx, rx) = mpsc::channel(1);
                    let _ = tx.try_send(Ok(Bytes::from(text.clone().into_bytes())));
                    (rx, None)
                }
                (None, None) => return FileOutcome::Skipped,
            };
            let send_offset = (is_ferry && offset > 0).then_some(offset);
            let result = client.upload(session_id, &file.id, token, send_offset, chunks, progress, attempt).await;

            match result {
                Ok(upload) => {
                    // Compare the receiver's hash with ours (read once while sending;
                    // resumed files are re-hashed locally from disk).
                    let ours = match local_hash {
                        Some(rx) => match rx.await.ok().flatten() {
                            Some(h) => Some(hex::encode(h)),
                            None => match &file.path {
                                Some(p) if upload.receiver_sha256.is_some() => {
                                    entry.set_file_state(&file.id, FileState::Verifying, None);
                                    hash_file_async(p.clone()).await.ok().map(hex::encode)
                                }
                                _ => None,
                            },
                        },
                        None => None,
                    };
                    let ours = ours.or(precomputed.clone());
                    let mismatch = matches!((&ours, &upload.receiver_sha256), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b));
                    if mismatch {
                        tracing::warn!("hash mismatch for {} to {alias}", file.name);
                        if is_ferry {
                            let _ = client.verify(session_id, &file.id, token, ours.as_deref().unwrap_or_default()).await;
                        }
                        if self.bump_attempt(out, &file.id) {
                            offset = 0;
                            continue;
                        }
                        entry.set_file_state(&file.id, FileState::Failed, Some(ErrorInfo::checksum_mismatch(&file.name)));
                        return FileOutcome::Skipped;
                    }
                    let verified = ours.is_some() && upload.receiver_sha256.is_some();
                    if is_ferry && verified && file.size >= SMALL_FILE {
                        let _ = client.verify(session_id, &file.id, token, ours.as_deref().unwrap_or_default()).await;
                    }
                    out.done.lock().unwrap().insert(file.id.clone());
                    entry.set_file_state(&file.id, FileState::Done, None);
                    entry.reset_file_progress(&file.id, file.size);
                    return FileOutcome::Ok;
                }
                Err(ClientError::Cancelled) => {
                    entry.set_file_state(&file.id, FileState::Pending, None);
                    return FileOutcome::Interrupted;
                }
                Err(ClientError::Source(err)) => {
                    entry.set_file_state(&file.id, FileState::Failed, Some(ErrorInfo::file_unreadable(&file.name, &err)));
                    return FileOutcome::Skipped;
                }
                Err(ClientError::Status { status: 416, offset: Some(o), .. }) if is_ferry => {
                    offset = o.min(file.size);
                    continue;
                }
                Err(ClientError::Status { status: 422, .. }) => {
                    if self.bump_attempt(out, &file.id) {
                        offset = 0;
                        continue;
                    }
                    entry.set_file_state(&file.id, FileState::Failed, Some(ErrorInfo::checksum_mismatch(&file.name)));
                    return FileOutcome::Skipped;
                }
                Err(ClientError::Status { status: 507, .. }) => {
                    return FileOutcome::Fatal(SendError::User(ErrorInfo::new(
                        "receiver_disk_full",
                        format!("{alias} doesn't have enough free space."),
                    )));
                }
                Err(ClientError::Status { status: 403, .. }) => {
                    // Session gone on the receiver (cancelled or expired).
                    entry.set_file_state(&file.id, FileState::Pending, None);
                    if is_ferry {
                        return FileOutcome::Interrupted;
                    }
                    return FileOutcome::Fatal(SendError::User(ErrorInfo::cancelled_by_peer()));
                }
                Err(ClientError::Status { status: 409, message, .. }) if message == "Cancelled" => {
                    // The receiver cancelled mid-upload; its /cancel notice may still be on the way.
                    entry.set_file_state(&file.id, FileState::Pending, None);
                    return FileOutcome::Fatal(SendError::User(ErrorInfo::cancelled_by_peer()));
                }
                Err(err) if err.is_connectivity() => {
                    entry.set_file_state(&file.id, FileState::Pending, None);
                    if !is_ferry && self.bump_attempt(out, &file.id) {
                        // LocalSend: the session usually survives a hiccup; retry from 0.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        offset = 0;
                        continue;
                    }
                    attempt.cancel();
                    return FileOutcome::Interrupted;
                }
                Err(err) => {
                    let info = describe_status(&err, &alias);
                    entry.set_file_state(&file.id, FileState::Failed, Some(info));
                    return FileOutcome::Skipped;
                }
            }
        }
    }

    fn bump_attempt(&self, out: &Outgoing, file_id: &str) -> bool {
        let mut attempts = out.attempts.lock().unwrap();
        let n = attempts.entry(file_id.to_string()).or_insert(0);
        *n += 1;
        *n < MAX_FILE_ATTEMPTS
    }

    /// Waits for the peer to come back (or the user to resume), then
    /// re-establishes the session with the same transfer id.
    async fn reconnect(
        &self,
        out: &Arc<Outgoing>,
        fingerprint: Option<String>,
        hashes: &std::collections::HashMap<String, String>,
        alias: &str,
    ) -> std::result::Result<(PeerClient, Option<PrepareUploadResponse>), SendError> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(500);
        loop {
            if *out.paused.lock().unwrap() {
                out.entry.set_state(TransferState::Paused);
                tokio::select! {
                    _ = out.resume.notified() => {}
                    _ = out.cancel.cancelled() => return Err(SendError::User(ErrorInfo::cancelled())),
                }
                continue;
            }
            out.entry.set_state(TransferState::Reconnecting);
            out.entry.set_error(Some(
                ErrorInfo::new("connection_lost", format!("Connection lost. Waiting for {alias}…"))
                    .with_hint("The transfer continues automatically from where it stopped."),
            ));
            self.lost_peer.notify_waiters();
            if let Ok((client, fp)) = self.connect(out).await
                && fp.is_some()
                && fp == fingerprint
            {
                match self.prepare(out, &client, true, hashes, alias).await {
                    Ok(session) => return Ok((client, session)),
                    Err(SendError::User(info)) if info.code == "cancelled" || info.code == "declined" => {
                        return Err(SendError::User(info));
                    }
                    Err(_) => {}
                }
            }
            if started.elapsed() > RECONNECT_WINDOW {
                return Err(SendError::User(
                    ErrorInfo::new("connection_lost", format!("{alias} didn't come back."))
                        .with_hint("Send again later. Ferry resumes from where it stopped for 24 hours."),
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = out.resume.notified() => {}
                _ = out.cancel.cancelled() => return Err(SendError::User(ErrorInfo::cancelled())),
            }
            delay = (delay * 2).min(Duration::from_secs(8));
        }
    }

    async fn precompute_hashes(&self, out: &Arc<Outgoing>) -> std::result::Result<std::collections::HashMap<String, String>, SendError> {
        let mut map = std::collections::HashMap::new();
        out.entry.set_state(TransferState::Preparing);
        for f in out.files.iter() {
            if out.cancel.is_cancelled() {
                return Err(SendError::User(ErrorInfo::cancelled()));
            }
            if let Some(path) = &f.path
                && let Ok(h) = hash_file_async(path.clone()).await
            {
                map.insert(f.id.clone(), hex::encode(h));
            }
        }
        Ok(map)
    }

    fn notify_cancel(&self, out: &Arc<Outgoing>, session: String) {
        let shared = self.shared.clone();
        let target = out.target.clone();
        let fingerprint = out.peer_fingerprint.lock().unwrap().clone();
        tokio::spawn(async move {
            let addrs = match &target {
                Target::Device { id } => shared.devices.channels(id),
                Target::Address { host, port, protocol, .. } => vec![PeerAddress { host: host.clone(), port: *port, protocol: *protocol }],
            };
            if let Some(addr) = addrs.into_iter().next()
                && let Ok(client) = PeerClient::new(&shared.identity, addr, fingerprint)
            {
                client.cancel(Some(&session)).await;
            }
        });
    }

    fn finish_history(&self, out: &Arc<Outgoing>) {
        if !self.shared.settings.get().history_enabled {
            return;
        }
        let peer = out.entry.peer();
        let files = out.entry.files();
        let state = out.entry.state();
        for (file, src) in files.iter().zip(out.files.iter()) {
            let status = match file.state {
                FileState::Done => HistoryStatus::Completed,
                FileState::Skipped => continue,
                _ if state == TransferState::Cancelled => HistoryStatus::Cancelled,
                _ => HistoryStatus::Failed,
            };
            let is_text = src.text.is_some();
            let entry = NewHistoryEntry {
                transfer_id: out.id.clone(),
                direction: Direction::Send,
                peer_id: peer.id.clone(),
                peer_alias: peer.alias.clone(),
                peer_kind: peer.device_kind,
                kind: if is_text { HistoryKind::Text } else { HistoryKind::File },
                name: if is_text {
                    src.text.as_deref().map(|t| t.chars().take(80).collect()).unwrap_or_default()
                } else {
                    src.name.clone()
                },
                size: src.size,
                mime: src.mime.clone(),
                path: src.path.as_ref().map(|p| p.display().to_string()),
                text: None,
                timestamp_ms: now_ms(),
                status,
                verified: false,
            };
            if let Ok(e) = self.shared.db.add_history(&entry) {
                self.shared.events.emit(EngineEvent::HistoryAdded { entry: e });
            }
        }
    }
}

fn set_connection(entry: &TransferEntry, addr: &PeerAddress) {
    entry.set_connection(ConnectionInfo {
        transport: "lan".into(),
        encrypted: addr.protocol == Protocol::Https,
        ip_version: Some(addr.ip_version()),
        relayed: false,
        address: Some(addr.display()),
    });
}

enum UploadOutcome {
    Finished,
    Interrupted,
    Fatal(SendError),
}

enum FileOutcome {
    Ok,
    Skipped,
    Interrupted,
    Fatal(SendError),
}

#[derive(Debug)]
enum SendError {
    Client(ClientError),
    User(ErrorInfo),
}

impl From<crate::FerryError> for SendError {
    fn from(value: crate::FerryError) -> Self {
        SendError::User(value.info())
    }
}

/// Expands the selection into files with relative names (folders keep their
/// structure: `Album/2026/a.jpg`). Symlinks are not followed.
pub fn build_manifest(paths: &[PathBuf]) -> Result<Vec<OutFile>> {
    let mut out = Vec::new();
    let mut names = HashSet::new();
    for path in paths {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| ErrorInfo::file_unreadable(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), &e))?;
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into());
        if meta.is_dir() {
            let mut stack = vec![(path.clone(), base.clone())];
            while let Some((dir, rel)) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                let mut entries: Vec<_> = entries.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries.into_iter().rev() {
                    let Ok(ft) = e.file_type() else { continue };
                    let name = e.file_name().to_string_lossy().into_owned();
                    let child_rel = format!("{rel}/{name}");
                    if ft.is_dir() {
                        stack.push((e.path(), child_rel));
                    } else if ft.is_file() {
                        push_file(&mut out, &mut names, e.path(), child_rel)?;
                    }
                }
                if out.len() > MAX_MANIFEST_FILES {
                    return Err(ErrorInfo::new("too_many_files", format!("Too many files (more than {MAX_MANIFEST_FILES}).")).into());
                }
            }
        } else if meta.is_file() {
            push_file(&mut out, &mut names, path.clone(), base)?;
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn push_file(out: &mut Vec<OutFile>, names: &mut HashSet<String>, path: PathBuf, mut rel: String) -> Result<()> {
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!("skipping unreadable {}: {err}", path.display());
            return Ok(());
        }
    };
    // Two selected files with the same name: keep both.
    if !names.insert(rel.clone()) {
        let mut n = 2;
        loop {
            let candidate = crate::fsutil::unique::candidate(&rel, n);
            if names.insert(candidate.clone()) {
                rel = candidate;
                break;
            }
            n += 1;
        }
    }
    out.push(OutFile {
        id: format!("f{}", out.len()),
        mime: crate::util::mime_for(&rel),
        name: rel,
        path: Some(path),
        size: meta.len(),
        modified: meta.modified().ok(),
        text: None,
    });
    Ok(())
}
