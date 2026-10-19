//! The daemon-ensure concern: a read-only socket probe, detached supervisor
//! spawn, and bounded startup polling. A foreign peer never grants takeover.

use anyhow::Context as _;
use tokio::io::AsyncBufReadExt as _;

use super::{anyhow, Command, Duration, Instant, Path, Result, Stdio};

const DAEMON_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DAEMON_CONNECT_TIMEOUT_MS: u64 = 3_000;
const DAEMON_HELLO_TIMEOUT_MS: u64 = 15_000;
/// Pause between daemon-startup probes (TS `ensureDaemonRunning` polls at
/// 25ms). A cold supervisor binds its socket ~39ms after the spawn, so the
/// poll interval quantizes every cold launch: the bind lands 0-interval
/// before the next probe, a pure wait on a floor of microseconds. The 1ms
/// interval caps that overshoot at <=1ms for the cost of ~40 failed
/// connects per cold launch (each ~us; bounded by the same 30s startup
/// budget, so a hung boot adds at most ~1k connects/s of syscall work,
/// not a spin). Overshoot tables: probe-grid record 20261001-034500 on the
/// bench repo. Timing-only: the probe itself, the 30s startup budget, and
/// the timeout error are unchanged.
const DAEMON_PROBE_INTERVAL_MS: u64 = 1;

#[cfg(all(test, unix))]
#[path = "daemon_tests.rs"]
mod tests;

enum DaemonProbe {
    Absent,
    Current,
}

/// Probe without sending commands. The TUI client rejects unknown protocol
/// names during connect, so its connect error cannot distinguish a foreign
/// listener from an absent one. Only a definitive transport refusal permits
/// spawning; any connected peer must supply the current native hello.
async fn probe_daemon(socket_path: &Path) -> Result<DaemonProbe> {
    let stream = match tokio::time::timeout(
        Duration::from_millis(DAEMON_CONNECT_TIMEOUT_MS),
        pa_types::platform::transport::connect_transport(socket_path),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error))
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            return Ok(DaemonProbe::Absent);
        }
        Ok(Err(error)) => return Err(refuse_peer(socket_path, &error.to_string())),
        Err(_) => return Err(refuse_peer(socket_path, "connection timed out")),
    };
    let (reader, _writer) = stream.split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    match tokio::time::timeout(
        Duration::from_millis(DAEMON_HELLO_TIMEOUT_MS),
        reader.read_line(&mut line),
    )
    .await
    {
        Ok(Ok(0)) => {
            return Err(refuse_peer(
                socket_path,
                "connection closed before the hello",
            ))
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(refuse_peer(socket_path, &error.to_string())),
        Err(_) => return Err(refuse_peer(socket_path, "hello timed out")),
    }
    let hello: serde_json::Value = serde_json::from_str(&line)
        .map_err(|_| refuse_peer(socket_path, "invalid daemon hello"))?;
    let current = hello.get("type") == Some(&serde_json::json!("daemon_hello"))
        && hello.get("protocol").and_then(|p| p.get("name"))
            == Some(&serde_json::json!(pa_types::daemon::DAEMON_PROTOCOL_NAME))
        && hello.get("protocol").and_then(|p| p.get("version"))
            == Some(&serde_json::json!(
                pa_types::daemon::DAEMON_PROTOCOL_VERSION
            ))
        && hello.get("schemaId") == Some(&serde_json::json!(pa_types::daemon::DAEMON_SCHEMA_ID));
    if current {
        Ok(DaemonProbe::Current)
    } else {
        Err(refuse_peer(socket_path, "incompatible protocol or schema"))
    }
}

fn refuse_peer(socket_path: &Path, reason: &str) -> anyhow::Error {
    anyhow!(
        "The Prime Agent Rust client cannot use the peer on selected socket {}: {reason}. No shutdown or replacement was attempted. Select a separate private socket with --daemon-socket, or use the matching client for that runtime.",
        socket_path.display()
    )
}

/// Ensure a current native daemon is listening on `socket_path`, spawning this
/// executable in `--mode daemon` only when the selected socket is absent.
/// Foreign peers are left running, including peers with no active sessions.
///
/// # Errors
/// Returns an error when the executable path cannot be resolved, the selected
/// socket has an incompatible or unverified peer, the supervisor cannot be
/// spawned, or no current daemon starts before the startup timeout.
pub async fn ensure_daemon_running(socket_path: &Path, spawn_cwd: &Path) -> Result<()> {
    if matches!(probe_daemon(socket_path).await?, DaemonProbe::Current) {
        return Ok(());
    }
    let exe = std::env::current_exe().context("resolve the prime-agent executable")?;
    ensure_daemon_running_with(&exe, socket_path, spawn_cwd).await
}

/// [`ensure_daemon_running`] with an explicit supervisor executable (the
/// product path uses this process's own binary).
///
/// # Errors
/// Returns an error for an incompatible or unverified peer, when the supervisor
/// cannot be spawned, or when no current daemon starts before the timeout.
pub async fn ensure_daemon_running_with(
    exe: &Path,
    socket_path: &Path,
    spawn_cwd: &Path,
) -> Result<()> {
    if matches!(probe_daemon(socket_path).await?, DaemonProbe::Current) {
        return Ok(());
    }
    spawn_supervisor_detached(socket_path, spawn_cwd, exe)?;
    let deadline = Instant::now() + Duration::from_millis(DAEMON_STARTUP_TIMEOUT_MS);
    loop {
        if matches!(probe_daemon(socket_path).await?, DaemonProbe::Current) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Timed out waiting for the Prime Agent Rust daemon to start on selected socket {}. Check the selected socket and daemon diagnostics, then retry.",
                socket_path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(DAEMON_PROBE_INTERVAL_MS)).await;
    }
}

/// Spawn a detached supervisor on `socket_path` (the child outlives this
/// CLI).
fn spawn_supervisor_detached(socket_path: &Path, spawn_cwd: &Path, exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket_path)
        .current_dir(spawn_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The binary entry strips any inherited worker handshake env, so the
    // spawned supervisor never starts in worker mode. A daemon must also not
    // share the launching TUI's terminal session: a session-wide terminal
    // cleanup could hang it up after the TUI exits; on Unix, setsid also
    // creates its own process group.
    #[cfg(unix)]
    pa_core::platform::process::set_new_session(&mut command);
    #[cfg(not(unix))]
    pa_core::platform::process::set_new_process_group(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn the Prime Agent daemon on {}", socket_path.display()))?;
    Ok(())
}
