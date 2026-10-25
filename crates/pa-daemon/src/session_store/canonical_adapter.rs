//! Per-row epoch admission, provider-step ownership, and poisoned-read checks.

use std::sync::Arc;

use pa_core::session::canonical::{
    CanonicalEntryDraft, CanonicalFailureKind, CanonicalIoStage, CanonicalMutationKind,
    CanonicalRequestedWrite, CanonicalSourceDisposition, CanonicalStoreError,
};
use pa_core::session_engine::canonical_coordinator::{
    CanonicalAppendGrant, CanonicalEpoch, CanonicalProviderStep,
};
use pa_types::session::FileEntry;

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
