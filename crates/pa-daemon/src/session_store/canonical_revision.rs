//! Physical-owner revision and uncertainty tracking. No file I/O or authority callbacks.
//!
//! Attach one clone-shared tracker to each actual `SessionFile` owner epoch. The adapter
//! must verify its weak lease, header and path before exposing owner certificates.
//! Move the owned attempt into the actual blocking writer, not its awaiting task.

#![allow(dead_code)] // Temporary; removed when the writer seam consumes this API.

use std::sync::{Arc, Mutex, MutexGuard};

use pa_core::session::canonical::{CanonicalOperation, CanonicalOwnerScope};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevisionError {
    Poisoned,
    Invalidated,
    Busy,
    BarrierMismatch,
    BarrierAlreadySet,
    NoBarrier,
    StaleBarrier,
    Overflow,
    InvalidAttempt,
    MutexPoisoned,
}

type Result<T> = std::result::Result<T, RevisionError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevisionSnapshot {
    pub owner: CanonicalOwnerScope,
    pub canonical_revision: u64,
    pub branch_revision: u64,
}

#[derive(Debug)]
struct State {
    canonical_revision: u64,
    branch_revision: u64,
    poisoned: bool,
    invalidated: bool,
    active_attempts: usize,
    active_io: usize,
    barrier: Option<BarrierTicket>,
}

impl State {
    fn available(&self) -> Result<()> {
        if self.poisoned {
            Err(RevisionError::Poisoned)
        } else if self.invalidated {
            Err(RevisionError::Invalidated)
        } else {
            Ok(())
        }
    }

    fn permits(&self, operation: Option<&CanonicalOperation>) -> Result<()> {
        self.available()?;
        if self
            .barrier
            .as_ref()
            .is_some_and(|expected| Some(&expected.operation) != operation)
        {
            return Err(RevisionError::BarrierMismatch);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Shared {
    owner: CanonicalOwnerScope,
    state: Mutex<State>,
    changed: Notify,
}

impl Shared {
    // Never recover a poisoned mutex as usable state. Even cleanup leaves it poisoned.
    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| RevisionError::MutexPoisoned)
    }

    fn snapshot(&self, state: &State) -> RevisionSnapshot {
        RevisionSnapshot {
            owner: self.owner.clone(),
            canonical_revision: state.canonical_revision,
            branch_revision: state.branch_revision,
        }
    }
}

/// Opaque incarnation of one operation-bound physical barrier. Keeping this
/// ticket alone does not arm a write or retain actor authority.
#[derive(Debug, Clone)]
pub(crate) struct BarrierTicket {
    operation: CanonicalOperation,
    instance: Arc<()>,
}

impl PartialEq for BarrierTicket {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation && Arc::ptr_eq(&self.instance, &other.instance)
    }
}
impl Eq for BarrierTicket {}

/// Clone-shared state for exactly one actual owner lifetime. No reset-poison API.
#[derive(Debug, Clone)]
pub(crate) struct CanonicalRevision {
    shared: Arc<Shared>,
}

impl CanonicalRevision {
    /// Only create for a new actual owner scope, never for a cache reload.
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                owner: CanonicalOwnerScope::new(),
                state: Mutex::new(State {
                    canonical_revision: 0,
                    branch_revision: 0,
                    poisoned: false,
                    invalidated: false,
                    active_attempts: 0,
                    active_io: 0,
                    barrier: None,
                }),
                changed: Notify::new(),
            }),
        }
    }

    /// Refuse certification while a physical attempt is owned, including before I/O.
    pub(crate) fn snapshot(&self) -> Result<RevisionSnapshot> {
        let state = self.shared.lock()?;
        state.available()?;
        if state.active_attempts != 0 {
            return Err(RevisionError::Busy);
        }
        Ok(self.shared.snapshot(&state))
    }

    /// Short admission only. The caller must move the guard into the physical writer.
    /// Serial admission reserves exactly one checked revision step before any I/O.
    pub(crate) fn admit_write(
        &self,
        operation: Option<&CanonicalOperation>,
    ) -> Result<OwnedWriteAttempt> {
        let mut state = self.shared.lock()?;
        state.permits(operation)?;
        if state.active_attempts != 0 {
            return Err(RevisionError::Busy);
        }
        state
            .canonical_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        state.active_attempts = 1;
        Ok(OwnedWriteAttempt {
            shared: self.shared.clone(),
            operation: operation.cloned(),
            started: false,
            finished: false,
        })
    }

    /// May close admission while an existing flight drains. It cannot undo I/O
    /// already started. Parent authority gates must prevent new matching flights.
    pub(crate) fn set_barrier(&self, operation: &CanonicalOperation) -> Result<BarrierTicket> {
        let mut state = self.shared.lock()?;
        state.available()?;
        if state.barrier.is_some() {
            return Err(RevisionError::BarrierAlreadySet);
        }
        let ticket = BarrierTicket {
            operation: operation.clone(),
            instance: Arc::new(()),
        };
        state.barrier = Some(ticket.clone());
        Ok(ticket)
    }

    /// Release only after matching `PreparedACK`. This does not disarm the actor's
    /// unique caller guard and cannot clear physical uncertainty or invalidation.
    /// A fresh ticket distinguishes Prepare and Commit barriers for the same op.
    pub(crate) fn clear_barrier(
        &self,
        operation: &CanonicalOperation,
        ticket: &BarrierTicket,
    ) -> Result<()> {
        let mut state = self.shared.lock()?;
        state.available()?;
        let expected = state.barrier.as_ref().ok_or(RevisionError::NoBarrier)?;
        if &expected.operation != operation || &ticket.operation != operation {
            return Err(RevisionError::BarrierMismatch);
        }
        if !Arc::ptr_eq(&expected.instance, &ticket.instance) {
            return Err(RevisionError::StaleBarrier);
        }
        if state.active_attempts != 0 {
            return Err(RevisionError::Busy);
        }
        state.barrier = None;
        Ok(())
    }

    /// Preflight a ledger transaction before arming any pending receipt.
    pub(crate) fn check_owner_state_advance(&self) -> Result<()> {
        let state = self.shared.lock()?;
        state.permits(None)?;
        if state.active_attempts != 0 || state.active_io != 0 {
            return Err(RevisionError::Busy);
        }
        state
            .canonical_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        Ok(())
    }

    /// Advance an accepted owner ledger transaction, without pretending to do I/O.
    /// Check every refusal before the caller changes its owner state under its lock.
    pub(crate) fn advance_owner_state(
        &self,
        operation: Option<&CanonicalOperation>,
    ) -> Result<RevisionSnapshot> {
        let mut state = self.shared.lock()?;
        state.permits(operation)?;
        if state.active_attempts != 0 || state.active_io != 0 {
            return Err(RevisionError::Busy);
        }
        let next = state
            .canonical_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        state.canonical_revision = next;
        Ok(self.shared.snapshot(&state))
    }

    /// Explicit navigation/branch ABA changes both revisions, even without bytes.
    /// No navigation is allowed through a pending physical barrier.
    pub(crate) fn advance_branch(&self) -> Result<RevisionSnapshot> {
        let mut state = self.shared.lock()?;
        state.available()?;
        if state.barrier.is_some() {
            return Err(RevisionError::BarrierMismatch);
        }
        if state.active_attempts != 0 {
            return Err(RevisionError::Busy);
        }
        let canonical = state
            .canonical_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        let branch = state
            .branch_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        state.canonical_revision = canonical;
        state.branch_revision = branch;
        Ok(self.shared.snapshot(&state))
    }

    /// Header, path, store replacement or lease loss permanently closes this scope.
    pub(crate) fn invalidate(&self) -> Result<()> {
        let result = self.shared.lock().map(|mut state| state.invalidated = true);
        self.shared.changed.notify_waiters();
        result
    }

    pub(crate) fn active_io(&self) -> Result<usize> {
        Ok(self.shared.lock()?.active_io)
    }

    /// Wait outside all state locks. Cancellation does not release an owned flight.
    /// This is a drain observation, NOT an authority gate against successor writes.
    pub(crate) async fn drain(&self) -> Result<RevisionSnapshot> {
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.shared.lock()?;
                if state.active_attempts == 0 {
                    state.available()?;
                    return Ok(self.shared.snapshot(&state));
                }
            }
            notified.await;
        }
    }
}

/// Owned physical attempt, deliberately not `Clone`. Drop before `begin_io` is a
/// known no-I/O cancellation; drop/panic/error after `begin_io` is permanent poison.
#[derive(Debug)]
pub(crate) struct OwnedWriteAttempt {
    shared: Arc<Shared>,
    operation: Option<CanonicalOperation>,
    started: bool,
    finished: bool,
}

impl OwnedWriteAttempt {
    /// Call immediately BEFORE the first potentially mutating syscall. No locks
    /// remain held when this returns. From here cancellation is unknown, not no-I/O.
    pub(crate) fn begin_io(&mut self) -> Result<()> {
        if self.started || self.finished {
            return Err(RevisionError::InvalidAttempt);
        }
        let mut state = self.shared.lock()?;
        state.permits(self.operation.as_ref())?;
        state
            .canonical_revision
            .checked_add(1)
            .ok_or(RevisionError::Overflow)?;
        if state.active_attempts != 1 || state.active_io != 0 {
            state.poisoned = true;
            return Err(RevisionError::Poisoned);
        }
        state.active_io = 1;
        self.started = true;
        Ok(())
    }

    /// Call exactly once only when all physical writes belonging to this attempt
    /// succeeded. Each physical attempt advances canonical revision once.
    pub(crate) fn finish_success(mut self) -> Result<RevisionSnapshot> {
        if !self.started {
            return Err(RevisionError::InvalidAttempt);
        }
        let shared = self.shared.clone();
        let mut state = shared.lock()?;
        let result = if let Some(next) = state.canonical_revision.checked_add(1) {
            state.canonical_revision = next;
            state.available().map(|()| shared.snapshot(&state))
        } else {
            state.poisoned = true;
            Err(RevisionError::Poisoned)
        };
        state.active_io = 0;
        state.active_attempts = 0;
        self.finished = true;
        drop(state);
        shared.changed.notify_waiters();
        result
    }

    /// Explicit physical error; dropping after `begin_io` has the same semantics.
    pub(crate) fn finish_error(self) {
        drop(self);
    }
}

impl Drop for OwnedWriteAttempt {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // On mutex poison cleanup may release counters, but NEVER rehabilitates
        // the mutex or uncertainty. lock() continues failing closed forever.
        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.started {
            state.poisoned = true;
            state.active_io = 0;
        }
        state.active_attempts = 0;
        drop(state);
        self.shared.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn ledger_advance_has_no_io_and_preserves_epoch_scope() {
        let tracker = CanonicalRevision::new();
        let initial = tracker.snapshot().unwrap();
        for revision in 1..=4 {
            tracker.check_owner_state_advance().unwrap();
            let next = tracker.advance_owner_state(None).unwrap();
            assert_eq!(next.owner, initial.owner);
            assert_eq!(next.branch_revision, initial.branch_revision);
            assert_eq!(next.canonical_revision, revision);
            assert_eq!(tracker.active_io(), Ok(0));
        }
        let branch = tracker.advance_branch().unwrap();
        assert_eq!(branch.canonical_revision, 5);
        assert_eq!(branch.branch_revision, 1);
        let mut write = tracker.admit_write(None).unwrap();
        write.begin_io().unwrap();
        let written = write.finish_success().unwrap();
        assert_eq!(written.canonical_revision, 6);
        assert_eq!(written.branch_revision, 1);
    }

    #[test]
    fn ledger_advance_refuses_active_io_barrier_overflow_and_poison_before_effects() {
        let tracker = CanonicalRevision::new();
        let initial = tracker.snapshot().unwrap();
        let operation = CanonicalOperation::new();
        let ticket = tracker.set_barrier(&operation).unwrap();
        assert_eq!(
            tracker.advance_owner_state(None),
            Err(RevisionError::BarrierMismatch)
        );
        assert_eq!(tracker.snapshot(), Ok(initial.clone()));
        tracker.clear_barrier(&operation, &ticket).unwrap();
        let mut attempt = tracker.admit_write(None).unwrap();
        assert_eq!(tracker.advance_owner_state(None), Err(RevisionError::Busy));
        attempt.begin_io().unwrap();
        assert_eq!(tracker.advance_owner_state(None), Err(RevisionError::Busy));
        drop(attempt);
        assert_eq!(
            tracker.advance_owner_state(None),
            Err(RevisionError::Poisoned)
        );
        let tracker = CanonicalRevision::new();
        tracker.shared.lock().unwrap().canonical_revision = u64::MAX;
        let before = tracker.snapshot().unwrap();
        assert_eq!(
            tracker.check_owner_state_advance(),
            Err(RevisionError::Overflow)
        );
        assert_eq!(
            tracker.advance_owner_state(None),
            Err(RevisionError::Overflow)
        );
        assert_eq!(tracker.snapshot(), Ok(before));
    }

    #[test]
    fn successful_write_advances_exactly_once_without_branch_change() {
        let tracker = CanonicalRevision::new();
        let original = tracker.snapshot().unwrap();
        let mut attempt = tracker.admit_write(None).unwrap();
        assert_eq!(tracker.active_io(), Ok(0));
        assert_eq!(tracker.snapshot(), Err(RevisionError::Busy));
        attempt.begin_io().unwrap();
        assert_eq!(tracker.active_io(), Ok(1));
        let done = attempt.finish_success().unwrap();
        assert_eq!(done.owner, original.owner);
        assert_eq!(done.canonical_revision, 1);
        assert_eq!(done.branch_revision, 0);
        assert_eq!(tracker.snapshot(), Ok(done));
        assert_eq!(tracker.active_io(), Ok(0));
    }

    #[test]
    fn known_no_io_drop_does_not_advance_or_poison() {
        let tracker = CanonicalRevision::new();
        let before = tracker.snapshot().unwrap();
        drop(tracker.admit_write(None).unwrap());
        assert_eq!(tracker.snapshot(), Ok(before));
        assert!(tracker.admit_write(None).is_ok());
    }

    #[test]
    fn success_before_io_is_rejected_and_cleans_up_without_poison() {
        let tracker = CanonicalRevision::new();
        assert_eq!(
            tracker.admit_write(None).unwrap().finish_success(),
            Err(RevisionError::InvalidAttempt)
        );
        assert_eq!(tracker.snapshot().unwrap().canonical_revision, 0);
    }

    #[test]
    fn error_and_drop_after_start_permanently_poison() {
        for explicit_error in [false, true] {
            let tracker = CanonicalRevision::new();
            let op = CanonicalOperation::new();
            let op_ticket = tracker.set_barrier(&op).unwrap();
            let mut attempt = tracker.admit_write(Some(&op)).unwrap();
            attempt.begin_io().unwrap();
            if explicit_error {
                attempt.finish_error();
            } else {
                drop(attempt);
            }
            assert_eq!(tracker.snapshot(), Err(RevisionError::Poisoned));
            assert_eq!(
                tracker.clear_barrier(&op, &op_ticket),
                Err(RevisionError::Poisoned)
            );
            assert_eq!(tracker.advance_branch(), Err(RevisionError::Poisoned));
            assert_eq!(
                tracker.admit_write(None).unwrap_err(),
                RevisionError::Poisoned
            );
            assert_eq!(tracker.active_io(), Ok(0));
        }
    }

    #[test]
    fn panic_in_actual_writer_poisons_without_poisoning_state_mutex() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        let thread = std::thread::spawn(move || {
            attempt.begin_io().unwrap();
            panic!("actual writer panicked");
        });
        assert!(thread.join().is_err());
        assert_eq!(tracker.snapshot(), Err(RevisionError::Poisoned));
    }

    #[test]
    fn opaque_owner_epoch_is_not_cache_revision_identity() {
        let first = CanonicalRevision::new();
        let second = CanonicalRevision::new();
        let first_snapshot = first.snapshot().unwrap();
        assert_eq!(first_snapshot, first.snapshot().unwrap());
        assert_ne!(first_snapshot.owner, second.snapshot().unwrap().owner);
    }

    #[test]
    fn barrier_rejects_wrong_none_and_late_tokens() {
        let tracker = CanonicalRevision::new();
        let op = CanonicalOperation::new();
        let wrong = CanonicalOperation::new();
        let op_ticket = tracker.set_barrier(&op).unwrap();
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::BarrierMismatch
        );
        assert_eq!(
            tracker.admit_write(Some(&wrong)).unwrap_err(),
            RevisionError::BarrierMismatch
        );
        assert_eq!(
            tracker.clear_barrier(&wrong, &op_ticket),
            Err(RevisionError::BarrierMismatch)
        );
        assert_eq!(
            tracker.advance_branch(),
            Err(RevisionError::BarrierMismatch)
        );
        assert_eq!(
            tracker.set_barrier(&wrong),
            Err(RevisionError::BarrierAlreadySet)
        );
        let mut attempt = tracker.admit_write(Some(&op)).unwrap();
        attempt.begin_io().unwrap();
        assert_eq!(
            tracker.clear_barrier(&op, &op_ticket),
            Err(RevisionError::Busy)
        );
        attempt.finish_success().unwrap();
        tracker.clear_barrier(&op, &op_ticket).unwrap();
        assert_eq!(
            tracker.clear_barrier(&op, &op_ticket),
            Err(RevisionError::NoBarrier)
        );
        let second_ticket = tracker.set_barrier(&op).unwrap();
        assert_eq!(
            tracker.clear_barrier(&op, &op_ticket),
            Err(RevisionError::StaleBarrier)
        );
        tracker.clear_barrier(&op, &second_ticket).unwrap();
        drop(tracker.admit_write(Some(&op)).unwrap());
        let wrong_ticket = tracker.set_barrier(&wrong).unwrap();
        assert_eq!(
            tracker.clear_barrier(&op, &op_ticket),
            Err(RevisionError::BarrierMismatch)
        );
        tracker.clear_barrier(&wrong, &wrong_ticket).unwrap();
        assert!(tracker.admit_write(None).is_ok());
    }

    #[test]
    fn barrier_inserted_after_admission_is_rechecked_before_io() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        let op = CanonicalOperation::new();
        let op_ticket = tracker.set_barrier(&op).unwrap();
        assert_eq!(attempt.begin_io(), Err(RevisionError::BarrierMismatch));
        drop(attempt);
        assert_eq!(tracker.snapshot().unwrap().canonical_revision, 0);
        tracker.clear_barrier(&op, &op_ticket).unwrap();
    }

    #[test]
    fn branch_aba_bumps_both_revisions_without_bytes() {
        let tracker = CanonicalRevision::new();
        let a = tracker.advance_branch().unwrap();
        let b = tracker.advance_branch().unwrap();
        assert_eq!((a.canonical_revision, a.branch_revision), (1, 1));
        assert_eq!((b.canonical_revision, b.branch_revision), (2, 2));
    }

    #[test]
    fn checked_overflow_refuses_before_io_and_navigation_is_atomic() {
        let tracker = CanonicalRevision::new();
        tracker.shared.state.lock().unwrap().canonical_revision = u64::MAX;
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::Overflow
        );
        assert_eq!(tracker.active_io(), Ok(0));
        assert_eq!(tracker.advance_branch(), Err(RevisionError::Overflow));
        let mut state = tracker.shared.state.lock().unwrap();
        state.canonical_revision = 12;
        state.branch_revision = u64::MAX;
        drop(state);
        assert_eq!(tracker.advance_branch(), Err(RevisionError::Overflow));
        assert_eq!(tracker.snapshot().unwrap().canonical_revision, 12);
    }

    #[test]
    fn final_available_revision_is_reserved_and_used_once() {
        let tracker = CanonicalRevision::new();
        tracker.shared.state.lock().unwrap().canonical_revision = u64::MAX - 1;
        let mut attempt = tracker.admit_write(None).unwrap();
        assert_eq!(tracker.admit_write(None).unwrap_err(), RevisionError::Busy);
        assert_eq!(tracker.advance_branch(), Err(RevisionError::Busy));
        attempt.begin_io().unwrap();
        assert_eq!(
            attempt.finish_success().unwrap().canonical_revision,
            u64::MAX
        );
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::Overflow
        );
    }

    #[test]
    fn impossible_post_io_overflow_poisons_fail_closed() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        attempt.begin_io().unwrap();
        // Only a test can violate the reserved-step invariant.
        tracker.shared.state.lock().unwrap().canonical_revision = u64::MAX;
        assert_eq!(attempt.finish_success(), Err(RevisionError::Poisoned));
        assert_eq!(tracker.snapshot(), Err(RevisionError::Poisoned));
    }

    #[test]
    fn invalidation_closes_old_scope_even_during_physical_flight() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        attempt.begin_io().unwrap();
        tracker.invalidate().unwrap();
        assert_eq!(attempt.finish_success(), Err(RevisionError::Invalidated));
        assert_eq!(tracker.shared.state.lock().unwrap().canonical_revision, 1);
        assert_eq!(tracker.snapshot(), Err(RevisionError::Invalidated));
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::Invalidated
        );
        assert_eq!(tracker.advance_branch(), Err(RevisionError::Invalidated));
    }

    #[test]
    fn poisoned_std_mutex_is_never_recovered_as_success() {
        let tracker = CanonicalRevision::new();
        let other = tracker.clone();
        assert!(std::thread::spawn(move || {
            let _state = other.shared.state.lock().unwrap();
            panic!("state invariant panic");
        })
        .join()
        .is_err());
        assert_eq!(tracker.snapshot(), Err(RevisionError::MutexPoisoned));
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::MutexPoisoned
        );
        assert_eq!(tracker.invalidate(), Err(RevisionError::MutexPoisoned));
    }

    #[test]
    fn poisoned_mutex_cleanup_never_clears_poison() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        attempt.begin_io().unwrap();
        let other = tracker.clone();
        assert!(std::thread::spawn(move || {
            let _state = other.shared.state.lock().unwrap();
            panic!("state invariant panic");
        })
        .join()
        .is_err());
        drop(attempt);
        assert_eq!(tracker.snapshot(), Err(RevisionError::MutexPoisoned));
        assert!(tracker.shared.state.is_poisoned());
    }

    #[tokio::test]
    async fn caller_loss_keeps_real_writer_owned_and_barrier_blocks_successor() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let waiter = tokio::task::spawn_blocking(move || {
            attempt.begin_io().unwrap();
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            attempt.finish_success()
        });
        started_rx.await.unwrap();
        // Dropping the async caller's JoinHandle does not stop the blocking writer.
        drop(waiter);
        let op = CanonicalOperation::new();
        let op_ticket = tracker.set_barrier(&op).unwrap();
        assert_eq!(tracker.active_io(), Ok(1));
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::BarrierMismatch
        );
        let drain = tracker.drain();
        tokio::pin!(drain);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), drain.as_mut())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.canonical_revision, 1);
        assert_eq!(tracker.active_io(), Ok(0));
        assert_eq!(
            tracker.admit_write(None).unwrap_err(),
            RevisionError::BarrierMismatch
        );
        tracker.clear_barrier(&op, &op_ticket).unwrap();
    }

    #[tokio::test]
    async fn held_unknown_flight_drains_to_failure_never_success() {
        let tracker = CanonicalRevision::new();
        let mut attempt = tracker.admit_write(None).unwrap();
        attempt.begin_io().unwrap();
        let op = CanonicalOperation::new();
        let op_ticket = tracker.set_barrier(&op).unwrap();
        let drain = tracker.drain();
        tokio::pin!(drain);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), drain.as_mut())
                .await
                .is_err()
        );
        drop(attempt);
        assert_eq!(drain.await, Err(RevisionError::Poisoned));
        assert_eq!(
            tracker.clear_barrier(&op, &op_ticket),
            Err(RevisionError::Poisoned)
        );
        assert_eq!(
            tracker.admit_write(Some(&op)).unwrap_err(),
            RevisionError::Poisoned
        );
    }
}
