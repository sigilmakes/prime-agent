//! The required behavior list from the session-log spec, one test per item.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::disk::{Disk, RealDisk};
use super::line::MAX_LINE_BYTES;
use super::{Error, Holder, NotWritten, Outcome, RepairReport, SessionLog};

const ID: &str = "01a18af1-session";

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn row(n: u64) -> serde_json::Value {
    json!({ "kind": "test", "n": n })
}

fn committed_seq(outcome: &Outcome) -> Option<u64> {
    match outcome {
        Outcome::Committed { seq, .. } => Some(*seq),
        _ => None,
    }
}

fn log_path(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{ID}.jsonl"))
}

type FsyncHook = Box<dyn Fn(&File) -> io::Result<()> + Send + Sync>;
type DirHook = Box<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type WriteHook = Box<dyn Fn(&mut File, &[u8]) -> io::Result<()> + Send + Sync>;

/// The real disk with test hooks. The default hooks skip fsync: durability is
/// proven by the tests that install failing hooks, not by repetition here.
struct TestDisk {
    disk: RealDisk,
    fsync: FsyncHook,
    sync_dir: DirHook,
    write: WriteHook,
}

impl TestDisk {
    fn new() -> Self {
        Self {
            disk: RealDisk,
            fsync: Box::new(|_| Ok(())),
            sync_dir: Box::new(|_| Ok(())),
            write: Box::new(File::write_all),
        }
    }

    fn fsync<F>(mut self, hook: F) -> Self
    where
        F: Fn(&File) -> io::Result<()> + Send + Sync + 'static,
    {
        self.fsync = Box::new(hook);
        self
    }

    fn sync_dir<F>(mut self, hook: F) -> Self
    where
        F: Fn(&Path) -> io::Result<()> + Send + Sync + 'static,
    {
        self.sync_dir = Box::new(hook);
        self
    }

    fn write<F>(mut self, hook: F) -> Self
    where
        F: Fn(&mut File, &[u8]) -> io::Result<()> + Send + Sync + 'static,
    {
        self.write = Box::new(hook);
        self
    }
}

impl Disk for TestDisk {
    fn open_log(&self, path: &Path) -> io::Result<(File, bool)> {
        self.disk.open_log(path)
    }
    fn open_lock(&self, path: &Path) -> io::Result<File> {
        self.disk.open_lock(path)
    }
    fn open_rw(&self, path: &Path) -> io::Result<File> {
        self.disk.open_rw(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<File> {
        self.disk.open_read(path)
    }
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        (self.write)(file, bytes)
    }
    fn fsync(&self, file: &File) -> io::Result<()> {
        (self.fsync)(file)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        (self.sync_dir)(dir)
    }
    fn len(&self, file: &File) -> io::Result<u64> {
        self.disk.len(file)
    }
    fn truncate(&self, file: &File, len: u64) -> io::Result<()> {
        self.disk.truncate(file, len)
    }
    fn try_lock(&self, file: &File) -> io::Result<bool> {
        self.disk.try_lock(file)
    }
    fn unlock(&self, file: &File) -> io::Result<()> {
        self.disk.unlock(file)
    }
}

#[test]
fn append_and_read_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));

    let rows = log.read_since(0).unwrap();
    assert_eq!(rows.iter().map(|row| row.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(rows[0].body, row(1));
    assert_eq!(rows[0].ts.len(), 24, "YYYY-MM-DDTHH:MM:SS.mmmZ");
    assert!(rows[0].ts.ends_with('Z'));

    let tail = log.read_since(1).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 2);
}

#[test]
fn a_killed_writer_leaves_a_torn_tail_that_repair_truncates() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
    let good = std::fs::metadata(&path).unwrap().len();

    // What a writer killed mid-line leaves behind: no terminating newline.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(
        br#"{"seq":3,"ts":"2026-10-30T04:00:00.000Z","crc":"00000000","row":{"kind":"test","#,
    )
    .unwrap();
    file.sync_all().unwrap();
    drop(file);
    let torn = std::fs::metadata(&path).unwrap().len() - good;

    let reopened = SessionLog::open(temp.path(), ID, deadline());
    assert!(
        matches!(
            reopened,
            Err(Error::Damaged {
                last_good_seq: 2,
                ..
            })
        ),
        "a torn tail must be reported, not silently kept"
    );

    let report = SessionLog::repair(temp.path(), ID, deadline()).unwrap();
    assert_eq!(
        report,
        RepairReport {
            last_good_seq: 2,
            truncated_bytes: torn,
            rows: 2,
        }
    );

    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(3), deadline())), Some(3));
    assert_eq!(log.read_since(0).unwrap().len(), 3);
}

#[test]
fn reading_stops_at_a_torn_tail_without_repairing() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"seq\":3").unwrap();
    drop(file);

    assert_eq!(log.read_since(0).unwrap().len(), 2);
    assert!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .truncated_bytes
            > 0
    );
}

#[test]
fn an_interrupted_write_is_provably_not_written() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let disk: Arc<dyn Disk> = {
        let interrupt_once = Arc::new(AtomicBool::new(true));
        Arc::new(TestDisk::new().write(move |file, bytes| {
            if interrupt_once.swap(false, Ordering::SeqCst) {
                file.write_all(&bytes[..bytes.len() / 2])?;
                return Err(io::Error::other("write interrupted"));
            }
            file.write_all(bytes)
        }))
    };
    let mut log = SessionLog::open_with(disk, temp.path(), ID, deadline()).unwrap();
    let outcome = log.append(&row(1), deadline());
    assert!(
        matches!(
            outcome,
            Outcome::NotWritten {
                reason: NotWritten::Io(_)
            }
        ),
        "a half line cannot be a row: {outcome:?}"
    );

    let torn = std::fs::metadata(&path).unwrap().len();
    assert!(torn > 0, "the interrupted bytes are on disk");
    let report = SessionLog::repair(temp.path(), ID, deadline()).unwrap();
    assert_eq!(
        report,
        RepairReport {
            last_good_seq: 0,
            truncated_bytes: torn,
            rows: 0,
        }
    );
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
}

#[test]
fn a_second_writer_is_refused_at_the_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    let lock_path = temp.path().join(format!("{ID}.lock"));
    // A real second flock holder: same file, its own open file description.
    let rival = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    rival.lock().unwrap();

    let until = deadline();
    match log.append(&row(1), until) {
        Outcome::Busy {
            until: reported, ..
        } => assert_eq!(reported, until),
        other => panic!("expected Busy, got {other:?}"),
    }
    assert!(log.read_since(0).unwrap().is_empty());

    rival.unlock().unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
}

#[test]
fn two_writers_never_share_a_seq() {
    let temp = tempfile::tempdir().unwrap();
    let mut left = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    let mut right = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    for n in 0..5u64 {
        assert_eq!(
            committed_seq(&left.append(&row(n), deadline())),
            Some(2 * n + 1)
        );
        assert_eq!(
            committed_seq(&right.append(&row(n), deadline())),
            Some(2 * n + 2)
        );
    }
    let rows = left.read_since(0).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<_>>()
    );
}

#[test]
fn a_crash_before_the_directory_sync_leaves_no_partial_row() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let lossy: Arc<dyn Disk> = Arc::new(
        TestDisk::new().sync_dir(|_| Err(io::Error::other("power lost before the directory sync"))),
    );
    assert!(matches!(
        SessionLog::open_with(lossy, temp.path(), ID, deadline()),
        Err(Error::Io(_))
    ));
    // Whatever survived the lost entry is a complete log: absent or empty.
    if let Ok(bytes) = std::fs::read(&path) {
        assert!(bytes.is_empty(), "the file must not hold a partial row");
    }

    // The entry was lost with the crash: the next open starts a fresh log.
    let _ = std::fs::remove_file(&path);
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    assert_eq!(log.read_since(0).unwrap().len(), 1);
}

#[test]
fn a_failed_fsync_is_undecided_and_recover_never_lies() {
    for bytes_survive in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let fail_once = Arc::new(AtomicBool::new(true));
        let disk: Arc<dyn Disk> = {
            let fail_once = fail_once.clone();
            Arc::new(TestDisk::new().fsync(move |file| {
                // The empty file's create-time barrier succeeds; the barrier
                // for the row's bytes fails, once.
                let pending = file.metadata()?.len() > 0;
                if pending && fail_once.swap(false, Ordering::SeqCst) {
                    if !bytes_survive {
                        // An unflushed write that dies with the failure.
                        file.set_len(0)?;
                    }
                    return Err(io::Error::other("fsync failed"));
                }
                crate::platform::fsync(file)
            }))
        };
        let mut log = SessionLog::open_with(disk, temp.path(), ID, deadline()).unwrap();
        let outcome = log.append(&row(1), deadline());
        let Outcome::Unknown { recover } = outcome else {
            panic!("a failed barrier is Unknown, got {outcome:?}");
        };
        assert_eq!(recover.seq(), 1);
        let resolved = recover.recover(deadline()).unwrap();

        // What the log actually holds, read without the injected disk.
        let rows = SessionLog::open(temp.path(), ID, deadline())
            .unwrap()
            .read_since(0)
            .unwrap();
        match &resolved {
            Outcome::Committed { seq, .. } => {
                assert_eq!(*seq, 1);
                assert_eq!(rows.len(), 1, "Committed must mean the row is there");
            }
            Outcome::NotWritten {
                reason: NotWritten::Absent { seq },
            } => {
                assert_eq!(*seq, 1);
                assert!(rows.is_empty(), "NotWritten must mean the row is not there");
            }
            other => panic!("recover must decide, got {other:?}"),
        }
        assert_eq!(
            matches!(resolved, Outcome::Committed { .. }),
            bytes_survive,
            "recover agrees with what is on disk"
        );
        // The retry pins the seq the recover answer named, so the log still
        // holds exactly one row: this is the no-ACK idempotency.
        assert_eq!(
            committed_seq(&log.append_at(1, &row(1), deadline())),
            Some(1)
        );
        let after = SessionLog::open(temp.path(), ID, deadline())
            .unwrap()
            .read_since(0)
            .unwrap();
        assert_eq!(after.len(), 1, "the retry added no second row");
    }
}

#[test]
fn a_duplicate_retry_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(
        committed_seq(&log.append_at(7, &row(7), deadline())),
        Some(7)
    );
    assert_eq!(
        committed_seq(&log.append_at(7, &row(7), deadline())),
        Some(7)
    );
    let rows = log.read_since(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 7);

    let taken = log.append_at(7, &row(99), deadline());
    assert!(
        matches!(
            taken,
            Outcome::NotWritten {
                reason: NotWritten::SeqConflict { seq: 7 }
            }
        ),
        "a different row at the same seq is not a retry: {taken:?}"
    );
    assert_eq!(log.read_since(0).unwrap().len(), 1);
}

#[test]
fn a_flipped_byte_stops_the_read_and_is_reported_by_repair() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    for n in 1..=3u64 {
        assert_eq!(committed_seq(&log.append(&row(n), deadline())), Some(n));
    }

    let mut bytes = std::fs::read(&path).unwrap();
    let target = bytes
        .windows(6)
        .position(|window| window == br#""n":2}"#)
        .expect("the second row's body")
        + 4;
    bytes[target] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    // The stream delivers the rows before the damage, then the error.
    let collected: Vec<_> = log.iter_since(0).unwrap().collect();
    assert_eq!(collected.len(), 2, "one row, then the verdict");
    assert_eq!(collected[0].as_ref().unwrap().seq, 1);
    assert!(matches!(collected[1], Err(Error::Corrupt { after_seq: 1 })));
    // The Vec form is all-or-error: the caller repairs instead of guessing.
    assert!(matches!(
        log.read_since(0),
        Err(Error::Corrupt { after_seq: 1 })
    ));

    let report = SessionLog::repair(temp.path(), ID, deadline()).unwrap();
    assert_eq!(report.last_good_seq, 1);
    assert_eq!(report.rows, 1);
    assert!(report.truncated_bytes > 0, "repair reports the damage");
}

#[test]
fn a_flipped_seq_is_caught_by_the_checksum() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    for n in 1..=3u64 {
        assert_eq!(committed_seq(&log.append(&row(n), deadline())), Some(n));
    }

    // The last row's seq, raised past every later seq: the order still
    // increases, so only the checksum can catch it.
    let mut bytes = std::fs::read(&path).unwrap();
    let target = bytes
        .windows(8)
        .position(|window| window == br#""seq":3,"#)
        .expect("the third row's seq")
        + 6;
    assert_eq!(bytes[target], b'3');
    bytes[target] = b'5';
    std::fs::write(&path, &bytes).unwrap();

    assert!(matches!(
        log.read_since(0),
        Err(Error::Corrupt { after_seq: 2 })
    ));
    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .last_good_seq,
        2
    );
}

#[test]
fn a_creator_crash_before_the_directory_sync_is_repaired_by_the_next_open() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    // The creator dies at its directory sync, so the entry is never durable.
    let lossy: Arc<dyn Disk> = Arc::new(
        TestDisk::new().sync_dir(|_| Err(io::Error::other("power lost before the directory sync"))),
    );
    assert!(matches!(
        SessionLog::open_with(lossy, temp.path(), ID, deadline()),
        Err(Error::Io(_))
    ));
    assert!(
        path.exists(),
        "the creator's entry is visible to later opens"
    );

    // Every later open sees `created == false`; the sync must not depend on it.
    let syncs = Arc::new(AtomicUsize::new(0));
    let counter: Arc<dyn Disk> = {
        let syncs = syncs.clone();
        Arc::new(TestDisk::new().sync_dir(move |_| {
            syncs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
    };
    let mut log = SessionLog::open_with(counter, temp.path(), ID, deadline()).unwrap();
    assert_eq!(
        syncs.load(Ordering::SeqCst),
        1,
        "the reopening writer syncs the entry"
    );
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
}

#[test]
fn the_busy_holder_record_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let lock_path = temp.path().join(format!("{ID}.lock"));
    let test_started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();

    // Taking the lock writes the holder's note.
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    let read_note = || {
        let note = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, since_ms) = note.trim().split_once(' ').expect("pid and start time");
        (
            pid.parse::<u32>().unwrap(),
            since_ms.parse::<u64>().unwrap(),
        )
    };
    let (pid, since_ms) = read_note();
    assert_eq!(pid, std::process::id());
    assert!(since_ms > 0);
    // A start is not in the future, up to one `/proc` tick of precision.
    assert!(
        since_ms <= test_started_ms + 20,
        "a process start cannot follow the note that records it: {since_ms} vs {test_started_ms}"
    );
    // And it is stable across acquisitions, unlike an acquisition time: the
    // recovered start is only as fine as `/proc/uptime`'s hundredths, so allow
    // one tick of sampling difference.
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
    let (later_pid, later_since) = read_note();
    assert_eq!(later_pid, pid);
    assert!(
        since_ms.abs_diff(later_since) <= 20,
        "stable start times: {since_ms} vs {later_since}"
    );

    // A foreign holder's note is what a refused writer reads back.
    let rival = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    rival.lock().unwrap();
    std::fs::write(&lock_path, "4242 1700000000123\n").unwrap();
    match log.append(&row(3), deadline()) {
        Outcome::Busy {
            holder: Some(holder),
            ..
        } => assert_eq!(
            holder,
            Holder {
                pid: 4242,
                since_ms: 1_700_000_000_123,
            }
        ),
        other => panic!("expected Busy with the holder's note, got {other:?}"),
    }
    rival.unlock().unwrap();
    assert_eq!(committed_seq(&log.append(&row(3), deadline())), Some(3));
}

#[test]
fn an_oversized_row_is_refused_before_it_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    let before = std::fs::read(&path).unwrap();

    let oversized = json!({ "kind": "test", "blob": "x".repeat(MAX_LINE_BYTES) });
    match log.append(&oversized, deadline()) {
        Outcome::NotWritten {
            reason: NotWritten::TooLong { bytes },
        } => assert!(bytes > MAX_LINE_BYTES as u64, "{bytes}"),
        other => panic!("expected TooLong, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the log is byte-identical: nothing was written"
    );
    // A refused row must not look like damage to the next reader or repair.
    assert_eq!(log.read_since(0).unwrap().len(), 1);
    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline()).unwrap(),
        RepairReport {
            last_good_seq: 1,
            truncated_bytes: 0,
            rows: 1,
        }
    );
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
}

#[test]
fn an_append_behind_a_corrupt_tail_is_refused_with_its_own_reason() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));

    // A foreign writer leaves a *complete* line with a bad checksum behind the
    // tail: appending after it would bury a row behind damage.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(
        b"{\"seq\":3,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"00000000\",\"row\":{\"kind\":\"test\",\"n\":3}}\n",
    )
    .unwrap();
    file.sync_all().unwrap();
    drop(file);

    let outcome = log.append(&row(4), deadline());
    assert!(
        matches!(
            outcome,
            Outcome::NotWritten {
                reason: NotWritten::Corrupt { after_seq: 2 }
            }
        ),
        "a complete bad line is Corrupt, not a torn tail: {outcome:?}"
    );
    // The refusal is still a refusal: the log is untouched until repair.
    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .last_good_seq,
        2
    );
    assert_eq!(committed_seq(&log.append(&row(4), deadline())), Some(3));
}

#[test]
fn seq_keeps_increasing_across_handles() {
    let temp = tempfile::tempdir().unwrap();
    let mut first = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&first.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&first.append(&row(2), deadline())), Some(2));
    drop(first);

    let mut second = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&second.append(&row(3), deadline())), Some(3));
    assert_eq!(second.read_since(1).unwrap().len(), 2);
}

#[test]
fn refresh_reads_only_the_new_tail_and_advances_the_cursor() {
    let temp = tempfile::tempdir().unwrap();
    // The seam's default hooks skip fsync: this test is about the cursor, and
    // 10k durable appends would dominate the suite's runtime.
    let disk: Arc<dyn Disk> = Arc::new(TestDisk::new());
    let mut follower = SessionLog::open_with(disk.clone(), temp.path(), ID, deadline()).unwrap();
    let mut writer = SessionLog::open_with(disk, temp.path(), ID, deadline()).unwrap();
    for n in 1..=10_000u64 {
        assert_eq!(committed_seq(&writer.append(&row(n), deadline())), Some(n));
    }

    // One scan adopts the whole delta, in order.
    let adopted = follower.refresh(deadline()).unwrap();
    assert_eq!(adopted.len(), 10_000);
    assert_eq!(adopted[0].seq, 1);
    assert_eq!(adopted[9_999].seq, 10_000);
    assert!(follower.refresh(deadline()).unwrap().is_empty());

    // A foreign append is read as a delta, not as a rescan of the log.
    assert_eq!(
        committed_seq(&writer.append(&row(10_001), deadline())),
        Some(10_001)
    );
    let delta = follower.refresh(deadline()).unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].seq, 10_001);
    // The follower's own append continues from the adopted tail.
    assert_eq!(
        committed_seq(&follower.append(&row(10_002), deadline())),
        Some(10_002)
    );

    // An idle refresh is a length check: if it rescanned 10k rows, 500 of them
    // would take tens of seconds.
    let started = Instant::now();
    for _ in 0..500 {
        assert!(follower.refresh(deadline()).unwrap().is_empty());
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "idle refreshes took {:?}",
        started.elapsed()
    );
}

#[test]
fn refresh_reports_damage_the_way_an_append_does() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut writer = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&writer.append(&row(1), deadline())), Some(1));
    assert_eq!(committed_seq(&writer.append(&row(2), deadline())), Some(2));
    let mut follower = SessionLog::open(temp.path(), ID, deadline()).unwrap();

    // A torn tail: the cursor stays on the last complete line.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"seq\":3").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(matches!(
        follower.refresh(deadline()),
        Err(Error::Damaged {
            last_good_seq: 2,
            ..
        })
    ));
    assert!(
        follower.refresh(deadline()).is_err(),
        "still torn until repaired"
    );
    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .last_good_seq,
        2
    );
    assert!(follower.refresh(deadline()).unwrap().is_empty());

    // A complete bad line is Corrupt, and it also stops the cursor before it.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(
        b"{\"seq\":3,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"00000000\",\"row\":{\"kind\":\"test\",\"n\":3}}\n",
    )
    .unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(matches!(
        follower.refresh(deadline()),
        Err(Error::Corrupt { after_seq: 2 })
    ));
    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .last_good_seq,
        2
    );
    assert_eq!(
        committed_seq(&follower.append(&row(3), deadline())),
        Some(3)
    );
}

#[test]
fn a_hundred_thousand_rows_stay_usable() {
    let temp = tempfile::tempdir().unwrap();
    let disk: Arc<dyn Disk> = Arc::new(TestDisk::new());
    let mut log = SessionLog::open_with(disk, temp.path(), ID, deadline()).unwrap();
    let started = Instant::now();
    for n in 0..100_000u64 {
        assert_eq!(committed_seq(&log.append(&row(n), deadline())), Some(n + 1));
    }
    let appended = started.elapsed();

    let tail = log.read_since(99_990).unwrap();
    assert_eq!(tail.len(), 10);
    assert_eq!(tail[9].seq, 100_000);

    // The streaming read holds one row at a time; the count is the proof.
    let mut streamed = 0u64;
    for row in log.iter_since(0).unwrap() {
        assert_eq!(row.unwrap().seq, streamed + 1);
        streamed += 1;
    }
    assert_eq!(streamed, 100_000);
    assert!(
        appended < Duration::from_secs(60),
        "appends took {appended:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(180),
        "appends and reads took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_row_that_is_not_an_object_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    let outcome = log.append(&"text", deadline());
    assert!(matches!(
        outcome,
        Outcome::NotWritten {
            reason: NotWritten::Encode(_)
        }
    ));
    assert!(log.read_since(0).unwrap().is_empty());
}

#[test]
fn a_session_id_must_be_one_path_component() {
    let temp = tempfile::tempdir().unwrap();
    assert!(matches!(
        SessionLog::open(temp.path(), "../escape", deadline()),
        Err(Error::Io(_))
    ));
}

#[cfg(unix)]
#[test]
fn an_unopenable_session_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    if nix::unistd::geteuid().is_root() {
        // Root bypasses the mode bits; the case cannot be built.
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let lock_path = temp.path().join(format!("{ID}.lock"));
    std::fs::write(&lock_path, b"").unwrap();
    std::fs::write(log_path(temp.path()), b"").unwrap();
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o400)).unwrap();

    let error = SessionLog::open(temp.path(), ID, deadline());
    assert!(
        matches!(error, Err(Error::Io(_))),
        "an unopenable lock is an error, not a wedge: {error:?}"
    );
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Io(_))
    ));

    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(
        log_path(temp.path()),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let error = SessionLog::open(temp.path(), ID, deadline());
    assert!(
        matches!(error, Err(Error::Io(_))),
        "an unreadable log is an error: {error:?}"
    );
    std::fs::set_permissions(
        log_path(temp.path()),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}
