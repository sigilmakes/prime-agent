//! Live physical owner of the canonical journal. No model callbacks or async locks.
//!
//! The state is attached once at the actual lease boundary, shared by snapshots,
//! and retained until exact Core adoption is acknowledged. A lost receipt never
//! turns into permission to write or project another source.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pa_core::session::canonical::{
    CanonicalAckPhase, CanonicalAcknowledgement, CanonicalAdmissionPolicy, CanonicalAppendReceipt,
    CanonicalCallerGuard, CanonicalCallerNotifier, CanonicalCallerSignal, CanonicalCallerState,
    CanonicalCommitDisposition, CanonicalCursor, CanonicalFailureKind, CanonicalIoStage,
    CanonicalOperation, CanonicalPrepareMode, CanonicalPrepared, CanonicalRequestedWrite,
    CanonicalSourceDisposition, CanonicalStoreError,
};
use pa_core::session_engine::canonical_coordinator::{
    CanonicalAdmittedTool, CanonicalAppendGrant, CanonicalOwnerEpoch, CanonicalOwnerRun,
    CanonicalProviderStep, MetadataAction,
};
use pa_core::session_engine::canonical_live::{
    CanonicalLiveAcknowledged, CanonicalLiveAppendRequest, CanonicalLiveCustomPrepared,
    CanonicalLiveCustomRequest, CanonicalLivePrepareRequest, CanonicalLiveProviderStepRequest,
    CanonicalLiveRun, CanonicalLiveRunRequest, CanonicalLiveSnapshot, CanonicalLiveToolRequest,
    CanonicalLiveUserPrepared, CanonicalLiveUserRequest,
};

use super::canonical_raw::{CaptureMode, RawCapture, RawPosition};
use super::canonical_revision::{BarrierTicket, CanonicalRevision};
use super::{SessionEntry, SessionFile};

#[derive(Clone, Copy)]
enum NavigationPreparation {
    MoveOnly,
    PrepareCheckpoint,
}

#[derive(Clone, Copy)]
enum OwnerDelivery {
    User,
    Custom,
    Metadata,
}

type Result<T> = std::result::Result<T, CanonicalStoreError>;

fn failure(detail: impl std::fmt::Display) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::Unchanged,
        detail: detail.to_string(),
    }
}

fn unresolved(detail: impl std::fmt::Display) -> CanonicalStoreError {
    unresolved_with(detail, None)
}

/// One refusal whose outcome is unresolved. `reason` overrides the fixed-detail
/// lookup with the caller's own fixed source-check label.
fn unresolved_with(
    detail: impl std::fmt::Display,
    reason: Option<&'static str>,
) -> CanonicalStoreError {
    let detail = detail.to_string();
    // Report only fixed source-check labels, never the dynamic error payload.
    let reason = reason.unwrap_or_else(|| label_of(&detail));
    eprintln!("pa-daemon: canonical refusal phase=owner reason={reason}");
    CanonicalStoreError {
        kind: CanonicalFailureKind::AdoptionConflict,
        requested: CanonicalRequestedWrite::Unknown,
        source: CanonicalSourceDisposition::NeedsReconcile,
        detail,
    }
}

/// How long one caller waits for another caller's adoption window. A window is
/// a local prepare/write/ACK round trip on the same machine, so this is orders
/// of magnitude above a real settle and far below a client turn timeout; it
/// exists so a broken or abandoned caller cannot park a turn forever.
pub(crate) const ADOPTION_ORDER_WAIT: Duration = Duration::from_secs(5);

/// Wait for the owner's adoption window to close, without holding a lock.
///
/// The signal is armed before the deciding check, so a release that lands
/// between the check and the await is not lost. Every wake re-reads the actual
/// window instead of assuming it closed, and the deadline bounds the wait.
pub(crate) async fn wait_out_adoption(
    window: impl Fn() -> Result<Option<(Arc<tokio::sync::Notify>, bool)>>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    loop {
        let Some((signal, _)) = window()? else {
            return Ok(());
        };
        let notified = signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !window()?.is_some_and(|(_, pending)| pending) {
            return Ok(());
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Err(pending_adoption("canonical owner adoption did not settle"));
        }
    }
}

/// A request refused by another caller's in-flight adoption window. The owner
/// proves it created no operation, barrier or write, so the caller's resolution
/// is to wait for that caller's ACK and issue the same request again.
fn pending_adoption(detail: impl std::fmt::Display) -> CanonicalStoreError {
    let detail = detail.to_string();
    eprintln!(
        "pa-daemon: canonical refusal phase=owner reason={}",
        label_of(&detail)
    );
    CanonicalStoreError {
        kind: CanonicalFailureKind::AdoptionConflict,
        requested: CanonicalRequestedWrite::NotRequested,
        source: CanonicalSourceDisposition::PendingAdoption,
        detail,
    }
}

fn label_of(detail: &str) -> &'static str {
    match detail {
        "canonical owner adoption abandoned" => "canonical_owner_abandoned_pending_adoption",
        "canonical owner invalidated" => "canonical_owner_invalidated",
        "canonical owner authority poisoned" => "canonical_owner_authority_poisoned",
        "canonical owner awaiting adoption acknowledgement" => "canonical_owner_pending_adoption",
        "canonical owner adoption did not settle" => "canonical_owner_adoption_wait_timeout",
        "canonical owner caller abandoned before acknowledgement" => {
            "canonical_owner_abandoned_caller"
        }
        "canonical lease expired" => "canonical_lease_expired",
        "canonical owner path/header/lease changed" => "canonical_owner_path_header_lease_changed",
        "canonical serving epoch is poisoned" => "canonical_serving_epoch_is_poisoned",
        "canonical owner mutex poisoned" => "canonical_owner_mutex_poisoned",
        "canonical owner has prepared mutations" => "canonical_owner_prepared_mutations",
        "canonical owner has admitted tools" => "canonical_owner_admitted_tools",
        "canonical serving run is live" => "canonical_serving_run_live",
        "canonical run has no terminal ACK" => "canonical_run_terminal_unacknowledged",
        "canonical child usage I/O is unresolved" => "canonical_child_usage_unresolved",
        "canonical caller has not completed" => "canonical_caller_incomplete",
        "actual tool executions still lack terminal adoption" => {
            "actual_tool_executions_still_lack_terminal_adoption"
        }
        "serving epoch still borrowed at ordered boundary" => {
            "serving_epoch_still_borrowed_at_ordered_boundary"
        }
        "prepared mutation lost caller binding" => "prepared_mutation_lost_caller_binding",
        "commit pending state lost" => "commit_pending_state_lost",
        "staged delivery has no FIFO head" => "staged_delivery_has_no_fifo_head",
        "staged delivery FIFO changed" => "staged_delivery_fifo_changed",
        "user pending state lost" => "user_pending_state_lost",
        "staged operation lost" => "staged_operation_lost",
        "staged delivery lost" => "staged_delivery_lost",
        "delivered Custom FIFO row missing from physical source" => {
            "delivered_custom_fifo_row_missing_from_physical_source"
        }
        "delivered Custom FIFO changed during preparation" => {
            "delivered_custom_fifo_changed_during_preparation"
        }
        detail if detail.starts_with("revision: ") => "revision",
        detail if detail.starts_with("invalidate: ") => "invalidate",
        detail if detail.starts_with("navigation prepare barrier: ") => {
            "navigation_prepare_barrier"
        }
        detail if detail.starts_with("ordered source boundary: ") => "ordered_source_boundary",
        detail if detail.starts_with("ACK barrier: ") => "ack_barrier",
        detail if detail.starts_with("commit admission: ") => "commit_admission",
        detail if detail.starts_with("commit start: ") => "commit_start",
        detail if detail.starts_with("commit finish: ") => "commit_finish",
        detail if detail.starts_with("user admission: ") => "user_admission",
        detail if detail.starts_with("user write start: ") => "user_write_start",
        detail if detail.starts_with("user finish: ") => "user_finish",
        detail if detail.starts_with("delivery admission: ") => "delivery_admission",
        detail if detail.starts_with("delivery write start: ") => "delivery_write_start",
        detail if detail.starts_with("delivery finish: ") => "delivery_finish",
        "canonical source advanced outside owner" => "unowned_source_advance",
        "canonical source changed outside owner" => "unowned_source_change",
        "canonical Custom source changed outside owner" => "unowned_custom_source_change",
        "canonical Custom admission source CAS conflict" => "custom_admission_source_changed",
        "canonical user admission source CAS conflict" => "user_admission_source_changed",
        "canonical prepare source/branch CAS conflict" => "prepare_source_branch_changed",
        "canonical Custom write lacks exact single physical suffix" => {
            "custom_write_physical_suffix"
        }
        "delivered Custom FIFO head differs from exact accepted intent" => {
            "delivered_custom_intent_mismatch"
        }
        detail if detail.starts_with("Custom write admission: ") => "custom_write_admission",
        detail if detail.starts_with("Custom write start: ") => "custom_write_start",
        detail if detail.starts_with("Custom write finish: ") => "custom_write_finish",
        detail if detail.starts_with("Custom preflight: ") => "custom_preflight",
        detail if detail.starts_with("Custom delivery barrier: ") => "custom_delivery_barrier",
        detail if detail.starts_with("Custom ledger: ") => "custom_ledger",
        "actual AgentStart belongs to another certified source" => "actual_run_source_mismatch",
        "actual AgentStart source changed outside owner" => "actual_run_external_source_change",
        "actual run was already registered" => "actual_run_duplicate_registration",
        "previous actual run has no acknowledged native terminal row" => {
            "actual_run_terminal_unacknowledged"
        }
        _ => "unclassified_owner_error",
    }
}

/// The owner's own completion signal. Every caller-state transition and every
/// adoption-window release wakes the callers waiting on that window, so an
/// ordering conflict becomes a wait instead of a refusal.
struct Notify(Arc<tokio::sync::Notify>);
impl CanonicalCallerNotifier for Notify {
    fn changed(&self) {
        self.0.notify_waiters();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingProvenance {
    UnwrittenObservation,
    Retained,
}

struct Pending {
    /// When this window was armed. An adoption whose caller can never return is
    /// released after [`ADOPTION_ORDER_WAIT`]; a live caller is never awaited
    /// that long because the owner state mutex serializes entire operations.
    armed: std::time::Instant,
    provenance: PendingProvenance,
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    barrier: BarrierTicket,
    prepared: bool,
    mutation: bool,
    signal: Option<CanonicalCallerSignal>,
    staged: bool,
    from: CanonicalCursor,
    opening_run: Option<Arc<CanonicalLiveRun>>,
}

struct RegisteredRun {
    identity: Arc<CanonicalLiveRun>,
    abort: Option<CanonicalOwnerRun>,
    ever_provider_admitted: bool,
    terminal_acked: bool,
}

struct PreparedBinding {
    cursor: CanonicalCursor,
    signal: CanonicalCallerSignal,
}

struct State {
    revision: CanonicalRevision,
    authority: CanonicalOwnerEpoch,
    provider: Option<CanonicalProviderStep>,
    serving_run: Option<Arc<CanonicalLiveRun>>,
    registered_run: Option<RegisteredRun>,
    tools: HashMap<String, CanonicalAdmittedTool>,
    lease: Weak<crate::lease::SessionLease>,
    raw: RawCapture,
    pending: Option<Pending>,
    prepared: HashMap<CanonicalOperation, PreparedBinding>,
    delivered: VecDeque<String>,
    delivered_custom: VecDeque<String>,
    registered: HashSet<String>,
    admitted: HashSet<String>,
    blocked: bool,
    #[cfg(test)]
    fail_stage_capture: bool,
    callers: Vec<CanonicalCallerSignal>,
    /// Wakes waiters whenever this owner's adoption window closes.
    adoption: Arc<tokio::sync::Notify>,
}

/// Clone-shared ownership state. Read-only opens never create this allocation.
#[derive(Clone)]
pub(crate) struct LiveCanonicalOwner(Arc<Mutex<State>>);
impl std::fmt::Debug for LiveCanonicalOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveCanonicalOwner(<lease-bound>)")
    }
}

impl LiveCanonicalOwner {
    pub(super) fn invalidate(&self) {
        match self.0.lock() {
            Ok(mut state) => {
                state.blocked = true;
                if let Err(error) = state.revision.invalidate() {
                    eprintln!("pa-daemon: canonical revision invalidation failed: {error:?}");
                }
            }
            Err(_) => eprintln!("pa-daemon: canonical owner mutex poisoned at invalidation"),
        }
    }
}

/// Why a refused request is not a certified unstarted prior source. Each variant
/// is a fixed log label: the refusal must name the actual cause, never the
/// generic unclassified fallback.
#[derive(Clone, Copy)]
enum UnstartedRefusal {
    Unavailable,
    PreparedWork,
    OwnerScope,
    Session,
    Path,
    Header,
    Branch,
    Revision,
}

impl UnstartedRefusal {
    fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "refused_unstarted_owner_unavailable",
            Self::PreparedWork => "refused_unstarted_prepared_work",
            Self::OwnerScope => "refused_unstarted_owner_scope_changed",
            Self::Session => "refused_unstarted_session_changed",
            Self::Path => "refused_unstarted_path_changed",
            Self::Header => "refused_unstarted_header_changed",
            Self::Branch => "refused_unstarted_branch_changed",
            Self::Revision => "refused_unstarted_revision_ahead",
        }
    }
}

impl State {
    /// Release an adoption window that provably wrote nothing and whose caller
    /// can never return to complete it.
    ///
    /// Two things make a window unresolvable: its caller guard dropped (recorded
    /// `Abandoned`, after which the ACK necessarily refuses), or a provisional
    /// window armed before a fallible preflight was left with no caller at all.
    /// Either one refuses every later caller forever if kept. Neither wrote
    /// anything: the owner state mutex serializes a whole operation, so no
    /// in-flight write is ever observable here, and an operation that wrote
    /// advances `cursor` past `from`. A written-but-unacknowledged row keeps its
    /// window: that is committed debt, and only the reconcile boundary may
    /// resolve it. A live caller (active or holding an acknowledged prepared
    /// ticket) keeps its window for as long as its work takes.
    fn expire_unwritten_adoption(&mut self) -> Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(());
        };
        if pending.cursor != pending.from || pending.armed.elapsed() < ADOPTION_ORDER_WAIT {
            return Ok(());
        }
        let live = pending.signal.as_ref().is_some_and(|signal| {
            matches!(
                signal.state(),
                CanonicalCallerState::Active | CanonicalCallerState::PreparedAcknowledged
            )
        });
        if live {
            return Ok(());
        }
        let operation = pending.operation.clone();
        let barrier = pending.barrier.clone();
        self.revision
            .clear_barrier(&operation, &barrier)
            .map_err(|e| unresolved(format!("expired adoption barrier: {e:?}")))?;
        self.pending.take();
        self.adoption.notify_waiters();
        Ok(())
    }

    // Called only inside the verified lease mutation, before this request creates
    // an operation. A stale SDK source is not permission to ignore existing debt.
    fn refuse_unstarted(
        &self,
        expected: &pa_core::session::canonical::CanonicalCursor,
        detail: &str,
    ) -> CanonicalStoreError {
        match self.certify_unstarted(expected) {
            Ok(()) => CanonicalStoreError {
                kind: CanonicalFailureKind::SourceChanged,
                requested: CanonicalRequestedWrite::NotRequested,
                source: CanonicalSourceDisposition::ReadyForObservation,
                detail: detail.to_owned(),
            },
            Err(refusal) => unresolved_with(detail, Some(refusal.label())),
        }
    }

    /// Certify that `expected` is a previous source of this same owned journal and
    /// that this request wrote nothing, so Core can re-observe and retry.
    ///
    /// Only the refusal's own debt matters: an unacknowledged write
    /// (`available`), or a prepared mutation whose adoption Core never
    /// committed. Unrelated work that is still settling — an admitted tool
    /// awaiting its terminal, a live run, an unfinished caller — does not make
    /// this unwritten refusal uncertain, because it is reached before any
    /// operation, barrier or write exists.
    fn certify_unstarted(
        &self,
        expected: &CanonicalCursor,
    ) -> std::result::Result<(), UnstartedRefusal> {
        self.available()
            .map_err(|_| UnstartedRefusal::Unavailable)?;
        if !self.prepared.is_empty() {
            return Err(UnstartedRefusal::PreparedWork);
        }
        let raw = &self.raw.cursor;
        if expected.owner != raw.owner {
            return Err(UnstartedRefusal::OwnerScope);
        }
        if expected.session_id != raw.session_id {
            return Err(UnstartedRefusal::Session);
        }
        if expected.source_path != raw.source_path {
            return Err(UnstartedRefusal::Path);
        }
        if expected.header_certificate != raw.header_certificate {
            return Err(UnstartedRefusal::Header);
        }
        if expected.branch_revision != raw.branch_revision {
            return Err(UnstartedRefusal::Branch);
        }
        if expected.canonical_revision > raw.canonical_revision {
            return Err(UnstartedRefusal::Revision);
        }
        Ok(())
    }

    /// The strict run-opening form: no adoption, mutation, tool or caller debt may
    /// remain. An `AgentStart` may only open on such a fully settled source.
    fn certifies_unstarted_source(&self, expected: &CanonicalCursor) -> bool {
        self.certify_unstarted(expected).is_ok()
            && self.tools.is_empty()
            && self.callers.iter().all(|signal| {
                signal.state() == pa_core::session::canonical::CanonicalCallerState::Completed
            })
    }

    fn terminal_metadata_suffix(
        &self,
        expected: &CanonicalCursor,
    ) -> Result<Vec<pa_core::session::canonical::CanonicalJournalRow>> {
        let rows = self.raw.certified_suffix_since(expected).map_err(failure)?;
        let mut leaf = expected.selected_leaf.as_deref();
        for row in &rows {
            if !matches!(
                row.entry(),
                pa_types::session::FileEntry::ChildUsageAttributed { .. }
            ) || row.entry().parent_id() != leaf
            {
                return Err(failure(
                    "native terminal source advanced beyond owner metadata",
                ));
            }
            leaf = row.entry().id();
        }
        if leaf != self.raw.cursor.selected_leaf.as_deref() {
            return Err(failure(
                "native terminal metadata changed selected ancestry",
            ));
        }
        Ok(rows)
    }

    fn position(&self, leaf: Option<String>) -> Result<RawPosition> {
        let revision = self
            .revision
            .snapshot()
            .map_err(|e| unresolved(format!("revision: {e:?}")))?;
        Ok(RawPosition {
            owner: revision.owner,
            canonical_revision: revision.canonical_revision,
            branch_revision: revision.branch_revision,
            selected_leaf: leaf,
        })
    }

    fn available(&self) -> Result<()> {
        if self.blocked {
            return Err(unresolved("canonical owner invalidated"));
        }
        if self.authority.poisoned() {
            return Err(unresolved("canonical owner authority poisoned"));
        }
        let abandoned = self
            .callers
            .iter()
            .chain(
                self.pending
                    .iter()
                    .filter_map(|pending| pending.signal.as_ref()),
            )
            .chain(self.prepared.values().map(|binding| &binding.signal))
            .any(|signal| {
                signal.state() == pa_core::session::canonical::CanonicalCallerState::Abandoned
            });
        match (self.pending.is_some(), abandoned) {
            (true, true) => return Err(unresolved("canonical owner adoption abandoned")),
            (true, false) => {
                return Err(pending_adoption(
                    "canonical owner awaiting adoption acknowledgement",
                ));
            }
            (false, true) => {
                return Err(unresolved(
                    "canonical owner caller abandoned before acknowledgement",
                ));
            }
            (false, false) => {}
        }
        self.revision
            .snapshot()
            .map_err(|e| unresolved(format!("revision: {e:?}")))?;
        Ok(())
    }

    fn verify(&self, file: &SessionFile) -> Result<Arc<crate::lease::SessionLease>> {
        let lease = self
            .lease
            .upgrade()
            .ok_or_else(|| unresolved("canonical lease expired"))?;
        if !file
            .lease
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &lease))
            || file.path != self.raw.cursor.source_path
            || file.header.id != self.raw.cursor.session_id
            || !matches!(self.raw.header.entry(), pa_types::session::FileEntry::Header { header } if header == &file.header)
        {
            self.revision
                .invalidate()
                .map_err(|e| unresolved(format!("invalidate: {e:?}")))?;
            return Err(unresolved("canonical owner path/header/lease changed"));
        }
        if super::canonical_adapter::epoch_is_poisoned(file.epoch.as_ref()) {
            return Err(unresolved("canonical serving epoch is poisoned"));
        }
        Ok(lease)
    }

    fn current(&self, file: &SessionFile) -> Result<RawCapture> {
        RawCapture::capture(
            &file.path,
            self.position(file.leaf_id.clone())?,
            CaptureMode::Full,
        )
        .map_err(failure)
    }

    fn safe_visible_leaf(&self) -> anyhow::Result<Option<String>> {
        let selected: HashMap<_, _> = self
            .raw
            .plan
            .ancestry
            .iter()
            .filter_map(|index| {
                self.raw.plan.rows[*index]
                    .id
                    .as_ref()
                    .map(|id| (id.as_str(), *index))
            })
            .collect();
        let mut queued = HashSet::new();
        for id in &self.delivered {
            let index = *selected.get(id.as_str()).ok_or_else(|| {
                failure("pending User delivery is not on certified selected branch")
            })?;
            anyhow::ensure!(
                queued.insert(id.as_str()),
                "duplicate pending delivery identity"
            );
            anyhow::ensure!(
                matches!(
                    self.raw.retained[index].entry(),
                    pa_types::session::FileEntry::Message {
                        message: pa_types::session::AgentMessage::User(_),
                        ..
                    }
                ),
                "pending User ledger has another canonical kind"
            );
        }
        for id in &self.delivered_custom {
            let index = *selected.get(id.as_str()).ok_or_else(|| {
                failure("pending Custom delivery is not on certified selected branch")
            })?;
            anyhow::ensure!(
                queued.insert(id.as_str()),
                "duplicate pending delivery identity"
            );
            anyhow::ensure!(
                matches!(
                    self.raw.retained[index].entry(),
                    pa_types::session::FileEntry::CustomMessage { .. }
                ),
                "pending Custom ledger has another canonical kind"
            );
        }
        let visible_leaf = self
            .raw
            .plan
            .ancestry
            .iter()
            .find_map(|index| {
                let row = &self.raw.plan.rows[*index];
                row.id
                    .as_deref()
                    .filter(|id| queued.contains(id))
                    .map(|_| row.parent_id.clone())
            })
            .unwrap_or_else(|| self.raw.cursor.selected_leaf.clone());
        Ok(visible_leaf)
    }

    fn approve_admission(&self, policy: &CanonicalAdmissionPolicy) -> Result<()> {
        match policy {
            CanonicalAdmissionPolicy::Preserve => Ok(()),
            CanonicalAdmissionPolicy::Append(ids) => {
                let mut seen = HashSet::new();
                for id in ids {
                    if !self.registered.contains(id)
                        || self.admitted.contains(id)
                        || !seen.insert(id)
                    {
                        return Err(failure(
                            "user admission lacks exact owner delivery or repeats a delivery",
                        ));
                    }
                }
                Ok(())
            }
            CanonicalAdmissionPolicy::Replace(ids) => {
                if !self.admitted.is_empty() || ids.iter().any(|id| !self.registered.contains(id)) {
                    return Err(failure(
                        "replay reset is not the initial owner-delivered binding",
                    ));
                }
                Ok(())
            }
        }
    }
}

impl SessionFile {
    /// Attach at the actual writer/lease boundary, never on an SDK cache reload.
    pub(crate) fn attach_canonical_owner(&mut self) -> Result<CanonicalCursor> {
        if let Some(owner) = self.canonical_live.clone() {
            let state = owner
                .0
                .lock()
                .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
            state.available()?;
            let lease = state.verify(self)?;
            return lease
                .with_owned_mutation(&self.path, || {
                    let current = state.current(self)?;
                    anyhow::ensure!(
                        current.cursor == state.raw.cursor,
                        "canonical source advanced outside owner"
                    );
                    Ok(current.cursor)
                })
                .map_err(unresolved);
        }
        if self.child_usage_recovery.pending.is_some() {
            return Err(unresolved("canonical owner has unresolved child usage I/O"));
        }
        let lease = self
            .lease
            .clone()
            .ok_or_else(|| failure("live canonical owner requires session lease"))?;
        let revision = CanonicalRevision::new();
        let snapshot = revision
            .snapshot()
            .map_err(|e| failure(format!("revision: {e:?}")))?;
        let position = RawPosition {
            owner: snapshot.owner,
            canonical_revision: snapshot.canonical_revision,
            branch_revision: snapshot.branch_revision,
            selected_leaf: self.leaf_id.clone(),
        };
        let raw = lease
            .with_owned_mutation(&self.path, || {
                let raw = RawCapture::capture(&self.path, position, CaptureMode::Full)?;
                anyhow::ensure!(
                    raw.cursor.session_id == self.header.id,
                    "canonical header mismatch"
                );
                let pa_types::session::FileEntry::Header { header } = raw.header.entry() else {
                    anyhow::bail!("canonical source lacks header");
                };
                anyhow::ensure!(header == &self.header, "canonical owner header differs");
                Ok(raw)
            })
            .map_err(failure)?;
        let cursor = raw.cursor.clone();
        self.canonical_live = Some(LiveCanonicalOwner(Arc::new(Mutex::new(State {
            authority: CanonicalOwnerEpoch::genesis(&cursor),
            provider: None,
            serving_run: None,
            registered_run: None,
            tools: HashMap::new(),
            revision,
            lease: Arc::downgrade(&lease),
            raw,
            pending: None,
            prepared: HashMap::new(),
            delivered: self.canonical_delivered.clone(),
            delivered_custom: self.canonical_delivered_custom.clone(),
            registered: self.canonical_delivered.iter().cloned().collect(),
            admitted: HashSet::new(),
            blocked: false,
            #[cfg(test)]
            fail_stage_capture: false,
            callers: Vec::new(),
            adoption: Arc::new(tokio::sync::Notify::new()),
        }))));
        Ok(cursor)
    }

    /// Read-only certification. No operation, caller, barrier or admission is created.
    pub(crate) fn canonical_snapshot(&self) -> Result<CanonicalLiveSnapshot> {
        let owner = self
            .canonical_live
            .as_ref()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path, || {
                let current = state.current(self)?;
                anyhow::ensure!(
                    current.cursor == state.raw.cursor,
                    "canonical snapshot source changed outside owner"
                );
                Ok(CanonicalLiveSnapshot {
                    checkpoint: current
                        .checkpoint(pa_core::session::canonical::CanonicalSourceRequest::Full)?,
                    admitted_users: state.admitted.clone(),
                    pending_users: state.delivered.clone(),
                    pending_custom: state.delivered_custom.clone(),
                    safe_visible_leaf: state.safe_visible_leaf()?,
                })
            })
            .map_err(failure)
    }

    /// The worker calls this only for an actually accepted durable User row.
    pub(crate) fn register_delivered_user(&mut self, id: &str) -> Result<()> {
        let row = self
            .entry(id)
            .ok_or_else(|| failure("delivered user is not durable owner row"))?;
        if !super::canonical_adapter::is_user_row(row) {
            return Err(failure("delivered row is not User"));
        }
        if let Some(owner) = &self.canonical_live {
            let mut state = owner
                .0
                .lock()
                .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
            state.available()?;
            if state.registered.contains(id) {
                return Err(failure("user delivery already registered"));
            }
            let lease = state.verify(self)?;
            lease
                .with_owned_mutation(&self.path, || {
                    anyhow::ensure!(
                        state.current(self)?.cursor == state.raw.cursor,
                        "user registration source changed outside owner"
                    );
                    state
                        .revision
                        .advance_owner_state(None)
                        .map_err(|e| failure(format!("delivery registration: {e:?}")))?;
                    let position = state.position(self.leaf_id.clone())?;
                    state.raw.refresh_owner_position(&position);
                    state.registered.insert(id.to_owned());
                    state.delivered.push_back(id.to_owned());
                    Ok(())
                })
                .map_err(failure)?;
        } else {
            if self.canonical_delivered.iter().any(|prior| prior == id) {
                return Err(failure("user delivery already registered"));
            }
            self.canonical_delivered.push_back(id.to_owned());
        }
        Ok(())
    }

    /// Permanently retire a replaced actual owner. Never resets poison or pending I/O.
    pub(crate) fn invalidate_canonical_owner(&mut self) -> Result<()> {
        let Some(owner) = self.canonical_live.clone() else {
            return Ok(());
        };
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "replacement source changed"
                );
                self.retire_canonical_source(&mut state)?;
                state.blocked = true;
                state
                    .revision
                    .invalidate()
                    .map_err(|e| unresolved(format!("invalidate: {e:?}")))?;
                Ok(())
            })
            .map_err(unresolved)
    }

    /// Retire only a proven completed runtime, preserving exact undelivered FIFOs.
    pub(crate) fn retire_completed_canonical_runtime(
        &mut self,
        expected: &CanonicalCursor,
    ) -> Result<()> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor
                        && expected == &state.raw.cursor,
                    "completed retirement source changed"
                );
                anyhow::ensure!(
                    self.child_usage_recovery.pending.is_none(),
                    "child usage I/O remains unresolved"
                );
                anyhow::ensure!(
                    state.prepared.is_empty() && state.tools.is_empty(),
                    "actual effects remain unfinished"
                );
                anyhow::ensure!(
                    state
                        .registered_run
                        .as_ref()
                        .is_none_or(|run| run.terminal_acked),
                    "actual run has no terminal ACK"
                );
                // The local Core queue was checked by the retiring view. Owner membership
                // is historical admission, never a substitute execution queue.
                self.retire_owned_epoch(&mut state)
                    .map_err(anyhow::Error::from)
            })
            .map_err(unresolved)
    }

    /// Retire this actual epoch at the already-certified durable boundary.
    ///
    /// The caller proves the durable source before this runs; here only the
    /// owner's ordered close and detach remain. Undelivered FIFOs survive.
    fn retire_owned_epoch(&mut self, state: &mut State) -> Result<()> {
        self.retire_canonical_source(state)?;
        state.blocked = true;
        state
            .revision
            .invalidate()
            .map_err(|e| unresolved(format!("invalidate: {e:?}")))?;
        self.canonical_delivered.clone_from(&state.delivered);
        self.canonical_delivered_custom
            .clone_from(&state.delivered_custom);
        self.canonical_live = None;
        Ok(())
    }

    /// Atomically navigate the actual owner, including branch ABA with no new bytes.
    /// The caller must rebuild its SDK mirror/binding before the next projection.
    pub(crate) fn advance_canonical_branch(&mut self, selected_leaf: Option<&str>) -> Result<()> {
        self.navigate_canonical_source(
            selected_leaf,
            /* expected */ None,
            NavigationPreparation::MoveOnly,
        )?;
        Ok(())
    }

    /// Strict old-source CAS, ordered branch successor and new pending checkpoint
    /// are one lease-fenced operation. Core must explicitly rebind then ACK it.
    pub(crate) fn prepare_canonical_navigation(
        &mut self,
        expected: &CanonicalCursor,
        selected_leaf: Option<&str>,
    ) -> Result<CanonicalPrepared> {
        if self.canonical_live.is_none() {
            self.attach_canonical_owner()?;
        }
        self.navigate_canonical_source(
            selected_leaf,
            Some(expected),
            NavigationPreparation::PrepareCheckpoint,
        )?
        .ok_or_else(|| failure("canonical navigation requires actual leased owner"))
    }

    fn navigate_canonical_source(
        &mut self,
        selected_leaf: Option<&str>,
        expected: Option<&CanonicalCursor>,
        preparation: NavigationPreparation,
    ) -> Result<Option<CanonicalPrepared>> {
        if self.canonical_live.is_none() {
            self.leaf_id = selected_leaf.map(str::to_owned);
            return Ok(None);
        }
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "navigation source changed"
                );
                if let Some(expected) = expected {
                    anyhow::ensure!(
                        expected == &state.raw.cursor,
                        "navigation source/branch/frontier/leaf CAS conflict"
                    );
                }
                anyhow::ensure!(
                    state.delivered.is_empty() && state.delivered_custom.is_empty(),
                    "navigation has unconsumed owner-delivered rows"
                );
                // Validate membership before closing an otherwise healthy source.
                RawCapture::capture(
                    &self.path,
                    state.position(selected_leaf.map(str::to_owned))?,
                    CaptureMode::Full,
                )?;
                let retired = self.retire_canonical_source(&mut state)?;
                state
                    .revision
                    .advance_branch()
                    .map_err(|e| failure(format!("navigation: {e:?}")))?;
                let raw = RawCapture::capture(
                    &self.path,
                    state.position(selected_leaf.map(str::to_owned))?,
                    CaptureMode::Full,
                )?;
                anyhow::ensure!(
                    raw.cursor.raw_source_generation == state.raw.cursor.raw_source_generation
                        && raw.cursor.frontier == state.raw.cursor.frontier
                        && raw.cursor.header_certificate == state.raw.cursor.header_certificate,
                    "navigation changed physical journal"
                );
                state.authority =
                    CanonicalOwnerEpoch::successor_after_navigation(&raw.cursor, retired)?;
                state.raw = raw;
                state.prepared.clear();
                state.admitted.clear();
                state.serving_run = None;
                self.leaf_id = selected_leaf.map(str::to_owned);
                match preparation {
                    NavigationPreparation::MoveOnly => Ok(None),
                    NavigationPreparation::PrepareCheckpoint => {
                        let checkpoint = state.raw.checkpoint(
                            pa_core::session::canonical::CanonicalSourceRequest::Full,
                        )?;
                        let operation = CanonicalOperation::new();
                        let barrier = state.revision.set_barrier(&operation).map_err(|e| {
                            unresolved(format!("navigation prepare barrier: {e:?}"))
                        })?;
                        let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(Arc::clone(&state.adoption))));
                        state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                            provenance: PendingProvenance::Retained,
                            operation: operation.clone(),
                            cursor: checkpoint.cursor.clone(),
                            barrier,
                            prepared: true,
                            mutation: false,
                            signal: Some(signal),
                            staged: false,
                            from: checkpoint.cursor.clone(),
                            opening_run: None,
                        });
                        Ok(Some(CanonicalPrepared {
                            caller,
                            operation,
                            checkpoint,
                        }))
                    }
                }
            })
            .map_err(unresolved)
    }

    fn retire_canonical_source(
        &mut self,
        state: &mut State,
    ) -> Result<pa_core::session_engine::canonical_coordinator::CanonicalRetiredEpoch> {
        if !state.tools.is_empty() {
            return Err(unresolved(
                "actual tool executions still lack terminal adoption",
            ));
        }
        if let Some(run) = state.registered_run.as_mut() {
            if let Some(abort) = run.abort.take() {
                abort.finish_ordered_boundary();
            }
        }
        let closing = state
            .authority
            .request_close_at_current_source(&state.raw.cursor)?;
        if let Some(handle) = self.epoch.take() {
            let epoch = match Arc::try_unwrap(handle.0) {
                Ok(epoch) => epoch,
                Err(epoch) => {
                    self.epoch = Some(super::canonical_adapter::EpochHandle(epoch));
                    state.blocked = true;
                    return Err(unresolved(
                        "serving epoch still borrowed at ordered boundary",
                    ));
                }
            };
            state.provider = None;
            epoch.finish_ordered_boundary();
        }
        closing
            .finish()
            .map_err(|e| unresolved(format!("ordered source boundary: {e:?}")))
    }

    pub(crate) fn prepare_canonical(
        &mut self,
        request: CanonicalLivePrepareRequest,
    ) -> Result<CanonicalPrepared> {
        if self.canonical_live.is_none() {
            self.attach_canonical_owner()?;
        }
        self.confirm_child_usage_write().map_err(failure)?;
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path, || {
                let raw = state.current(self)?;
                anyhow::ensure!(
                    raw.cursor == state.raw.cursor,
                    "canonical source changed outside owner"
                );
                if let Some(expected) = request.expected.as_ref().filter(|expected| {
                    *expected != &raw.cursor
                        && !(request.mode == CanonicalPrepareMode::Mutation(pa_core::session::canonical::CanonicalMutationKind::AdmittedTerminal)
                            && state.terminal_metadata_suffix(expected).is_ok())
                }) {
                    return Err(state.refuse_unstarted(expected, "canonical prepare source/branch CAS conflict").into());
                }
                state.approve_admission(&request.admission)?;
                raw.checkpoint(request.source)?;
                if matches!(&request.admission, CanonicalAdmissionPolicy::Append(ids) | CanonicalAdmissionPolicy::Replace(ids) if !ids.is_empty()) {
                    state.revision.check_owner_state_advance()
                        .map_err(|e| failure(format!("prepare preflight: {e:?}")))?;
                }
                let operation = CanonicalOperation::new();
                let barrier = state.revision.set_barrier(&operation)
                    .map_err(|e| failure(format!("prepare barrier: {e:?}")))?;
                state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                    provenance: PendingProvenance::Retained,
                    operation: operation.clone(), cursor: raw.cursor.clone(), barrier: barrier.clone(),
                    prepared: true, mutation: matches!(request.mode, CanonicalPrepareMode::Mutation(_)),
                    signal: None, staged: false, from: raw.cursor,
                    opening_run: None,
                });
                // A nonempty explicit bootstrap admission is a guarded mutation.
                let ids = match &request.admission {
                    CanonicalAdmissionPolicy::Append(ids) | CanonicalAdmissionPolicy::Replace(ids) => ids.clone(),
                    CanonicalAdmissionPolicy::Preserve => Vec::new(),
                };
                if !ids.is_empty() {
                    state.revision.advance_owner_state(Some(&operation))
                        .map_err(|e| failure(format!("bootstrap admission: {e:?}")))?;
                    let position = state.position(self.leaf_id.clone())?;
                    state.raw.refresh_owner_position(&position);
                }
                let checkpoint = state.raw.checkpoint(request.source)?;
                let from = checkpoint.cursor.clone();
                let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(Arc::clone(&state.adoption))));
                let provenance = if matches!(request.mode, CanonicalPrepareMode::Observe)
                    && match &request.admission {
                        CanonicalAdmissionPolicy::Preserve => true,
                        CanonicalAdmissionPolicy::Append(ids)
                        | CanonicalAdmissionPolicy::Replace(ids) => ids.is_empty(),
                    } {
                    PendingProvenance::UnwrittenObservation
                } else {
                    PendingProvenance::Retained
                };
                let ids = match request.admission {
                    CanonicalAdmissionPolicy::Append(ids)
                    | CanonicalAdmissionPolicy::Replace(ids) => ids,
                    CanonicalAdmissionPolicy::Preserve => Vec::new(),
                };
                state.admitted.extend(ids);
                state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                    provenance,
                    operation: operation.clone(),
                    cursor: checkpoint.cursor.clone(),
                    barrier,
                    prepared: true,
                    mutation: matches!(request.mode, CanonicalPrepareMode::Mutation(_)),
                    signal: Some(signal),
                    staged: false,
                    from,
                    opening_run: None,
                });
                Ok(CanonicalPrepared {
                    caller,
                    operation,
                    checkpoint,
                })
            })
            .map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => failure(error),
            })
    }

    pub(crate) fn acknowledge_canonical(
        &mut self,
        request: CanonicalAcknowledgement,
    ) -> Result<CanonicalLiveAcknowledged> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                if request.phase == CanonicalAckPhase::AbandonPreparedOnly
                    && state.pending.is_none()
                {
        state.expire_unwritten_adoption()?;
                    state.available()?;
                    let prepared = state
                        .prepared
                        .get(&request.operation)
                        .ok_or_else(|| failure("no matching prepared-only canonical operation"))?;
                    anyhow::ensure!(
                        prepared.cursor == request.cursor,
                        "prepared-only abandon cursor mismatch"
                    );
                    anyhow::ensure!(
                        prepared.signal.state()
                            != pa_core::session::canonical::CanonicalCallerState::Abandoned,
                        "prepared-only caller already abandoned"
                    );
                    // The binding proves this operation has not entered I/O:
                    // a write would retain pending adoption until its ACK removes
                    // the binding. Other certified owner deliveries may advance
                    // the source while the caller does work. Abandon only the old
                    // preparation, without adopting or rebasing onto those rows.
                    anyhow::ensure!(
                        state.current(self)?.cursor == state.raw.cursor,
                        "prepared-only source changed outside owner; no requested row written"
                    );
                    let visible_leaf = state.safe_visible_leaf()?;
                    state.prepared.remove(&request.operation);
                    return Ok(CanonicalLiveAcknowledged {
                        operation: request.operation,
                        cursor: request.cursor,
                        visible_leaf,
                    });
                }
                let pending = state
                    .pending
                    .as_ref()
                    .ok_or_else(|| failure("no canonical adoption pending"))?;
                anyhow::ensure!(
                    pending.operation == request.operation && pending.cursor == request.cursor,
                    "canonical ACK operation/cursor mismatch"
                );
                if request.phase == CanonicalAckPhase::AbandonPreparedOnly {
                    anyhow::ensure!(
                        pending.provenance == PendingProvenance::UnwrittenObservation
                            && pending.prepared && !pending.mutation && !pending.staged
                            && pending.signal.as_ref().is_some_and(|signal| signal.state()
                                != pa_core::session::canonical::CanonicalCallerState::PreparedAcknowledged),
                        "pending operation is not a proven unwritten observation"
                    );
                    anyhow::ensure!(
                        !state.blocked && !state.authority.poisoned()
                            && !state.callers.iter().any(|signal| signal.state()
                                == pa_core::session::canonical::CanonicalCallerState::Abandoned)
                            && !state.prepared.values().any(|binding| binding.signal.state()
                                == pa_core::session::canonical::CanonicalCallerState::Abandoned),
                        "another canonical operation remains unresolved"
                    );
                    anyhow::ensure!(
                        state.raw.cursor == request.cursor
                            && pending.from == request.cursor
                            && state.current(self)?.cursor == request.cursor,
                        "unwritten observation source changed"
                    );
                    let visible_leaf = state.safe_visible_leaf()?;
                    state.revision.clear_barrier(&request.operation, &pending.barrier)
                        .map_err(|e| unresolved(format!("ACK barrier: {e:?}")))?;
                    state.pending.take();
                    state.adoption.notify_waiters();
                    return Ok(CanonicalLiveAcknowledged {
                        operation: request.operation,
                        cursor: request.cursor,
                        visible_leaf,
                    });
                }
                let phase_matches = match request.phase {
                    CanonicalAckPhase::Prepared => pending.prepared,
                    CanonicalAckPhase::AppliedReady => !pending.prepared,
                    CanonicalAckPhase::ReconciledBlocked | CanonicalAckPhase::Quarantined
                    | CanonicalAckPhase::AbandonPreparedOnly => false,
                };
                anyhow::ensure!(
                    phase_matches,
                    "canonical ACK cannot reopen blocked or wrong-stage source"
                );
                if request.phase != CanonicalAckPhase::AbandonPreparedOnly {
                    let certificate = request.adoption.as_ref().ok_or_else(|| {
                        failure("canonical ACK requires Core adoption certificate")
                    })?;
                    anyhow::ensure!(
                        certificate.operation() == &request.operation
                            && certificate.cursor() == &request.cursor,
                        "canonical adoption certificate mismatch"
                    );
                }
                let current = state.current(self)?;
                anyhow::ensure!(
                    current.cursor == pending.cursor,
                    "canonical source changed before ACK"
                );
                if let Some(signal) = &pending.signal {
                    anyhow::ensure!(
                        signal.state()
                            != pa_core::session::canonical::CanonicalCallerState::Abandoned,
                        "canonical caller abandoned"
                    );
                }
                let visible_leaf = state.safe_visible_leaf()?;
                anyhow::ensure!(
                    !state.callers.iter().any(|signal| signal.state()
                        == pa_core::session::canonical::CanonicalCallerState::Abandoned),
                    "canonical operation caller abandoned before ACK"
                );
                let opening_run = pending.opening_run.clone();
                let barrier = pending.barrier.clone();
                if let Some(identity) = opening_run {
                    let retired = self.retire_canonical_source(&mut state)?;
                    state.authority = CanonicalOwnerEpoch::successor(&state.raw.cursor, retired)?;
                    let abort = state.authority.register_run(identity.clone())?;
                    state.registered_run = Some(RegisteredRun {
                        identity,
                        abort: Some(abort),
                        ever_provider_admitted: false,
                        terminal_acked: false,
                    });
                    state.provider = None;
                    state.serving_run = None;
                }
                state
                    .revision
                    .clear_barrier(&request.operation, &barrier)
                    .map_err(|e| unresolved(format!("ACK barrier: {e:?}")))?;
                let pending = state
                    .pending
                    .take()
                    .ok_or_else(|| failure("pending receipt disappeared"))?;
                state.adoption.notify_waiters();
                if !pending.prepared {
                    if matches!(
                        state
                            .raw
                            .retained
                            .last()
                            .map(pa_core::session::canonical::CanonicalJournalRow::entry),
                        Some(pa_types::session::FileEntry::Message {
                            message: pa_types::session::AgentMessage::Assistant(_),
                            ..
                        })
                    ) {
                        if let Some(run) = state.registered_run.as_mut() {
                            run.terminal_acked = true;
                        }
                    }
                    let completed_tool =
                        state.raw.retained.last().and_then(|row| match row.entry() {
                            pa_types::session::FileEntry::Message {
                                message: pa_types::session::AgentMessage::ToolResult(result),
                                ..
                            } => Some(result.tool_call_id.clone()),
                            _ => None,
                        });
                    if let Some(id) = completed_tool {
                        state.tools.remove(&id);
                    }
                }
                if let Some(signal) = pending.signal.clone() {
                    state.callers.retain(|prior| {
                        prior.state()
                            != pa_core::session::canonical::CanonicalCallerState::Completed
                    });
                    state.callers.push(signal);
                }
                if pending.prepared
                    && pending.mutation
                    && request.phase == CanonicalAckPhase::Prepared
                {
                    let signal = pending
                        .signal
                        .ok_or_else(|| unresolved("prepared mutation lost caller binding"))?;
                    state.prepared.insert(
                        pending.operation,
                        PreparedBinding {
                            cursor: pending.cursor,
                            signal,
                        },
                    );
                } else {
                    state.prepared.remove(&request.operation);
                }
                Ok(CanonicalLiveAcknowledged {
                    operation: request.operation,
                    cursor: request.cursor,
                    visible_leaf,
                })
            })
            .map_err(unresolved)
    }
}

impl SessionFile {
    pub(crate) fn append_canonical(
        &mut self,
        request: CanonicalLiveAppendRequest,
    ) -> Result<CanonicalCommitDisposition> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        if state
            .prepared
            .get(&request.operation)
            .map(|prepared| &prepared.cursor)
            != Some(&request.expected)
        {
            return Err(failure(
                "canonical append has no matching prepared operation",
            ));
        }
        if matches!(
            request.draft.entry(),
            pa_types::session::FileEntry::Message {
                message: pa_types::session::AgentMessage::Assistant(_)
                    | pa_types::session::AgentMessage::ToolResult(_),
                ..
            }
        ) {
            let actual_run = request
                .run
                .as_ref()
                .ok_or_else(|| failure("native terminal request has no actual run provenance"))?;
            if !state
                .registered_run
                .as_ref()
                .is_some_and(|run| Arc::ptr_eq(&run.identity, actual_run))
            {
                return Err(failure(
                    "native terminal request belongs to another actual run",
                ));
            }
        }
        let mut value = serde_json::to_value(request.draft.entry()).map_err(failure)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| failure("canonical draft is not an object"))?;
        let entry_type = object
            .remove("type")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| failure("canonical draft lacks type"))?;
        for key in ["id", "parentId", "timestamp"] {
            object.remove(key);
        }
        let entry = SessionEntry {
            type_: entry_type,
            id: if self.window.is_some() {
                uuid::Uuid::new_v4().to_string()
            } else {
                super::new_entry_id(&self.by_id)
            },
            parent_id: self.leaf_id.clone(),
            timestamp: crate::util::now_iso(),
            fields: value,
        };
        let mut io_started = false;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                let current = state.current(self)?;
                anyhow::ensure!(
                    current.cursor == state.raw.cursor,
                    "canonical append source changed outside owner; requested row not written"
                );
                let mut metadata_suffix = Vec::new();
                if current.cursor != request.expected {
                    if request.draft.kind()
                        == pa_core::session::canonical::CanonicalMutationKind::AdmittedTerminal
                    {
                        metadata_suffix = state.terminal_metadata_suffix(&request.expected)?;
                    } else {
                        return Err(CanonicalStoreError {
                            kind: CanonicalFailureKind::SourceChanged,
                            requested: CanonicalRequestedWrite::NotWritten,
                            source: CanonicalSourceDisposition::Unchanged,
                            detail:
                                "canonical append source advanced; prepared operation not written"
                                    .into(),
                        }
                        .into());
                    }
                }
                // Validate sparse support before any requested mutation, never after writing.
                current.checkpoint(request.source)?;
                let mut admit = || -> anyhow::Result<_> {
                    // A selected context capability must remain current through the
                    // actual RequestedMutation grant/enter_io and physical admission.
                    let grant = state.grant(self, &request.draft)?;
                    let barrier = state
                        .revision
                        .set_barrier(&request.operation)
                        .map_err(|e| failure(format!("commit barrier: {e:?}")))?;
                    state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                        provenance: PendingProvenance::Retained,
                        operation: request.operation.clone(),
                        cursor: current.cursor.clone(),
                        barrier,
                        prepared: false,
                        mutation: false,
                        signal: None,
                        staged: false,
                        from: current.cursor.clone(),
                        opening_run: None,
                    });
                    let mut attempt = state
                        .revision
                        .admit_write(Some(&request.operation))
                        .map_err(|e| unresolved(format!("commit admission: {e:?}")))?;
                    attempt
                        .begin_io()
                        .map_err(|e| unresolved(format!("commit start: {e:?}")))?;
                    Ok((grant, attempt))
                };
                let (grant, attempt) = match request.context_admission.as_ref() {
                    Some(capability) => capability.with_admission(&request.draft, admit)?,
                    None => admit()?,
                };
                // Registration/host guards are gone. The admitted writer alone owns
                // classification and completion; no plugin lock crosses disk I/O.
                io_started = true;
                let id = match self.persist_granted_entry(entry, None, Some(grant)) {
                    Ok(id) => id,
                    Err(error) => {
                        state.blocked = true;
                        return Err(error);
                    }
                };
                attempt
                    .finish_success()
                    .map_err(|e| unresolved(format!("commit finish: {e:?}")))?;
                let position = state.position(self.leaf_id.clone())?;
                let appended = match RawCapture::append_from(
                    &state.raw,
                    &current.cursor,
                    position,
                    CaptureMode::Full,
                ) {
                    Ok(appended) => appended,
                    Err(error) => {
                        state.blocked = true;
                        return Err(error.into());
                    }
                };
                anyhow::ensure!(
                    appended.rows.len() == 1 && appended.rows[0].entry().id() == Some(id.as_str()),
                    "canonical append does not contain exactly the owner's requested row"
                );
                let to = appended.capture.cursor.clone();
                state
                    .pending
                    .as_mut()
                    .ok_or_else(|| unresolved("commit pending state lost"))?
                    .cursor = to.clone();
                state.raw = appended.capture;
                metadata_suffix.extend(appended.rows);
                Ok(CanonicalCommitDisposition::Committed(
                    CanonicalAppendReceipt {
                        operation: request.operation,
                        from: request.expected,
                        to,
                        rows: metadata_suffix,
                        committed_entry_id: Some(id),
                        replacement_window: None,
                    },
                ))
            })
            .map_err(|error| {
                if io_started {
                    unresolved(error)
                } else {
                    match error.downcast::<CanonicalStoreError>() {
                        Ok(error) => error,
                        Err(error) => failure(error),
                    }
                }
            })
    }

    /// Explicit delivered FIFO replay, or one newly accepted SDK continuation.
    pub(crate) fn admit_canonical_user(
        &mut self,
        request: CanonicalLiveUserRequest,
    ) -> Result<CanonicalLiveUserPrepared> {
        self.attach_canonical_owner()?;
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease.with_owned_mutation(&self.path.clone(), || {
            let current = state.current(self)?;
            anyhow::ensure!(current.cursor == state.raw.cursor, "canonical source changed outside owner");
            if let Some(expected) = request.expected.as_ref().filter(|expected| *expected != &current.cursor) {
                return Err(state.refuse_unstarted(expected, "canonical user admission source CAS conflict").into());
            }
            current.checkpoint(request.source)?;
            if let Some(id) = state.delivered.front() {
                validate_user_intent(&current, id, &request.message)?;
                if !state.admitted.contains(id) {
                    state.approve_admission(&CanonicalAdmissionPolicy::Append(vec![id.clone()]))?;
                }
            }
            state.revision.check_owner_state_advance()
                .map_err(|e| failure(format!("user preflight: {e:?}")))?;
            let operation = CanonicalOperation::new();
            let barrier = state.revision.set_barrier(&operation)
                .map_err(|e| failure(format!("user prepare barrier: {e:?}")))?;
            // Arm pending before delivery/write. Losing the reply keeps this barrier.
            state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                            provenance: PendingProvenance::Retained,
                operation: operation.clone(), cursor: current.cursor.clone(), barrier,
                prepared: true, mutation: false, signal: None, staged: false, from: current.cursor.clone(),
                opening_run: None,
            });
            let id = if let Some(id) = state.delivered.front().cloned() {
                validate_user_intent(&current, &id, &request.message)?;
                state.revision.advance_owner_state(Some(&operation))
                    .map_err(|e| failure(format!("user ledger: {e:?}")))?;
                let position = state.position(self.leaf_id.clone())?;
                state.raw.refresh_owner_position(&position);
                state.delivered.pop_front();
                id
            } else {
                let entry = SessionEntry {
                    type_: "message".to_owned(),
                    id: if self.window.is_some() { uuid::Uuid::new_v4().to_string() } else { super::new_entry_id(&self.by_id) },
                    parent_id: self.leaf_id.clone(),
                    timestamp: crate::util::now_iso(),
                    fields: serde_json::json!({"message": pa_types::session::AgentMessage::User(request.message)}),
                };
                let grant = state.authority.begin_prepare_append(CanonicalIoStage::PreparePreflight)?;
                let mut attempt = state.revision.admit_write(Some(&operation))
                    .map_err(|e| unresolved(format!("user admission: {e:?}")))?;
                attempt.begin_io().map_err(|e| unresolved(format!("user write start: {e:?}")))?;
                let id = match self.persist_granted_entry(entry, None, Some(grant)) {
                    Ok(id) => id,
                    Err(error) => { state.blocked = true; return Err(error); }
                };
                attempt.finish_success().map_err(|e| unresolved(format!("user finish: {e:?}")))?;
                let appended = RawCapture::append_from(&state.raw, &current.cursor,
                    state.position(self.leaf_id.clone())?, CaptureMode::Full)?;
                anyhow::ensure!(appended.rows.len() == 1 && appended.rows[0].entry().id() == Some(id.as_str()),
                    "canonical User write lacks exact single physical suffix");
                state.raw = appended.capture;
                state.registered.insert(id.clone());
                id
            };
            if !state.admitted.contains(&id) {
                state.approve_admission(&CanonicalAdmissionPolicy::Append(vec![id.clone()]))?;
            }
            state.admitted.insert(id.clone());
            let checkpoint = state.raw.checkpoint(request.source)?;
            let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(Arc::clone(&state.adoption))));
            let pending = state.pending.as_mut().ok_or_else(|| unresolved("user pending state lost"))?;
            pending.cursor = checkpoint.cursor.clone();
            pending.signal = Some(signal);
            Ok(CanonicalLiveUserPrepared {
                prepared: CanonicalPrepared { caller, operation, checkpoint },
                admitted_entry_id: id,
            })
        }).map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => unresolved(error),
            })
    }
}

fn metadata_action(entry: &pa_types::session::FileEntry) -> Option<MetadataAction> {
    use pa_types::session::{AgentMessage, FileEntry};
    match entry {
        FileEntry::ModelChange { .. } => Some(MetadataAction::ModelChange),
        FileEntry::ThinkingLevelChange { .. } => Some(MetadataAction::ThinkingLevelChange),
        FileEntry::ServiceTierChange { .. } => Some(MetadataAction::ServiceTierChange),
        FileEntry::SessionInfo { .. } => Some(MetadataAction::SessionInfo),
        FileEntry::SessionState { .. } => Some(MetadataAction::SessionState),
        FileEntry::GitState { .. } => Some(MetadataAction::GitState),
        FileEntry::Label { .. } => Some(MetadataAction::Label),
        FileEntry::Custom { .. } => Some(MetadataAction::CustomEntry),
        FileEntry::CustomMessage { .. } => Some(MetadataAction::CustomMessage),
        FileEntry::ChildUsageAttributed { .. } => Some(MetadataAction::ChildUsageAttributed),
        FileEntry::Message {
            message: AgentMessage::BashExecution(_),
            ..
        } => Some(MetadataAction::BashExecution),
        FileEntry::Message { .. }
        | FileEntry::Header { .. }
        | FileEntry::Unknown { .. }
        | FileEntry::Compaction { .. }
        | FileEntry::BranchSummary { .. } => None,
    }
}

fn metadata_draft(
    entry: &SessionEntry,
) -> Result<pa_core::session::canonical::CanonicalEntryDraft> {
    let mut value = serde_json::to_value(entry).map_err(failure)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| failure("metadata row is not an object"))?;
    for key in ["id", "parentId", "timestamp"] {
        object.remove(key);
    }
    pa_core::session::canonical::CanonicalEntryDraft::new(
        pa_core::session::canonical::CanonicalMutationKind::OwnerMetadata,
        serde_json::from_value(value).map_err(failure)?,
    )
    .map_err(failure)
}

impl State {
    fn grant(
        &self,
        file: &SessionFile,
        draft: &pa_core::session::canonical::CanonicalEntryDraft,
    ) -> Result<CanonicalAppendGrant> {
        use pa_types::session::{AgentMessage, FileEntry};
        let stage = CanonicalIoStage::RequestedMutation;
        let action = metadata_action(draft.entry());
        if let Some(action) = action {
            return self.authority.begin_metadata_append(action, draft, stage);
        }
        if matches!(
            draft.entry(),
            FileEntry::Compaction { .. } | FileEntry::BranchSummary { .. }
        ) {
            return self.authority.begin_context_append(draft, stage);
        }
        if matches!(
            draft.entry(),
            FileEntry::Message {
                message: AgentMessage::Assistant(_),
                ..
            }
        ) {
            let run = self
                .registered_run
                .as_ref()
                .ok_or_else(|| failure("assistant has no registered actual AgentStart run"))?;
            if !run.ever_provider_admitted {
                let abort = run
                    .abort
                    .as_ref()
                    .ok_or_else(|| failure("no-provider abort provenance is unavailable"))?;
                return self.authority.begin_no_provider_terminal(abort, draft, stage)
                    .inspect_err(|_| {
                        eprintln!("pa-daemon: canonical refusal phase=no_provider_terminal reason=terminal_provenance_refused");
                    });
            }
        }
        let epoch = file
            .epoch
            .as_ref()
            .ok_or_else(|| failure("native append has no actual serving capture"))?;
        match draft.entry() {
            FileEntry::Message {
                message: AgentMessage::Assistant(_),
                ..
            } => {
                let step = self
                    .provider
                    .as_ref()
                    .ok_or_else(|| failure("assistant has no admitted provider invocation"))?;
                epoch.0.begin_terminal_append(step, draft, stage)
            }
            FileEntry::Message {
                message: AgentMessage::ToolResult(result),
                ..
            } => {
                let tool = self
                    .tools
                    .get(&result.tool_call_id)
                    .ok_or_else(|| failure("tool result has no admitted execution"))?;
                epoch.0.begin_tool_append(tool, draft, stage)
            }
            FileEntry::Compaction { .. } | FileEntry::BranchSummary { .. } => {
                epoch.0.begin_append(stage, draft)
            }
            FileEntry::Message { .. }
            | FileEntry::Header { .. }
            | FileEntry::ThinkingLevelChange { .. }
            | FileEntry::ServiceTierChange { .. }
            | FileEntry::ModelChange { .. }
            | FileEntry::Custom { .. }
            | FileEntry::ChildUsageAttributed { .. }
            | FileEntry::Label { .. }
            | FileEntry::SessionInfo { .. }
            | FileEntry::SessionState { .. }
            | FileEntry::GitState { .. }
            | FileEntry::CustomMessage { .. }
            | FileEntry::Unknown { .. } => Err(failure("unsupported canonical grant")),
        }
    }
}

impl SessionFile {
    /// Compare immutable scope identity only. Pending receipts remain ACK-able.
    pub(crate) fn has_canonical_owner_scope(
        &self,
        expected: &pa_core::session::canonical::CanonicalOwnerScope,
    ) -> Result<bool> {
        let Some(owner) = &self.canonical_live else {
            return Ok(false);
        };
        let state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        // The original scope is stable even when revision/adoption is blocked.
        Ok(&state.raw.cursor.owner == expected)
    }

    pub(crate) fn is_canonical_bound(&self) -> bool {
        self.canonical_live.is_some()
    }

    /// A new SDK may bootstrap an attached owner only before its first actual
    /// `AgentStart`. Owner membership is not an execution-queue handoff proof.
    pub(crate) fn validate_initial_canonical_sdk(&self) -> Result<()> {
        let Some(owner) = &self.canonical_live else {
            return Ok(());
        };
        let state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.available()?;
        state.verify(self)?;
        if state.registered_run.is_some()
            || state.serving_run.is_some()
            || state.provider.is_some()
            || !state.tools.is_empty()
            || self.epoch.is_some()
        {
            return Err(failure(
                "fresh SDK under an owner with actual execution has no queue handoff",
            ));
        }
        if !state.admitted.is_empty() {
            return Err(failure(
                "initial SDK bootstrap already has owner admission debt",
            ));
        }
        Ok(())
    }

    /// Called at the real main stream invocation, before provider I/O.
    pub(crate) fn begin_canonical_provider_step(
        &mut self,
        request: CanonicalLiveProviderStepRequest,
        cancelled: bool,
    ) -> Result<()> {
        self.attach_canonical_owner()?;
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "canonical provider source changed"
                );
                if cancelled {
                    let mut error = state.refuse_unstarted(&request.expected, "new canonical provider work is closing or aborted");
                    if error.source == CanonicalSourceDisposition::ReadyForObservation {
                        error.kind = CanonicalFailureKind::Cancelled;
                    }
                    return Err(error.into());
                }
                if request.expected != state.raw.cursor {
                    return Err(state.refuse_unstarted(&request.expected, "actual provider invocation projected another canonical source; refusing without rebase").into());
                }
                anyhow::ensure!(
                    state.tools.is_empty(),
                    "previous tool executions have no terminal adoption"
                );
                let registered = state.registered_run.as_mut().ok_or_else(|| failure("provider invocation has no actual registered run"))?;
                anyhow::ensure!(Arc::ptr_eq(&registered.identity, &request.run), "provider invocation belongs to another actual run");
                // Permanent for this actual AgentStart run, including all source
                // successors and failed/retired provider captures. Never fall back.
                registered.ever_provider_admitted = true;
                let retired = self.retire_canonical_source(&mut state)?;
                state.authority = CanonicalOwnerEpoch::successor(&state.raw.cursor, retired)?;
                let epoch = state
                    .authority
                    .capture_serving_run(request.run.clone(), request.model)?;
                state.provider = Some(epoch.admit_provider_step(request.invocation)?);
                state.serving_run = Some(request.run);
                self.epoch = Some(super::canonical_adapter::EpochHandle(Arc::new(epoch)));
                Ok(())
            })
            .map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => unresolved(error),
            })
    }

    /// Called by the awaited `ToolExecutionStart` boundary, before tool execution.
    pub(crate) fn admit_canonical_tool(&mut self, request: CanonicalLiveToolRequest) -> Result<()> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path, || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "canonical tool source changed"
                );
                anyhow::ensure!(
                    state
                        .serving_run
                        .as_ref()
                        .is_some_and(|run| Arc::ptr_eq(run, &request.run)),
                    "tool execution belongs to another actual run"
                );
                let id = request.tool.tool_call_id.clone();
                anyhow::ensure!(
                    !state.tools.contains_key(&id),
                    "tool execution already admitted"
                );
                // Check the actual start record against the certified assistant. The
                // authority then pins the same execution allocation through its result.
                let assistant = state
                    .raw
                    .retained
                    .iter()
                    .rev()
                    .find_map(|row| match row.entry() {
                        pa_types::session::FileEntry::Message {
                            message: pa_types::session::AgentMessage::Assistant(message),
                            ..
                        } => Some(message),
                        _ => None,
                    })
                    .ok_or_else(|| failure("tool start has no canonical assistant"))?;
                let calls: Vec<_> = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        pa_types::ai::AssistantContentBlock::ToolCall(call) if call.id == id => {
                            Some(call)
                        }
                        _ => None,
                    })
                    .collect();
                anyhow::ensure!(
                    calls.len() == 1
                        && calls[0].name == request.tool.tool_name
                        && request.tool.args.as_object() == Some(&calls[0].arguments),
                    "actual tool start differs from exact canonical assistant call"
                );
                let step = state
                    .provider
                    .as_ref()
                    .ok_or_else(|| failure("tool execution lacks actual provider step"))?;
                let tool = step.admit_tool(&id, request.tool)?;
                state.tools.insert(id, tool);
                Ok(())
            })
            .map_err(failure)
    }
}

fn validate_user_intent(
    raw: &RawCapture,
    id: &str,
    intended: &pa_types::ai::UserMessage,
) -> Result<()> {
    let row = raw
        .retained
        .iter()
        .find(|row| row.entry().id() == Some(id))
        .ok_or_else(|| failure("delivered FIFO row missing from physical source"))?;
    let pa_types::session::FileEntry::Message {
        message: pa_types::session::AgentMessage::User(actual),
        ..
    } = row.entry()
    else {
        return Err(failure("delivered FIFO row is not User"));
    };
    let mut actual = serde_json::to_value(actual).map_err(failure)?;
    let mut intended = serde_json::to_value(intended).map_err(failure)?;
    actual
        .as_object_mut()
        .ok_or_else(|| failure("User payload is not object"))?
        .remove("timestamp");
    intended
        .as_object_mut()
        .ok_or_else(|| failure("User intent is not object"))?
        .remove("timestamp");
    if actual != intended {
        return Err(failure(
            "delivered User FIFO head differs from accepted intent",
        ));
    }
    Ok(())
}

impl SessionFile {
    /// Durable acceptance before SDK construction or early abort. Bound acceptance
    /// leaves an operation-bound staged checkpoint; it never clears its own barrier.
    pub(crate) fn persist_delivered_user(&mut self, message: serde_json::Value) -> Result<String> {
        if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
            return Err(failure("accepted user payload has another role"));
        }
        let mut fields = serde_json::Map::new();
        fields.insert("message".to_owned(), message);
        let fields = serde_json::Value::Object(fields);
        if !self.is_canonical_bound() {
            let id = self.persist_entry("message", fields).map_err(failure)?;
            self.register_delivered_user(&id)?;
            return Ok(id);
        }
        self.stage_delivered_entry("message", fields)
    }

    pub(crate) fn persist_delivered_custom(&mut self, fields: serde_json::Value) -> Result<String> {
        if !self.is_canonical_bound() {
            let id = self
                .persist_entry("custom_message", fields)
                .map_err(failure)?;
            self.canonical_delivered_custom.push_back(id.clone());
            return Ok(id);
        }
        self.stage_delivered_entry("custom_message", fields)
    }

    fn stage_delivered_entry(
        &mut self,
        entry_type: &str,
        fields: serde_json::Value,
    ) -> Result<String> {
        let entry = SessionEntry {
            type_: entry_type.to_owned(),
            id: if self.window.is_some() {
                uuid::Uuid::new_v4().to_string()
            } else {
                super::new_entry_id(&self.by_id)
            },
            parent_id: self.leaf_id.clone(),
            timestamp: crate::util::now_iso(),
            fields,
        };
        let delivery = match entry_type {
            "message" => OwnerDelivery::User,
            "custom_message" => OwnerDelivery::Custom,
            _ => return Err(failure("unsupported delivered row kind")),
        };
        self.stage_canonical_entry(entry, delivery)
    }

    pub(super) fn stage_owned_entry(&mut self, entry: SessionEntry) -> Result<String> {
        self.stage_canonical_entry(entry, OwnerDelivery::Metadata)
    }

    fn stage_canonical_entry(
        &mut self,
        entry: SessionEntry,
        delivery: OwnerDelivery,
    ) -> Result<String> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                let current = state.current(self)?;
                anyhow::ensure!(
                    current.cursor == state.raw.cursor,
                    "accepted delivery source changed outside owner"
                );
                anyhow::ensure!(
                    !state
                        .raw
                        .plan
                        .rows
                        .iter()
                        .any(|row| row.id.as_deref() == Some(entry.id.as_str())),
                    "canonical stable row identity already exists"
                );
                state
                    .revision
                    .check_owner_state_advance()
                    .map_err(|e| failure(format!("stage preflight: {e:?}")))?;
                match delivery {
                    OwnerDelivery::User => anyhow::ensure!(
                        super::canonical_adapter::is_user_row(&entry),
                        "staged User has another kind"
                    ),
                    OwnerDelivery::Custom => anyhow::ensure!(
                        entry.type_ == "custom_message",
                        "staged Custom has another kind"
                    ),
                    OwnerDelivery::Metadata => {}
                }
                let grant = if matches!(delivery, OwnerDelivery::User) {
                    state
                        .authority
                        .begin_prepare_append(CanonicalIoStage::PreparePreflight)?
                } else {
                    let draft = metadata_draft(&entry)?;
                    metadata_action(draft.entry())
                        .ok_or_else(|| failure("unsupported explicit owner staging action"))?;
                    state.grant(self, &draft)?
                };
                let operation = CanonicalOperation::new();
                let barrier = state
                    .revision
                    .set_barrier(&operation)
                    .map_err(|e| failure(format!("delivery barrier: {e:?}")))?;
                state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                    provenance: PendingProvenance::Retained,
                    operation: operation.clone(),
                    cursor: current.cursor.clone(),
                    barrier,
                    prepared: true,
                    mutation: false,
                    signal: None,
                    staged: true,
                    from: current.cursor.clone(),
                    opening_run: None,
                });
                let mut attempt = state
                    .revision
                    .admit_write(Some(&operation))
                    .map_err(|e| unresolved(format!("delivery admission: {e:?}")))?;
                attempt
                    .begin_io()
                    .map_err(|e| unresolved(format!("delivery write start: {e:?}")))?;
                let id = match self.persist_granted_entry(entry, None, Some(grant)) {
                    Ok(id) => id,
                    Err(error) => {
                        state.blocked = true;
                        return Err(error);
                    }
                };
                attempt
                    .finish_success()
                    .map_err(|e| unresolved(format!("delivery finish: {e:?}")))?;
                // Keep successful-write debt intact when suffix capture fails.
                #[cfg(test)]
                if std::mem::take(&mut state.fail_stage_capture) {
                    return Err(unresolved("injected stage suffix capture failure").into());
                }
                let appended = RawCapture::append_from(
                    &state.raw,
                    &current.cursor,
                    state.position(self.leaf_id.clone())?,
                    CaptureMode::Full,
                )?;
                anyhow::ensure!(
                    appended.rows.len() == 1 && appended.rows[0].entry().id() == Some(id.as_str()),
                    "accepted delivery lacks exact physical suffix"
                );
                state.raw = appended.capture;
                let cursor = state.raw.cursor.clone();
                state
                    .pending
                    .as_mut()
                    .ok_or_else(|| unresolved("staged delivery lost"))?
                    .cursor = cursor;
                match delivery {
                    OwnerDelivery::User => {
                        state.registered.insert(id.clone());
                        state.delivered.push_back(id.clone());
                    }
                    OwnerDelivery::Custom => state.delivered_custom.push_back(id.clone()),
                    OwnerDelivery::Metadata => {}
                }
                let pending = state
                    .pending
                    .as_ref()
                    .ok_or_else(|| unresolved("staged delivery lost"))?;
                state
                    .revision
                    .clear_barrier(&operation, &pending.barrier)
                    .map_err(|e| unresolved(format!("delivery completion barrier: {e:?}")))?;
                state.pending.take();
                state.adoption.notify_waiters();
                Ok(id)
            })
            .map_err(unresolved)
    }
}

impl SessionFile {
    pub(crate) fn admit_canonical_custom(
        &mut self,
        request: CanonicalLiveCustomRequest,
    ) -> Result<CanonicalLiveCustomPrepared> {
        self.attach_canonical_owner()?;
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        let payload = pa_types::session::CustomMessageEntry {
            custom_type: request.message.custom_type,
            content: request.message.content,
            display: request.message.display,
            details: request.message.details,
            rest: request.message.rest,
        };
        lease.with_owned_mutation(&self.path.clone(), || {
            let current = state.current(self)?;
            anyhow::ensure!(current.cursor == state.raw.cursor, "canonical Custom source changed outside owner");
            if let Some(expected) = request.expected.as_ref().filter(|expected| *expected != &current.cursor) {
                return Err(state.refuse_unstarted(expected, "canonical Custom admission source CAS conflict").into());
            }
            current.checkpoint(request.source)?;
            let existing = state.delivered_custom.front().cloned();
            if let Some(id) = &existing {
                let row = current.retained.iter().find(|row| row.entry().id() == Some(id.as_str()))
                    .ok_or_else(|| failure("delivered Custom FIFO row missing"))?;
                if !matches!(row.entry(), pa_types::session::FileEntry::CustomMessage { payload: actual, .. } if actual == &payload) {
                    // Nothing of this request exists yet: the FIFO head is an
                    // already-durable row another producer staged first. Refuse the
                    // ordering, never the session.
                    return Err(CanonicalStoreError {
                        kind: CanonicalFailureKind::SourceChanged,
                        requested: CanonicalRequestedWrite::NotRequested,
                        source: CanonicalSourceDisposition::Unchanged,
                        detail: "delivered Custom FIFO head differs from exact accepted intent"
                            .to_owned(),
                    }
                    .into());
                }
            }
            state.revision.check_owner_state_advance()
                .map_err(|e| failure(format!("Custom preflight: {e:?}")))?;
            let operation = CanonicalOperation::new();
            let barrier = state.revision.set_barrier(&operation)
                .map_err(|e| failure(format!("Custom delivery barrier: {e:?}")))?;
            state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                provenance: PendingProvenance::Retained,
                operation: operation.clone(), cursor: current.cursor.clone(), barrier,
                prepared: true, mutation: false, signal: None, staged: false, from: current.cursor.clone(),
                opening_run: None,
            });
            let id = if let Some(id) = existing {
                state.revision.advance_owner_state(Some(&operation))
                    .map_err(|e| failure(format!("Custom ledger: {e:?}")))?;
                let position = state.position(self.leaf_id.clone())?;
                state.raw.refresh_owner_position(&position);
                state.delivered_custom.pop_front();
                id
            } else {
                let entry = SessionEntry {
                    type_: "custom_message".to_owned(),
                    id: if self.window.is_some() { uuid::Uuid::new_v4().to_string() } else { super::new_entry_id(&self.by_id) },
                    parent_id: self.leaf_id.clone(), timestamp: crate::util::now_iso(),
                    fields: serde_json::to_value(&payload).map_err(failure)?,
                };
                let draft = metadata_draft(&entry)?;
                let grant = state.grant(self, &draft)?;
                let mut attempt = state.revision.admit_write(Some(&operation))
                    .map_err(|e| failure(format!("Custom write admission: {e:?}")))?;
                attempt.begin_io().map_err(|e| unresolved(format!("Custom write start: {e:?}")))?;
                let id = match self.persist_granted_entry(entry, None, Some(grant)) {
                    Ok(id) => id,
                    Err(error) => { state.blocked = true; return Err(error); }
                };
                attempt.finish_success().map_err(|e| unresolved(format!("Custom write finish: {e:?}")))?;
                let appended = RawCapture::append_from(&state.raw, &current.cursor,
                    state.position(self.leaf_id.clone())?, CaptureMode::Full)?;
                anyhow::ensure!(appended.rows.len() == 1 && appended.rows[0].entry().id() == Some(id.as_str()),
                    "canonical Custom write lacks exact single physical suffix");
                state.raw = appended.capture;
                id
            };
            let checkpoint = state.raw.checkpoint(request.source)?;
            let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(Arc::clone(&state.adoption))));
            let pending = state.pending.as_mut().ok_or_else(|| unresolved("Custom pending state lost"))?;
            pending.cursor = checkpoint.cursor.clone();
            pending.signal = Some(signal);
            Ok(CanonicalLiveCustomPrepared {
                prepared: CanonicalPrepared { caller, operation, checkpoint }, admitted_entry_id: id,
            })
        }).map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => unresolved(error),
            })
    }
}

impl SessionFile {
    /// Actual worker command/lifecycle admission, never a generic canonical write.
    /// The exact typed row must match one of the named `MetadataAction` variants.
    pub(crate) fn persist_owner_metadata(
        &mut self,
        kind: &str,
        fields: serde_json::Value,
    ) -> Result<String> {
        if !self.is_canonical_bound() {
            return self.persist_entry(kind, fields).map_err(failure);
        }
        let entry = SessionEntry {
            type_: kind.to_owned(),
            id: if self.window.is_some() {
                uuid::Uuid::new_v4().to_string()
            } else {
                super::new_entry_id(&self.by_id)
            },
            parent_id: self.leaf_id.clone(),
            timestamp: crate::util::now_iso(),
            fields,
        };
        let draft = metadata_draft(&entry)?;
        if metadata_action(draft.entry()).is_none() {
            return Err(failure(
                "worker lifecycle row has no exact named MetadataAction",
            ));
        }
        self.stage_owned_entry(entry)
    }
}

impl SessionFile {
    /// Uncertain/invalid source, distinct from a known durable staged acceptance.
    /// UI may show known durable rows while Core still owes their exact ACK.
    pub(crate) fn canonical_source_uncertain(&self) -> bool {
        let Some(owner) = &self.canonical_live else {
            return super::canonical_adapter::epoch_is_poisoned(self.epoch.as_ref());
        };
        let Ok(state) = owner.0.lock() else {
            return true;
        };
        let Ok(revision) = state.revision.snapshot() else {
            return true;
        };
        let blocked = state.blocked;
        let authority_poison_observed = !blocked && state.authority.poisoned();
        if blocked
            || authority_poison_observed
            || revision.canonical_revision != state.raw.cursor.canonical_revision
            || revision.branch_revision != state.raw.cursor.branch_revision
            || state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.signal.is_none())
            || state.callers.iter().any(|signal| {
                signal.state() == pa_core::session::canonical::CanonicalCallerState::Abandoned
            })
            || state
                .pending
                .as_ref()
                .and_then(|pending| pending.signal.as_ref())
                .is_some_and(|signal| {
                    signal.state() == pa_core::session::canonical::CanonicalCallerState::Abandoned
                })
        {
            return true;
        }
        let Ok(lease) = state.verify(self) else {
            return true;
        };
        lease.with_owned_mutation(&self.path, || Ok(())).is_err()
    }

    /// Whether another caller currently holds this owner's adoption window.
    pub(crate) fn canonical_adoption_pending(&self) -> bool {
        self.canonical_adoption_window()
            .is_some_and(|(_, pending)| pending)
    }

    /// The owner's adoption window: its change signal and whether another
    /// caller currently holds it. `None` when no owner is attached.
    pub(crate) fn canonical_adoption_window(&self) -> Option<(Arc<tokio::sync::Notify>, bool)> {
        let owner = self.canonical_live.as_ref()?;
        let state = owner.0.lock().ok()?;
        Some((
            Arc::clone(&state.adoption),
            state.pending.is_some() && !state.blocked,
        ))
    }

    /// Reconcile a quarantined SDK view back to a servable owner.
    ///
    /// A quarantined view refuses every event, so nothing it still owes can ever
    /// be completed: its pending adoption, prepared ticket, admitted tools and
    /// un-ACKed run are dead debt, and requiring them to settle is exactly the
    /// permanent refusal this path exists to repair. The reconcile trusts the
    /// durable journal instead: a full capture must still equal the owner's last
    /// certified source, which proves the rows are complete and that no foreign
    /// writer advanced them. Child-usage I/O lives outside the journal and must
    /// be resolved. The successor epoch then re-reads those rows; history is
    /// never rewritten and no unknown physical owner is reused.
    pub(crate) fn retire_quarantined_canonical_owner(&mut self) -> Result<()> {
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        // A poisoned authority or revision means this process cannot certify what
        // it wrote. Only a new physical owner may recover that, never a reopen.
        if state.authority.poisoned() {
            return Err(unresolved("canonical owner authority poisoned"));
        }
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "quarantined canonical source changed outside owner"
                );
                anyhow::ensure!(
                    self.child_usage_recovery.pending.is_none(),
                    "child usage I/O remains unresolved"
                );
                // This view refuses every later event, so an admitted tool can
                // never receive its terminal row from it. The durable rows are
                // complete; drop the dead admissions before the ordered close.
                state.tools.clear();
                self.retire_owned_epoch(&mut state)
                    .map_err(anyhow::Error::from)
            })
            .map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => unresolved(error),
            })
    }
}

impl SessionFile {
    /// Actual awaited `AgentStart`, after raw adoption/ACK and before any paid work.
    pub(crate) fn register_canonical_run(
        &mut self,
        request: CanonicalLiveRunRequest,
    ) -> Result<CanonicalPrepared> {
        self.attach_canonical_owner()?;
        let owner = self
            .canonical_live
            .clone()
            .ok_or_else(|| failure("canonical owner not attached"))?;
        let mut state = owner
            .0
            .lock()
            .map_err(|_| unresolved("canonical owner mutex poisoned"))?;
        state.expire_unwritten_adoption()?;
        state.available()?;
        let lease = state.verify(self)?;
        lease
            .with_owned_mutation(&self.path.clone(), || {
                anyhow::ensure!(
                    state.current(self)?.cursor == state.raw.cursor,
                    "actual AgentStart source changed outside owner"
                );
                // This check admits only an exact source or a monotone, same-owner
                // advance with no outstanding adoption, mutation, or tool debt.
                anyhow::ensure!(
                    state.certifies_unstarted_source(&request.expected)
                        && state.raw.certifies_prior_prefix(&request.expected),
                    "actual AgentStart belongs to another certified source"
                );
                if let Some(previous) = &state.registered_run {
                    anyhow::ensure!(
                        !Arc::ptr_eq(&previous.identity, &request.run),
                        "actual run was already registered"
                    );
                    anyhow::ensure!(
                        previous.terminal_acked,
                        "previous actual run has no acknowledged native terminal row"
                    );
                }
                let checkpoint = state
                    .raw
                    .checkpoint(pa_core::session::canonical::CanonicalSourceRequest::Full)?;
                let operation = CanonicalOperation::new();
                let barrier = state
                    .revision
                    .set_barrier(&operation)
                    .map_err(|e| unresolved(format!("run opening barrier: {e:?}")))?;
                let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(Arc::clone(&state.adoption))));
                state.pending = Some(Pending {
                    armed: std::time::Instant::now(),
                    provenance: PendingProvenance::Retained,
                    operation: operation.clone(),
                    cursor: checkpoint.cursor.clone(),
                    barrier,
                    prepared: true,
                    mutation: false,
                    signal: Some(signal),
                    staged: false,
                    from: checkpoint.cursor.clone(),
                    opening_run: Some(request.run),
                });
                Ok(CanonicalPrepared {
                    caller,
                    operation,
                    checkpoint,
                })
            })
            .map_err(|error| match error.downcast::<CanonicalStoreError>() {
                Ok(error) => error,
                Err(error) => unresolved(error),
            })
    }
}

#[cfg(test)]
impl SessionFile {
    pub(super) fn fail_next_stage_capture(&mut self) {
        self.canonical_live
            .as_ref()
            .unwrap()
            .0
            .lock()
            .unwrap()
            .fail_stage_capture = true;
    }
}
