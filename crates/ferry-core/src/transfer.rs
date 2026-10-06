//! Live transfers, shared by send and receive. Streams bump atomic counters;
//! a ticker turns them into coalesced UI events (≤ 10 Hz per transfer) with a
//! smoothed speed and ETA.

use crate::error::ErrorInfo;
use crate::events::{EngineEvent, EventBus};
use crate::model::*;
use crate::util::now_ms;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(100);
/// Finished transfers kept for the UI.
const MAX_FINISHED: usize = 100;

pub struct TransferEntry {
    pub id: String,
    pub direction: Direction,
    pub drop_id: Option<String>,
    pub started_at_ms: u64,
    total_bytes: AtomicU64,
    meta: Mutex<Meta>,
    files: Mutex<Vec<FileSlot>>,
    /// File id → position in `files` (the list never changes shape).
    index: HashMap<String, usize>,
    dirty: AtomicBool,
    files_dirty: Mutex<HashSet<usize>>,
    speed: Mutex<Speed>,
}

struct Meta {
    peer: PeerRef,
    state: TransferState,
    error: Option<ErrorInfo>,
    finished_at_ms: Option<u64>,
    connection: Option<ConnectionInfo>,
    resumable: bool,
    text: Option<String>,
    save_dir: Option<String>,
}

struct FileSlot {
    file: TransferFile,
    progress: Arc<AtomicU64>,
}

struct Speed {
    last_bytes: u64,
    last_at: Instant,
    ema_bps: f64,
}

impl TransferEntry {
    /// Takes one lock at a time (meta, files, speed, never nested), so it
    /// can't deadlock with the ticker or with per-file updates.
    pub fn summary(&self) -> TransferSummary {
        let (peer, state, finished_at_ms, connection, resumable, text, error, save_dir) = {
            let m = self.meta.lock().unwrap();
            (
                m.peer.clone(),
                m.state,
                m.finished_at_ms,
                m.connection.clone(),
                m.resumable,
                m.text.clone(),
                m.error.clone(),
                m.save_dir.clone(),
            )
        };
        let total = self.total_bytes.load(Ordering::Relaxed);
        let (done, files_done, file_count, title) = {
            let files = self.files.lock().unwrap();
            (
                sum_progress(&files).min(total),
                files.iter().filter(|f| f.file.state == FileState::Done).count() as u32,
                files.len() as u32,
                files.first().map(|f| f.file.name.clone()).unwrap_or_default(),
            )
        };
        let ema = self.speed.lock().unwrap().ema_bps;
        let speed_bps = if state == TransferState::Transferring { ema as u64 } else { 0 };
        let eta_secs = (speed_bps > 0 && total > done).then(|| (total - done) / speed_bps.max(1));
        TransferSummary {
            id: self.id.clone(),
            direction: self.direction,
            drop_id: self.drop_id.clone(),
            peer,
            state,
            file_count,
            files_done,
            total_bytes: total,
            bytes_done: done,
            speed_bps,
            eta_secs,
            started_at_ms: self.started_at_ms,
            finished_at_ms,
            connection,
            resumable,
            title,
            text,
            error,
            save_dir,
        }
    }

    pub fn state(&self) -> TransferState {
        self.meta.lock().unwrap().state
    }

    pub fn peer(&self) -> PeerRef {
        self.meta.lock().unwrap().peer.clone()
    }

    pub fn set_state(&self, state: TransferState) {
        let mut meta = self.meta.lock().unwrap();
        if meta.state.is_final() {
            return;
        }
        meta.state = state;
        if state.is_final() {
            meta.finished_at_ms = Some(now_ms());
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn fail(&self, state: TransferState, error: Option<ErrorInfo>) {
        {
            let mut meta = self.meta.lock().unwrap();
            if meta.state.is_final() {
                return;
            }
            meta.error = error;
        }
        self.set_state(state);
    }

    pub fn set_error(&self, error: Option<ErrorInfo>) {
        self.meta.lock().unwrap().error = error;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_connection(&self, connection: ConnectionInfo) {
        self.meta.lock().unwrap().connection = Some(connection);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_resumable(&self, resumable: bool) {
        self.meta.lock().unwrap().resumable = resumable;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_save_dir(&self, dir: Option<String>) {
        self.meta.lock().unwrap().save_dir = dir;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_peer(&self, peer: PeerRef) {
        self.meta.lock().unwrap().peer = peer;
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Bytes done across all files (sum of the per-file counters).
    pub fn bytes_done(&self) -> u64 {
        sum_progress(&self.files.lock().unwrap())
    }

    /// The per-file progress counter a stream increments.
    pub fn file_progress(&self, file_id: &str) -> Option<Arc<AtomicU64>> {
        let i = *self.index.get(file_id)?;
        self.files.lock().unwrap().get(i).map(|f| f.progress.clone())
    }

    /// Updates one file. `bytes_done` (if given) resets the file's counter and
    /// corrects the transfer total accordingly (used on restart/resume).
    pub fn update_file(&self, file_id: &str, f: impl FnOnce(&mut TransferFile)) {
        let Some(&index) = self.index.get(file_id) else { return };
        let mut files = self.files.lock().unwrap();
        if let Some(slot) = files.get_mut(index) {
            f(&mut slot.file);
            self.files_dirty.lock().unwrap().insert(index);
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn set_file_state(&self, file_id: &str, state: FileState, error: Option<ErrorInfo>) {
        self.update_file(file_id, |f| {
            f.state = state;
            f.error = error;
        });
    }

    /// Sets a file's progress to an absolute value (restart / resume / done).
    pub fn reset_file_progress(&self, file_id: &str, value: u64) {
        if let Some(&index) = self.index.get(file_id)
            && let Some(slot) = self.files.lock().unwrap().get(index)
        {
            slot.progress.store(value, Ordering::Relaxed);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Marks files the peer didn't accept and removes their bytes from the total.
    pub fn skip_files(&self, accepted: &HashSet<String>) {
        let mut files = self.files.lock().unwrap();
        let mut skipped = 0;
        for (index, slot) in files.iter_mut().enumerate() {
            if !accepted.contains(&slot.file.id) && slot.file.state == FileState::Pending {
                slot.file.state = FileState::Skipped;
                skipped += slot.file.size;
                self.files_dirty.lock().unwrap().insert(index);
            }
        }
        self.total_bytes.fetch_sub(skipped.min(self.total_bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn files(&self) -> Vec<TransferFile> {
        self.files
            .lock()
            .unwrap()
            .iter()
            .map(|s| {
                let mut f = s.file.clone();
                f.bytes_done = s.progress.load(Ordering::Relaxed).min(f.size);
                f
            })
            .collect()
    }

    /// Final state from the per-file results.
    pub fn conclude(&self) -> TransferState {
        let files = self.files.lock().unwrap();
        let done = files.iter().filter(|f| f.file.state == FileState::Done).count();
        let failed = files.iter().filter(|f| f.file.state == FileState::Failed).count();
        let cancelled = files.iter().filter(|f| f.file.state == FileState::Cancelled).count();
        drop(files);
        let state = if failed == 0 && cancelled == 0 {
            TransferState::Completed
        } else if done > 0 {
            TransferState::CompletedWithErrors
        } else if cancelled > 0 && failed == 0 {
            TransferState::Cancelled
        } else {
            TransferState::Failed
        };
        self.set_state(state);
        state
    }
}

fn sum_progress(files: &[FileSlot]) -> u64 {
    files.iter().map(|s| s.progress.load(Ordering::Relaxed).min(s.file.size)).sum()
}

pub struct NewTransfer {
    pub id: String,
    pub direction: Direction,
    pub drop_id: Option<String>,
    pub peer: PeerRef,
    pub files: Vec<TransferFile>,
    pub state: TransferState,
    pub resumable: bool,
    pub text: Option<String>,
    pub save_dir: Option<String>,
    pub connection: Option<ConnectionInfo>,
}

pub struct TransferRegistry {
    entries: Mutex<HashMap<String, Arc<TransferEntry>>>,
    order: Mutex<Vec<String>>,
    events: EventBus,
}

impl TransferRegistry {
    pub fn new(events: EventBus) -> Arc<Self> {
        Arc::new(Self { entries: Mutex::new(HashMap::new()), order: Mutex::new(Vec::new()), events })
    }

    pub fn create(&self, t: NewTransfer) -> Arc<TransferEntry> {
        let total: u64 = t.files.iter().map(|f| f.size).sum();
        let done: u64 = t.files.iter().map(|f| f.bytes_done.min(f.size)).sum();
        let entry = Arc::new(TransferEntry {
            id: t.id.clone(),
            direction: t.direction,
            drop_id: t.drop_id,
            started_at_ms: now_ms(),
            total_bytes: AtomicU64::new(total),
            meta: Mutex::new(Meta {
                peer: t.peer,
                state: t.state,
                error: None,
                finished_at_ms: None,
                connection: t.connection,
                resumable: t.resumable,
                text: t.text,
                save_dir: t.save_dir,
            }),
            index: t.files.iter().enumerate().map(|(i, f)| (f.id.clone(), i)).collect(),
            files: Mutex::new(
                t.files
                    .into_iter()
                    .map(|file| {
                        let progress = Arc::new(AtomicU64::new(file.bytes_done));
                        FileSlot { file, progress }
                    })
                    .collect(),
            ),
            dirty: AtomicBool::new(true),
            files_dirty: Mutex::new(HashSet::new()),
            speed: Mutex::new(Speed { last_bytes: done, last_at: Instant::now(), ema_bps: 0.0 }),
        });
        self.entries.lock().unwrap().insert(t.id.clone(), entry.clone());
        self.order.lock().unwrap().push(t.id);
        self.prune();
        // Announce immediately (with the file list) rather than on the next tick.
        self.events.emit(EngineEvent::TransferUpdated { transfer: entry.summary() });
        self.events.emit(EngineEvent::TransferFilesUpdated { id: entry.id.clone(), files: entry.files() });
        entry.dirty.store(false, Ordering::Relaxed);
        entry
    }

    pub fn get(&self, id: &str) -> Option<Arc<TransferEntry>> {
        self.entries.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<TransferSummary> {
        // Lock order everywhere: `order` before `entries` (see `prune`); and no
        // entry locks while holding either.
        let entries: Vec<Arc<TransferEntry>> = {
            let order = self.order.lock().unwrap();
            let entries = self.entries.lock().unwrap();
            order.iter().filter_map(|id| entries.get(id).cloned()).collect()
        };
        entries.iter().map(|e| e.summary()).collect()
    }

    /// Removes a finished transfer from the list (user dismissed it).
    pub fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            match entries.get(id) {
                Some(e) if e.state().is_final() => entries.remove(id).is_some(),
                _ => false,
            }
        };
        if removed {
            self.order.lock().unwrap().retain(|x| x != id);
            self.events.emit(EngineEvent::TransferRemoved { id: id.to_string() });
        }
        removed
    }

    fn prune(&self) {
        let mut order = self.order.lock().unwrap();
        let mut entries = self.entries.lock().unwrap();
        // `state()` takes an entry lock; entries never take registry locks, so
        // this nesting (registry → entry) is the only direction that exists.
        let finished: Vec<String> = order.iter().filter(|id| entries.get(*id).is_some_and(|e| e.state().is_final())).cloned().collect();
        if finished.len() > MAX_FINISHED {
            for id in &finished[..finished.len() - MAX_FINISHED] {
                entries.remove(id);
            }
            order.retain(|id| entries.contains_key(id));
        }
    }

    /// Emits coalesced updates; call every [`TICK`].
    pub fn tick(&self) {
        let entries: Vec<Arc<TransferEntry>> = self.entries.lock().unwrap().values().cloned().collect();
        for entry in entries {
            let transferring = entry.state() == TransferState::Transferring;
            {
                // Read the byte count *before* taking the speed lock: never
                // hold `speed` while locking `files` (see `summary`).
                let bytes = entry.bytes_done();
                let mut speed = entry.speed.lock().unwrap();
                let now = Instant::now();
                let dt = now.duration_since(speed.last_at).as_secs_f64();
                if dt >= 0.09 {
                    let instant = bytes.saturating_sub(speed.last_bytes) as f64 / dt;
                    // Fast attack when starting, smooth afterwards.
                    let alpha = if speed.ema_bps == 0.0 { 1.0 } else { 0.15 };
                    speed.ema_bps = alpha * instant + (1.0 - alpha) * speed.ema_bps;
                    speed.last_bytes = bytes;
                    speed.last_at = now;
                }
            }
            if transferring || entry.dirty.swap(false, Ordering::Relaxed) {
                self.events.emit(EngineEvent::TransferUpdated { transfer: entry.summary() });
            }
            let dirty_files: Vec<usize> = entry.files_dirty.lock().unwrap().drain().collect();
            let active: Vec<usize> = if transferring {
                entry
                    .files
                    .lock()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| matches!(s.file.state, FileState::Transferring | FileState::Verifying))
                    .map(|(i, _)| i)
                    .collect()
            } else {
                Vec::new()
            };
            if !dirty_files.is_empty() || !active.is_empty() {
                let files = entry.files.lock().unwrap();
                let mut indexes: Vec<usize> = dirty_files.into_iter().chain(active).collect();
                indexes.sort_unstable();
                indexes.dedup();
                let changed = indexes
                    .into_iter()
                    .filter_map(|i| files.get(i))
                    .map(|s| {
                        let mut f = s.file.clone();
                        f.bytes_done = s.progress.load(Ordering::Relaxed).min(f.size);
                        f
                    })
                    .take(500)
                    .collect::<Vec<_>>();
                drop(files);
                self.events.emit(EngineEvent::TransferFilesUpdated { id: entry.id.clone(), files: changed });
            }
        }
    }

    pub fn spawn_ticker(self: &Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => registry.tick(),
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(i: usize) -> TransferFile {
        TransferFile {
            id: format!("f{i}"),
            name: format!("file{i}.bin"),
            size: 4096,
            mime: "application/octet-stream".into(),
            state: FileState::Pending,
            bytes_done: 0,
            error: None,
            path: None,
        }
    }

    fn peer() -> PeerRef {
        PeerRef { id: "p".into(), alias: "Peer".into(), device_kind: DeviceKind::Desktop, device_model: None, verified: true }
    }

    /// Regression: the ticker once held `speed` while locking `files`, and
    /// `summary` held `files` while locking `speed`: a deadlock under load.
    #[test]
    fn concurrent_updates_summaries_and_ticks_do_not_deadlock() {
        let events = EventBus::new();
        let registry = TransferRegistry::new(events.clone());
        let entry = registry.create(NewTransfer {
            id: "t".into(),
            direction: Direction::Receive,
            drop_id: None,
            peer: peer(),
            files: (0..2_000).map(file).collect(),
            state: TransferState::Transferring,
            resumable: false,
            text: None,
            save_dir: None,
            connection: None,
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for t in 0..4 {
            let entry = entry.clone();
            let registry = registry.clone();
            threads.push(std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let mut i = 0usize;
                while started.elapsed() < std::time::Duration::from_millis(700) {
                    let id = format!("f{}", (i * 7 + t) % 2_000);
                    match t {
                        0 => {
                            entry.set_file_state(&id, FileState::Transferring, None);
                            entry.reset_file_progress(&id, (i % 4096) as u64);
                        }
                        1 => {
                            let _ = entry.summary();
                        }
                        2 => registry.tick(),
                        _ => {
                            let _ = registry.list();
                            let _ = entry.files();
                        }
                    }
                    i += 1;
                }
            }));
        }
        std::thread::spawn(move || {
            for t in threads {
                t.join().unwrap();
            }
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("deadlock: threads did not finish");
    }

    #[test]
    fn progress_and_speed_are_reported() {
        let registry = TransferRegistry::new(EventBus::new());
        let entry = registry.create(NewTransfer {
            id: "t".into(),
            direction: Direction::Send,
            drop_id: None,
            peer: peer(),
            files: (0..3).map(file).collect(),
            state: TransferState::Transferring,
            resumable: true,
            text: None,
            save_dir: None,
            connection: None,
        });
        entry.reset_file_progress("f0", 4096);
        entry.reset_file_progress("f1", 1000);
        std::thread::sleep(std::time::Duration::from_millis(120));
        registry.tick();
        let s = entry.summary();
        assert_eq!(s.total_bytes, 3 * 4096);
        assert_eq!(s.bytes_done, 5096);
        assert!(s.speed_bps > 0);
        entry.set_file_state("f0", FileState::Done, None);
        entry.set_file_state("f1", FileState::Failed, None);
        entry.set_file_state("f2", FileState::Done, None);
        assert_eq!(entry.conclude(), TransferState::CompletedWithErrors);
    }
}
