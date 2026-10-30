//! Worker orphan garbage collection (TS `exitIfSupervisorOrphanedForTooLong`): a worker
//! whose supervisor died without a graceful stop exits after a bounded unreachable
//! window (sessions persist on disk). Divergence from TS: this port implements the
//! give-up branch directly, not the TS replacement-supervisor launch.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::worker::{Worker, WORKER_SHUTDOWN_SETTLE_MS_ENV, WORKER_SUPERVISOR_LOST_EXIT_MS_ENV};

/// TS `DEFAULT_WORKER_SUPERVISOR_LOST_EXIT_MS`: five minutes.
const DEFAULT_LOST_EXIT_MS: u64 = 5 * 60_000;
/// How long a requested stop may stay incomplete before the worker leaves on its own:
/// comfortably past one cooperative close (a turn settle, one routed `kill` per RLM
/// child, the canonical drain), short enough that a worker whose stop was refused never
/// outlives the supervisor that refused it by more than this.
const DEFAULT_SHUTDOWN_SETTLE_MS: u64 = 2 * 60_000;
/// TS `scheduleSupervisorAvailabilityCheck` cadence: the first check
/// 1.5s after boot, then every 5s.
const FIRST_CHECK: Duration = Duration::from_millis(1_500);
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Bounded probe: a supervisor socket that answers slower than this counts
/// as unreachable for the window bookkeeping.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The supervisor-lost exit window (TS `workerSupervisorLostExitMs`): the
/// env override when it is a finite non-negative number, else the default.
fn lost_exit_ms() -> u64 {
    lost_exit_ms_from(
        std::env::var(WORKER_SUPERVISOR_LOST_EXIT_MS_ENV)
            .ok()
            .as_deref(),
    )
}

/// The TS parse contract: `Number(raw)` kept only when finite and >= 0.
fn lost_exit_ms_from(raw: Option<&str>) -> u64 {
    raw.and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or(DEFAULT_LOST_EXIT_MS, |value| value as u64)
}

/// The shutdown settle deadline: the same parse contract, its own default.
fn shutdown_settle_ms() -> u64 {
    shutdown_settle_ms_from(
        std::env::var(WORKER_SHUTDOWN_SETTLE_MS_ENV)
            .ok()
            .as_deref(),
    )
}

fn shutdown_settle_ms_from(raw: Option<&str>) -> u64 {
    raw.and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or(DEFAULT_SHUTDOWN_SETTLE_MS, |value| value as u64)
}

/// Whether a requested stop has outlived its settle deadline. A stop that never
/// reported completion (a refused close, a caller that died mid-stop) cannot be
/// waited out: past the deadline the process must leave like an orphan.
fn shutdown_overdue(
    requested_at: Option<std::time::Instant>,
    deadline: Duration,
    now: std::time::Instant,
) -> bool {
    requested_at.is_some_and(|at| now.saturating_duration_since(at) >= deadline)
}

/// Whether the supervisor socket accepts connections (TS `canConnectToSupervisor`): a
/// connect that lands proves a live supervisor owns the socket.
async fn supervisor_reachable(socket: &Path) -> bool {
    crate::socket::can_connect(socket, CONNECT_TIMEOUT).await
}

/// Arm the orphan-exit monitor for `worker`: it runs for the process's whole lifetime,
/// like the TS `startSupervisorMonitor` timer chain.
pub(crate) fn start(worker: Arc<Worker>) {
    tokio::spawn(async move {
        monitor(worker).await;
    });
}

/// The availability-check loop (TS `checkSupervisorAvailability` guard order): a live
/// supervisor connection disarms, a requested stop is honoured only inside its settle
/// deadline, a reachable socket resets the absence timer, and a socket unreachable for
/// the whole window with no session work in flight exits the worker.
async fn monitor(worker: Arc<Worker>) {
    let window = Duration::from_millis(lost_exit_ms());
    let deadline = Duration::from_millis(shutdown_settle_ms());
    let mut absent_since: Option<tokio::time::Instant> = None;
    let mut delay = FIRST_CHECK;
    loop {
        tokio::time::sleep(delay).await;
        delay = CHECK_INTERVAL;
        if worker.supervisor_claims.load(Ordering::SeqCst) > 0 {
            absent_since = None;
            continue;
        }
        let (requested, overdue) = {
            let core = worker.core.lock().unwrap();
            (
                core.shutdown_requested,
                shutdown_overdue(core.shutdown_requested_at, deadline, std::time::Instant::now()),
            )
        };
        // A requested stop is a stop IN FLIGHT only until its deadline: a worker whose
        // close was refused (or whose caller died mid-stop) has nothing left that can
        // complete it, so past the deadline it leaves like an orphan instead of living
        // forever behind a gate that refuses every new turn.
        if requested && !overdue {
            absent_since = None;
            continue;
        }
        if !overdue {
            if supervisor_reachable(&worker.config.supervisor_socket_path).await {
                absent_since = None;
                continue;
            }
            let since = *absent_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() < window {
                continue;
            }
        }
        let ongoing =
            worker.core.lock().unwrap().has_ongoing_work() || worker.user_bash.is_running();
        if ongoing {
            // TS `hasOngoingSessionWork` (`isSessionActive`, which counts
            // `isBashRunning`): an active run or a running user bash owns the
            // worker a little longer; its end lets the next check reconsider.
            continue;
        }
        if overdue {
            exit_worker(
                &worker,
                &format!(
                    "requested shutdown never completed within {}s",
                    deadline.as_secs()
                ),
            )
            .await;
        } else {
            let absent = absent_since.map_or(0, |since| since.elapsed().as_secs());
            exit_worker(
                &worker,
                &format!(
                    "supervisor {} unreachable for {absent}s",
                    worker.config.supervisor_socket_path.display()
                ),
            )
            .await;
        }
    }
}

/// The TS give-up exit: dispose the session's kernel, persist the recovery
/// journal, close the listener and remove the worker's own socket file,
/// and end the process (the same sequence the routed `shutdown` command
/// runs; the monitor only reaches this with no session work in flight,
/// so nothing else needs to settle first). The kernel dispose is the TS
/// `shutdown(0)` close pass (`closeSession` -> runtime dispose ->
/// `IpythonKernelProvisioner.dispose`): the process exit runs no
/// destructors, so an undisposed kernel would be orphaned here. `reason`
/// names why the monitor gave up: an unreachable supervisor, or a stop
/// request that outlived its deadline.
async fn exit_worker(worker: &Worker, reason: &str) {
    eprintln!("pa-daemon worker: {reason}; exiting");
    if let Some(agent_engine) = &worker.agent_engine {
        agent_engine.dispose_kernel().await;
    }
    let _ = worker.record_recovery(false, "shutdown");
    // The lease is retained through exit, but the sidecars still get the
    // holder's current state before the process dies; a drain writer
    // racing the fold leaves a valid warm tail (the load re-checks the
    // generation and prefix).
    let lease = worker
        .core
        .lock()
        .unwrap()
        .store
        .as_ref()
        .and_then(|store| store.lease.clone());
    if let Some(lease) = lease {
        lease.write_sidecars();
    }
    // Retain the lease through process exit: kernel disposal has a bounded
    // host-request drain, so writers may survive until the process dies.
    // The dead-owner sweep or the next acquire reclaims its directory.
    // The listener closes FIRST and the cleanup probes the path with the
    // owner's bind provably released (the TS graceful-shutdown sequence,
    // shared with `exit_after_close`), so both a REPLACED file and a
    // POISONED capture (a replacement landing in the bind->capture
    // window) spare a successor worker's live socket - the
    // deterministic-path relaunch - while the worker's own dead file
    // still unlinks through the identity gate.
    worker.close_listener_then_cleanup_socket().await;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_exit_window_parses_the_ts_contract() {
        assert_eq!(lost_exit_ms_from(None), DEFAULT_LOST_EXIT_MS);
        assert_eq!(lost_exit_ms_from(Some("garbage")), DEFAULT_LOST_EXIT_MS);
        assert_eq!(lost_exit_ms_from(Some("-1")), DEFAULT_LOST_EXIT_MS);
        assert_eq!(lost_exit_ms_from(Some("NaN")), DEFAULT_LOST_EXIT_MS);
        assert_eq!(lost_exit_ms_from(Some("0")), 0);
        assert_eq!(lost_exit_ms_from(Some("15000")), 15_000);
        assert_eq!(lost_exit_ms_from(Some("15000.5")), 15_000);
    }

    fn lost_exit_ms_from(raw: Option<&str>) -> u64 {
        raw.and_then(|raw| raw.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map_or(DEFAULT_LOST_EXIT_MS, |value| value as u64)
    }

    #[test]
    fn shutdown_settle_deadline_parses_like_the_lost_window() {
        assert_eq!(shutdown_settle_ms_from(None), DEFAULT_SHUTDOWN_SETTLE_MS);
        assert_eq!(shutdown_settle_ms_from(Some("garbage")), DEFAULT_SHUTDOWN_SETTLE_MS);
        assert_eq!(shutdown_settle_ms_from(Some("NaN")), DEFAULT_SHUTDOWN_SETTLE_MS);
        assert_eq!(shutdown_settle_ms_from(Some("9000")), 9_000);
    }

    /// A requested stop outlives its deadline only when a request armed it: an
    /// unrequested worker keeps the supervisor-lost window, and a request inside the
    /// deadline is still a stop in flight (a refusal is retryable).
    #[test]
    fn a_requested_stop_is_overdue_only_past_its_deadline() {
        let deadline = Duration::from_secs(60);
        let now = std::time::Instant::now();
        assert!(!shutdown_overdue(None, deadline, now));
        assert!(!shutdown_overdue(Some(now - Duration::from_secs(59)), deadline, now));
        assert!(shutdown_overdue(Some(now - Duration::from_secs(61)), deadline, now));
        assert!(shutdown_overdue(Some(now - Duration::from_secs(3600)), deadline, now));
    }
}
