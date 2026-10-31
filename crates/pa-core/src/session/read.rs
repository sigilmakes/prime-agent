//! Read-only session history from validated SessionLog envelopes.
use pa_types::session::FileEntry;
use std::io::{self, BufRead};
use std::path::Path;

/// Read an entire session without creating, repairing, or modifying any file.
///
/// # Errors
/// Rejects unsupported formats, invalid envelopes, ordering, checksums or rows.
pub fn read_entries(path: &Path) -> io::Result<Vec<FileEntry>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    let mut line = Vec::new();
    let mut seq = 0;
    let mut entries = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        if line.pop() != Some(b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session log has an unterminated tail",
            ));
        }
        let row = crate::session_log::Row::decode(&line, seq)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        seq = row.seq;
        entries.push(serde_json::from_value(row.body)?);
        line.clear();
    }
    if !matches!(entries.first(), Some(FileEntry::Header { header }) if header.version == Some(super::CURRENT_SESSION_VERSION) && !header.id.is_empty() && header.id != "." && header.id != ".." && !header.id.contains(['/', '\\', '\0'])) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session log has no header",
        ));
    }
    super::apply_child_usage_attributions(&mut entries);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::{Outcome, SessionLog};
    #[test]
    fn reads_envelopes_and_rejects_corruption_without_repairing_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.jsonl");
        let mut log = SessionLog::open(dir.path(), "read", std::time::Instant::now()).unwrap();
        let header = serde_json::json!({"type":"session","id":"read","version":3,"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"});
        assert!(matches!(
            log.append(&header, std::time::Instant::now()),
            Outcome::Committed { .. }
        ));
        assert_eq!(read_entries(&path).unwrap().len(), 1);
        let mut damaged = std::fs::read(&path).unwrap();
        let index = damaged
            .windows(4)
            .position(|bytes| bytes == b"/tmp")
            .unwrap();
        damaged[index + 1] = b'x';
        std::fs::write(&path, &damaged).unwrap();
        assert!(read_entries(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }
}
