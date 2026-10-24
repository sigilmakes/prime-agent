//! Generation-certified active session windows. Cold reads scan metadata to root;
//! warm reads touch only the canonical header and the retained transcript suffix.
//! JSONL remains authoritative; historical consumers explicitly hydrate.
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::window_cache::{self, Generation, Snapshot};
pub use super::window_cache::{append_cached, flush as flush_cache, AppendOwnership};
use pa_types::session::{FileEntry, SessionHeader};
use serde::{Deserialize, Serialize};

use super::{build_session_context, SessionContext};

const CHUNK_BYTES: usize = 64 * 1024;
/// Actual source read ranges and sidecar bytes for this open.
#[derive(Default, Clone, Debug)]
pub struct WindowReadStats {
    pub jsonl_bytes: u64,
    pub jsonl_ranges: Vec<(u64, u64)>,
    pub cache_bytes: u64,
    pub cache_hit: bool,
}

struct ReverseLines {
    file: std::fs::File,
    position: u64,
    pending: Vec<u8>,
    reads: WindowReadStats,
    line_start: u64,
}

impl ReverseLines {
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut pieces = Vec::new();
        loop {
            if let Some(index) = self.pending.iter().rposition(|byte| *byte == b'\n') {
                let mut line = self.pending.split_off(index + 1);
                self.pending.pop();
                self.line_start = self.position + self.pending.len() as u64 + 1;
                for piece in pieces.into_iter().rev() {
                    line.extend(piece);
                }
                if !line.is_empty() {
                    return Ok(Some(line));
                }
                pieces = Vec::new();
                continue;
            }
            pieces.push(std::mem::take(&mut self.pending));
            if self.position == 0 {
                self.line_start = 0;
                let line: Vec<u8> = pieces.into_iter().rev().flatten().collect();
                return Ok((!line.is_empty()).then_some(line));
            }
            let count = self.position.min(CHUNK_BYTES as u64) as usize;
            self.position -= count as u64;
            self.file.seek(SeekFrom::Start(self.position))?;
            self.pending.resize(count, 0);
            self.file.read_exact(&mut self.pending)?;
            self.reads.jsonl_bytes += count as u64;
            self.reads.jsonl_ranges.push((self.position, count as u64));
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    parent_id: Option<String>,
    message: Option<MessageMetadata>,
    custom_type: Option<String>,
}

#[derive(Deserialize)]
struct MessageMetadata {
    role: String,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<serde_json::Value>,
    content: Option<MessageContentMetadata>,
}

enum MessageContentMetadata {
    Blocks(Vec<ContentMetadata>),
    Text,
}

impl<'de> Deserialize<'de> for MessageContentMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> serde::de::Visitor<'de> for ContentVisitor {
            type Value = MessageContentMetadata;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("message text or content blocks")
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(MessageContentMetadata::Text)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element()? {
                    blocks.push(block);
                }
                Ok(MessageContentMetadata::Blocks(blocks))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

#[derive(Deserialize)]
struct ContentMetadata {
    #[serde(rename = "type")]
    kind: String,
}

/// Whole-branch accounting older than the retained suffix.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct WindowStats {
    pub total_messages: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_results: u64,
    pub tool_calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(with = "super::window_cache::float_bits")]
    pub cost: f64,
}

/// One older-path assistant row's spend-relevant usage: an attribution
/// targeting the row replaces it with the cumulative aggregate.
#[derive(Debug, Clone, Copy, Default)]
struct OlderPathUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

impl OlderPathUsage {
    fn from_usage(usage: &serde_json::Value) -> Self {
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default()
        };
        Self {
            input: field("input"),
            output: field("output"),
            cache_read: field("cacheRead"),
            cache_write: field("cacheWrite"),
            cost: usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or_default(),
        }
    }
}

/// Owner-captured physical row facts. These are implementation inputs, not authority.
/// Bodies omitted from the window must not be loaded to populate this index.
#[derive(Clone, Debug)]
pub struct CanonicalWindowRow {
    pub byte_offset: u64,
    pub byte_len: u64,
    pub physical_row: u64,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub kind: String,
    pub assistant_model: Option<(String, String)>,
}

/// In-memory projection inputs supplied by the bound canonical raw owner.
/// The owner certifies the index, ancestry, metadata and older statistics against
/// its capture. The cache snapshot created here is NEVER a raw-source proof.
pub struct CanonicalWindowParts {
    pub cursor: super::canonical::CanonicalCursor,
    pub header: super::canonical::CanonicalJournalRow,
    pub retained: Vec<super::canonical::CanonicalJournalRow>,
    /// ALL physical non-message rows, including all branches and retained rows.
    pub metadata: Vec<super::canonical::CanonicalJournalRow>,
    /// Every non-header physical row, in physical order.
    pub rows: Vec<CanonicalWindowRow>,
    /// Selected ancestry, oldest first; zero-based indices into `rows`.
    pub ancestry: Vec<usize>,
    /// Actual metadata already captured by the owner; never stat the path here.
    pub physical_metadata: std::fs::Metadata,
    /// Selected ancestry messages absent from retained bodies, with the LAST
    /// physical attribution already folded by the owner. Raw rows stay unchanged.
    pub older_path_stats: WindowStats,
    pub first_user_message: Option<serde_json::Value>,
}

/// What this store holds for the retained rows. `full` and `detached` are one
/// lifecycle, not two flags: construction picks the window or whole-file arm,
/// [`WindowedSessionStore::take_retained`] moves to the detached arm at the same
/// completeness, and [`WindowedSessionStore::ensure_full_history`] returns to
/// `WholeFile`. The detached arms still report their completeness: an adopted
/// whole-file capture keeps skipping the settings overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyState {
    /// The compacted window's retained suffix, owned by this store.
    Window,
    /// Every file row, owned by this store (a whole-file walk or re-read).
    WholeFile,
    /// The rows went to the manager ([`WindowedSessionStore::take_retained`]).
    DetachedWindow,
    /// A whole-file capture whose rows went to the manager.
    DetachedWholeFile,
}

impl BodyState {
    /// Whether the whole history is available to this store's readers.
    const fn is_full(self) -> bool {
        matches!(self, Self::WholeFile | Self::DetachedWholeFile)
    }

    /// Whether the retained rows were handed to the manager.
    const fn is_detached(self) -> bool {
        matches!(self, Self::DetachedWindow | Self::DetachedWholeFile)
    }

    /// The same completeness with the rows handed over.
    const fn detached(self) -> Self {
        if self.is_full() {
            Self::DetachedWholeFile
        } else {
            Self::DetachedWindow
        }
    }
}

/// An active compacted window plus verified settings from its older ancestry.
/// The underlying JSONL is never rewritten by this reader.
// One state value, not four flags: `bodies` is the body lifecycle above, the
// walk's `retained_whole_file` fact lives in the snapshot it was certified
// with, and `canonical_derived` is the immutable construction kind (a derived
// projection over an owner-certified capture), which no transition changes and
// no other field implies.
pub struct WindowedSessionStore {
    path: PathBuf,
    entries: Vec<FileEntry>,
    raw_entries: Vec<String>,
    metadata_entries: Vec<String>,
    message_count: usize,
    older_path_stats: WindowStats,
    first_user_message: Option<serde_json::Value>,
    leaf_id: String,
    settings: SessionContext,
    boundary_model: Option<(String, String)>,
    /// The body lifecycle; a second resident copy measured ~29.5MiB on the
    /// 10MiB fixture, so the detached arms hold none.
    bodies: BodyState,
    snapshot: Snapshot,
    reads: WindowReadStats,
    /// Canonical-derived stores must never reopen a source or publish a cache.
    canonical_derived: bool,
}

impl WindowedSessionStore {
    /// Build only a derived projection of an owner-certified raw capture.
    /// No file, sidecar, cache loader, or lazy historical reader is consulted.
    /// Source certificates and lazy reads remain the bound owner's responsibility.
    ///
    /// # Errors
    /// Reject inconsistent physical facts, header, index, retained rows or ancestry.
    #[allow(clippy::too_many_lines)]
    pub fn from_canonical_parts(parts: CanonicalWindowParts) -> anyhow::Result<Self> {
        use sha2::{Digest, Sha256};
        let CanonicalWindowParts {
            cursor,
            header,
            retained,
            metadata,
            rows,
            ancestry,
            physical_metadata,
            older_path_stats,
            first_user_message,
        } = parts;
        let FileEntry::Header {
            header: parsed_header,
        } = header.entry()
        else {
            anyhow::bail!("canonical window header is not a header");
        };
        anyhow::ensure!(
            header.byte_offset() == 0 && header.physical_row() == 0,
            "canonical header position mismatch"
        );
        anyhow::ensure!(
            parsed_header.id == cursor.session_id
                && parsed_header.version == Some(3)
                && <[u8; 32]>::from(Sha256::digest(header.raw())) == cursor.header_certificate,
            "canonical header/cursor mismatch"
        );
        anyhow::ensure!(
            cursor.source_path.is_absolute()
                && physical_metadata.is_file()
                && physical_metadata.len() == cursor.frontier.bytes,
            "canonical physical metadata/frontier mismatch"
        );
        anyhow::ensure!(
            u64::try_from(rows.len())?.checked_add(1) == Some(cursor.frontier.rows),
            "canonical physical row count mismatch"
        );
        let mut offset = u64::try_from(header.raw().len())?;
        let mut ids = HashMap::new();
        for (index, row) in rows.iter().enumerate() {
            anyhow::ensure!(
                row.physical_row == u64::try_from(index)? + 1
                    && row.byte_offset == offset
                    && row.byte_len > 1
                    && !row.kind.is_empty()
                    && row.kind != "session",
                "invalid canonical physical row position/type"
            );
            offset = offset
                .checked_add(row.byte_len)
                .ok_or_else(|| anyhow::anyhow!("canonical offset overflow"))?;
            if let Some(parent) = &row.parent_id {
                anyhow::ensure!(ids.contains_key(parent), "canonical parent is not prior");
            }
            if let Some(id) = &row.id {
                anyhow::ensure!(
                    !id.is_empty()
                        && id != &cursor.session_id
                        && ids.insert(id.clone(), index).is_none(),
                    "duplicate canonical ID"
                );
            } else {
                anyhow::ensure!(row.parent_id.is_none(), "IDless canonical tree row");
            }
            anyhow::ensure!(
                row.assistant_model.is_none() || row.kind == "message",
                "assistant facts on non-message"
            );
        }
        anyhow::ensure!(
            offset == cursor.frontier.bytes,
            "canonical physical byte frontier mismatch"
        );
        let leaf_id = cursor.selected_leaf.as_ref().ok_or_else(|| {
            anyhow::anyhow!("a derived window requires an explicit selected leaf")
        })?;
        let mut expected_ancestry = Vec::new();
        let mut next = Some(leaf_id.as_str());
        while let Some(id) = next {
            let index = *ids
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("selected ancestry ID absent"))?;
            expected_ancestry.push(index);
            next = rows[index].parent_id.as_deref();
        }
        expected_ancestry.reverse();
        anyhow::ensure!(
            expected_ancestry == ancestry,
            "canonical selected ancestry mismatch"
        );
        let validate = |row: &super::canonical::CanonicalJournalRow| -> anyhow::Result<usize> {
            let index = usize::try_from(row.physical_row())?
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("duplicate canonical header"))?;
            let fact = rows
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("raw row outside frontier"))?;
            let value: serde_json::Value = serde_json::from_slice(row.raw())?;
            anyhow::ensure!(
                row.byte_offset() == fact.byte_offset
                    && u64::try_from(row.raw().len())? == fact.byte_len
                    && value.get("type").and_then(serde_json::Value::as_str)
                        == Some(fact.kind.as_str())
                    && value.get("id").and_then(serde_json::Value::as_str) == fact.id.as_deref()
                    && value.get("parentId").and_then(serde_json::Value::as_str)
                        == fact.parent_id.as_deref(),
                "canonical row/index mismatch"
            );
            let model = match row.entry() {
                FileEntry::Message {
                    message: pa_types::session::AgentMessage::Assistant(message),
                    ..
                } => Some((message.provider.clone(), message.model.clone())),
                _ => None,
            };
            anyhow::ensure!(
                model == fact.assistant_model,
                "canonical assistant facts mismatch"
            );
            Ok(index)
        };
        let mut retained_indices = HashSet::new();
        let mut previous = None;
        for row in &retained {
            let index = validate(row)?;
            anyhow::ensure!(
                previous.is_none_or(|previous| index > previous),
                "canonical retained rows not strictly ordered"
            );
            previous = Some(index);
            retained_indices.insert(index);
        }
        let expected_metadata: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| (row.kind != "message").then_some(i))
            .collect();
        let mut metadata_indices = Vec::new();
        for row in &metadata {
            metadata_indices.push(validate(row)?);
        }
        anyhow::ensure!(
            metadata_indices == expected_metadata,
            "incomplete/unordered physical metadata"
        );
        let metadata_by_index: HashMap<usize, _> =
            metadata_indices.into_iter().zip(&metadata).collect();
        for row in &retained {
            let index = usize::try_from(row.physical_row())? - 1;
            if let Some(meta) = metadata_by_index.get(&index) {
                anyhow::ensure!(meta.raw() == row.raw(), "retained metadata bytes disagree");
            }
        }
        let older_messages = ancestry
            .iter()
            .filter(|i| !retained_indices.contains(i) && rows[**i].kind == "message")
            .count();
        anyhow::ensure!(
            older_path_stats.total_messages == u64::try_from(older_messages)?
                && older_path_stats
                    .user_messages
                    .checked_add(older_path_stats.assistant_messages)
                    .and_then(|n| n.checked_add(older_path_stats.tool_results))
                    == Some(older_path_stats.total_messages)
                && older_path_stats.cost.is_finite(),
            "older selected statistics mismatch"
        );
        let raw_text = |row: &super::canonical::CanonicalJournalRow| -> anyhow::Result<String> {
            Ok(std::str::from_utf8(row.raw())?.to_owned())
        };
        let metadata_entries: Vec<String> = metadata
            .iter()
            .map(raw_text)
            .collect::<anyhow::Result<_>>()?;
        let raw_entries = retained
            .iter()
            .map(raw_text)
            .collect::<anyhow::Result<_>>()?;
        let mut entries = vec![header.entry().clone()];
        entries.extend(retained.iter().map(|row| row.entry().clone()));
        // Apply ALL physical attributions once, oldest first, to DERIVED clones.
        // Do not re-fold a retained attribution afterward: an older retained row
        // must not overwrite the final aggregate from the complete metadata list.
        apply_attributions(&mut entries, &metadata_entries);
        let retained_whole_file = retained_indices.len() == rows.len();
        let message_count = rows.iter().filter(|row| row.kind == "message").count();
        let compaction_count = rows.iter().filter(|row| row.kind == "compaction").count();
        let mut snapshot = Snapshot {
            version: window_cache::SNAPSHOT_VERSION,
            generation: Generation::of(&physical_metadata),
            header: raw_text(&header)?,
            start: retained.first().map_or(
                cursor.frontier.bytes,
                super::canonical::CanonicalJournalRow::byte_offset,
            ),
            leaf: leaf_id.clone(),
            thinking: "off".to_owned(),
            thinking_present: false,
            tier: None,
            tier_present: false,
            model: None,
            boundary_model: None,
            metadata: metadata_entries.clone(),
            message_count,
            compaction_count,
            stats: older_path_stats.clone(),
            first_user: first_user_message.clone(),
            goal: None,
            anthropic_warning_shown: false,
            non_bootstrap: false,
            retained_whole_file,
        };
        // Metadata for other branches remains raw, but cannot select settings.
        for index in &ancestry {
            if let Some(row) = metadata_by_index.get(index) {
                update_snapshot(&mut snapshot, row.entry());
                if !retained_indices.contains(index) {
                    if let FileEntry::ModelChange { payload, .. } = row.entry() {
                        snapshot.boundary_model =
                            Some((payload.provider.clone(), payload.model_id.clone()));
                    }
                }
            }
            if let Some(model) = &rows[*index].assistant_model {
                snapshot.model = Some(model.clone());
            }
        }
        // Counts are physical, not counts of the selected metadata fold.
        snapshot.message_count = message_count;
        snapshot.compaction_count = compaction_count;
        snapshot.non_bootstrap = rows.iter().any(|row| {
            !matches!(
                row.kind.as_str(),
                "model_change" | "thinking_level_change" | "service_tier_change"
            ) && metadata_by_index
                .get(
                    &(usize::try_from(row.physical_row)
                        .unwrap_or(0)
                        .saturating_sub(1)),
                )
                .is_none_or(|meta| !super::is_bootstrap_entry(meta.entry()))
        });
        let settings = SessionContext {
            archive: None,
            messages: Vec::new(),
            thinking_level: snapshot.thinking.clone(),
            service_tier: snapshot.tier,
            model: snapshot.model.clone(),
        };
        Ok(Self {
            path: cursor.source_path,
            entries,
            raw_entries,
            metadata_entries,
            message_count,
            older_path_stats,
            first_user_message,
            leaf_id: leaf_id.clone(),
            settings,
            boundary_model: snapshot.boundary_model.clone(),
            bodies: if retained_whole_file {
                BodyState::WholeFile
            } else {
                BodyState::Window
            },
            snapshot,
            reads: WindowReadStats::default(),
            canonical_derived: true,
        })
    }

    /// Return `None` for old schemas, torn rows, or ambiguous ancestry so callers
    /// can use their ordinary full reader. No size or message-count admission cap.
    ///
    /// # Errors
    ///
    /// I/O error when the file cannot be opened or read; other failures yield `Ok(None)`.
    ///
    /// # Panics
    ///
    /// The two internal `expect`s cannot fire (rows parse first; the header is proven).
    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        // The generation certificate anchors on unix inode identity; a
        // same-length replace is indistinguishable under weak non-unix metadata.
        if !cfg!(unix) {
            return Ok(None);
        }
        let mut file = std::fs::File::open(path)?;
        let generation = Generation::of(&file.metadata()?);
        let size = file.metadata()?.len();
        let mut reads = WindowReadStats::default();
        if let Some(snapshot) = window_cache::load(path, &file, &mut reads) {
            if let Some(store) = Self::from_snapshot(path, &mut file, snapshot, reads.clone())? {
                return Ok(Some(store));
            }
        }
        if size == 0 {
            return Ok(None);
        }
        // Appending to an unterminated row would merge two JSON records.
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Ok(None);
        }
        reads.jsonl_bytes += 1;
        reads.jsonl_ranges.push((size - 1, 1));
        let mut reader = ReverseLines {
            file,
            position: size,
            pending: Vec::new(),
            line_start: 0,
            reads,
        };
        let mut retained = Vec::new();
        let mut raw_entries = Vec::new();
        let mut metadata_entries = Vec::new();
        let mut message_count = 0;
        let mut older_path_stats = WindowStats::default();
        let mut older_usage: Vec<(String, OlderPathUsage)> = Vec::new();
        let mut older_aggregates: HashMap<String, OlderPathUsage> = HashMap::new();
        let mut first_user_line = None;
        let mut expected: Option<String> = None;
        let mut leaf_id = None;
        let mut first_kept = None;
        let mut found_boundary = false;
        let mut retained_start = 0;
        let mut compaction_count = 0;
        let mut non_bootstrap = false;
        let mut goal = None;
        // The Anthropic subscription warning's once-per-session-lifecycle
        // gate: set when the walk meets the marked-shown custom row (any
        // position on the active branch hydrates it, exactly like `goal`).
        let mut anthropic_warning_shown = false;
        let mut window_done = false;
        let mut seen = HashSet::new();
        let mut thinking = None;
        let mut tier = None;
        let mut model = None;
        // The boundary model: the usage fold seeds its timeline with this,
        // not `model` (the leaf's).
        let mut boundary_model = None;
        let mut header = None;
        while let Some(line) = reader.next()? {
            let Ok(text) = std::str::from_utf8(&line) else {
                return Ok(None);
            };
            let Ok(meta) = serde_json::from_str::<Envelope>(text) else {
                return Ok(None);
            };
            if meta.kind == "session" {
                if reader.position != 0
                    || reader
                        .pending
                        .iter()
                        .any(|byte| !byte.is_ascii_whitespace())
                {
                    return Ok(None);
                }
                header = serde_json::from_slice::<FileEntry>(&line).ok();
                break;
            }
            if !(matches!(
                meta.kind.as_str(),
                "model_change" | "thinking_level_change" | "service_tier_change"
            ) || meta.kind == "custom"
                && meta.custom_type.as_deref()
                    == Some(pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE))
            {
                non_bootstrap = true;
            }
            if meta.kind == "compaction" {
                compaction_count += 1;
            }
            if meta.kind == "message" {
                message_count += 1;
                if meta
                    .message
                    .as_ref()
                    .is_some_and(|message| message.role == "user")
                {
                    first_user_line = Some(line.clone());
                }
            } else if window_done
                && (matches!(
                    meta.kind.as_str(),
                    "session_info"
                        | "session_state"
                        | "agent_status"
                        | "git_state"
                        | "child_usage_attributed"
                ) || matches!(
                    (meta.kind.as_str(), meta.custom_type.as_deref()),
                    (
                        "custom",
                        Some(
                            "provider_quota_park"
                                | "provider_quota_resume"
                                | pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE
                        )
                    )
                ))
            {
                metadata_entries.push(
                    String::from_utf8(line.clone())
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                );
            }
            if meta.kind == "child_usage_attributed" {
                // Older-path targets are dropped from the retained metadata, so
                // capture here or their attributed spend is lost. Newest-first,
                // the FIRST aggregate seen per target is the cumulative one.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                    if let Some(target) = value.get("targetId").and_then(serde_json::Value::as_str)
                    {
                        // A malformed aggregate (null, a scalar) must not
                        // replace a valid row usage with zeros.
                        if let Some(aggregate) = value
                            .get("aggregateUsage")
                            .filter(|aggregate| aggregate.is_object())
                        {
                            older_aggregates
                                .entry(target.to_owned())
                                .or_insert_with(|| OlderPathUsage::from_usage(aggregate));
                        }
                    }
                }
            }
            let Some(id) = meta.id.as_deref() else {
                return Ok(None);
            };
            if !seen.insert(id.to_owned()) {
                return Ok(None);
            }
            if leaf_id.is_none() {
                leaf_id = Some(id.to_owned());
                expected.clone_from(&leaf_id);
            }
            let on_path = expected.as_deref() == Some(id);
            if on_path {
                if window_done && meta.kind == "message" {
                    if let Some(message) = &meta.message {
                        older_path_stats.total_messages += 1;
                        match message.role.as_str() {
                            "user" => older_path_stats.user_messages += 1,
                            "toolResult" => older_path_stats.tool_results += 1,
                            "assistant" => {
                                older_path_stats.assistant_messages += 1;
                                if let Some(MessageContentMetadata::Blocks(blocks)) =
                                    &message.content
                                {
                                    older_path_stats.tool_calls += blocks
                                        .iter()
                                        .filter(|block| block.kind == "toolCall")
                                        .count()
                                        as u64;
                                }
                                // Recorded per row, not summed inline: an attribution
                                // replaces its usage with the cumulative aggregate.
                                older_usage.push((
                                    id.to_owned(),
                                    message
                                        .usage
                                        .as_ref()
                                        .map(OlderPathUsage::from_usage)
                                        .unwrap_or_default(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                expected.clone_from(&meta.parent_id);
                if let Some(message) = meta
                    .message
                    .as_ref()
                    .filter(|message| message.role == "assistant" && model.is_none())
                {
                    let (Some(provider), Some(name)) = (&message.provider, &message.model) else {
                        return Ok(None);
                    };
                    model = Some((provider.clone(), name.clone()));
                }
            }
            // Parse only retained bodies and sparse setting records; serde
            // ignores the older ancestry without allocating it.
            let entry = if !window_done
                || (on_path
                    && (matches!(
                        meta.kind.as_str(),
                        "model_change" | "thinking_level_change" | "service_tier_change"
                    ) || (goal.is_none()
                        && meta.custom_type.as_deref()
                            == Some(crate::goals::GOAL_STATE_CUSTOM_TYPE))
                        || (!anthropic_warning_shown
                            && meta.custom_type.as_deref()
                                == Some(crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE))))
            {
                match serde_json::from_slice::<FileEntry>(&line) {
                    Ok(entry) => Some(entry),
                    Err(_) => return Ok(None),
                }
            } else {
                None
            };
            if on_path {
                if goal.is_none() {
                    goal = entry.as_ref().and_then(valid_goal);
                }
                if !anthropic_warning_shown {
                    anthropic_warning_shown = entry.as_ref().is_some_and(valid_warning_shown);
                }
                match entry.as_ref() {
                    Some(FileEntry::ThinkingLevelChange { payload, .. }) if thinking.is_none() => {
                        thinking = Some(payload.thinking_level.clone());
                    }
                    Some(FileEntry::ServiceTierChange { payload, .. }) if tier.is_none() => {
                        tier = Some(payload.service_tier);
                    }
                    Some(FileEntry::ModelChange { payload, .. }) => {
                        if model.is_none() {
                            model = Some((payload.provider.clone(), payload.model_id.clone()));
                        }
                        // The boundary model: the newest `model_change` in the discarded prefix.
                        if window_done && boundary_model.is_none() {
                            boundary_model =
                                Some((payload.provider.clone(), payload.model_id.clone()));
                        }
                    }
                    Some(FileEntry::Compaction { payload, .. }) if first_kept.is_none() => {
                        found_boundary = payload.first_kept_entry_id.is_none();
                        first_kept = Some(payload.first_kept_entry_id.clone());
                    }
                    _ => {}
                }
                if first_kept.as_ref().and_then(Option::as_deref) == Some(id) {
                    found_boundary = true;
                }
            }
            if !window_done {
                retained_start = reader.line_start;
                retained.push(entry.expect("window entries parsed"));
                raw_entries.push(
                    String::from_utf8(line)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                );
                window_done = found_boundary;
            }
        }
        let Some(FileEntry::Header {
            header: SessionHeader {
                version: Some(3), ..
            },
        }) = &header
        else {
            return Ok(None);
        };
        if expected.is_some() {
            return Ok(None);
        }
        retained.push(header.expect("header checked"));
        retained.reverse();
        raw_entries.reverse();
        metadata_entries.reverse();
        let retained_ids: HashSet<&str> = retained.iter().filter_map(FileEntry::id).collect();
        let mut latest = std::collections::HashMap::new();
        let mut keep = HashSet::new();
        for (index, row) in metadata_entries.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(row)?;
            let kind = value["type"].as_str().unwrap_or("");
            match kind {
                "session_info" | "session_state" | "git_state" => {
                    latest.insert(kind.to_owned(), index);
                }
                "child_usage_attributed"
                    if value["targetId"]
                        .as_str()
                        .is_some_and(|id| retained_ids.contains(id)) =>
                {
                    latest.insert(format!("attribution:{}", value["targetId"]), index);
                }
                // Quota-park episodes end on resume rows, so every
                // park/resume row must survive the keep filter.
                "custom"
                    if matches!(
                        value["customType"].as_str(),
                        Some(
                            "provider_quota_park"
                                | "provider_quota_resume"
                                | pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE
                        )
                    ) =>
                {
                    keep.insert(index);
                }
                _ => {}
            }
        }
        keep.extend(latest.into_values());
        metadata_entries = metadata_entries
            .into_iter()
            .enumerate()
            .filter_map(|(index, row)| keep.contains(&index).then_some(row))
            .collect();
        // Fold oldest-first (the cost sum's float order matches the pre-fold
        // walk): an attributed assistant reports its cumulative aggregate.
        for (id, usage) in older_usage.iter().rev() {
            let folded = older_aggregates.get(id).unwrap_or(usage);
            older_path_stats.input += folded.input;
            older_path_stats.output += folded.output;
            older_path_stats.cache_read += folded.cache_read;
            older_path_stats.cache_write += folded.cache_write;
            older_path_stats.cost += folded.cost;
        }
        let first_user_message = first_user_line
            .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
            .and_then(|row| row.get("message").cloned());
        let thinking_present = thinking.is_some();
        let tier_present = tier.is_some();
        let Some(leaf_id) = leaf_id else {
            return Ok(None);
        };
        let snapshot = Snapshot {
            version: window_cache::SNAPSHOT_VERSION,
            generation: generation.clone(),
            header: serde_json::to_string(&retained[0])?,
            start: retained_start,
            leaf: leaf_id.clone(),
            thinking: thinking.clone().unwrap_or_else(|| "off".to_owned()),
            thinking_present,
            tier: tier.flatten(),
            tier_present,
            model: model.clone(),
            boundary_model: boundary_model.clone(),
            metadata: metadata_entries.clone(),
            message_count,
            compaction_count,
            stats: older_path_stats.clone(),
            first_user: first_user_message.clone(),
            goal,
            anthropic_warning_shown,
            non_bootstrap,
            retained_whole_file: !window_done,
        };
        if !generation.valid(&reader.file, path)? {
            return Ok(None);
        }
        // A disposable sidecar failure must never prevent opening the source.
        let _ = window_cache::save(path, &snapshot);
        apply_attributions(&mut retained, &metadata_entries);
        super::apply_child_usage_attributions(&mut retained);
        Ok(Some(Self {
            path: path.to_owned(),
            entries: retained,
            raw_entries,
            metadata_entries,
            message_count,
            older_path_stats,
            first_user_message,
            leaf_id,
            settings: SessionContext {
                archive: None,
                messages: Vec::new(),
                thinking_level: thinking.unwrap_or_else(|| "off".to_owned()),
                service_tier: tier.flatten(),
                model,
            },
            boundary_model,
            bodies: BodyState::Window,
            snapshot,
            reads: reader.reads,
            canonical_derived: false,
        }))
    }

    fn from_snapshot(
        path: &Path,
        file: &mut std::fs::File,
        snapshot: Snapshot,
        mut reads: WindowReadStats,
    ) -> io::Result<Option<Self>> {
        file.seek(SeekFrom::Start(0))?;
        let mut header = Vec::new();
        let mut byte = [0];
        while file.read(&mut byte)? != 0 {
            header.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        reads.jsonl_bytes += header.len() as u64;
        reads.jsonl_ranges.push((0, header.len() as u64));
        let Ok(canonical) = serde_json::from_slice::<FileEntry>(&header) else {
            return Ok(None);
        };
        if serde_json::to_string(&canonical)? != snapshot.header
            || snapshot.start < header.len() as u64
            || snapshot.start > file.metadata()?.len()
        {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(snapshot.start))?;
        let mut suffix = String::new();
        file.read_to_string(&mut suffix)?;
        reads.jsonl_bytes += suffix.len() as u64;
        reads
            .jsonl_ranges
            .push((snapshot.start, suffix.len() as u64));
        let raw_entries: Vec<String> = suffix
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let mut entries = vec![canonical];
        for line in &raw_entries {
            let Ok(entry) = serde_json::from_str::<FileEntry>(line) else {
                return Ok(None);
            };
            entries.push(entry);
        }
        if entries.last().and_then(FileEntry::id) != Some(snapshot.leaf.as_str())
            || !snapshot.generation.valid(file, path)?
        {
            return Ok(None);
        }
        apply_attributions(&mut entries, &snapshot.metadata);
        super::apply_child_usage_attributions(&mut entries);
        reads.cache_hit = true;
        Ok(Some(Self {
            path: path.to_owned(),
            entries,
            raw_entries,
            metadata_entries: snapshot.metadata.clone(),
            message_count: snapshot.message_count,
            older_path_stats: snapshot.stats.clone(),
            first_user_message: snapshot.first_user.clone(),
            leaf_id: snapshot.leaf.clone(),
            settings: SessionContext {
                archive: None,
                messages: Vec::new(),
                thinking_level: snapshot.thinking.clone(),
                service_tier: snapshot.tier,
                model: snapshot.model.clone(),
            },
            boundary_model: snapshot.boundary_model.clone(),
            bodies: BodyState::Window,
            snapshot,
            reads,
            canonical_derived: false,
        }))
    }
    /// Move the retained raw JSONL rows out; holding both the raw lines and
    /// the parsed store doubles the load's resident peak.
    pub fn take_raw_entries(&mut self) -> Vec<String> {
        std::mem::take(&mut self.raw_entries)
    }

    /// Move the retained metadata rows out (same single-consumer contract).
    pub fn take_metadata_entries(&mut self) -> Vec<String> {
        std::mem::take(&mut self.metadata_entries)
    }

    /// Move the retained typed rows AND raw JSONL lines out for a one-copy
    /// adoption ([`super::manager::SessionManager::adopt_window`]): the
    /// manager owns the parsed trees; a detached window's context comes from it.
    pub fn take_retained(&mut self) -> (Vec<FileEntry>, Vec<String>) {
        self.bodies = self.bodies.detached();
        (
            std::mem::take(&mut self.entries),
            std::mem::take(&mut self.raw_entries),
        )
    }

    #[must_use]
    pub fn retained_detached(&self) -> bool {
        self.bodies.is_detached()
    }

    #[must_use]
    pub fn full_history(&self) -> bool {
        self.bodies.is_full()
    }

    /// Whether the walk retained every file row (no compaction boundary):
    /// the manager's entries already cover every persisted row.
    #[must_use]
    pub fn retained_whole_file(&self) -> bool {
        self.snapshot.retained_whole_file
    }

    /// The walk-resolved settings (the owning manager's context overlay
    /// source while this window is attached).
    #[must_use]
    pub fn settings(&self) -> &SessionContext {
        &self.settings
    }

    #[must_use]
    pub fn has_non_bootstrap_entries(&self) -> bool {
        self.snapshot.non_bootstrap
    }
    #[must_use]
    pub fn read_stats(&self) -> &WindowReadStats {
        &self.reads
    }
    #[must_use]
    pub fn compaction_count(&self) -> usize {
        self.snapshot.compaction_count
    }
    #[must_use]
    pub fn goal_state(&self) -> Option<&crate::goals::GoalState> {
        self.snapshot.goal.as_ref()
    }
    /// Whether the active branch already carries the Anthropic subscription
    /// warning's shown marker (the once-per-session-lifecycle gate's
    /// hydrated read; [`crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE`]).
    #[must_use]
    pub fn anthropic_warning_shown(&self) -> bool {
        self.snapshot.anthropic_warning_shown
    }
    #[must_use]
    pub fn has_thinking_level(&self) -> bool {
        self.snapshot.thinking_present
    }
    #[must_use]
    pub fn has_service_tier(&self) -> bool {
        self.snapshot.tier_present
    }
    /// Fold a newly persisted linear entry without hydrating historical bodies.
    pub fn append_entry(&mut self, entry: FileEntry) {
        if !self.bodies.is_detached() {
            if let FileEntry::ChildUsageAttributed { payload, .. } = &entry {
                // A live append must fold into the retained assistant copy too, or
                // `context()` serves stale usage until reopen; a detached window
                // holds no bodies — the manager's append folds its copy.
                for retained in self.entries.iter_mut().rev() {
                    if retained.id() == Some(payload.target_id.as_str()) {
                        if let FileEntry::Message {
                            message: pa_types::session::AgentMessage::Assistant(assistant),
                            ..
                        } = retained
                        {
                            assistant.usage = payload.aggregate_usage.clone();
                        }
                        break;
                    }
                }
            }
        }
        update_snapshot(&mut self.snapshot, &entry);
        self.settings
            .thinking_level
            .clone_from(&self.snapshot.thinking);
        self.settings.service_tier = self.snapshot.tier;
        self.settings.model.clone_from(&self.snapshot.model);
        if let Some(id) = entry.id() {
            id.clone_into(&mut self.leaf_id);
        }
        if !self.bodies.is_detached() {
            self.entries.push(entry);
        }
    }

    /// Retained file-order entries, including the original header.
    #[must_use]
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Original retained JSONL records, excluding the header. Preserves wire
    /// fields and map order for consumers with a separate raw entry type.
    #[must_use]
    pub fn raw_entries(&self) -> &[String] {
        &self.raw_entries
    }

    /// Older non-message records, in file order, for persisted metadata
    /// consumers. These are not a complete ancestor chain.
    #[must_use]
    pub fn metadata_entries(&self) -> &[String] {
        &self.metadata_entries
    }

    #[must_use]
    pub fn older_path_stats(&self) -> &WindowStats {
        &self.older_path_stats
    }

    #[must_use]
    pub fn message_count(&self) -> usize {
        self.message_count
    }

    #[must_use]
    pub fn first_user_message(&self) -> Option<&serde_json::Value> {
        self.first_user_message.as_ref()
    }

    #[must_use]
    pub fn leaf_id(&self) -> &str {
        &self.leaf_id
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_full_history(&self) -> bool {
        self.bodies.is_full()
    }

    /// The model in effect at the retained-window boundary (the newest
    /// `model_change` in the discarded prefix): the per-model usage fold
    /// seeds its timeline with it; `None` without `model_change` rows or full history.
    #[must_use]
    pub fn boundary_model(&self) -> Option<&(String, String)> {
        self.boundary_model.as_ref()
    }

    /// Model context with settings resolved across the entire active ancestry.
    ///
    /// # Panics
    ///
    /// Panics when detached ([`Self::take_retained`]); `ensure_full_history` re-arms them.
    #[must_use]
    pub fn context(&self) -> SessionContext {
        assert!(
            !self.bodies.is_detached(),
            "a detached window's transcript context comes from the owning manager"
        );
        let mut context = build_session_context(&self.entries, Some(&self.leaf_id));
        if !self.bodies.is_full() {
            context
                .thinking_level
                .clone_from(&self.settings.thinking_level);
            context.service_tier = self.settings.service_tier;
            context.model.clone_from(&self.settings.model);
        }
        context
    }

    /// Hydrate off the async executor. This store is read-only: disk appends
    /// are included, the selected leaf remains pinned, and mutable stores
    /// must merge their own pending rows.
    ///
    /// # Errors
    ///
    /// Error when reading the session file fails or the blocking read task fails to join.
    pub async fn ensure_full_history(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.canonical_derived,
            "canonical-derived history requires the bound raw store and cursor"
        );
        if self.bodies.is_full() {
            return Ok(());
        }
        let path = self.path.clone();
        let entries = tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(path).map(|text| super::parse_session_entries(&text))
        })
        .await??;
        self.entries = entries;
        // Re-hydrated bodies re-arm the retained copies (and with them
        // `context()`), like a fresh walk: one transition, not two flags.
        self.bodies = BodyState::WholeFile;
        Ok(())
    }
}

/// Whether `entry` is the session's Anthropic subscription warning shown
/// marker (the once-per-lifecycle gate's persisted row,
/// [`crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE`] with
/// `data.shown == true`).
fn valid_warning_shown(entry: &FileEntry) -> bool {
    let FileEntry::Custom { payload, .. } = entry else {
        return false;
    };
    payload.custom_type == crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE
        && payload
            .data
            .as_ref()
            .and_then(|data| data.get("shown"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn valid_goal(entry: &FileEntry) -> Option<crate::goals::GoalState> {
    let FileEntry::Custom { payload, .. } = entry else {
        return None;
    };
    let data = payload.data.as_ref()?;
    if payload.custom_type != crate::goals::GOAL_STATE_CUSTOM_TYPE
        || !crate::goals::is_persisted_goal_state(data)
    {
        return None;
    }
    serde_json::from_value(data.clone())
        .ok()
        .map(crate::goals::normalize_goal_state)
}
fn apply_attributions(entries: &mut [FileEntry], metadata: &[String]) {
    for row in metadata {
        if let Ok(FileEntry::ChildUsageAttributed { payload, .. }) = serde_json::from_str(row) {
            for entry in entries.iter_mut() {
                if entry.id() == Some(payload.target_id.as_str()) {
                    if let FileEntry::Message {
                        message: pa_types::session::AgentMessage::Assistant(message),
                        ..
                    } = entry
                    {
                        message.usage = payload.aggregate_usage.clone();
                    }
                }
            }
        }
    }
}
pub(super) fn update_snapshot(snapshot: &mut Snapshot, entry: &FileEntry) {
    if !super::is_bootstrap_entry(entry) {
        snapshot.non_bootstrap = true;
    }
    if let Some(goal) = valid_goal(entry) {
        snapshot.goal = Some(goal);
    }
    // A live append of the warning marker folds into the retained window's
    // snapshot the same way, so the sidecar the next reload serves stays
    // current without a re-walk.
    if valid_warning_shown(entry) {
        snapshot.anthropic_warning_shown = true;
    }
    match entry {
        FileEntry::ThinkingLevelChange { payload, .. } => {
            snapshot.thinking.clone_from(&payload.thinking_level);
            snapshot.thinking_present = true;
        }
        FileEntry::ServiceTierChange { payload, .. } => {
            snapshot.tier = payload.service_tier;
            snapshot.tier_present = true;
        }
        FileEntry::ModelChange { payload, .. } => {
            snapshot.model = Some((payload.provider.clone(), payload.model_id.clone()));
        }
        FileEntry::Message { message, .. } => {
            snapshot.message_count += 1;
            if let pa_types::session::AgentMessage::Assistant(message) = message {
                snapshot.model = Some((message.provider.clone(), message.model.clone()));
            }
            if snapshot.first_user.is_none()
                && matches!(message, pa_types::session::AgentMessage::User(_))
            {
                snapshot.first_user = serde_json::to_value(message).ok();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
