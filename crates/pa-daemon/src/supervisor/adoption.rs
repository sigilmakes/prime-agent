//! Worker adoption: the boot discovery pass and the per-worker
//! adoption outcomes.

use std::sync::Arc;

use super::{
    anyhow, json, load_descriptors, response_failure, response_success, socket,
    worker_connect_deadline, Context, DaemonCommand, DaemonResponse, DaemonWorkerLifecycle,
    Duration, Ordering, Path, PathBuf, ResidentWorker, Result, Supervisor, Value,
    WorkerRegistration,
};

/// An unobservable start identity does not prove that a live pid was recycled.
/// Only an observed mismatch proves the descriptor's process is gone.
fn recorded_process_alive(
    alive: Option<bool>,
    expected: Option<&str>,
    observed: Option<&str>,
) -> bool {
    alive != Some(false) && (expected.is_none() || observed.is_none() || expected == observed)
}

#[cfg(test)]
mod recorded_process_tests {
    use super::recorded_process_alive;

    #[test]
    fn unobservable_start_id_keeps_live_tombstone_owned_by_recorded_pid() {
        assert!(recorded_process_alive(Some(true), Some("original"), None));
        assert!(recorded_process_alive(None, Some("original"), None));
        assert!(recorded_process_alive(Some(true), None, None));
        assert!(recorded_process_alive(
            Some(true),
            Some("original"),
            Some("original")
        ));
        assert!(!recorded_process_alive(
            Some(true),
            Some("original"),
            Some("recycled")
        ));
        assert!(!recorded_process_alive(Some(false), Some("original"), None));
    }
}

/// The boot the descriptor-adoption pass runs under. An update boot relaunches kept
/// workers before the roster restore walks the rows (spec §6 step 2's create-or-adopt
/// order). A plain startup revives only genuinely interrupted ones — no mass-revival
/// of historical idle sessions.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum AdoptionBoot {
    /// Update boot: the roster's kept workers relaunch eagerly ahead of the restore
    /// pass; busy-at-crash workers revive too. The kept set is shared (an `Arc`).
    UpdateRoster {
        kept: Arc<std::collections::HashSet<String>>,
    },
    /// Plain startup: only journal-proven live work revives.
    PlainStartup,
}

/// One descriptor's boot-adoption decision.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AdoptionOutcome {
    /// Live socket adopted (incl. a worker that re-registered before the
    /// descriptor scan reached it).
    AdoptedLive,
    /// Dead descriptor relaunched (busy evidence on a plain boot, kept
    /// worker on an update boot).
    Revived,
    /// Dead descriptor with no durable busy evidence: stayed down.
    SkippedIdle,
    /// The descriptor carried a durable stop tombstone: the boot re-ran
    /// the stop's finalization instead of adopting or reviving.
    Stopped,
    Failed,
}

fn registration_start_id(worker_id: &str, pid: u32) -> Result<String> {
    #[cfg(test)]
    if let Some(observed) = registration_identity_overrides()
        .lock()
        .expect("identity overrides")
        .remove(worker_id)
    {
        return observed
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Session worker registration identity is unavailable"));
    }
    let _ = worker_id;
    crate::protocol::process_start_id(pid)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Session worker registration identity is unavailable"))
}

#[cfg(test)]
fn registration_identity_overrides(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<String>>> {
    static OVERRIDES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    OVERRIDES.get_or_init(Default::default)
}

impl Supervisor {
    /// Adopt or relaunch persisted workers, concurrently: one dead worker's relaunch must
    /// not delay adopting live sessions; the fan-out is capped at
    /// [`crate::recovery_pacing::ADOPTION_CONCURRENCY`] to avoid a relaunch storm.
    pub(super) async fn adopt_persisted_workers(self: &Arc<Self>, boot: AdoptionBoot) {
        let descriptors = load_descriptors(&self.descriptor_dir, &self.options.socket_path);
        let jobs: Vec<_> = descriptors
            .into_iter()
            .map(|(path, descriptor)| {
                let supervisor = Arc::clone(self);
                let boot = boot.clone();
                move || async move {
                    supervisor
                        .adopt_persisted_worker(path, descriptor, boot)
                        .await;
                }
            })
            .collect();
        crate::recovery_pacing::run_bounded(jobs, crate::recovery_pacing::ADOPTION_CONCURRENCY)
            .await;
        // TS arms the owner cleanup for every adopted worker: an owner that
        // does not reconnect within the grace loses its worker.
        for resident in self.registry.list().await {
            self.schedule_owned_worker_cleanup(&resident).await;
        }
        // The boot roster seed runs exactly once, in the background, now
        // that adoption settled: the registry's residents are the seed
        // roots. TS awaits its seed before adoption; the Rust daemon
        // deliberately accepts before and during adoption, so the seed
        // follows the pass and its one `roster_update` publish carries
        // the rows to early subscribers. Tests drain the pending-seed
        // barrier for completion.
        self.spawn_roster_boot_seed();
    }

    /// Positive owner-death evidence for the recorded pid (the inverse of
    /// the tombstone's [`recorded_process_alive`]): the process is
    /// provably gone, or the pid provably names a different process now.
    /// An unobservable pid proves nothing and answers `false`.
    async fn recorded_worker_is_gone(&self, resident: &Arc<ResidentWorker>) -> bool {
        let (pid, expected) = {
            let descriptor = resident.descriptor.lock().await;
            (descriptor.pid, descriptor.process_start_id.clone())
        };
        let Ok(pid) = u32::try_from(pid) else {
            return false;
        };
        tokio::task::spawn_blocking(move || {
            let alive = crate::lease::is_process_alive(pid).ok();
            let observed = crate::lease::get_process_start_id(pid);
            !recorded_process_alive(alive, expected.as_deref(), observed.as_deref())
        })
        .await
        .unwrap_or(false)
    }

    /// Adopt one persisted worker descriptor. Serialized against worker
    /// self-registration by the per-worker adoption gate: whichever path
    /// arrives first builds the roster entry; the other finds it present.
    async fn adopt_persisted_worker(
        self: &Arc<Self>,
        path: PathBuf,
        descriptor: crate::descriptor::WorkerDescriptor,
        boot: AdoptionBoot,
    ) -> AdoptionOutcome {
        let Ok(recorded_pid) = u32::try_from(descriptor.pid) else {
            self.log_line("worker descriptor PID is out of range; adoption remains unresolved");
            return AdoptionOutcome::Failed;
        };
        let Ok(admission) = self.acquire_launch_admission() else {
            return AdoptionOutcome::Failed;
        };
        let worker_id = descriptor.worker_id.clone();
        let guard = self.registry.adoption_guard(&worker_id).await;
        if self.registry.get(&worker_id).await.is_some() {
            // The worker re-registered before the descriptor scan reached it.
            self.log_line(&format!(
                "session worker {worker_id} already registered; skipping descriptor adoption"
            ));
            return AdoptionOutcome::AdoptedLive;
        }
        let socket_path = PathBuf::from(&descriptor.socket_path);
        // A tombstoned descriptor belongs to its recorded process, not
        // whichever listener now owns its pathname. A dead pid (or a
        // recycled one with a different start id) must bypass auth and
        // finish the stop; a foreign listener can otherwise keep adoption
        // waiting behind the worker-auth budget. TS adoptOrRecoverWorker
        // checks the recorded pid before connecting a stopped worker.
        // Ordinary descriptors retain the existing socket-based revival
        // decision, which also handles descriptors without a start id.
        // The identity probes are the tombstone's alone: an ordinary
        // descriptor's stop term is `stop_requested_at.is_none()` and the
        // OR never reads the probes, so running them for every descriptor
        // pays a process spawn (`ps` on Unix platforms without /proc or
        // sysctl) inside the async adoption task - blocking an executor
        // worker at boot. The tombstoned path runs the probes off the
        // runtime through `spawn_blocking`; a join failure conservatively
        // treats the recorded process as alive (the graceful IPC leg
        // below degrades to the same finalize a dead verdict runs).
        let tombstoned = descriptor.stop_requested_at.is_some();
        let recorded_process_alive = if tombstoned {
            let pid = recorded_pid;
            let expected = descriptor.process_start_id.clone();
            tokio::task::spawn_blocking(move || {
                recorded_process_alive(
                    crate::lease::is_process_alive(pid).ok(),
                    expected.as_deref(),
                    crate::lease::get_process_start_id(pid).as_deref(),
                )
            })
            .await
            .unwrap_or(true)
        } else {
            true
        };
        let alive = (descriptor.stop_requested_at.is_none() || recorded_process_alive)
            && socket::can_connect(&socket_path, Duration::from_millis(500)).await;
        let pid = descriptor.pid;
        let orphan_generation = super::supervision::OrphanGeneration::from_descriptor(&descriptor);
        let journal_path = PathBuf::from(&descriptor.recovery_journal_path);
        let resident = ResidentWorker::new(worker_id.clone(), descriptor, path);
        // The durable pending FIRST: a failed identity persist left a side record carrying the
        // moved-to identity — apply it before any routing or revival acts on the stale record.
        self.apply_identity_pending(&resident).await;
        // The stop tombstone outranks liveness (TS's stop ownership: the stop was durable intent
        // BEFORE the worker was told): a supervisor that died between the tombstone and the
        // shutdown finishes the stop on the next boot — never adopts it as healthy.
        if resident.descriptor.lock().await.stop_requested_at.is_some() {
            self.finish_tombstoned_stop(&resident, alive).await;
            return AdoptionOutcome::Stopped;
        }
        // The monitor must watch the process that actually runs, never the
        // descriptor's stale pre-restart pid (a revived worker was once left
        // orphaned behind a phantom-exit loop while serving live).
        let (result, revived_child) = if alive {
            let adopted = self
                .connect_worker(&resident, worker_connect_deadline())
                .await;
            if adopted.is_ok() {
                // The boot reconciliation: the adopted worker may already serve a superseded
                // session the persisted record never learned. Pull the live state BEFORE the
                // routing opens — the roster write carries the identity follow, so the record
                // and binding re-bind first.
                if !self.refresh_roster_entry(&resident).await {
                    // A failed pull is not proof the worker is dead: the resident is quarantined
                    // from every identity route until the live word lands (the routing refuses
                    // instead of serving the superseded identity).
                    resident.mark_identity_quarantined();
                    self.spawn_identity_reconciliation_retry(&resident);
                    self.log_line(&format!(
                        "session worker {worker_id}: the boot reconciliation pull failed; the resident is quarantined from routing until the live state lands"
                    ));
                }
                // The adopted worker's session already exists (its create ran before the
                // supervisor restart): routed client commands may reach it immediately.
                resident.note_session_ready();
            }
            (adopted, None)
        } else {
            // An unconnectable socket proves nothing about the process: a
            // live-but-slow worker still owns its journaled `bash()` groups,
            // so the boot reap needs the same positive owner-death evidence
            // the tombstone probes use (TS `recoverWorker` runs its orphan
            // sweep only once the recorded process identity is gone).
            if self.recorded_worker_is_gone(&resident).await {
                self.reap_dead_worker_orphans(&resident).await;
            }
            let interrupted =
                crate::journal::WorkerRecoveryJournal::read_interrupted(&journal_path);
            let kept = match &boot {
                AdoptionBoot::UpdateRoster { kept } => kept.contains(&worker_id),
                AdoptionBoot::PlainStartup => false,
            };
            if !interrupted && !kept {
                // Dead worker with no durable busy state — and, on an update boot, not
                // kept: leave it down (the descriptor stays on disk); the session reopens
                // on the next client create.
                self.log_line(&format!(
                    "session worker {worker_id} was idle at exit; not revived (reopens on the next client open)"
                ));
                return AdoptionOutcome::SkippedIdle;
            }
            // The revival ownership gate: the busy-evidence filter answered "did the journal
            // ever prove live work?"; this gate answers "is that proof still a genuine
            // interruption THIS boot must heal?" A give-up, a stopped session, another
            // worker's lease, or stale evidence vetoes.
            let busy_recorded_at =
                crate::journal::WorkerRecoveryJournal::latest_busy_recorded_at(&journal_path);
            let gated = resident.descriptor.lock().await;
            let veto = crate::revival_gate::revival_veto(
                &self.options.agent_dir,
                &gated,
                kept,
                busy_recorded_at.as_deref(),
            );
            drop(gated);
            if let Some(veto) = veto {
                self.log_line(&format!(
                    "session worker {worker_id} not revived: {}",
                    veto.log_reason()
                ));
                return AdoptionOutcome::SkippedIdle;
            }
            // Dead worker with journal-proven live work (or a kept worker): relaunch from
            // the durable create command. The spawned child rides out to the monitor arming
            // below — the descriptor's pid is the DEAD pre-restart holder.
            match self
                .relaunch_worker_admitted(&resident, Arc::clone(&admission))
                .await
            {
                Ok(child) => (Ok(()), Some(child)),
                Err(error) => (Err(error), None),
            }
        };
        let outcome = match result {
            Ok(()) => {
                #[cfg(test)]
                self.pause_launch_at("adoption_before_registry").await;
                self.registry.insert(Arc::clone(&resident)).await;
                // A live leftover keeps its real pid; a revived worker is watched through
                // the child handle itself.
                match revived_child {
                    Some(child) => {
                        let child_pid = child.id().unwrap_or(0);
                        self.spawn_monitor(
                            Arc::clone(&resident),
                            Some(child),
                            u64::from(child_pid),
                            None,
                        );
                    }
                    None => self.spawn_monitor(Arc::clone(&resident), None, pid, orphan_generation),
                }
                // The adopted worker joins the roster from its live state.
                self.refresh_roster_entry(&resident).await;
                // A restore pass that owns this session's roster row can settle it now (spec
                // §10.4): the waiters attach to the live worker instead of queueing behind the
                // recovery — unless the row still needs its §10.5 continuation prompt.
                if let Some(session_file) = resident.descriptor.lock().await.session_file.clone() {
                    if let Some(stem) = Path::new(&session_file)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                    {
                        self.restore.settle_adopted(&stem);
                    }
                }
                self.log_line(&format!(
                    "adopted session worker {worker_id} (was alive: {alive})"
                ));
                if alive {
                    AdoptionOutcome::AdoptedLive
                } else {
                    AdoptionOutcome::Revived
                }
            }
            Err(error) => {
                self.log_line(&format!("could not adopt worker {worker_id}: {error:#}"));
                AdoptionOutcome::Failed
            }
        };
        drop(guard);
        outcome
    }

    /// `worker_register`: a session worker presenting its identity (boot registration or
    /// re-registration after a supervisor restart). The token was issued at spawn or
    /// adoption, so an unknown id or mismatch is rejected.
    pub(super) async fn handle_worker_register(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        command: &DaemonCommand,
    ) -> DaemonResponse {
        let DaemonCommand::WorkerRegister {
            active_session_id,
            session_id,
            socket_path,
            worker_instance_id,
            token,
            pid,
            ..
        } = command
        else {
            return response_failure(Some(command_id), type_name, "not a registration", None);
        };
        let fail = |error: &str| response_failure(Some(command_id), type_name, error, None);
        if self.shutting_down.load(Ordering::SeqCst) {
            return fail("Supervisor is shutting down");
        }
        if active_session_id.is_empty() || socket_path.is_empty() || *pid == 0 {
            return fail("Session worker registration is missing identity fields");
        }
        let Ok(registration_pid) = u32::try_from(*pid) else {
            return fail("Session worker registration PID is out of range");
        };
        let worker_instance_id =
            (!worker_instance_id.is_empty()).then(|| worker_instance_id.clone());
        let registration = WorkerRegistration {
            active_session_id: active_session_id.clone(),
            session_id: session_id
                .clone()
                .filter(|value: &String| !value.is_empty()),
            socket_path: socket_path.clone(),
            worker_instance_id: worker_instance_id.clone(),
            pid: *pid,
        };
        // Serialize against descriptor adoption for the same worker.
        let guard = self.registry.adoption_guard(active_session_id).await;
        let (resident, re_arm_monitor, registration_start_id) = match self
            .registry
            .get(active_session_id)
            .await
        {
            Some(resident) => {
                // The monitor ownership reads before this registration's
                // descriptor update overwrites the pid: a registrant naming
                // a different process than the watched one takes the resident.
                let descriptor = resident.descriptor.lock().await;
                if token.as_str() != descriptor.authentication_token {
                    return fail("Session worker authentication failed");
                }
                let registration_start_id =
                    match registration_start_id(active_session_id, registration_pid) {
                        Ok(start_id) => start_id,
                        Err(error) => return fail(&error.to_string()),
                    };
                let pid_changed = descriptor.pid != *pid;
                drop(descriptor);
                (resident, pid_changed, registration_start_id)
            }
            None => match self.adopt_registered_worker(&registration, token).await {
                Ok((resident, start_id)) => (resident, false, start_id),
                Err(error) => {
                    let message = format!("{error:#}");
                    // The definitive unknown-worker refusal is observable in the local log
                    // and the worker retires on it: a live worker this supervisor will
                    // never adopt would otherwise hold its lease forever.
                    if message.starts_with(crate::registration::UNKNOWN_SESSION_WORKER_PREFIX) {
                        self.log_line(&format!(
                            "session worker {active_session_id} registration refused; the worker retires"
                        ));
                    }
                    return fail(&message);
                }
            },
        };
        // The registration's durable id is optional on the wire: a re-registering worker
        // that does not report it still owns its persisted descriptor. Both reads share
        // this one lock acquisition — a second one let the create path race into a stall.
        let (durable_session_id, orphan_generation) = {
            let mut descriptor = resident.descriptor.lock().await;
            // The initial registration check preceded adoption/auth awaits.
            // Never republish a process identity after terminal admission closed.
            if self.shutting_down.load(Ordering::SeqCst) {
                return fail("Supervisor is shutting down");
            }
            if token.as_str() != descriptor.authentication_token {
                return fail("Session worker authentication failed");
            }
            // A registrant must not inherit the predecessor's journal merely
            // because registration updates its pid. Spawn publishes the path
            // and pid together; only that already-matching pair is cleanup evidence.
            let orphan_generation =
                super::supervision::OrphanGeneration::from_descriptor(&descriptor)
                    .filter(|generation| u64::from(generation.pid) == *pid);
            let previous_worker_instance_id = descriptor.worker_instance_id.clone();
            // A REPLACEMENT registration flips the roster's stale-delta slot to the replacement
            // BEFORE it is exposed anywhere: a predecessor's pull or frame still in flight must
            // meet the slot naming the replacement; a re-register keeps it untouched.
            if previous_worker_instance_id.as_deref() != worker_instance_id.as_deref() {
                let replacement = worker_instance_id.clone().unwrap_or_default();
                let mut roster = self.roster.lock().unwrap();
                roster.note_worker_generation(&resident.worker_id, &replacement);
            }
            if descriptor.pid != *pid {
                // The old monitor retains the predecessor's cleanup identity.
                // The registrant's path is unknown: reserve an unused generation
                // path so boot backfill cannot restore the predecessor's legacy
                // path. This does not claim the live registrant writes this file.
                descriptor.orphan_process_journal_path = Some(
                    crate::journal::orphan_journal_path(
                        &self.descriptor_dir,
                        &format!("{}-{}", descriptor.worker_id, uuid::Uuid::new_v4()),
                    )
                    .to_string_lossy()
                    .into_owned(),
                );
            }
            descriptor.pid = *pid;
            // Reuse the one authenticated registration observation. Missing or
            // empty identities were refused before descriptor/registry mutation.
            descriptor.process_start_id = Some(registration_start_id);
            descriptor.socket_path.clone_from(socket_path);
            descriptor
                .worker_instance_id
                .clone_from(&worker_instance_id);
            if let Some(session_id) = &registration.session_id {
                descriptor.root_session_id = Some(session_id.clone());
            }
            descriptor.lifecycle = DaemonWorkerLifecycle::Ready;
            // A (re-)registered worker refreshes its binding: the id stays addressable
            // with its current durable identity; a newly owned session file supersedes.
            self.record_session_binding(
                active_session_id,
                descriptor.root_session_id.as_deref(),
                descriptor.session_file.as_deref(),
            );
            // The registration refreshes the resident in memory only — no
            // persist here. The spawn record already carries the
            // launch-time identity (pid, socket), and the create-completion
            // persist (`launch_worker`'s post-create write) owns the next
            // durable state — `Ready` with the session identity — as the
            // metadata-survival barrier. The boot scan adopts a live worker
            // socket-first regardless of the recorded lifecycle, and a dead
            // worker's recovery replays the durable create command.
            let durable_session_id = match registration.session_id.clone() {
                Some(session_id) => Some(session_id),
                None => descriptor
                    .session_file
                    .clone()
                    .as_deref()
                    .and_then(|file| Path::new(file).file_stem())
                    .map(|stem| stem.to_string_lossy().to_string()),
            };
            (durable_session_id, orphan_generation)
        };
        // A pre-existing resident whose registrant named a different process
        // (boot adoption can watch a stale on-disk pid): arm the monitor for
        // the registrant. The generation claim retires the stale watch at its
        // exit check.
        if re_arm_monitor {
            self.spawn_monitor(Arc::clone(&resident), None, *pid, orphan_generation);
        }
        let record = self.registry.record_registration(registration).await;
        // A restore pass that owns this session's roster row can settle it now (spec §10.4).
        // The settle lands after the registration is recorded, so a woken waiter's
        // re-resolve cannot miss it.
        if let Some(session_id) = durable_session_id {
            self.restore.settle_adopted(&session_id);
        }
        let verb = if record.epoch > 1 {
            "re-registered"
        } else {
            "registered"
        };
        self.log_line(&format!(
            "session worker {active_session_id} {verb} (epoch {}, pid {pid})",
            record.epoch
        ));
        drop(guard);
        // Registration rebuilt the resident: refresh its roster entry from
        // the live worker so the roster reflects the re-registered state.
        self.refresh_roster_entry(&resident).await;
        // A worker that registers after the boot seed publishes its passive ledger family in
        // the background: TS reseeds on the worker's first roster snapshot, and this port's
        // workers push only their own summary, so the daemon walks the family here.
        let family_root = {
            let descriptor = resident.descriptor.lock().await;
            descriptor
                .session_file
                .clone()
                .or_else(|| descriptor.create_command.session_path.clone())
        };
        if let Some(root) = family_root {
            self.spawn_roster_registration_seed(Path::new(&root));
        }
        response_success(
            Some(command_id),
            type_name,
            Some(json!({
                "workerId": active_session_id,
                "sessionId": session_id,
                "supervisorGeneration": format!("sup:{}", std::process::id()),
                "supervisorPid": std::process::id(),
                "epoch": record.epoch,
            })),
        )
    }

    /// A registration for a worker with no roster entry: adopt it from its
    /// persisted descriptor (the durable fallback record). The registration
    /// proves the worker process is alive; adoption connects it for routing.
    async fn adopt_registered_worker(
        self: &Arc<Self>,
        registration: &WorkerRegistration,
        token: &str,
    ) -> Result<(Arc<ResidentWorker>, String)> {
        let registration_pid = u32::try_from(registration.pid)
            .context("Session worker registration PID is out of range")?;
        let _admission = self.acquire_launch_admission()?;
        let descriptor_path = self
            .descriptor_dir
            .join(format!("{}.json", registration.active_session_id));
        let content = match std::fs::read_to_string(&descriptor_path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The TS unknown-worker error: this supervisor holds no descriptor for
                // the identity, so it can never adopt the registrant, which retires.
                return Err(anyhow!(
                    "{}: {}",
                    crate::registration::UNKNOWN_SESSION_WORKER_PREFIX,
                    registration.active_session_id
                ));
            }
            // An unreadable descriptor is NOT an unknown worker: the identity exists on disk,
            // and retiring it over a transient I/O failure would strand a live lease holder.
            Err(error) => {
                return Err(anyhow!(
                    "descriptor read failed for {}: {error}",
                    registration.active_session_id
                ));
            }
        };
        let mut descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_str(&content)
            .with_context(|| format!("invalid descriptor {}", descriptor_path.display()))?;
        crate::descriptor::validate_descriptor(&descriptor, &self.options.socket_path)?;
        u32::try_from(descriptor.pid).context("worker descriptor PID is out of range")?;
        if token != descriptor.authentication_token.as_str() {
            return Err(anyhow!("Session worker authentication failed"));
        }
        let registration_start_id =
            registration_start_id(&registration.active_session_id, registration_pid)?;
        // The same boot-load backfill: a pre-upgrade descriptor wires its
        // orphan journal here too, so a relaunch of this worker journals.
        crate::descriptor::backfill_orphan_journal_path(&self.descriptor_dir, &mut descriptor);
        let worker_id = descriptor.worker_id.clone();
        let orphan_generation = super::supervision::OrphanGeneration::from_descriptor(&descriptor)
            .filter(|generation| u64::from(generation.pid) == registration.pid);
        let resident = ResidentWorker::new(
            registration.active_session_id.clone(),
            descriptor,
            descriptor_path,
        );
        // The durable pending (the same repair the descriptor adoption runs): apply the failed
        // follow's side record before the routing opens, so the moved-to session is served.
        self.apply_identity_pending(&resident).await;
        // A tombstoned identity is mid-stop (TS `adoptOrRecoverWorker`'s stopRequestedAt
        // branch): adoption finishes the stop and never adopts the worker as healthy.
        // The refusal is definitive: the registrant is the process the stop must
        // retire, so it exits on the verdict instead of re-registering into the
        // same unfinished stop.
        if resident.descriptor.lock().await.stop_requested_at.is_some() {
            // The registering process is the identity the stop must retire: the persisted
            // descriptor carries the stopped worker's stale pid, so observing the
            // registrant's live identity keeps the escalation tied to the live process.
            {
                let mut descriptor = resident.descriptor.lock().await;
                if descriptor.pid != registration.pid {
                    // A tombstoned registrant also has an unknown journal path.
                    // Reserve an unused generation instead of None, which boot
                    // backfill could bind to the predecessor's legacy journal.
                    descriptor.orphan_process_journal_path = Some(
                        crate::journal::orphan_journal_path(
                            &self.descriptor_dir,
                            &format!("{}-{}", descriptor.worker_id, uuid::Uuid::new_v4()),
                        )
                        .to_string_lossy()
                        .into_owned(),
                    );
                    descriptor.pid = registration.pid;
                }
                descriptor.process_start_id = Some(registration_start_id.clone());
                let _ = crate::descriptor::persist_worker(&resident.descriptor_path, &descriptor);
            }
            self.finish_tombstoned_stop(&resident, true).await;
            return Err(anyhow!(
                crate::registration::tombstoned_registration_refusal(
                    &registration.active_session_id
                )
            ));
        }
        self.connect_worker(&resident, worker_connect_deadline())
            .await?;
        // The boot reconciliation: registration rebuilt the resident from the PERSISTED record;
        // the worker may already serve a replacement the record never learned. Pull the live
        // state BEFORE the resident joins the registry, so the record and binding re-bind first.
        if !self.refresh_roster_entry(&resident).await {
            // A failed pull is not proof the worker is dead: the resident is quarantined
            // from every identity route until the live word lands.
            resident.mark_identity_quarantined();
            self.spawn_identity_reconciliation_retry(&resident);
            self.log_line(&format!(
                "session worker {worker_id}: the registration reconciliation pull failed; the resident is quarantined from routing until the live state lands"
            ));
        }
        // The self-registered worker's session already exists: routed
        // client commands may reach it immediately.
        resident.note_session_ready();
        #[cfg(test)]
        self.pause_launch_at("registration_before_registry").await;
        self.registry.insert(Arc::clone(&resident)).await;
        self.spawn_monitor(
            Arc::clone(&resident),
            None,
            registration.pid,
            orphan_generation,
        );
        self.log_line(&format!(
            "adopted session worker {worker_id} via self-registration"
        ));
        Ok((resident, registration_start_id))
    }

    /// Record one RLM child admission: the spawn edge in the daemon-owned
    /// ledger (durable topology) and the child's display file (hydration
    /// metadata). No-op for top-level sessions.
    pub(super) async fn record_rlm_child_admission(
        self: &Arc<Self>,
        command: &DaemonCommand,
        summary: &Value,
    ) -> Result<()> {
        let DaemonCommand::Create {
            name,
            config,
            runtime_metadata,
            ..
        } = command
        else {
            return Ok(());
        };
        let Some(metadata) = runtime_metadata else {
            return Ok(());
        };
        if metadata.get("kind").and_then(Value::as_str) != Some("subagent") {
            return Ok(());
        }
        let child_id = metadata
            .get("rlmChildId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("RLM child admission is missing rlmChildId"))?
            .to_string();
        let depth = metadata
            .get("rlmDepth")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        let parent = metadata
            .get("parentSessionFile")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("RLM child admission is missing parentSessionFile"))?;
        let child = summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("RLM child admission is missing the child session file"))?;
        let session_name = summary
            .get("sessionName")
            .and_then(Value::as_str)
            .or(name.as_deref())
            .unwrap_or_default()
            .to_string();
        let session_dir = config
            .as_ref()
            .and_then(|config| config.get("sessionDir"))
            .and_then(Value::as_str)
            .map_or_else(
                || {
                    Path::new(child)
                        .parent()
                        .map(|dir| dir.to_string_lossy().to_string())
                        .unwrap_or_default()
                },
                str::to_string,
            );
        let ledger = self.rlm_spawn_ledger_for(None).await?;
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: child_id.clone(),
                parent: parent.to_string(),
                child: child.to_string(),
                depth,
                name: session_name.clone(),
            })
            .inspect_err(|error| {
                self.log_line(&format!("failed to append RLM ledger spawn: {error:#}"));
            })?;
        // Reopening a resident worker is not a new child task. Keep the
        // completed task's display intact when its durable identity matches.
        if crate::rlm_ledger::read_rlm_subagent_display(Path::new(&session_dir)).is_some_and(
            |display| {
                display.child_id == child_id
                    && display.status == "completed"
                    && crate::lease::canonical_session_path(Path::new(&display.session_file))
                        == crate::lease::canonical_session_path(Path::new(child))
            },
        ) {
            return Ok(());
        }
        let display = crate::rlm_ledger::RlmSubagentDisplayEntry {
            type_tag: "rlm_subagent".to_string(),
            child_id,
            session_name,
            session_dir,
            session_file: child.to_string(),
            rlm_parent_node_id: metadata
                .get("rlmParentNodeId")
                .and_then(Value::as_str)
                .map(str::to_string),
            prompt: metadata
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::to_string),
            spawn_code: metadata
                .get("spawnCode")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: metadata.get("model").cloned(),
            status: "running".to_string(),
            created_at: metadata
                .get("createdAt")
                .and_then(Value::as_u64)
                .unwrap_or_else(crate::util::now_ms),
        };
        let written =
            crate::rlm_ledger::write_rlm_subagent_display(&display).inspect_err(|error| {
                self.log_line(&format!(
                    "failed to persist RLM subagent display entry: {error:#}"
                ));
            })?;
        if !written {
            self.log_line(&format!(
                "skipped RLM subagent display entry for {}: deleted tombstone exists",
                display.child_id
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod rlm_child_admission_tests {
    use super::*;

    #[tokio::test]
    async fn restored_child_admission_preserves_completed_display_and_ledger_edge() {
        let dir = tempfile::tempdir().expect("fixture");
        let agent_dir = dir.path().join("agent");
        let supervisor = Arc::new(
            Supervisor::new(super::super::SupervisorOptions {
                socket_path: dir.path().join("supervisor.sock"),
                agent_dir: agent_dir.clone(),
            })
            .expect("supervisor"),
        );
        let parent = agent_dir.join("sessions/parent.jsonl");
        let child_dir = agent_dir.join("session-artifacts/parent/sub-kid");
        let child = child_dir.join("child.jsonl");
        std::fs::create_dir_all(parent.parent().unwrap()).expect("sessions");
        std::fs::create_dir_all(&child_dir).expect("child dir");
        std::fs::write(&parent, "{\"type\":\"session\",\"id\":\"parent\"}\n").expect("parent");
        std::fs::write(&child, "{\"type\":\"session\",\"id\":\"child\"}\n").expect("child");
        let create = json!({
            "type":"create", "name":"kid", "config":{"sessionDir":child_dir},
            "runtimeMetadata": {
                "kind":"subagent", "rlmChildId":"sub-kid", "rlmDepth":1,
                "parentSessionFile":parent, "prompt":"finished task", "createdAt":123,
                "model":{"provider":"private-loopback","modelId":"mock-1"}
            }
        });
        let command: DaemonCommand = serde_json::from_value(create.clone()).expect("create");
        let summary = json!({"sessionFile":child, "sessionName":"kid"});
        supervisor
            .record_rlm_child_admission(&command, &summary)
            .await
            .expect("initial admission");
        let mut completed =
            crate::rlm_ledger::read_rlm_subagent_display(&child_dir).expect("display");
        completed.status = "completed".into();
        crate::rlm_ledger::write_rlm_subagent_display(&completed).expect("settled display");
        let display_path = child_dir.join("rlm-subagent.json");
        let before = std::fs::read(&display_path).expect("completed bytes");
        let ledger = supervisor.rlm_spawn_ledger_for(None).await.expect("ledger");
        let edges = ledger.live_edges().expect("edges");
        let mut restore = create;
        restore["sessionPath"] = json!(child);
        let command = serde_json::from_value(restore).expect("restored create");
        supervisor
            .record_rlm_child_admission(&command, &summary)
            .await
            .expect("restored admission");
        assert_eq!(
            std::fs::read(display_path).expect("restored display bytes"),
            before
        );
        assert_eq!(ledger.live_edges().expect("restored edges"), edges);
    }
}

#[cfg(test)]
mod launch_admission_tests {
    use super::*;

    #[tokio::test]
    async fn terminal_stop_refuses_boot_adoption_and_unknown_registration_before_publication() {
        let dir = tempfile::tempdir().expect("fixture");
        let supervisor = Arc::new(
            Supervisor::new(super::super::SupervisorOptions {
                socket_path: dir.path().join("supervisor.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .expect("supervisor"),
        );
        let descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_value(json!({
            "version": 2, "workerId": "refused", "pid": 0,
            "socketPath": dir.path().join("worker.sock"),
            "recoveryJournalPath": dir.path().join("recovery.jsonl"),
            "supervisorSocketPath": supervisor.options.socket_path,
            "authenticationToken": "fixture-only", "rootActiveSessionId": "refused",
            "createdAt": "t", "updatedAt": "t", "lifecycle": "ready",
            "createCommand": {}, "consecutiveFailures": 0
        }))
        .expect("descriptor");
        supervisor.shutting_down.store(true, Ordering::SeqCst);
        let outcome = supervisor
            .adopt_persisted_worker(
                dir.path().join("refused.json"),
                descriptor,
                AdoptionBoot::PlainStartup,
            )
            .await;
        assert!(matches!(outcome, AdoptionOutcome::Failed));
        let registration = WorkerRegistration {
            active_session_id: "refused".into(),
            session_id: None,
            socket_path: dir
                .path()
                .join("worker.sock")
                .to_string_lossy()
                .into_owned(),
            worker_instance_id: None,
            pid: 1,
        };
        let error = supervisor
            .adopt_registered_worker(&registration, "fixture-only")
            .await
            .err()
            .expect("registration refused");
        assert!(error.to_string().contains("shutting down"));
        assert!(supervisor.registry.list().await.is_empty());
        assert!(!dir.path().join("refused.json").exists());
    }
}

#[cfg(test)]
mod pid_range_tests {
    use super::*;

    #[tokio::test]
    async fn oversized_pid_is_refused_before_registration_or_boot_adoption_work() {
        let dir = tempfile::tempdir().expect("fixture");
        let supervisor = Arc::new(
            Supervisor::new(super::super::SupervisorOptions {
                socket_path: dir.path().join("supervisor.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .expect("supervisor"),
        );
        let oversized = u64::from(u32::MAX) + 1; // Low bits zero: never an ambient target.
        let descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_value(json!({
            "version": 2, "workerId": "oversized", "pid": oversized,
            "socketPath": dir.path().join("worker.sock"),
            "recoveryJournalPath": dir.path().join("recovery.jsonl"),
            "supervisorSocketPath": supervisor.options.socket_path,
            "authenticationToken": "fixture-only", "rootActiveSessionId": "oversized",
            "createdAt": "t", "updatedAt": "t", "lifecycle": "ready",
            "stopRequestedAt": "t", "createCommand": {}, "consecutiveFailures": 0
        }))
        .expect("descriptor");
        let path = dir.path().join("worker.json");
        assert!(matches!(
            supervisor
                .adopt_persisted_worker(
                    path.clone(),
                    descriptor.clone(),
                    AdoptionBoot::PlainStartup,
                )
                .await,
            AdoptionOutcome::Failed
        ));
        assert!(supervisor.registry.list().await.is_empty());
        assert!(!path.exists());
        let command = DaemonCommand::WorkerRegister {
            id: None,
            active_session_id: "oversized".into(),
            session_id: None,
            socket_path: dir
                .path()
                .join("worker.sock")
                .to_string_lossy()
                .into_owned(),
            worker_instance_id: "fixture".into(),
            token: "fixture-only".into(),
            pid: oversized,
            rest: serde_json::Map::default(),
        };
        let refusal = supervisor
            .handle_worker_register("unknown", "worker_register", &command)
            .await;
        assert!(!refusal.success);
        assert!(refusal.error.unwrap_or_default().contains("out of range"));
        let resident = ResidentWorker::new("oversized".into(), descriptor, path.clone());
        supervisor.registry.insert(Arc::clone(&resident)).await;
        let before = serde_json::to_value(&*resident.descriptor.lock().await).expect("before");
        let refusal = supervisor
            .handle_worker_register("known", "worker_register", &command)
            .await;
        assert!(!refusal.success);
        assert!(refusal.error.unwrap_or_default().contains("out of range"));
        assert_eq!(
            serde_json::to_value(&*resident.descriptor.lock().await).expect("after"),
            before
        );
        let registration = WorkerRegistration {
            active_session_id: "oversized".into(),
            session_id: None,
            socket_path: dir
                .path()
                .join("worker.sock")
                .to_string_lossy()
                .into_owned(),
            worker_instance_id: None,
            pid: oversized,
        };
        assert!(supervisor
            .adopt_registered_worker(&registration, "fixture-only")
            .await
            .err()
            .expect("direct adoption refused")
            .to_string()
            .contains("out of range"));
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod registration_uncertainty_tests {
    use super::*;

    #[tokio::test]
    async fn missing_or_empty_registration_identity_preserves_known_and_unknown_state() {
        for known in [false, true] {
            for observed in [None, Some(String::new())] {
                let dir = tempfile::tempdir().expect("fixture");
                let supervisor = Arc::new(
                    Supervisor::new(super::super::SupervisorOptions {
                        socket_path: dir.path().join("supervisor.sock"),
                        agent_dir: dir.path().join("agent"),
                    })
                    .expect("supervisor"),
                );
                let worker_id = format!("identity-{}", uuid::Uuid::new_v4());
                let pid = u64::from(std::process::id());
                let descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_value(json!({
                    "version": 2, "workerId": worker_id, "pid": pid,
                    "processStartId": "must-not-retain", "socketPath": dir.path().join("worker.sock"),
                    "recoveryJournalPath": dir.path().join("recovery.jsonl"),
                    "supervisorSocketPath": supervisor.options.socket_path,
                    "authenticationToken": "fixture-only", "rootActiveSessionId": worker_id,
                    "createdAt": "t", "updatedAt": "t", "lifecycle": "ready",
                    "createCommand": {}, "consecutiveFailures": 0
                })).expect("descriptor");
                let path = supervisor.descriptor_dir.join(format!("{worker_id}.json"));
                let bytes = serde_json::to_vec(&descriptor).expect("fixture bytes");
                std::fs::write(&path, &bytes).expect("fixture descriptor");
                let resident = ResidentWorker::new(worker_id.clone(), descriptor, path.clone());
                if known {
                    supervisor.registry.insert(Arc::clone(&resident)).await;
                }
                let before =
                    serde_json::to_value(&*resident.descriptor.lock().await).expect("before");
                registration_identity_overrides()
                    .lock()
                    .expect("overrides")
                    .insert(worker_id.clone(), observed);
                let command = DaemonCommand::WorkerRegister {
                    id: None,
                    active_session_id: worker_id.clone(),
                    session_id: None,
                    socket_path: dir
                        .path()
                        .join("worker.sock")
                        .to_string_lossy()
                        .into_owned(),
                    worker_instance_id: "fixture".into(),
                    token: "fixture-only".into(),
                    pid,
                    rest: serde_json::Map::default(),
                };
                let response = supervisor
                    .handle_worker_register("uncertain", "worker_register", &command)
                    .await;
                assert!(!response.success);
                assert!(response
                    .error
                    .unwrap_or_default()
                    .contains("identity is unavailable"));
                assert_eq!(
                    serde_json::to_value(&*resident.descriptor.lock().await).expect("after"),
                    before
                );
                assert_eq!(std::fs::read(&path).expect("preserved descriptor"), bytes);
                assert_eq!(supervisor.registry.list().await.len(), usize::from(known));
                assert!(!registration_identity_overrides()
                    .lock()
                    .expect("overrides")
                    .contains_key(&worker_id));
                assert!(
                    resident.cmd_tx.lock().await.is_none(),
                    "refusal never starts monitor/stop or connects"
                );
            }
        }
    }
}
