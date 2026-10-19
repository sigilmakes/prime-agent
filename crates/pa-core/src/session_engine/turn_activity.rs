//! The engine's turn-activity signal for deferred plugin work.
//!
//! A deferred plugin job may only run at a genuine turn boundary. The agent's
//! run slot is per attempt, so a provider retry gap looks idle there, and a
//! compaction-driven retry (an overflow settle that compacts and re-issues the
//! turn) is idle for even longer. TS draws the same line: its auto-refine seam
//! returns before scheduling from an `agent_end` that initiated a retry
//! (`agent-session.ts:4740`) and never reviews while `isStreaming || isCompacting`
//! (`agent-session.ts:9318-9319`).
//!
//! The state is the ENGINE's, not a host's: [`TurnActivityState`] is owned by the
//! session and set from the session's own turn, compaction and retry scopes, so a
//! core-only embedder is protected without installing anything. [`TurnActivitySignal::install`]
//! stays as a host override for a caller that owns a wider scope than the engine
//! can see.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// The turn-scoped busy classes (the shape `scheduled_jobs` already models for
/// cron deferral: streaming, compacting, retrying).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnActivity {
    /// A turn is admitted and running, retries included.
    pub streaming: bool,
    /// A compaction owns the session.
    pub compacting: bool,
    /// A retry/failover episode is live.
    pub retrying: bool,
}

impl TurnActivity {
    /// Any busy class: a deferred job must wait for the turn boundary.
    #[must_use]
    pub fn busy(self) -> bool {
        self.streaming || self.compacting || self.retrying
    }
}

/// The host's probe: one snapshot per call, cheap and non-blocking.
pub type TurnActivityProbe = Arc<dyn Fn() -> TurnActivity + Send + Sync>;

/// The engine-owned activity counters. Depth counters, not booleans: the
/// daemon's turn loop wraps the core's own prompt scope, so a nested scope
/// must not clear the outer one.
#[derive(Debug, Default)]
pub struct TurnActivityState {
    turns: AtomicU32,
    compactions: AtomicU32,
    retries: AtomicU32,
}

impl TurnActivityState {
    fn turns(&self) -> &AtomicU32 {
        &self.turns
    }

    fn compactions(&self) -> &AtomicU32 {
        &self.compactions
    }

    fn retries(&self) -> &AtomicU32 {
        &self.retries
    }

    /// The current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> TurnActivity {
        TurnActivity {
            streaming: self.turns.load(Ordering::SeqCst) > 0,
            compacting: self.compactions.load(Ordering::SeqCst) > 0,
            retrying: self.retries.load(Ordering::SeqCst) > 0,
        }
    }

    /// Enter a turn scope; the guard clears it on drop.
    #[must_use]
    pub fn begin_turn(self: &Arc<Self>) -> TurnActivityGuard {
        TurnActivityGuard::enter(self, TurnActivityState::turns)
    }

    /// Enter a compaction scope; the guard clears it on drop.
    #[must_use]
    pub fn begin_compaction(self: &Arc<Self>) -> TurnActivityGuard {
        TurnActivityGuard::enter(self, TurnActivityState::compactions)
    }

    /// Enter a retry scope (a retry gap or a re-issued attempt); the guard
    /// clears it on drop.
    #[must_use]
    pub fn begin_retry(self: &Arc<Self>) -> TurnActivityGuard {
        TurnActivityGuard::enter(self, TurnActivityState::retries)
    }
}

/// One entered activity scope; dropping it leaves the scope.
#[derive(Debug)]
pub struct TurnActivityGuard {
    state: Arc<TurnActivityState>,
    counter: fn(&TurnActivityState) -> &AtomicU32,
}

impl TurnActivityGuard {
    fn enter(
        state: &Arc<TurnActivityState>,
        counter: fn(&TurnActivityState) -> &AtomicU32,
    ) -> Self {
        counter(state).fetch_add(1, Ordering::SeqCst);
        Self {
            state: Arc::clone(state),
            counter,
        }
    }
}

impl Drop for TurnActivityGuard {
    fn drop(&mut self) {
        (self.counter)(&self.state).fetch_sub(1, Ordering::SeqCst);
    }
}

/// The engine's activity state plus an optional host override.
pub struct TurnActivitySignal {
    state: Arc<TurnActivityState>,
    probe: Mutex<Option<TurnActivityProbe>>,
}

impl Default for TurnActivitySignal {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            probe: Mutex::new(None),
        }
    }
}

impl TurnActivitySignal {
    /// A signal with an empty engine state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine state every scope guard mutates.
    #[must_use]
    pub fn state(&self) -> Arc<TurnActivityState> {
        Arc::clone(&self.state)
    }

    /// Enter a turn scope on the engine state.
    #[must_use]
    pub fn begin_turn(&self) -> TurnActivityGuard {
        self.state.begin_turn()
    }

    /// Enter a compaction scope on the engine state.
    #[must_use]
    pub fn begin_compaction(&self) -> TurnActivityGuard {
        self.state.begin_compaction()
    }

    /// Enter a retry scope on the engine state.
    #[must_use]
    pub fn begin_retry(&self) -> TurnActivityGuard {
        self.state.begin_retry()
    }

    /// Install (or replace) a host probe that overrides the engine state.
    ///
    /// # Panics
    ///
    /// The slot's mutex is poisoned only by a panicking installer; recovery
    /// keeps the last installed probe usable.
    pub fn install(&self, probe: TurnActivityProbe) {
        *self.probe.lock().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    }

    /// The current snapshot: the host override when installed, else the
    /// engine's own state.
    ///
    /// # Panics
    ///
    /// The probe is called outside the slot lock, so a probe that reaches back
    /// into this session cannot deadlock here.
    #[must_use]
    pub fn activity(&self) -> TurnActivity {
        let probe = self
            .probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        probe.map_or_else(|| self.state.snapshot(), |probe| probe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_nest_and_clear_only_at_the_outer_drop() {
        let signal = TurnActivitySignal::new();
        assert!(!signal.activity().busy());
        let outer = signal.begin_turn();
        let inner = signal.begin_turn();
        assert!(signal.activity().streaming);
        drop(inner);
        assert!(signal.activity().streaming, "the outer turn scope holds");
        drop(outer);
        assert!(!signal.activity().busy());
    }

    #[test]
    fn compaction_and_retry_are_distinct_busy_classes() {
        let signal = TurnActivitySignal::new();
        let compaction = signal.begin_compaction();
        assert_eq!(
            signal.activity(),
            TurnActivity {
                streaming: false,
                compacting: true,
                retrying: false,
            }
        );
        drop(compaction);
        let retry = signal.begin_retry();
        assert_eq!(
            signal.activity(),
            TurnActivity {
                streaming: false,
                compacting: false,
                retrying: true,
            }
        );
        drop(retry);
        assert!(!signal.activity().busy());
    }

    #[test]
    fn a_host_probe_overrides_the_engine_state() {
        let signal = TurnActivitySignal::new();
        let _turn = signal.begin_turn();
        assert!(signal.activity().busy());
        signal.install(Arc::new(TurnActivity::default));
        assert!(!signal.activity().busy(), "the override wins");
    }
}
