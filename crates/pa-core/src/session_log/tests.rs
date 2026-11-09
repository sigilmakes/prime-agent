//! The required behavior list from the session-log spec, one test per item.

use std::fmt::Write as _;
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

/// Write `rows` the way the 2026-10-30 core wrote them: one envelope per line,
/// seq equal to the line number, CRC over the seq and the row's exact bytes.
fn write_enveloped(path: &Path, rows: &[serde_json::Value]) {
    let mut text = String::new();
    for (index, row) in rows.iter().enumerate() {
        let seq = index as u64 + 1;
        let body = serde_json::to_string(row).unwrap();
        writeln!(
            text,
            "{{\"seq\":{seq},\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}",
            super::line::checksum_of(seq, &body)
        )
        .unwrap();
    }
    std::fs::write(path, text).unwrap();
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
    // The line on disk is the row itself: no envelope, no checksum, no second
    // timestamp. Any JSONL reader sees the same objects this reader returns.
    assert_eq!(
        std::fs::read_to_string(log_path(temp.path())).unwrap(),
        format!("{}\n{}\n", row(1), row(2))
    );

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
        committed_seq(&log.append_at(1, &row(1), deadline())),
        Some(1)
    );
    assert_eq!(
        committed_seq(&log.append_at(1, &row(1), deadline())),
        Some(1)
    );
    let rows = log.read_since(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 1);

    let taken = log.append_at(1, &row(99), deadline());
    assert!(
        matches!(
            taken,
            Outcome::NotWritten {
                reason: NotWritten::SeqConflict { seq: 1 }
            }
        ),
        "a different row at the same seq is not a retry: {taken:?}"
    );
    // A pin past the end is not a place a line can go: a row's seq is its
    // position, so a gap is unrepresentable rather than silently numbered.
    let past_the_end = log.append_at(3, &row(3), deadline());
    assert!(
        matches!(
            past_the_end,
            Outcome::NotWritten {
                reason: NotWritten::SeqConflict { seq: 3 }
            }
        ),
        "a pin past the end leaves an unnumberable line: {past_the_end:?}"
    );
    assert_eq!(log.read_since(0).unwrap().len(), 1);
}

#[test]
fn a_flipped_byte_stops_the_read_and_is_reported_by_repair() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    // An enveloped file is the one whose repair may drop a bad line: the CRC
    // proves the damage, and its rows all come from the core that wrote it.
    write_enveloped(&path, &[row(1), row(2), row(3)]);
    let log = SessionLog::open(temp.path(), ID, deadline()).unwrap();

    // The `"` that closes the second row's `kind` value: a flip there breaks the
    // envelope's checksum and the JSON.
    let mut bytes = std::fs::read(&path).unwrap();
    let target = bytes
        .windows(6)
        .position(|window| window == br#""n":2}"#)
        .expect("the second row's body")
        - 2;
    assert_eq!(bytes[target], b'"', "the quote that closes `test`");
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
fn a_byte_flip_inside_a_string_is_the_damage_a_bare_line_cannot_show() {
    // The envelope's checksum caught this; a bare line does not. Stated as a
    // test so the loss stays visible: a flip that leaves valid JSON is read as
    // that different row, and only `repair` can never learn it happened.
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
    bytes[target] = b'9';
    std::fs::write(&path, &bytes).unwrap();

    let rows = log.read_since(0).unwrap();
    assert_eq!(rows.len(), 3, "all three lines are still valid JSON");
    assert_eq!(
        rows[1].body,
        row(9),
        "and the flipped row reads as its new text"
    );
}

#[test]
fn a_legacy_envelope_log_loads_and_its_seq_must_match_its_position() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let envelope = |seq: u64| {
        let body = serde_json::to_string(&row(seq)).unwrap();
        format!(
            "{{\"seq\":{seq},\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}\n",
            super::line::checksum_of(seq, &body)
        )
    };
    // The shape the 2026-10-30 core wrote: one envelope per line, seq equal to
    // the line number, CRC over the seq and the row's exact bytes.
    std::fs::write(
        &path,
        format!("{}{}{}", envelope(1), envelope(2), envelope(3)),
    )
    .unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    let rows = log.read_since(0).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(rows[2].body, row(3));
    // The writer has one shape now: the next line is the row, even here. The
    // reader accepts both, so the file still reads back as four rows.
    assert_eq!(committed_seq(&log.append(&row(4), deadline())), Some(4));
    let appended = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        appended.lines().last(),
        Some(row(4).to_string().as_str()),
        "an append writes the row itself"
    );
    assert_eq!(
        log.read_since(0)
            .unwrap()
            .iter()
            .map(|row| row.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );

    // The same three lines with one seq moved off its position: damage, because
    // a bare line's seq is only ever its position.
    std::fs::write(
        &path,
        format!("{}{}{}", envelope(1), envelope(2), envelope(5)),
    )
    .unwrap();
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
fn a_new_file_is_bare_and_its_lines_are_the_rows() {
    let temp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    for n in 1..=3u64 {
        assert_eq!(committed_seq(&log.append(&row(n), deadline())), Some(n));
    }
    let text = std::fs::read_to_string(log_path(temp.path())).unwrap();
    let mut expected = String::new();
    for n in 1..=3u64 {
        writeln!(expected, "{}", row(n)).unwrap();
    }
    assert_eq!(text, expected);
}

#[test]
fn a_partial_envelope_appended_by_a_foreign_writer_is_corrupt_not_a_row() {
    // A line that names any part of the envelope is judged as an envelope. A
    // half-written one must never be read as a row of its own.
    for partial in [
        "{\"seq\":3,\"row\":{\"kind\":\"test\",\"n\":3}}",
        "{\"ts\":\"2026-10-30T04:00:00.000Z\",\"row\":{\"kind\":\"test\",\"n\":3}}",
        "{\"seq\":3,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"00000000\",\"row\":{\"kind\":\"test\",\"n\":3},\"x\":1}",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = log_path(temp.path());
        let log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
        drop(log);
        std::fs::write(&path, format!("{}\n{}\n", row(1), partial)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            matches!(
                SessionLog::open(temp.path(), ID, deadline()),
                Err(Error::Damaged {
                    last_good_seq: 1,
                    ..
                })
            ),
            "{partial}"
        );
        // And nothing is repaired behind the caller's back.
        assert!(matches!(
            SessionLog::repair(temp.path(), ID, deadline()),
            Err(Error::Corrupt { after_seq: 1 })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{partial}");
    }
}

#[test]
fn a_wrong_seq_envelope_is_rejected_wherever_it_is_read() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let body = serde_json::to_string(&row(2)).unwrap();
    let envelope = format!(
        "{{\"seq\":3,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}\n",
        super::line::checksum_of(3, &body)
    );
    std::fs::write(&path, &envelope).unwrap();

    // The line-level decoder knows it should be line one.
    assert!(super::Row::decode(envelope.trim_end().as_bytes(), 0).is_err());
    // The log reader agrees: the first line is damage, not a row.
    assert!(matches!(
        SessionLog::open(temp.path(), ID, deadline()),
        Err(Error::Damaged {
            last_good_seq: 0,
            ..
        })
    ));
    // Nothing before the damage, so nothing may be truncated.
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Corrupt { after_seq: 0 })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), envelope.as_bytes());
}

#[test]
fn every_append_writes_the_row_itself() {
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let body = serde_json::to_string(&row(1)).unwrap();
    let envelope = format!(
        "{{\"seq\":1,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}\n",
        super::line::checksum_of(1, &body)
    );
    // Over a legacy envelope the new line is a row: the writer has one shape,
    // and the reader accepts both, so the file still reads.
    std::fs::write(&path, &envelope).unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{envelope}{}\n", row(2))
    );
    drop(log);

    // A bare file gains a bare line, as it always did.
    std::fs::write(&path, format!("{}\n", row(1))).unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{}\n{}\n", row(1), row(2))
    );
}

#[test]
fn a_foreign_line_does_not_change_what_the_next_append_writes() {
    // Another writer may append either shape after this handle opened: an old
    // binary writes envelopes, this one writes rows. The append that follows
    // adopts those lines for numbering and writes a row regardless; nothing
    // reads the foreign line's shape to decide the next line's.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    let body = serde_json::to_string(&row(2)).unwrap();
    let foreign = format!(
        "{{\"seq\":2,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}\n",
        super::line::checksum_of(2, &body)
    );
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(foreign.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert_eq!(committed_seq(&log.append(&row(3), deadline())), Some(3));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        format!("{}\n{foreign}{}\n", row(1), row(3)),
        "the foreign line is kept and the next line is a row"
    );
    assert_eq!(
        log.read_since(0)
            .unwrap()
            .iter()
            .map(|row| row.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[test]
fn a_row_may_not_carry_a_reserved_name() {
    // The reader judges a line that names seq/ts/crc/row as an envelope, so a
    // row carrying one would be committed and then be unreadable — taking the
    // rest of the log with it. The writer refuses before the disk is touched.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    let before = std::fs::read(&path).unwrap();
    for reserved in ["seq", "ts", "crc", "row"] {
        let mut body = serde_json::json!({"type": "test", "n": 2});
        body[reserved] = serde_json::json!(null);
        assert!(
            matches!(
                log.append(&body, deadline()),
                Outcome::NotWritten {
                    reason: NotWritten::Encode(_)
                }
            ),
            "{reserved}"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    assert_eq!(log.read_since(0).unwrap().len(), 1, "the log still reads");
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
}

#[test]
fn a_file_holding_both_shapes_reads() {
    // A row followed by an envelope is what an append by this binary to a legacy
    // file produces. Both lines carry their position the same way — the first by
    // being line one, the second by declaring it — so the file reads.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let body = serde_json::to_string(&row(2)).unwrap();
    std::fs::write(
        &path,
        format!(
            "{}\n{{\"seq\":2,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"{:08x}\",\"row\":{body}}}\n",
            row(1),
            super::line::checksum_of(2, &body)
        ),
    )
    .unwrap();
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(
        log.read_since(0)
            .unwrap()
            .iter()
            .map(|row| row.seq)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(committed_seq(&log.append(&row(3), deadline())), Some(3));
    assert_eq!(log.read_since(0).unwrap().len(), 3);
}

#[test]
fn a_corrupt_first_envelope_line_is_left_for_an_operator() {
    // With nothing before the damage there is no anchor: a repair would empty
    // the file, and the file's shape is unknown. Refused, not truncated.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let body = serde_json::to_string(&row(1)).unwrap();
    std::fs::write(
        &path,
        format!(
            "{{\"seq\":1,\"ts\":\"2026-10-30T04:00:00.000Z\",\"crc\":\"00000000\",\"row\":{body}}}\n"
        ),
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        SessionLog::open(temp.path(), ID, deadline()),
        Err(Error::Damaged {
            last_good_seq: 0,
            ..
        })
    ));
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Corrupt { after_seq: 0 })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
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
    // The refusal is still a refusal, and repair refuses too: this file is
    // bare, so a complete line that is not a row may be history this binary did
    // not write, and truncating there would take everything behind it.
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Corrupt { after_seq: 2 })
    ));
    // And the log still reads as damage at the same line, never as three rows.
    assert!(matches!(
        log.read_since(0),
        Err(Error::Corrupt { after_seq: 2 })
    ));
}

#[test]
fn a_damaged_bare_file_is_never_truncated_by_open_or_repair() {
    // Nineteen logs on the live disk are bare and carry a complete line that is
    // not JSON, with up to 18 MB of history behind the first one.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    // A bare log whose fourth line is not a row, with a good row behind it.
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{{\"kind\":\"test\",\"n\":3,\"cut\n{}\n",
            row(1),
            row(2),
            row(4)
        ),
    )
    .unwrap();
    let damaged = std::fs::read(&path).unwrap();

    assert!(
        matches!(
            SessionLog::open(temp.path(), ID, deadline()),
            Err(Error::Damaged {
                last_good_seq: 2,
                ..
            })
        ),
        "open reports the damage without touching it"
    );
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Corrupt { after_seq: 2 })
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        damaged,
        "neither open nor repair wrote anything"
    );
}

#[test]
fn a_torn_tail_on_a_bare_file_is_still_repaired() {
    // A torn tail is a write that never became a row: dropping it loses nothing,
    // whatever shape the file has.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    let good = std::fs::metadata(&path).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"kind\":\"test\",").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let torn = std::fs::metadata(&path).unwrap().len() - good;

    assert_eq!(
        SessionLog::repair(temp.path(), ID, deadline())
            .unwrap()
            .truncated_bytes,
        torn
    );
    assert_eq!(committed_seq(&log.append(&row(2), deadline())), Some(2));
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
    // A bare file is not repaired from a complete bad line, so the cursor cannot
    // move past it either: the caller has to act, not the core.
    assert!(matches!(
        SessionLog::repair(temp.path(), ID, deadline()),
        Err(Error::Corrupt { after_seq: 2 })
    ));
    assert!(matches!(
        follower.refresh(deadline()),
        Err(Error::Corrupt { after_seq: 2 })
    ));
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

#[test]
fn a_pinned_retry_reestablishes_the_failed_durability_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let barriers = Arc::new(AtomicUsize::new(0));
    let disk = {
        let fail = Arc::clone(&fail);
        let barriers = Arc::clone(&barriers);
        Arc::new(TestDisk::new().fsync(move |_| {
            barriers.fetch_add(1, Ordering::SeqCst);
            if fail.load(Ordering::SeqCst) {
                Err(io::Error::other("barrier unavailable"))
            } else {
                Ok(())
            }
        }))
    };
    let mut log = SessionLog::open_with(disk, dir.path(), ID, deadline()).unwrap();
    fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        log.append_at(1, &row(1), deadline()),
        Outcome::Unknown { .. }
    ));
    let before = barriers.load(Ordering::SeqCst);
    assert!(!matches!(
        log.append_at(1, &row(1), deadline()),
        Outcome::Committed { .. }
    ));
    assert_eq!(barriers.load(Ordering::SeqCst), before + 1);
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        committed_seq(&log.append_at(1, &row(1), deadline())),
        Some(1)
    );
    assert_eq!(log.read_since(0).unwrap().len(), 1);
}

#[test]
fn a_first_append_never_adopts_a_rivals_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = SessionLog::open(dir.path(), ID, deadline()).unwrap();
    let mut rival = SessionLog::open(dir.path(), ID, deadline()).unwrap();
    let header = json!({"type":"session", "id":ID});
    assert_eq!(
        committed_seq(&first.append_first(&header, deadline())),
        Some(1)
    );
    let bytes = std::fs::read(log_path(dir.path())).unwrap();
    assert!(matches!(
        rival.append_first(&header, deadline()),
        Outcome::NotWritten {
            reason: NotWritten::NonEmpty
        }
    ));
    assert_eq!(std::fs::read(log_path(dir.path())).unwrap(), bytes);
    // A torn first attempt is existing bytes too, not permission to overwrite.
    std::fs::write(log_path(dir.path()), b"{\"seq\":").unwrap();
    let torn = std::fs::read(log_path(dir.path())).unwrap();
    assert!(matches!(
        rival.append_first(&header, deadline()),
        Outcome::NotWritten {
            reason: NotWritten::NonEmpty
        }
    ));
    assert_eq!(std::fs::read(log_path(dir.path())).unwrap(), torn);
}

#[test]
fn recover_does_not_truncate_a_bare_files_complete_bad_line() {
    // `recover` resolves this process's own undecided append. A complete line
    // that is not a row is not that append: it may be history this binary did
    // not write, so recover refuses exactly where repair does.
    let temp = tempfile::tempdir().unwrap();
    let path = log_path(temp.path());
    let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
    assert_eq!(committed_seq(&log.append(&row(1), deadline())), Some(1));
    drop(log);

    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"kind\":\"test\",\"cut\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before = std::fs::read(&path).unwrap();

    let recover = super::Recover {
        disk: Arc::new(RealDisk),
        log_path: path.clone(),
        lock_path: temp.path().join(format!("{ID}.lock")),
        seq: 2,
        body: serde_json::to_string(&row(2)).unwrap(),
        cause: io::Error::other("test"),
    };
    assert!(matches!(
        recover.recover(deadline()),
        Err(Error::Corrupt { after_seq: 1 })
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "nothing was truncated"
    );
}
