// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! A cell that raises `SystemExit` must fail as a cell, not end the kernel: the
//! runtime reports it as that cell's error and keeps serving. And when a cell
//! never settles, the "still running the previously interrupted cell" refusal
//! must stay typed, because the ipython tool's wait/kill recovery branches on it.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pa_core::kernel::cancellation::AbortSignal;
use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelBusyAfterInterruptError,
    KernelManagerOptions,
};

mod fixture_executable {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-support/executable.rs"
    ));
}

/// The kernel interpreter (the product's auto-bootstrapped venv); skipped with a
/// note when absent. `PA_CORE_KERNEL_PYTHON` points at an explicit one instead.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

/// The runtime under test, pinned ahead of the installed copy in the venv so the
/// live tests exercise this checkout's `rlm.repl` instead of a released one.
fn runtime_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime/src")
}

fn options(python: PathBuf, session: &str) -> KernelManagerOptions {
    let pythonpath = match std::env::var("PYTHONPATH") {
        Ok(existing) if !existing.is_empty() => {
            format!("{}:{existing}", runtime_src().display())
        }
        _ => runtime_src().display().to_string(),
    };
    KernelManagerOptions {
        python: Some(python),
        cwd: Some(std::env::temp_dir()),
        env: HashMap::from([("PYTHONPATH".to_string(), pythonpath)]),
        session_id: Some(session.to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        on_background_work_settled: None,
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path: None,
    }
}

fn busy_kernel_path() -> tempfile::TempPath {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file = tempfile::NamedTempFile::new().expect("fake kernel file");
    let source = fixture_executable::script(include_str!("fixtures/busy_kernel.py"), "python3");
    file.write_all(source.as_bytes())
        .expect("write fake kernel");
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake kernel");
    file.into_temp_path()
}

#[tokio::test]
async fn a_system_exit_cell_fails_that_cell_and_the_kernel_keeps_serving() {
    let Some(python) = kernel_python() else {
        return;
    };
    let manager = ReplKernelManager::new(options(python, "system-exit-cell"));
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("kernel starts");
    let warm = manager
        .execute("value = 41", ExecuteOptions::default())
        .await
        .expect("warm cell");
    assert_eq!(warm.status, ExecuteStatus::Ok);

    let exit = tokio::time::timeout(
        Duration::from_secs(20),
        manager.execute("raise SystemExit", ExecuteOptions::default()),
    )
    .await
    .expect("the SystemExit cell must settle")
    .expect("the cell reports an error instead of killing the transport");
    assert_eq!(exit.status, ExecuteStatus::Error);
    assert_eq!(
        exit.error.as_ref().map(|error| error.ename.as_str()),
        Some("SystemExit")
    );

    let after = tokio::time::timeout(
        Duration::from_secs(20),
        manager.execute("print(value + 1)", ExecuteOptions::default()),
    )
    .await
    .expect("the follow-up cell must not hang")
    .expect("the kernel still serves after a SystemExit cell");
    assert_eq!(after.status, ExecuteStatus::Ok);
    assert_eq!(after.stdout.trim(), "42");
    manager.kill();
}

#[tokio::test]
async fn an_unsettled_cell_leaves_the_busy_refusal_typed() {
    let fixture = busy_kernel_path();
    let manager = ReplKernelManager::new(options(fixture.to_path_buf(), "busy-kernel"));
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("fake kernel starts");

    // Abort a cell the kernel never finishes: the manager keeps it active on
    // purpose, so the next cell waits out the busy window and is refused.
    let signal = AbortSignal::new();
    let abort = signal.clone();
    let abort_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        abort.abort();
    });
    let first = tokio::time::timeout(
        Duration::from_secs(20),
        manager.execute(
            "stuck",
            ExecuteOptions {
                signal: Some(signal),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("the aborted cell settles")
    .expect("an aborted cell returns a result");
    abort_task.abort();
    assert_eq!(first.status, ExecuteStatus::Aborted);

    let second = tokio::time::timeout(
        Duration::from_secs(30),
        manager.execute("1 + 1", ExecuteOptions::default()),
    )
    .await
    .expect("the busy refusal returns instead of hanging");
    let error = second.expect_err("the unsettled cell must refuse the next one");
    assert!(
        error
            .downcast_ref::<KernelBusyAfterInterruptError>()
            .is_some(),
        "the busy refusal lost its type on the way out of the manager: {error:#}"
    );
    manager.kill();
}
