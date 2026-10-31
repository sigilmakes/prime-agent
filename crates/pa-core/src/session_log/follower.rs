//! Independent validated positions for live folds and their clones.
use std::time::Instant;

use super::{End, Error, Row, Scanner, SessionLog};

/// One fold's position. Clones advance independently; the previous row is
/// retained to detect a repaired suffix even after the file has grown again.
#[derive(Debug, Clone, Default)]
pub struct ReadCursor {
    start: u64,
    end: u64,
    last: Option<Row>,
}

/// A follower could not safely extend its existing fold.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The log operation failed without advancing the cursor.
    #[error(transparent)]
    Log(#[from] Error),
    /// Previously observed history was removed or replaced. Reopen the fold.
    #[error("session history changed at seq {seq}; reopen the session before writing")]
    Replaced { seq: u64 },
}

impl SessionLog {
    /// Append at a pinned sequence only while this fold's previously observed
    /// boundary is still present. Validation and mutation share one flock.
    ///
    /// # Errors
    /// The same errors as [`Self::read_tail`]. A rejected boundary writes nothing.
    pub fn append_at_checked<T: serde::Serialize>(
        &mut self,
        seq: u64,
        row: &T,
        cursor: &ReadCursor,
        deadline: Instant,
    ) -> Result<super::Outcome, ReadError> {
        let body = match super::line::body_of(row) {
            Ok(body) => body,
            Err(reason) => {
                return Ok(super::Outcome::NotWritten {
                    reason: super::NotWritten::Encode(reason),
                })
            }
        };
        self.locked(deadline, |log| {
            Ok(log
                .read_cursor(&mut cursor.clone())
                .and_then(|_| log.write_at(Some(seq), body).map_err(ReadError::from)))
        })?
    }
    /// Read new rows through an independent byte cursor under the session lock.
    /// Validates the previous row before extending it, so a repair followed by
    /// regrowth cannot silently keep a stale suffix. Work is bounded by that
    /// one previous row plus the new tail, not the full history.
    ///
    /// # Errors
    /// [`ReadError::Replaced`] when the previous row no longer exists unchanged;
    /// [`ReadError::Log`] for lock, I/O, checksum, ordering or torn-tail errors.
    /// Neither failure advances the caller's cursor.
    pub fn read_tail(
        &mut self,
        cursor: &mut ReadCursor,
        deadline: Instant,
    ) -> Result<Vec<Row>, ReadError> {
        // Keep the domain error outside `locked`, whose error vocabulary is
        // shared with append and repair, not with a particular fold's history.
        self.locked(deadline, |log| Ok(log.read_cursor(cursor)))?
    }

    fn read_cursor(&self, cursor: &mut ReadCursor) -> Result<Vec<Row>, ReadError> {
        let replaced = || ReadError::Replaced {
            seq: cursor.last.as_ref().map_or(0, |row| row.seq),
        };
        let len = self.file_len()?;
        if len < cursor.end {
            return Err(replaced());
        }
        let mut scanner = Scanner::open(self.disk.as_ref(), &self.log_path, cursor.start)?;
        let mut position = cursor.clone();
        let mut rows = Vec::new();
        let mut first = true;
        let mut previous_seq = 0;
        loop {
            let start = scanner.offset;
            let Some(parsed) = scanner.advance()? else {
                break;
            };
            let row = Row {
                seq: parsed.seq,
                ts: parsed.ts.to_owned(),
                body: serde_json::from_str(parsed.body).map_err(|_| Error::Corrupt {
                    after_seq: previous_seq,
                })?,
            };
            if first {
                first = false;
                if let Some(expected) = &cursor.last {
                    if row.seq != expected.seq
                        || row.ts != expected.ts
                        || row.body != expected.body
                        || scanner.offset != cursor.end
                    {
                        return Err(replaced());
                    }
                    previous_seq = row.seq;
                    continue;
                }
            }
            if row.seq <= previous_seq {
                return Err(Error::Corrupt {
                    after_seq: previous_seq,
                }
                .into());
            }
            previous_seq = row.seq;
            position.start = start;
            position.end = scanner.offset;
            position.last = Some(row.clone());
            rows.push(row);
        }
        if first && cursor.last.is_some() {
            return Err(replaced());
        }
        if let End::Bad { terminated } = scanner.end {
            return Err(if terminated {
                Error::Corrupt {
                    after_seq: previous_seq,
                }
            } else {
                Error::Damaged {
                    last_good_seq: previous_seq,
                    truncated_bytes: len - scanner.offset,
                }
            }
            .into());
        }
        *cursor = position;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn independent_cursors_extend_without_replaying_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path(), "followers", deadline()).unwrap();
        for n in 0..64 {
            assert!(matches!(
                log.append(&json!({"n":n}), deadline()),
                super::super::Outcome::Committed { .. }
            ));
        }
        let mut cursor = ReadCursor::default();
        assert_eq!(log.read_tail(&mut cursor, deadline()).unwrap().len(), 64);
        let mut clone = cursor.clone();
        assert!(matches!(
            log.append(&json!({"n":64}), deadline()),
            super::super::Outcome::Committed { .. }
        ));
        assert_eq!(
            log.read_tail(&mut cursor, deadline())
                .unwrap()
                .iter()
                .map(|row| row.body.clone())
                .collect::<Vec<_>>(),
            vec![json!({"n":64})]
        );
        assert_eq!(log.read_tail(&mut clone, deadline()).unwrap().len(), 1);
        // A caller's cursor owns only the tail boundary, not a full-history
        // integrity audit. This also proves no offset-zero scan on each read.
        let path = dir.path().join("followers.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] = b'!';
        std::fs::write(path, bytes).unwrap();
        assert!(log.read_tail(&mut cursor, deadline()).unwrap().is_empty());
    }

    #[test]
    fn repair_and_regrowth_cannot_reuse_a_followers_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path(), "regrowth", deadline()).unwrap();
        log.append(&json!({"n":1}), deadline());
        log.append(&json!({"n":2}), deadline());
        let mut cursor = ReadCursor::default();
        log.read_tail(&mut cursor, deadline()).unwrap();
        let path = dir.path().join("regrowth.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        let tail_start = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        bytes[tail_start] = b'!';
        std::fs::write(&path, bytes).unwrap();
        SessionLog::repair(dir.path(), "regrowth", deadline()).unwrap();
        let mut rival = SessionLog::open(dir.path(), "regrowth", deadline()).unwrap();
        rival.append(
            &json!({"n":"replacement row is longer than original"}),
            deadline(),
        );
        rival.append(&json!({"n":3}), deadline());
        assert!(matches!(
            log.read_tail(&mut cursor, deadline()),
            Err(ReadError::Replaced { seq: 2 })
        ));
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            log.append_at_checked(4, &json!({"n":"stale append"}), &cursor, deadline()),
            Err(ReadError::Replaced { seq: 2 })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut reopened = ReadCursor::default();
        assert_eq!(log.read_tail(&mut reopened, deadline()).unwrap().len(), 3);
    }
}
