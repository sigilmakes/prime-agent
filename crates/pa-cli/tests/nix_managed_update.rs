//! Nix owns installation and rollback. Refuse before any installer I/O.

use std::process::Command;

use pa_core::update::installer::{run_bundled_installer, run_installer_from, InstallerOutput};

struct RestoreOwner(Option<std::ffi::OsString>);

impl Drop for RestoreOwner {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            std::env::set_var("PRIME_AGENT_INSTALL_MANAGED_BY", value);
        } else {
            std::env::remove_var("PRIME_AGENT_INSTALL_MANAGED_BY");
        }
    }
}

// This integration test executable has one test: its process-local ownership
// marker cannot race another test. Subprocesses receive an environment allowlist.
#[tokio::test]
async fn nix_ownership_blocks_cli_and_both_direct_installer_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let restore = RestoreOwner(std::env::var_os("PRIME_AGENT_INSTALL_MANAGED_BY"));
    std::env::set_var("PRIME_AGENT_INSTALL_MANAGED_BY", "nix");
    let error = run_installer_from(
        "http://127.0.0.1:9/not-an-installer",
        &root.join("prefix"),
        None,
        InstallerOutput::Capture,
    )
    .await
    .unwrap_err();
    assert!(
        error.message.contains("cd /config && just update-prime"),
        "{error:?}"
    );
    let error = run_bundled_installer(&root.join("prefix"), &["--rollback".as_ref()])
        .await
        .unwrap_err();
    assert!(error.message.contains("managed by Nix"), "{error:?}");
    assert!(
        !root.join("prefix").exists(),
        "installer wrote before refusal"
    );
    drop(restore);

    for arguments in [
        vec!["update"],
        vec!["update", "--check"],
        vec!["update", "--nightly", "--force"],
        vec!["update", "--rollback"],
        vec![
            "update",
            "--archive",
            "/absent/archive.tar.gz",
            "--source",
            "https://example.invalid",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .args(&arguments)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("TMPDIR", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("agent"))
            .env("PRIME_AGENT_SESSION_DIR", root.join("sessions"))
            .env("PRIME_AGENT_KERNEL_VENV", root.join("kernel"))
            .env("RLM_GLOBAL_HARNESS_STATE_DIR", root.join("global-harness"))
            .env("RLM_HARNESS_STATE_DIR", root.join("local-harness"))
            .env("RLM_SESSION_DIR", root.join("artifacts"))
            .env("PRIME_AGENT_INSTALL_MANAGED_BY", "nix")
            .env(
                "PRIME_AGENT_RUST_INSTALLER_URL",
                "http://127.0.0.1:9/installer",
            )
            .env(
                "PRIME_AGENT_DOWNLOAD_BASE_URL",
                "http://127.0.0.1:9/releases",
            )
            .env("PRIME_AGENT_RUST_PREFIX", root.join("prefix"))
            .env("DO_NOT_TRACK", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .current_dir(root)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(75), "{arguments:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("just update-prime"));
    }
    for path in ["prefix", "kernel", "sessions", "artifacts"] {
        assert!(!root.join(path).exists(), "unexpected side effects: {path}");
    }
}
