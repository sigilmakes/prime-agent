//! `CanonicalSessionStore` over the real session file.
//!
//! This is the adapter the canonical coordinator drives: `prepare` certifies what
//! is already durable, `commit` writes the one row the operation requested, and
//! `acknowledge` binds the receipt the caller adopted.
//!
//! Three rules this module obeys because the committed texts require them:
//!
//! * `prepare` writes NOTHING. `CanonicalAdmissionPolicy::Append(ids)` admits IDs
//!   of rows that are already durable ("This never writes User rows"), and the
//!   coordinator's own `prepare` performs no I/O. The turn's user row reached the
//!   file earlier through the ordinary row path.
//! * The adapter acquires no replacement, pause, build or event-delivery gate. It
//!   holds the session lease and the file handle only.
//! * A checkpoint is taken with [`RawCapture::checkpoint`], never hand-rolled, and
//!   only after the preparation's own writes (there are none today) have settled.
//!
//! Recorded for the commit body that is still to be written, because both were
//! real defects in the first draft: the row must be written INSIDE the lease fence
//! (`with_owned_mutation` plus the appended-bytes path, keeping the imported
//! durability proof), or a concurrent `release()` can drain and close the lease
//! while the write is mid-flight - an epoch flight stops the EPOCH draining and
//! does nothing for the lease. And the grant must be settled BEFORE a receipt can
//! be observed, or a caller can read a committed row whose bytes are unclassified.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_core::session::canonical::{
    CanonicalAcknowledgement, CanonicalAdmissionPolicy, CanonicalCallerGuard,
    CanonicalCallerNotifier, CanonicalCallerSignal, CanonicalCommitDisposition,
    CanonicalCommitRequest, CanonicalEntryDraft, CanonicalFailureKind, CanonicalFuture,
    CanonicalIoStage, CanonicalMutationKind, CanonicalOperation, CanonicalPrepareRequest,
    CanonicalPrepared, CanonicalRequestedWrite, CanonicalSessionStore, CanonicalSourceDisposition,
    CanonicalStoreError,
};
use pa_core::session_engine::canonical_coordinator::{
    CanonicalAppendGrant, CanonicalEpoch, CanonicalProviderStep,
};

use pa_types::session::FileEntry;

use super::canonical_raw::{RawCapture, RawPosition};
use super::canonical_revision::CanonicalRevision;

/// Notification-only caller hook: it must not lock or panic.
#[expect(
    dead_code,
    reason = "constructed by prepare once the call sites drive this backend"
)]
struct CallerNotify(tokio::sync::Notify);

impl CanonicalCallerNotifier for CallerNotify {
    fn changed(&self) {
        self.0.notify_waiters();
    }
}

#[expect(
    dead_code,
    reason = "closes when the row-write call sites construct the backend and consume these refusals"
)]
fn refused(detail: &str) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::Unchanged,
        detail: detail.to_owned(),
    }
}

/// The typed refusal for a read served under a poisoned epoch.
///
/// A poisoned epoch means an uncertain write: the raw tail may be partially
/// present, so nothing may be read as if its bytes were classified. `NeedsReconcile`
/// is the honest disposition - the source is neither `Unchanged` nor
/// `PendingAdoption`, and the caller must reconcile before serving again.
pub(crate) fn refused_poisoned_read() -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::NeedsReconcile,
        detail: "canonical epoch is poisoned by an uncertain write".to_owned(),
    }
}

/// Whether a live epoch is poisoned, as a read path may observe it.
pub(crate) fn epoch_is_poisoned(epoch: Option<&EpochHandle>) -> bool {
    epoch.is_some_and(|handle| handle.0.poisoned())
}

/// One provider request attempt's identity, created where the attempt starts.
///
/// The authority treats a step as opaque pointer identity and refuses the same
/// allocation twice, so this object IS the daemon's actual step record for the
/// attempt: one per attempt, never reused across a retry or a failover.
pub struct ProviderStepIdentity;

/// The worker's shared channel for the live provider step.
///
/// Division of ownership, which is the whole reason this type exists:
///
/// * the WORKER owns it and creates the slot with its session core;
/// * the per-turn engine method (`run_turn_once`) is the only writer, and it
///   admits one step per provider attempt, before the prompt is issued;
/// * `worker/turn.rs` is the only reader, at the assistant row it persists.
///
/// The subscriber closure inside `run_turn_once` captures NO slot: a clone there
/// would be a second owner, and a slot that is cloned per task and never read is a
/// silent uncovered path. A step admitted and never written leaves an unconsumed
/// seal and no flight, so it is harmless rather than a leak.
#[derive(Clone)]
pub struct CanonicalStepAdmission {
    epoch: Option<Arc<CanonicalEpoch>>,
    slot: Arc<std::sync::Mutex<Option<CanonicalProviderStep>>>,
}

impl CanonicalStepAdmission {
    pub(crate) fn new(
        epoch: Option<Arc<CanonicalEpoch>>,
        slot: Arc<std::sync::Mutex<Option<CanonicalProviderStep>>>,
    ) -> Self {
        Self { epoch, slot }
    }

    /// Admit ONE actual provider step for this attempt.
    ///
    /// The engine must call this before the prompt is issued; a missing epoch is
    /// not an error (the turn runs uncovered and the counter moves), but an
    /// admission refusal under a live epoch IS one.
    pub(crate) fn admit(&self, step: Arc<ProviderStepIdentity>) -> anyhow::Result<()> {
        let Some(epoch) = self.epoch.as_ref() else {
            return Ok(());
        };
        let admitted = epoch.admit_provider_step(step)?;
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One attempt, one step: a leftover handle would let a later row borrow an
        // earlier step, so the newer admission replaces it and the old seal stays
        // unconsumed.
        *slot = Some(admitted);
        Ok(())
    }
}

/// Debug-able handle: `CanonicalEpoch` implements no `Debug`, and `SessionFile`
/// derives it, so the live epoch travels wrapped.
#[derive(Clone)]
pub(crate) struct EpochHandle(pub(crate) Arc<CanonicalEpoch>);

impl std::fmt::Debug for EpochHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EpochHandle(<live canonical epoch>)")
    }
}

/// One adapter per session file, shared with the coordinator.
#[expect(
    dead_code,
    reason = "constructed by the row-write call sites in the next step"
)]
pub(crate) struct SessionStoreBackend {
    path: PathBuf,
    /// The session lease this store appends under. `commit` writes inside the
    /// lease fence (`with_owned_mutation`), never against a bare path.
    lease: Option<Arc<crate::lease::SessionLease>>,
    revision: CanonicalRevision,
    epoch: Option<Arc<CanonicalEpoch>>,
    signal: Mutex<Option<CanonicalCallerSignal>>,
    /// IDs already admitted through this binding. `B` replaces this with the
    /// durable per-binding ledger; until then the set is in-memory and dies with
    /// the adapter, which is why it is named here rather than assumed.
    admitted: Mutex<HashSet<String>>,
}

#[expect(
    dead_code,
    reason = "the row-write call sites construct and use this impl in the next step"
)]
impl SessionStoreBackend {
    pub(crate) fn new(
        path: PathBuf,
        lease: Option<Arc<crate::lease::SessionLease>>,
        epoch: Option<Arc<CanonicalEpoch>>,
    ) -> Self {
        Self {
            path,
            lease,
            revision: CanonicalRevision::new(),
            epoch,
            signal: Mutex::new(None),
            admitted: Mutex::new(HashSet::new()),
        }
    }

    fn position(&self) -> Result<RawPosition, CanonicalStoreError> {
        let snapshot = self
            .revision
            .snapshot()
            .map_err(|_| refused("canonical revision is unavailable"))?;
        Ok(RawPosition {
            owner: snapshot.owner,
            canonical_revision: snapshot.canonical_revision,
            branch_revision: snapshot.branch_revision,
            selected_leaf: None,
        })
    }

    /// Bind `admission` to rows that are already durable in `checkpoint`.
    ///
    /// The policy is a ledger instruction, not a write: every admitted ID must
    /// already exist in the captured source, and a repeat is refused.
    fn bind_admission(
        &self,
        admission: &CanonicalAdmissionPolicy,
        checkpoint: &pa_core::session::canonical::CanonicalCheckpoint,
    ) -> Result<(), CanonicalStoreError> {
        let ids = match admission {
            CanonicalAdmissionPolicy::Append(ids) | CanonicalAdmissionPolicy::Replace(ids) => ids,
            CanonicalAdmissionPolicy::Preserve => return Ok(()),
        };
        let pa_core::session::canonical::CanonicalSource::Full(rows) = &checkpoint.source else {
            return Err(refused("canonical source is not a full capture"));
        };
        let mut present = HashSet::new();
        for row in rows {
            if let Some(id) = row.entry().id() {
                present.insert(id.to_owned());
            }
        }
        if let Some(row) = &checkpoint.header.entry().id() {
            present.insert((*row).to_owned());
        }
        let mut admitted = self
            .admitted
            .lock()
            .map_err(|_| refused("admission ledger is poisoned"))?;
        for id in ids {
            if !present.contains(id) {
                return Err(refused("admitted id is not durable in the source"));
            }
            if !admitted.insert(id.clone()) {
                return Err(refused("admitted id was already admitted"));
            }
        }
        Ok(())
    }
}

/// Choose the canonical admission for one row this store is about to write.
///
/// Three cases, and each one is named rather than assumed:
///
/// * A **user** row takes a prepare-phase flight. [`CanonicalEntryDraft::new`]
///   refuses a user row ("user rows require exact owner admission/replay") and no
///   final admission exists without a draft, so a prepare flight is the only
///   admission it can take. That flight binds no draft: the epoch vouches for the
///   WRITE's completion, not its payload, which is acceptable because the session
///   owner admitted that content when it accepted the turn.
/// * A **context-policy** row (compaction) takes a requested-mutation grant with a
///   draft derived from the row. `CanonicalEpoch::begin_append` admits through the
///   context-policy scope, so this is the one draft kind it accepts.
/// * Everything else is **declared outside and counted**, with the reason stated:
///   an assistant or tool row can only be admitted through `ProviderStep::
///   seal_assistant` - kind `AdmittedTerminal`, sealed against an admitted provider
///   step, once per step - and metadata only through one of the seven exact
///   lifecycle actions. The daemon's generic row loop cannot take a grant for those
///   without faking one, so it does not, and the counter states the fact instead.
pub(super) fn grant_for_row(
    epoch: &CanonicalEpoch,
    entry: &super::SessionEntry,
    step: Option<&CanonicalProviderStep>,
) -> anyhow::Result<Option<CanonicalAppendGrant>> {
    if is_user_row(entry) {
        return epoch
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .map(Some)
            .map_err(anyhow::Error::from);
    }
    // The native final output: an assistant row is admitted by the provider step
    // that produced it, never by a draft-only generic grant. A missing step under a
    // live epoch means the engine did not admit where we thought it did, so the row
    // is REFUSED - writing it uncovered would make the drain claim a lie.
    if is_assistant_row(entry) {
        let Some(step) = step else {
            anyhow::bail!(
                "engine admitted no provider step for this assistant row: refusing to write it without an epoch grant"
            );
        };
        let draft = draft_for_row(entry)?;
        return epoch
            .begin_terminal_append(step, &draft, CanonicalIoStage::RequestedMutation)
            .map(Some)
            .map_err(anyhow::Error::from);
    }
    if matches!(entry.type_.as_str(), "compaction" | "branch_summary") {
        let draft = draft_for_row(entry)?;
        return epoch
            .begin_append(CanonicalIoStage::RequestedMutation, &draft)
            .map(Some)
            .map_err(anyhow::Error::from);
    }
    // LAST arm on purpose: a row type nobody has thought about is COUNTED rather
    // than silently admitted, which is what makes the counter a boundary instead of
    // a list of known rows.
    Ok(None)
}

/// Whether this row is an assistant message: the native final output, covered only
/// by an admitted provider step's seal.
pub(super) fn is_assistant_row(entry: &super::SessionEntry) -> bool {
    entry.type_ == "message"
        && entry.fields.get("message").and_then(|m| m.get("role"))
            == Some(&serde_json::Value::String("assistant".to_owned()))
}

/// Whether this row is the turn's user prompt, which no draft may carry.
pub(super) fn is_user_row(entry: &super::SessionEntry) -> bool {
    entry.type_ == "message"
        && entry.fields.get("message").and_then(|m| m.get("role"))
            == Some(&serde_json::Value::String("user".to_owned()))
}

/// Build the draft for a row that is a requested mutation.
///
/// The envelope identity is stripped before the draft is built: the draft rejects
/// a supplied id, parent or timestamp, and the store mints those at write time.
pub(super) fn draft_for_row(entry: &super::SessionEntry) -> anyhow::Result<CanonicalEntryDraft> {
    let mut value = serde_json::to_value(entry)?;
    if let Some(object) = value.as_object_mut() {
        for key in ["id", "parentId", "timestamp"] {
            object.remove(key);
        }
    }
    let file_entry: FileEntry = serde_json::from_value(value)?;
    // Verified at HEAD, not taken on report: `Purpose::ContextPolicy::validate_draft`
    // accepts `Compaction | BranchSummary`, and `MetadataAction::matches` has no
    // branch-summary arm, so a branch summary is context policy and nothing else.
    let kind = match entry.type_.as_str() {
        // LIVE as of the terminal wiring: an assistant row reaches this arm through
        // `begin_terminal_append`, which seals it to the admitted provider step. A
        // user row never gets here (it takes a prepare flight, because no draft may
        // carry a user row), and a tool result stays counted until a tool seal path
        // exists. Test: `an_assistant_row_takes_the_provider_step_grant`.
        "message" => CanonicalMutationKind::AdmittedTerminal,
        "compaction" | "branch_summary" => CanonicalMutationKind::ContextPolicy,
        _ => CanonicalMutationKind::OwnerMetadata,
    };
    CanonicalEntryDraft::new(kind, file_entry)
}

impl CanonicalSessionStore for SessionStoreBackend {
    fn prepare(&self, request: CanonicalPrepareRequest) -> CanonicalFuture<CanonicalPrepared> {
        let position = match self.position() {
            Ok(position) => position,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let checkpoint = match RawCapture::capture_checkpoint(&self.path, position, request.source)
        {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                return Box::pin(async move {
                    Err(refused(&format!("canonical capture failed: {error}")))
                });
            }
        };
        if let Err(error) = self.bind_admission(&request.admission, &checkpoint) {
            return Box::pin(async move { Err(error) });
        }
        if let Some(expected) = request.expected.as_ref() {
            if expected != &checkpoint.cursor {
                return Box::pin(
                    async move { Err(refused("prepared cursor differs from expected")) },
                );
            }
        }
        let notify = Arc::new(CallerNotify(tokio::sync::Notify::new()));
        let (caller, signal) = CanonicalCallerGuard::new(notify);
        if let Ok(mut slot) = self.signal.lock() {
            *slot = Some(signal);
        }
        let prepared = CanonicalPrepared {
            caller,
            operation: CanonicalOperation::new(),
            checkpoint,
        };
        Box::pin(async move { Ok(prepared) })
    }

    fn commit(
        &self,
        _request: CanonicalCommitRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition> {
        // WITHDRAWN, not finished, and the reason is structural rather than a
        // missing call: a store that receives a permit can enter a flight but can
        // never classify it. The recorder is minted from `TransactionControl`,
        // which only Core holds, and frozen v3 gives this request no recorder
        // channel - so any flight the store opened here would drop unclassified and
        // poison the epoch. Taking the store's own grant instead would put two
        // admissions over one row and leave the caller's prepared permit reaching
        // no `admit_final` at all, so caller-loss and adoption would never run.
        //
        // The honest answer is a typed refusal until v3 grows a recorder channel
        // for the coordinator's door. Store-initiated grants on the row path are
        // the door that works today.
        Box::pin(async {
            Err(refused(
                "the store cannot classify a flight it did not mint: canonical v3 gives commit no recorder channel",
            ))
        })
    }

    fn acknowledge(&self, _request: CanonicalAcknowledgement) -> CanonicalFuture<()> {
        // A refusal, never a false success: returning Ok here would read as
        // "adoption bound" to the coordinator while nothing was bound.
        Box::pin(async { Err(refused("acknowledge binding is not wired")) })
    }
}

#[cfg(test)]
mod ordering_tests {
    //! A witness for the allowed lock order `SessionCore -> Core-entry`.
    //!
    //! The ruling allows taking a canonical grant while the daemon holds the
    //! ordinary session lock, because the owner-entry path is synchronous and
    //! never calls back into the daemon. This test executes that assumption: an
    //! ordinary mutex stands in for `SessionCore`, the guard stays live across the
    //! whole record path, and the record path is a sequence of plain synchronous
    //! calls, so it cannot be waiting for the guard to be released.
    //!
    //! What it does NOT prove: that no future code awaits inside the owner path.
    //! The compiler proves that at the call sites in this test - each call below is
    //! non-async, so a change to an async signature breaks this file rather than
    //! silently deadlocking production. The rule itself lives in the doc comment
    //! where the grant is taken; this test is its witness, not its proof.

    use std::sync::{Arc, Mutex};

    use pa_core::session::canonical::{
        CanonicalCursor, CanonicalEntryDraft, CanonicalFrontier, CanonicalIoStage,
        CanonicalMutationKind, CanonicalOwnerScope,
    };
    use pa_core::session_engine::canonical_coordinator::{CanonicalTestDrain, CanonicalTestEpoch};

    use super::super::canonical_write_seam::commit_canonical_rows;

    fn source() -> CanonicalCursor {
        CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "ordering-session".to_owned(),
            source_path: std::path::PathBuf::from("/private/ordering.jsonl"),
            raw_source_generation: [4; 32],
            header_certificate: [5; 32],
            canonical_revision: 0,
            branch_revision: 1,
            selected_leaf: None,
            frontier: CanonicalFrontier {
                bytes: 9,
                rows: 1,
                prefix_certificate: [6; 32],
            },
        }
    }

    fn draft() -> CanonicalEntryDraft {
        // The serving capture holds context-policy authority, so the witness
        // admits a compaction draft: the authority kind must match the draft.
        CanonicalEntryDraft::new(
            CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(serde_json::json!({
                "type": "compaction",
                "summary": "ordering",
                "firstKeptEntryId": "a",
                "tokensBefore": 1,
                "details": {"bytes": "AA=="}
            }))
            .expect("native entry"),
        )
        .expect("draft")
    }

    #[test]
    fn grant_and_record_complete_while_the_ordinary_lock_is_held() {
        let mut epoch = CanonicalTestEpoch::new(source()).expect("epoch");
        epoch
            .capture_serving(
                Arc::new(String::from("run")),
                Arc::new(String::from("model")),
            )
            .expect("serving capture");
        let cursor = epoch.cursor();
        let path = std::env::temp_dir().join(format!(
            "pa-canonical-ordering-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        // The ordinary session lock, held across the entire record path.
        let core: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let guard = core.lock().expect("core lock");

        let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
        let outcome = commit_canonical_rows(
            ticket,
            CanonicalIoStage::RequestedMutation,
            &cursor,
            &operation,
            &draft(),
            &path,
            b"{\"row\":\"ordering\"}\n",
        )
        .expect("record path");

        // Both halves of the assumption, observed from inside the guard's scope:
        // the write happened, and it happened without the guard being released.
        assert_eq!(outcome, super::super::canonical_io::IoOutcome::DurableOk);
        assert_eq!(
            std::fs::read(&path).expect("bytes"),
            b"{\"row\":\"ordering\"}\n"
        );
        assert_eq!(
            std::mem::size_of_val(&*guard),
            0,
            "the guard is still alive here, so the record path ran under it"
        );
        drop(guard);

        epoch.request_close().expect("close");
        assert_eq!(
            epoch.close_status(),
            CanonicalTestDrain::Pending {
                io: 0,
                transactions: 0,
                anchors: 1,
            },
            "the settled flight leaves nothing pending, only the live anchor"
        );
        let _ = std::fs::remove_file(&path);
    }
}
