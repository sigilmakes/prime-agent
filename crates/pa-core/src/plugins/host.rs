use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    runtime::{self, Generation},
    Contribution, ContributionKind, ContributionSnapshot, HostCapabilities, Invocation,
    PluginError, PluginSource,
};

#[derive(Default)]
struct State {
    generations: BTreeMap<String, Arc<Generation>>,
    snapshot: Arc<ContributionSnapshot>,
}

/// Owns registrations and Lua state for exactly one session. No process-global registry.
/// Reconcile is atomic for the whole desired set. Loading happens outside registry locks;
/// concurrent reconciles use optimistic revision checks, never overwrite newer commits.
pub struct PluginHost {
    session: String,
    capabilities: Arc<dyn HostCapabilities>,
    reserved_commands: BTreeSet<String>,
    state: Mutex<State>,
}

impl PluginHost {
    /// Construct an empty session host. Reserve slash-command names without the `/`.
    /// The composition root supplies its complete command inventory. Reservations
    /// are not hard-coded here: optional plugin policy must be able to move out of core.
    #[must_use]
    pub fn new(
        session: String,
        capabilities: Arc<dyn HostCapabilities>,
        reserved_commands: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            session,
            capabilities,
            reserved_commands: reserved_commands.into_iter().collect(),
            state: Mutex::new(State::default()),
        }
    }

    /// Replace the desired enabled set. Unchanged source and config retain live state.
    /// Disabled sources lose every registration, but this host never deletes saved data.
    /// Activation is synchronous, bounded Lua initialization; it cannot call capabilities.
    ///
    /// # Errors
    /// Invalid names, duplicate/reserved registrations, initialization errors, or a
    /// concurrent reconcile leave the entire previous activation untouched.
    pub fn reconcile(
        &self,
        desired: Vec<PluginSource>,
    ) -> Result<Arc<ContributionSnapshot>, PluginError> {
        self.reconcile_validated(desired, |_| Ok(()))
    }

    /// Validate composition policy against the candidate before publishing or retiring
    /// anything. Validation must not mutate the host or run plugin callbacks.
    pub(crate) fn reconcile_validated(
        &self,
        desired: Vec<PluginSource>,
        validate: impl FnOnce(&ContributionSnapshot) -> Result<(), PluginError>,
    ) -> Result<Arc<ContributionSnapshot>, PluginError> {
        let (revision, previous) = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            (state.snapshot.revision, state.generations.clone())
        };
        let mut next = BTreeMap::new();
        for source in desired {
            super::names::validate_plugin_name(&source.name)?;
            if next.contains_key(&source.name) {
                return Err(PluginError::Invalid(format!(
                    "duplicate plugin {}",
                    source.name
                )));
            }
            let generation = match previous.get(&source.name) {
                Some(active)
                    if active.source == source.source && active.config == source.config =>
                {
                    Arc::clone(active)
                }
                Some(_) | None => Arc::new(Generation::load(&source)?),
            };
            next.insert(source.name, generation);
        }
        let mut contributions = Vec::new();
        let mut commands = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for generation in next.values() {
            for contribution in generation.contributions() {
                if contribution.kind == ContributionKind::Command {
                    if self.reserved_commands.contains(&contribution.name) {
                        return Err(PluginError::Invalid(format!(
                            "reserved command {}",
                            contribution.name
                        )));
                    }
                    if !commands.insert(contribution.name.clone()) {
                        return Err(PluginError::Invalid(format!(
                            "duplicate command {}",
                            contribution.name
                        )));
                    }
                }
                if contribution.kind == ContributionKind::Provider
                    && !providers.insert(contribution.name.clone())
                {
                    return Err(PluginError::Invalid(format!(
                        "duplicate provider {}",
                        contribution.name
                    )));
                }
                contributions.push(contribution.clone());
            }
        }
        let candidate = ContributionSnapshot {
            revision: revision + 1,
            contributions,
        };
        validate(&candidate)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.snapshot.revision != revision {
            return Err(PluginError::ReconcileConflict);
        }
        if next.len() == state.generations.len()
            && next.iter().all(|(name, generation)| {
                state
                    .generations
                    .get(name)
                    .is_some_and(|active| Arc::ptr_eq(active, generation))
            })
        {
            return Ok(Arc::clone(&state.snapshot));
        }
        // Includes empty-contribution plugins: their live state must still reconcile.
        let snapshot = Arc::new(candidate);
        for (name, generation) in &state.generations {
            if !next
                .get(name)
                .is_some_and(|new| Arc::ptr_eq(new, generation))
            {
                generation.retirement.cancel();
            }
        }
        state.generations = next;
        state.snapshot = Arc::clone(&snapshot);
        Ok(snapshot)
    }

    /// Commit intentional removals without evaluating unrelated replacements.
    /// A later invalid enabled source cannot keep a disabled generation active.
    pub(crate) fn retain_plugins(
        &self,
        wanted: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, PluginError> {
        for name in wanted {
            super::names::validate_plugin_name(name)?;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let removed: BTreeSet<_> = state
            .generations
            .keys()
            .filter(|name| !wanted.contains(*name))
            .cloned()
            .collect();
        if removed.is_empty() {
            return Ok(removed);
        }
        for name in &removed {
            if let Some(generation) = state.generations.remove(name) {
                generation.retirement.cancel();
            }
        }
        state.snapshot = Arc::new(ContributionSnapshot {
            revision: state.snapshot.revision + 1,
            contributions: state
                .snapshot
                .contributions
                .iter()
                .filter(|target| !removed.contains(&target.plugin))
                .cloned()
                .collect(),
        });
        Ok(removed)
    }

    /// Return the immutable active inventory without loading or running plugin code.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ContributionSnapshot> {
        Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .snapshot,
        )
    }

    /// Hold generation authority through a synchronous commit. Callers must keep the
    /// closure short, never await, and never re-enter this host from the closure.
    pub(crate) fn with_current_contributions<T>(
        &self,
        targets: &[&Contribution],
        commit: impl FnOnce() -> T,
    ) -> Result<T, PluginError> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !targets
            .iter()
            .all(|target| state.snapshot.contributions.contains(target))
        {
            return Err(PluginError::Missing);
        }
        Ok(commit())
    }

    pub(crate) fn retirement(
        &self,
        target: &Contribution,
    ) -> Result<CancellationToken, PluginError> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.snapshot.contributions.contains(target) {
            return Err(PluginError::Missing);
        }
        Ok(state
            .generations
            .get(&target.plugin)
            .ok_or(PluginError::Missing)?
            .retirement
            .clone())
    }

    /// Invoke exactly the generation described by a snapshot contribution. No stale
    /// descriptor is silently redirected. Both modes use the same event consumer.
    ///
    /// # Errors
    /// Returns `Missing` for an altered, foreign, disabled or stale descriptor, and
    /// `NoRuntime` outside Tokio. Callback failures arrive as terminal events.
    pub fn invoke(
        &self,
        contribution: &Contribution,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<Invocation, PluginError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| PluginError::NoRuntime)?;
        let generation = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.snapshot.contributions.contains(contribution) {
                return Err(PluginError::Missing);
            }
            Arc::clone(
                state
                    .generations
                    .get(&contribution.plugin)
                    .ok_or(PluginError::Missing)?,
            )
        };
        let cancellation = cancellation.child_token();
        let (sender, receiver) = mpsc::channel(16);
        handle.spawn(runtime::run(
            Arc::clone(&generation),
            contribution.clone(),
            input,
            self.session.clone(),
            Arc::clone(&self.capabilities),
            cancellation.clone(),
            sender,
        ));
        Ok(Invocation {
            receiver,
            cancellation,
            retirement: generation.retirement.clone(),
            _generation: generation,
            finished: false,
        })
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        for generation in self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .generations
            .values()
        {
            generation.retirement.cancel();
        }
    }
}
