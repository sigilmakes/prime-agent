//! Replacement session-log core: one append-only JSONL file per session, one
//! exclusive `flock` owner at a time, and four append outcomes the caller acts
//! on. Invariants and the reason there is no ownership handshake live in
//! `crates/pa-core/src/session_log/README.md`.
//!
//! The core never waits, retries, or applies policy: `append` makes one
//! non-blocking lock attempt, one write, one fsync, and reports one of the four
//! outcomes. Waiting for another writer is the caller's decision.

mod crc;
mod disk;
mod line;
#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use disk::{Disk, RealDisk};
pub use line::Row;

/// Why nothing durable was written.
#[derive(Debug)]
pub enum NotWritten {
    /// The row did not serialize to a JSON object; the disk was not touched.
    Encode(String),
    /// `seq` is already named by another row, or lies below the log's next seq
    /// without naming a row; nothing was written.
    SeqConflict { seq: u64 },
    /// The log's tail is unusable because an earlier writer died mid-line;
    /// nothing was written. [`SessionLog::repair`] makes the log appendable.
    Damaged {
        last_good_seq: u64,
        truncated_bytes: u64,
    },
    /// The write failed provably before the line was complete, or the file
    /// could not be opened at all.
    Io(io::Error),
    /// A resolved [`Outcome::Unknown`]: the row is not in the log.
    Absent { seq: u64 },
}

/// What one append attempt established.
#[derive(Debug)]
pub enum Outcome {
    /// The row is durable at `seq`; `ts` is the stamp this writer wrote into
    /// the row's envelope (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
    Committed { seq: u64, ts: String },
    /// Nothing durable was written; `reason` says why.
    NotWritten { reason: NotWritten },
    /// The whole line reached the file but its barrier failed, so the row may
    /// or may not be there; `recover` decides.
    Unknown { recover: Recover },
    /// Another writer held the lock. `until` is the caller's deadline, echoed;
    /// `holder` is who held it, as that writer recorded itself.
    Busy {
        until: Instant,
        holder: Option<Holder>,
    },
}

/// Who held the lock when a caller was refused.
///
/// The record is written by the holder into the lock file for diagnostics; it
/// is never consulted for correctness, and it may be absent (a holder killed
/// between `flock` and the record) or stale. Ownership is the `flock` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process id of the holder.
    pub pid: u32,
    /// Wall-clock milliseconds when the holder took the lock.
    pub since_ms: u64,
}

/// Why a log operation could not run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem failure.
    #[error("session log I/O: {0}")]
    Io(#[from] io::Error),
    /// Another writer holds the lock at the caller's deadline.
    #[error("session log lock is held by another writer")]
    /// `until` is the caller's deadline; `holder` is who held the lock.
    Busy {
        until: Instant,
        holder: Option<Holder>,
    },
    /// The tail is torn or corrupt; [`SessionLog::repair`] truncates it.
    #[error(
        "session log damaged after seq {last_good_seq}: {truncated_bytes} unusable trailing bytes"
    )]
    Damaged {
        last_good_seq: u64,
        truncated_bytes: u64,
    },
}

/// What [`SessionLog::repair`] left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairReport {
    /// Last complete, checksum-valid seq kept.
    pub last_good_seq: u64,
    /// Bytes dropped from the tail.
    pub truncated_bytes: u64,
    /// Complete, checksum-valid rows kept.
    pub rows: u64,
}

/// The undecided tail of an [`Outcome::Unknown`] append.
pub struct Recover {
    disk: Arc<dyn Disk>,
    log_path: PathBuf,
    lock_path: PathBuf,
    seq: u64,
    body: String,
    cause: io::Error,
}

impl Recover {
    /// The seq the undecided append asked for.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Why the append could not prove durability.
    #[must_use]
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    /// Decide the undecided write: truncate any torn tail, then look for the
    /// row. A `Committed` answer is re-fsynced first, so it is durable.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] while another writer holds the lock; [`Error::Io`] when
    /// the log cannot be read or re-synced, in which case the write is still
    /// undecided and the caller may try again.
    pub fn recover(&self, deadline: Instant) -> Result<Outcome, Error> {
        let lock = self.disk.open_lock(&self.lock_path)?;
        let file = match self.disk.open_rw(&self.log_path) {
            Ok(file) => file,
            // The whole file is gone: nothing of this write survived.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Outcome::NotWritten {
                    reason: NotWritten::Absent { seq: self.seq },
                })
            }
            Err(error) => return Err(error.into()),
        };
        let mut log = SessionLog::new(
            self.disk.clone(),
            self.log_path.clone(),
            self.lock_path.clone(),
            file,
            lock,
        );
        log.locked(deadline, |log| {
            let mut scan = log.scan(0, self.seq.saturating_sub(1))?;
            if !scan.clean {
                log.disk.truncate(&log.file, scan.good_len)?;
                log.disk.fsync(&log.file)?;
                scan = log.scan(0, self.seq.saturating_sub(1))?;
            }
            let found = scan.rows.into_iter().find(|row| row.seq == self.seq);
            match found {
                Some(row) if body_matches(&row, &self.body) => {
                    // "Committed" must mean durable: take a fresh barrier.
                    log.disk.fsync(&log.file)?;
                    Ok(Outcome::Committed {
                        seq: self.seq,
                        ts: row.ts,
                    })
                }
                Some(_) => Ok(Outcome::NotWritten {
                    reason: NotWritten::SeqConflict { seq: self.seq },
                }),
                None => Ok(Outcome::NotWritten {
                    reason: NotWritten::Absent { seq: self.seq },
                }),
            }
        })
    }
}

impl std::fmt::Debug for Recover {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Recover")
            .field("log", &self.log_path)
            .field("seq", &self.seq)
            .field("cause", &self.cause)
            .finish_non_exhaustive()
    }
}

/// One session's append-only log.
pub struct SessionLog {
    disk: Arc<dyn Disk>,
    log_path: PathBuf,
    lock_path: PathBuf,
    file: std::fs::File,
    lock: std::fs::File,
    next_seq: u64,
    known_len: u64,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLog")
            .field("log", &self.log_path)
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

impl SessionLog {
    /// Open (creating when absent) the log for `id` in `dir`.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] while another writer holds the lock; [`Error::Damaged`]
    /// when the tail is torn or corrupt (call [`Self::repair`], then open
    /// again); I/O errors as the OS reports them, so an unreadable or
    /// unwritable session is reported instead of being recreated.
    pub fn open(dir: &Path, id: &str, deadline: Instant) -> Result<Self, Error> {
        Self::open_with(Arc::new(RealDisk), dir, id, deadline)
    }

    /// Append `row` at the log's next seq.
    pub fn append<T: Serialize>(&mut self, row: &T, deadline: Instant) -> Outcome {
        self.append_inner(None, row, deadline)
    }

    /// Append `row` at `seq`. Re-appending the same seq with the same body is
    /// idempotent and returns the existing row's outcome, so retries need no
    /// acknowledgement from anyone.
    pub fn append_at<T: Serialize>(&mut self, seq: u64, row: &T, deadline: Instant) -> Outcome {
        self.append_inner(Some(seq), row, deadline)
    }

    /// Rows with `seq > since`, in order, stopping at the first unusable line
    /// (torn tail, bad envelope, checksum mismatch, non-increasing seq).
    ///
    /// Reading takes no lock: a concurrent append can only leave a torn line at
    /// the end, which ends the scan.
    ///
    /// # Errors
    ///
    /// I/O errors from opening or reading the log.
    pub fn read_since(&self, since: u64) -> Result<Vec<Row>, Error> {
        self.iter_since(since)?.collect()
    }

    /// Rows with `seq > since`, one at a time and without holding the file in
    /// memory: the caller decides how many to keep. Stops at the first unusable
    /// line exactly like [`Self::read_since`].
    ///
    /// # Errors
    ///
    /// I/O errors from opening the log; later reads surface as the item's error.
    pub fn iter_since(&self, since: u64) -> Result<Rows, Error> {
        Ok(Rows {
            scanner: Scanner::open(self.disk.as_ref(), &self.log_path, 0)?,
            since,
            last_seq: 0,
            done: false,
        })
    }

    /// Truncate the unusable tail to the last complete, checksum-valid line,
    /// then fsync it.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] while another writer holds the lock; I/O errors.
    pub fn repair(dir: &Path, id: &str, deadline: Instant) -> Result<RepairReport, Error> {
        Self::repair_with(Arc::new(RealDisk), dir, id, deadline)
    }

    fn open_with(
        disk: Arc<dyn Disk>,
        dir: &Path,
        id: &str,
        deadline: Instant,
    ) -> Result<Self, Error> {
        let (log_path, lock_path) = paths(dir, id)?;
        let (file, created) = disk.open_log(&log_path)?;
        let lock = disk.open_lock(&lock_path)?;
        if created {
            // Create is the only directory-entry change this core makes, so it
            // is the only place a directory sync is owed: without it the entry
            // may vanish while the bytes survive.
            disk.fsync(&file)?;
            disk.sync_dir(dir)?;
        }
        let mut log = Self::new(disk, log_path, lock_path, file, lock);
        log.locked(deadline, |log| {
            let scan = log.scan(0, u64::MAX)?;
            if !scan.clean {
                return Err(Error::Damaged {
                    last_good_seq: scan.last_seq,
                    truncated_bytes: log.file_len()? - scan.good_len,
                });
            }
            log.next_seq = scan.last_seq + 1;
            log.known_len = scan.good_len;
            Ok(())
        })?;
        Ok(log)
    }

    fn repair_with(
        disk: Arc<dyn Disk>,
        dir: &Path,
        id: &str,
        deadline: Instant,
    ) -> Result<RepairReport, Error> {
        let (log_path, lock_path) = paths(dir, id)?;
        let lock = disk.open_lock(&lock_path)?;
        let file = match disk.open_rw(&log_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RepairReport::default())
            }
            Err(error) => return Err(error.into()),
        };
        let mut log = Self::new(disk, log_path, lock_path, file, lock);
        log.locked(deadline, |log| {
            let len = log.file_len()?;
            let scan = log.scan(0, u64::MAX)?;
            let report = RepairReport {
                last_good_seq: scan.last_seq,
                truncated_bytes: len - scan.good_len,
                rows: scan.row_count,
            };
            if !scan.clean {
                log.disk.truncate(&log.file, scan.good_len)?;
                log.disk.fsync(&log.file)?;
            }
            Ok(report)
        })
    }

    fn new(
        disk: Arc<dyn Disk>,
        log_path: PathBuf,
        lock_path: PathBuf,
        file: std::fs::File,
        lock: std::fs::File,
    ) -> Self {
        Self {
            disk,
            log_path,
            lock_path,
            file,
            lock,
            next_seq: 1,
            known_len: 0,
        }
    }

    fn append_inner<T: Serialize>(
        &mut self,
        pinned: Option<u64>,
        row: &T,
        deadline: Instant,
    ) -> Outcome {
        let body = match line::body_of(row) {
            Ok(body) => body,
            Err(reason) => {
                return Outcome::NotWritten {
                    reason: NotWritten::Encode(reason),
                }
            }
        };
        match self.locked(deadline, |log| log.write_at(pinned, body)) {
            Ok(outcome) => outcome,
            Err(Error::Busy { until, holder }) => Outcome::Busy { until, holder },
            Err(Error::Io(error)) => Outcome::NotWritten {
                reason: NotWritten::Io(error),
            },
            Err(Error::Damaged {
                last_good_seq,
                truncated_bytes,
            }) => Outcome::NotWritten {
                reason: NotWritten::Damaged {
                    last_good_seq,
                    truncated_bytes,
                },
            },
        }
    }

    /// The write itself; the lock is held by the caller.
    fn write_at(&mut self, pinned: Option<u64>, body: String) -> Result<Outcome, Error> {
        self.sync_tail()?;
        let seq = match pinned {
            Some(seq) if seq < self.next_seq => return self.retry_outcome(seq, &body),
            Some(seq) => seq,
            None => self.next_seq,
        };
        let ts = line::now_iso8601();
        let line = line::encode(seq, &ts, &body);
        let start = self.known_len;
        if let Err(error) = self.disk.write(&mut self.file, &line) {
            // A row exists only as a complete line: an interrupted write
            // cannot have committed it, so this is provably nothing.
            return Ok(Outcome::NotWritten {
                reason: NotWritten::Io(error),
            });
        }
        if let Err(cause) = self.disk.fsync(&self.file) {
            return Ok(Outcome::Unknown {
                recover: Recover {
                    disk: self.disk.clone(),
                    log_path: self.log_path.clone(),
                    lock_path: self.lock_path.clone(),
                    seq,
                    body,
                    cause,
                },
            });
        }
        self.next_seq = seq.saturating_add(1);
        self.known_len = start + line.len() as u64;
        Ok(Outcome::Committed { seq, ts })
    }

    /// A retry of a seq the log already passed: committed when the body at that
    /// seq is the same row, otherwise the seq belongs to someone else.
    fn retry_outcome(&self, seq: u64, body: &str) -> Result<Outcome, Error> {
        let scan = self.scan(0, seq.saturating_sub(1))?;
        let existing = scan.rows.into_iter().find(|row| row.seq == seq);
        Ok(match existing {
            Some(row) if body_matches(&row, body) => Outcome::Committed { seq, ts: row.ts },
            _ => Outcome::NotWritten {
                reason: NotWritten::SeqConflict { seq },
            },
        })
    }

    /// Re-read what other writers appended since our last write, under the
    /// lock, so seq assignment stays strictly increasing across writers.
    fn sync_tail(&mut self) -> Result<(), Error> {
        let len = self.file_len()?;
        if len == self.known_len {
            return Ok(());
        }
        let from = if len < self.known_len {
            // The file was truncated or replaced: start over.
            self.next_seq = 1;
            0
        } else {
            self.known_len
        };
        let scan = self.scan(from, u64::MAX)?;
        if !scan.clean {
            return Err(Error::Damaged {
                last_good_seq: scan.last_seq.max(self.next_seq - 1),
                truncated_bytes: len - scan.good_len,
            });
        }
        self.next_seq = self.next_seq.max(scan.last_seq + 1);
        self.known_len = scan.good_len;
        Ok(())
    }

    /// Read complete, valid lines, stopping at the first bad one.
    fn scan(&self, from: u64, since: u64) -> Result<Scan, Error> {
        let mut scanner = Scanner::open(self.disk.as_ref(), &self.log_path, from)?;
        let mut scan = Scan {
            rows: Vec::new(),
            row_count: 0,
            last_seq: 0,
            good_len: from,
            clean: true,
        };
        while let Some(parsed) = scanner.advance()? {
            if parsed.seq <= scan.last_seq {
                scan.clean = false;
                break;
            }
            scan.last_seq = parsed.seq;
            scan.row_count += 1;
            if parsed.seq > since {
                let Ok(body) = serde_json::from_str(parsed.body) else {
                    scan.clean = false;
                    break;
                };
                scan.rows.push(Row {
                    seq: parsed.seq,
                    ts: parsed.ts.to_owned(),
                    body,
                });
            }
        }
        scan.good_len = scanner.offset;
        scan.clean = scan.clean && scanner.end == End::Eof;
        Ok(scan)
    }

    /// Leave a courtesy note of who holds the lock. Failure is invisible to the
    /// caller: the note is diagnostics, the `flock` is the ownership.
    fn record_holder(&self) {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let since_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let record = format!("{} {since_ms}\n", std::process::id());
        let mut file = &self.lock;
        let written = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(record.as_bytes()));
        if let Err(error) = written {
            tracing::debug!(%error, path = %self.lock_path.display(), "session log holder record not written");
        }
    }

    /// Read the holder's courtesy note; `None` when there is none to trust.
    fn read_holder(&self) -> Option<Holder> {
        let note = std::fs::read_to_string(&self.lock_path).ok()?;
        let (pid, since_ms) = note.split_once(' ')?;
        Some(Holder {
            pid: pid.parse().ok()?,
            since_ms: since_ms.trim().parse().ok()?,
        })
    }

    fn file_len(&self) -> Result<u64, Error> {
        Ok(self.disk.len(&self.file)?)
    }

    /// One non-blocking lock attempt around `f`. There is no waiting here: the
    /// incident this core replaces was a wait that never ended.
    fn locked<T>(
        &mut self,
        deadline: Instant,
        f: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.disk.try_lock(&self.lock)? {
            return Err(Error::Busy {
                until: deadline,
                holder: self.read_holder(),
            });
        }
        self.record_holder();
        let result = f(self);
        if let Err(error) = self.disk.unlock(&self.lock) {
            // The lock dies with this handle at worst; a failed unlock must not
            // turn a completed append into a failure.
            tracing::warn!(path = %self.lock_path.display(), %error, "session log unlock failed");
        }
        result
    }
}

/// Streaming rows after a seq, validated line by line.
pub struct Rows {
    scanner: Scanner,
    since: u64,
    last_seq: u64,
    done: bool,
}

impl Iterator for Rows {
    type Item = Result<Row, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            match self.scanner.advance() {
                Ok(Some(parsed)) => {
                    if parsed.seq <= self.last_seq {
                        self.done = true;
                        continue;
                    }
                    self.last_seq = parsed.seq;
                    if parsed.seq <= self.since {
                        continue;
                    }
                    let Ok(body) = serde_json::from_str(parsed.body) else {
                        self.done = true;
                        continue;
                    };
                    return Some(Ok(Row {
                        seq: parsed.seq,
                        ts: parsed.ts.to_owned(),
                        body,
                    }));
                }
                Ok(None) => self.done = true,
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
        None
    }
}

/// Why a line-by-line read stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// Unread lines may remain.
    Open,
    /// The file ended on a complete line.
    Eof,
    /// A torn, corrupt, or oversized line ended the read.
    Bad,
}

/// Reads validated lines from an offset, one at a time.
struct Scanner {
    reader: std::io::BufReader<std::fs::File>,
    buffer: Vec<u8>,
    /// Byte offset of the next unread line: the end of the last valid one.
    offset: u64,
    end: End,
}

impl Scanner {
    fn open(disk: &dyn Disk, path: &Path, from: u64) -> Result<Self, Error> {
        use std::io::{BufReader, Seek, SeekFrom};

        let mut reader = BufReader::new(disk.open_read(path)?);
        reader.seek(SeekFrom::Start(from))?;
        Ok(Self {
            reader,
            buffer: Vec::new(),
            offset: from,
            end: End::Open,
        })
    }

    /// The next complete, valid line; `None` once the read has ended, with
    /// [`Self::end`] saying whether that was the file's end or damage.
    fn advance(&mut self) -> Result<Option<line::Parsed<'_>>, Error> {
        use std::io::{BufRead, Read as _};

        if self.end != End::Open {
            return Ok(None);
        }
        self.buffer.clear();
        let read = self
            .reader
            .by_ref()
            .take(line::MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut self.buffer)?;
        if read == 0 {
            self.end = End::Eof;
            return Ok(None);
        }
        if self.buffer.last() != Some(&b'\n') || self.buffer.len() > line::MAX_LINE_BYTES {
            self.end = End::Bad;
            return Ok(None);
        }
        self.buffer.pop();
        let Some(parsed) = line::parse(&self.buffer) else {
            self.end = End::Bad;
            return Ok(None);
        };
        self.offset += read as u64;
        Ok(Some(parsed))
    }
}

/// One scan of the log, from the start or from a known offset.
struct Scan {
    rows: Vec<Row>,
    row_count: u64,
    last_seq: u64,
    good_len: u64,
    clean: bool,
}

/// The two paths one session owns. The id is a single path component: it comes
/// from a caller, and it names files.
fn paths(dir: &Path, id: &str) -> Result<(PathBuf, PathBuf), Error> {
    let single = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\'])
        && !id.contains('\0');
    if !single {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session id {id:?} is not a single path component"),
        )
        .into());
    }
    Ok((
        dir.join(format!("{id}.jsonl")),
        dir.join(format!("{id}.lock")),
    ))
}

fn body_matches(row: &Row, body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| value == row.body)
}
