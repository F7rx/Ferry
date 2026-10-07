//! Image previews: which received files the webview may load through the
//! asset protocol. (An integration test: Windows only starts test binaries
//! that embed the app manifest, see build.rs.)

use ferry_core::model::*;
use ferry_desktop_lib::grant_previews;
use ferry_desktop_lib::preview::previewable_file;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::Manager;

fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Received");
    fs::create_dir_all(root.join("Album")).unwrap();
    let file = root.join("Album").join("photo.png");
    fs::write(&file, b"png").unwrap();
    (dir, root, file)
}

#[test]
fn accepts_a_file_inside_its_root() {
    let (_dir, root, file) = tree();
    let got = previewable_file(&file, Some(&root)).unwrap();
    assert_eq!(got, fs::canonicalize(&file).unwrap());
    // Without a known root (history restored on startup) the file still qualifies.
    assert!(previewable_file(&file, None).is_some());
}

#[test]
fn refuses_relative_missing_and_non_file_paths() {
    let (_dir, root, file) = tree();
    assert!(previewable_file(Path::new("photo.png"), None).is_none());
    assert!(previewable_file(&root.join("Album").join("gone.png"), Some(&root)).is_none());
    assert!(previewable_file(&root.join("Album"), Some(&root)).is_none(), "directories are never granted");
    let dotted = root.join("Album").join("..").join("Album").join("photo.png");
    assert!(previewable_file(&dotted, Some(&root)).is_none());
    assert!(file.exists());
}

#[test]
fn refuses_files_outside_the_root() {
    let (dir, root, _file) = tree();
    let outside = dir.path().join("secret.png");
    fs::write(&outside, b"png").unwrap();
    assert!(previewable_file(&outside, Some(&root)).is_none());
    assert!(previewable_file(&outside, Some(&root.join("missing"))).is_none());
}

#[cfg(windows)]
#[test]
fn refuses_unc_device_and_stream_paths() {
    let (_dir, _root, file) = tree();
    for p in [
        r"\\server\share\photo.png",
        r"\\?\UNC\server\share\photo.png",
        r"\\.\C:\photo.png",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1\photo.png",
    ] {
        assert!(previewable_file(Path::new(p), None).is_none(), "{p}");
    }
    let stream = PathBuf::from(format!("{}:hidden", file.display()));
    assert!(previewable_file(&stream, None).is_none());
    // The verbatim form of a real drive path is fine.
    assert!(previewable_file(&fs::canonicalize(&file).unwrap(), None).is_some());
}

#[test]
fn refuses_symlinks() {
    let (dir, root, file) = tree();
    let outside = dir.path().join("secret.png");
    fs::write(&outside, b"png").unwrap();
    let link = root.join("link.png");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&outside, &link).is_ok();
    if made {
        assert!(previewable_file(&link, None).is_none(), "a symlink is never granted");
        assert!(previewable_file(&link, Some(&root)).is_none());
    }
    // A directory link inside the root that leads outside it.
    let dir_link = root.join("escape");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(dir.path(), &dir_link).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(dir.path(), &dir_link).is_ok()
        // Symlinks need a privilege; a junction works for everyone.
        || std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&dir_link)
            .arg(dir.path())
            .output()
            .is_ok_and(|o| o.status.success());
    if made {
        assert!(dir_link.join("secret.png").exists());
        assert!(previewable_file(&dir_link.join("secret.png"), Some(&root)).is_none());
    }
    assert!(previewable_file(&file, Some(&root)).is_some());
}

fn entry(id: i64, transfer_id: &str, direction: Direction, path: &Path) -> HistoryEntry {
    HistoryEntry {
        id,
        transfer_id: transfer_id.into(),
        direction,
        peer_id: "peer".into(),
        peer_alias: "Phone".into(),
        peer_kind: DeviceKind::Mobile,
        kind: HistoryKind::File,
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        size: 3,
        mime: "image/png".into(),
        path: Some(path.display().to_string()),
        text: None,
        timestamp_ms: 0,
        status: HistoryStatus::Completed,
        verified: true,
    }
}

fn transfer(id: &str, save_dir: &Path) -> TransferSummary {
    serde_json::from_value(serde_json::json!({
        "id": id, "direction": "receive", "dropId": null,
        "peer": { "id": "peer", "alias": "Phone", "deviceKind": "mobile", "deviceModel": null, "verified": true },
        "state": "completed", "fileCount": 1, "filesDone": 1, "totalBytes": 3, "bytesDone": 3, "speedBps": 0,
        "etaSecs": null, "startedAtMs": 0, "finishedAtMs": 0, "connection": null, "resumable": false,
        "title": "photo.png", "text": null, "error": null, "saveDir": save_dir.display().to_string(),
    }))
    .unwrap()
}

/// Runs Tauri's own scope matching (the check the asset protocol makes).
#[test]
fn previews_are_granted_per_received_file() {
    let app = tauri::test::mock_app();
    let scope = app.asset_protocol_scope();
    let dir = tempfile::tempdir().unwrap();
    // A folder chosen for one transfer, not the default save folder.
    let chosen = dir.path().join("Chosen");
    std::fs::create_dir_all(&chosen).unwrap();
    let write = |p: &Path| std::fs::write(p, b"png").unwrap();
    let photo = chosen.join("photo.png");
    let neighbour = chosen.join("neighbour.png");
    let outside = dir.path().join("outside.png");
    let sent = dir.path().join("sent.png");
    for p in [&photo, &neighbour, &outside, &sent] {
        write(p);
    }
    let transfers = [transfer("t1", &chosen)];
    let entries = [
        entry(1, "t1", Direction::Receive, &photo),
        // Claims transfer t1 but lies outside its save folder.
        entry(2, "t1", Direction::Receive, &outside),
        entry(3, "t2", Direction::Send, &sent),
        entry(4, "t3", Direction::Receive, &chosen.join("deleted.png")),
    ];
    let granted = grant_previews(&scope, &transfers, &entries);
    assert_eq!(granted, [photo.display().to_string()]);
    assert!(scope.is_allowed(&photo));
    for denied in [&neighbour, &outside, &sent, &chosen] {
        assert!(!scope.is_allowed(denied), "{}", denied.display());
    }

    // History restored on startup (its transfer is gone): still granted
    // when the file is there.
    let restored = entry(5, "old", Direction::Receive, &outside);
    assert_eq!(grant_previews(&scope, &[], &[restored]), [outside.display().to_string()]);
    assert!(scope.is_allowed(&outside));
    assert!(!scope.is_allowed(&neighbour));
}
