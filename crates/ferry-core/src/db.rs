//! Persistent state in SQLite (WAL): known/trusted devices, history, and the
//! state of resumable incoming transfers. Settings and keys live elsewhere.

use crate::error::Result;
use crate::model::{DeviceKind, Direction, HistoryEntry, HistoryKind, HistoryStatus};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    fingerprint   TEXT PRIMARY KEY,
    alias         TEXT NOT NULL,
    custom_alias  TEXT,
    device_model  TEXT,
    device_kind   TEXT NOT NULL,
    trusted       INTEGER NOT NULL DEFAULT 0,
    favorite      INTEGER NOT NULL DEFAULT 0,
    mine          INTEGER NOT NULL DEFAULT 0,
    last_address  TEXT,
    last_port     INTEGER,
    last_protocol TEXT,
    last_seen_ms  INTEGER NOT NULL DEFAULT 0,
    is_ferry      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS history (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    transfer_id  TEXT NOT NULL,
    direction    TEXT NOT NULL,
    peer_id      TEXT NOT NULL,
    peer_alias   TEXT NOT NULL,
    peer_kind    TEXT NOT NULL,
    kind         TEXT NOT NULL,
    name         TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mime         TEXT NOT NULL,
    path         TEXT,
    text         TEXT,
    timestamp_ms INTEGER NOT NULL,
    status       TEXT NOT NULL,
    verified     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS history_by_time ON history(timestamp_ms DESC);

CREATE TABLE IF NOT EXISTS inbound_transfers (
    peer_fingerprint TEXT NOT NULL,
    transfer_id      TEXT NOT NULL,
    peer_alias       TEXT NOT NULL,
    created_ms       INTEGER NOT NULL,
    updated_ms       INTEGER NOT NULL,
    PRIMARY KEY (peer_fingerprint, transfer_id)
);

CREATE TABLE IF NOT EXISTS inbound_files (
    peer_fingerprint TEXT NOT NULL,
    transfer_id      TEXT NOT NULL,
    file_id          TEXT NOT NULL,
    rel_name         TEXT NOT NULL,
    size             INTEGER NOT NULL,
    mime             TEXT NOT NULL,
    part_path        TEXT NOT NULL,
    final_path       TEXT,
    offset           INTEGER NOT NULL DEFAULT 0,
    done             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (peer_fingerprint, transfer_id, file_id),
    FOREIGN KEY (peer_fingerprint, transfer_id)
        REFERENCES inbound_transfers(peer_fingerprint, transfer_id) ON DELETE CASCADE
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KnownDevice {
    pub fingerprint: String,
    pub alias: String,
    pub custom_alias: Option<String>,
    pub device_model: Option<String>,
    pub device_kind: DeviceKind,
    pub trusted: bool,
    pub favorite: bool,
    pub mine: bool,
    pub last_address: Option<String>,
    pub last_port: Option<u16>,
    pub last_protocol: Option<String>,
    pub last_seen_ms: u64,
    pub is_ferry: bool,
}

#[derive(Clone, Debug)]
pub struct NewHistoryEntry {
    pub transfer_id: String,
    pub direction: Direction,
    pub peer_id: String,
    pub peer_alias: String,
    pub peer_kind: DeviceKind,
    pub kind: HistoryKind,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub path: Option<String>,
    pub text: Option<String>,
    pub timestamp_ms: u64,
    pub status: HistoryStatus,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboundFileRecord {
    pub file_id: String,
    pub rel_name: String,
    pub size: u64,
    pub mime: String,
    pub part_path: String,
    pub final_path: Option<String>,
    pub offset: u64,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboundRecord {
    pub peer_fingerprint: String,
    pub transfer_id: String,
    pub peer_alias: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub files: Vec<InboundFileRecord>,
}

fn enum_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn enum_parse<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        Ok(Db { conn: Mutex::new(conn) })
    }

    // ── Devices ──────────────────────────────────────────────────────────

    pub fn known_devices(&self) -> Result<Vec<KnownDevice>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT fingerprint, alias, custom_alias, device_model, device_kind, trusted, favorite, mine,
                    last_address, last_port, last_protocol, last_seen_ms, is_ferry FROM devices",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(KnownDevice {
                fingerprint: r.get(0)?,
                alias: r.get(1)?,
                custom_alias: r.get(2)?,
                device_model: r.get(3)?,
                device_kind: enum_parse(&r.get::<_, String>(4)?).unwrap_or_default(),
                trusted: r.get(5)?,
                favorite: r.get(6)?,
                mine: r.get(7)?,
                last_address: r.get(8)?,
                last_port: r.get::<_, Option<i64>>(9)?.map(|p| p as u16),
                last_protocol: r.get(10)?,
                last_seen_ms: r.get::<_, i64>(11)? as u64,
                is_ferry: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn upsert_device(&self, d: &KnownDevice) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO devices (fingerprint, alias, custom_alias, device_model, device_kind, trusted, favorite, mine,
                                  last_address, last_port, last_protocol, last_seen_ms, is_ferry)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(fingerprint) DO UPDATE SET
                alias = excluded.alias, custom_alias = excluded.custom_alias, device_model = excluded.device_model,
                device_kind = excluded.device_kind, trusted = excluded.trusted, favorite = excluded.favorite,
                mine = excluded.mine, last_address = excluded.last_address, last_port = excluded.last_port,
                last_protocol = excluded.last_protocol, last_seen_ms = excluded.last_seen_ms, is_ferry = excluded.is_ferry",
            params![
                d.fingerprint,
                d.alias,
                d.custom_alias,
                d.device_model,
                enum_str(&d.device_kind),
                d.trusted,
                d.favorite,
                d.mine,
                d.last_address,
                d.last_port.map(|p| p as i64),
                d.last_protocol,
                d.last_seen_ms as i64,
                d.is_ferry,
            ],
        )?;
        Ok(())
    }

    pub fn forget_device(&self, fingerprint: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM devices WHERE fingerprint = ?1", [fingerprint])?;
        Ok(())
    }

    // ── History ──────────────────────────────────────────────────────────

    pub fn add_history(&self, e: &NewHistoryEntry) -> Result<HistoryEntry> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO history (transfer_id, direction, peer_id, peer_alias, peer_kind, kind, name, size, mime,
                                  path, text, timestamp_ms, status, verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                e.transfer_id,
                enum_str(&e.direction),
                e.peer_id,
                e.peer_alias,
                enum_str(&e.peer_kind),
                enum_str(&e.kind),
                e.name,
                e.size as i64,
                e.mime,
                e.path,
                e.text,
                e.timestamp_ms as i64,
                enum_str(&e.status),
                e.verified,
            ],
        )?;
        let id = conn.last_insert_rowid();
        Ok(HistoryEntry {
            id,
            transfer_id: e.transfer_id.clone(),
            direction: e.direction,
            peer_id: e.peer_id.clone(),
            peer_alias: e.peer_alias.clone(),
            peer_kind: e.peer_kind,
            kind: e.kind,
            name: e.name.clone(),
            size: e.size,
            mime: e.mime.clone(),
            path: e.path.clone(),
            text: e.text.clone(),
            timestamp_ms: e.timestamp_ms,
            status: e.status,
            verified: e.verified,
        })
    }

    /// Newest first. `before_id` pages backwards.
    pub fn history(&self, limit: u32, before_id: Option<i64>, direction: Option<Direction>) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, transfer_id, direction, peer_id, peer_alias, peer_kind, kind, name, size, mime, path, text,
                    timestamp_ms, status, verified
             FROM history
             WHERE (?1 IS NULL OR id < ?1) AND (?2 IS NULL OR direction = ?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let dir = direction.map(|d| enum_str(&d));
        let rows = stmt.query_map(params![before_id, dir, limit as i64], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                transfer_id: r.get(1)?,
                direction: enum_parse(&r.get::<_, String>(2)?).unwrap_or(Direction::Receive),
                peer_id: r.get(3)?,
                peer_alias: r.get(4)?,
                peer_kind: enum_parse(&r.get::<_, String>(5)?).unwrap_or_default(),
                kind: enum_parse(&r.get::<_, String>(6)?).unwrap_or(HistoryKind::File),
                name: r.get(7)?,
                size: r.get::<_, i64>(8)? as u64,
                mime: r.get(9)?,
                path: r.get(10)?,
                text: r.get(11)?,
                timestamp_ms: r.get::<_, i64>(12)? as u64,
                status: enum_parse(&r.get::<_, String>(13)?).unwrap_or(HistoryStatus::Completed),
                verified: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_history(&self, id: i64) -> Result<bool> {
        Ok(self.conn.lock().unwrap().execute("DELETE FROM history WHERE id = ?1", [id])? > 0)
    }

    pub fn history_has_path(&self, path: &str) -> Result<bool> {
        let n: i64 = self.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM history WHERE path = ?1", [path], |r| r.get(0))?;
        Ok(n > 0)
    }

    pub fn set_history_verified(&self, id: i64) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE history SET verified = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM history", [])?;
        Ok(())
    }

    // ── Resumable inbound transfers ──────────────────────────────────────

    pub fn save_inbound(&self, record: &InboundRecord) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO inbound_transfers (peer_fingerprint, transfer_id, peer_alias, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record.peer_fingerprint, record.transfer_id, record.peer_alias, record.created_ms as i64, record.updated_ms as i64],
        )?;
        for f in &record.files {
            tx.execute(
                "INSERT OR REPLACE INTO inbound_files
                    (peer_fingerprint, transfer_id, file_id, rel_name, size, mime, part_path, final_path, offset, done)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    record.peer_fingerprint,
                    record.transfer_id,
                    f.file_id,
                    f.rel_name,
                    f.size as i64,
                    f.mime,
                    f.part_path,
                    f.final_path,
                    f.offset as i64,
                    f.done
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_inbound(&self, peer_fingerprint: &str, transfer_id: &str) -> Result<Option<InboundRecord>> {
        let conn = self.conn.lock().unwrap();
        let head = conn
            .query_row(
                "SELECT peer_alias, created_ms, updated_ms FROM inbound_transfers
                 WHERE peer_fingerprint = ?1 AND transfer_id = ?2",
                params![peer_fingerprint, transfer_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()?;
        let Some((peer_alias, created_ms, updated_ms)) = head else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT file_id, rel_name, size, mime, part_path, final_path, offset, done FROM inbound_files
             WHERE peer_fingerprint = ?1 AND transfer_id = ?2",
        )?;
        let files = stmt
            .query_map(params![peer_fingerprint, transfer_id], |r| {
                Ok(InboundFileRecord {
                    file_id: r.get(0)?,
                    rel_name: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mime: r.get(3)?,
                    part_path: r.get(4)?,
                    final_path: r.get(5)?,
                    offset: r.get::<_, i64>(6)? as u64,
                    done: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(InboundRecord {
            peer_fingerprint: peer_fingerprint.to_string(),
            transfer_id: transfer_id.to_string(),
            peer_alias,
            created_ms: created_ms as u64,
            updated_ms: updated_ms as u64,
            files,
        }))
    }

    pub fn update_inbound_file(
        &self,
        peer_fingerprint: &str,
        transfer_id: &str,
        file_id: &str,
        offset: u64,
        done: bool,
        final_path: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE inbound_files SET offset = ?4, done = ?5, final_path = COALESCE(?6, final_path)
             WHERE peer_fingerprint = ?1 AND transfer_id = ?2 AND file_id = ?3",
            params![peer_fingerprint, transfer_id, file_id, offset as i64, done, final_path],
        )?;
        conn.execute(
            "UPDATE inbound_transfers SET updated_ms = ?3 WHERE peer_fingerprint = ?1 AND transfer_id = ?2",
            params![peer_fingerprint, transfer_id, crate::util::now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn delete_inbound(&self, peer_fingerprint: &str, transfer_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM inbound_transfers WHERE peer_fingerprint = ?1 AND transfer_id = ?2",
            params![peer_fingerprint, transfer_id],
        )?;
        Ok(())
    }

    /// Removes resumable transfers idle since before `cutoff_ms`; returns
    /// their unfinished part files so the caller can delete them.
    pub fn expire_inbound(&self, cutoff_ms: u64) -> Result<Vec<String>> {
        // SQLite integers are signed; never let a huge cutoff wrap negative.
        let cutoff_ms = cutoff_ms.min(i64::MAX as u64);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT f.part_path FROM inbound_files f JOIN inbound_transfers t
               ON f.peer_fingerprint = t.peer_fingerprint AND f.transfer_id = t.transfer_id
             WHERE t.updated_ms < ?1 AND f.done = 0",
        )?;
        let parts = stmt.query_map([cutoff_ms as i64], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        conn.execute("DELETE FROM inbound_transfers WHERE updated_ms < ?1", [cutoff_ms as i64])?;
        Ok(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(fp: &str) -> KnownDevice {
        KnownDevice {
            fingerprint: fp.into(),
            alias: "Laptop".into(),
            custom_alias: None,
            device_model: Some("Windows".into()),
            device_kind: DeviceKind::Desktop,
            trusted: true,
            favorite: false,
            mine: false,
            last_address: Some("192.168.1.5".into()),
            last_port: Some(53317),
            last_protocol: Some("https".into()),
            last_seen_ms: 42,
            is_ferry: true,
        }
    }

    #[test]
    fn devices_upsert_and_forget() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_device(&device("AA")).unwrap();
        let mut d = device("AA");
        d.alias = "Renamed".into();
        db.upsert_device(&d).unwrap();
        let all = db.known_devices().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].alias, "Renamed");
        db.forget_device("AA").unwrap();
        assert!(db.known_devices().unwrap().is_empty());
    }

    #[test]
    fn history_pages_newest_first() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..5 {
            db.add_history(&NewHistoryEntry {
                transfer_id: "t".into(),
                direction: if i % 2 == 0 { Direction::Receive } else { Direction::Send },
                peer_id: "p".into(),
                peer_alias: "Phone".into(),
                peer_kind: DeviceKind::Mobile,
                kind: HistoryKind::File,
                name: format!("f{i}"),
                size: i,
                mime: "text/plain".into(),
                path: None,
                text: None,
                timestamp_ms: i,
                status: HistoryStatus::Completed,
                verified: true,
            })
            .unwrap();
        }
        let page = db.history(2, None, None).unwrap();
        assert_eq!(page.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["f4", "f3"]);
        let next = db.history(10, Some(page[1].id), None).unwrap();
        assert_eq!(next.len(), 3);
        let received = db.history(10, None, Some(Direction::Receive)).unwrap();
        assert_eq!(received.len(), 3);
        assert!(db.delete_history(page[0].id).unwrap());
        assert_eq!(db.history(10, None, None).unwrap().len(), 4);
    }

    #[test]
    fn inbound_resume_state_round_trips_and_expires() {
        let db = Db::open_in_memory().unwrap();
        let record = InboundRecord {
            peer_fingerprint: "FP".into(),
            transfer_id: "T1".into(),
            peer_alias: "Laptop".into(),
            created_ms: 1,
            updated_ms: 1,
            files: vec![InboundFileRecord {
                file_id: "f0".into(),
                rel_name: "big.iso".into(),
                size: 100,
                mime: "application/octet-stream".into(),
                part_path: "/tmp/big.iso.ferrypart".into(),
                final_path: None,
                offset: 0,
                done: false,
            }],
        };
        db.save_inbound(&record).unwrap();
        db.update_inbound_file("FP", "T1", "f0", 64, false, None).unwrap();
        let loaded = db.load_inbound("FP", "T1").unwrap().unwrap();
        assert_eq!(loaded.files[0].offset, 64);
        assert!(db.load_inbound("OTHER", "T1").unwrap().is_none());

        let parts = db.expire_inbound(u64::MAX).unwrap();
        assert_eq!(parts, vec!["/tmp/big.iso.ferrypart".to_string()]);
        assert!(db.load_inbound("FP", "T1").unwrap().is_none());
    }
}
