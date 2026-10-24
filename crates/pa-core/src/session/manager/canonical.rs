//! Atomic, no-I/O adoption of exact owner-certified canonical sources.
//!
//! Raw graph coverage, local replay/view state, and owner authority are distinct.
//! This module never mints entry identity and never reads or writes a journal.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};

use anyhow::{ensure, Context};
use sha2::{Digest, Sha256};

use super::{AgentMessage, FileEntry, SessionManager};
use crate::session::canonical::{
    CanonicalAdmissionPolicy, CanonicalAdoptionCertificate, CanonicalAppendReceipt,
    CanonicalCheckpoint, CanonicalCursor, CanonicalJournalRow, CanonicalOperation,
    CanonicalPrepared, CanonicalSource,
};
use crate::session::window::WindowedSessionStore;

#[cfg(test)]
mod tests;

/// Coverage of immutable raw rows, never inferred from derived retained bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalRawCoverage {
    /// Every physical non-header row, including every branch and unknown row.
    Full,
    /// Only explicitly owner-certified retained rows. Older reads require the owner.
    OwnerVerifiedSparse,
}

/// Opaque local view/replay approval. Public callers can only preserve local state.
///
/// The private coordinator creates an approved plan ONLY after matching successful
/// owner preparation to its exact request and captured actual serving authority.
/// This approval is not proof of native/Agent publication.
pub struct CanonicalLocalAdoption {
    approved: Option<PreparedApproval>,
}

struct PreparedApproval {
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    policy: CanonicalAdmissionPolicy,
    serving: Weak<()>,
    local_revision: u64,
    previous_visible: Option<String>,
    visible: Option<String>,
}

impl CanonicalLocalAdoption {
    /// Preserve visible scope and the remaining admitted-user FIFO exactly.
    #[must_use]
    pub fn preserve() -> Self {
        Self { approved: None }
    }

    /// The Core coordinator must match the returned preparation to the exact
    /// approved request and actual live serving/run allocation before calling.
    /// Naked raw IDs or an arbitrary operation are not authorization.
    #[allow(dead_code)] // Coordinator wiring is a separate held change.
    pub(crate) fn from_owner_prepared(
        prepared: &CanonicalPrepared,
        approved_policy: CanonicalAdmissionPolicy,
        serving: &Arc<()>,
        local_revision: u64,
        previous_visible: Option<String>,
        visible: Option<String>,
    ) -> Self {
        Self {
            approved: Some(PreparedApproval {
                operation: prepared.operation.clone(),
                cursor: prepared.checkpoint.cursor.clone(),
                policy: approved_policy,
                serving: Arc::downgrade(serving),
                local_revision,
                previous_visible,
                visible,
            }),
        }
    }
}

pub(super) struct RawCanonicalState {
    header: CanonicalJournalRow,
    rows: Vec<CanonicalJournalRow>,
    cursor: CanonicalCursor,
    coverage: CanonicalRawCoverage,
    visible: Option<String>,
    seen_users: HashSet<String>,
    used_approvals: HashSet<CanonicalOperation>,
}

struct LocalCandidate {
    visible: Option<String>,
    queue: VecDeque<String>,
    seen: HashSet<String>,
    approvals: HashSet<CanonicalOperation>,
    fingerprint: [u8; 32],
}

impl SessionManager {
    /// Current SDK-local compare-and-swap revision; not owner authority.
    #[must_use]
    pub fn canonical_local_revision(&self) -> u64 {
        self.canonical_local_revision
    }

    /// Bound actual owner/source/branch cursor, separate from local/cache revisions.
    #[must_use]
    pub fn canonical_binding(&self) -> Option<&CanonicalCursor> {
        self.canonical_raw.as_ref().map(|raw| &raw.cursor)
    }

    /// Exact original physical header row.
    #[must_use]
    pub fn canonical_raw_header(&self) -> Option<&CanonicalJournalRow> {
        self.canonical_raw.as_ref().map(|raw| &raw.header)
    }

    /// Explicit coverage; sparse rows must never be presented as full history.
    #[must_use]
    pub fn canonical_raw_coverage(&self) -> Option<CanonicalRawCoverage> {
        self.canonical_raw.as_ref().map(|raw| raw.coverage)
    }

    /// All physical non-header rows, only when independently verified as full.
    #[must_use]
    pub fn canonical_raw_full_rows(&self) -> Option<&[CanonicalJournalRow]> {
        self.canonical_raw.as_ref().and_then(|raw| {
            (raw.coverage == CanonicalRawCoverage::Full).then_some(raw.rows.as_slice())
        })
    }

    /// Owner-certified retained raw rows only; this does NOT imply full coverage.
    #[must_use]
    pub fn canonical_raw_retained_rows(&self) -> Option<&[CanonicalJournalRow]> {
        self.canonical_raw.as_ref().map(|raw| raw.rows.as_slice())
    }

    /// Find a retained raw row. None may mean omitted sparse history, not absence.
    #[must_use]
    pub fn canonical_raw_entry(&self, id: &str) -> Option<&CanonicalJournalRow> {
        self.canonical_raw
            .as_ref()?
            .rows
            .iter()
            .find(|row| row.entry().id() == Some(id))
    }

    /// Visible/admitted leaf, independent of the raw owner's selected leaf.
    #[must_use]
    pub fn canonical_visible_leaf(&self) -> Option<&str> {
        self.canonical_raw
            .as_ref()
            .and_then(|raw| raw.visible.as_deref())
    }

    /// Pending exact admitted users, in remaining delivery order.
    #[must_use]
    pub fn canonical_pending_admitted_users(&self) -> Vec<String> {
        self.admitted_user_rows.iter().cloned().collect()
    }

    /// Canonical model view. Unlike the legacy builder, None means no visible row.
    /// Sparse settings are usable only for the same explicitly approved leaf.
    #[must_use]
    pub fn canonical_visible_context(&self) -> crate::session::SessionContext {
        let Some(raw) = &self.canonical_raw else {
            return self.active_context();
        };
        let Some(visible) = raw.visible.as_deref() else {
            return crate::session::build_session_context(&self.file_entries[..1], None);
        };
        let mut context = crate::session::build_session_context(&self.file_entries, Some(visible));
        if raw.cursor.selected_leaf.as_deref() == Some(visible) {
            if let Some(window) = &self.window {
                let settings = window.settings();
                context.thinking_level.clone_from(&settings.thinking_level);
                context.service_tier = settings.service_tier;
                context.model.clone_from(&settings.model);
            }
        }
        context
    }

    /// Invalidate previously captured local CAS inputs before parent-routed mutation.
    /// # Errors
    /// Refuses revision overflow without changing state.
    pub(crate) fn invalidate_canonical_local_revision(&mut self) -> anyhow::Result<()> {
        self.canonical_local_revision = self
            .canonical_local_revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        Ok(())
    }

    /// Install a whole exact checkpoint without journal I/O or identity generation.
    /// # Errors
    /// Any invalid source, stale binding/revision, or unapproved view/replay change
    /// leaves all manager fields unchanged and produces no adoption certificate.
    pub fn install_canonical_checkpoint(
        &mut self,
        operation: &CanonicalOperation,
        expected_local_revision: u64,
        expected_binding: Option<&CanonicalCursor>,
        checkpoint: CanonicalCheckpoint,
        local: CanonicalLocalAdoption,
    ) -> anyhow::Result<CanonicalAdoptionCertificate> {
        self.validate_cas(expected_local_revision, expected_binding)?;
        let CanonicalCheckpoint {
            cursor,
            header,
            source,
        } = checkpoint;
        validate_header(&header, &cursor)?;
        if let Some(old) = &self.canonical_raw {
            validate_refresh_cursor(&old.cursor, &cursor)?;
            ensure!(
                old.header.raw() == header.raw(),
                "canonical raw header changed"
            );
        } else {
            ensure!(
                self.file_entries.len() == 1
                    && self.window.is_none()
                    && self.admitted_user_rows.is_empty()
                    && self.leaf_id.is_none(),
                "first canonical bind requires pristine exact-header mirror"
            );
            ensure!(
                self.file_entries.first() == Some(header.entry()),
                "first canonical header differs"
            );
            ensure!(
                !self.persist && self.session_file.is_none(),
                "canonical mirror must not own journal I/O"
            );
        }
        let (rows, coverage, window) = match source {
            CanonicalSource::Full(rows) => (rows, CanonicalRawCoverage::Full, None),
            CanonicalSource::Window(window) => {
                ensure!(window.cursor == cursor, "sparse window cursor mismatch");
                validate_window(&window.derived, &header, &window.rows, &cursor)?;
                (
                    window.rows,
                    CanonicalRawCoverage::OwnerVerifiedSparse,
                    Some(window.derived),
                )
            }
        };
        validate_rows(&header, &rows, &cursor, coverage)?;
        if let Some(old) = &self.canonical_raw {
            validate_overlap(old, &rows, &cursor, coverage)?;
            if old.cursor.branch_revision == cursor.branch_revision {
                validate_descendant_selection(&old.cursor, &cursor, &rows)?;
            } else {
                ensure!(
                    matches!(
                        local.approved.as_ref().map(|a| &a.policy),
                        Some(CanonicalAdmissionPolicy::Replace(_))
                    ),
                    "branch navigation requires explicit owner-approved reset"
                );
            }
        }
        let candidate = self.validate_local(operation, &cursor, &rows, coverage, local)?;
        self.commit_candidate(operation, header, rows, cursor, coverage, window, candidate)
    }

    /// Apply the complete contiguous physical suffix, with an owner replacement
    /// when sparse derived state cannot be advanced safely. No I/O occurs.
    /// # Errors
    /// Refuses stale cursor/revision, corrupt rows/certificates, graph changes, or
    /// invalid replay/view authority atomically, without issuing a certificate.
    pub fn apply_canonical_suffix(
        &mut self,
        expected_local_revision: u64,
        receipt: CanonicalAppendReceipt,
        local: CanonicalLocalAdoption,
    ) -> anyhow::Result<CanonicalAdoptionCertificate> {
        self.validate_cas(expected_local_revision, Some(&receipt.from))?;
        let old = self
            .canonical_raw
            .as_ref()
            .context("canonical suffix has no binding")?;
        validate_refresh_cursor(&receipt.from, &receipt.to)?;
        ensure!(
            receipt.from.branch_revision == receipt.to.branch_revision,
            "canonical suffix crossed navigation epoch"
        );
        validate_suffix_rows(&receipt.rows, &receipt.from, &receipt.to)?;
        validate_descendant_selection(&receipt.from, &receipt.to, &receipt.rows)?;
        if let Some(id) = &receipt.committed_entry_id {
            ensure!(
                receipt
                    .rows
                    .iter()
                    .any(|row| row.entry().id() == Some(id.as_str())),
                "committed ID is not in exact suffix"
            );
        }
        let header = old.header.clone();
        let mut rows = old.rows.clone();
        rows.extend(receipt.rows.iter().cloned());
        let (rows, coverage, window) = if let Some(replacement) = receipt.replacement_window {
            ensure!(
                replacement.cursor == receipt.to,
                "replacement cursor mismatch"
            );
            validate_window(
                &replacement.derived,
                &header,
                &replacement.rows,
                &receipt.to,
            )?;
            for row in &receipt.rows {
                ensure!(
                    replacement
                        .rows
                        .iter()
                        .any(|r| r.physical_row() == row.physical_row()
                            && r.byte_offset() == row.byte_offset()
                            && r.raw() == row.raw()),
                    "replacement omits or changes suffix row"
                );
            }
            validate_overlap(
                old,
                &replacement.rows,
                &receipt.to,
                CanonicalRawCoverage::OwnerVerifiedSparse,
            )?;
            if old.coverage == CanonicalRawCoverage::Full {
                validate_rows(&header, &rows, &receipt.to, CanonicalRawCoverage::Full)?;
                (rows, CanonicalRawCoverage::Full, None)
            } else {
                (
                    replacement.rows,
                    CanonicalRawCoverage::OwnerVerifiedSparse,
                    Some(replacement.derived),
                )
            }
        } else {
            ensure!(
                old.coverage == CanonicalRawCoverage::Full,
                "sparse suffix requires fresh owner-certified replacement window"
            );
            (rows, CanonicalRawCoverage::Full, None)
        };
        validate_rows(&header, &rows, &receipt.to, coverage)?;
        let candidate =
            self.validate_local(&receipt.operation, &receipt.to, &rows, coverage, local)?;
        self.commit_candidate(
            &receipt.operation,
            header,
            rows,
            receipt.to,
            coverage,
            window,
            candidate,
        )
    }

    fn validate_cas(&self, revision: u64, binding: Option<&CanonicalCursor>) -> anyhow::Result<()> {
        ensure!(
            revision == self.canonical_local_revision,
            "canonical SDK local revision conflict"
        );
        ensure!(
            binding == self.canonical_binding(),
            "canonical owner cursor conflict"
        );
        ensure!(
            self.session_id == self.get_header().context("missing local header")?.id,
            "local session identity differs from header"
        );
        if let Some(raw) = &self.canonical_raw {
            ensure!(
                self.leaf_id == raw.visible,
                "local visible leaf changed without canonical approval"
            );
        }
        Ok(())
    }

    fn validate_local(
        &self,
        operation: &CanonicalOperation,
        cursor: &CanonicalCursor,
        rows: &[CanonicalJournalRow],
        coverage: CanonicalRawCoverage,
        local: CanonicalLocalAdoption,
    ) -> anyhow::Result<LocalCandidate> {
        let mut queue = self.admitted_user_rows.clone();
        let mut seen = self
            .canonical_raw
            .as_ref()
            .map_or_else(HashSet::new, |raw| raw.seen_users.clone());
        let mut approvals = self
            .canonical_raw
            .as_ref()
            .map_or_else(HashSet::new, |raw| raw.used_approvals.clone());
        let mut visible = self
            .canonical_raw
            .as_ref()
            .and_then(|raw| raw.visible.clone());
        let policy = if let Some(approval) = local.approved {
            ensure!(
                approval.operation == *operation && approval.cursor == *cursor,
                "local approval operation/cursor mismatch"
            );
            ensure!(
                approval.local_revision == self.canonical_local_revision
                    && approval.previous_visible == visible,
                "stale local view approval"
            );
            let _live_serving = approval
                .serving
                .upgrade()
                .context("approved serving/run scope is closed")?;
            ensure!(
                approvals.insert(operation.clone()),
                "local approval already consumed"
            );
            visible.clone_from(&approval.visible);
            approval.policy
        } else {
            CanonicalAdmissionPolicy::Preserve
        };
        match &policy {
            CanonicalAdmissionPolicy::Preserve => {
                ensure!(
                    visible == self.canonical_visible_leaf().map(str::to_owned),
                    "Preserve cannot advance visible scope"
                );
            }
            CanonicalAdmissionPolicy::Append(ids) => {
                for id in ids {
                    validate_user(rows, id)?;
                    ensure!(
                        seen.insert(id.clone()),
                        "duplicate or already replayed admission"
                    );
                    queue.push_back(id.clone());
                }
            }
            CanonicalAdmissionPolicy::Replace(ids) => {
                queue.clear();
                seen.clear();
                for id in ids {
                    validate_user(rows, id)?;
                    ensure!(seen.insert(id.clone()), "duplicate replacement admission");
                    queue.push_back(id.clone());
                }
            }
        }
        for id in &queue {
            validate_user(rows, id)?;
            ensure!(
                seen.contains(id),
                "pending user missing from admission ledger"
            );
        }
        if let Some(leaf) = &visible {
            ensure!(
                rows.iter()
                    .any(|row| row.entry().id() == Some(leaf.as_str())),
                "visible leaf is omitted from candidate source"
            );
            if coverage == CanonicalRawCoverage::OwnerVerifiedSparse {
                ensure!(
                    cursor.selected_leaf.as_ref() == Some(leaf),
                    "sparse view requires owner window for exact visible leaf"
                );
            }
        }
        let mut sorted_seen: Vec<_> = seen.iter().collect();
        sorted_seen.sort_unstable();
        let policy_value = match &policy {
            CanonicalAdmissionPolicy::Preserve => serde_json::json!(["preserve"]),
            CanonicalAdmissionPolicy::Append(ids) => serde_json::json!(["append", ids]),
            CanonicalAdmissionPolicy::Replace(ids) => serde_json::json!(["replace", ids]),
        };
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"policy":policy_value,
            "visible":visible,"queue":queue,"seen":sorted_seen}))?;
        Ok(LocalCandidate {
            visible,
            queue,
            seen,
            approvals,
            fingerprint: Sha256::digest(bytes).into(),
        })
    }

    #[allow(clippy::too_many_arguments)] // One staged atomic commit, no partially applied setters.
    fn commit_candidate(
        &mut self,
        operation: &CanonicalOperation,
        header: CanonicalJournalRow,
        rows: Vec<CanonicalJournalRow>,
        cursor: CanonicalCursor,
        coverage: CanonicalRawCoverage,
        mut window: Option<WindowedSessionStore>,
        local: LocalCandidate,
    ) -> anyhow::Result<CanonicalAdoptionCertificate> {
        let revision = self
            .canonical_local_revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        let mut entries = Vec::with_capacity(rows.len() + 1);
        entries.push(header.entry().clone());
        entries.extend(rows.iter().map(|row| row.entry().clone()));
        // Exact row interpretations stay immutable. Last physical aggregate wins ONLY here.
        crate::session::apply_child_usage_attributions(&mut entries);
        let mut by_id = HashMap::new();
        let mut labels = HashMap::new();
        let mut timestamps = HashMap::new();
        for (index, entry) in entries.iter().enumerate().skip(1) {
            if let Some(id) = entry.id() {
                by_id.insert(id.to_owned(), index);
            }
            if let FileEntry::Label { payload, .. } = entry {
                if let Some(label) = &payload.label {
                    labels.insert(payload.target_id.clone(), label.clone());
                    timestamps.insert(payload.target_id.clone(), entry.timestamp().to_owned());
                } else {
                    labels.remove(&payload.target_id);
                    timestamps.remove(&payload.target_id);
                }
            }
        }
        if let Some(window) = &mut window {
            let _ = window.take_retained();
        }
        let has_assistant = entries.iter().any(|entry| {
            matches!(
                entry,
                FileEntry::Message {
                    message: AgentMessage::Assistant(_),
                    ..
                }
            )
        });
        let certificate = CanonicalAdoptionCertificate {
            operation: operation.clone(),
            cursor: cursor.clone(),
            local_revision: revision,
            admission_fingerprint: local.fingerprint,
        };
        // All fallible checks and all candidate construction precede the first mutation.
        self.file_entries = entries;
        self.by_id = by_id;
        self.labels_by_id = labels;
        self.label_timestamps_by_id = timestamps;
        self.leaf_id.clone_from(&local.visible);
        self.admitted_user_rows = local.queue;
        self.has_assistant_entry = has_assistant;
        self.flushed = true;
        self.window = window;
        self.canonical_local_revision = revision;
        self.canonical_raw = Some(RawCanonicalState {
            header,
            rows,
            cursor,
            coverage,
            visible: local.visible,
            seen_users: local.seen,
            used_approvals: local.approvals,
        });
        Ok(certificate)
    }
}

fn validate_user(rows: &[CanonicalJournalRow], id: &str) -> anyhow::Result<()> {
    ensure!(
        rows.iter().any(|row| row.entry().id() == Some(id)
            && matches!(
                row.entry(),
                FileEntry::Message {
                    message: AgentMessage::User(_),
                    ..
                }
            )),
        "admitted ID is not an exact retained owner User row"
    );
    Ok(())
}

fn validate_header(header: &CanonicalJournalRow, cursor: &CanonicalCursor) -> anyhow::Result<()> {
    ensure!(
        header.byte_offset() == 0 && header.physical_row() == 0,
        "invalid header position"
    );
    let FileEntry::Header { header: parsed } = header.entry() else {
        anyhow::bail!("missing canonical header")
    };
    ensure!(
        parsed.id == cursor.session_id && !parsed.id.is_empty(),
        "header session mismatch"
    );
    ensure!(
        !cursor.source_path.as_os_str().is_empty(),
        "empty canonical source path"
    );
    ensure!(
        <[u8; 32]>::from(Sha256::digest(header.raw())) == cursor.header_certificate,
        "header SHA256 certificate mismatch"
    );
    ensure!(
        cursor.frontier.rows >= 1 && cursor.frontier.bytes >= u64::try_from(header.raw().len())?,
        "frontier precedes header"
    );
    Ok(())
}

fn validate_refresh_cursor(old: &CanonicalCursor, next: &CanonicalCursor) -> anyhow::Result<()> {
    ensure!(
        old.owner == next.owner
            && old.session_id == next.session_id
            && old.source_path == next.source_path
            && old.header_certificate == next.header_certificate,
        "canonical owner/source/header changed"
    );
    // F attests same dev/ino and the exact old generation before append. Its
    // source generation includes size/mtime/ctime, so it changes on growth.
    // NOIO validates the certified transition; it never stats the journal.
    if old.raw_source_generation != next.raw_source_generation {
        ensure!(
            next.frontier.bytes > old.frontier.bytes && next.frontier.rows > old.frontier.rows,
            "source generation changed without physical append growth"
        );
    }
    ensure!(
        next.canonical_revision >= old.canonical_revision
            && next.branch_revision >= old.branch_revision
            && next.frontier.bytes >= old.frontier.bytes
            && next.frontier.rows >= old.frontier.rows,
        "canonical cursor regressed"
    );
    if next.frontier != old.frontier {
        ensure!(
            next.canonical_revision > old.canonical_revision,
            "source advanced without canonical revision"
        );
    }
    if next.frontier.bytes == old.frontier.bytes || next.frontier.rows == old.frontier.rows {
        ensure!(
            next.frontier == old.frontier,
            "same-frontier certificate/position mismatch"
        );
    }
    Ok(())
}

/// Same-navigation selection can advance only through NEW physical descendants.
/// No historical branch selection or same-frontier switch is an append.
fn validate_descendant_selection(
    from: &CanonicalCursor,
    to: &CanonicalCursor,
    rows: &[CanonicalJournalRow],
) -> anyhow::Result<()> {
    if from.selected_leaf == to.selected_leaf {
        return Ok(());
    }
    ensure!(
        to.frontier.rows > from.frontier.rows && to.frontier.bytes > from.frontier.bytes,
        "raw leaf switched without physical append"
    );
    let mut selected = to
        .selected_leaf
        .as_deref()
        .context("append cannot clear raw selected leaf")?;
    let mut visited = HashSet::new();
    loop {
        ensure!(
            visited.insert(selected),
            "cycle in selected suffix ancestry"
        );
        let row = rows
            .iter()
            .find(|row| {
                row.physical_row() >= from.frontier.rows && row.entry().id() == Some(selected)
            })
            .context("selected suffix leaves newly appended ancestry")?;
        match row.entry().parent_id() {
            parent if parent == from.selected_leaf.as_deref() => return Ok(()),
            Some(parent) => selected = parent,
            None => anyhow::bail!("selected suffix does not descend from prior raw leaf"),
        }
    }
}

fn validate_rows(
    header: &CanonicalJournalRow,
    rows: &[CanonicalJournalRow],
    cursor: &CanonicalCursor,
    coverage: CanonicalRawCoverage,
) -> anyhow::Result<()> {
    let full = coverage == CanonicalRawCoverage::Full;
    let mut offset = u64::try_from(header.raw().len())?;
    let mut ordinal = 1_u64;
    let mut ids = HashMap::new();
    let mut hash = Sha256::new();
    hash.update(header.raw());
    for row in rows {
        ensure!(
            !matches!(row.entry(), FileEntry::Header { .. }),
            "duplicate canonical header"
        );
        let end = row
            .byte_offset()
            .checked_add(u64::try_from(row.raw().len())?)
            .context("row byte offset overflow")?;
        let next_row = row
            .physical_row()
            .checked_add(1)
            .context("row ordinal overflow")?;
        ensure!(
            row.byte_offset() >= offset
                && row.physical_row() >= ordinal
                && end <= cursor.frontier.bytes
                && next_row <= cursor.frontier.rows,
            "row position outside physical frontier"
        );
        if full {
            ensure!(
                row.byte_offset() == offset && row.physical_row() == ordinal,
                "noncontiguous full source"
            );
        }
        if let Some(parent) = row.entry().parent_id() {
            ensure!(!parent.is_empty(), "empty parent ID");
            if full {
                ensure!(ids.contains_key(parent), "missing/forward canonical parent");
            }
        }
        if let Some(id) = row.entry().id() {
            ensure!(
                !id.is_empty() && ids.insert(id.to_owned(), row.physical_row()).is_none(),
                "duplicate/empty canonical row ID"
            );
            ensure!(
                row.entry().parent_id() != Some(id),
                "self-parent canonical row"
            );
        } else {
            ensure!(
                matches!(row.entry(), FileEntry::Unknown { .. }),
                "known row lacks ID"
            );
        }
        hash.update(row.raw());
        offset = end;
        ordinal = next_row;
    }
    for row in rows {
        if let Some(parent_ordinal) = row.entry().parent_id().and_then(|p| ids.get(p)) {
            ensure!(
                *parent_ordinal < row.physical_row(),
                "canonical parent is not physically earlier"
            );
        }
    }
    if full {
        ensure!(
            offset == cursor.frontier.bytes && ordinal == cursor.frontier.rows,
            "full source frontier mismatch"
        );
        ensure!(
            <[u8; 32]>::from(hash.finalize()) == cursor.frontier.prefix_certificate,
            "prefix SHA256 certificate mismatch"
        );
        if let Some(leaf) = &cursor.selected_leaf {
            ensure!(ids.contains_key(leaf), "raw selected leaf missing");
        }
    } else if let Some(leaf) = &cursor.selected_leaf {
        ensure!(ids.contains_key(leaf), "sparse selected leaf omitted");
    }
    Ok(())
}

fn validate_suffix_rows(
    rows: &[CanonicalJournalRow],
    from: &CanonicalCursor,
    to: &CanonicalCursor,
) -> anyhow::Result<()> {
    let mut bytes = from.frontier.bytes;
    let mut ordinal = from.frontier.rows;
    for row in rows {
        ensure!(
            row.byte_offset() == bytes && row.physical_row() == ordinal,
            "suffix offset/ordinal mismatch"
        );
        bytes = bytes
            .checked_add(u64::try_from(row.raw().len())?)
            .context("suffix byte overflow")?;
        ordinal = ordinal.checked_add(1).context("suffix ordinal overflow")?;
    }
    ensure!(
        bytes == to.frontier.bytes && ordinal == to.frontier.rows,
        "suffix is not complete physical frontier"
    );
    Ok(())
}

fn validate_overlap(
    old: &RawCanonicalState,
    rows: &[CanonicalJournalRow],
    cursor: &CanonicalCursor,
    coverage: CanonicalRawCoverage,
) -> anyhow::Result<()> {
    let by_ordinal: HashMap<_, _> = rows.iter().map(|row| (row.physical_row(), row)).collect();
    for row in &old.rows {
        if let Some(next) = by_ordinal.get(&row.physical_row()) {
            ensure!(
                row.raw() == next.raw() && row.byte_offset() == next.byte_offset(),
                "immutable canonical row changed"
            );
        } else if coverage == CanonicalRawCoverage::Full {
            anyhow::bail!("full checkpoint omitted previously known row");
        }
    }
    if coverage == CanonicalRawCoverage::Full {
        let mut hash = Sha256::new();
        hash.update(old.header.raw());
        for row in rows
            .iter()
            .filter(|row| row.physical_row() < old.cursor.frontier.rows)
        {
            hash.update(row.raw());
        }
        ensure!(
            <[u8; 32]>::from(hash.finalize()) == old.cursor.frontier.prefix_certificate,
            "checkpoint does not extend prior certified prefix"
        );
    }
    ensure!(
        cursor.frontier.bytes >= old.cursor.frontier.bytes,
        "checkpoint truncates prefix"
    );
    Ok(())
}

fn validate_window(
    window: &WindowedSessionStore,
    header: &CanonicalJournalRow,
    rows: &[CanonicalJournalRow],
    cursor: &CanonicalCursor,
) -> anyhow::Result<()> {
    ensure!(
        window.source_path() == cursor.source_path
            && Some(window.leaf_id()) == cursor.selected_leaf.as_deref(),
        "derived sparse window source/leaf mismatch"
    );
    ensure!(
        !window.retained_detached() && window.entries().first() == Some(header.entry()),
        "sparse window missing exact interpreted header"
    );
    for entry in window.entries().iter().skip(1) {
        ensure!(
            rows.iter().any(|row| row.entry().id() == entry.id()),
            "derived sparse body has no raw source row"
        );
    }
    // Window raw strings are compared directly, never reconstructed from derived entries.
    for raw in window.raw_entries().iter().chain(window.metadata_entries()) {
        ensure!(
            rows.iter()
                .any(|row| row.raw().strip_suffix(b"\n") == Some(raw.as_bytes())),
            "window original row is absent from certified raw retained source"
        );
    }
    Ok(())
}

impl SessionManager {
    /// Install a live owner checkpoint as one no-I/O candidate. Bootstrap replaces
    /// a derived legacy mirror, never the canonical file or its raw owner rows.
    #[allow(clippy::too_many_arguments)] // One complete CAS candidate, never partially applied setters.
    pub(crate) fn install_canonical_live_checkpoint(
        &mut self,
        operation: &CanonicalOperation,
        revision: u64,
        binding: Option<&CanonicalCursor>,
        checkpoint: CanonicalCheckpoint,
        admission: CanonicalAdmissionPolicy,
        serving: &Arc<()>,
    ) -> anyhow::Result<CanonicalAdoptionCertificate> {
        self.validate_cas(revision, binding)?;
        let previous_visible = self.canonical_visible_leaf().map(str::to_owned);
        if self.canonical_raw.is_some() {
            let prepared_local = CanonicalLocalAdoption {
                approved: Some(PreparedApproval {
                    operation: operation.clone(),
                    cursor: checkpoint.cursor.clone(),
                    policy: admission,
                    serving: Arc::downgrade(serving),
                    local_revision: revision,
                    previous_visible: previous_visible.clone(),
                    visible: previous_visible,
                }),
            };
            return self.install_canonical_checkpoint(
                operation,
                revision,
                binding,
                checkpoint,
                prepared_local,
            );
        }
        ensure!(
            !self.persist && self.session_file.is_none(),
            "live mirror must not own journal I/O"
        );
        let FileEntry::Header { header } = checkpoint.header.entry() else {
            anyhow::bail!("live owner has no header")
        };
        ensure!(
            header.id == self.session_id,
            "live owner session differs from mirror"
        );
        let mut candidate =
            Self::in_memory_from_header(&self.cwd, &self.session_dir, header.clone())
                .map_err(anyhow::Error::msg)?;
        candidate.canonical_local_revision = revision;
        ensure!(
            !matches!(admission, CanonicalAdmissionPolicy::Append(_)),
            "first bind requires explicit owner replay reset"
        );
        // No raw leaf is visible until the matching owner ACK supplies its exact
        // safe prefix, accounting for both User and Custom delivery ledgers.
        let visible = None;
        let local = CanonicalLocalAdoption {
            approved: Some(PreparedApproval {
                operation: operation.clone(),
                cursor: checkpoint.cursor.clone(),
                policy: admission,
                serving: Arc::downgrade(serving),
                local_revision: revision,
                previous_visible: None,
                visible,
            }),
        };
        let certificate =
            candidate.install_canonical_checkpoint(operation, revision, None, checkpoint, local)?;
        // Move only canonical/view fields; runtime directories and observers stay owned here.
        self.file_entries = candidate.file_entries;
        self.by_id = candidate.by_id;
        self.labels_by_id = candidate.labels_by_id;
        self.label_timestamps_by_id = candidate.label_timestamps_by_id;
        self.leaf_id = candidate.leaf_id;
        self.admitted_user_rows = candidate.admitted_user_rows;
        self.has_assistant_entry = candidate.has_assistant_entry;
        self.flushed = candidate.flushed;
        self.window = candidate.window;
        self.canonical_raw = candidate.canonical_raw;
        self.canonical_local_revision = candidate.canonical_local_revision;
        Ok(certificate)
    }

    /// Sealed live publication after the matching owner ACK. Raw adoption alone
    /// cannot call this from outside Core. Any local change invalidates its CAS.
    pub(crate) fn publish_canonical_live_receipt(
        &mut self,
        publication: CanonicalLivePublication,
    ) -> anyhow::Result<()> {
        let certificate = publication.certificate();
        self.validate_cas(certificate.local_revision(), Some(certificate.cursor()))?;
        let raw = self
            .canonical_raw
            .as_ref()
            .context("live publication has no raw binding")?;
        let visible = publication.visible_leaf;
        validate_live_visible(raw, visible.as_deref(), &self.admitted_user_rows)?;
        let revision = self
            .canonical_local_revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        self.leaf_id.clone_from(&visible);
        self.canonical_raw
            .as_mut()
            .expect("validated binding")
            .visible = visible;
        self.canonical_local_revision = revision;
        Ok(())
    }

    /// Consume only the exact owner-delivered FIFO head after Prepared ACK.
    pub(crate) fn consume_canonical_live_user(
        &mut self,
        operation: &CanonicalOperation,
        cursor: &CanonicalCursor,
        local_revision: u64,
        id: &str,
        visible_leaf: Option<String>,
    ) -> anyhow::Result<()> {
        self.validate_cas(local_revision, Some(cursor))?;
        ensure!(
            self.canonical_binding() == Some(cursor),
            "delivered User binding changed"
        );
        ensure!(
            self.admitted_user_rows.front().map(String::as_str) == Some(id),
            "delivered User is not FIFO head"
        );
        let raw = self
            .canonical_raw
            .as_ref()
            .context("delivered User has no raw source")?;
        ensure!(
            raw.used_approvals.contains(operation),
            "delivered User operation was not approved"
        );
        ensure!(
            raw.rows.iter().any(|row| row.entry().id() == Some(id)),
            "delivered User raw row absent"
        );
        let mut pending = self.admitted_user_rows.clone();
        pending.pop_front();
        validate_live_visible(raw, visible_leaf.as_deref(), &pending)?;
        let revision = self
            .canonical_local_revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        self.admitted_user_rows = pending;
        self.leaf_id.clone_from(&visible_leaf);
        self.canonical_raw
            .as_mut()
            .expect("validated binding")
            .visible = visible_leaf;
        self.canonical_local_revision = revision;
        Ok(())
    }
}

/// Sealed approval created only after this driver's matching successful owner ACK.
/// A raw-adoption certificate alone cannot advance the visible scope.
pub(crate) struct CanonicalLivePublication {
    certificate: CanonicalAdoptionCertificate,
    _serving: Arc<()>,
    visible_leaf: Option<String>,
}
impl CanonicalLivePublication {
    /// Only the live Core driver may issue this after matching owner ACK success.
    pub(crate) fn after_owner_ack(
        certificate: CanonicalAdoptionCertificate,
        serving: &Arc<()>,
        visible_leaf: Option<String>,
    ) -> Self {
        Self {
            certificate,
            _serving: Arc::clone(serving),
            visible_leaf,
        }
    }
    pub(crate) fn certificate(&self) -> &CanonicalAdoptionCertificate {
        &self.certificate
    }
}

impl SessionManager {
    pub(crate) fn bind_canonical_metadata_writer(
        &mut self,
        writer: std::sync::Weak<dyn crate::session::canonical::CanonicalMetadataWriter>,
    ) {
        self.canonical_metadata_writer = Some(writer);
    }
    pub(crate) fn canonical_metadata_writer(
        &self,
    ) -> Option<Arc<dyn crate::session::canonical::CanonicalMetadataWriter>> {
        self.canonical_metadata_writer
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }
    pub(crate) fn append_metadata_intent(
        &mut self,
        mut entry: FileEntry,
    ) -> std::io::Result<String> {
        self.require_local_append()?;
        let (FileEntry::ThinkingLevelChange { base, .. }
        | FileEntry::ServiceTierChange { base, .. }
        | FileEntry::ModelChange { base, .. }
        | FileEntry::Custom { base, .. }
        | FileEntry::ChildUsageAttributed { base, .. }
        | FileEntry::Label { base, .. }
        | FileEntry::SessionInfo { base, .. }
        | FileEntry::SessionState { base, .. }
        | FileEntry::GitState { base, .. }
        | FileEntry::CustomMessage { base, .. }
        | FileEntry::Message {
            base,
            message: AgentMessage::BashExecution(_),
        }) = &mut entry
        else {
            return Err(std::io::Error::other(
                "metadata intent is not a metadata action",
            ));
        };
        if base.id.is_some() || base.parent_id.is_some() || base.timestamp.is_some() {
            return Err(std::io::Error::other(
                "metadata intent supplied canonical identity",
            ));
        }
        *base = self.next_base();
        let id = base.id.clone().expect("local base minted identity");
        self.append_entry(entry)?;
        Ok(id)
    }
}

impl SessionManager {
    /// Exact custom delivery after the owner has certified and acknowledged its checkpoint.
    pub(crate) fn consume_canonical_live_custom(
        &mut self,
        operation: &CanonicalOperation,
        cursor: &CanonicalCursor,
        local_revision: u64,
        id: &str,
        visible_leaf: Option<String>,
    ) -> anyhow::Result<()> {
        self.validate_cas(local_revision, Some(cursor))?;
        ensure!(
            self.canonical_binding() == Some(cursor),
            "custom delivery binding changed"
        );
        let raw = self
            .canonical_raw
            .as_ref()
            .context("custom delivery has no source")?;
        ensure!(
            raw.used_approvals.contains(operation),
            "custom delivery operation not approved"
        );
        ensure!(
            raw.rows.iter().any(|row| row.entry().id() == Some(id)
                && matches!(row.entry(), FileEntry::CustomMessage { .. })),
            "custom delivery raw row absent"
        );
        validate_live_visible(raw, visible_leaf.as_deref(), &self.admitted_user_rows)?;
        let revision = self
            .canonical_local_revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        self.leaf_id.clone_from(&visible_leaf);
        self.canonical_raw
            .as_mut()
            .expect("validated binding")
            .visible = visible_leaf;
        self.canonical_local_revision = revision;
        Ok(())
    }
}

impl SessionManager {
    pub(crate) fn check_canonical_live_revision(
        &self,
        revision: u64,
        cursor: &CanonicalCursor,
    ) -> anyhow::Result<()> {
        self.validate_cas(revision, Some(cursor))
    }
}

fn validate_live_visible(
    raw: &RawCanonicalState,
    visible: Option<&str>,
    pending_users: &VecDeque<String>,
) -> anyhow::Result<()> {
    let Some(visible) = visible else {
        return Ok(());
    };
    let mut current = raw.cursor.selected_leaf.as_deref();
    let mut ancestors = HashSet::new();
    while let Some(id) = current {
        ensure!(
            ancestors.insert(id.to_owned()),
            "cycle in owner publication ancestry"
        );
        current = raw
            .rows
            .iter()
            .find(|row| row.entry().id() == Some(id))
            .context("owner publication ancestry is not retained")?
            .entry()
            .parent_id();
    }
    ensure!(
        ancestors.contains(visible),
        "owner publication leaf is not on selected ancestry"
    );
    let mut current = Some(visible);
    while let Some(id) = current {
        ensure!(
            !pending_users.iter().any(|pending| pending == id),
            "owner publication includes undelivered User"
        );
        current = raw
            .rows
            .iter()
            .find(|row| row.entry().id() == Some(id))
            .context("owner publication prefix absent")?
            .entry()
            .parent_id();
    }
    Ok(())
}

impl SessionManager {
    /// Called only by the live driver after checking its exact owner ACK proof.
    pub(crate) fn publish_canonical_live_prepared(
        &mut self,
        operation: &CanonicalOperation,
        cursor: &CanonicalCursor,
        revision: u64,
        visible_leaf: Option<String>,
    ) -> anyhow::Result<()> {
        self.validate_cas(revision, Some(cursor))?;
        let raw = self
            .canonical_raw
            .as_ref()
            .context("prepared publication has no raw binding")?;
        ensure!(
            raw.used_approvals.contains(operation),
            "prepared publication operation not approved"
        );
        validate_live_visible(raw, visible_leaf.as_deref(), &self.admitted_user_rows)?;
        let revision = revision
            .checked_add(1)
            .context("canonical local revision overflow")?;
        self.leaf_id.clone_from(&visible_leaf);
        self.canonical_raw
            .as_mut()
            .expect("validated binding")
            .visible = visible_leaf;
        self.canonical_local_revision = revision;
        Ok(())
    }
}
