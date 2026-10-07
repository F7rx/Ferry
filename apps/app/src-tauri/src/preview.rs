//! Which received files the webview may load through the asset protocol
//! (image previews). Access is granted per file, never per folder: each path
//! must be a file Ferry received and committed, checked again here.

use ferry_core::model::{Direction, HistoryEntry, HistoryKind, HistoryStatus};
use std::path::{Component, Path, PathBuf};

/// The canonical form of `path` when it may be previewed: an absolute local
/// path (no network share or device namespace, no `..`), naming an existing
/// regular file that is not a symlink, inside `root` when one is known.
pub fn previewable_file(path: &Path, root: Option<&Path>) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return None;
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        // Only drive paths: `C:\…` or `\\?\C:\…`. UNC shares and device
        // paths (`\\.\…`, `\\?\GLOBALROOT…`) are refused.
        match path.components().next() {
            Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
            _ => return None,
        }
        // A colon after the drive names an alternate data stream.
        if path.components().any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().contains(':'))) {
            return None;
        }
    }
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    if let Some(root) = root {
        let root = std::fs::canonicalize(root).ok()?;
        if !canonical.starts_with(&root) {
            return None;
        }
    }
    Some(canonical)
}

/// A received, completed file entry with a path: the only kind with a preview.
pub fn received_file(entry: &HistoryEntry) -> Option<&str> {
    let received = entry.direction == Direction::Receive && entry.kind == HistoryKind::File && entry.status == HistoryStatus::Completed;
    if received { entry.path.as_deref() } else { None }
}
