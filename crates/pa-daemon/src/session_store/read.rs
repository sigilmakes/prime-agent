//! The read arm: the file-IO load surface — the streamed and windowed opens, the bounded
//! header-only readers, the file-layout helpers, and the in-memory create.

use super::view::is_warning_shown_row;
use super::{
    fold_child_usage_attributions, fs, BufRead, Context, HashMap, Map, Path, PathBuf, Read, Result,
    SessionEntry, SessionFile, SessionHeader, Value,
};

const CURRENT_SESSION_VERSION: u32 = 3;

fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[must_use]
pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

#[must_use]
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    let mut seq = 0;
    let mut rows = Vec::new();
    for line in content.split_inclusive('\n') {
        let Some(line) = line.strip_suffix('\n') else {
            break;
        };
        let Ok(row) = pa_core::session_log::Row::decode(line.as_bytes(), seq) else {
            return Vec::new();
        };
        seq = row.seq;
        rows.push(row.body);
    }
    rows
}

/// The bounded first-line read cap for header-only judgments: a session
/// header line lands well inside 512 bytes.
pub const SESSION_LIST_HEADER_READ_MAX_BYTES: usize = 512;

/// Parse one session file line as the `session` header: a JSON object tagged
/// `session` that deserializes into the typed header.
pub(crate) fn parse_session_header_line(line: &str) -> Option<SessionHeader> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let pa_types::session::FileEntry::Header { header } = serde_json::from_value(value).ok()?
    else {
        return None;
    };
    Some(header)
}

/// The session file's header, read from the first line bounded to
/// [`SESSION_LIST_HEADER_READ_MAX_BYTES`]: `None` also covers an over-long
/// first line — the bounded read refuses to judge a truncated one.
#[must_use]
pub fn read_session_header_bounded(path: &Path) -> Option<SessionHeader> {
    let file = fs::File::open(path).ok()?;
    let mut line = Vec::new();
    std::io::BufReader::new(file)
        .take(SESSION_LIST_HEADER_READ_MAX_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.len() > SESSION_LIST_HEADER_READ_MAX_BYTES || line.pop() != Some(b'\n') {
        return None;
    }
    let row = pa_core::session_log::Row::decode(&line, 0).ok()?;
    parse_session_header_line(&serde_json::to_string(&row.body).ok()?)
}

/// Read the first line of a session file and parse it as a header.
#[must_use]
pub fn read_session_header(path: &Path) -> Option<SessionHeader> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    if !first.ends_with('\n') {
        return None;
    }
    let row = pa_core::session_log::Row::decode(first.trim_end_matches('\n').as_bytes(), 0).ok()?;
    parse_session_header_line(&serde_json::to_string(&row.body).ok()?)
}

/// A session file is valid when its first line is a `session` header, judged
/// on the bounded header read.
#[must_use]
pub fn is_valid_session_file(path: &Path) -> bool {
    read_session_header_bounded(path).is_some()
}

enum OpenPolicy {
    Validate,
    Repair,
}

impl SessionFile {
    /// Load an existing session file; malformed entry lines are skipped (the TS loader).
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, is empty, or its header is invalid.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_log_path(path, OpenPolicy::Validate)
    }

    /// Open an execution owner's actual log path, repairing only after the
    /// transport established a supported header. Read-only callers use `open`.
    pub(crate) fn open_owned_log(path: &Path) -> Result<Self> {
        Self::open_log_path(path, OpenPolicy::Repair)
    }

    /// Load a log-transported session: the log is the source of truth, the
    /// fold replays it.
    ///
    /// The entry chain mirrors [`SessionFile::open`]'s old-format loader
    /// (malformed-but-complete rows are skipped, never truncated), so
    /// serving code sees the same fold either transport produced. The
    /// returned store carries the held wire: its cursor counts exactly the
    /// replayed entry rows, so later flushes only move durability.
    ///
    /// # Errors
    ///
    /// A log that no longer yields a session header (the repair truncation
    /// emptied it), a header whose type is not `session`, I/O, or a foreign
    /// writer holding the log.
    pub(crate) fn open_log(session_path: &Path, agent_dir: &Path) -> Result<Self> {
        let actual = crate::session_transport::log_file(
            agent_dir,
            &crate::session_transport::session_id_of(session_path)
                .context("session path has no id")?,
        );
        Self::open_log_path(&actual, OpenPolicy::Repair)
    }

    fn open_log_path(session_path: &Path, policy: OpenPolicy) -> Result<Self> {
        anyhow::ensure!(
            session_path.is_file(),
            "session log is unavailable: {}",
            session_path.display()
        );
        let session_id = crate::session_transport::session_id_of(session_path)
            .context("session path has no id")?;
        let directory = session_path
            .parent()
            .context("session log has no directory")?;
        let (mut log, repair) = match policy {
            OpenPolicy::Repair => {
                crate::session_transport::open_repaired_dir(directory, &session_id)
                    .map_err(anyhow::Error::from)?
            }
            OpenPolicy::Validate => (
                pa_core::session_log::SessionLog::open(
                    directory,
                    &session_id,
                    crate::session_transport::deadline(),
                )?,
                None,
            ),
        };
        if let Some(report) = &repair {
            eprintln!(
                "pa-daemon: session log {} repaired: {} unusable trailing byte(s) after seq {}",
                session_id, report.truncated_bytes, report.last_good_seq
            );
        }
        let mut cursor = pa_core::session_log::ReadCursor::default();
        let rows = log
            .read_tail(&mut cursor, crate::session_transport::deadline())
            .map_err(anyhow::Error::from)
            .context("read the session log")?;
        let Some(header_row) = rows.first() else {
            anyhow::bail!("session log {session_id} holds no header row (the repair emptied it)");
        };
        if header_row.body.get("type").and_then(Value::as_str) != Some("session") {
            anyhow::bail!("session log {session_id} row one is not a session header");
        }
        let pa_types::session::FileEntry::Header { header } =
            serde_json::from_value::<pa_types::session::FileEntry>(header_row.body.clone())
                .context("invalid session header in the session log")?
        else {
            anyhow::bail!("session log {session_id} row one is not a session header");
        };
        let mut file = SessionFile {
            path: session_path.to_path_buf(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            window: None,
            lease: None,
            anthropic_warning_shown: false,
            skipped_lines: 0,
            log_transport: None,
            log_progress: super::log_wire::LogProgress::default(),
        };
        for row in rows.iter().skip(1) {
            if row.body.get("type").and_then(Value::as_str) == Some("branch_selection") {
                let selected = row
                    .body
                    .get("selectedLeaf")
                    .context("branch selection has no leaf")?;
                file.leaf_id = match selected {
                    Value::Null => None,
                    Value::String(id) if file.by_id.contains_key(id) => Some(id.clone()),
                    _ => anyhow::bail!("branch selection names an absent entry"),
                };
                continue;
            }
            let entry = serde_json::from_value::<SessionEntry>(row.body.clone())
                .with_context(|| format!("invalid session entry at log seq {}", row.seq))?;
            file.push_index(entry);
        }
        fold_child_usage_attributions(&mut file.entries);
        file.anthropic_warning_shown = file.branch().iter().copied().any(is_warning_shown_row);
        file.log_progress = super::log_wire::LogProgress::opened(cursor, file.entries.len());
        let wire = crate::session_transport::LogWire::held(
            log,
            directory,
            rows.last().map_or(1, |row| row.seq),
        );
        file.log_transport = Some(wire);
        Ok(file)
    }

    /// Load the verified compacted context without decoding old message bodies.
    ///
    /// # Errors
    ///
    /// Returns an error when the windowed load fails or holds no header; a
    /// malformed retained row falls back to [`SessionFile::open`].
    pub fn open_windowed(path: &Path) -> Result<Self> {
        // One validated log replay owns history. Context projection remains the
        // existing branch/compaction fold, not a second storage implementation.
        Self::open(path)
    }

    #[cfg(test)]
    pub(super) fn ensure_full_history(&mut self) -> Result<()> {
        if self.window.is_some() {
            let full = Self::open(&self.path)?;
            self.install_full_history(full);
        }
        Ok(())
    }

    /// Merge appends made while the disk snapshot loaded without holding the store lock.
    pub(crate) fn install_full_history(&mut self, mut full: Self) {
        let Some(window) = &self.window else {
            return;
        };
        for entry in &self.entries[window.loaded_entries..] {
            if !full.by_id.contains_key(&entry.id) {
                full.push_index(entry.clone());
            }
        }
        full.leaf_id.clone_from(&self.leaf_id);
        full.lease.clone_from(&self.lease);
        // The merged store's leaf is the window's leaf: re-derive the gate
        // from the merged ACTIVE branch — the full open's own-leaf answer
        // can disagree, a post-snapshot marker must hydrate, and an
        // off-path one must not (the full scan never wins here).
        full.anthropic_warning_shown = full.branch().iter().copied().any(is_warning_shown_row);
        *self = full;
    }

    /// Create a new in-memory session; persisted with the first flush.
    pub fn create(cwd: &str, parent_session: Option<&str>, rlm_depth: u32) -> Self {
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id(),
            timestamp: crate::util::now_iso(),
            cwd: cwd.to_string(),
            parent_session: parent_session.map(str::to_string),
            rlm_depth: Some(u64::from(rlm_depth)),
            git: None,
            rest: Map::default(),
        };
        SessionFile {
            path: PathBuf::new(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            window: None,
            lease: None,
            anthropic_warning_shown: false,
            skipped_lines: 0,
            log_transport: None,
            log_progress: super::log_wire::LogProgress::default(),
        }
    }
}
