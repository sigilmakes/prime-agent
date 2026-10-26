//! Typed canonical-owner checkpoints, exact rows, and staged write receipts.
//!
//! These types do not grant authority by themselves. A store binds its owner
//! token to its actual weak runtime owner, header, path, and lifetime lease.
//! File cache generations and SDK revisions are not owner authority.
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use pa_types::session::{AgentMessage, FileEntry};

use super::window::WindowedSessionStore;

/// Requested-write certainty is separate from source-write certainty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalRequestedWrite {
    NotRequested,
    NotWritten,
    Committed,
    Unknown,
}

/// A possible preflight write can require reconciliation even when the requested row was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalSourceDisposition {
    Unchanged,
    PendingAdoption,
    NeedsReconcile,
    /// The leased owner refused before starting this request and certifies no
    /// outstanding prepare, write, tool or adoption debt. Observe again before use.
    ReadyForObservation,
}

/// Typed refusal/failure without guessing whether an append happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalFailureKind {
    InvalidRequest,
    OwnerChanged,
    BranchChanged,
    SourceChanged,
    AuthorityClosed,
    Io,
    Cancelled,
    CallerAbandoned,
    AdoptionConflict,
}

/// Owner failure classification. Never convert this to retained-write success.
#[derive(Debug)]
pub struct CanonicalStoreError {
    pub kind: CanonicalFailureKind,
    pub requested: CanonicalRequestedWrite,
    pub source: CanonicalSourceDisposition,
    pub detail: String,
}
impl std::fmt::Display for CanonicalStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}
impl std::error::Error for CanonicalStoreError {}

/// One asynchronous canonical-owner operation.
pub type CanonicalFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, CanonicalStoreError>> + Send + 'static>>;

/// Identity of one actual owner epoch. Equality is allocation identity, not value equality.
#[derive(Clone)]
pub struct CanonicalOwnerScope(Arc<()>);

impl CanonicalOwnerScope {
    /// Issue a token for a new actual owner epoch, never for an SDK/cache generation.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl Default for CanonicalOwnerScope {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for CanonicalOwnerScope {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CanonicalOwnerScope {}
impl std::hash::Hash for CanonicalOwnerScope {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}
impl std::fmt::Debug for CanonicalOwnerScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CanonicalOwnerScope(..)")
    }
}

/// Identity of an owner-issued operation, not a canonical entry ID.
#[derive(Clone)]
pub struct CanonicalOperation(Arc<()>);
impl CanonicalOperation {
    /// Issue a token for an actual prepared owner operation.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
}
impl Default for CanonicalOperation {
    fn default() -> Self {
        Self::new()
    }
}
impl PartialEq for CanonicalOperation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CanonicalOperation {}
impl std::hash::Hash for CanonicalOperation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}
impl std::fmt::Debug for CanonicalOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CanonicalOperation(..)")
    }
}

/// Certified physical append frontier, independent of selected graph leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalFrontier {
    /// Physical byte offset, including every terminal newline.
    pub bytes: u64,
    /// Physical row count, including the header and unknown metadata rows.
    pub rows: u64,
    /// SHA-256 of ALL original physical bytes through this frontier.
    pub prefix_certificate: [u8; 32],
}

/// One captured canonical source and branch position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalCursor {
    pub owner: CanonicalOwnerScope,
    pub session_id: String,
    pub source_path: PathBuf,
    /// Physical source identity/generation certificate; NOT owner authority.
    pub raw_source_generation: [u8; 32],
    /// SHA-256 of the exact raw header row, including its newline.
    pub header_certificate: [u8; 32],
    pub canonical_revision: u64,
    pub branch_revision: u64,
    pub selected_leaf: Option<String>,
    pub frontier: CanonicalFrontier,
}

/// An exact physical JSONL row plus its immutable parsed interpretation.
/// Never construct this from a usage-folded/indexed entry or reserialize that entry.
#[derive(Clone)]
pub struct CanonicalJournalRow {
    raw: Arc<[u8]>,
    parsed: Arc<FileEntry>,
    byte_offset: u64,
    physical_row: u64,
}
impl std::fmt::Debug for CanonicalJournalRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalJournalRow")
            .field("id", &self.parsed.id())
            .field("bytes", &self.raw.len())
            .field("byte_offset", &self.byte_offset)
            .field("physical_row", &self.physical_row)
            .finish_non_exhaustive()
    }
}
impl CanonicalJournalRow {
    /// Parse one exact, newline-terminated physical journal row.
    ///
    /// # Errors
    /// Rejects empty, unterminated, multi-line, or invalid JSONL records.
    pub fn parse(raw: Arc<[u8]>, byte_offset: u64, physical_row: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            raw.last() == Some(&b'\n'),
            "canonical row has no terminal newline"
        );
        anyhow::ensure!(
            raw.len() > 1 && !raw[..raw.len() - 1].contains(&b'\n'),
            "canonical row is not one physical record"
        );
        let parsed = serde_json::from_slice(&raw)?;
        Ok(Self {
            raw,
            parsed: Arc::new(parsed),
            byte_offset,
            physical_row,
        })
    }
    /// Physical start offset; the header starts at byte zero.
    #[must_use]
    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }
    /// Physical row ordinal; the header is row zero.
    #[must_use]
    pub fn physical_row(&self) -> u64 {
        self.physical_row
    }
    /// Exact bytes written to the journal, including the terminal newline.
    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    /// Immutable source interpretation; derived usage must live elsewhere.
    #[must_use]
    pub fn entry(&self) -> &FileEntry {
        &self.parsed
    }
}

/// An owner-certified sparse raw view and its separate derived metadata/statistics.
pub struct CanonicalWindow {
    /// Certifies the full physical graph frontier, not only retained bodies.
    /// Lazy older raw reads must use the bound store and this cursor, not a derived loader.
    pub cursor: CanonicalCursor,
    /// Exact retained physical rows, excluding the separately supplied header.
    pub rows: Vec<CanonicalJournalRow>,
    /// A verified sparse window, not a claim that retained bodies are full history.
    pub derived: WindowedSessionStore,
}

/// Full physical graph or an explicitly sparse certified source.
pub enum CanonicalSource {
    /// Every physical non-header row, in journal order, including all branches.
    Full(Vec<CanonicalJournalRow>),
    /// Source authority remains bound to the owner's complete physical frontier.
    Window(Box<CanonicalWindow>),
}

/// A source checkpoint. The exact header appears once, not in source rows.
pub struct CanonicalCheckpoint {
    pub cursor: CanonicalCursor,
    pub header: CanonicalJournalRow,
    pub source: CanonicalSource,
}

/// Source bodies needed by the caller. `PreferWindow` must preserve sparse support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalSourceRequest {
    Full,
    PreferWindow,
}

/// Terminal persistence is intentionally distinct from abort-sensitive policy work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalMutationKind {
    /// Actual terminal rows from already admitted work. Ordinary abort does not erase them.
    AdmittedTerminal,
    /// Context/selection/archive policy work; abort and retirement must linearize with I/O.
    ContextPolicy,
    /// Admitted metadata/lifecycle work, under its specific owner authority.
    OwnerMetadata,
}

/// Whether a prepared source ticket may later submit one mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalPrepareMode {
    Observe,
    Mutation(CanonicalMutationKind),
}

/// Prepare confirms eligible pending usage before certifying the checkpoint.
/// The adapter observes/awaits reports outside its core mutex.
pub struct CanonicalPrepareRequest {
    /// Owned source/work admission for preflight; released after Prepared ACK,
    /// never retained across arbitrary Lua callbacks.
    pub permit: CanonicalWritePermit,
    pub expected: Option<CanonicalCursor>,
    pub source: CanonicalSourceRequest,
    pub mode: CanonicalPrepareMode,
    /// The owner validates this against its actual delivery ledger and captured
    /// owner/source/cursor/run/operation purpose before certifying preparation.
    /// Raw User membership alone is never delivery authorization. The approved
    /// exact policy is bound to the returned operation and its ACK/certificate.
    pub admission: CanonicalAdmissionPolicy,
}

/// Preparation may itself write rows. Adopt this checkpoint and ACK Prepared
/// before any auxiliary/main/source projection. The mutation ticket remains armed.
/// The owner operation certifies the exact requested admission policy. Core must
/// install only that approved policy, bound to this operation/cursor/run, and never
/// substitute another ID list merely because those User rows exist in raw history.
pub struct CanonicalPrepared {
    /// Unique caller half. Drop is synchronous abandonment, independent of permit ownership.
    pub caller: CanonicalCallerGuard,
    pub operation: CanonicalOperation,
    pub checkpoint: CanonicalCheckpoint,
}

/// Payload intent only. No ID, parent, timestamp, or reserved canonical row.
#[derive(Debug, Clone)]
pub struct CanonicalEntryDraft {
    kind: CanonicalMutationKind,
    entry: FileEntry,
}
impl CanonicalEntryDraft {
    /// Construct a noncanonical payload intent.
    ///
    /// # Errors
    /// Rejects header/unknown intents, user admissions, supplied envelope identity,
    /// and a compaction without context-policy authority.
    pub fn new(kind: CanonicalMutationKind, entry: FileEntry) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !matches!(entry, FileEntry::Header { .. } | FileEntry::Unknown { .. }),
            "unsupported canonical draft kind"
        );
        anyhow::ensure!(
            !matches!(
                entry,
                FileEntry::Message {
                    message: AgentMessage::User(_),
                    ..
                }
            ),
            "user rows require exact owner admission/replay"
        );
        let base = match &entry {
            FileEntry::Message { base, .. }
            | FileEntry::ThinkingLevelChange { base, .. }
            | FileEntry::ServiceTierChange { base, .. }
            | FileEntry::ModelChange { base, .. }
            | FileEntry::Compaction { base, .. }
            | FileEntry::BranchSummary { base, .. }
            | FileEntry::Custom { base, .. }
            | FileEntry::ChildUsageAttributed { base, .. }
            | FileEntry::Label { base, .. }
            | FileEntry::SessionInfo { base, .. }
            | FileEntry::SessionState { base, .. }
            | FileEntry::GitState { base, .. }
            | FileEntry::CustomMessage { base, .. } => base,
            FileEntry::Header { .. } | FileEntry::Unknown { .. } => unreachable!("rejected above"),
        };
        anyhow::ensure!(
            base.id.is_none() && base.parent_id.is_none() && base.timestamp.is_none(),
            "canonical draft supplied envelope identity"
        );
        anyhow::ensure!(
            ["type", "id", "parentId", "timestamp"]
                .iter()
                .all(|key| !base.rest.contains_key(*key)),
            "canonical draft supplied a reserved envelope key"
        );
        let payload_rest = match &entry {
            FileEntry::Custom { payload, .. } => Some(&payload.rest),
            FileEntry::CustomMessage { payload, .. } => Some(&payload.rest),
            _ => None,
        };
        anyhow::ensure!(
            payload_rest.is_none_or(|rest| ["type", "id", "parentId", "timestamp"]
                .iter()
                .all(|key| !rest.contains_key(*key))),
            "canonical draft payload supplied a reserved envelope key"
        );
        anyhow::ensure!(
            !matches!(entry, FileEntry::Compaction { .. })
                || kind == CanonicalMutationKind::ContextPolicy,
            "compaction requires context-policy authority"
        );
        if kind == CanonicalMutationKind::AdmittedTerminal {
            anyhow::ensure!(
                matches!(
                    entry,
                    FileEntry::Message {
                        message: AgentMessage::Assistant(_) | AgentMessage::ToolResult(_),
                        ..
                    }
                ),
                "terminal authority requires an actual admitted assistant/tool row"
            );
        }
        Ok(Self { kind, entry })
    }
    #[must_use]
    pub fn kind(&self) -> CanonicalMutationKind {
        self.kind
    }
    #[must_use]
    pub fn entry(&self) -> &FileEntry {
        &self.entry
    }
    #[must_use]
    pub fn into_entry(self) -> FileEntry {
        self.entry
    }
}

/// Actual canonical I/O stage. Preflight never consumes a terminal-message slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalIoStage {
    PreparePreflight,
    CommitPreflight,
    RequestedMutation,
}

/// An owned admitted I/O flight. The ACTUAL blocking writer owns this through
/// append/flush/fsync and recording its result, independently of the awaiter.
/// Dropping its underlying guard releases the flight's drain count.
pub struct CanonicalIoFlight {
    _guard: Box<dyn Send + Sync + 'static>,
}
impl CanonicalIoFlight {
    /// Wrap an actual RAII I/O-flight guard. A no-op guard is not production authority.
    #[must_use]
    pub fn new<T: Send + Sync + 'static>(guard: T) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

/// An OWNED transaction lease, not merely a check or generation Arc.
///
/// `enter_io` atomically validates/adopts a flight against the SAME short Closing
/// gates used by retirement/abort. All registry/selection/SDK/Agent state guards
/// are released before lease-fenced I/O. close COMPLETION/successor activation
/// waits for admitted I/O and transactions. A hung flight stays Closing/Blocked.
///
/// `AdmittedTerminal` binds a serving run and ONE already admitted provider-step
/// assistant slot or existing tool-call slot. Ordinary abort still permits exact
/// terminal cleanup; no arbitrary CustomMessage/new work is authorized by kind.
/// `ContextPolicy` denies new I/O flights after abort/retirement starts.
/// Implementation/wiring requires Root permit review.
pub trait CanonicalWriteAuthority: Send + Sync {
    /// Atomically admit one owned I/O flight for this exact scope/purpose/draft.
    /// `RequestedMutation` validates/consumes its once-only terminal slot, if any.
    ///
    /// # Errors
    /// Typed no-write/closed-scope refusal. A check without owned admission is invalid.
    fn enter_io(
        &self,
        stage: CanonicalIoStage,
        scope: &CanonicalCursor,
        operation: &CanonicalOperation,
        draft: Option<&CanonicalEntryDraft>,
    ) -> Result<CanonicalIoFlight, CanonicalStoreError>;
}

/// Unique owned transaction lease. Backend retains it through local adoption and ACK.
/// Caller-loss detection is separate and cannot be hidden by retained authority Arcs.
pub struct CanonicalWritePermit {
    kind: CanonicalMutationKind,
    authority: Arc<dyn CanonicalWriteAuthority>,
}
impl CanonicalWritePermit {
    /// Wrap an actual owned admission lease, not a check-only implementation.
    #[must_use]
    pub fn new(kind: CanonicalMutationKind, authority: Arc<dyn CanonicalWriteAuthority>) -> Self {
        Self { kind, authority }
    }
    #[must_use]
    pub fn kind(&self) -> CanonicalMutationKind {
        self.kind
    }
    /// Admit an actual independently owned I/O flight.
    ///
    /// # Errors
    /// Refuses a mismatched mutation kind or expired specific admission.
    pub fn enter_io(
        &self,
        stage: CanonicalIoStage,
        scope: &CanonicalCursor,
        operation: &CanonicalOperation,
        draft: Option<&CanonicalEntryDraft>,
    ) -> Result<CanonicalIoFlight, CanonicalStoreError> {
        if draft.is_some_and(|draft| draft.kind() != self.kind) {
            return Err(CanonicalStoreError {
                kind: CanonicalFailureKind::InvalidRequest,
                requested: CanonicalRequestedWrite::NotWritten,
                source: CanonicalSourceDisposition::Unchanged,
                detail: "write permit purpose differs from draft".to_owned(),
            });
        }
        self.authority.enter_io(stage, scope, operation, draft)
    }
}

/// Observable caller lifetime. Backend ACK state and caller completion are separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalCallerState {
    Active,
    PreparedAcknowledged,
    Completed,
    Abandoned,
}

/// Notification only: implementations must be nonblocking/nonpanicking and must
/// not enter Core/SDK/Agent/registry locks. Use weak/non-cyclic finalizer references.
pub trait CanonicalCallerNotifier: Send + Sync {
    fn changed(&self);
}

struct CallerStateInner {
    state: std::sync::atomic::AtomicU8,
    notify: Arc<dyn CanonicalCallerNotifier>,
}

/// Backend/task half. It does not keep the caller alive or mark an operation ready.
#[derive(Clone)]
pub struct CanonicalCallerSignal(Arc<CallerStateInner>);
impl CanonicalCallerSignal {
    #[must_use]
    pub fn state(&self) -> CanonicalCallerState {
        match self.0.state.load(std::sync::atomic::Ordering::Acquire) {
            1 => CanonicalCallerState::PreparedAcknowledged,
            2 => CanonicalCallerState::Completed,
            3 => CanonicalCallerState::Abandoned,
            _ => CanonicalCallerState::Active,
        }
    }
}

/// UNIQUE caller half; never stored/cloned by the backend. Prepare futures own it
/// before first poll and transfer it to Prepared on return. Drop always records
/// abandonment synchronously before notifying the owner-supervised finalizer.
/// Retained permit/token Arcs cannot hide caller loss. The finalizer waits actual
/// I/O completion, records exact outcome, then releases drain work while keeping
/// lost/unadopted source BLOCKED until explicit NEW-owner reconciliation.
/// An armed prepared ticket with proven NO outstanding/unadopted I/O can be
/// abandoned as known-not-written. No uncertain or active I/O may use that exception.
pub struct CanonicalCallerGuard {
    signal: CanonicalCallerSignal,
    armed: bool,
}
impl CanonicalCallerGuard {
    /// Create distinct caller/backend halves for an actual owner operation.
    #[must_use]
    pub fn new(notify: Arc<dyn CanonicalCallerNotifier>) -> (Self, CanonicalCallerSignal) {
        let signal = CanonicalCallerSignal(Arc::new(CallerStateInner {
            state: std::sync::atomic::AtomicU8::new(0),
            notify,
        }));
        (
            Self {
                signal: signal.clone(),
                armed: true,
            },
            signal,
        )
    }
    /// Record successful Mutation-mode Prepared ACK without disarming caller loss.
    /// Owner prepare readiness requires BOTH its matching ACK and this signal.
    /// The prepare permit releases; the armed ticket holds no authority/SDK lease
    /// across callbacks. Observe-mode Prepared ACK is terminal: call complete instead.
    ///
    /// # Errors
    /// Rejects duplicate or non-active preparation transitions.
    pub fn prepared_acknowledged(&mut self) -> Result<(), CanonicalStoreError> {
        self.signal
            .0
            .state
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| CanonicalStoreError {
                kind: CanonicalFailureKind::InvalidRequest,
                requested: CanonicalRequestedWrite::NotWritten,
                source: CanonicalSourceDisposition::PendingAdoption,
                detail: "caller preparation was not active".to_owned(),
            })?;
        self.signal.0.notify.changed();
        Ok(())
    }
    /// Consume ONLY after matching terminal ACK succeeds (or safe Abandon ACK).
    /// Final readiness requires BOTH terminal owner ACK and Completed. Prepared
    /// mutation readiness instead uses `PreparedAcknowledged`; it does not disarm this guard.
    pub fn complete(mut self) {
        self.signal
            .0
            .state
            .store(2, std::sync::atomic::Ordering::Release);
        self.armed = false;
        self.signal.0.notify.changed();
    }
}
impl Drop for CanonicalCallerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal
                .0
                .state
                .store(3, std::sync::atomic::Ordering::Release);
            self.signal.0.notify.changed();
        }
    }
}

/// Strict staged commit. No implicit preflight reparenting or callback retry.
pub struct CanonicalCommitRequest {
    pub operation: CanonicalOperation,
    pub expected: CanonicalCursor,
    pub source: CanonicalSourceRequest,
    pub draft: CanonicalEntryDraft,
    pub permit: CanonicalWritePermit,
}

/// Complete contiguous physical suffix since `from`, not an ID substitution.
pub struct CanonicalAppendReceipt {
    pub operation: CanonicalOperation,
    pub from: CanonicalCursor,
    pub to: CanonicalCursor,
    pub rows: Vec<CanonicalJournalRow>,
    /// Present exactly once for Committed; absent for `KnownNoRequestedWrite`.
    pub committed_entry_id: Option<String>,
    /// Fresh certified sparse overlay when an omitted attribution target needs it.
    pub replacement_window: Option<Box<CanonicalWindow>>,
}

/// A known refusal is not an uncertain write failure and must not retry callbacks.
pub enum CanonicalCommitDisposition {
    Committed(CanonicalAppendReceipt),
    /// Preflight advanced the source; requested row was definitely NOT written.
    /// Adopt+ACK this suffix, then stop this context-derived draft in v1.
    NeedsCatchUp(CanonicalAppendReceipt),
}

/// Prepared ACK clears checkpoint pending; Applied clears final receipt pending.
/// Abandon is legal only for a prepared ticket with no unadopted/uncertain write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalAckPhase {
    Prepared,
    /// Exact raw adoption AND permitted publication; never inferred from raw adoption alone.
    AppliedReady,
    /// Raw cleanup/accounting only while Closing. Never reopens old projection.
    ReconciledBlocked,
    /// Explicit quarantine/lost-publication resolution; never Ready.
    Quarantined,
    AbandonPreparedOnly,
}

/// Proof produced ONLY by successful whole-candidate no-I/O Core adoption.
/// Local revision/FIFO bookkeeping is not owner authority and cannot replace cursor checks.
#[derive(Debug, Clone)]
pub struct CanonicalAdoptionCertificate {
    pub(super) operation: CanonicalOperation,
    pub(super) cursor: CanonicalCursor,
    pub(super) local_revision: u64,
    pub(super) admission_fingerprint: [u8; 32],
}
impl CanonicalAdoptionCertificate {
    #[must_use]
    pub fn operation(&self) -> &CanonicalOperation {
        &self.operation
    }
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        &self.cursor
    }
    #[must_use]
    pub fn local_revision(&self) -> u64 {
        self.local_revision
    }
    #[must_use]
    pub fn admission_fingerprint(&self) -> &[u8; 32] {
        &self.admission_fingerprint
    }
}

/// ACK is bound to the exact operation, owner scope, and accepted cursor. No row is written.
pub struct CanonicalAcknowledgement {
    pub operation: CanonicalOperation,
    pub cursor: CanonicalCursor,
    pub phase: CanonicalAckPhase,
    /// Required for Prepared/Applied/ReconciledBlocked. Quarantine/Abandon cannot imply adoption.
    pub adoption: Option<CanonicalAdoptionCertificate>,
}

/// Explicit local replay policy. Ordinary suffix/checkpoint refresh preserves FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalAdmissionPolicy {
    Preserve,
    /// Newly actually owner-admitted/durably accepted User IDs, in delivery order.
    /// The caller must validate the owner/source/cursor/run/operation delivery
    /// binding, not infer admission from an ID merely existing in the raw graph.
    /// Install atomically with raw+view state; preserve the remaining FIFO exactly,
    /// then append these IDs. Reject duplicates, queued/consumed IDs, and historical
    /// replay through the per-binding seen ledger. This never writes User rows.
    /// Raw selected leaf and visible/admitted policy leaf remain distinct; queued
    /// but not actually delivered users cannot enter a policy/model projection.
    Append(Vec<String>),
    /// Only after strict startup/navigation proof and explicit binding/reset.
    /// Never use replacement to append later delivered users.
    Replace(Vec<String>),
}

/// Three-method canonical-owner seam. None/standalone SDK behavior is separate.
///
/// All methods bind the actual weak owner/header/path/lease, revisions, and poison.
/// Commit confirms pending usage THEN compares the exact source/branch cursor.
/// Any possible I/O failure poisons at the SHARED write seam, regardless of caller.
/// Return/lost reply does not clear committed-but-unadopted state. Retain permit
/// and pending state until the matching ACK or explicit new-owner reconciliation.
/// No replacement/pause/build/event-delivery gates may be acquired by the adapter.
pub trait CanonicalSessionStore: Send + Sync {
    fn prepare(&self, request: CanonicalPrepareRequest) -> CanonicalFuture<CanonicalPrepared>;
    fn commit(
        &self,
        request: CanonicalCommitRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition>;
    fn acknowledge(&self, request: CanonicalAcknowledgement) -> CanonicalFuture<()>;
}

/// Exact completion of one strict metadata write, for chaining only its own frontier.
pub struct CanonicalMetadataReceipt {
    pub entry_id: String,
    pub cursor: CanonicalCursor,
}

/// Async envelope-free metadata persistence for shared-manager hosts. A live
/// implementation routes to the actual owner and completes raw adoption + ACK.
/// Callers must release all SDK/goal/registry guards before awaiting this seam.
pub trait CanonicalMetadataWriter: Send + Sync {
    /// Certify and ACK the actual source before deriving a metadata policy intent.
    fn observe_metadata_source(self: Arc<Self>) -> CanonicalFuture<()>;
    fn append_metadata(self: Arc<Self>, entry: FileEntry) -> CanonicalFuture<String>;
    /// A precomputed context-derived intent may not silently rebase after prepare.
    fn append_metadata_from(
        self: Arc<Self>,
        entry: FileEntry,
        expected: CanonicalCursor,
    ) -> CanonicalFuture<CanonicalMetadataReceipt>;
}

/// Persist metadata through the live owner, or the standalone local manager.
/// The shared manager guard is always released before an owner future is polled.
///
/// # Errors
/// Returns owner, adoption, ACK, or standalone persistence failures.
pub async fn persist_metadata_entry(
    session: &Arc<tokio::sync::Mutex<super::manager::SessionManager>>,
    entry: FileEntry,
) -> anyhow::Result<String> {
    let writer = {
        let mut manager = session.lock().await;
        if let Some(writer) = manager.canonical_metadata_writer() {
            writer
        } else {
            return manager.append_metadata_intent(entry).map_err(Into::into);
        }
    };
    writer.append_metadata(entry).await.map_err(Into::into)
}

/// Persist one precomputed metadata intent against its exact captured source.
/// Source advance stops this draft; callers must not rerun paid policy work.
///
/// # Errors
/// Rejects any source change or incomplete owner adoption/ACK.
pub async fn persist_metadata_entry_from(
    session: &Arc<tokio::sync::Mutex<super::manager::SessionManager>>,
    entry: FileEntry,
    expected: CanonicalCursor,
) -> anyhow::Result<CanonicalMetadataReceipt> {
    let writer = {
        let manager = session.lock().await;
        anyhow::ensure!(
            manager.canonical_binding() == Some(&expected),
            "metadata source changed before owner preparation"
        );
        manager
            .canonical_metadata_writer()
            .ok_or_else(|| anyhow::anyhow!("strict metadata source has no live owner"))?
    };
    writer
        .append_metadata_from(entry, expected)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn custom() -> FileEntry {
        serde_json::from_value(serde_json::json!({
            "type":"custom", "customType":"test", "data":{"opaque":"AA=="}
        }))
        .expect("custom draft")
    }

    #[test]
    fn tokens_compare_and_hash_by_owner_allocation_not_unit_value() {
        use std::collections::HashSet;
        let owner = CanonicalOwnerScope::new();
        assert_eq!(owner, owner.clone());
        assert_ne!(owner, CanonicalOwnerScope::new());
        let mut owners = HashSet::new();
        assert!(owners.insert(owner.clone()));
        assert!(!owners.insert(owner));
        assert!(owners.insert(CanonicalOwnerScope::new()));
        let operation = CanonicalOperation::new();
        assert_eq!(operation, operation.clone());
        assert_ne!(operation, CanonicalOperation::new());
    }

    #[test]
    fn exact_row_keeps_physical_bytes_positions_and_unknown_maps() {
        let raw: Arc<[u8]> = Arc::from(&b"{ \"type\":\"custom\",\"id\":\"r\",\"parentId\":null,\"customType\":\"x\",\"data\":{\"native\":\"AA==\"},\"future\":{\"z\":[1,true]}}\n"[..]);
        let row = CanonicalJournalRow::parse(raw.clone(), 71, 4).expect("physical row");
        assert_eq!(row.raw(), raw.as_ref());
        assert_eq!(row.byte_offset(), 71);
        assert_eq!(row.physical_row(), 4);
        let value = serde_json::to_value(row.entry()).expect("lookup interpretation");
        assert_eq!(value["future"], serde_json::json!({"z":[1,true]}));
        let mut derived = row.entry().clone();
        if let FileEntry::Custom { payload, .. } = &mut derived {
            payload.data = Some(serde_json::json!({"folded":true}));
        }
        assert_eq!(row.raw(), raw.as_ref());
        assert_eq!(
            serde_json::to_value(row.entry()).expect("unchanged raw interpretation")["data"],
            serde_json::json!({"native":"AA=="})
        );
    }

    #[test]
    fn row_rejects_unterminated_empty_and_multiple_physical_records() {
        for raw in [&b"{}"[..], &b"\n"[..], &b"{}\n{}\n"[..]] {
            assert!(CanonicalJournalRow::parse(Arc::from(raw), 0, 0).is_err());
        }
    }

    #[test]
    fn draft_rejects_even_empty_envelope_values_and_reserved_flattened_keys() {
        for (key, value) in [("id", ""), ("parentId", ""), ("timestamp", "")] {
            let mut entry = serde_json::to_value(custom()).expect("value");
            entry[key] = serde_json::json!(value);
            assert!(CanonicalEntryDraft::new(
                CanonicalMutationKind::OwnerMetadata,
                serde_json::from_value(entry).expect("draft envelope")
            )
            .is_err());
        }
        let mut entry = custom();
        if let FileEntry::Custom { base, .. } = &mut entry {
            base.rest.insert("id".into(), serde_json::json!("hidden"));
        }
        assert!(CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, entry).is_err());
        let mut entry = custom();
        if let FileEntry::Custom { payload, .. } = &mut entry {
            payload
                .rest
                .insert("parentId".into(), serde_json::json!("hidden-parent"));
        }
        assert!(CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, entry).is_err());
        assert!(CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, custom()).is_ok());
    }

    #[test]
    fn terminal_kind_does_not_authorize_arbitrary_custom_notice() {
        assert!(
            CanonicalEntryDraft::new(CanonicalMutationKind::AdmittedTerminal, custom()).is_err()
        );
        let entry = serde_json::from_value(serde_json::json!({"type":"custom_message",
            "customType":"x","content":"notice","display":true,"timestamp":null}))
        .expect("custom message");
        assert!(CanonicalEntryDraft::new(CanonicalMutationKind::AdmittedTerminal, entry).is_err());
    }

    struct Notify(Arc<AtomicUsize>);
    impl CanonicalCallerNotifier for Notify {
        fn changed(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn caller_drop_records_abandoned_even_while_backend_signal_is_retained() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(notifications.clone())));
        let backend = signal.clone();
        assert_eq!(backend.state(), CanonicalCallerState::Active);
        drop(caller);
        assert_eq!(signal.state(), CanonicalCallerState::Abandoned);
        assert_eq!(backend.state(), CanonicalCallerState::Abandoned);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn completed_caller_does_not_later_abandon_on_drop() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify(notifications.clone())));
        caller.complete();
        assert_eq!(signal.state(), CanonicalCallerState::Completed);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn prepared_ack_keeps_caller_armed_without_terminal_completion() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let (mut caller, signal) =
            CanonicalCallerGuard::new(Arc::new(Notify(notifications.clone())));
        caller.prepared_acknowledged().expect("prepared ACK");
        assert_eq!(signal.state(), CanonicalCallerState::PreparedAcknowledged);
        assert!(caller.prepared_acknowledged().is_err());
        drop(caller);
        assert_eq!(signal.state(), CanonicalCallerState::Abandoned);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn terminal_completion_after_prepared_ack_is_distinct() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let (mut caller, signal) =
            CanonicalCallerGuard::new(Arc::new(Notify(notifications.clone())));
        caller.prepared_acknowledged().expect("prepared ACK");
        caller.complete();
        assert_eq!(signal.state(), CanonicalCallerState::Completed);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn draft_preserves_opaque_nested_payload_identity_fields() {
        let mut entry = custom();
        if let FileEntry::Custom { payload, .. } = &mut entry {
            payload.data = Some(
                serde_json::json!({"id":"payload-id","parentId":"nested-parent",
                "timestamp":"opaque-time","data":{"id":"nested-id","bytes":"AA=="}}),
            );
        }
        let expected = entry.clone();
        let draft = CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, entry)
            .expect("nested payload is not envelope");
        assert_eq!(draft.into_entry(), expected);
    }
    #[test]
    fn explicit_new_delivery_append_is_not_refresh_or_startup_reset() {
        let ids = vec!["owner-second".to_owned(), "owner-first".to_owned()];
        let append = CanonicalAdmissionPolicy::Append(ids.clone());
        assert_ne!(append, CanonicalAdmissionPolicy::Preserve);
        assert_ne!(append, CanonicalAdmissionPolicy::Replace(ids.clone()));
        let CanonicalAdmissionPolicy::Append(actual) = append else {
            panic!("explicit admission must not become refresh/reset");
        };
        assert_eq!(actual, ids);
    }
}
