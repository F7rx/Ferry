//! Receiving: request policy, sessions, and the upload path.
//!
//! Differences from upstream LocalSend that senders can observe:
//! - several sessions may run at once (per-peer and global caps instead of a
//!   single slot that answers everyone else with 409);
//! - Ferry senders can resume (`ferry.transferId` + `offset`) and verify;
//! - upload responses carry the receiver's SHA-256 (ignored by LocalSend).
// Early exits return the HTTP response itself; it is built once, rarely.
#![allow(clippy::result_large_err)]

use crate::db::{InboundFileRecord, InboundRecord, NewHistoryEntry};
use crate::error::ErrorInfo;
use crate::events::{EngineEvent, NoticeLevel};
use crate::fsutil::part::{self, DEFAULT_CHECKPOINT_BYTES, OpenError, PartSpec, PartWriter};
use crate::fsutil::sanitize::{SafeRelativePath, sanitize_relative_path};
use crate::fsutil::{PART_SUFFIX, motw, space, unique};
use crate::model::*;
use crate::net::limits::{Attempt, FailureTracker, RateLimiter, peer_key};
use crate::proto::*;
use crate::server::{self, PeerContext, Resp, Routes, empty, error, json};
use crate::settings::AutoAccept;
use crate::shared::Shared;
use crate::transfer::{NewTransfer, TransferEntry};
use crate::util::{now_ms, random_token, secret_eq};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper::body::Incoming;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const MAX_FILES: usize = 20_000;
/// Files up to this size take the in-memory fast path.
const SMALL_FILE: u64 = 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const MAX_FILE_SIZE: u64 = 1 << 50;
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PENDING_PER_PEER: usize = 3;
const MAX_PENDING: usize = 16;
const MAX_SESSIONS_PER_PEER: usize = 8;
const MAX_SESSIONS: usize = 32;
const MAX_ATTEMPTS: u32 = 3;
/// Files at least this big are fsync'd before being renamed into place.
const SYNC_THRESHOLD: u64 = 8 * 1024 * 1024;
/// Idle non-resumable sessions are failed after this long.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Idle resumable sessions are dropped from memory (kept in the db) after this.
const RESUMABLE_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Resumable partial transfers are deleted after this long without activity.
pub const RESUME_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Finished sessions linger so late `verify` calls still find them.
const FINISHED_LINGER: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InState {
    Pending,
    Active(u64),
    Done,
    Failed,
}

struct InFile {
    token: String,
    rel: SafeRelativePath,
    size: u64,
    mime: String,
    expected_sha256: Option<String>,
    metadata: Option<FileMetadata>,
    dest_dir: PathBuf,
    part_path: PathBuf,
    state: InState,
    active_cancel: Option<CancellationToken>,
    /// Bytes confirmed on disk (resumable sessions).
    offset: u64,
    attempts: u32,
    final_path: Option<PathBuf>,
    received_sha256: Option<String>,
    history_id: Option<i64>,
}

struct Session {
    id: String,
    peer: PeerContext,
    peer_dto: DeviceDto,
    peer_ref: PeerRef,
    /// Set when the sender is a verified Ferry device: the transfer is resumable.
    ferry_transfer_id: Option<String>,
    save_dir: PathBuf,
    cancel: CancellationToken,
    entry: Arc<TransferEntry>,
    files: Mutex<IndexMap<String, InFile>>,
    last_activity: Mutex<Instant>,
    finished_at: Mutex<Option<Instant>>,
    generation: std::sync::atomic::AtomicU64,
    released: tokio::sync::Notify,
    /// Folders already created and checked to be inside the save folder.
    checked_dirs: Mutex<HashSet<PathBuf>>,
}

impl Session {
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    fn peer_matches(&self, peer: &PeerContext) -> bool {
        match (&self.peer.identity, &peer.identity) {
            (PeerIdentity::Verified { fingerprint: a }, PeerIdentity::Verified { fingerprint: b }) => a == b,
            // Without a proven identity, the session is bound to the address.
            (a, b) if a == b => self.peer.ip.ip == peer.ip.ip,
            _ => false,
        }
    }

    fn resumable(&self) -> bool {
        self.ferry_transfer_id.is_some()
    }
}

struct Pending {
    decision: Option<oneshot::Sender<Decision>>,
    /// IP group (IPv4 address or IPv6 /64), for per-peer caps.
    key: IpAddr,
    /// Exact address of an unverified sender (verified ones match by fingerprint).
    ip: IpAddr,
    fingerprint: Option<String>,
}

pub struct ReceiveManager {
    shared: Arc<Shared>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    pending: Mutex<HashMap<String, Pending>>,
    pin_failures: FailureTracker,
    prepare_rate: RateLimiter,
}

/// Removes a pending request when the handler ends: answered, timed out, or
/// dropped because the sender disconnected.
struct PendingGuard<'a> {
    manager: &'a ReceiveManager,
    id: String,
    reason: &'static str,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.manager.pending.lock().unwrap().remove(&self.id).is_some() {
            self.manager.shared.events.emit(EngineEvent::IncomingRequestClosed { id: self.id.clone(), reason: self.reason.to_string() });
        }
    }
}

struct FileSpec {
    id: String,
    rel: SafeRelativePath,
    size: u64,
    mime: String,
    sha256: Option<String>,
    preview: Option<String>,
    metadata: Option<FileMetadata>,
}

impl ReceiveManager {
    pub fn new(shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            pin_failures: FailureTracker::new(5, 50, Duration::from_secs(5 * 60)),
            prepare_rate: RateLimiter::new(2.0, 10.0),
        })
    }

    // ── prepare-upload ───────────────────────────────────────────────────

    pub async fn prepare_upload(&self, peer: &PeerContext, query: &HashMap<String, String>, body: Incoming, routes: &Routes) -> Resp {
        // Checked before the body is read: a missing PIN costs no parsing.
        if let Err(resp) = self.admit(peer, query.get("pin").map(String::as_str)) {
            return resp;
        }
        let request: PrepareUploadRequest = match server::read_json(peer, body, server::PREPARE_JSON_LIMIT, routes).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        self.prepare_parsed(peer, request).await
    }

    /// Rate limit, receiving switch and PIN, shared by every way in
    /// (LocalSend API, browser link).
    pub fn admit(&self, peer: &PeerContext, pin_given: Option<&str>) -> Result<(), Resp> {
        self.admit_with(peer, Some(pin_given))
    }

    /// Admission for a browser link: the link token (and the link's own PIN,
    /// checked by the link server) stand in for the device PIN. Rate limit
    /// and the receiving switch still apply, and every upload is still asked.
    pub fn admit_link(&self, peer: &PeerContext) -> Result<(), Resp> {
        self.admit_with(peer, None)
    }

    /// `pin`: None skips the device PIN; Some(given) checks it.
    fn admit_with(&self, peer: &PeerContext, pin: Option<Option<&str>>) -> Result<(), Resp> {
        let settings = self.shared.settings.get();
        if !self.prepare_rate.check(peer.ip.ip) {
            return Err(error(StatusCode::TOO_MANY_REQUESTS, "Too many requests"));
        }
        if !settings.receive_enabled {
            return Err(error(StatusCode::FORBIDDEN, "Receiving is turned off"));
        }
        let trust = peer.identity.fingerprint().map(|fp| self.shared.devices.trust(fp)).unwrap_or_default();
        // Trusted (verified) devices skip the PIN; everyone else must know it.
        if let (Some(pin), Some(pin_given)) = (settings.pin.as_deref(), pin)
            && !trust.trusted
        {
            if self.pin_failures.check(peer.ip.ip) == Attempt::LockedOut {
                return Err(error(StatusCode::TOO_MANY_REQUESTS, "Too many attempts"));
            }
            match pin_given {
                None => return Err(error(StatusCode::UNAUTHORIZED, "PIN required")),
                Some(given) if !secret_eq(given, pin) => {
                    self.pin_failures.record_failure(peer.ip.ip);
                    return Err(error(StatusCode::UNAUTHORIZED, "Invalid PIN"));
                }
                Some(_) => self.pin_failures.record_success(peer.ip.ip),
            }
        }
        Ok(())
    }

    /// Everything after admission: validation, policy, the user's decision,
    /// and session creation.
    pub async fn prepare_parsed(&self, peer: &PeerContext, request: PrepareUploadRequest) -> Resp {
        let settings = self.shared.settings.get();
        let trust = peer.identity.fingerprint().map(|fp| self.shared.devices.trust(fp)).unwrap_or_default();
        if request.info.validate().is_err() || request.files.is_empty() || request.files.len() > MAX_FILES {
            return error(StatusCode::BAD_REQUEST, "Invalid body");
        }
        let specs = match validate_files(&request.files) {
            Ok(specs) => specs,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        let peer_ref = self.shared.peer_ref(&peer.identity, &request.info, &format!("ip:{}", peer.ip));

        // A lone text file with a preview is a message (LocalSend semantics).
        if specs.len() == 1 && specs[0].mime.starts_with("text/") && specs[0].preview.is_some() {
            return self.receive_message(peer, &peer_ref, &specs[0], trust.trusted);
        }

        // Resumption of a known transfer from the same verified device.
        if let (Some(fp), Some(ext)) = (peer.identity.fingerprint(), request.ferry.as_ref())
            && ext.transfer_id.len() <= 64
            && let Some(resp) = self.try_resume(peer, fp, &ext.transfer_id, &request, &peer_ref).await
        {
            return resp;
        }

        if let Err(resp) = self.check_capacity(peer) {
            return resp;
        }

        let save_dir = settings.save_dir();
        let total: u64 = specs.iter().map(|s| s.size).sum();
        if let Err(resp) = self.check_space(&save_dir, total, &peer_ref.alias) {
            return resp;
        }

        let auto = peer.identity.is_verified()
            && match settings.auto_accept {
                AutoAccept::Off => false,
                AutoAccept::MyDevices => trust.mine,
                AutoAccept::Trusted => trust.trusted,
            };

        let decision = if auto {
            Decision::accept_all()
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(
                id.clone(),
                Pending {
                    decision: Some(tx),
                    key: peer_key(peer.ip.ip),
                    ip: peer.ip.ip,
                    fingerprint: peer.identity.fingerprint().map(str::to_string),
                },
            );
            let mut guard = PendingGuard { manager: self, id: id.clone(), reason: "cancelled" };
            let timeout = Duration::from_secs(settings.decision_timeout_secs.clamp(10, 3600));
            self.shared.events.emit(EngineEvent::IncomingRequest {
                request: IncomingRequest {
                    id: id.clone(),
                    peer: peer_ref.clone(),
                    files: specs
                        .iter()
                        .map(|s| IncomingFile { id: s.id.clone(), name: s.rel.display(), size: s.size, mime: s.mime.clone() })
                        .collect(),
                    total_bytes: total,
                    text: None,
                    received_at_ms: now_ms(),
                    trusted: trust.trusted,
                    default_save_dir: save_dir.display().to_string(),
                    expires_at_ms: now_ms() + timeout.as_millis() as u64,
                },
            });
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(decision)) => {
                    guard.reason = "answered";
                    decision
                }
                Ok(Err(_)) => {
                    guard.reason = "cancelled";
                    return error(StatusCode::FORBIDDEN, "Rejected");
                }
                Err(_) => {
                    guard.reason = "expired";
                    return error(StatusCode::FORBIDDEN, "No answer");
                }
            }
        };

        if decision.decline {
            return error(StatusCode::FORBIDDEN, "Rejected");
        }
        let accepted: HashSet<String> = match &decision.accept {
            None => specs.iter().map(|s| s.id.clone()).collect(),
            Some(ids) => ids.iter().filter(|id| specs.iter().any(|s| &s.id == *id)).cloned().collect(),
        };
        if accepted.is_empty() {
            return empty(StatusCode::NO_CONTENT);
        }
        if decision.trust
            && let Some(fp) = peer.identity.fingerprint()
        {
            let _ = self.shared.devices.update_flags(fp, Some(true), None, None, None);
        }
        let save_dir = decision.save_dir.clone().unwrap_or(save_dir);
        let resumable_id = match (peer.identity.fingerprint(), request.ferry.as_ref()) {
            (Some(_), Some(ext)) if ext.transfer_id.len() <= 64 => Some(ext.transfer_id.clone()),
            _ => None,
        };
        match self.create_session(peer, &request.info, peer_ref, &specs, &accepted, save_dir, resumable_id) {
            Ok(session) => {
                let tokens: IndexMap<String, String> =
                    session.files.lock().unwrap().iter().map(|(id, f)| (id.clone(), f.token.clone())).collect();
                let ferry = session.resumable().then(|| FerryPrepareResponse { resumable: true, offsets: HashMap::new() });
                json(StatusCode::OK, &PrepareUploadResponse { session_id: session.id.clone(), files: tokens, ferry })
            }
            Err(err) => {
                tracing::error!("could not create session: {err}");
                self.shared.events.emit(EngineEvent::Notice {
                    level: NoticeLevel::Error,
                    code: "save_folder".into(),
                    message: format!("Couldn't prepare the save folder: {err}"),
                });
                error(StatusCode::INTERNAL_SERVER_ERROR, "Could not prepare the save folder")
            }
        }
    }

    fn receive_message(&self, peer: &PeerContext, peer_ref: &PeerRef, spec: &FileSpec, trusted: bool) -> Resp {
        let text = spec.preview.clone().unwrap_or_default();
        let settings = self.shared.settings.get();
        let id = uuid::Uuid::new_v4().to_string();
        self.shared.events.emit(EngineEvent::IncomingRequest {
            request: IncomingRequest {
                id: id.clone(),
                peer: peer_ref.clone(),
                files: Vec::new(),
                total_bytes: 0,
                text: Some(text.clone()),
                received_at_ms: now_ms(),
                trusted,
                default_save_dir: String::new(),
                expires_at_ms: now_ms(),
            },
        });
        if settings.history_enabled {
            let entry = NewHistoryEntry {
                transfer_id: id,
                direction: Direction::Receive,
                peer_id: peer_ref.id.clone(),
                peer_alias: peer_ref.alias.clone(),
                peer_kind: peer_ref.device_kind,
                kind: HistoryKind::Text,
                name: summarize_text(&text),
                size: text.len() as u64,
                mime: spec.mime.clone(),
                path: None,
                text: settings.keep_message_text.then_some(text),
                timestamp_ms: now_ms(),
                status: HistoryStatus::Completed,
                verified: peer.identity.is_verified(),
            };
            if let Ok(entry) = self.shared.db.add_history(&entry) {
                self.shared.events.emit(EngineEvent::HistoryAdded { entry });
            }
        }
        empty(StatusCode::NO_CONTENT)
    }

    fn check_capacity(&self, peer: &PeerContext) -> Result<(), Resp> {
        let key = peer_key(peer.ip.ip);
        let pending = self.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING || pending.values().filter(|p| p.key == key).count() >= MAX_PENDING_PER_PEER {
            return Err(error(StatusCode::CONFLICT, "Blocked by another session"));
        }
        drop(pending);
        let sessions = self.sessions.lock().unwrap();
        let live = sessions.values().filter(|s| s.finished_at.lock().unwrap().is_none());
        let (mut total, mut mine) = (0, 0);
        for s in live {
            total += 1;
            if peer_key(s.peer.ip.ip) == key {
                mine += 1;
            }
        }
        if total >= MAX_SESSIONS || mine >= MAX_SESSIONS_PER_PEER {
            return Err(error(StatusCode::CONFLICT, "Blocked by another session"));
        }
        Ok(())
    }

    fn check_space(&self, dir: &Path, needed: u64, alias: &str) -> Result<(), Resp> {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(available) = space::available_space(dir) {
            // Keep a little headroom so the disk isn't filled to the last byte.
            if needed.saturating_add(64 * 1024 * 1024) > available {
                let info = ErrorInfo::disk_full(needed, available);
                self.shared.events.emit(EngineEvent::Notice {
                    level: NoticeLevel::Warning,
                    code: "disk_full".into(),
                    message: format!("Declined a transfer from {alias}: {}", info.message),
                });
                return Err(error(StatusCode::INSUFFICIENT_STORAGE, "Not enough storage"));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn create_session(
        &self,
        peer: &PeerContext,
        info: &DeviceDto,
        peer_ref: PeerRef,
        specs: &[FileSpec],
        accepted: &HashSet<String>,
        save_dir: PathBuf,
        ferry_transfer_id: Option<String>,
    ) -> std::io::Result<Arc<Session>> {
        std::fs::create_dir_all(&save_dir)?;
        // Each transfer's top-level folders get their own (unique) directory,
        // so two transfers of "Album" never mix.
        let mut folder_map: HashMap<String, PathBuf> = HashMap::new();
        let mut files = IndexMap::new();
        for spec in specs.iter().filter(|s| accepted.contains(&s.id)) {
            let dest_dir = match spec.rel.parents().split_first() {
                None => save_dir.clone(),
                Some((top, rest)) => {
                    let top_dir = match folder_map.get(top) {
                        Some(dir) => dir.clone(),
                        None => {
                            let dir = unique::create_unique_dir(&save_dir, top)?;
                            folder_map.insert(top.clone(), dir.clone());
                            dir
                        }
                    };
                    rest.iter().fold(top_dir, |p, c| p.join(c))
                }
            };
            let part_name = format!("{}.{}{PART_SUFFIX}", spec.rel.file_name(), &random_token()[..6]);
            files.insert(
                spec.id.clone(),
                InFile {
                    token: random_token(),
                    rel: spec.rel.clone(),
                    size: spec.size,
                    mime: spec.mime.clone(),
                    expected_sha256: spec.sha256.clone(),
                    metadata: spec.metadata.clone(),
                    part_path: dest_dir.join(part_name),
                    dest_dir,
                    state: InState::Pending,
                    active_cancel: None,
                    offset: 0,
                    attempts: 0,
                    final_path: None,
                    received_sha256: None,
                    history_id: None,
                },
            );
        }
        let save_root = if folder_map.len() == 1 && specs.iter().filter(|s| accepted.contains(&s.id)).all(|s| !s.rel.parents().is_empty()) {
            folder_map.values().next().cloned()
        } else {
            Some(save_dir.clone())
        };
        Ok(self.register_session(peer, info, peer_ref, files, save_dir, save_root, ferry_transfer_id, None))
    }

    #[allow(clippy::too_many_arguments)]
    fn register_session(
        &self,
        peer: &PeerContext,
        info: &DeviceDto,
        peer_ref: PeerRef,
        files: IndexMap<String, InFile>,
        save_dir: PathBuf,
        save_root: Option<PathBuf>,
        ferry_transfer_id: Option<String>,
        created_ms: Option<u64>,
    ) -> Arc<Session> {
        let id = uuid::Uuid::new_v4().to_string();
        let transfer_files: Vec<TransferFile> = files
            .iter()
            .map(|(fid, f)| TransferFile {
                id: fid.clone(),
                name: f.rel.display(),
                size: f.size,
                mime: f.mime.clone(),
                state: if f.state == InState::Done { FileState::Done } else { FileState::Pending },
                bytes_done: if f.state == InState::Done { f.size } else { f.offset },
                error: None,
                path: f.final_path.as_ref().map(|p| p.display().to_string()),
            })
            .collect();
        let entry = self.shared.transfers.create(NewTransfer {
            id: id.clone(),
            direction: Direction::Receive,
            drop_id: None,
            peer: peer_ref.clone(),
            files: transfer_files,
            state: TransferState::Transferring,
            resumable: ferry_transfer_id.is_some(),
            text: None,
            save_dir: save_root.map(|p| p.display().to_string()),
            connection: Some(ConnectionInfo {
                transport: "lan".into(),
                encrypted: peer.identity != PeerIdentity::PlainHttp,
                ip_version: Some(if peer.ip.ip.is_ipv6() { 6 } else { 4 }),
                relayed: false,
                address: Some(peer.ip.to_string()),
            }),
        });
        let session = Arc::new(Session {
            id: id.clone(),
            peer: peer.clone(),
            peer_dto: info.clone(),
            peer_ref,
            ferry_transfer_id: ferry_transfer_id.clone(),
            save_dir,
            cancel: CancellationToken::new(),
            entry,
            files: Mutex::new(files),
            last_activity: Mutex::new(Instant::now()),
            finished_at: Mutex::new(None),
            generation: Default::default(),
            released: tokio::sync::Notify::new(),
            checked_dirs: Mutex::new(HashSet::new()),
        });
        if let (Some(fp), Some(tid)) = (peer.identity.fingerprint(), ferry_transfer_id) {
            let record = InboundRecord {
                peer_fingerprint: fp.to_string(),
                transfer_id: tid,
                peer_alias: session.peer_ref.alias.clone(),
                created_ms: created_ms.unwrap_or_else(now_ms),
                updated_ms: now_ms(),
                files: session
                    .files
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(fid, f)| InboundFileRecord {
                        file_id: fid.clone(),
                        rel_name: f.rel.display(),
                        size: f.size,
                        mime: f.mime.clone(),
                        part_path: f.part_path.display().to_string(),
                        final_path: f.final_path.as_ref().map(|p| p.display().to_string()),
                        offset: f.offset,
                        done: f.state == InState::Done,
                    })
                    .collect(),
            };
            if let Err(err) = self.shared.db.save_inbound(&record) {
                tracing::warn!("could not persist resumable transfer: {err}");
            }
        }
        self.sessions.lock().unwrap().insert(id, session.clone());
        session
    }

    /// Answers a reconnecting Ferry sender without prompting again.
    async fn try_resume(
        &self,
        peer: &PeerContext,
        fingerprint: &str,
        transfer_id: &str,
        request: &PrepareUploadRequest,
        peer_ref: &PeerRef,
    ) -> Option<Resp> {
        // Still in memory (the receiver didn't restart)?
        let existing = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .find(|s| s.ferry_transfer_id.as_deref() == Some(transfer_id) && s.peer.identity.fingerprint() == Some(fingerprint))
            .cloned();
        let session = match existing {
            Some(session) => session,
            None => {
                let record = self.shared.db.load_inbound(fingerprint, transfer_id).ok()??;
                let mut files = IndexMap::new();
                for f in &record.files {
                    let Ok(rel) = sanitize_relative_path(&f.rel_name) else { continue };
                    let part_path = PathBuf::from(&f.part_path);
                    let on_disk = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
                    let Some(dest_dir) = part_path.parent().map(Path::to_path_buf) else { continue };
                    let metadata = request.files.get(&f.file_id).and_then(|d| d.metadata.clone());
                    files.insert(
                        f.file_id.clone(),
                        InFile {
                            token: random_token(),
                            rel,
                            size: f.size,
                            mime: f.mime.clone(),
                            expected_sha256: None,
                            metadata,
                            dest_dir,
                            part_path,
                            state: if f.done { InState::Done } else { InState::Pending },
                            active_cancel: None,
                            // Never trust more than what is actually on disk.
                            offset: f.offset.min(on_disk),
                            attempts: 0,
                            final_path: f.final_path.as_ref().map(PathBuf::from),
                            received_sha256: None,
                            history_id: None,
                        },
                    );
                }
                let save_dir = self.shared.settings.get().save_dir();
                tracing::info!("Resuming transfer {transfer_id} from {}", peer_ref.alias);
                self.register_session(
                    peer,
                    &request.info,
                    peer_ref.clone(),
                    files,
                    save_dir.clone(),
                    Some(save_dir),
                    Some(transfer_id.to_string()),
                    Some(record.created_ms),
                )
            }
        };
        // Uploads from the old connection may not have noticed it died yet.
        // Stop them and wait until their data is flushed, so the offsets we
        // report are what is really on disk (a stale 0 would discard it).
        for _ in 0..50 {
            let active: Vec<CancellationToken> = session
                .files
                .lock()
                .unwrap()
                .values()
                .filter(|f| matches!(f.state, InState::Active(_)))
                .filter_map(|f| f.active_cancel.clone())
                .collect();
            if active.is_empty() {
                break;
            }
            active.iter().for_each(CancellationToken::cancel);
            let _ = tokio::time::timeout(Duration::from_millis(200), session.released.notified()).await;
        }
        session.touch();
        session.entry.set_state(TransferState::Transferring);
        let files = session.files.lock().unwrap();
        let tokens: IndexMap<String, String> =
            files.iter().filter(|(_, f)| f.state != InState::Done).map(|(id, f)| (id.clone(), f.token.clone())).collect();
        let offsets: HashMap<String, u64> =
            files.iter().filter(|(_, f)| f.state != InState::Done).map(|(id, f)| (id.clone(), f.offset)).collect();
        drop(files);
        if tokens.is_empty() {
            return Some(empty(StatusCode::NO_CONTENT));
        }
        Some(json(
            StatusCode::OK,
            &PrepareUploadResponse {
                session_id: session.id.clone(),
                files: tokens,
                ferry: Some(FerryPrepareResponse { resumable: true, offsets }),
            },
        ))
    }

    // ── upload ───────────────────────────────────────────────────────────

    pub async fn upload(&self, peer: &PeerContext, query: &HashMap<String, String>, body: Incoming) -> Resp {
        let (Some(session_id), Some(file_id), Some(token)) = (query.get("sessionId"), query.get("fileId"), query.get("token")) else {
            return error(StatusCode::BAD_REQUEST, "Missing parameters");
        };
        let Some(session) = self.sessions.lock().unwrap().get(session_id).cloned() else {
            return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
        };
        if !session.peer_matches(peer) || session.cancel.is_cancelled() {
            return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
        }
        let requested_offset = match query.get("offset").map(|o| o.parse::<u64>()) {
            None => None,
            Some(Ok(o)) if session.resumable() => Some(o),
            Some(_) => return error(StatusCode::BAD_REQUEST, "Invalid offset"),
        };

        // Claim the file, superseding a stale upload from the same device
        // (its connection may still look alive after a network change).
        let claim = match self.claim_file(&session, file_id, token, requested_offset).await {
            Ok(claim) => claim,
            Err(resp) => return resp,
        };
        session.touch();
        let result = self.receive_file(&session, file_id, &claim, body).await;
        session.released.notify_waiters();
        result
    }

    async fn claim_file(&self, session: &Arc<Session>, file_id: &str, token: &str, offset: Option<u64>) -> Result<Claim, Resp> {
        for _ in 0..50 {
            let waiting = {
                let mut files = session.files.lock().unwrap();
                let Some(file) = files.get_mut(file_id) else {
                    return Err(error(StatusCode::FORBIDDEN, "Invalid token or IP address"));
                };
                if !secret_eq(&file.token, token) {
                    return Err(error(StatusCode::FORBIDDEN, "Invalid token or IP address"));
                }
                match file.state {
                    InState::Done => return Err(error(StatusCode::FORBIDDEN, "File already received")),
                    InState::Failed if file.attempts >= MAX_ATTEMPTS => return Err(error(StatusCode::FORBIDDEN, "File failed")),
                    InState::Active(_) => {
                        if let Some(cancel) = &file.active_cancel {
                            cancel.cancel();
                        }
                        true
                    }
                    InState::Pending | InState::Failed => {
                        let start = match offset {
                            Some(o) if o > file.offset => {
                                return Err(json(
                                    StatusCode::RANGE_NOT_SATISFIABLE,
                                    &ErrorBody { message: "offset mismatch".into(), offset: Some(file.offset) },
                                ));
                            }
                            Some(o) => o,
                            None => 0,
                        };
                        let generation = session.generation.fetch_add(1, Ordering::Relaxed) + 1;
                        let cancel = session.cancel.child_token();
                        file.state = InState::Active(generation);
                        file.active_cancel = Some(cancel.clone());
                        return Ok(Claim {
                            generation,
                            cancel,
                            start,
                            size: file.size,
                            part_path: file.part_path.clone(),
                            dest_dir: file.dest_dir.clone(),
                            name: file.rel.file_name().to_string(),
                            discard_existing: start == 0 && (file.offset > 0 || file.part_path.exists()),
                        });
                    }
                }
            };
            if waiting {
                let _ = tokio::time::timeout(Duration::from_millis(200), session.released.notified()).await;
            }
        }
        Err(error(StatusCode::CONFLICT, "File is busy"))
    }

    async fn receive_file(&self, session: &Arc<Session>, file_id: &str, claim: &Claim, mut body: Incoming) -> Resp {
        let entry = session.entry.clone();
        let alias = session.peer_ref.alias.clone();

        if claim.discard_existing {
            let _ = tokio::fs::remove_file(&claim.part_path).await;
        }
        if let Err(resp) = self.prepare_dest(session, &claim.dest_dir).await {
            self.release(session, file_id, claim.generation, ReleaseAs::Pending { offset: claim.start }, None);
            return resp;
        }
        if claim.start == 0 && claim.size <= SMALL_FILE {
            return self.receive_small(session, file_id, claim, body).await;
        }
        if let Ok(available) = space::available_space(&claim.dest_dir)
            && claim.size - claim.start > available
        {
            let err = ErrorInfo::disk_full(claim.size - claim.start, available);
            self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(err));
            return error(StatusCode::INSUFFICIENT_STORAGE, "Not enough storage");
        }

        let resumable = session.resumable();
        let mut writer = match PartWriter::start(PartSpec {
            path: claim.part_path.clone(),
            offset: claim.start,
            expected_len: claim.size,
            checkpoint_every: resumable.then_some(DEFAULT_CHECKPOINT_BYTES),
        })
        .await
        {
            Ok(w) => w,
            Err(OpenError::Short { available }) => {
                self.release(session, file_id, claim.generation, ReleaseAs::Pending { offset: available }, None);
                return json(StatusCode::RANGE_NOT_SATISFIABLE, &ErrorBody { message: "offset mismatch".into(), offset: Some(available) });
            }
            Err(OpenError::Io(err)) => {
                tracing::error!("cannot open {}: {err}", claim.part_path.display());
                self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(ErrorInfo::internal(&err)));
                return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not write file");
            }
        };

        entry.set_file_state(file_id, FileState::Transferring, None);
        entry.reset_file_progress(file_id, claim.start);
        let progress = entry.file_progress(file_id);

        // Persist durable checkpoints so a crash resumes from disk truth.
        let checkpoint_task = resumable.then(|| {
            let mut rx = writer.checkpoints();
            let db = self.shared.db.clone();
            let session = session.clone();
            let file_id = file_id.to_string();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let offset = *rx.borrow();
                    if let Some(f) = session.files.lock().unwrap().get_mut(&file_id) {
                        f.offset = offset;
                    }
                    if let (Some(fp), Some(tid)) = (session.peer.identity.fingerprint(), &session.ferry_transfer_id) {
                        let _ = db.update_inbound_file(fp, tid, &file_id, offset, false, None);
                    }
                }
            })
        });

        let mut failure: Option<(StatusCode, &'static str)> = None;
        loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(BODY_IDLE_TIMEOUT, body.frame()) => frame,
                _ = claim.cancel.cancelled() => { failure = Some((StatusCode::CONFLICT, "Cancelled")); break; }
            };
            match frame {
                Err(_) => {
                    failure = Some((StatusCode::REQUEST_TIMEOUT, "Connection stalled"));
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(_))) => {
                    failure = Some((StatusCode::BAD_REQUEST, "Connection lost"));
                    break;
                }
                Ok(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else { continue };
                    if data.is_empty() {
                        continue;
                    }
                    let n = data.len() as u64;
                    if writer.write(data).await.is_err() {
                        failure = Some((StatusCode::BAD_REQUEST, "Write failed"));
                        break;
                    }
                    if let Some(p) = &progress {
                        p.fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
        }
        drop(body);
        let outcome = writer.finish().await;
        if let Some(task) = checkpoint_task {
            let _ = task.await;
        }
        session.touch();

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                // Overrun or disk error: the part file is unusable.
                let _ = tokio::fs::remove_file(&claim.part_path).await;
                let (status, info) = if err.kind() == std::io::ErrorKind::InvalidData {
                    (StatusCode::BAD_REQUEST, ErrorInfo::new("protocol", format!("{alias} sent more data than announced.")))
                } else if err.kind() == std::io::ErrorKind::StorageFull {
                    (StatusCode::INSUFFICIENT_STORAGE, ErrorInfo::new("disk_full", "The disk is full."))
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, ErrorInfo::internal(&err))
                };
                self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(info));
                return error(status, "Could not write file");
            }
        };

        if outcome.len != claim.size {
            // Interrupted. Resumable sessions keep what is durable on disk.
            if resumable {
                self.release(session, file_id, claim.generation, ReleaseAs::Pending { offset: outcome.len }, None);
                if let (Some(fp), Some(tid)) = (session.peer.identity.fingerprint(), &session.ferry_transfer_id) {
                    let _ = self.shared.db.update_inbound_file(fp, tid, file_id, outcome.len, false, None);
                }
            } else {
                let _ = tokio::fs::remove_file(&claim.part_path).await;
                self.release(session, file_id, claim.generation, ReleaseAs::Retry, None);
            }
            let (status, message) = failure.unwrap_or((StatusCode::BAD_REQUEST, "Incomplete file"));
            return error(status, message);
        }

        self.complete_file(session, file_id, claim, outcome.inline_sha256).await
    }

    async fn prepare_dest(&self, session: &Session, dest_dir: &Path) -> Result<(), Resp> {
        if session.checked_dirs.lock().unwrap().contains(dest_dir) {
            return Ok(());
        }
        let dest = dest_dir.to_path_buf();
        let root = session.save_dir.clone();
        let ok = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
            std::fs::create_dir_all(&dest)?;
            // Defense in depth: the resolved folder must stay inside the save folder.
            let real_dest = std::fs::canonicalize(&dest)?;
            let real_root = std::fs::canonicalize(&root)?;
            Ok(real_dest.starts_with(real_root))
        })
        .await;
        match ok {
            Ok(Ok(true)) => {
                session.checked_dirs.lock().unwrap().insert(dest_dir.to_path_buf());
                Ok(())
            }
            Ok(Ok(false)) => Err(error(StatusCode::FORBIDDEN, "Invalid path")),
            _ => Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Could not create folder")),
        }
    }

    /// Small files: collect the body in memory, then write + hash in a single
    /// blocking call (the per-file overhead dominates for thousands of files).
    async fn receive_small(&self, session: &Arc<Session>, file_id: &str, claim: &Claim, mut body: Incoming) -> Resp {
        let entry = session.entry.clone();
        entry.set_file_state(file_id, FileState::Transferring, None);
        entry.reset_file_progress(file_id, 0);
        let mut data = bytes::BytesMut::with_capacity(claim.size as usize);
        let failure = loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(BODY_IDLE_TIMEOUT, body.frame()) => frame,
                _ = claim.cancel.cancelled() => break Some((StatusCode::CONFLICT, "Cancelled")),
            };
            match frame {
                Err(_) => break Some((StatusCode::REQUEST_TIMEOUT, "Connection stalled")),
                Ok(None) => break None,
                Ok(Some(Err(_))) => break Some((StatusCode::BAD_REQUEST, "Connection lost")),
                Ok(Some(Ok(frame))) => {
                    let Ok(chunk) = frame.into_data() else { continue };
                    if data.len() as u64 + chunk.len() as u64 > claim.size {
                        break Some((StatusCode::BAD_REQUEST, "More data than announced"));
                    }
                    data.extend_from_slice(&chunk);
                }
            }
        };
        drop(body);
        if failure.is_some() || data.len() as u64 != claim.size {
            self.release(session, file_id, claim.generation, ReleaseAs::Retry, None);
            let (status, message) = failure.unwrap_or((StatusCode::BAD_REQUEST, "Incomplete file"));
            return error(status, message);
        }
        if let Some(p) = entry.file_progress(file_id) {
            p.store(claim.size, Ordering::Relaxed);
        }
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(&data));
        let (expected, metadata) = {
            let files = session.files.lock().unwrap();
            let f = &files[file_id];
            (f.expected_sha256.clone(), f.metadata.clone())
        };
        // Checked before anything touches the disk: damaged data never lands.
        if self.shared.settings.get().verify_incoming_checksums
            && let Some(expected) = expected.as_ref()
            && !expected.eq_ignore_ascii_case(&hash)
        {
            self.release(session, file_id, claim.generation, ReleaseAs::Retry, Some(ErrorInfo::checksum_mismatch(&claim.name)));
            return error(StatusCode::UNPROCESSABLE_ENTITY, "Checksum mismatch");
        }
        let dest_dir = claim.dest_dir.clone();
        let name = claim.name.clone();
        let data = data.freeze();
        let written = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
            let path = unique::write_new_unique(
                &dest_dir,
                &name,
                &data,
                metadata.as_ref().and_then(|m| m.modified_time()),
                metadata.as_ref().and_then(|m| m.accessed_time()),
            )?;
            Ok(path)
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        match written {
            Ok(final_path) => {
                let verified = expected.is_some_and(|e| e.eq_ignore_ascii_case(&hash));
                self.mark_done(session, file_id, claim.size, final_path, hash, verified)
            }
            Err(err) => {
                let status = if err.kind() == std::io::ErrorKind::StorageFull {
                    StatusCode::INSUFFICIENT_STORAGE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(ErrorInfo::internal(&err)));
                error(status, "Could not write file")
            }
        }
    }

    async fn complete_file(&self, session: &Arc<Session>, file_id: &str, claim: &Claim, inline: Option<[u8; 32]>) -> Resp {
        let entry = session.entry.clone();
        entry.set_file_state(file_id, FileState::Verifying, None);
        let part_path = claim.part_path.clone();
        let size = claim.size;
        let hash = match inline {
            Some(h) => Ok(h),
            None => tokio::task::spawn_blocking(move || part::hash_file(&part_path)).await.map_err(std::io::Error::other).and_then(|r| r),
        };
        let hash = match hash {
            Ok(h) => hex::encode(h),
            Err(err) => {
                self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(ErrorInfo::internal(&err)));
                return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not read file");
            }
        };

        let (expected, metadata) = {
            let files = session.files.lock().unwrap();
            let f = &files[file_id];
            (f.expected_sha256.clone(), f.metadata.clone())
        };
        let verify = self.shared.settings.get().verify_incoming_checksums;
        if let Some(expected) = expected.as_ref().filter(|_| verify)
            && !expected.eq_ignore_ascii_case(&hash)
        {
            let _ = tokio::fs::remove_file(&claim.part_path).await;
            let name = claim.name.clone();
            self.release(session, file_id, claim.generation, ReleaseAs::Retry, Some(ErrorInfo::checksum_mismatch(&name)));
            return error(StatusCode::UNPROCESSABLE_ENTITY, "Checksum mismatch");
        }

        // Durable, then visible under its final (never colliding) name.
        let part_path = claim.part_path.clone();
        let dest_dir = claim.dest_dir.clone();
        let name = claim.name.clone();
        let final_path = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
            if size >= SYNC_THRESHOLD {
                part::sync_file(&part_path)?;
            }
            // Tagged before it becomes visible; the stream moves with the rename.
            motw::mark_from_network(&part_path);
            let final_path = unique::rename_unique(&part_path, &dest_dir, &name)?;
            part::set_times(
                &final_path,
                metadata.as_ref().and_then(|m| m.modified_time()),
                metadata.as_ref().and_then(|m| m.accessed_time()),
            );
            Ok(final_path)
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        let final_path = match final_path {
            Ok(p) => p,
            Err(err) => {
                self.release(session, file_id, claim.generation, ReleaseAs::Failed, Some(ErrorInfo::internal(&err)));
                return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save file");
            }
        };

        let verified = expected.is_some_and(|e| e.eq_ignore_ascii_case(&hash));
        self.mark_done(session, file_id, size, final_path, hash, verified)
    }

    /// Bookkeeping once a file is safely on disk under its final name.
    fn mark_done(&self, session: &Arc<Session>, file_id: &str, size: u64, final_path: PathBuf, hash: String, verified: bool) -> Resp {
        let entry = session.entry.clone();
        let history_id = self.record_history(session, file_id, &final_path, verified);
        {
            let mut files = session.files.lock().unwrap();
            if let Some(f) = files.get_mut(file_id) {
                f.state = InState::Done;
                f.active_cancel = None;
                f.offset = f.size;
                f.final_path = Some(final_path.clone());
                f.received_sha256 = Some(hash.clone());
                f.history_id = history_id;
            }
        }
        if let (Some(fp), Some(tid)) = (session.peer.identity.fingerprint(), &session.ferry_transfer_id) {
            let _ = self.shared.db.update_inbound_file(fp, tid, file_id, size, true, Some(&final_path.display().to_string()));
        }
        entry.update_file(file_id, |f| {
            f.state = FileState::Done;
            f.error = None;
            f.path = Some(final_path.display().to_string());
        });
        entry.reset_file_progress(file_id, size);
        self.maybe_finish(session);
        json(StatusCode::OK, &UploadResponse { sha256: Some(hash) })
    }

    fn record_history(&self, session: &Session, file_id: &str, path: &Path, verified: bool) -> Option<i64> {
        if !self.shared.settings.get().history_enabled {
            return None;
        }
        let (name, size, mime) = {
            let files = session.files.lock().unwrap();
            let f = &files[file_id];
            (f.rel.display(), f.size, f.mime.clone())
        };
        let entry = NewHistoryEntry {
            transfer_id: session.id.clone(),
            direction: Direction::Receive,
            peer_id: session.peer_ref.id.clone(),
            peer_alias: session.peer_ref.alias.clone(),
            peer_kind: session.peer_ref.device_kind,
            kind: HistoryKind::File,
            name,
            size,
            mime,
            path: Some(path.display().to_string()),
            text: None,
            timestamp_ms: now_ms(),
            status: HistoryStatus::Completed,
            verified,
        };
        match self.shared.db.add_history(&entry) {
            Ok(e) => {
                let id = e.id;
                self.shared.events.emit(EngineEvent::HistoryAdded { entry: e });
                Some(id)
            }
            Err(err) => {
                tracing::warn!("history write failed: {err}");
                None
            }
        }
    }

    fn release(&self, session: &Session, file_id: &str, generation: u64, as_: ReleaseAs, error_info: Option<ErrorInfo>) {
        let mut files = session.files.lock().unwrap();
        let Some(f) = files.get_mut(file_id) else { return };
        // A newer upload took over; it owns the state now.
        if f.state != InState::Active(generation) {
            return;
        }
        f.active_cancel = None;
        let state = match as_ {
            ReleaseAs::Pending { offset } => {
                f.offset = offset;
                f.state = InState::Pending;
                FileState::Pending
            }
            ReleaseAs::Retry => {
                f.offset = 0;
                f.attempts += 1;
                if f.attempts >= MAX_ATTEMPTS {
                    f.state = InState::Failed;
                    FileState::Failed
                } else {
                    f.state = InState::Pending;
                    FileState::Pending
                }
            }
            ReleaseAs::Failed => {
                f.attempts = MAX_ATTEMPTS;
                f.state = InState::Failed;
                FileState::Failed
            }
        };
        let offset = f.offset;
        drop(files);
        session.entry.set_file_state(file_id, state, error_info);
        session.entry.reset_file_progress(file_id, offset);
        if state == FileState::Failed {
            self.maybe_finish(session);
        }
    }

    fn maybe_finish(&self, session: &Session) {
        let all_final = session.files.lock().unwrap().values().all(|f| matches!(f.state, InState::Done | InState::Failed));
        if !all_final {
            return;
        }
        let mut finished = session.finished_at.lock().unwrap();
        if finished.is_some() {
            return;
        }
        *finished = Some(Instant::now());
        drop(finished);
        let state = session.entry.conclude();
        tracing::info!("Transfer {} from {} finished: {state:?}", session.id, session.peer_ref.alias);
        if let (Some(fp), Some(tid)) = (session.peer.identity.fingerprint(), &session.ferry_transfer_id) {
            let _ = self.shared.db.delete_inbound(fp, tid);
        }
    }

    // ── Ferry: verify / status ───────────────────────────────────────────

    pub async fn verify(&self, peer: &PeerContext, query: &HashMap<String, String>, body: Incoming, routes: &Routes) -> Resp {
        let (Some(session_id), Some(file_id), Some(token)) = (query.get("sessionId"), query.get("fileId"), query.get("token")) else {
            return error(StatusCode::BAD_REQUEST, "Missing parameters");
        };
        let request: VerifyRequest = match server::read_json(peer, body, server::SMALL_JSON_LIMIT, routes).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        let Some(session) = self.sessions.lock().unwrap().get(session_id).cloned() else {
            return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
        };
        if !session.peer_matches(peer) {
            return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
        }
        let (matches, final_path, history_id) = {
            let files = session.files.lock().unwrap();
            let Some(f) = files.get(file_id.as_str()) else {
                return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
            };
            if !secret_eq(&f.token, token) {
                return error(StatusCode::FORBIDDEN, "Invalid token or IP address");
            }
            if f.state != InState::Done {
                return error(StatusCode::CONFLICT, "File not complete");
            }
            (f.received_sha256.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(&request.sha256)), f.final_path.clone(), f.history_id)
        };
        if matches {
            if let Some(id) = history_id {
                let _ = self.shared.db.set_history_verified(id);
            }
            return json(StatusCode::OK, &VerifyResponse { ok: true });
        }
        // The data differs from what the sender read: discard and let it resend.
        tracing::warn!("verification failed for {file_id} from {}", session.peer_ref.alias);
        if let Some(path) = final_path {
            let _ = tokio::fs::remove_file(&path).await;
        }
        if let Some(id) = history_id {
            let _ = self.shared.db.delete_history(id);
        }
        {
            let mut files = session.files.lock().unwrap();
            if let Some(f) = files.get_mut(file_id.as_str()) {
                f.state = InState::Pending;
                f.offset = 0;
                f.attempts += 1;
                f.final_path = None;
                f.received_sha256 = None;
                if f.attempts >= MAX_ATTEMPTS {
                    f.state = InState::Failed;
                }
            }
        }
        *session.finished_at.lock().unwrap() = None;
        session.entry.set_file_state(file_id, FileState::Pending, Some(ErrorInfo::checksum_mismatch(file_id)));
        session.entry.reset_file_progress(file_id, 0);
        json(StatusCode::UNPROCESSABLE_ENTITY, &VerifyResponse { ok: false })
    }

    pub fn transfer_status(&self, peer: &PeerContext, transfer_id: &str) -> Resp {
        let Some(fp) = peer.identity.fingerprint() else {
            return error(StatusCode::FORBIDDEN, "Forbidden");
        };
        let session = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .find(|s| s.ferry_transfer_id.as_deref() == Some(transfer_id) && s.peer.identity.fingerprint() == Some(fp))
            .cloned();
        let files: HashMap<String, FileProgressDto> = match session {
            Some(s) => s
                .files
                .lock()
                .unwrap()
                .iter()
                .map(|(id, f)| (id.clone(), FileProgressDto { offset: f.offset, done: f.state == InState::Done }))
                .collect(),
            None => match self.shared.db.load_inbound(fp, transfer_id) {
                Ok(Some(record)) => {
                    record.files.into_iter().map(|f| (f.file_id, FileProgressDto { offset: f.offset, done: f.done })).collect()
                }
                _ => return error(StatusCode::NOT_FOUND, "Unknown transfer"),
            },
        };
        json(StatusCode::OK, &TransferStatusResponse { files })
    }

    // ── Cancellation & decisions ─────────────────────────────────────────

    /// A peer asked to cancel. Returns whether it concerned one of our
    /// incoming sessions or pending requests.
    pub fn cancel_from_peer(&self, peer: &PeerContext, session_id: Option<&str>) -> bool {
        if let Some(session_id) = session_id {
            let session = self.sessions.lock().unwrap().get(session_id).cloned();
            if let Some(session) = session
                && session.peer_matches(peer)
            {
                self.cancel_session(&session, true);
                return true;
            }
            return false;
        }
        // Without a session id: the sender aborts its own pending request(s).
        // Same kind of identity only: a fingerprint, or else the exact address,
        // so an unverified neighbour can't cancel a verified device's request.
        let fingerprint = peer.identity.fingerprint();
        let mut pending = self.pending.lock().unwrap();
        let mut any = false;
        for p in pending.values_mut() {
            let same = match (fingerprint, &p.fingerprint) {
                (Some(a), Some(b)) => a == b,
                (None, None) => p.ip == peer.ip.ip,
                _ => false,
            };
            if same {
                // Dropping the sender wakes the handler, which answers and cleans up.
                p.decision.take();
                any = true;
            }
        }
        any
    }

    /// Answers a pending request (from the UI).
    pub fn respond(&self, request_id: &str, decision: Decision) -> bool {
        let tx = self.pending.lock().unwrap().get_mut(request_id).and_then(|p| p.decision.take());
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Cancels an incoming transfer (from the UI, or because the sender did).
    pub fn cancel_transfer(&self, transfer_id: &str) -> bool {
        let session = self.sessions.lock().unwrap().get(transfer_id).cloned();
        match session {
            Some(session) => {
                self.cancel_session(&session, false);
                true
            }
            None => false,
        }
    }

    fn cancel_session(&self, session: &Arc<Session>, by_peer: bool) {
        session.cancel.cancel();
        self.sessions.lock().unwrap().remove(&session.id);
        let parts: Vec<PathBuf> =
            session.files.lock().unwrap().values().filter(|f| f.state != InState::Done).map(|f| f.part_path.clone()).collect();
        let mut names = Vec::new();
        {
            let files = session.files.lock().unwrap();
            for (id, f) in files.iter() {
                if f.state != InState::Done {
                    names.push(id.clone());
                }
            }
        }
        for id in names {
            session.entry.set_file_state(&id, FileState::Cancelled, None);
        }
        if let (Some(fp), Some(tid)) = (session.peer.identity.fingerprint(), &session.ferry_transfer_id) {
            let _ = self.shared.db.delete_inbound(fp, tid);
        }
        let error = if by_peer { ErrorInfo::cancelled_by_peer() } else { ErrorInfo::cancelled() };
        session.entry.fail(TransferState::Cancelled, Some(error));
        tokio::spawn(async move {
            // Give in-flight writers a moment to close their handles.
            tokio::time::sleep(Duration::from_millis(300)).await;
            for part in parts {
                let _ = tokio::fs::remove_file(part).await;
            }
        });
        if !by_peer {
            self.notify_sender_cancel(session.clone());
        }
    }

    fn notify_sender_cancel(&self, session: Arc<Session>) {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let (Some(port), Some(protocol)) = (session.peer_dto.port, session.peer_dto.protocol) else { return };
            let addr = crate::client::PeerAddress { host: session.peer.ip.to_string(), port, protocol };
            let pin = session.peer.identity.fingerprint().map(str::to_string);
            if let Ok(client) = crate::client::PeerClient::new(&shared.identity, addr, pin) {
                client.cancel(Some(&session.id)).await;
            }
        });
    }

    /// Periodic cleanup: idle sessions, finished sessions, expired partials.
    pub fn housekeeping(&self) {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            if let Some(finished) = *session.finished_at.lock().unwrap() {
                if finished.elapsed() > FINISHED_LINGER {
                    self.sessions.lock().unwrap().remove(&session.id);
                }
                continue;
            }
            let idle = session.last_activity.lock().unwrap().elapsed();
            let active = session.files.lock().unwrap().values().any(|f| matches!(f.state, InState::Active(_)));
            if active {
                continue;
            }
            if session.resumable() {
                if idle > Duration::from_secs(45) && session.entry.state() == TransferState::Transferring {
                    // The sender's job is to come back; show that we're waiting.
                    session.entry.set_state(TransferState::Reconnecting);
                }
                if idle > RESUMABLE_IDLE_TIMEOUT {
                    session.cancel.cancel();
                    self.sessions.lock().unwrap().remove(&session.id);
                    session.entry.fail(
                        TransferState::Failed,
                        Some(
                            ErrorInfo::new("connection_lost", format!("{} stopped sending.", session.peer_ref.alias))
                                .with_hint("If it reconnects within 24 hours, the transfer continues where it stopped."),
                        ),
                    );
                }
            } else if idle > SESSION_IDLE_TIMEOUT {
                self.cancel_session_quietly(&session);
            }
        }
        let cutoff = now_ms().saturating_sub(RESUME_RETENTION.as_millis() as u64);
        if let Ok(parts) = self.shared.db.expire_inbound(cutoff) {
            for part in parts {
                let _ = std::fs::remove_file(part);
            }
        }
    }

    fn cancel_session_quietly(&self, session: &Arc<Session>) {
        session.cancel.cancel();
        self.sessions.lock().unwrap().remove(&session.id);
        for f in session.files.lock().unwrap().values() {
            if f.state != InState::Done {
                let _ = std::fs::remove_file(&f.part_path);
            }
        }
        session
            .entry
            .fail(TransferState::Failed, Some(ErrorInfo::new("connection_lost", format!("{} stopped sending.", session.peer_ref.alias))));
    }

    /// Ids of transfers currently receiving (for shutdown / diagnostics).
    pub fn active_count(&self) -> usize {
        self.sessions.lock().unwrap().values().filter(|s| s.finished_at.lock().unwrap().is_none()).count()
    }
}

struct Claim {
    generation: u64,
    cancel: CancellationToken,
    start: u64,
    size: u64,
    part_path: PathBuf,
    dest_dir: PathBuf,
    name: String,
    discard_existing: bool,
}

enum ReleaseAs {
    /// Keep partial data at `offset` (resumable) and wait for the sender.
    Pending {
        offset: u64,
    },
    /// Data discarded; the sender may retry (up to MAX_ATTEMPTS).
    Retry,
    Failed,
}

fn validate_files(files: &IndexMap<String, FileDto>) -> Result<Vec<FileSpec>, &'static str> {
    let mut out = Vec::with_capacity(files.len());
    for (id, dto) in files {
        if id.is_empty() || id.len() > 128 {
            return Err("Invalid file id");
        }
        if dto.size > MAX_FILE_SIZE {
            return Err("File too large");
        }
        let rel = sanitize_relative_path(&dto.file_name).map_err(|_| "Invalid file name")?;
        let mime = if dto.file_type.is_empty() || dto.file_type.len() > 255 {
            crate::util::mime_for(rel.file_name())
        } else {
            dto.file_type.to_lowercase()
        };
        if dto.preview.as_ref().is_some_and(|p| p.len() > MAX_PREVIEW_BYTES) {
            return Err("Preview too large");
        }
        let sha256 = dto.sha256.as_ref().filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).cloned();
        out.push(FileSpec {
            id: id.clone(),
            rel,
            size: dto.size,
            mime,
            sha256,
            preview: dto.preview.clone(),
            metadata: dto.metadata.clone(),
        });
    }
    Ok(out)
}

fn summarize_text(text: &str) -> String {
    let line: String = text.lines().next().unwrap_or("").chars().take(80).collect();
    if line.is_empty() { "Message".to_string() } else { line }
}

#[allow(unused)]
fn _assert_bytes(_: Bytes) {}
