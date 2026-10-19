//! One-shot refresh never starts a daemon or a session. All state is fixture-local.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn cli_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(args)
        .env_clear()
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("PRIME_AGENT_KERNEL_VENV", dir.join("kernel-venv"))
        .env("RLM_SESSION_DIR", dir.join("session"))
        .env("RLM_HARNESS_STATE_DIR", dir.join("harness"))
        .env("RLM_GLOBAL_HARNESS_STATE_DIR", dir.join("harness-global"))
        .env(
            "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
            dir.join("run/daemon.sock"),
        )
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("PRIME_AGENT_CODING_AGENT_DIR", dir.join("agent"))
        .env("PRIME_AGENT_SESSION_DIR", dir.join("agent/sessions"))
        .env("DO_NOT_TRACK", "1")
        .env("PI_OFFLINE", "1")
        .current_dir(dir)
        .stdin(Stdio::null());
    command
}

fn cli(dir: &Path, args: &[&str]) -> Output {
    cli_command(dir, args).output().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn refresh_offline_without_a_daemon_does_not_create_a_socket_or_session() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("absent.sock");
    let output = cli(
        dir.path(),
        &[
            "--refresh",
            "--offline",
            "--daemon-socket",
            socket.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("offline"), "{}", text(&output));
    assert!(!socket.exists());
    assert!(!dir.path().join("agent/sessions").exists());
}

#[test]
fn explicit_offline_refresh_never_contacts_a_resident_daemon() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("resident.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = cli(
        dir.path(),
        &[
            "--refresh",
            "--offline",
            "--daemon-socket",
            socket.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("offline"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn refresh_rejects_session_prompt_and_command_options_before_any_launch() {
    let dir = tempfile::TempDir::new().unwrap();
    for args in [
        vec!["--refresh", "hello"],
        vec!["--refresh", "--model", "test"],
        vec!["--refresh", "@missing-file"],
        vec!["status", "--refresh"],
        vec!["--refresh", "--mode", "daemon"],
        vec!["--refresh", "--cwd", "/missing"],
    ] {
        let output = cli(dir.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert!(text(&output).contains("--refresh accepts only --daemon-socket and --offline; no session or prompt options."), "{}", text(&output));
    }
    assert!(!dir.path().join("agent").exists());
}

fn mock_refresh(revision: u64, capability: bool, response: Value) -> (Output, Vec<Value>) {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        writeln!(writer, "{}", json!({
            "type": "daemon_hello", "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "schemaRevision": revision,
            "serverCapabilities": if capability { vec!["model_refresh"] } else { Vec::<&str>::new() },
        })).unwrap();
        let mut reader = BufReader::new(stream);
        let mut requests = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            let mut reply = response.clone();
            reply["type"] = json!("response");
            reply["id"] = request["id"].clone();
            reply["command"] = request["command"]["type"].clone();
            requests.push(request["command"].clone());
            writeln!(writer, "{reply}").unwrap();
            line.clear();
        }
        requests
    });
    let output = cli(
        dir.path(),
        &["--refresh", "--daemon-socket", socket.to_str().unwrap()],
    );
    let requests = server.join().unwrap();
    assert!(!dir.path().join("agent/sessions").exists());
    (output, requests)
}

#[test]
fn compatible_resident_daemon_refreshes_once_and_prints_sanitized_fields() {
    let (output, requests) = mock_refresh(
        32,
        /*capability*/ true,
        json!({ "success": true, "data": {
            "startedAt": 1, "finishedAt": 2, "sources": [
                { "source": "openrouter", "status": "updated", "modelCount": 42 },
                { "source": "https://private.invalid/token", "status": "skipped", "reason": "unconfigured" }
            ], "workers": { "total": 1, "succeeded": 1, "failed": 0 }
        }}),
    );
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "openrouter: updated (42 models)\nunknown-source: skipped (unconfigured)\n"
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["type"], "refresh_all_models");
}

#[test]
fn old_or_unadvertised_daemons_fall_back_without_sending_a_refresh() {
    for (revision, capability) in [(31, true), (32, false)] {
        let (output, requests) = mock_refresh(revision, capability, json!({}));
        assert!(output.status.success(), "{}", text(&output));
        assert!(requests.is_empty());
        assert!(text(&output).contains("offline"));
    }
}

#[test]
fn remote_failure_does_not_fall_back_or_expose_daemon_error_text() {
    let (output, requests) = mock_refresh(
        32,
        /*capability*/ true,
        json!({ "success": false, "error": "secret endpoint/token" }),
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output),
        "Model catalog refresh failed. Check the agent log for details.\n"
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn zero_resident_workers_uses_the_standalone_registry() {
    let (output, requests) = mock_refresh(
        32,
        /*capability*/ true,
        json!({ "success": true, "data": {
            "startedAt": 1, "finishedAt": 2, "sources": [{ "source": "mock-source", "status": "updated" }],
            "workers": { "total": 0, "succeeded": 0, "failed": 0 }
        }}),
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(!text(&output).contains("mock-source"));
    assert!(text(&output).contains("offline"));
    assert_eq!(requests.len(), 1);
}

#[test]
fn stale_network_and_worker_failures_exit_nonzero_but_offline_stale_does_not() {
    for (status, reason, failed, exit) in [
        ("stale", "network", 0, 1),
        ("failed", "network", 0, 1),
        ("stale", "offline", 0, 0),
        ("unchanged", "offline", 1, 1),
    ] {
        let (output, _) = mock_refresh(
            32,
            /*capability*/ true,
            json!({ "success": true, "data": {
                "startedAt": 1, "finishedAt": 2,
                "sources": [{ "source": "openrouter", "status": status, "reason": reason }],
                "workers": { "total": 1, "succeeded": 1 - failed, "failed": failed }
            }}),
        );
        assert_eq!(output.status.code(), Some(exit), "{}", text(&output));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_saturated_resident_backlog_cannot_hold_the_one_shot_open() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("full.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // SAFETY: the descriptor is a live listening Unix socket, held for the whole test.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(&socket).unwrap();
    let mut child = cli_command(
        dir.path(),
        &["--refresh", "--daemon-socket", socket.to_str().unwrap()],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let finished = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(finished, "refresh blocked on a saturated listener");
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert_eq!(
        text(&output),
        "Model catalog refresh failed. Check the agent log for details.\n"
    );
    assert!(
        !dir.path().join("agent").exists(),
        "an ambiguous connect must not fall back"
    );
}
