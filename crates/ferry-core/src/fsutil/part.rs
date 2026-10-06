//! The part-file writer: every received byte goes through here.
//!
//! - Data lands in `<name>.ferrypart`, created with `create_new` (refuses to
//!   follow or replace anything that already exists).
//! - Writing happens on a dedicated blocking thread fed by a bounded channel,
//!   so a slow disk applies backpressure to the TCP connection instead of
//!   buffering in memory. Memory per file is constant.
//! - SHA-256 is computed inline while writing (no second read) whenever the
//!   file is written from offset 0.
//! - Resumable writers fsync at checkpoints and publish the durable offset, so
//!   after a crash the file is truncated to a point known to be on disk.

use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, watch};

const WRITE_BUFFER_BYTES: usize = 1 << 20;
const CHANNEL_CHUNKS: usize = 32;
const HASH_BUFFER_BYTES: usize = 1 << 20;

/// Durable checkpoint interval for resumable transfers: every this many
/// bytes, or every [`CHECKPOINT_INTERVAL`] once [`CHECKPOINT_MIN_BYTES`] are
/// pending, so a crash loses at most a couple of seconds of progress.
pub const DEFAULT_CHECKPOINT_BYTES: u64 = 64 << 20;
const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const CHECKPOINT_MIN_BYTES: u64 = 1 << 20;

pub struct PartSpec {
    /// Path of the `.ferrypart` file.
    pub path: PathBuf,
    /// 0 creates a new file; > 0 continues an existing part file at that offset.
    pub offset: u64,
    /// Full length of the file being received.
    pub expected_len: u64,
    /// fsync and publish the offset every this many bytes (resumable transfers).
    pub checkpoint_every: Option<u64>,
}

#[derive(Debug)]
pub struct WriteOutcome {
    /// Bytes now in the part file.
    pub len: u64,
    /// SHA-256 of the whole file, when it is complete and was written from 0.
    pub inline_sha256: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Resuming needs at least `offset` bytes, but only `available` are on disk.
    #[error("part file holds only {available} bytes")]
    Short { available: u64 },
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct PartWriter {
    tx: Option<mpsc::Sender<Bytes>>,
    task: tokio::task::JoinHandle<io::Result<WriteOutcome>>,
    checkpoints: watch::Receiver<u64>,
}

impl PartWriter {
    /// Opens (or creates) the part file and starts the writer thread.
    pub async fn start(spec: PartSpec) -> Result<PartWriter, OpenError> {
        let path = spec.path.clone();
        let offset = spec.offset;
        let file = tokio::task::spawn_blocking(move || open_part(&path, offset)).await.map_err(io::Error::other)??;

        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        let (cp_tx, cp_rx) = watch::channel(spec.offset);
        let task = tokio::task::spawn_blocking(move || write_loop(file, spec, rx, cp_tx));
        Ok(PartWriter { tx: Some(tx), task, checkpoints: cp_rx })
    }

    /// Queues a chunk. Waits when the disk is slower than the network.
    pub async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let Some(tx) = &self.tx else {
            return Err(io::Error::other("writer already finished"));
        };
        if tx.send(chunk).await.is_err() {
            // The thread stopped early: surface its actual error.
            self.tx = None;
            return match (&mut self.task).await {
                Ok(Err(err)) => Err(err),
                Ok(Ok(_)) => Err(io::Error::other("writer stopped")),
                Err(join) => Err(io::Error::other(join)),
            };
        }
        Ok(())
    }

    /// Durable offsets published at each checkpoint.
    pub fn checkpoints(&self) -> watch::Receiver<u64> {
        self.checkpoints.clone()
    }

    /// Flushes everything and returns what is on disk.
    pub async fn finish(mut self) -> io::Result<WriteOutcome> {
        self.tx = None;
        (&mut self.task).await.map_err(io::Error::other)?
    }
}

fn open_part(path: &Path, offset: u64) -> Result<File, OpenError> {
    if offset == 0 {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        return Ok(file);
    }
    // Resuming: the part file must be a regular file we created earlier.
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "part path is not a regular file").into());
    }
    if meta.len() < offset {
        return Err(OpenError::Short { available: meta.len() });
    }
    let mut file = OpenOptions::new().write(true).open(path)?;
    // Anything past the requested offset was never confirmed; drop it.
    file.set_len(offset)?;
    file.seek(SeekFrom::End(0))?;
    Ok(file)
}

fn write_loop(file: File, spec: PartSpec, mut rx: mpsc::Receiver<Bytes>, checkpoints: watch::Sender<u64>) -> io::Result<WriteOutcome> {
    let mut hasher = (spec.offset == 0).then(Sha256::new);
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    let mut len = spec.offset;
    let mut last_checkpoint = spec.offset;
    let mut last_checkpoint_at = std::time::Instant::now();

    while let Some(chunk) = rx.blocking_recv() {
        let new_len = len + chunk.len() as u64;
        if new_len > spec.expected_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "received more data than the announced file size"));
        }
        writer.write_all(&chunk)?;
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        len = new_len;

        if let Some(every) = spec.checkpoint_every {
            let pending = len - last_checkpoint;
            if pending >= every || (pending >= CHECKPOINT_MIN_BYTES.min(every) && last_checkpoint_at.elapsed() >= CHECKPOINT_INTERVAL) {
                writer.flush()?;
                writer.get_ref().sync_data()?;
                last_checkpoint = len;
                last_checkpoint_at = std::time::Instant::now();
                let _ = checkpoints.send(len);
            }
        }
    }

    writer.flush()?;
    let file = writer.into_inner().map_err(|e| e.into_error())?;
    if spec.checkpoint_every.is_some() && len != last_checkpoint && len != spec.expected_len {
        // Interrupted: make what we have durable so a resume can start here.
        file.sync_data()?;
        let _ = checkpoints.send(len);
    }
    let inline_sha256 = match hasher {
        Some(hasher) if len == spec.expected_len => Some(hasher.finalize().into()),
        _ => None,
    };
    Ok(WriteOutcome { len, inline_sha256 })
}

/// SHA-256 of a whole file (used once, after a resumed file completes).
pub fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Makes a completed file durable before it is renamed into place.
pub fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Applies the sender's timestamps (best effort).
pub fn set_times(path: &Path, modified: Option<std::time::SystemTime>, accessed: Option<std::time::SystemTime>) {
    if modified.is_none() && accessed.is_none() {
        return;
    }
    let Ok(file) = OpenOptions::new().write(true).open(path) else {
        return;
    };
    let mut times = std::fs::FileTimes::new();
    if let Some(modified) = modified {
        times = times.set_modified(modified);
    }
    if let Some(accessed) = accessed {
        times = times.set_accessed(accessed);
    }
    if let Err(err) = file.set_times(times) {
        tracing::debug!("could not set file times on {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    #[tokio::test]
    async fn writes_and_hashes_inline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        let data = vec![7u8; 3 * 1024 * 1024 + 123];
        let mut w = PartWriter::start(PartSpec { path: path.clone(), offset: 0, expected_len: data.len() as u64, checkpoint_every: None })
            .await
            .unwrap();
        for chunk in data.chunks(50_000) {
            w.write(Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        let out = w.finish().await.unwrap();
        assert_eq!(out.len, data.len() as u64);
        assert_eq!(out.inline_sha256, Some(sha(&data)));
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[tokio::test]
    async fn refuses_existing_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        std::fs::write(&path, "victim").unwrap();
        let err =
            PartWriter::start(PartSpec { path: path.clone(), offset: 0, expected_len: 1, checkpoint_every: None }).await.err().unwrap();
        assert!(matches!(err, OpenError::Io(e) if e.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "victim");
    }

    #[tokio::test]
    async fn rejects_overrun() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        let mut w = PartWriter::start(PartSpec { path, offset: 0, expected_len: 10, checkpoint_every: None }).await.unwrap();
        w.write(Bytes::from_static(b"0123456789")).await.unwrap();
        // The overrun is detected by the thread; it surfaces on a later write or on finish.
        let _ = w.write(Bytes::from_static(b"!")).await;
        assert!(w.finish().await.is_err());
    }

    #[tokio::test]
    async fn resumes_from_offset_and_truncates_unconfirmed_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        // A previous run wrote 150k bytes, but only 100k were confirmed.
        std::fs::write(&path, &data[..150_000]).unwrap();

        let mut w =
            PartWriter::start(PartSpec { path: path.clone(), offset: 100_000, expected_len: data.len() as u64, checkpoint_every: Some(1) })
                .await
                .unwrap();
        w.write(Bytes::copy_from_slice(&data[100_000..])).await.unwrap();
        let out = w.finish().await.unwrap();
        assert_eq!(out.len, data.len() as u64);
        // Not written from 0, so no inline hash; the full-file hash matches.
        assert_eq!(out.inline_sha256, None);
        assert_eq!(hash_file(&path).unwrap(), sha(&data));
    }

    #[tokio::test]
    async fn short_part_file_reports_available_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        std::fs::write(&path, vec![0u8; 10]).unwrap();
        let err = PartWriter::start(PartSpec { path, offset: 20, expected_len: 30, checkpoint_every: None }).await.err().unwrap();
        assert!(matches!(err, OpenError::Short { available: 10 }));
    }

    #[tokio::test]
    async fn interrupted_resumable_write_publishes_durable_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ferrypart");
        let mut w = PartWriter::start(PartSpec {
            path: path.clone(),
            offset: 0,
            expected_len: 1_000,
            checkpoint_every: Some(DEFAULT_CHECKPOINT_BYTES),
        })
        .await
        .unwrap();
        let checkpoints = w.checkpoints();
        w.write(Bytes::from(vec![1u8; 400])).await.unwrap();
        let out = w.finish().await.unwrap();
        assert_eq!(out.len, 400);
        assert_eq!(*checkpoints.borrow(), 400);
    }
}
