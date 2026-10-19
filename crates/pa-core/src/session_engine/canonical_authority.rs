//! Owned canonical authority, independent of SDK locks and runtime wiring.
//!
//! Only the Core coordinator may supply actual serving allocations and native
//! final outcomes. These primitives cannot prove that a runtime `SessionLease` was
//! captured. One short entry lock orders admission/close; no callbacks or waits
//! occur under it. Backend writers must carry the flight AND its result recorder.
// This module is intentionally not wired yet. Remove this allowance at integration.
#![allow(dead_code)]

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pa_types::ai::AssistantContentBlock;
use pa_types::session::{AgentMessage, FileEntry};
use tokio::sync::Notify;

use crate::session::canonical::{
    CanonicalCursor, CanonicalEntryDraft, CanonicalFailureKind, CanonicalIoFlight,
    CanonicalIoStage, CanonicalMutationKind, CanonicalOperation, CanonicalPrepareMode,
    CanonicalRequestedWrite, CanonicalSourceDisposition, CanonicalStoreError,
    CanonicalWriteAuthority, CanonicalWritePermit,
};

type Result<T> = std::result::Result<T, CanonicalStoreError>;
type Allocation = Arc<dyn Any + Send + Sync>;

fn refused(kind: CanonicalFailureKind, detail: &str) -> CanonicalStoreError {
    CanonicalStoreError {
        kind,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::Unchanged,
        detail: detail.to_owned(),
    }
}
fn closed() -> CanonicalStoreError {
    refused(
        CanonicalFailureKind::AuthorityClosed,
        "captured canonical authority is closed",
    )
}
fn invalid(detail: &str) -> CanonicalStoreError {
    refused(CanonicalFailureKind::InvalidRequest, detail)
}

/// Strong pin to an actual selected contribution registration and generation.
/// Names cannot mint an identity. Retirement is monotonic under the owner gate.
#[derive(Clone)]
pub(super) struct ContributionSelection {
    registration: Allocation,
    generation: u64,
    retired: Arc<AtomicBool>,
}
impl ContributionSelection {
    pub(super) fn capture<T: Any + Send + Sync>(registration: Arc<T>, generation: u64) -> Self {
        Self {
            registration,
            generation,
            retired: Arc::new(AtomicBool::new(false)),
        }
    }
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registration, &other.registration) && self.generation == other.generation
    }
}

struct ServingIdentity {
    actual_run: Allocation,
    actual_model: Allocation,
    contributions: Vec<ContributionSelection>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CloseReason {
    OrdinaryAbort,
    Retirement,
    Navigation,
    SourceChanged,
    ModelChange,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Closing(CloseReason),
    Closed,
}

struct Entry {
    source: CanonicalCursor,
    serving: Option<Arc<ServingIdentity>>,
    gate: Gate,
    io: usize,
    transactions: usize,
    anchors: usize,
    admitted_steps: Vec<Allocation>,
    poisoned: bool,
    projection_quarantined: bool,
}
struct Shared {
    entry: Mutex<Entry>,
    changed: Notify,
}
impl Shared {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Entry>> {
        self.entry.lock().map_err(|_| closed())
    }
    fn quarantine(&self) {
        // Poison is never interpreted as drained or ready, including in Drop.
        if let Ok(mut entry) = self.entry.lock() {
            entry.poisoned = true;
            entry.projection_quarantined = true;
            if entry.gate == Gate::Open {
                entry.gate = Gate::Closing(CloseReason::SourceChanged);
            }
        }
        self.changed.notify_waiters();
    }
}

/// One actual owner/source epoch. No successor can reopen this entry.
pub(super) struct AuthorityOwner {
    shared: Arc<Shared>,
}
impl AuthorityOwner {
    pub(super) fn new(actual_source: CanonicalCursor) -> Self {
        Self {
            shared: Arc::new(Shared {
                entry: Mutex::new(Entry {
                    source: actual_source,
                    serving: None,
                    gate: Gate::Open,
                    io: 0,
                    transactions: 0,
                    anchors: 0,
                    admitted_steps: Vec::new(),
                    poisoned: false,
                    projection_quarantined: false,
                }),
                changed: Notify::new(),
            }),
        }
    }

    /// Called at ACTUAL serving-model capture, not at desired-next selection.
    /// Both allocations remain strongly pinned through every captured scope.
    pub(super) fn capture_serving_run<R: Any + Send + Sync, M: Any + Send + Sync>(
        &self,
        actual_source: &CanonicalCursor,
        actual_run: Arc<R>,
        actual_model: Arc<M>,
        contributions: Vec<ContributionSelection>,
    ) -> Result<(CapturedRun, LateRunAnchor)> {
        let mut entry = self.shared.lock()?;
        validate_source(&entry.source, actual_source)?;
        if entry.gate != Gate::Open
            || entry.poisoned
            || entry.serving.is_some()
            || contributions
                .iter()
                .any(|c| c.retired.load(Ordering::Acquire))
        {
            return Err(closed());
        }
        let serving = Arc::new(ServingIdentity {
            actual_run,
            actual_model,
            contributions,
        });
        entry.serving = Some(serving.clone());
        entry.anchors += 1;
        let life = Arc::new(AtomicBool::new(true));
        Ok((
            CapturedRun {
                shared: self.shared.clone(),
                source: actual_source.clone(),
                serving,
            },
            LateRunAnchor {
                _serving: entry.serving.as_ref().expect("registered serving").clone(),
                _source: actual_source.clone(),
                shared: Arc::downgrade(&self.shared),
                life,
                boundary: false,
            },
        ))
    }

    /// Nonblocking Closing transition. Waiting and activation are separate.
    pub(super) fn request_close(&self, reason: CloseReason) -> CloseTicket {
        stage_close(&self.shared, reason, None)
    }

    pub(super) fn retire_contribution(
        &self,
        selection: &ContributionSelection,
    ) -> Result<CloseTicket> {
        let mut entry = self.shared.lock()?;
        let serving = entry.serving.as_ref().ok_or_else(closed)?;
        let Some(actual) = serving.contributions.iter().find(|c| c.same(selection)) else {
            return Err(invalid("contribution is not in this serving capture"));
        };
        actual.retired.store(true, Ordering::Release);
        selection.retired.store(true, Ordering::Release);
        if entry.gate == Gate::Open || entry.gate == Gate::Closing(CloseReason::OrdinaryAbort) {
            entry.gate = Gate::Closing(CloseReason::Retirement);
        }
        entry.projection_quarantined = true;
        drop(entry);
        self.shared.changed.notify_waiters();
        Ok(CloseTicket {
            shared: self.shared.clone(),
            self_hold: None,
        })
    }

    /// Uncertain writes require NEW-owner reconciliation. Cleanup cannot clear it.
    pub(super) fn quarantine(&self) {
        self.shared.quarantine();
    }
    pub(super) fn projection_allowed(&self) -> bool {
        self.shared
            .lock()
            .is_ok_and(|e| e.gate == Gate::Open && !e.poisoned && !e.projection_quarantined)
    }
}

impl Drop for AuthorityOwner {
    fn drop(&mut self) {
        // The coordinator's actual owner lifetime is distinct from capture Arcs.
        // Retaining a capture must never keep a retired owner open.
        stage_close(&self.shared, CloseReason::Retirement, None);
    }
}

fn validate_source(captured: &CanonicalCursor, actual: &CanonicalCursor) -> Result<()> {
    if captured.owner != actual.owner {
        return Err(refused(
            CanonicalFailureKind::OwnerChanged,
            "owner allocation changed",
        ));
    }
    if captured.session_id != actual.session_id
        || captured.source_path != actual.source_path
        || captured.header_certificate != actual.header_certificate
    {
        return Err(refused(
            CanonicalFailureKind::SourceChanged,
            "header/path/source identity changed",
        ));
    }
    if captured.branch_revision != actual.branch_revision {
        return Err(refused(
            CanonicalFailureKind::BranchChanged,
            "branch epoch changed",
        ));
    }
    // NOT a strict post-preflight CAS: head, frontier, raw generation and leaf
    // may advance in our OWN preflight. The store must do its separate strict CAS.
    Ok(())
}
fn validate_capture(entry: &Entry, capture: &CapturedRun, late_terminal: bool) -> Result<()> {
    validate_source(&capture.source, &entry.source)?;
    let Some(serving) = &entry.serving else {
        return Err(closed());
    };
    if entry.anchors == 0
        || !Arc::ptr_eq(serving, &capture.serving)
        || !Arc::ptr_eq(&serving.actual_run, &capture.serving.actual_run)
        || !Arc::ptr_eq(&serving.actual_model, &capture.serving.actual_model)
        || serving.contributions.len() != capture.serving.contributions.len()
        || serving
            .contributions
            .iter()
            .zip(&capture.serving.contributions)
            .any(|(a, b)| {
                !a.same(b) || a.retired.load(Ordering::Acquire) || b.retired.load(Ordering::Acquire)
            })
        || entry.poisoned
    {
        return Err(closed());
    }
    match entry.gate {
        Gate::Open => Ok(()),
        Gate::Closing(CloseReason::OrdinaryAbort) if late_terminal && entry.anchors > 0 => Ok(()),
        Gate::Closing(_) | Gate::Closed => Err(closed()),
    }
}

/// Immutable exact source/run/model/contribution capture. No caller lifetime.
#[derive(Clone)]
pub(super) struct CapturedRun {
    shared: Arc<Shared>,
    source: CanonicalCursor,
    serving: Arc<ServingIdentity>,
}
impl CapturedRun {
    pub(super) fn source(&self) -> &CanonicalCursor {
        &self.source
    }
    pub(super) fn same_serving(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared) && Arc::ptr_eq(&self.serving, &other.serving)
    }
    pub(super) fn observe(&self) -> CapturedScope {
        self.scope(Purpose::Observe)
    }
    pub(super) fn context_policy(&self) -> CapturedScope {
        self.scope(Purpose::ContextPolicy)
    }
    pub(super) fn owner_metadata(
        &self,
        action: MetadataAction,
        draft: CanonicalEntryDraft,
    ) -> Result<CapturedScope> {
        if draft.kind() != CanonicalMutationKind::OwnerMetadata || !action.matches(draft.entry()) {
            return Err(invalid(
                "metadata requires a specific admitted lifecycle action and exact payload",
            ));
        }
        let entry = self.shared.lock()?;
        validate_capture(&entry, self, false)?;
        Ok(self.scope(Purpose::Metadata {
            action,
            exact: Arc::new(draft),
        }))
    }
    fn scope(&self, purpose: Purpose) -> CapturedScope {
        CapturedScope(Arc::new(ScopeSpec {
            run: self.clone(),
            purpose,
        }))
    }
    /// The coordinator invokes this only at actual provider/main-step admission.
    pub(super) fn admit_provider_step<S: Any + Send + Sync>(
        &self,
        actual_step: Arc<S>,
    ) -> Result<ProviderStep> {
        let mut entry = self.shared.lock()?;
        validate_capture(&entry, self, false)?;
        let actual_step: Allocation = actual_step;
        if entry
            .admitted_steps
            .iter()
            .any(|step| Arc::ptr_eq(step, &actual_step))
        {
            return Err(invalid("provider step allocation already admitted"));
        }
        entry.admitted_steps.push(actual_step.clone());
        Ok(ProviderStep {
            run: self.clone(),
            actual_step,
            state: Mutex::new(StepState {
                assistant: None,
                tools: Vec::new(),
            }),
        })
    }
}

/// Exact supported metadata actions. There is no generic Custom/CustomMessage fallback.
#[derive(Clone, Copy)]
pub(super) enum MetadataAction {
    ModelChange,
    ThinkingLevelChange,
    ServiceTierChange,
    SessionInfo,
    SessionState,
    GitState,
    Label,
}
impl MetadataAction {
    fn matches(self, entry: &FileEntry) -> bool {
        matches!(
            (self, entry),
            (Self::ModelChange, FileEntry::ModelChange { .. })
                | (
                    Self::ThinkingLevelChange,
                    FileEntry::ThinkingLevelChange { .. }
                )
                | (Self::ServiceTierChange, FileEntry::ServiceTierChange { .. })
                | (Self::SessionInfo, FileEntry::SessionInfo { .. })
                | (Self::SessionState, FileEntry::SessionState { .. })
                | (Self::GitState, FileEntry::GitState { .. })
                | (Self::Label, FileEntry::Label { .. })
        )
    }
}
struct TerminalSeal {
    draft: CanonicalEntryDraft,
    consumed: AtomicBool,
}
enum Purpose {
    Observe,
    ContextPolicy,
    Metadata {
        action: MetadataAction,
        exact: Arc<CanonicalEntryDraft>,
    },
    Terminal {
        seal: Arc<TerminalSeal>,
        _step: Allocation,
        _tool: Option<Allocation>,
    },
}
impl Purpose {
    fn kind(&self) -> CanonicalMutationKind {
        match self {
            Self::Observe | Self::ContextPolicy => CanonicalMutationKind::ContextPolicy,
            Self::Metadata { .. } => CanonicalMutationKind::OwnerMetadata,
            Self::Terminal { .. } => CanonicalMutationKind::AdmittedTerminal,
        }
    }
    fn mode(&self) -> CanonicalPrepareMode {
        match self {
            Self::Observe => CanonicalPrepareMode::Observe,
            _ => CanonicalPrepareMode::Mutation(self.kind()),
        }
    }
    fn late(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
    fn validate_draft(&self, draft: &CanonicalEntryDraft) -> Result<()> {
        if draft.kind() != self.kind() {
            return Err(invalid("authority kind differs from draft"));
        }
        match self {
            Self::Observe => Err(invalid("observe cannot mutate")),
            Self::ContextPolicy => {
                if matches!(
                    draft.entry(),
                    FileEntry::Compaction { .. } | FileEntry::BranchSummary { .. }
                ) {
                    Ok(())
                } else {
                    Err(invalid(
                        "context authority cannot write metadata or native terminal rows",
                    ))
                }
            }
            Self::Metadata { action, exact } => {
                if action.matches(draft.entry()) && exact.entry() == draft.entry() {
                    Ok(())
                } else {
                    Err(invalid(
                        "metadata action or exact lifecycle payload differs",
                    ))
                }
            }
            Self::Terminal { seal, .. } => {
                if seal.draft.entry() == draft.entry() {
                    Ok(())
                } else {
                    Err(invalid(
                        "native terminal outcome differs from sealed actual outcome",
                    ))
                }
            }
        }
    }
}
struct ScopeSpec {
    run: CapturedRun,
    purpose: Purpose,
}
#[derive(Clone)]
pub(super) struct CapturedScope(Arc<ScopeSpec>);
impl CapturedScope {
    pub(super) fn mode(&self) -> CanonicalPrepareMode {
        self.0.purpose.mode()
    }
    /// Multiple operations may observe the same immutable capture.
    pub(super) fn begin_prepare(&self) -> Result<(OperationCapture, AdmittedTransaction)> {
        let operation = OperationCapture(Arc::new(OperationState {
            scope: self.clone(),
            operation: Mutex::new(None),
            final_admitted: AtomicBool::new(false),
            prepare_released: AtomicBool::new(false),
        }));
        let transaction = operation.admit(Phase::Prepare, None)?;
        Ok((operation, transaction))
    }
}

struct StepState {
    assistant: Option<Arc<TerminalSeal>>,
    tools: Vec<(String, Allocation)>,
}
/// One ADMITTED provider step, not one turn-wide terminal slot.
pub(super) struct ProviderStep {
    run: CapturedRun,
    actual_step: Allocation,
    state: Mutex<StepState>,
}
impl ProviderStep {
    /// Only the actual native final-output path may call this, exactly once.
    pub(super) fn seal_assistant(&self, actual: CanonicalEntryDraft) -> Result<CapturedScope> {
        if actual.kind() != CanonicalMutationKind::AdmittedTerminal
            || !matches!(
                actual.entry(),
                FileEntry::Message {
                    message: AgentMessage::Assistant(_),
                    ..
                }
            )
        {
            return Err(invalid(
                "provider final outcome must be an actual assistant row",
            ));
        }
        let entry = self.run.shared.lock()?;
        validate_capture(&entry, &self.run, true)?;
        let mut state = self.state.lock().map_err(|_| closed())?;
        if state.assistant.is_some() {
            return Err(invalid("provider step already sealed"));
        }
        let seal = Arc::new(TerminalSeal {
            draft: actual,
            consumed: AtomicBool::new(false),
        });
        state.assistant = Some(seal.clone());
        Ok(self.run.scope(Purpose::Terminal {
            seal,
            _step: self.actual_step.clone(),
            _tool: None,
        }))
    }
    /// Bind only a tool call that exists in THIS step's actual assistant output.
    /// Admission must happen before abort; final sealing may happen after abort.
    pub(super) fn admit_tool<T: Any + Send + Sync>(
        &self,
        tool_call_id: &str,
        actual_tool: Arc<T>,
    ) -> Result<AdmittedTool> {
        let entry = self.run.shared.lock()?;
        validate_capture(&entry, &self.run, false)?;
        let mut state = self.state.lock().map_err(|_| closed())?;
        let Some(seal) = &state.assistant else {
            return Err(invalid("tool has no actual provider output"));
        };
        let FileEntry::Message {
            message: AgentMessage::Assistant(message),
            ..
        } = seal.draft.entry()
        else {
            return Err(invalid("step is not an assistant"));
        };
        let mut calls = message
            .content
            .iter()
            .filter_map(|block| {
                if let AssistantContentBlock::ToolCall(call) = block {
                    Some(call)
                } else {
                    None
                }
            })
            .filter(|call| call.id == tool_call_id);
        let call = calls
            .next()
            .ok_or_else(|| invalid("toolCallId absent from admitted provider step"))?;
        if calls.next().is_some() || state.tools.iter().any(|(id, _)| id == tool_call_id) {
            return Err(invalid("duplicate/ambiguous actual toolCallId"));
        }
        let name = call.name.clone();
        let actual_tool: Allocation = actual_tool;
        state
            .tools
            .push((tool_call_id.to_owned(), actual_tool.clone()));
        Ok(AdmittedTool {
            run: self.run.clone(),
            step: self.actual_step.clone(),
            actual_tool,
            id: tool_call_id.to_owned(),
            name,
            sealed: AtomicBool::new(false),
        })
    }
}
/// Existing/admitted native tool allocation, exact call ID and step binding.
pub(super) struct AdmittedTool {
    run: CapturedRun,
    step: Allocation,
    actual_tool: Allocation,
    id: String,
    name: String,
    sealed: AtomicBool,
}
impl AdmittedTool {
    pub(super) fn seal_final_outcome(&self, actual: CanonicalEntryDraft) -> Result<CapturedScope> {
        let FileEntry::Message {
            message: AgentMessage::ToolResult(result),
            ..
        } = actual.entry()
        else {
            return Err(invalid("native tool outcome must be a tool result"));
        };
        if actual.kind() != CanonicalMutationKind::AdmittedTerminal
            || result.tool_call_id != self.id
            || result.tool_name != self.name
        {
            return Err(invalid(
                "native tool ID/name differs from actual admitted call",
            ));
        }
        let entry = self.run.shared.lock()?;
        validate_capture(&entry, &self.run, true)?;
        if self.sealed.swap(true, Ordering::AcqRel) {
            return Err(invalid("native final outcome already sealed"));
        }
        Ok(self.run.scope(Purpose::Terminal {
            seal: Arc::new(TerminalSeal {
                draft: actual,
                consumed: AtomicBool::new(false),
            }),
            _step: self.step.clone(),
            _tool: Some(self.actual_tool.clone()),
        }))
    }
}

struct OperationState {
    scope: CapturedScope,
    operation: Mutex<Option<CanonicalOperation>>,
    final_admitted: AtomicBool,
    prepare_released: AtomicBool,
}
/// Opaque binding shared between preflight and fresh final transaction admission.
/// It holds identities only. The REAL frozen caller guard remains coordinator/F-owned.
#[derive(Clone)]
pub(super) struct OperationCapture(Arc<OperationState>);
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepare,
    Final,
}
impl OperationCapture {
    fn admit(
        &self,
        phase: Phase,
        exact: Option<CanonicalEntryDraft>,
    ) -> Result<AdmittedTransaction> {
        let scope = &self.0.scope.0;
        let mut entry = scope.run.shared.lock()?;
        validate_capture(&entry, &scope.run, scope.purpose.late())?;
        if phase == Phase::Final {
            if !self.0.prepare_released.load(Ordering::Acquire)
                || self.0.final_admitted.load(Ordering::Acquire)
            {
                return Err(invalid(
                    "prepare lease remains held or final already admitted",
                ));
            }
            scope.purpose.validate_draft(
                exact
                    .as_ref()
                    .ok_or_else(|| invalid("final draft missing"))?,
            )?;
            self.0.final_admitted.store(true, Ordering::Release);
        }
        entry.transactions += 1;
        let lease = Arc::new(LeaseState {
            shared: Arc::downgrade(&scope.run.shared),
            active: Arc::new(AtomicBool::new(true)),
            operation: self.clone(),
            phase,
            exact,
            flights: Mutex::new([None, None, None]),
        });
        let control = TransactionControl {
            lease: Arc::downgrade(&lease),
        };
        let permit =
            CanonicalWritePermit::new(scope.purpose.kind(), Arc::new(OwnedAuthority { lease }));
        Ok(AdmittedTransaction { permit, control })
    }
    /// Coordinator calls ONLY after matching successful F Prepared ACK and real
    /// caller `PreparedAcknowledged` signal. This primitive never fabricates either.
    pub(super) fn admit_final(
        &self,
        operation: &CanonicalOperation,
        draft: CanonicalEntryDraft,
    ) -> Result<AdmittedTransaction> {
        if self.0.operation.lock().map_err(|_| closed())?.as_ref() != Some(operation) {
            return Err(invalid(
                "final operation differs from prepared owner operation",
            ));
        }
        self.admit(Phase::Final, Some(draft))
    }
    pub(super) fn captured_run(&self) -> &CapturedRun {
        &self.0.scope.0.run
    }
}
struct LeaseState {
    shared: Weak<Shared>,
    active: Arc<AtomicBool>,
    operation: OperationCapture,
    phase: Phase,
    exact: Option<CanonicalEntryDraft>,
    flights: Mutex<[Option<Weak<FlightState>>; 3]>,
}
impl LeaseState {
    fn release(&self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut entry) = shared.entry.lock() {
                if self.active.swap(false, Ordering::AcqRel) {
                    entry.transactions = entry.transactions.saturating_sub(1);
                    if self.phase == Phase::Prepare {
                        self.operation
                            .0
                            .prepare_released
                            .store(true, Ordering::Release);
                    }
                }
            }
            shared.changed.notify_waiters();
        }
    }
}
impl Drop for LeaseState {
    fn drop(&mut self) {
        self.release();
    }
}
/// Only the permit owns the non-Clone transaction lease. Move it directly to F.
/// Keeping the weak control cannot hide caller loss or retain a transaction.
pub(super) struct AdmittedTransaction {
    pub(super) permit: CanonicalWritePermit,
    pub(super) control: TransactionControl,
}
#[derive(Clone)]
pub(super) struct TransactionControl {
    lease: Weak<LeaseState>,
}
impl TransactionControl {
    /// Explicit release revokes even an incorrectly retained/rewrapped authority.
    pub(super) fn release(&self) {
        if let Some(lease) = self.lease.upgrade() {
            lease.release();
        }
    }
    /// Pair this unique recorder with the actual writer-owned flight for this stage.
    pub(super) fn io_recorder(&self, stage: CanonicalIoStage) -> Result<IoResultRecorder> {
        let lease = self.lease.upgrade().ok_or_else(closed)?;
        let flights = lease.flights.lock().map_err(|_| closed())?;
        let flight = flights[stage_index(stage)]
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| invalid("no owned I/O flight for recorder"))?;
        if flight.recorder_claimed.swap(true, Ordering::AcqRel) {
            return Err(invalid("I/O recorder already claimed"));
        }
        Ok(IoResultRecorder { flight })
    }
    pub(super) fn request_close(&self, reason: CloseReason) -> Result<CloseTicket> {
        let lease = self.lease.upgrade().ok_or_else(closed)?;
        let shared = lease.shared.upgrade().ok_or_else(closed)?;
        Ok(stage_close(
            &shared,
            reason,
            Some(Arc::downgrade(&lease.active)),
        ))
    }
}

struct OwnedAuthority {
    lease: Arc<LeaseState>,
}
fn stage_index(stage: CanonicalIoStage) -> usize {
    match stage {
        CanonicalIoStage::PreparePreflight => 0,
        CanonicalIoStage::CommitPreflight => 1,
        CanonicalIoStage::RequestedMutation => 2,
    }
}
impl CanonicalWriteAuthority for OwnedAuthority {
    fn enter_io(
        &self,
        stage: CanonicalIoStage,
        cursor: &CanonicalCursor,
        operation: &CanonicalOperation,
        draft: Option<&CanonicalEntryDraft>,
    ) -> Result<CanonicalIoFlight> {
        let lease = &self.lease;
        let capture = &lease.operation.0.scope.0;
        let shared = &capture.run.shared;
        let mut entry = shared.lock()?;
        if !lease.active.load(Ordering::Acquire) {
            return Err(closed());
        }
        validate_capture(&entry, &capture.run, capture.purpose.late())?;
        validate_source(&capture.run.source, cursor)?;
        // Mode, stage, purpose, operation and actual payload are checked HERE,
        // even if callers rewrap this Arc with a cross-kind public permit.
        match (lease.phase, stage) {
            (Phase::Prepare, CanonicalIoStage::PreparePreflight) => {
                if draft.is_some() {
                    return Err(invalid("prepare preflight has no requested payload"));
                }
            }
            (
                Phase::Final,
                CanonicalIoStage::CommitPreflight | CanonicalIoStage::RequestedMutation,
            ) => {
                let requested =
                    draft.ok_or_else(|| invalid("final stage requires exact authorized draft"))?;
                capture.purpose.validate_draft(requested)?;
                if lease.exact.as_ref().is_none_or(|exact| {
                    exact.kind() != requested.kind() || exact.entry() != requested.entry()
                }) {
                    return Err(invalid("per-operation exact requested draft differs"));
                }
            }
            (
                Phase::Prepare,
                CanonicalIoStage::CommitPreflight | CanonicalIoStage::RequestedMutation,
            )
            | (Phase::Final, CanonicalIoStage::PreparePreflight) => {
                return Err(invalid("I/O stage differs from transaction phase"))
            }
        }
        let mut bound = lease.operation.0.operation.lock().map_err(|_| closed())?;
        if let Some(actual) = bound.as_ref() {
            if actual != operation {
                return Err(invalid("owner-issued operation differs"));
            }
        } else if lease.phase == Phase::Prepare {
            *bound = Some(operation.clone());
        } else {
            return Err(invalid("unprepared owner operation"));
        }
        let mut flights = lease.flights.lock().map_err(|_| closed())?;
        let index = stage_index(stage);
        if flights[index].is_some() {
            return Err(invalid("I/O stage already admitted"));
        }
        if stage == CanonicalIoStage::RequestedMutation {
            if let Purpose::Terminal { seal, .. } = &capture.purpose {
                if seal.consumed.swap(true, Ordering::AcqRel) {
                    return Err(invalid("native terminal slot already consumed"));
                }
            }
        }
        let flight_record = Arc::new(FlightState {
            _capture: capture.run.clone(),
            shared: Arc::downgrade(shared),
            recorded: AtomicU8::new(0),
            recorder_claimed: AtomicBool::new(false),
        });
        flights[index] = Some(Arc::downgrade(&flight_record));
        entry.io += 1;
        Ok(CanonicalIoFlight::new(FlightGuard {
            state: flight_record,
        }))
    }
}
struct FlightState {
    _capture: CapturedRun,
    shared: Weak<Shared>,
    recorded: AtomicU8,
    recorder_claimed: AtomicBool,
}
struct FlightGuard {
    state: Arc<FlightState>,
}
impl Drop for FlightGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.state.shared.upgrade() {
            if let Ok(mut entry) = shared.entry.lock() {
                entry.io = entry.io.saturating_sub(1);
                if self.state.recorded.load(Ordering::Acquire) != 1 {
                    entry.poisoned = true;
                    entry.projection_quarantined = true;
                    if entry.gate == Gate::Open {
                        entry.gate = Gate::Closing(CloseReason::SourceChanged);
                    }
                }
            }
            shared.changed.notify_waiters();
        }
    }
}
/// The result classification is recorded by the ACTUAL writer, not its awaiter.
#[derive(Clone, Copy)]
pub(super) enum RecordedIo {
    Known,
    Uncertain,
}
pub(super) struct IoResultRecorder {
    flight: Arc<FlightState>,
}
impl IoResultRecorder {
    pub(super) fn record(self, result: RecordedIo) -> Result<()> {
        let value = match result {
            RecordedIo::Known => 1,
            RecordedIo::Uncertain => 2,
        };
        self.flight
            .recorded
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| invalid("I/O result already recorded"))?;
        if value == 2 {
            if let Some(shared) = self.flight.shared.upgrade() {
                shared.quarantine();
            }
        }
        Ok(())
    }
}

/// Registered at serving capture. Finish ONLY after final ordered listener/idle boundary.
/// An abandoned anchor quarantines; Drop never fabricates that boundary.
pub(super) struct LateRunAnchor {
    _serving: Arc<ServingIdentity>,
    _source: CanonicalCursor,
    shared: Weak<Shared>,
    life: Arc<AtomicBool>,
    boundary: bool,
}
impl LateRunAnchor {
    pub(super) fn finish_ordered_boundary(mut self) {
        self.boundary = true;
    }
    pub(super) fn request_close(&self, reason: CloseReason) -> Result<CloseTicket> {
        let shared = self.shared.upgrade().ok_or_else(closed)?;
        Ok(stage_close(
            &shared,
            reason,
            Some(Arc::downgrade(&self.life)),
        ))
    }
}
impl Drop for LateRunAnchor {
    fn drop(&mut self) {
        self.life.store(false, Ordering::Release);
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut entry) = shared.entry.lock() {
                entry.anchors = entry.anchors.saturating_sub(1);
                if !self.boundary {
                    entry.poisoned = true;
                    entry.projection_quarantined = true;
                }
            }
            shared.changed.notify_waiters();
        }
    }
}
fn stage_close(
    shared: &Arc<Shared>,
    reason: CloseReason,
    self_hold: Option<Weak<AtomicBool>>,
) -> CloseTicket {
    if let Ok(mut entry) = shared.entry.lock() {
        if entry.gate == Gate::Open
            || (entry.gate == Gate::Closing(CloseReason::OrdinaryAbort)
                && reason != CloseReason::OrdinaryAbort)
        {
            entry.gate = Gate::Closing(reason);
        }
        entry.projection_quarantined = true;
    }
    shared.changed.notify_waiters();
    CloseTicket {
        shared: shared.clone(),
        self_hold,
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DrainStatus {
    Pending {
        io: usize,
        transactions: usize,
        anchors: usize,
    },
    Drained,
    Blocked,
    WouldDrainSelf,
}
/// Closing is requested synchronously; all waits happen outside entry locks.
pub(super) struct CloseTicket {
    shared: Arc<Shared>,
    self_hold: Option<Weak<AtomicBool>>,
}
impl CloseTicket {
    pub(super) fn status(&self) -> DrainStatus {
        if self
            .self_hold
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|c| c.load(Ordering::Acquire))
        {
            return DrainStatus::WouldDrainSelf;
        }
        let Ok(entry) = self.shared.lock() else {
            return DrainStatus::Blocked;
        };
        if entry.poisoned {
            return DrainStatus::Blocked;
        }
        if entry.io == 0 && entry.transactions == 0 && entry.anchors == 0 {
            DrainStatus::Drained
        } else {
            DrainStatus::Pending {
                io: entry.io,
                transactions: entry.transactions,
                anchors: entry.anchors,
            }
        }
    }
    pub(super) async fn wait_drained(&self) -> DrainStatus {
        loop {
            let notification = self.shared.changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            let status = self.status();
            if !matches!(status, DrainStatus::Pending { .. }) {
                return status;
            }
            notification.await;
        }
    }
    /// External idle-boundary coordinator consumes the ticket before activating
    /// a NEW owner/serving epoch. This cannot clear old projection quarantine.
    pub(super) fn finish_at_external_idle(self) -> Result<()> {
        if self.status() != DrainStatus::Drained {
            return Err(closed());
        }
        let mut entry = self.shared.lock()?;
        if entry.poisoned || entry.io != 0 || entry.transactions != 0 || entry.anchors != 0 {
            return Err(closed());
        }
        entry.gate = Gate::Closed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::canonical::{CanonicalFrontier, CanonicalOwnerScope};
    use std::path::PathBuf;
    use std::sync::{mpsc, Barrier};
    use std::time::Duration;

    // Synthetic authority facts only. No actual runtime SessionLease/backend proof.
    struct Fixture {
        owner: AuthorityOwner,
        run: CapturedRun,
        anchor: Option<LateRunAnchor>,
    }
    fn cursor() -> CanonicalCursor {
        CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "s".into(),
            source_path: PathBuf::from("/private/s.jsonl"),
            raw_source_generation: [1; 32],
            header_certificate: [2; 32],
            canonical_revision: 0,
            branch_revision: 1,
            selected_leaf: None,
            frontier: CanonicalFrontier {
                bytes: 9,
                rows: 1,
                prefix_certificate: [3; 32],
            },
        }
    }
    fn fixture(contributions: Vec<ContributionSelection>) -> Fixture {
        let source = cursor();
        let owner = AuthorityOwner::new(source.clone());
        let (run, anchor) = owner
            .capture_serving_run(
                &source,
                Arc::new(String::from("same run name")),
                Arc::new(String::from("same model name")),
                contributions,
            )
            .expect("actual capture");
        Fixture {
            owner,
            run,
            anchor: Some(anchor),
        }
    }
    fn draft(kind: CanonicalMutationKind, value: serde_json::Value) -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(kind, serde_json::from_value(value).expect("native entry"))
            .expect("draft")
    }
    fn assistant(call: Option<&str>) -> CanonicalEntryDraft {
        let content = call.map_or_else(
            || serde_json::json!([]),
            |id| {
                serde_json::json!([
                    {"type":"toolCall","id":id,"name":"native","arguments":{"x":1}}
                ])
            },
        );
        draft(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::json!({"type":"message","message":{
                "role":"assistant","content":content,"api":"fake","provider":"fake","model":"same model name",
                "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
                "stopReason":if call.is_some() { "toolUse" } else { "stop" },"timestamp":1
            }}),
        )
    }
    fn tool(id: &str, content: &serde_json::Value, error: bool) -> CanonicalEntryDraft {
        draft(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::json!({"type":"message","message":{
                "role":"toolResult","toolCallId":id,"toolName":"native","content":content,
                "details":{"native":[1,null,true]},"isError":error,"timestamp":2
            }}),
        )
    }
    fn compact() -> CanonicalEntryDraft {
        draft(
            CanonicalMutationKind::ContextPolicy,
            serde_json::json!({"type":"compaction",
            "summary":"exact","firstKeptEntryId":"a","tokensBefore":1,"details":{"bytes":"AA=="}}),
        )
    }
    fn label(value: &str) -> CanonicalEntryDraft {
        draft(
            CanonicalMutationKind::OwnerMetadata,
            serde_json::json!({"type":"label","targetId":"a","label":value}),
        )
    }
    fn enter(
        tx: &AdmittedTransaction,
        stage: CanonicalIoStage,
        run: &CapturedRun,
        operation: &CanonicalOperation,
        draft: Option<&CanonicalEntryDraft>,
    ) -> CanonicalIoFlight {
        tx.permit
            .enter_io(stage, run.source(), operation, draft)
            .expect("admitted I/O")
    }
    fn record(tx: &AdmittedTransaction, stage: CanonicalIoStage) {
        tx.control
            .io_recorder(stage)
            .expect("unique recorder")
            .record(RecordedIo::Known)
            .expect("record actual result");
    }
    // This helper models ONLY the authority side of successful prepare/ACK. It
    // does not certify an F ACK, canonical append, or strict source CAS.
    fn prepared(scope: &CapturedScope) -> (OperationCapture, CanonicalOperation) {
        let (capture, tx) = scope.begin_prepare().expect("prepare lease");
        let operation = CanonicalOperation::new();
        let flight = enter(
            &tx,
            CanonicalIoStage::PreparePreflight,
            &scope.0.run,
            &operation,
            None,
        );
        record(&tx, CanonicalIoStage::PreparePreflight);
        drop(flight);
        drop(tx);
        (capture, operation)
    }
    fn persist(scope: &CapturedScope, draft: &CanonicalEntryDraft) {
        let (operation, token) = prepared(scope);
        let tx = operation
            .admit_final(&token, draft.clone())
            .expect("fresh final transaction");
        for stage in [
            CanonicalIoStage::CommitPreflight,
            CanonicalIoStage::RequestedMutation,
        ] {
            let flight = enter(&tx, stage, &scope.0.run, &token, Some(draft));
            record(&tx, stage);
            drop(flight);
        }
    }

    #[test]
    fn close_and_new_admission_share_one_linearization_gate() {
        for _ in 0..12 {
            let mut f = fixture(vec![]);
            let barrier = Arc::new(Barrier::new(2));
            let scope = f.run.context_policy();
            let start = barrier.clone();
            let worker = std::thread::spawn(move || {
                start.wait();
                scope.begin_prepare()
            });
            barrier.wait();
            let close = f.owner.request_close(CloseReason::Retirement);
            let admission = worker.join().expect("worker");
            if let Ok((_capture, tx)) = admission {
                assert_eq!(
                    close.status(),
                    DrainStatus::Pending {
                        io: 0,
                        transactions: 1,
                        anchors: 1
                    }
                );
                assert!(tx
                    .permit
                    .enter_io(
                        CanonicalIoStage::PreparePreflight,
                        f.run.source(),
                        &CanonicalOperation::new(),
                        None
                    )
                    .is_err());
                drop(tx);
            }
            assert!(f.run.observe().begin_prepare().is_err());
            f.anchor.take().expect("anchor").finish_ordered_boundary();
            assert_eq!(close.status(), DrainStatus::Drained);
            close
                .finish_at_external_idle()
                .expect("external successor boundary");
            assert!(!f.owner.projection_allowed());
        }
    }

    struct NoopNotify;
    impl crate::session::canonical::CanonicalCallerNotifier for NoopNotify {
        fn changed(&self) {}
    }
    #[test]
    fn caller_drop_does_not_release_actual_writer_flight_or_recording() {
        let mut f = fixture(vec![]);
        let (_, tx) = f.run.context_policy().begin_prepare().expect("prepare");
        let op = CanonicalOperation::new();
        let flight = enter(&tx, CanonicalIoStage::PreparePreflight, &f.run, &op, None);
        let recorder = tx
            .control
            .io_recorder(CanonicalIoStage::PreparePreflight)
            .expect("writer recorder");
        let (caller, signal) =
            crate::session::canonical::CanonicalCallerGuard::new(Arc::new(NoopNotify));
        let (start_tx, start_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            start_rx.recv().expect("write allowed to finish");
            recorder
                .record(RecordedIo::Known)
                .expect("writer records result");
            drop(flight);
        });
        let close = f.owner.request_close(CloseReason::OrdinaryAbort);
        drop(caller);
        assert_eq!(
            signal.state(),
            crate::session::canonical::CanonicalCallerState::Abandoned
        );
        drop(tx);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(
            close.status(),
            DrainStatus::Pending {
                io: 1,
                transactions: 0,
                anchors: 0
            }
        );
        start_tx.send(()).expect("release real writer");
        writer.join().expect("writer finished");
        assert_eq!(close.status(), DrainStatus::Drained);
        assert!(!f.owner.projection_allowed());
    }
    #[test]
    fn transaction_and_registered_late_run_anchor_each_prevent_drain() {
        let mut f = fixture(vec![]);
        let (_, tx) = f.run.observe().begin_prepare().expect("transaction");
        let close = f.owner.request_close(CloseReason::OrdinaryAbort);
        assert_eq!(
            close.status(),
            DrainStatus::Pending {
                io: 0,
                transactions: 1,
                anchors: 1
            }
        );
        drop(tx);
        assert_eq!(
            close.status(),
            DrainStatus::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            }
        );
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(close.status(), DrainStatus::Drained);
    }
    #[tokio::test]
    async fn hung_flight_timeout_cannot_activate_successor() {
        let mut f = fixture(vec![]);
        let (_, tx) = f.run.observe().begin_prepare().expect("transaction");
        let op = CanonicalOperation::new();
        let flight = enter(&tx, CanonicalIoStage::PreparePreflight, &f.run, &op, None);
        let recorder = tx
            .control
            .io_recorder(CanonicalIoStage::PreparePreflight)
            .expect("recorder");
        let close = f.owner.request_close(CloseReason::Retirement);
        drop(tx);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert!(tokio::time::timeout(Duration::ZERO, close.wait_drained())
            .await
            .is_err());
        assert_eq!(
            close.status(),
            DrainStatus::Pending {
                io: 1,
                transactions: 0,
                anchors: 0
            }
        );
        assert!(close.finish_at_external_idle().is_err());
        recorder.record(RecordedIo::Known).expect("finish result");
        drop(flight);
    }
    #[tokio::test]
    async fn wait_is_outside_gate_and_observes_drain_notification() {
        let mut f = fixture(vec![]);
        let close = f.owner.request_close(CloseReason::Retirement);
        let waiter = tokio::spawn(async move { close.wait_drained().await });
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(waiter.await.expect("waiter"), DrainStatus::Drained);
    }
    #[test]
    fn abandoned_or_uncertain_writer_and_abandoned_anchor_are_blocked() {
        for outcome in [None, Some(RecordedIo::Uncertain)] {
            let mut f = fixture(vec![]);
            let (_, tx) = f.run.observe().begin_prepare().expect("transaction");
            let flight = enter(
                &tx,
                CanonicalIoStage::PreparePreflight,
                &f.run,
                &CanonicalOperation::new(),
                None,
            );
            if let Some(result) = outcome {
                tx.control
                    .io_recorder(CanonicalIoStage::PreparePreflight)
                    .expect("recorder")
                    .record(result)
                    .expect("record");
            }
            drop(flight);
            drop(tx);
            let close = f.owner.request_close(CloseReason::OrdinaryAbort);
            f.anchor.take().expect("anchor").finish_ordered_boundary();
            assert_eq!(close.status(), DrainStatus::Blocked);
            assert!(close.finish_at_external_idle().is_err());
            assert!(!f.owner.projection_allowed());
        }
        let mut f = fixture(vec![]);
        let close = f.owner.request_close(CloseReason::Retirement);
        drop(f.anchor.take());
        assert_eq!(close.status(), DrainStatus::Blocked);
    }
    #[test]
    fn lock_poison_never_counts_as_successful_drain() {
        let f = fixture(vec![]);
        let shared = f.owner.shared.clone();
        assert!(std::thread::spawn(move || {
            let _held = shared.entry.lock().expect("entry");
            panic!("native negative poison");
        })
        .join()
        .is_err());
        let close = f.owner.request_close(CloseReason::Retirement);
        assert_eq!(close.status(), DrainStatus::Blocked);
        assert!(f.run.observe().begin_prepare().is_err());
        assert!(close.finish_at_external_idle().is_err());
    }
    #[test]
    fn self_drain_is_staged_not_waited_and_prepare_lease_releases_before_final() {
        let mut f = fixture(vec![]);
        let (capture, tx) = f.run.context_policy().begin_prepare().expect("prepare");
        let token = CanonicalOperation::new();
        let flight = enter(
            &tx,
            CanonicalIoStage::PreparePreflight,
            &f.run,
            &token,
            None,
        );
        record(&tx, CanonicalIoStage::PreparePreflight);
        drop(flight);
        assert!(capture.admit_final(&token, compact()).is_err());
        let ticket = tx
            .control
            .request_close(CloseReason::Retirement)
            .expect("staged close");
        assert_eq!(ticket.status(), DrainStatus::WouldDrainSelf);
        drop(tx);
        assert_eq!(
            ticket.status(),
            DrainStatus::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            }
        );
        let own_anchor_ticket = f
            .anchor
            .as_ref()
            .expect("anchor")
            .request_close(CloseReason::Retirement)
            .expect("anchor staged");
        assert_eq!(own_anchor_ticket.status(), DrainStatus::WouldDrainSelf);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(ticket.status(), DrainStatus::Drained);
    }
    #[test]
    fn retained_scope_is_not_transaction_and_released_authority_cannot_mint_io() {
        let f = fixture(vec![]);
        let scope = f.run.observe();
        let (capture, tx) = scope.begin_prepare().expect("transaction");
        let retained_authority = tx.control.lease.upgrade().expect("lease");
        let rewrapped = CanonicalWritePermit::new(
            CanonicalMutationKind::OwnerMetadata,
            Arc::new(OwnedAuthority {
                lease: retained_authority,
            }),
        );
        tx.control.release();
        assert!(rewrapped
            .enter_io(
                CanonicalIoStage::PreparePreflight,
                f.run.source(),
                &CanonicalOperation::new(),
                None
            )
            .is_err());
        assert_eq!(f.owner.shared.lock().expect("entry").transactions, 0);
        drop(tx);
        drop(rewrapped);
        drop(capture);
        assert_eq!(f.owner.shared.lock().expect("entry").transactions, 0);
    }
    #[test]
    fn same_capture_supports_multiple_observe_operations_without_mutation() {
        let f = fixture(vec![]);
        let scope = f.run.observe();
        let (first, a) = prepared(&scope);
        let (second, b) = prepared(&scope);
        assert_ne!(a, b);
        assert!(first.captured_run().same_serving(second.captured_run()));
        assert!(first.admit_final(&a, compact()).is_err());
        assert_eq!(f.owner.shared.lock().expect("entry").transactions, 0);
    }
    #[test]
    fn preflight_does_not_consume_terminal_slot_and_each_provider_step_has_one() {
        let f = fixture(vec![]);
        for _ in 0..3 {
            let actual = assistant(Some("same-id-next-step"));
            let step = f
                .run
                .admit_provider_step(Arc::new(()))
                .expect("provider step");
            let terminal = step
                .seal_assistant(actual.clone())
                .expect("actual provider outcome");
            let tool_admission = step
                .admit_tool("same-id-next-step", Arc::new(()))
                .expect("actual tool");
            persist(&terminal, &actual);
            let (again, op) = prepared(&terminal);
            let tx = again
                .admit_final(&op, actual.clone())
                .expect("new transaction but same terminal slot");
            let preflight = enter(
                &tx,
                CanonicalIoStage::CommitPreflight,
                &f.run,
                &op,
                Some(&actual),
            );
            record(&tx, CanonicalIoStage::CommitPreflight);
            drop(preflight);
            assert!(tx
                .permit
                .enter_io(
                    CanonicalIoStage::RequestedMutation,
                    f.run.source(),
                    &op,
                    Some(&actual)
                )
                .is_err());
            let result = tool("same-id-next-step", &serde_json::json!([]), false);
            let scope = tool_admission
                .seal_final_outcome(result.clone())
                .expect("native tool final");
            persist(&scope, &result);
        }
    }
    #[test]
    fn tool_id_step_native_outcome_and_duplicate_seal_checks() {
        let f = fixture(vec![]);
        let allocation = Arc::new(());
        let step = f.run.admit_provider_step(allocation.clone()).expect("step");
        assert!(f.run.admit_provider_step(allocation).is_err());
        step.seal_assistant(assistant(Some("real")))
            .expect("assistant");
        assert!(step.seal_assistant(assistant(None)).is_err());
        assert!(step.admit_tool("forged", Arc::new(())).is_err());
        let tool_slot = step
            .admit_tool("real", Arc::new(()))
            .expect("existing tool");
        assert!(step.admit_tool("real", Arc::new(())).is_err());
        assert!(tool_slot
            .seal_final_outcome(tool("wrong", &serde_json::json!([]), false))
            .is_err());
        let actual = tool("real", &serde_json::json!([]), true);
        let terminal = tool_slot
            .seal_final_outcome(actual.clone())
            .expect("native exact failure");
        assert!(tool_slot.seal_final_outcome(actual.clone()).is_err());
        for forged in [
            tool(
                "real",
                &serde_json::json!([{"type":"text","text":""}]),
                true,
            ),
            tool("real", &serde_json::json!([]), false),
        ] {
            let (capture, op) = prepared(&terminal);
            assert!(capture.admit_final(&op, forged).is_err());
        }
        persist(&terminal, &actual);
    }
    #[test]
    fn cross_kind_rewrap_and_metadata_slot_bypasses_fail_at_actual_io() {
        let f = fixture(vec![]);
        let terminal_draft = assistant(None);
        let step = f.run.admit_provider_step(Arc::new(())).expect("step");
        let terminal = step
            .seal_assistant(terminal_draft.clone())
            .expect("terminal");
        let (capture, op) = prepared(&terminal);
        let tx = capture
            .admit_final(&op, terminal_draft.clone())
            .expect("final");
        let lease = tx
            .control
            .lease
            .upgrade()
            .expect("retained/rewrapped allocation");
        let cross = CanonicalWritePermit::new(
            CanonicalMutationKind::OwnerMetadata,
            Arc::new(OwnedAuthority { lease }),
        );
        assert!(cross
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                f.run.source(),
                &op,
                Some(&label("forged"))
            )
            .is_err());
        assert!(tx
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                f.run.source(),
                &op,
                None
            )
            .is_err());
        let actual = enter(
            &tx,
            CanonicalIoStage::RequestedMutation,
            &f.run,
            &op,
            Some(&terminal_draft),
        );
        record(&tx, CanonicalIoStage::RequestedMutation);
        drop(actual);
        let metadata = f
            .run
            .owner_metadata(MetadataAction::Label, label("specific"))
            .expect("specific action");
        assert!(f
            .run
            .owner_metadata(MetadataAction::ModelChange, label("wrong action"))
            .is_err());
        let custom = draft(
            CanonicalMutationKind::OwnerMetadata,
            serde_json::json!({"type":"custom","customType":"anything","data":{}}),
        );
        assert!(f.run.owner_metadata(MetadataAction::Label, custom).is_err());
        let (capture, op) = prepared(&metadata);
        assert!(capture
            .admit_final(&op, label("different payload"))
            .is_err());
        persist(&metadata, &label("specific"));
        assert!(f
            .run
            .context_policy()
            .begin_prepare()
            .expect("prepare")
            .0
            .admit_final(&CanonicalOperation::new(), label("not policy"))
            .is_err());
    }
    #[test]
    fn every_stage_validates_wrong_operation_owner_header_path_branch() {
        let f = fixture(vec![]);
        let (capture, op) = prepared(&f.run.context_policy());
        assert!(capture
            .admit_final(&CanonicalOperation::new(), compact())
            .is_err());
        let exact = compact();
        let tx = capture.admit_final(&op, exact.clone()).expect("final");
        for stage in [
            CanonicalIoStage::CommitPreflight,
            CanonicalIoStage::RequestedMutation,
        ] {
            for change in 0..5 {
                let mut wrong = f.run.source().clone();
                match change {
                    0 => wrong.owner = CanonicalOwnerScope::new(),
                    1 => wrong.header_certificate = [8; 32],
                    2 => wrong.source_path = PathBuf::from("/different"),
                    3 => wrong.branch_revision += 1,
                    4 => wrong.session_id = "different".into(),
                    _ => unreachable!(),
                }
                assert!(tx
                    .permit
                    .enter_io(stage, &wrong, &op, Some(&exact))
                    .is_err());
            }
            assert!(tx
                .permit
                .enter_io(
                    stage,
                    f.run.source(),
                    &CanonicalOperation::new(),
                    Some(&exact)
                )
                .is_err());
            let flight = enter(&tx, stage, &f.run, &op, Some(&exact));
            record(&tx, stage);
            drop(flight);
        }
    }
    #[test]
    fn own_preflight_head_advance_is_not_generic_authority_cas_failure() {
        let f = fixture(vec![]);
        let (capture, op) = prepared(&f.run.context_policy());
        let exact = compact();
        let tx = capture.admit_final(&op, exact.clone()).expect("final");
        let mut advanced = f.run.source().clone();
        advanced.raw_source_generation = [99; 32];
        advanced.canonical_revision += 1;
        advanced.frontier.bytes += 7;
        advanced.frontier.rows += 1;
        advanced.frontier.prefix_certificate = [88; 32];
        advanced.selected_leaf = Some("own-preflight-row".into());
        assert_ne!(
            &advanced,
            f.run.source(),
            "store strict CAS must still catch this advance"
        );
        let flight = tx
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                &advanced,
                &op,
                Some(&exact),
            )
            .expect("authority is not strict store CAS");
        record(&tx, CanonicalIoStage::RequestedMutation);
        drop(flight);
    }
    #[test]
    fn zero_contribution_default_still_checks_actual_source_run_model() {
        for change in 0..4 {
            let f = fixture(vec![]);
            let scope = f.run.context_policy();
            let (_, tx) = scope.begin_prepare().expect("pre-admitted transaction");
            {
                let mut entry = f.owner.shared.lock().expect("entry");
                let old = entry.serving.as_ref().expect("serving");
                match change {
                    0 => entry.source.header_certificate = [9; 32],
                    1 => entry.source.branch_revision += 1,
                    2 | 3 => {
                        entry.serving = Some(Arc::new(ServingIdentity {
                            actual_run: if change == 2 {
                                Arc::new("same run name")
                            } else {
                                old.actual_run.clone()
                            },
                            actual_model: if change == 3 {
                                Arc::new("same model name")
                            } else {
                                old.actual_model.clone()
                            },
                            contributions: vec![],
                        }));
                    }
                    _ => unreachable!(),
                }
            }
            assert!(tx
                .permit
                .enter_io(
                    CanonicalIoStage::PreparePreflight,
                    f.run.source(),
                    &CanonicalOperation::new(),
                    None
                )
                .is_err());
            assert!(scope.begin_prepare().is_err());
        }
    }
    #[test]
    fn same_name_aba_and_retired_contributions_never_reopen() {
        let selected = ContributionSelection::capture(Arc::new(String::from("strategy")), 7);
        let mut f = fixture(vec![selected.clone()]);
        let scope = f.run.context_policy();
        let (_, tx) = scope.begin_prepare().expect("transaction");
        let close = f
            .owner
            .retire_contribution(&selected)
            .expect("retire actual generation");
        assert!(tx
            .permit
            .enter_io(
                CanonicalIoStage::PreparePreflight,
                f.run.source(),
                &CanonicalOperation::new(),
                None
            )
            .is_err());
        drop(tx);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        close.finish_at_external_idle().expect("closed");
        assert!(scope.begin_prepare().is_err());
        let source = cursor();
        let next = AuthorityOwner::new(source.clone());
        assert!(next
            .capture_serving_run(
                &source,
                Arc::new("same run name"),
                Arc::new("same model name"),
                vec![selected]
            )
            .is_err());
        let successor = fixture(vec![ContributionSelection::capture(
            Arc::new(String::from("strategy")),
            8,
        )]);
        assert!(!f.run.same_serving(&successor.run));
        assert!(successor.run.observe().begin_prepare().is_ok());
        assert!(!f.owner.projection_allowed());
    }
    #[test]
    fn late_real_terminal_after_abort_exact_payload_survives_but_new_work_does_not() {
        let mut f = fixture(vec![]);
        let step = f
            .run
            .admit_provider_step(Arc::new(()))
            .expect("admitted main step");
        let assistant_draft = assistant(Some("actual"));
        let terminal = step
            .seal_assistant(assistant_draft.clone())
            .expect("actual assistant");
        let admitted_tool = step
            .admit_tool("actual", Arc::new(()))
            .expect("existing tool");
        let context = f.run.context_policy();
        let close = f.owner.request_close(CloseReason::OrdinaryAbort);
        assert!(context.begin_prepare().is_err());
        assert!(f.run.admit_provider_step(Arc::new(())).is_err());
        assert!(step.admit_tool("actual", Arc::new(())).is_err());
        assert!(f
            .run
            .owner_metadata(MetadataAction::Label, label("new lifecycle"))
            .is_err());
        persist(&terminal, &assistant_draft);
        let actual = tool("actual", &serde_json::json!([]), true);
        let late = admitted_tool
            .seal_final_outcome(actual.clone())
            .expect("actual native failure after abort");
        let (capture, op) = prepared(&late);
        assert!(capture
            .admit_final(
                &op,
                tool(
                    "actual",
                    &serde_json::json!([{"type":"text","text":"synthetic"}]),
                    true
                )
            )
            .is_err());
        persist(&late, &actual);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(close.status(), DrainStatus::Drained);
        assert!(!f.owner.projection_allowed());
        assert!(
            late.begin_prepare().is_err(),
            "ordered boundary ends late admission"
        );
    }
    #[test]
    fn late_terminal_never_bypasses_navigation_source_or_poison_fences() {
        for reason in [
            CloseReason::Navigation,
            CloseReason::SourceChanged,
            CloseReason::ModelChange,
            CloseReason::Retirement,
        ] {
            let f = fixture(vec![]);
            let step = f.run.admit_provider_step(Arc::new(())).expect("step");
            let terminal = step.seal_assistant(assistant(None)).expect("actual");
            f.owner.request_close(reason);
            assert!(terminal.begin_prepare().is_err());
        }
        let f = fixture(vec![]);
        let step = f.run.admit_provider_step(Arc::new(())).expect("step");
        let terminal = step.seal_assistant(assistant(None)).expect("actual");
        f.owner.request_close(CloseReason::OrdinaryAbort);
        f.owner.quarantine();
        assert!(terminal.begin_prepare().is_err());
        assert!(!f.owner.projection_allowed());
    }
    #[test]
    fn exact_assistant_stop_reason_error_details_and_empty_content_are_immutable() {
        let f = fixture(vec![]);
        let step = f.run.admit_provider_step(Arc::new(())).expect("step");
        let actual = assistant(None);
        let terminal = step.seal_assistant(actual.clone()).expect("actual");
        for field in ["content", "stopReason", "errorMessage", "details"] {
            let mut value = serde_json::to_value(actual.entry()).expect("exact native value");
            value["message"][field] = match field {
                "content" => serde_json::json!([{"type":"text","text":""}]),
                "stopReason" => serde_json::json!("error"),
                "errorMessage" => serde_json::json!("forged"),
                "details" => serde_json::json!({"forged":true}),
                _ => unreachable!(),
            };
            let forged = draft(CanonicalMutationKind::AdmittedTerminal, value);
            let (capture, op) = prepared(&terminal);
            assert!(capture.admit_final(&op, forged).is_err());
        }
        persist(&terminal, &actual);
    }
    #[test]
    fn cleanup_cannot_clear_projection_quarantine_or_open_old_epoch() {
        let mut f = fixture(vec![]);
        let (_, tx) = f.run.observe().begin_prepare().expect("prepare");
        let flight = enter(
            &tx,
            CanonicalIoStage::PreparePreflight,
            &f.run,
            &CanonicalOperation::new(),
            None,
        );
        let recorder = tx
            .control
            .io_recorder(CanonicalIoStage::PreparePreflight)
            .expect("recorder");
        let close = f.owner.request_close(CloseReason::OrdinaryAbort);
        recorder
            .record(RecordedIo::Known)
            .expect("raw cleanup recorded");
        drop(flight);
        drop(tx);
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        close.finish_at_external_idle().expect("drain complete");
        assert!(!f.owner.projection_allowed());
        assert!(f.run.observe().begin_prepare().is_err());
    }
    #[test]
    fn actual_owner_drop_revokes_captures_even_with_retained_transaction() {
        let Fixture { owner, run, anchor } = fixture(vec![]);
        let (_, tx) = run.observe().begin_prepare().expect("admission");
        drop(owner);
        assert!(run.observe().begin_prepare().is_err());
        assert!(tx
            .permit
            .enter_io(
                CanonicalIoStage::PreparePreflight,
                run.source(),
                &CanonicalOperation::new(),
                None
            )
            .is_err());
        drop(tx);
        anchor.expect("anchor").finish_ordered_boundary();
    }
    #[test]
    fn provider_final_seals_after_abort_only_for_previously_admitted_step() {
        let mut f = fixture(vec![]);
        let step = f
            .run
            .admit_provider_step(Arc::new(()))
            .expect("already admitted provider step");
        let close = f.owner.request_close(CloseReason::OrdinaryAbort);
        let actual = assistant(None);
        let terminal = step
            .seal_assistant(actual.clone())
            .expect("late legitimate native final");
        persist(&terminal, &actual);
        assert!(f.run.admit_provider_step(Arc::new(())).is_err());
        f.anchor.take().expect("anchor").finish_ordered_boundary();
        assert_eq!(close.status(), DrainStatus::Drained);
        assert!(!f.owner.projection_allowed());
    }
}
