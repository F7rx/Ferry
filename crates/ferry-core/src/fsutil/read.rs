//! Reading files for sending: a blocking thread reads fixed-size chunks and
//! hashes them on the way out, so a file is read exactly once per send.

use bytes::{Bytes, BytesMut};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

/// 256 KiB chunks × 4 in flight = 1 MiB of read-ahead per stream.
const READ_CHUNK_BYTES: usize = 256 * 1024;
const CHANNEL_CHUNKS: usize = 4;

pub struct FileStream {
    pub chunks: mpsc::Receiver<io::Result<Bytes>>,
    /// SHA-256 of the bytes streamed, if the stream started at offset 0 and
    /// reached the end. Resolved after the last chunk.
    pub sha256: oneshot::Receiver<Option<[u8; 32]>>,
}

/// Streams `path` from `offset` to `expected_len`. Fails the stream (rather
/// than silently truncating) if the file changed size or disappeared.
pub fn stream_file(path: PathBuf, offset: u64, expected_len: u64) -> FileStream {
    let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
    let (hash_tx, hash_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let result = read_loop(&path, offset, expected_len, &tx);
        match result {
            Ok(hash) => {
                let _ = hash_tx.send(hash);
            }
            Err(err) => {
                let _ = tx.blocking_send(Err(err));
                let _ = hash_tx.send(None);
            }
        }
    });
    FileStream { chunks: rx, sha256: hash_rx }
}

fn read_loop(path: &PathBuf, offset: u64, expected_len: u64, tx: &mpsc::Sender<io::Result<Bytes>>) -> io::Result<Option<[u8; 32]>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file changed size since it was selected ({expected_len} → {len} bytes)"),
        ));
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut hasher = (offset == 0).then(Sha256::new);
    let mut remaining = expected_len - offset;
    let mut buf = BytesMut::with_capacity(READ_CHUNK_BYTES);
    while remaining > 0 {
        let want = remaining.min(READ_CHUNK_BYTES as u64) as usize;
        buf.resize(want, 0);
        let mut filled = 0;
        while filled < want {
            let n = file.read(&mut buf[filled..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file was truncated while sending"));
            }
            filled += n;
        }
        let chunk = buf.split_to(filled).freeze();
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        remaining -= chunk.len() as u64;
        if tx.blocking_send(Ok(chunk)).is_err() {
            // The request was dropped (cancelled / connection lost).
            return Ok(None);
        }
        buf.reserve(READ_CHUNK_BYTES);
    }
    Ok(hasher.map(|h| h.finalize().into()))
}

/// SHA-256 of `path` read in full on a blocking thread.
pub async fn hash_file_async(path: PathBuf) -> io::Result<[u8; 32]> {
    tokio::task::spawn_blocking(move || super::part::hash_file(&path)).await.map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(mut s: FileStream) -> (Vec<u8>, Option<[u8; 32]>, Option<io::Error>) {
        let mut out = Vec::new();
        let mut error = None;
        while let Some(chunk) = s.chunks.recv().await {
            match chunk {
                Ok(c) => out.extend_from_slice(&c),
                Err(e) => error = Some(e),
            }
        }
        (out, s.sha256.await.unwrap_or(None), error)
    }

    #[tokio::test]
    async fn streams_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let data: Vec<u8> = (0..1_000_000u32).map(|i| i as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (out, hash, err) = collect(stream_file(path, 0, data.len() as u64)).await;
        assert!(err.is_none());
        assert_eq!(out, data);
        assert_eq!(hash, Some(Sha256::digest(&data).into()));
    }

    #[tokio::test]
    async fn streams_from_offset_without_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"0123456789").unwrap();
        let (out, hash, err) = collect(stream_file(path, 4, 10)).await;
        assert!(err.is_none());
        assert_eq!(out, b"456789");
        assert_eq!(hash, None);
    }

    #[tokio::test]
    async fn fails_when_file_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"0123456789").unwrap();
        let (_, _, err) = collect(stream_file(path, 0, 99)).await;
        assert_eq!(err.unwrap().kind(), io::ErrorKind::InvalidData);
    }
}
