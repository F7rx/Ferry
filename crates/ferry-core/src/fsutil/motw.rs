//! Mark-of-the-Web: received files carry the same "came from the network"
//! zone tag a browser download gets, so SmartScreen, Office Protected View
//! and script hosts treat them with the usual caution.

use std::path::Path;

/// Zone 3 ("Internet"), as browsers use for downloads. LAN peers aren't
/// verified publishers either, so we don't claim the Intranet zone.
#[cfg(windows)]
const ZONE_IDENTIFIER: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

/// Tags `path` as downloaded. Best effort: volumes without alternate data
/// streams (FAT32/exFAT drives, some network shares) silently keep no tag.
#[cfg(windows)]
pub fn mark_from_network(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(err) = std::fs::write(&stream, ZONE_IDENTIFIER) {
        tracing::debug!("No Mark-of-the-Web on {}: {err}", path.display());
    }
}

#[cfg(not(windows))]
pub fn mark_from_network(_path: &Path) {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn tag_is_written_and_survives_rename() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("report.pdf.ferrypart");
        std::fs::write(&part, b"%PDF-1.7").unwrap();
        mark_from_network(&part);
        let done = dir.path().join("report.pdf");
        std::fs::rename(&part, &done).unwrap();
        let tag = std::fs::read(format!("{}:Zone.Identifier", done.display())).unwrap();
        assert_eq!(tag, ZONE_IDENTIFIER);
        assert_eq!(std::fs::read(&done).unwrap(), b"%PDF-1.7", "file contents untouched");
    }
}
