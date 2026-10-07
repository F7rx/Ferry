//! A resumed transfer keeps what the user approved: the announced checksums,
//! the offered files and the chosen save folder, whether the receiver stayed
//! up or restarted in between. Driven by a hand-made sender so the re-offers
//! can differ from the original in exactly one way.

mod common;

use common::raw::*;
use common::*;
use ferry_core::model::*;
use ferry_core::proto::PrepareUploadResponse;
use ferry_core::{Engine, Settings};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bigger than the receiver's in-memory fast path, so partial data is kept.
const SIZE: usize = 2 * 1024 * 1024 + 3;
const PREFIX: usize = 1024 * 1024 + 7;

struct Setup {
    data_dir: tempfile::TempDir,
    save_dir: tempfile::TempDir,
    decision: Decision,
    tweak: fn(&mut Settings),
    rx: Arc<Engine>,
    prompts: Vec<Arc<AtomicUsize>>,
    sender: RawSender,
}

impl Setup {
    async fn new(decision: Decision, tweak: fn(&mut Settings)) -> Setup {
        let data_dir = tempfile::tempdir().unwrap();
        let save_dir = tempfile::tempdir().unwrap();
        let rx = start_receiver(data_dir.path(), save_dir.path(), tweak).await;
        let prompts = vec![respond_all(&rx, decision.clone())];
        let sender = RawSender::new(&rx);
        Setup { data_dir, save_dir, decision, tweak, rx, prompts, sender }
    }

    /// Same device (same data folder), new process and port.
    async fn restart_with(&mut self, tweak: impl FnOnce(&mut Settings)) {
        self.rx.shutdown().await;
        let base = self.tweak;
        self.rx = start_receiver(self.data_dir.path(), self.save_dir.path(), |s| {
            base(s);
            tweak(s);
        })
        .await;
        self.prompts.push(respond_all(&self.rx, self.decision.clone()));
        self.sender.retarget(&self.rx);
    }

    async fn restart(&mut self) {
        self.restart_with(|_| {}).await;
    }

    /// Accept prompts shown since the last (re)start.
    fn new_prompts(&self) -> usize {
        self.prompts.last().unwrap().load(Ordering::SeqCst)
    }

    fn prompts(&self) -> usize {
        self.prompts.iter().map(|p| p.load(Ordering::SeqCst)).sum()
    }

    fn tamper(&self, sql: &str, params: &[&String]) {
        let conn = rusqlite::Connection::open(self.data_dir.path().join("ferry.db")).unwrap();
        conn.execute(sql, rusqlite::params_from_iter(params)).unwrap();
    }
}

fn offset(session: &PrepareUploadResponse, id: &str) -> u64 {
    session.ferry.as_ref().expect("not resumable").offsets.get(id).copied().unwrap_or(0)
}

fn transfer_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn damaged(data: &[u8], at: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    out[at] ^= 0x5a;
    out
}

async fn status(sender: &RawSender, offer: &ferry_core::proto::PrepareUploadRequest) -> u16 {
    match sender.prepare(offer).await {
        Ok(Some(_)) => 200,
        Ok(None) => 204,
        Err(e) => e.status().unwrap_or_else(|| panic!("{e}")),
    }
}

async fn approved_checksum_holds(restart: bool) {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let data = pattern(SIZE, 1);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, Some(sha256_hex(&data)))], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    if restart {
        s.restart().await;
    }

    // The re-offer leaves the checksum out: the approved one still applies.
    let reoffer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, None)], Some(&tid));
    let session = s.sender.accepted(&reoffer).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    let bad_suffix = damaged(&data, SIZE - 1);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &bad_suffix[PREFIX..]).await, 422);
    // The damaged data is gone, so the next try starts over.
    assert!(files_in(s.save_dir.path()).is_empty(), "{:?}", files_in(s.save_dir.path()));
    let session = s.sender.accepted(&reoffer).await;
    assert_eq!(offset(&session, "f"), 0);

    // A damaged prefix is caught once the file is complete, too.
    let bad_prefix = damaged(&data, 10);
    assert_eq!(s.sender.upload(&session, "f", None, &bad_prefix[..PREFIX]).await, 400);
    let session = s.sender.accepted(&reoffer).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &data[PREFIX..]).await, 422);

    // The third attempt, intact, goes through.
    let session = s.sender.accepted(&reoffer).await;
    assert_eq!(offset(&session, "f"), 0);
    assert_eq!(s.sender.upload(&session, "f", None, &data).await, 200);
    assert!(std::fs::read(s.save_dir.path().join("big.bin")).unwrap() == data);
    assert_eq!(files_in(s.save_dir.path()), ["big.bin"]);
    assert_eq!(s.prompts(), 1, "a resumed transfer never asks again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_file_is_checked_against_the_approved_checksum() {
    approved_checksum_holds(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_file_is_checked_against_the_approved_checksum() {
    approved_checksum_holds(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_attempts_stay_counted_across_a_restart() {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let data = pattern(SIZE, 2);
    let bad = damaged(&data, 5);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, Some(sha256_hex(&data)))], Some(&tid));
    for _ in 0..2 {
        let session = s.sender.accepted(&offer).await;
        assert_eq!(s.sender.upload(&session, "f", None, &bad).await, 422);
    }
    s.restart().await;
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &bad).await, 422);
    // That was the last of three attempts. (The receiver answers before
    // reading the body, so a short one avoids racing the refusal.)
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..16]).await, 403);
    assert!(files_in(s.save_dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_verification_a_restored_file_is_taken_as_is() {
    let mut s = Setup::new(Decision::accept_all(), |s| s.verify_incoming_checksums = false).await;
    let data = pattern(SIZE, 3);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, Some(sha256_hex(&data)))], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    s.restart().await;
    let session = s.sender.accepted(&offer).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    let bad = damaged(&data, SIZE - 1);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &bad[PREFIX..]).await, 200);
    assert!(std::fs::read(s.save_dir.path().join("big.bin")).unwrap() == bad);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checksum_can_not_be_added_on_resume() {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let data = pattern(SIZE, 4);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, None)], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    s.restart().await;
    let with_checksum = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, Some(sha256_hex(&data)))], Some(&tid));
    assert_eq!(status(&s.sender, &with_checksum).await, 400);
    // Approved without one: nothing to check the data against.
    let session = s.sender.accepted(&offer).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    let bad = damaged(&data, SIZE - 1);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &bad[PREFIX..]).await, 200);
    assert_eq!(s.prompts(), 1);
}

async fn changed_offers_are_refused(restart: bool) {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let data = pattern(SIZE, 5);
    let sha = sha256_hex(&data);
    let tid = transfer_id();
    let original = file_dto("f", "big.bin", SIZE as u64, Some(sha.clone()));
    let session = s.sender.accepted(&s.sender.offer(std::slice::from_ref(&original), Some(&tid))).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    if restart {
        s.restart().await;
    }

    let mut renamed = original.clone();
    renamed.file_name = "other.bin".into();
    let mut resized = original.clone();
    resized.size += 1;
    let mut retyped = original.clone();
    retyped.file_type = "text/plain".into();
    let mut rehashed = original.clone();
    rehashed.sha256 = Some(sha256_hex(b"something else"));
    let extra = file_dto("g", "extra.bin", 10, None);
    for (what, files) in [
        ("name", vec![renamed]),
        ("size", vec![resized]),
        ("type", vec![retyped]),
        ("checksum", vec![rehashed]),
        ("new file", vec![original.clone(), extra]),
    ] {
        assert_eq!(status(&s.sender, &s.sender.offer(&files, Some(&tid))).await, 400, "changed {what}");
    }

    // The approved offer still resumes where it stopped.
    let session = s.sender.accepted(&s.sender.offer(std::slice::from_ref(&original), Some(&tid))).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &data[PREFIX..]).await, 200);
    assert!(std::fs::read(s.save_dir.path().join("big.bin")).unwrap() == data);
    assert_eq!(s.prompts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_reoffer_is_refused_in_memory() {
    changed_offers_are_refused(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_reoffer_is_refused_after_restart() {
    changed_offers_are_refused(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subset_reoffer_resumes_and_declined_files_stay_declined() {
    let accept = Decision { accept: Some(vec!["a".into(), "b".into()]), ..Decision::default() };
    let mut s = Setup::new(accept, |_| {}).await;
    let a = pattern(SIZE, 6);
    let b = pattern(SIZE, 7);
    let tid = transfer_id();
    let fa = file_dto("a", "a.bin", SIZE as u64, None);
    let fb = file_dto("b", "b.bin", SIZE as u64, None);
    let fc = file_dto("c", "c.txt", 10, None);
    let session = s.sender.accepted(&s.sender.offer(&[fa, fb.clone(), fc.clone()], Some(&tid))).await;
    assert_eq!(session.files.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(s.sender.upload(&session, "a", None, &a).await, 200);
    assert_eq!(s.sender.upload(&session, "b", None, &b[..PREFIX]).await, 400);
    s.restart().await;

    // What a Ferry sender re-offers: the unfinished files, declined ones included.
    let mut changed = fc.clone();
    changed.size = 11;
    assert_eq!(status(&s.sender, &s.sender.offer(&[fb.clone(), changed], Some(&tid))).await, 400);
    let session = s.sender.accepted(&s.sender.offer(&[fb, fc], Some(&tid))).await;
    assert_eq!(session.files.keys().collect::<Vec<_>>(), ["b"], "the declined file stays declined");
    assert_eq!(offset(&session, "b"), PREFIX as u64);
    assert_eq!(s.sender.upload(&session, "b", Some(PREFIX as u64), &b[PREFIX..]).await, 200);
    assert_eq!(files_in(s.save_dir.path()), ["a.bin", "b.bin"]);
    assert_eq!(s.prompts(), 1);
}

/// What a record written before checksums, declined files and the save
/// folder were stored looks like after the schema upgrade.
fn make_legacy(s: &Setup) {
    s.tamper("UPDATE inbound_transfers SET save_root = NULL, display_root = NULL, manifest = 0", &[]);
    s.tamper("UPDATE inbound_files SET sha256 = NULL, attempts = 0", &[]);
    s.tamper("DELETE FROM inbound_declined", &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_record_resumes_in_the_default_folder_but_not_with_changes() {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let data = pattern(SIZE, 8);
    let tid = transfer_id();
    let original = file_dto("f", "Album/big.bin", SIZE as u64, None);
    let session = s.sender.accepted(&s.sender.offer(std::slice::from_ref(&original), Some(&tid))).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    s.rx.shutdown().await;
    make_legacy(&s);
    s.restart().await;

    let mut resized = original.clone();
    resized.size -= 1;
    assert_eq!(status(&s.sender, &s.sender.offer(&[resized], Some(&tid))).await, 400);
    let session = s.sender.accepted(&s.sender.offer(&[original], Some(&tid))).await;
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &data[PREFIX..]).await, 200);
    assert!(std::fs::read(s.save_dir.path().join("Album/big.bin")).unwrap() == data);
    assert_eq!(s.prompts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_record_outside_the_current_default_folder_is_asked_again() {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let elsewhere = tempfile::tempdir().unwrap();
    let data = pattern(SIZE, 9);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, None)], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    s.rx.shutdown().await;
    make_legacy(&s);
    let moved = elsewhere.path().to_path_buf();
    s.restart_with(move |settings| settings.save_dir = Some(moved)).await;

    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.new_prompts(), 1, "not resumable: the user decides again");
    assert_eq!(offset(&session, "f"), 0);
    assert_eq!(s.sender.upload(&session, "f", None, &data).await, 200);
    assert!(std::fs::read(elsewhere.path().join("big.bin")).unwrap() == data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approved_folder_is_restored_after_restart() {
    let custom = tempfile::tempdir().unwrap();
    let decision = Decision { save_dir: Some(custom.path().to_path_buf()), ..Decision::default() };
    let mut s = Setup::new(decision, |_| {}).await;
    let elsewhere = tempfile::tempdir().unwrap();
    let data = pattern(SIZE, 10);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "Album/sub/big.bin", SIZE as u64, None)], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    // The default folder changes while the transfer waits.
    let moved = elsewhere.path().to_path_buf();
    s.restart_with(move |settings| settings.save_dir = Some(moved)).await;

    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.new_prompts(), 0);
    assert_eq!(offset(&session, "f"), PREFIX as u64);
    let shown = s.rx.transfers().into_iter().find(|t| t.direction == Direction::Receive).unwrap();
    assert_eq!(shown.save_dir.as_deref().map(Path::new), Some(custom.path().join("Album").as_path()));
    assert_eq!(s.sender.upload(&session, "f", Some(PREFIX as u64), &data[PREFIX..]).await, 200);
    assert!(std::fs::read(custom.path().join("Album/sub/big.bin")).unwrap() == data);
    assert!(files_in(elsewhere.path()).is_empty());
    assert!(files_in(s.save_dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_pointing_outside_its_folder_is_not_resumed() {
    let mut s = Setup::new(Decision::accept_all(), |_| {}).await;
    let outside = tempfile::tempdir().unwrap();
    let data = pattern(SIZE, 11);
    let (t1, t2) = (transfer_id(), transfer_id());
    let o1 = s.sender.offer(&[file_dto("f", "one.bin", SIZE as u64, None)], Some(&t1));
    let o2 = s.sender.offer(&[file_dto("f", "two.bin", SIZE as u64, None)], Some(&t2));
    for o in [&o1, &o2] {
        let session = s.sender.accepted(o).await;
        assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    }
    s.rx.shutdown().await;
    // One record's partial file now points elsewhere (a file that exists),
    // the other's approved folder does.
    let planted = outside.path().join("one.bin.abc123.ferrypart");
    std::fs::write(&planted, &data[..PREFIX]).unwrap();
    let planted_str = planted.display().to_string();
    s.tamper("UPDATE inbound_files SET part_path = ?1 WHERE transfer_id = ?2", &[&planted_str, &t1]);
    let outside_str = outside.path().display().to_string();
    s.tamper("UPDATE inbound_transfers SET save_root = ?1 WHERE transfer_id = ?2", &[&outside_str, &t2]);
    let rows: i64 = rusqlite::Connection::open(s.data_dir.path().join("ferry.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM inbound_transfers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2);
    s.restart().await;

    for o in [&o1, &o2] {
        let session = s.sender.accepted(o).await;
        assert_eq!(offset(&session, "f"), 0, "a fresh transfer");
        assert_eq!(s.sender.upload(&session, "f", None, &data).await, 200);
    }
    assert_eq!(s.new_prompts(), 2, "both asked again");
    assert!(std::fs::read(&planted).unwrap() == data[..PREFIX], "nothing outside the folder is touched");
    assert_eq!(files_in(outside.path()), ["one.bin.abc123.ferrypart"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_approved_folder_is_not_recreated() {
    let custom = tempfile::tempdir().unwrap();
    let decision = Decision { save_dir: Some(custom.path().join("Inbox")), ..Decision::default() };
    let mut s = Setup::new(decision, |_| {}).await;
    let data = pattern(SIZE, 12);
    let tid = transfer_id();
    let offer = s.sender.offer(&[file_dto("f", "big.bin", SIZE as u64, None)], Some(&tid));
    let session = s.sender.accepted(&offer).await;
    assert_eq!(s.sender.upload(&session, "f", None, &data[..PREFIX]).await, 400);
    s.rx.shutdown().await;
    std::fs::remove_dir_all(custom.path().join("Inbox")).unwrap();
    s.decision = Decision::decline();
    s.restart().await;

    assert_eq!(status(&s.sender, &offer).await, 403);
    assert_eq!(s.new_prompts(), 1, "not resumable: the user decides again");
    assert!(!custom.path().join("Inbox").exists());
    assert!(s.sender.client.transfer_status(&tid).await.unwrap().is_none(), "the record is gone");
}
