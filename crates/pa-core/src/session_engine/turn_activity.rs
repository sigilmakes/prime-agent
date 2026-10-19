//! The host's turn-activity signal for deferred plugin work.
//!
//! A deferred plugin job may only run at a genuine turn boundary. The agent's
//! run slot is per attempt, so a provider retry gap looks idle there; a host
//! that owns the turn queue (the daemon worker) therefore installs a probe that
//! reports the turn-scoped busy classes. TS draws the same line: its auto-refine
//! seam returns before scheduling from an `agent_end` that initiated a retry
//! (`agent-session.ts:4740`) and never reviews while `isStreaming || isCompacting`
//! (`agent-session.ts:9318-9319`). A host without a turn queue leaves the slot
//! empty, which keeps the agent run slot as the whole turn (the prior behavior).

use std::sync::{Arc, Mutex, PoisonError};

/// The host's turn-scoped busy classes (the shape `scheduled_jobs` already
/// models for cron deferral: streaming, compacting, retrying).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnActivity {
    /// A turn is admitted and running, retries included.
    pub streaming: bool,
    /// A compaction owns the session.
    pub compacting: bool,
    /// A provider retry/failover episode is live.
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

/// The installed-probe slot shared by a session and its deferred-job registry.
#[derive(Default)]
pub struct TurnActivitySignal {
    probe: Mutex<Option<TurnActivityProbe>>,
}

impl TurnActivitySignal {
    /// An empty slot: no probe means the run slot is the whole turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the host's probe.
    ///
    /// # Panics
    ///
    /// The slot's mutex is poisoned only by a panicking installer; recovery
    /// keeps the last installed probe usable.
    pub fn install(&self, probe: TurnActivityProbe) {
        *self.probe.lock().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    }

    /// The current snapshot; `busy()` is false while no probe is installed.
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
        probe.map_or_else(TurnActivity::default, |probe| probe())
    }
}
