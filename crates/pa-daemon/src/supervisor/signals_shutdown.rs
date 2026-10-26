//! Shutdown and signal handling: the drain arms, the shutdown entry, and the
//! daemon-closing shutdown event.
use super::{json, Arc, Ordering, RouteAdmission, Supervisor, Value, ROUTE_TIMEOUT_MS};
// The only use is the broadcast inside the unix begin_signal_drain arm.
#[cfg(unix)]
use super::ClientRouting;
// The only use sits behind the unix update-drain arm below.
#[cfg(unix)]
use super::PrepareState;

/// The non-update `daemon_closing` frame: every connected client learns the daemon is
/// going down for a shutdown.
pub(super) fn daemon_closing_shutdown_event() -> Value {
    json!({ "type": "daemon_closing", "reason": "shutdown" })
}

impl Supervisor {
    /// Run the one terminal stop pass, whichever connection first reaches it: the stop
    /// must still start even if the initiating client disconnects. `shutdown_started`
    /// is the one-owner gate.
    pub(super) async fn ensure_shutdown_started(self: &Arc<Self>) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.begin_shutdown().await;
    }

    pub(super) async fn begin_shutdown(self: &Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Drain accepted launches, including rollback and registry publication.
        // Never hold the exclusive fence over routed shutdown callbacks.
        let Ok(launches) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.launch_admission.write(),
        )
        .await
        else {
            self.log_line("accepted launches have not drained; terminal shutdown remains pending");
            return;
        };
        if self.launch_cleanup_uncertain.load(Ordering::SeqCst) {
            self.log_line("launch cleanup is uncertain; terminal shutdown remains pending");
            return;
        }
        drop(launches);
        let mut complete = true;
        let mut residents = self.registry.list().await;
        residents.sort_by(|left, right| left.worker_id.cmp(&right.worker_id));
        for resident in residents {
            // Bind before the stop can retire or replace descriptor state.
            let (bound_reap, initial_pid, initial_start) = {
                let descriptor = resident.descriptor.lock().await;
                let owned = resident
                    .owned_reap
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                (
                    u32::try_from(descriptor.pid)
                        .map_err(|_| ())
                        .and_then(|pid| {
                            crate::registry::OwnedWorkerReap::bind(
                                pid,
                                descriptor.process_start_id.as_deref(),
                                owned,
                            )
                        }),
                    descriptor.pid,
                    descriptor.process_start_id.clone(),
                )
            };
            let Ok(bound_reap) = bound_reap else {
                self.log_line(
                    "owned worker generation is uncertain; terminal shutdown remains pending",
                );
                complete = false;
                continue;
            };
            resident.intentional_stop.store(true, Ordering::SeqCst);
            resident.note_retired();
            // The stop tombstone persists before the worker is even told (TS
            // `stopWorkerUntracked`): a supervisor that dies between here and the worker's
            // exit leaves durable stop intent, and the next boot finishes the stop.
            if self.persist_stop_tombstone(&resident).await.is_err() {
                self.log_line(&format!(
                    "session worker {} stop tombstone could not persist; leaving the worker untouched (the next boot retries the stop)",
                    resident.worker_id
                ));
                complete = false;
                continue;
            }
            let _ = self
                .route_command_typed(
                    &resident,
                    "shutdown",
                    json!({}),
                    ROUTE_TIMEOUT_MS,
                    RouteAdmission::SupervisorInternal,
                )
                .await;
            if let Some(bound_reap) = bound_reap.as_ref() {
                if !crate::registry::OwnedWorkerReap::matches_bound(bound_reap, &resident).await {
                    self.log_line("owned worker generation changed before retirement; terminal shutdown remains pending");
                    complete = false;
                    continue;
                }
            }
            if self.retire_worker_after_stop(&resident).await
                != super::worker_lifecycle::WorkerRetirement::Retired
            {
                self.log_line(
                    "resident retirement is unresolved; terminal shutdown remains pending",
                );
                complete = false;
                continue;
            }
            if let Some(bound_reap) = bound_reap {
                if !crate::registry::OwnedWorkerReap::wait_bound(
                    &bound_reap,
                    &resident,
                    std::time::Duration::from_secs(10),
                )
                .await
                {
                    self.log_line(
                        "owned worker OS reap did not complete; terminal shutdown remains pending",
                    );
                    complete = false;
                }
            } else if !crate::registry::OwnedWorkerReap::matches_adopted(
                &resident,
                initial_pid,
                initial_start.as_deref(),
            )
            .await
            {
                self.log_line(
                    "adopted worker generation changed; terminal shutdown remains pending",
                );
                complete = false;
            }
        }
        if !complete {
            self.log_line(
                "one or more resident stops remain incomplete; terminal shutdown remains pending",
            );
            return;
        }
        if self.launch_cleanup_uncertain.load(Ordering::SeqCst) {
            self.log_line("worker cleanup is uncertain; terminal shutdown remains pending");
            return;
        }
        self.registry.clear().await;
        // The workers are all stopped now, so the accept loop may exit; the gate alone
        // is not enough — an inbound connection could fall the loop out mid-stop.
        self.accept_exit.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_one();
    }

    /// The OS-signal drain step (the loop in `crate::signal_drain` runs this once per
    /// signal): the first signal enters the graceful drain; any later signal, or one
    /// that finds a shutdown or update exit already committed, force-exits. Returns
    /// `true` when this call started the drain; `false` when one was already in flight.
    /// A signal that finds `Stopping` or `accept_exit` published never flips the gate.
    #[cfg(unix)]
    pub(crate) fn begin_signal_drain(self: &Arc<Self>) -> bool {
        if self.update_prepare.active_state() == Some(PrepareState::Stopping)
            || self.accept_exit.load(Ordering::SeqCst)
            || self.shutting_down.swap(true, Ordering::SeqCst)
        {
            return false;
        }
        self.log_line(
            "received shutdown signal; entering graceful drain: new client commands refused, running turns settle through the workers' routed shutdown",
        );
        let _ = self.events.send((
            ClientRouting::Broadcast,
            std::sync::Arc::new(daemon_closing_shutdown_event()),
        ));
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            supervisor.ensure_shutdown_started().await;
        });
        true
    }
}

#[cfg(test)]
mod terminal_truth_tests {
    use super::super::{ResidentWorker, SupervisorOptions};
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Supervisor>) {
        let dir = tempfile::tempdir().expect("fixture");
        let supervisor = Arc::new(
            Supervisor::new(SupervisorOptions {
                socket_path: dir.path().join("supervisor.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .expect("supervisor"),
        );
        (dir, supervisor)
    }

    fn resident(supervisor: &Supervisor, path: std::path::PathBuf) -> Arc<ResidentWorker> {
        named_resident(supervisor, path, "terminal-truth", 0)
    }

    fn named_resident(
        supervisor: &Supervisor,
        path: std::path::PathBuf,
        name: &str,
        pid: u64,
    ) -> Arc<ResidentWorker> {
        // PID zero is never signalled. Negative retirement outcomes are injected
        // only at the typed boundary, not by weakening any process-death oracle.
        let descriptor = serde_json::from_value(json!({
            "version": 2, "workerId": name, "pid": pid,
            "socketPath": path.with_extension("sock"), "recoveryJournalPath": path.with_extension("recovery.jsonl"),
            "supervisorSocketPath": supervisor.options.socket_path,
            "authenticationToken": "fixture-only", "rootActiveSessionId": "terminal-truth",
            "createdAt": "t", "updatedAt": "t", "lifecycle": "ready", "createCommand": {}, "consecutiveFailures": 0
        })).expect("descriptor");
        ResidentWorker::new(name.into(), descriptor, path)
    }

    #[tokio::test]
    async fn tombstone_write_failure_never_clears_resident_or_publishes_terminal_exit() {
        let (dir, supervisor) = fixture();
        let blocked_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"fixture").expect("blocking file");
        let resident = resident(&supervisor, blocked_parent.join("worker.json"));
        supervisor.registry.insert(Arc::clone(&resident)).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            supervisor.begin_shutdown(),
        )
        .await
        .expect("failure is bounded");
        assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
        assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
        assert!(!resident.descriptor_path.exists());
    }

    #[tokio::test]
    async fn unresolved_typed_retirement_never_clears_resident_or_publishes_terminal_exit() {
        use super::super::worker_lifecycle::{retirement_overrides, WorkerRetirement};
        for outcome in [
            WorkerRetirement::Survived,
            WorkerRetirement::IdentityUnverified,
            WorkerRetirement::DeathUnproven,
        ] {
            let (dir, supervisor) = fixture();
            let resident = resident(&supervisor, dir.path().join("worker.json"));
            supervisor.registry.insert(Arc::clone(&resident)).await;
            retirement_overrides()
                .lock()
                .expect("overrides")
                .insert(resident.descriptor_path.clone(), outcome);
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                supervisor.begin_shutdown(),
            )
            .await
            .expect("failure is bounded");
            assert!(
                !supervisor.accept_exit.load(Ordering::SeqCst),
                "outcome={outcome:?}"
            );
            assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
            assert!(
                resident.descriptor_path.exists(),
                "tombstone evidence preserved"
            );
            assert!(resident.descriptor.lock().await.stop_requested_at.is_some());
        }
    }

    #[tokio::test]
    async fn incomplete_first_resident_does_not_skip_independent_resident_or_retry() {
        use super::super::worker_lifecycle::{retirement_overrides, WorkerRetirement};
        for failure in [
            "tombstone",
            "owned-identity",
            "oversized-pid",
            "survived",
            "unverified",
            "death-unproven",
        ] {
            let (dir, supervisor) = fixture();
            let failed_path = if failure == "tombstone" {
                let blocked = dir.path().join("not-a-directory");
                std::fs::write(&blocked, b"fixture").expect("blocking file");
                blocked.join("worker.json")
            } else {
                dir.path().join("failed.json")
            };
            let pid = if failure == "oversized-pid" {
                u64::from(u32::MAX) + 1
            } else {
                0
            };
            let failed = named_resident(&supervisor, failed_path, "a-incomplete", pid);
            let independent = named_resident(
                &supervisor,
                dir.path().join("independent.json"),
                "z-independent",
                0,
            );
            if failure == "owned-identity" {
                let (_sender, completed) = tokio::sync::watch::channel(Some(true));
                *failed.owned_reap.lock().expect("owned record") =
                    Some(Arc::new(crate::registry::OwnedWorkerReap {
                        pid: 7,
                        start_id: Some("fixture-generation".into()),
                        completed,
                    }));
            }
            let outcome = match failure {
                "survived" => Some(WorkerRetirement::Survived),
                "unverified" => Some(WorkerRetirement::IdentityUnverified),
                "death-unproven" => Some(WorkerRetirement::DeathUnproven),
                _ => None,
            };
            if let Some(outcome) = outcome {
                retirement_overrides()
                    .lock()
                    .expect("overrides")
                    .insert(failed.descriptor_path.clone(), outcome);
            }
            retirement_overrides().lock().expect("overrides").insert(
                independent.descriptor_path.clone(),
                WorkerRetirement::Retired,
            );
            supervisor.registry.insert(Arc::clone(&failed)).await;
            supervisor.registry.insert(Arc::clone(&independent)).await;
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                supervisor.ensure_shutdown_started(),
            )
            .await
            .expect("one pass is bounded");
            assert!(
                !supervisor.accept_exit.load(Ordering::SeqCst),
                "failure={failure}"
            );
            assert_eq!(supervisor.registry.list().await.len(), 2);
            assert!(
                independent
                    .descriptor
                    .lock()
                    .await
                    .stop_requested_at
                    .is_some(),
                "independent resident was attempted"
            );
            assert!(!retirement_overrides()
                .lock()
                .expect("overrides")
                .contains_key(&independent.descriptor_path));
            assert!(
                independent.descriptor_path.exists(),
                "single injected retirement did not retry"
            );
            // The one-owner gate is settled policy: no automatic second pass.
            supervisor.ensure_shutdown_started().await;
            assert!(independent.descriptor_path.exists());
            assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
            if failure == "oversized-pid" || failure == "owned-identity" {
                assert!(
                    failed.descriptor.lock().await.stop_requested_at.is_none(),
                    "identity failure precedes stop targeting"
                );
            }
        }
    }
}
