//! The `prime-agent update` driver: the invoking CLI's phases (spec §4
//! `Acquire`..`Staged`) plus the spawn of the detached coordinator and the
//! status tail. The terminal status drives the printed report and the exit
//! code.

use std::path::PathBuf;

use anyhow::{Context, Result};
use pa_types::daemon::update_flow::{UpdateState, UpdateStatus, UpdateTimeoutBudget};

use super::intent::{acquire, hand_over, release, status_path_for, AcquireOutcome};
use super::plan::{plan, UpdatePlan};
use super::report::UpdateReport;
use super::status::{read_status, StatusWriter};

/// The tail loop budgets: 30 minutes of progress, 3 minutes of heartbeat liveness.
const TAIL_PROGRESS_TIMEOUT_MS: u64 = 30 * 60_000;
const TAIL_LIVENESS_TIMEOUT_MS: u64 = 180_000;
/// Grace for a status file deleted by the successor's boot sweep (spec §6):
/// the coordinator's next write recreates it inside a normal boot window.
const TAIL_SWEEP_GRACE_MS: u64 = 60_000;
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The update command's fixed inputs (parsed by the public command layer).
pub struct UpdateCommandOptions {
    pub force: bool,
    pub rollback: bool,
    pub channel: Option<pa_core::update::version::UpdateChannel>,
    /// The manual/direct install: stage this local payload (a release
    /// archive or a payload directory) instead of resolving the channel.
    pub archive: Option<std::path::PathBuf>,
    /// The `http(s)://` origin recorded as the release's install source (required with `archive`;
    /// future updates resolve from it).
    pub source: Option<String>,
}

/// `prime-agent update`: plan, stage, spawn the coordinator, and relay its
/// terminal status. Returns the process exit code.
///
/// # Errors
/// Returns an error when the binary is not installer-owned or a phase,
/// the lock, a status write, or the coordinator spawn fails.
pub async fn run_update_command(options: &UpdateCommandOptions) -> Result<i32> {
    if let Some(message) = pa_core::update::managed::self_update_refusal() {
        anyhow::bail!(message);
    }
    let agent_dir = crate::config::get_agent_dir();
    let socket_path = pa_daemon::socket::default_daemon_socket_path();
    let install_root = super::activation_root().map_err(|_| {
        anyhow::anyhow!(
            "This compiled application is not owned by the Prime Agent installer. Update it using its original installer."
        )
    })?;
    let update_id = pa_types::daemon::update_flow::UpdateId::from(uuid::Uuid::now_v7().to_string());
    let status_path = status_path_for(&agent_dir, &socket_path.to_string_lossy());

    match acquire(
        &agent_dir,
        &socket_path.to_string_lossy(),
        &update_id,
        &status_path,
    )? {
        AcquireOutcome::Join { status_path } => {
            // Relay the running update's terminal status.
            let status = tail_status(&status_path).await;
            print_terminal(&status);
            return Ok(i32::from(status.state != UpdateState::Complete));
        }
        AcquireOutcome::Acquired => {}
    }
    let mut writer = StatusWriter::new(&status_path, &update_id, &socket_path.to_string_lossy())?;
    writer.set_state(UpdateState::Planning)?;
    let budget = UpdateTimeoutBudget::from_env();
    let download_base = std::env::var("PRIME_AGENT_DOWNLOAD_BASE_URL").ok();
    let planned = if options.archive.is_some() {
        plan_direct(&install_root, options).await
    } else {
        plan(
            &install_root,
            options.force,
            options.rollback,
            options.channel,
            download_base.as_deref(),
        )
        .await
    };
    let (coordinator_exe, candidate_dir) = match planned {
        Ok(UpdatePlan::Update {
            version,
            archive_url,
            archive_sha256,
            base_url,
        }) => {
            // `Downloading`: stream + digest the archive (one wall-clock budget across all
            // attempts, spec §9).
            writer.set_state(UpdateState::Downloading)?;
            let archive = agent_dir.join(format!("update-{update_id}.tar.gz"));
            pa_core::update::download::download_archive(
                &archive_url,
                &archive_sha256,
                &archive,
                pa_core::update::download::DownloadBudget {
                    total_ms: budget.download_ms,
                    attempts: budget.download_attempts,
                },
                &pa_core::update::release::update_user_agent(version.as_str()),
            )
            .await
            .with_context(|| "the release download failed; the installed version was kept")?;
            // `Staged`: extract, validate, and probe the candidate.
            writer.set_state(UpdateState::Staged)?;
            let release_dir = pa_core::update::download::stage_archive(
                &archive,
                &archive_sha256,
                &install_root,
                &version,
                base_url.as_str(),
            )?;
            let _ = std::fs::remove_file(&archive);
            super::swap::validate_candidate(&release_dir.join("prime-agent"), &version).await?;
            (release_dir.join("prime-agent"), release_dir)
        }
        Ok(UpdatePlan::Direct {
            version,
            candidate_dir,
        }) => {
            // `Staged` directly: `plan_direct` already staged the payload;
            // the probe below re-checks the binary's `--version`.
            writer.set_state(UpdateState::Staged)?;
            super::swap::validate_candidate(&candidate_dir.join("prime-agent"), &version).await?;
            (candidate_dir.join("prime-agent"), candidate_dir)
        }
        Ok(UpdatePlan::Rollback {
            coordinator_exe,
            candidate_dir,
            ..
        }) => {
            // A rollback stages nothing; the previous release is already validated on disk. The
            // coordinator IS the previous binary.
            writer.set_state(UpdateState::Staged)?;
            (coordinator_exe, candidate_dir)
        }
        Ok(UpdatePlan::Skipped { reason }) => {
            writer.set_state(UpdateState::Skipped)?;
            writer.set_message(Some(reason.clone()))?;
            release(&agent_dir, &socket_path.to_string_lossy())?;
            println!("{reason}");
            // An interactive child reports no-change with the not-attempted
            // code so the client keeps running (TS `setSelfUpdateNoChangeExitCode`).
            let interactive_child =
                std::env::var(crate::public_command::SELF_UPDATE_INTERACTIVE_CHILD_ENV).as_deref()
                    == Ok("1");
            return Ok(if interactive_child { 75 } else { 0 });
        }
        Err(error) => {
            // A planning failure aborts before any state that could wedge: the daemon never stopped
            // (spec §4 `Aborted`).
            writer.set_state(UpdateState::Aborted)?;
            writer.set_message(Some(error.to_string()))?;
            release(&agent_dir, &socket_path.to_string_lossy())?;
            anyhow::bail!("{error:#}");
        }
    };

    // Spawn the detached coordinator (the new - or previous - binary) and
    // hand the lock over (spec §3: only the status file couples them).
    let mut command = std::process::Command::new(&coordinator_exe);
    command
        .args([
            "update",
            crate::public_command::DAEMON_UPDATE_RESTART_COORDINATOR_FLAG,
            "--daemon-socket",
        ])
        .arg(&socket_path)
        .arg(crate::public_command::DAEMON_UPDATE_RESTART_STATUS_FLAG)
        .arg(&status_path)
        .env(super::coordinator::UPDATE_CANDIDATE_DIR_ENV, &candidate_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The coordinator must survive the CLI and its terminal closing.
    #[cfg(unix)]
    pa_core::platform::process::set_new_session(&mut command);
    #[cfg(not(unix))]
    pa_core::platform::process::set_new_process_group(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn the update coordinator at {}",
            coordinator_exe.display()
        )
    })?;
    hand_over(
        &agent_dir,
        &socket_path.to_string_lossy(),
        &update_id,
        u64::from(child.id()),
        &status_path,
    )?;
    let status = tail_status_with(&status_path, Some(&mut child), &mut |observed, _fresh| {
        if let Some(line) = phase_status_line(observed.state) {
            println!("{line}");
        }
    })
    .await;
    // Reap even when a terminal status arrives before process teardown. A
    // plain thread does not hold the async runtime open for a detached child.
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            eprintln!("Warning: could not reap the update coordinator: {error}");
        }
    });
    print_terminal(&status);
    Ok(i32::from(status.state != UpdateState::Complete))
}

/// The manual/direct install (`--archive <payload>`): the version comes from the
/// payload binary's `--version` probe, staged copy-to-scratch + fsync + atomic rename.
async fn plan_direct(
    install_root: &std::path::Path,
    options: &UpdateCommandOptions,
) -> Result<UpdatePlan> {
    let archive = options
        .archive
        .as_deref()
        .context("the direct install needs a release payload")?;
    let source = options.source.as_deref().context(
        "the direct install needs --source <https-url> (recorded as the release's install origin)",
    )?;
    if !archive.exists() {
        anyhow::bail!("the release payload {} does not exist", archive.display());
    }
    // The active launcher must name a valid release before anything is replaced: the coordinator
    // records it as the rollback target.
    if pa_core::update::install::read_installation(
        install_root,
        pa_core::update::install::CURRENT_LAUNCHER,
    )
    .is_err()
    {
        anyhow::bail!(
            "The active launcher does not point at a valid managed release. Repair the installation (reinstall with the published installer) before a direct install."
        );
    }
    let (candidate_dir, version) =
        pa_core::update::download::stage_local_payload(archive, install_root, source).await?;
    Ok(UpdatePlan::Direct {
        version,
        candidate_dir,
    })
}

/// Tail the coordinator's status file to a terminal state: progress, liveness, and the holder's
/// process lifetime all bound the wait.
async fn tail_status(status_path: &std::path::Path) -> UpdateStatus {
    tail_status_with(status_path, /*coordinator*/ None, &mut |_, _| {}).await
}

/// Tail status with an observer for each state transition.
async fn tail_status_with(
    status_path: &std::path::Path,
    mut coordinator: Option<&mut std::process::Child>,
    observe: &mut dyn FnMut(&UpdateStatus, bool),
) -> UpdateStatus {
    let started = std::time::Instant::now();
    let mut last_liveness = std::time::Instant::now();
    let mut last_epoch: Option<u64> = None;
    let mut last_state: Option<UpdateState> = None;
    // The successor's boot sweep (spec §6 step 1) deletes the scratch dir,
    // this status file included, while the coordinator still drives the
    // successor's `Restoring`/`Complete` writes: grace a seen-then-vanished
    // file instead of failing the tail mid-update.
    let mut missing_since: Option<std::time::Instant> = None;
    loop {
        if let Some(status) = read_status(status_path) {
            if Some(status.epoch) != last_epoch {
                last_epoch = Some(status.epoch);
                last_liveness = std::time::Instant::now();
            }
            missing_since = None;
            // One observation per NEW state (epoch churn inside a state - heartbeats - does not
            // re-fire the observers).
            if last_state != Some(status.state) {
                let fresh = last_state.is_some();
                last_state = Some(status.state);
                observe(&status, fresh);
            }
            if status.state.is_terminal() {
                return status;
            }
        } else if missing_since.is_none() && last_epoch.is_some() {
            missing_since = Some(std::time::Instant::now());
        }
        if let Some(child) = coordinator.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    // A final write can race the status read immediately above.
                    if let Some(status) =
                        read_status(status_path).filter(|status| status.state.is_terminal())
                    {
                        return status;
                    }
                    return unreported(
                        status_path,
                        &format!(
                            "the update coordinator exited with {exit} before reporting completion"
                        ),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    return unreported(
                        status_path,
                        &format!("could not check the update coordinator: {error}"),
                    );
                }
            }
        }
        let swept = missing_since
            .is_some_and(|seen| (seen.elapsed().as_millis() as u64) < TAIL_SWEEP_GRACE_MS);
        if !swept && last_liveness.elapsed().as_millis() as u64 >= TAIL_LIVENESS_TIMEOUT_MS {
            return unreported(
                status_path,
                "the update coordinator stopped reporting liveness",
            );
        }
        if started.elapsed().as_millis() as u64 >= TAIL_PROGRESS_TIMEOUT_MS {
            return unreported(status_path, "timed out waiting for update progress");
        }
        tokio::time::sleep(TAIL_POLL).await;
    }
}

fn unreported(status_path: &std::path::Path, message: &str) -> UpdateStatus {
    let mut status = read_status(status_path).unwrap_or_else(|| UpdateStatus {
        version: 1,
        update_id: pa_types::daemon::update_flow::UpdateId::from(String::new()),
        socket_path: String::new(),
        state: UpdateState::Failed,
        epoch: 0,
        coordinator: None,
        predecessor: None,
        successor: None,
        counts: pa_types::daemon::update_flow::UpdateStatusCounts::default(),
        failures: Vec::new(),
        message: Some(message.to_string()),
        started_at: String::new(),
        updated_at: String::new(),
        heartbeat_at: None,
        rest: serde_json::Map::default(),
    });
    status.state = UpdateState::Failed;
    status.message = Some(message.to_string());
    status
}

fn print_terminal(status: &UpdateStatus) {
    UpdateReport::build(status).print();
    if let Some(message) = &status.message {
        match status.state {
            UpdateState::Complete | UpdateState::Skipped => println!("{message}"),
            _ => eprintln!("{message}"),
        }
    }
}

/// The §11 CLI status line for one live state (the terminal states print the TS-parity report
/// instead).
fn phase_status_line(state: UpdateState) -> Option<&'static str> {
    match state {
        UpdateState::Preparing => Some("Preparing the daemon for the update…"),
        UpdateState::Prepared => Some("Prepared — stopping sessions"),
        UpdateState::Stopping => Some("Stopping sessions gracefully…"),
        UpdateState::Activating | UpdateState::Booting => Some("Activating → booting"),
        UpdateState::Restoring => Some("Restoring sessions…"),
        UpdateState::Rollback => Some("Rolling back"),
        // `Acquire`/`Join` precede the status file; `Stopped` is `Stopping`'s own
        // tail; `Downloading`/`Staged` print in the invoking phase.
        UpdateState::Acquire
        | UpdateState::Join
        | UpdateState::Downloading
        | UpdateState::Staged
        | UpdateState::Stopped
        | UpdateState::Planning
        | UpdateState::Skipped
        | UpdateState::Complete
        | UpdateState::Aborted
        | UpdateState::Failed => None,
    }
}

/// The coordinator mode entry (`--internal-update-restart-coordinator`): the
/// detached process that adopts the staged status and drives the FSM to a
/// terminal state. Returns the process exit code.
///
/// # Errors
/// Returns an error when adoption or a status write fails; an invalid
/// invocation is reported on stderr and returns `Ok(1)`.
pub async fn run_coordinator_mode(
    socket_path: PathBuf,
    status_path: PathBuf,
    origin: Option<&str>,
) -> Result<i32> {
    let agent_dir = crate::config::get_agent_dir();
    // TS parity: the status file belongs under the agent dir's `update-restarts/` - the coordinator
    // never writes status elsewhere.
    let restarts_dir = pa_types::daemon::update_flow::update_restarts_dir(&agent_dir);
    if !status_path.starts_with(&restarts_dir) {
        eprintln!("Invalid daemon update restart coordinator invocation.");
        return Ok(1);
    }
    let legacy_handoff = match std::fs::read(&status_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            value.get("state").is_none()
                && value
                    .get("requestId")
                    .is_some_and(serde_json::Value::is_string)
                && value.get("phase").and_then(serde_json::Value::as_str) == Some("starting")
        }
    };
    if legacy_handoff {
        return super::legacy_restart::run(&socket_path, &status_path, &agent_dir, origin).await;
    }
    let options = super::coordinator::CoordinatorOptions {
        agent_dir,
        socket_path,
        status_path,
        budget: UpdateTimeoutBudget::from_env(),
    };
    let status = super::coordinator::run(&options).await?;
    print_terminal(&status);
    Ok(i32::from(status.state != UpdateState::Complete))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_exited_coordinator_fails_without_waiting_for_the_heartbeat_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("status.json");
        let id = pa_types::daemon::update_flow::UpdateId::from("exited-child".to_string());
        let mut writer = StatusWriter::new(&path, &id, "/tmp/unused-update.sock").unwrap();
        writer.set_state(UpdateState::Planning).unwrap();
        writer.set_state(UpdateState::Downloading).unwrap();
        writer.set_state(UpdateState::Staged).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let observed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tail_status_with(&path, Some(&mut child), &mut |_, _| {}),
        )
        .await
        .expect("an exited child must not consume the 180-second liveness budget");
        let exit = child.wait().unwrap();
        let mut expected = writer.current().clone();
        expected.state = UpdateState::Failed;
        expected.message = Some(format!(
            "the update coordinator exited with {exit} before reporting completion"
        ));
        assert_eq!(observed, expected);
    }
}
