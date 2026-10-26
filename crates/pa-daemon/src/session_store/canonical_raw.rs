//! Strict physical journal capture. This module grants no runtime owner authority.
//! No usage overlay, derived loader, sidecar, or journal write is used here.
#![allow(dead_code)] // Private primitives land before the owner adapter.

use pa_core::session::canonical::{
    CanonicalCheckpoint, CanonicalCursor, CanonicalFrontier, CanonicalJournalRow,
    CanonicalOwnerScope, CanonicalSource, CanonicalSourceRequest,
};
use pa_types::session::{AgentMessage, FileEntry};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::Arc;

const MAX_ROW_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TAIL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawErrorKind {
    Io,
    Malformed,
    InvalidGraph,
    SourceChanged,
    CursorChanged,
    BranchChanged,
    Limit,
    WindowUnavailable,
}
#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub(crate) struct RawError {
    pub kind: RawErrorKind,
    pub detail: String,
}
type Result<T> = std::result::Result<T, RawError>;
fn error(kind: RawErrorKind, detail: impl Into<String>) -> RawError {
    RawError {
        kind,
        detail: detail.into(),
    }
}
#[allow(clippy::needless_pass_by_value)] // `map_err` supplies an owned I/O error.
fn io(e: std::io::Error) -> RawError {
    error(RawErrorKind::Io, e.to_string())
}
fn checked(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| error(RawErrorKind::Limit, "physical frontier overflow"))
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Values supplied by the adapter after it validates its actual weak owner/lease.
#[derive(Debug, Clone)]
pub(crate) struct RawPosition {
    pub owner: CanonicalOwnerScope,
    pub canonical_revision: u64,
    pub branch_revision: u64,
    pub selected_leaf: Option<String>,
}
#[derive(Debug, Clone, Copy)]
pub(crate) enum CaptureMode {
    Full,
    /// Bodies are bounded; full graph, anchor and exact-span metadata remain O(rows).
    Sparse {
        recent_rows: usize,
        retained_bytes: usize,
    },
}
/// Deliberately excludes atime: our own reads must not invalidate a capture.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Generation {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl Generation {
    fn of(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        }
    }
    fn certificate(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"canonical-raw-generation-v1\0");
        for v in [self.dev, self.ino, self.size] {
            h.update(v.to_le_bytes());
        }
        for v in [self.mtime, self.mtime_ns, self.ctime, self.ctime_ns] {
            h.update(v.to_le_bytes());
        }
        h.finalize().into()
    }
}
/// Not caller-constructible: only a span registered in this capture can be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowSpan {
    offset: u64,
    len: u64,
    row: u64,
    certificate: [u8; 32],
}
impl RowSpan {
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn physical_row(&self) -> u64 {
        self.row
    }
}
#[derive(Debug, Clone)]
pub(crate) struct RowMetadata {
    pub span: RowSpan,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub kind: String,
    pub assistant: bool,
    /// Exact prior IDs referenced by compaction, branch summary, label or attribution.
    pub anchors: Vec<String>,
}
#[derive(Debug)]
pub(crate) struct SparsePlan {
    /// Every physical non-header row. Bodies are not retained by this index.
    pub rows: Vec<RowMetadata>,
    /// Rows with no ID, including unknown global metadata.
    pub global_rows: Vec<usize>,
    /// Every non-message metadata row, including ID-bearing session-wide records.
    pub metadata_rows: Vec<usize>,
    /// IDs on selected ancestry, oldest first; spans remain in `rows`.
    pub ancestry: Vec<usize>,
    /// All references, including usage targets omitted from retained bodies.
    pub anchor_rows: Vec<usize>,
}
#[derive(Debug)]
pub(crate) struct RawCapture {
    pub cursor: CanonicalCursor,
    pub header: CanonicalJournalRow,
    pub retained: Vec<CanonicalJournalRow>,
    pub plan: SparsePlan,
    file: Arc<File>,
    generation: Generation,
    by_id: HashMap<String, usize>,
}
#[derive(Debug)]
pub(crate) struct RawAppend {
    pub capture: RawCapture,
    /// Complete contiguous suffix; never filtered to a selected branch.
    pub rows: Vec<CanonicalJournalRow>,
}

impl RawCapture {
    /// Standalone request helper until a sparse derived-store factory is available.
    /// `PreferWindow` scans with a 1 MiB body budget even if the file grows during open.
    pub fn capture_checkpoint(
        path: &Path,
        position: RawPosition,
        request: CanonicalSourceRequest,
    ) -> Result<CanonicalCheckpoint> {
        let mode = match request {
            CanonicalSourceRequest::Full => CaptureMode::Full,
            CanonicalSourceRequest::PreferWindow => CaptureMode::Sparse {
                recent_rows: usize::MAX,
                retained_bytes: 1024 * 1024,
            },
        };
        Self::capture(path, position, mode)?.checkpoint(request)
    }
    /// No sparse derived-store factory exists here. Never label a sparse view Full,
    /// and never expand a large `PreferWindow` request into all history bodies.
    pub fn checkpoint(&self, request: CanonicalSourceRequest) -> Result<CanonicalCheckpoint> {
        if (request == CanonicalSourceRequest::PreferWindow
            && self.cursor.frontier.bytes > 1024 * 1024)
            || self.retained.len() != self.plan.rows.len()
        {
            return Err(error(RawErrorKind::WindowUnavailable, "sparse raw plan is certified, but CanonicalWindow derived factory is not implemented"));
        }
        guard(&self.file, &self.cursor.source_path, &self.generation)?;
        Ok(CanonicalCheckpoint {
            cursor: self.cursor.clone(),
            header: self.header.clone(),
            source: CanonicalSource::Full(self.retained.clone()),
        })
    }
    /// Check a prior full cursor against the retained physical prefix and append ancestry.
    /// The caller must separately verify the live owner and current physical source.
    pub(super) fn certifies_prior_prefix(&self, expected: &CanonicalCursor) -> bool {
        if expected == &self.cursor {
            return true;
        }
        if expected.owner != self.cursor.owner
            || expected.session_id != self.cursor.session_id
            || expected.source_path != self.cursor.source_path
            || expected.header_certificate != self.cursor.header_certificate
            || expected.branch_revision != self.cursor.branch_revision
            || expected.canonical_revision > self.cursor.canonical_revision
            || expected.frontier.rows == 0
            || expected.frontier.rows > self.cursor.frontier.rows
            || expected.frontier.bytes > self.cursor.frontier.bytes
            || self.retained.len() != self.plan.rows.len()
        {
            return false;
        }
        if expected.frontier == self.cursor.frontier {
            return expected.raw_source_generation == self.cursor.raw_source_generation
                && expected.selected_leaf == self.cursor.selected_leaf;
        }
        if expected.canonical_revision == self.cursor.canonical_revision {
            return false;
        }
        let mut hash = Sha256::new();
        let mut bytes = 0_u64;
        let mut rows = 0_u64;
        for row in std::iter::once(&self.header).chain(&self.retained) {
            if rows == expected.frontier.rows {
                break;
            }
            hash.update(row.raw());
            bytes += row.raw().len() as u64;
            rows += 1;
        }
        if rows != expected.frontier.rows
            || bytes != expected.frontier.bytes
            || <[u8; 32]>::from(hash.finalize()) != expected.frontier.prefix_certificate
        {
            return false;
        }
        let mut walk = self.cursor.selected_leaf.as_deref();
        while walk != expected.selected_leaf.as_deref() {
            let Some(index) = walk.and_then(|id| self.by_id.get(id)).copied() else {
                return false;
            };
            if self.plan.rows[index].span.row < expected.frontier.rows {
                return false;
            }
            walk = self.plan.rows[index].parent_id.as_deref();
        }
        expected.selected_leaf.as_ref().is_none_or(|id| {
            self.by_id
                .get(id)
                .is_some_and(|index| self.plan.rows[*index].span.row < expected.frontier.rows)
        })
    }

    /// Refresh only the logical owner position after a locked ledger transaction.
    /// Physical identity, bytes, graph and generation remain exactly unchanged.
    pub(super) fn refresh_owner_position(&mut self, position: &RawPosition) {
        debug_assert_eq!(position.owner, self.cursor.owner);
        debug_assert_eq!(position.branch_revision, self.cursor.branch_revision);
        debug_assert_eq!(position.selected_leaf, self.cursor.selected_leaf);
        self.cursor.canonical_revision = position.canonical_revision;
    }

    pub fn capture(path: &Path, position: RawPosition, mode: CaptureMode) -> Result<Self> {
        Self::scan(path, position, mode, None).map(|(capture, _)| capture)
    }
    /// Validates the exact prior cursor and complete physical prefix before returning a tail.
    /// Caller-provided owner/revisions are comparisons, not runtime ownership proof.
    pub fn append_from(
        prior: &Self,
        expected: &CanonicalCursor,
        position: RawPosition,
        mode: CaptureMode,
    ) -> Result<RawAppend> {
        if expected != &prior.cursor
            || position.owner != expected.owner
            || position.canonical_revision < expected.canonical_revision
        {
            return Err(error(
                RawErrorKind::CursorChanged,
                "prior cursor/owner/revision mismatch",
            ));
        }
        if position.branch_revision != expected.branch_revision {
            return Err(error(
                RawErrorKind::BranchChanged,
                "branch revision changed",
            ));
        }
        let (capture, rows) = Self::scan(&expected.source_path, position, mode, Some(prior))?;
        if capture.cursor.session_id != expected.session_id
            || capture.cursor.header_certificate != expected.header_certificate
        {
            return Err(error(RawErrorKind::SourceChanged, "header changed"));
        }
        if capture.cursor.selected_leaf != expected.selected_leaf {
            let mut walk = capture.cursor.selected_leaf.as_deref();
            while walk != expected.selected_leaf.as_deref() {
                let Some(id) = walk else {
                    return Err(error(
                        RawErrorKind::BranchChanged,
                        "new selected leaf is not a descendant",
                    ));
                };
                let index = capture.by_id[id];
                if capture.plan.rows[index].span.row < expected.frontier.rows {
                    return Err(error(
                        RawErrorKind::BranchChanged,
                        "selection moved into the old graph",
                    ));
                }
                walk = capture.plan.rows[index].parent_id.as_deref();
            }
        }
        Ok(RawAppend { capture, rows })
    }
    fn scan(
        path: &Path,
        position: RawPosition,
        mode: CaptureMode,
        prior: Option<&Self>,
    ) -> Result<(Self, Vec<CanonicalJournalRow>)> {
        if !path.is_absolute() {
            return Err(error(
                RawErrorKind::Malformed,
                "source path must be absolute",
            ));
        }
        let before = fs::metadata(path).map_err(io)?;
        if !before.is_file() {
            return Err(error(
                RawErrorKind::Malformed,
                "source is not a regular file",
            ));
        }
        let file = Arc::new(File::open(path).map_err(io)?);
        let generation = Generation::of(&file.metadata().map_err(io)?);
        if generation != Generation::of(&before) {
            return Err(error(
                RawErrorKind::SourceChanged,
                "path changed during open",
            ));
        }
        if let Some(old) = prior {
            if old.generation.dev != generation.dev
                || old.generation.ino != generation.ino
                || generation.size < old.generation.size
                || old.cursor.raw_source_generation != old.generation.certificate()
            {
                return Err(error(
                    RawErrorKind::SourceChanged,
                    "replaced, truncated, or stale source",
                ));
            }
            if generation.size == old.generation.size && generation != old.generation {
                return Err(error(
                    RawErrorKind::SourceChanged,
                    "same-size generation changed",
                ));
            }
            if generation.size - old.generation.size > MAX_TAIL_BYTES {
                return Err(error(
                    RawErrorKind::Limit,
                    "complete tail exceeds bounded receipt limit",
                ));
            }
        }
        // `try_clone` shares the fd offset, so only the initial scan uses sequential I/O;
        // all lazy reads use pread and cannot move that offset.
        let mut reader = BufReader::new(file.try_clone().map_err(io)?);
        let mut hasher = Sha256::new();
        let (mut offset, mut ordinal) = (0_u64, 0_u64);
        let mut header: Option<CanonicalJournalRow> = None;
        let mut session_id = String::new();
        let mut rows: Vec<RowMetadata> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut retained = VecDeque::new();
        let mut retained_bytes = 0_usize;
        let mut tail = Vec::new();
        let mut prefix_checked = prior.is_none();
        loop {
            let mut bytes = Vec::new();
            let count = reader
                .by_ref()
                .take(MAX_ROW_BYTES + 1)
                .read_until(b'\n', &mut bytes)
                .map_err(io)?;
            if count == 0 {
                break;
            }
            if u64::try_from(count)
                .map_err(|_| error(RawErrorKind::Limit, "row length overflow"))?
                > MAX_ROW_BYTES
            {
                return Err(error(RawErrorKind::Limit, "physical row exceeds limit"));
            }
            let mut json = serde_json::Deserializer::from_slice(&bytes);
            <StrictJson as serde::Deserialize>::deserialize(&mut json)
                .and_then(|_| json.end())
                .map_err(|e| error(RawErrorKind::Malformed, format!("row {ordinal}: {e}")))?;
            let row = CanonicalJournalRow::parse(Arc::from(bytes), offset, ordinal)
                .map_err(|e| error(RawErrorKind::Malformed, format!("row {ordinal}: {e}")))?;
            let value: Value = serde_json::from_slice(row.raw())
                .map_err(|e| error(RawErrorKind::Malformed, e.to_string()))?;
            let map = value
                .as_object()
                .ok_or_else(|| error(RawErrorKind::Malformed, "row is not an object"))?;
            let kind = map
                .get("type")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| error(RawErrorKind::Malformed, "missing string type"))?;
            if ordinal == 0 {
                let FileEntry::Header { header: parsed } = row.entry() else {
                    return Err(error(
                        RawErrorKind::Malformed,
                        "first row is not a valid session header",
                    ));
                };
                session_id.clone_from(&parsed.id);
                header = Some(row.clone());
            } else {
                if kind == "session" {
                    return Err(error(RawErrorKind::Malformed, "duplicate header"));
                }
                if matches!(row.entry(), FileEntry::Unknown { .. }) && known_kind(kind) {
                    return Err(error(
                        RawErrorKind::Malformed,
                        format!("malformed known payload: {kind}"),
                    ));
                }
                let id = string_field(map, "id")?;
                let parent_id = string_field(map, "parentId")?;
                let _ = string_field(map, "timestamp")?;
                if let Some(id) = &id {
                    if id == &session_id || by_id.contains_key(id) {
                        return Err(error(RawErrorKind::InvalidGraph, "duplicate entry ID"));
                    }
                } else if parent_id.is_some() || !global_kind(kind) {
                    return Err(error(RawErrorKind::InvalidGraph, "tree row without ID"));
                }
                if let Some(parent) = &parent_id {
                    if !by_id.contains_key(parent) {
                        return Err(error(
                            RawErrorKind::InvalidGraph,
                            "parent is not a prior graph row",
                        ));
                    }
                }
                let anchors = anchors(row.entry());
                for anchor in &anchors {
                    let target = by_id
                        .get(anchor)
                        .and_then(|i| rows.get(*i))
                        .ok_or_else(|| {
                            error(
                                RawErrorKind::InvalidGraph,
                                "reference is not a prior graph row",
                            )
                        })?;
                    if matches!(row.entry(), FileEntry::ChildUsageAttributed { .. })
                        && !target.assistant
                    {
                        return Err(error(
                            RawErrorKind::InvalidGraph,
                            "usage target is not an assistant row",
                        ));
                    }
                }
                let span = RowSpan {
                    offset,
                    len: u64::try_from(count)
                        .map_err(|_| error(RawErrorKind::Limit, "span length overflow"))?,
                    row: ordinal,
                    certificate: digest(row.raw()),
                };
                let meta = RowMetadata {
                    span,
                    id: id.clone(),
                    parent_id,
                    kind: kind.to_owned(),
                    assistant: matches!(
                        row.entry(),
                        FileEntry::Message {
                            message: AgentMessage::Assistant(_),
                            ..
                        }
                    ),
                    anchors,
                };
                if let Some(id) = id {
                    by_id.insert(id, rows.len());
                }
                rows.push(meta);
                retained_bytes = retained_bytes
                    .checked_add(count)
                    .ok_or_else(|| error(RawErrorKind::Limit, "retained bytes overflow"))?;
                retained.push_back(row.clone());
                if let CaptureMode::Sparse {
                    recent_rows,
                    retained_bytes: cap,
                } = mode
                {
                    while retained.len() > recent_rows || retained_bytes > cap {
                        if let Some(removed) = retained.pop_front() {
                            retained_bytes -= removed.raw().len();
                        }
                    }
                }
                if prior.is_some_and(|old| ordinal >= old.cursor.frontier.rows) {
                    tail.push(row.clone());
                }
            }
            hasher.update(row.raw());
            offset = checked(
                offset,
                u64::try_from(count)
                    .map_err(|_| error(RawErrorKind::Limit, "byte count overflow"))?,
            )?;
            ordinal = checked(ordinal, 1)?;
            if let Some(old) = prior {
                if offset == old.cursor.frontier.bytes {
                    if ordinal != old.cursor.frontier.rows
                        || <[u8; 32]>::from(hasher.clone().finalize())
                            != old.cursor.frontier.prefix_certificate
                    {
                        return Err(error(
                            RawErrorKind::SourceChanged,
                            "prior physical prefix changed",
                        ));
                    }
                    prefix_checked = true;
                } else if offset > old.cursor.frontier.bytes && !prefix_checked {
                    return Err(error(
                        RawErrorKind::SourceChanged,
                        "prior frontier is not a row boundary",
                    ));
                }
            }
        }
        if !prefix_checked {
            return Err(error(RawErrorKind::SourceChanged, "missing prior prefix"));
        }
        let header = header.ok_or_else(|| error(RawErrorKind::Malformed, "empty journal"))?;
        if generation.size != offset {
            return Err(error(RawErrorKind::SourceChanged, "captured size differs"));
        }
        guard(&file, path, &generation)?;
        let mut ancestry = Vec::new();
        let mut leaf = position.selected_leaf.as_deref();
        while let Some(id) = leaf {
            let index = *by_id
                .get(id)
                .ok_or_else(|| error(RawErrorKind::InvalidGraph, "selected leaf does not exist"))?;
            ancestry.push(index);
            leaf = rows[index].parent_id.as_deref();
        }
        ancestry.reverse();
        let global_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| row.id.is_none().then_some(i))
            .collect();
        let metadata_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| (row.kind != "message").then_some(i))
            .collect();
        let anchor_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| (!row.anchors.is_empty()).then_some(i))
            .collect();
        let cursor = CanonicalCursor {
            owner: position.owner,
            session_id,
            source_path: path.to_path_buf(),
            raw_source_generation: generation.certificate(),
            header_certificate: digest(header.raw()),
            canonical_revision: position.canonical_revision,
            branch_revision: position.branch_revision,
            selected_leaf: position.selected_leaf,
            frontier: CanonicalFrontier {
                bytes: offset,
                rows: ordinal,
                prefix_certificate: hasher.finalize().into(),
            },
        };
        Ok((
            Self {
                cursor,
                header,
                retained: retained.into_iter().collect(),
                plan: SparsePlan {
                    rows,
                    global_rows,
                    metadata_rows,
                    ancestry,
                    anchor_rows,
                },
                file,
                generation,
                by_id,
            },
            tail,
        ))
    }
    /// Reads only a registered exact span from the captured fd and validates path generation.
    pub fn read_span(
        &self,
        cursor: &CanonicalCursor,
        span: &RowSpan,
    ) -> Result<CanonicalJournalRow> {
        if cursor != &self.cursor {
            return Err(error(RawErrorKind::CursorChanged, "lazy cursor mismatch"));
        }
        let index = span
            .row
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| error(RawErrorKind::InvalidGraph, "header is not a lazy row"))?;
        if self
            .plan
            .rows
            .get(index)
            .is_none_or(|meta| &meta.span != span)
        {
            return Err(error(RawErrorKind::InvalidGraph, "span not in capture"));
        }
        guard(&self.file, &cursor.source_path, &self.generation)?;
        let mut bytes =
            vec![
                0;
                usize::try_from(span.len)
                    .map_err(|_| error(RawErrorKind::Limit, "span allocation overflow"))?
            ];
        self.file
            .read_exact_at(&mut bytes, span.offset)
            .map_err(io)?;
        guard(&self.file, &cursor.source_path, &self.generation)?;
        if digest(&bytes) != span.certificate {
            return Err(error(RawErrorKind::SourceChanged, "lazy row changed"));
        }
        CanonicalJournalRow::parse(Arc::from(bytes), span.offset, span.row)
            .map_err(|e| error(RawErrorKind::Malformed, e.to_string()))
    }
}
fn guard(file: &File, path: &Path, expected: &Generation) -> Result<()> {
    if Generation::of(&file.metadata().map_err(io)?) != *expected
        || Generation::of(&fs::metadata(path).map_err(io)?) != *expected
    {
        return Err(error(
            RawErrorKind::SourceChanged,
            "file or path generation changed",
        ));
    }
    Ok(())
}
fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) if !v.is_empty() => Ok(Some(v.clone())),
        _ => Err(error(RawErrorKind::Malformed, format!("invalid {key}"))),
    }
}
fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "session"
            | "message"
            | "thinking_level_change"
            | "service_tier_change"
            | "model_change"
            | "compaction"
            | "branch_summary"
            | "custom"
            | "child_usage_attributed"
            | "label"
            | "session_info"
            | "session_state"
            | "git_state"
            | "custom_message"
    )
}
fn global_kind(kind: &str) -> bool {
    !known_kind(kind) || matches!(kind, "session_info" | "session_state" | "git_state")
}
fn anchors(entry: &FileEntry) -> Vec<String> {
    match entry {
        FileEntry::Compaction { payload, .. } => {
            let mut ids: Vec<_> = payload.first_kept_entry_id.iter().cloned().collect();
            if let Some(archive) = &payload.context_archive {
                ids.push(archive.prepared_leaf.clone());
                ids.extend(archive.covered_entry_ids.iter().cloned());
            }
            ids
        }
        FileEntry::BranchSummary { payload, .. } => vec![payload.from_id.clone()],
        FileEntry::ChildUsageAttributed { payload, .. } => vec![payload.target_id.clone()],
        FileEntry::Label { payload, .. } => vec![payload.target_id.clone()],
        _ => Vec::new(),
    }
}

/// Validate every nested object before `FileEntry`'s intentional lenient fallback.
struct StrictJson;
impl<'de> serde::Deserialize<'de> for StrictJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<StrictJson, M::Error> {
                let mut keys = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    map.next_value::<StrictJson>()?;
                }
                Ok(StrictJson)
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> std::result::Result<StrictJson, S::Error> {
                while seq.next_element::<StrictJson>()?.is_some() {}
                Ok(StrictJson)
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                _: bool,
            ) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<StrictJson, E> {
                Ok(StrictJson)
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fmt::Write as _;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const HEADER: &str = "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"t\",\"cwd\":\"/w\",\"unknownHeader\":{\"x\":1}}\r\n";
    const A: &str = " {\"type\":\"future_kind\",\"id\":\"a\",\"parentId\":null,\"unknown\":{\"z\":2,\"a\":[1,true]}}\r\n";
    const B: &str = "{\"type\":\"future_kind\",\"id\":\"b\",\"parentId\":null}\n";
    const META: &str = "{\"type\":\"future_global\",\"unmodeled\": {\"bytes\":\"\\u0061\"}}\n";
    fn source(bytes: &[u8]) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }
    fn position(leaf: Option<&str>) -> RawPosition {
        RawPosition {
            owner: CanonicalOwnerScope::new(),
            canonical_revision: 7,
            branch_revision: 3,
            selected_leaf: leaf.map(str::to_owned),
        }
    }
    fn next(old: &RawCapture, leaf: Option<&str>) -> RawPosition {
        RawPosition {
            owner: old.cursor.owner.clone(),
            canonical_revision: old.cursor.canonical_revision + 1,
            branch_revision: old.cursor.branch_revision,
            selected_leaf: leaf.map(str::to_owned),
        }
    }
    fn append(path: &Path, bytes: &[u8]) {
        File::options()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    fn capture(path: &Path, leaf: Option<&str>) -> RawCapture {
        RawCapture::capture(path, position(leaf), CaptureMode::Full).unwrap()
    }
    fn reject(bytes: &[u8]) -> RawError {
        let (_dir, path) = source(bytes);
        RawCapture::capture(&path, position(None), CaptureMode::Full).unwrap_err()
    }

    #[test]
    fn exact_all_physical_rows_branches_unknown_crlf_and_certificates() {
        let bytes = format!("{HEADER}{A}{B}{META}");
        let (_dir, path) = source(bytes.as_bytes());
        let c = capture(&path, Some("a"));
        assert_eq!(c.header.raw(), HEADER.as_bytes());
        assert_eq!(
            c.retained
                .iter()
                .flat_map(|r| r.raw().iter().copied())
                .collect::<Vec<_>>(),
            format!("{A}{B}{META}").as_bytes()
        );
        assert_eq!(
            c.cursor.frontier,
            CanonicalFrontier {
                bytes: bytes.len() as u64,
                rows: 4,
                prefix_certificate: digest(bytes.as_bytes())
            }
        );
        assert_eq!(c.cursor.header_certificate, digest(HEADER.as_bytes()));
        assert_eq!(c.plan.ancestry, vec![0]);
        assert_eq!(c.plan.global_rows, vec![2]);
        let mut offset = HEADER.len() as u64;
        for (i, r) in c.retained.iter().enumerate() {
            assert_eq!((r.byte_offset(), r.physical_row()), (offset, i as u64 + 1));
            assert_eq!(
                (c.plan.rows[i].span.offset(), c.plan.rows[i].span.len()),
                (offset, r.raw().len() as u64)
            );
            offset += r.raw().len() as u64;
        }
        let FileEntry::Unknown { rest } = c.retained[0].entry() else {
            panic!("unknown lost");
        };
        assert_eq!(rest["unknown"], json!({"z":2,"a":[1,true]}));
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[1].span).unwrap().raw(),
            B.as_bytes()
        );
    }
    #[test]
    fn append_returns_complete_contiguous_tail_including_other_branch_and_global() {
        let (_dir, path) = source(format!("{HEADER}{A}{B}").as_bytes());
        let c = capture(&path, Some("a"));
        let tail = format!("{META}{{\"type\":\"future\",\"id\":\"off\",\"parentId\":\"b\"}}\n{{\"type\":\"future\",\"id\":\"on\",\"parentId\":\"a\"}}\n");
        append(&path, tail.as_bytes());
        let result = RawCapture::append_from(
            &c,
            &c.cursor,
            next(&c, Some("on")),
            CaptureMode::Sparse {
                recent_rows: 1,
                retained_bytes: 1024,
            },
        )
        .unwrap();
        assert_eq!(
            result
                .rows
                .iter()
                .flat_map(|r| r.raw().iter().copied())
                .collect::<Vec<_>>(),
            tail.as_bytes()
        );
        assert_eq!(result.rows[0].byte_offset(), c.cursor.frontier.bytes);
        assert_eq!(result.rows[0].physical_row(), c.cursor.frontier.rows);
        assert_eq!(result.capture.plan.rows.len(), 5);
        assert_eq!(result.capture.retained.len(), 1);
        assert_eq!(
            result.capture.cursor.frontier.prefix_certificate,
            digest(&fs::read(&path).unwrap())
        );
    }
    #[test]
    fn prior_prefix_certifies_only_exact_boundaries_and_append_ancestry() {
        let (_dir, path) = source(format!("{HEADER}{A}{B}").as_bytes());
        let old = capture(&path, Some("a"));
        append(
            &path,
            format!("{META}{{\"type\":\"future\",\"id\":\"on\",\"parentId\":\"a\"}}\n").as_bytes(),
        );
        let current =
            RawCapture::append_from(&old, &old.cursor, next(&old, Some("on")), CaptureMode::Full)
                .unwrap()
                .capture;
        assert!(current.certifies_prior_prefix(&old.cursor));
        assert!(current.certifies_prior_prefix(&current.cursor));
        let mut logical = current.cursor.clone();
        logical.canonical_revision -= 1;
        assert!(current.certifies_prior_prefix(&logical));
        let mut forged = old.cursor.clone();
        forged.frontier.prefix_certificate[0] ^= 1;
        assert!(!current.certifies_prior_prefix(&forged));
        forged = old.cursor.clone();
        forged.frontier.bytes -= 1;
        assert!(!current.certifies_prior_prefix(&forged));
        forged = old.cursor.clone();
        forged.frontier.rows -= 1;
        assert!(!current.certifies_prior_prefix(&forged));
        forged = old.cursor.clone();
        forged.selected_leaf = Some("b".into());
        assert!(!current.certifies_prior_prefix(&forged));
        forged = current.cursor.clone();
        forged.raw_source_generation[0] ^= 1;
        assert!(!current.certifies_prior_prefix(&forged));
        forged = old.cursor;
        forged.canonical_revision = current.cursor.canonical_revision;
        assert!(!current.certifies_prior_prefix(&forged));
    }

    #[test]
    fn unchanged_capture_has_empty_tail() {
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        let c = capture(&path, Some("a"));
        assert!(
            RawCapture::append_from(&c, &c.cursor, next(&c, Some("a")), CaptureMode::Full)
                .unwrap()
                .rows
                .is_empty()
        );
    }
    #[test]
    fn changed_prefix_and_header_rejected_even_with_append() {
        for change_header in [false, true] {
            let original = format!("{HEADER}{A}");
            let (_dir, path) = source(original.as_bytes());
            let c = capture(&path, Some("a"));
            let changed = if change_header {
                original.replacen("\"cwd\":\"/w\"", "\"cwd\":\"/x\"", 1)
            } else {
                original.replacen("\"z\":2", "\"z\":3", 1)
            };
            fs::write(&path, format!("{changed}{META}")).unwrap();
            assert_eq!(
                RawCapture::append_from(&c, &c.cursor, next(&c, Some("a")), CaptureMode::Full)
                    .unwrap_err()
                    .kind,
                RawErrorKind::SourceChanged
            );
        }
    }
    #[test]
    fn identical_bytes_replacement_is_not_append_or_valid_lazy_source() {
        let bytes = format!("{HEADER}{A}");
        let (_dir, path) = source(bytes.as_bytes());
        let c = capture(&path, Some("a"));
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, bytes).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert_eq!(
            RawCapture::append_from(&c, &c.cursor, next(&c, Some("a")), CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::SourceChanged
        );
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[0].span)
                .unwrap_err()
                .kind,
            RawErrorKind::SourceChanged
        );
    }
    #[test]
    fn same_size_stale_generation_and_truncation_fail_closed() {
        let bytes = format!("{HEADER}{A}");
        for truncate in [false, true] {
            let (_dir, path) = source(bytes.as_bytes());
            let c = capture(&path, Some("a"));
            if truncate {
                fs::write(&path, HEADER).unwrap();
            } else {
                filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(42, 0)).unwrap();
            }
            assert_eq!(
                RawCapture::append_from(&c, &c.cursor, next(&c, Some("a")), CaptureMode::Full)
                    .unwrap_err()
                    .kind,
                RawErrorKind::SourceChanged
            );
        }
    }
    #[test]
    fn malformed_blank_unterminated_duplicate_headers_and_keys_rejected() {
        for suffix in [
            "\n",
            " \r\n",
            "{oops}\n",
            "{}\n",
            "[]\n",
            "{\"type\":\"future\"}",
            HEADER,
            "{\"type\":\"message\",\"id\":\"m\",\"message\":null}\n",
            "{\"type\":\"future\",\"id\":22}\n",
            "{\"type\":\"future\",\"id\":\"\"}\n",
            "{\"type\":\"future\",\"id\":\"x\",\"parentId\":22}\n",
            "{\"type\":\"future\",\"id\":\"x\",\"id\":\"y\"}\n",
            "{\"type\":\"future\",\"payload\":{\"x\":1,\"x\":2}}\n",
            "{\"type\":\"model_change\",\"id\":\"x\",\"provider\":22}\n",
        ] {
            assert_eq!(
                reject(format!("{HEADER}{suffix}").as_bytes()).kind,
                RawErrorKind::Malformed,
                "suffix {suffix:?}"
            );
        }
        for bytes in ["", "{}\n", "{\"type\":\"session\",\"id\":\"s\"}\n"] {
            assert_eq!(reject(bytes.as_bytes()).kind, RawErrorKind::Malformed);
        }
    }
    #[test]
    fn duplicate_ids_missing_forward_self_parents_and_missing_leaf_rejected() {
        for suffix in [A, "{\"type\":\"future\",\"id\":\"s\"}\n", "{\"type\":\"future\",\"id\":\"x\",\"parentId\":\"absent\"}\n",
            "{\"type\":\"future\",\"id\":\"x\",\"parentId\":\"x\"}\n",
            "{\"type\":\"future\",\"id\":\"x\",\"parentId\":\"y\"}\n{\"type\":\"future\",\"id\":\"y\"}\n",
            "{\"type\":\"future\",\"parentId\":\"a\"}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}}\n"] {
            assert_eq!(reject(format!("{HEADER}{A}{suffix}").as_bytes()).kind,RawErrorKind::InvalidGraph,"suffix {suffix:?}");
        }
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        assert_eq!(
            RawCapture::capture(&path, position(Some("missing")), CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::InvalidGraph
        );
    }
    #[test]
    fn owner_cursor_branch_and_old_sibling_selection_cannot_be_substituted() {
        let (_dir, path) = source(format!("{HEADER}{A}{B}").as_bytes());
        let c = capture(&path, Some("a"));
        let mut p = next(&c, Some("a"));
        p.owner = CanonicalOwnerScope::new();
        assert_eq!(
            RawCapture::append_from(&c, &c.cursor, p, CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::CursorChanged
        );
        let mut p = next(&c, Some("a"));
        p.branch_revision += 1;
        assert_eq!(
            RawCapture::append_from(&c, &c.cursor, p, CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::BranchChanged
        );
        assert_eq!(
            RawCapture::append_from(&c, &c.cursor, next(&c, Some("b")), CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::BranchChanged
        );
        let mut cursor = c.cursor.clone();
        cursor.frontier.rows += 1;
        assert_eq!(
            RawCapture::append_from(&c, &cursor, next(&c, Some("a")), CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::CursorChanged
        );
    }
    #[test]
    fn sparse_keeps_bounded_bodies_and_lazy_older_raw_not_a_derived_loader() {
        let mut bytes = format!("{HEADER}{A}{META}");
        for n in 0..100 {
            writeln!(
                bytes,
                "{{\"type\":\"future\",\"id\":\"r{n}\",\"parentId\":\"a\",\"large\":\"{}\"}}",
                "x".repeat(2048)
            )
            .unwrap();
        }
        let (_dir, path) = source(bytes.as_bytes());
        let c = RawCapture::capture(
            &path,
            position(Some("r99")),
            CaptureMode::Sparse {
                recent_rows: 2,
                retained_bytes: 6000,
            },
        )
        .unwrap();
        assert_eq!(c.plan.rows.len(), 102);
        assert_eq!(c.retained.len(), 2);
        assert!(c.retained.iter().map(|r| r.raw().len()).sum::<usize>() <= 6000);
        assert_eq!(c.plan.ancestry, vec![0, 101]);
        assert_eq!(c.plan.global_rows, vec![1]);
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[0].span).unwrap().raw(),
            A.as_bytes()
        );
        let mut stale = c.cursor.clone();
        stale.canonical_revision += 1;
        assert_eq!(
            c.read_span(&stale, &c.plan.rows[0].span).unwrap_err().kind,
            RawErrorKind::CursorChanged
        );
        append(&path, META.as_bytes());
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[0].span)
                .unwrap_err()
                .kind,
            RawErrorKind::SourceChanged
        );
    }
    #[test]
    fn sparse_usage_anchor_is_certified_without_folding_omitted_assistant() {
        let usage = serde_json::to_value(pa_types::ai::Usage::default()).unwrap();
        let assistant=json!({"type":"message","id":"assistant","parentId":null,"message":{"role":"assistant","content":[],"api":"openai-completions","provider":"openai","model":"test","usage":usage,"stopReason":"stop","timestamp":1}}).to_string()+"\n";
        let attribution=json!({"type":"child_usage_attributed","id":"usage","parentId":"assistant","targetId":"assistant","childUsage":usage,"aggregateUsage":usage}).to_string()+"\n";
        let (_dir, path) = source(format!("{HEADER}{assistant}{attribution}{META}").as_bytes());
        let c = RawCapture::capture(
            &path,
            position(Some("usage")),
            CaptureMode::Sparse {
                recent_rows: 1,
                retained_bytes: 1024,
            },
        )
        .unwrap();
        assert_eq!(c.retained.len(), 1);
        assert_eq!(c.retained[0].raw(), META.as_bytes());
        assert_eq!(c.plan.anchor_rows, vec![1]);
        assert_eq!(c.plan.rows[1].anchors, vec!["assistant"]);
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[0].span).unwrap().raw(),
            assistant.as_bytes()
        );
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[1].span).unwrap().raw(),
            attribution.as_bytes()
        );
    }
    #[test]
    fn missing_anchor_and_non_assistant_usage_target_rejected() {
        let usage = serde_json::to_value(pa_types::ai::Usage::default()).unwrap();
        for target in ["missing", "a"] {
            let attribution=json!({"type":"child_usage_attributed","id":"usage","targetId":target,"childUsage":usage,"aggregateUsage":usage}).to_string()+"\n";
            assert_eq!(
                reject(format!("{HEADER}{A}{attribution}").as_bytes()).kind,
                RawErrorKind::InvalidGraph
            );
        }
    }
    #[test]
    fn checked_frontier_overflow_and_zero_body_sparse() {
        assert_eq!(checked(u64::MAX, 1).unwrap_err().kind, RawErrorKind::Limit);
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        let c = RawCapture::capture(
            &path,
            position(Some("a")),
            CaptureMode::Sparse {
                recent_rows: 0,
                retained_bytes: 0,
            },
        )
        .unwrap();
        assert!(c.retained.is_empty());
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[0].span).unwrap().raw(),
            A.as_bytes()
        );
    }
    #[test]
    fn generation_ignores_atime_but_guards_all_write_generation_fields() {
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        let c = capture(&path, Some("a"));
        let before = c.generation.clone();
        let _ = fs::read(&path).unwrap();
        assert_eq!(before, Generation::of(&fs::metadata(&path).unwrap()));
        let mut altered = before.clone();
        altered.ctime_ns += 1;
        assert_ne!(before.certificate(), altered.certificate());
        assert_eq!(
            guard(&c.file, &path, &altered).unwrap_err().kind,
            RawErrorKind::SourceChanged
        );
    }
    #[test]
    fn sparse_checkpoint_and_large_prefer_window_fail_typed_until_derived_factory() {
        let (_dir, path) = source(format!("{HEADER}{A}{B}").as_bytes());
        let sparse = RawCapture::capture(
            &path,
            position(Some("a")),
            CaptureMode::Sparse {
                recent_rows: 1,
                retained_bytes: 1024,
            },
        )
        .unwrap();
        assert!(matches!(
            sparse.checkpoint(CanonicalSourceRequest::PreferWindow),
            Err(RawError {
                kind: RawErrorKind::WindowUnavailable,
                ..
            })
        ));
        let full = capture(&path, Some("a"));
        assert!(
            matches!(full.checkpoint(CanonicalSourceRequest::PreferWindow).unwrap().source,CanonicalSource::Full(rows) if rows.len()==2)
        );
        let large = format!(
            "{HEADER}{{\"type\":\"future\",\"data\":\"{}\"}}\n",
            "x".repeat(1024 * 1024)
        );
        fs::write(&path, large).unwrap();
        let full = capture(&path, None);
        assert!(full.checkpoint(CanonicalSourceRequest::Full).is_ok());
        assert!(matches!(
            full.checkpoint(CanonicalSourceRequest::PreferWindow),
            Err(RawError {
                kind: RawErrorKind::WindowUnavailable,
                ..
            })
        ));
    }
    #[test]
    fn oversized_physical_row_and_complete_tail_fail_typed() {
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        let c = capture(&path, Some("a"));
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(c.cursor.frontier.bytes + MAX_TAIL_BYTES + 1)
            .unwrap();
        assert_eq!(
            RawCapture::append_from(&c, &c.cursor, next(&c, Some("a")), CaptureMode::Full)
                .unwrap_err()
                .kind,
            RawErrorKind::Limit
        );
        assert_eq!(
            RawCapture::capture(
                &path,
                position(None),
                CaptureMode::Sparse {
                    recent_rows: 0,
                    retained_bytes: 0
                }
            )
            .unwrap_err()
            .kind,
            RawErrorKind::Limit
        );
    }
    #[test]
    fn request_helper_never_expands_large_prefer_window_and_keeps_small_exact() {
        let (_dir, path) = source(format!("{HEADER}{A}").as_bytes());
        let checkpoint = RawCapture::capture_checkpoint(
            &path,
            position(Some("a")),
            CanonicalSourceRequest::PreferWindow,
        )
        .unwrap();
        assert_eq!(checkpoint.header.raw(), HEADER.as_bytes());
        assert!(
            matches!(checkpoint.source,CanonicalSource::Full(rows) if rows[0].raw()==A.as_bytes())
        );
        let large = format!(
            "{HEADER}{{\"type\":\"future\",\"data\":\"{}\"}}\n",
            "x".repeat(1024 * 1024)
        );
        fs::write(&path, large).unwrap();
        assert!(matches!(
            RawCapture::capture_checkpoint(
                &path,
                position(None),
                CanonicalSourceRequest::PreferWindow
            ),
            Err(RawError {
                kind: RawErrorKind::WindowUnavailable,
                ..
            })
        ));
    }
    #[test]
    fn no_retained_boundary_preserves_raw_prefix_and_refuses_invalid_claimed_ids() {
        let archive = json!({
            "type":"compaction", "id":"c", "parentId":"b", "summary":"archive",
            "firstKeptEntryId":null, "tokensBefore":1,
            "contextArchive":{
                "version":1, "strategy":"standard", "compactor":"test",
                "creatorGeneration":"test", "preparedLeaf":"b",
                "coveredEntryIds":["a","b"], "representation":{"kind":"standard"}
            },
            "opaque":{"boundary":""}
        });
        for omitted in [false, true] {
            let mut value = archive.clone();
            if omitted {
                value.as_object_mut().unwrap().remove("firstKeptEntryId");
            }
            let row = value.to_string() + "\n";
            let bytes = format!("{HEADER}{A}{B}{row}").into_bytes();
            let (_dir, path) = source(&bytes);
            let actual = capture(&path, Some("c"));
            assert_eq!(actual.plan.rows[2].anchors, vec!["b", "a", "b"]);
            let retained = actual
                .read_span(&actual.cursor, &actual.plan.rows[2].span)
                .unwrap();
            assert_eq!(retained.raw(), row.as_bytes());
            assert!(
                matches!(retained.entry(), FileEntry::Compaction { payload, .. }
                if payload.first_kept_entry_id.is_none())
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        for boundary in ["", "missing", "c"] {
            let mut value = archive.clone();
            value["firstKeptEntryId"] = json!(boundary);
            let bytes = format!("{HEADER}{A}{B}{value}\n");
            assert_eq!(reject(bytes.as_bytes()).kind, RawErrorKind::InvalidGraph);
        }
        for boundary in [json!(1), json!(false), json!({})] {
            let mut value = archive.clone();
            value["firstKeptEntryId"] = boundary;
            let bytes = format!("{HEADER}{A}{B}{value}\n");
            assert_eq!(reject(bytes.as_bytes()).kind, RawErrorKind::Malformed);
        }
    }

    #[test]
    fn compaction_branch_and_archive_references_are_certified_prior_spans() {
        let compaction=json!({"type":"compaction","id":"c","parentId":"a","summary":"s","firstKeptEntryId":"a","tokensBefore":1,"contextArchive":{"version":1,"strategy":"standard","compactor":"test","creatorGeneration":"test","preparedLeaf":"a","coveredEntryIds":["a"],"representation":{"kind":"standard"}}}).to_string()+"\n";
        let summary=json!({"type":"branch_summary","id":"summary","parentId":"c","fromId":"b","summary":"other branch"}).to_string()+"\n";
        let (_dir, path) = source(format!("{HEADER}{A}{B}{compaction}{summary}{META}").as_bytes());
        let c = RawCapture::capture(
            &path,
            position(Some("summary")),
            CaptureMode::Sparse {
                recent_rows: 0,
                retained_bytes: 0,
            },
        )
        .unwrap();
        assert_eq!(c.plan.anchor_rows, vec![2, 3]);
        assert_eq!(c.plan.rows[2].anchors, vec!["a", "a", "a"]);
        assert_eq!(c.plan.metadata_rows, vec![0, 1, 2, 3, 4]);
        assert_eq!(
            c.read_span(&c.cursor, &c.plan.rows[2].span).unwrap().raw(),
            compaction.as_bytes()
        );
        assert_eq!(
            reject(
                format!(
                    "{HEADER}{A}{B}{}",
                    compaction.replace(
                        "\"coveredEntryIds\":[\"a\"]",
                        "\"coveredEntryIds\":[\"missing\"]"
                    )
                )
                .as_bytes()
            )
            .kind,
            RawErrorKind::InvalidGraph
        );
    }
}
