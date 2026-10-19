use super::*;
use crate::kernel::bootstrap::venv::version::write_bootstrap_version;
use crate::kernel::bootstrap::EnsureKernelPythonOptions;

fn identity(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn environment(path: &Path, identity: &str) {
    let python = kernel_venv_python(path);
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(python, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write_bootstrap_version(path, identity, &[]).unwrap();
    std::fs::write(path.join("user-package"), "keep me").unwrap();
}

#[test]
fn archive_copy_and_rollback_preserve_original_bytes_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("kernel");
    environment(&venv, &identity('a'));
    #[cfg(unix)]
    std::os::unix::fs::symlink("user-package", venv.join("user-link")).unwrap();
    std::fs::create_dir(venv.join("user-package-data")).unwrap();
    std::fs::write(
        venv.join("user-package-data/.bootstrap-version"),
        "user data",
    )
    .unwrap();
    let marker = std::fs::read(venv.join(".bootstrap-version")).unwrap();
    let backup = archive_for_upgrade(&venv, &identity('b')).unwrap();
    assert!(!venv.exists());
    assert!(intent_path(&venv).is_file());
    copy_venv(&backup, &venv).unwrap();
    assert!(!venv.join(".bootstrap-version").exists());
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package-data/.bootstrap-version")).unwrap(),
        "user data"
    );
    std::fs::write(venv.join("user-package"), "changed by failed sync").unwrap();
    assert_eq!(
        std::fs::read_to_string(backup.join("user-package")).unwrap(),
        "keep me"
    );
    rollback(&venv, &backup).unwrap();
    assert_eq!(
        std::fs::read(venv.join(".bootstrap-version")).unwrap(),
        marker
    );
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(venv.join("user-link")).unwrap(),
        Path::new("user-package")
    );
    assert!(!intent_path(&venv).exists());
}

#[test]
fn interrupted_intent_before_rename_leaves_active_environment_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("kernel");
    environment(&venv, &identity('a'));
    let backup = backup_directory(&venv)
        .unwrap()
        .join(backup_name().unwrap());
    std::fs::write(
        intent_path(&venv),
        serde_json::to_vec(&UpgradeIntent {
            backup,
            target: identity('b'),
        })
        .unwrap(),
    )
    .unwrap();
    recover_interrupted_upgrade(&venv, &identity('b')).unwrap();
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    assert!(!intent_path(&venv).exists());
}

#[test]
fn interrupted_archive_before_copy_restores_predecessor() {
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("kernel");
    environment(&venv, &identity('a'));
    let backup = archive_for_upgrade(&venv, &identity('b')).unwrap();
    recover_interrupted_upgrade(&venv, &identity('b')).unwrap();
    assert!(!backup.exists());
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    assert!(!intent_path(&venv).exists());
}

#[test]
fn unsafe_intent_and_symlink_backup_root_never_move_active_environment() {
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("kernel");
    environment(&venv, &identity('a'));
    for backup in [
        dir.path().to_path_buf(),
        dir.path().join("kernel.bootstrap-backups/../escape"),
    ] {
        std::fs::write(
            intent_path(&venv),
            serde_json::to_vec(&UpgradeIntent {
                backup,
                target: identity('b'),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(recover_interrupted_upgrade(&venv, &identity('b')).is_err());
        assert_eq!(
            std::fs::read_to_string(venv.join("user-package")).unwrap(),
            "keep me"
        );
    }
    #[cfg(unix)]
    {
        let root = backup_directory(&venv).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::os::unix::fs::symlink(dir.path(), root).unwrap();
        assert!(archive_for_upgrade(&venv, &identity('b')).is_err());
        assert!(venv.join("user-package").is_file());
    }
}

#[cfg(unix)]
#[test]
fn successful_upgrade_intent_and_offline_source_rollback_reuse_archives() {
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("kernel");
    let old = identity('a');
    let new = identity('b');
    environment(&venv, &old);
    let original_marker = std::fs::read(venv.join(".bootstrap-version")).unwrap();
    let backup = archive_for_upgrade(&venv, &new).unwrap();
    copy_venv(&backup, &venv).unwrap();
    write_bootstrap_version(&venv, &new, &[]).unwrap();
    recover_interrupted_upgrade(&venv, &new).unwrap();
    assert!(backup.exists());
    assert!(!intent_path(&venv).exists());
    assert!(restore(&venv, &old).unwrap());
    assert_eq!(
        std::fs::read(venv.join(".bootstrap-version")).unwrap(),
        original_marker
    );
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    assert!(restore(&venv, &new).unwrap());
}

#[cfg(unix)]
fn uv_fixture(root: &Path, sync_body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let uv = root.join("uv");
    let script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}/uv.log\"\nif [ \"$1\" = sync ]; then\n{sync_body}\nfi\nexit 0\n", root.display());
    std::fs::write(&uv, script).unwrap();
    std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
    uv
}

#[cfg(unix)]
#[tokio::test]
async fn failed_locked_upgrade_rolls_back_user_packages_and_marker() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    environment(&venv, &identity('a'));
    let original = std::fs::read(venv.join(".bootstrap-version")).unwrap();
    let uv = uv_fixture(root.path(), "exit 37");
    let result = super::super::rebuild_kernel_venv(
        uv.to_str().unwrap(),
        &venv,
        root.path(),
        &identity('b'),
        &[],
        &EnsureKernelPythonOptions::default(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(venv.join(".bootstrap-version")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    assert!(!intent_path(&venv).exists());
    let calls = std::fs::read_to_string(root.path().join("uv.log")).unwrap();
    assert!(calls.contains("sync --locked --inexact --reinstall-package prime-agent-runtime --no-default-groups --group kernel --no-config --project"), "{calls}");
    assert!(!calls.contains("venv "), "{calls}");
}

#[cfg(unix)]
#[tokio::test]
async fn successful_locked_upgrade_preserves_extra_packages_and_archive() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    environment(&venv, &identity('a'));
    let uv = uv_fixture(
        root.path(),
        "test -f \"$UV_PROJECT_ENVIRONMENT/user-package\" || exit 5",
    );
    super::super::rebuild_kernel_venv(
        uv.to_str().unwrap(),
        &venv,
        root.path(),
        &identity('b'),
        &[],
        &EnsureKernelPythonOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
    let intent: UpgradeIntent =
        serde_json::from_slice(&std::fs::read(intent_path(&venv)).unwrap()).unwrap();
    assert_eq!(
        std::fs::read_to_string(intent.backup.join("user-package")).unwrap(),
        "keep me"
    );
    assert!(restore(&venv, &identity('a')).unwrap());
}

#[cfg(unix)]
#[test]
fn refused_archive_keeps_active_environment() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    environment(&venv, &identity('a'));
    // A directory at the intent destination deterministically refuses rename,
    // without relying on permission checks (which differ under root).
    std::fs::create_dir(intent_path(&venv)).unwrap();
    assert!(archive_for_upgrade(&venv, &identity('b')).is_err());
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
}

#[cfg(unix)]
struct KillGroup(i32);
impl Drop for KillGroup {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

// The same test executable acts as the worker in the isolated child branch.
// This is process dispatch, not an environment-gated optional test.
#[cfg(unix)]
#[tokio::test]
async fn killed_upgrade_replays_durable_intent_and_keeps_user_packages() {
    use std::os::unix::process::CommandExt;
    use tokio::io::AsyncBufReadExt;
    if let Some(root) = std::env::var_os("PA_BOOTSTRAP_CRASH_FIXTURE") {
        let root = PathBuf::from(root);
        let venv = root.join("kernel");
        let _lock = crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&venv)
            .await
            .unwrap();
        super::super::rebuild_kernel_venv(
            root.join("uv").to_str().unwrap(),
            &venv,
            &root,
            &identity('b'),
            &[],
            &EnsureKernelPythonOptions::default(),
        )
        .await
        .unwrap();
        panic!("worker must remain stopped in sync until killed");
    } else {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel");
        environment(&venv, &identity('a'));
        let original = std::fs::read(venv.join(".bootstrap-version")).unwrap();
        uv_fixture(root.path(), "printf 'sync-ready\\n'\nkill -STOP $$\nexit 1");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "kernel::bootstrap::venv::recovery::tests::killed_upgrade_replays_durable_intent_and_keeps_user_packages", "--nocapture"])
            .env("PA_BOOTSTRAP_CRASH_FIXTURE", root.path())
            .env("HOME", root.path()).env("PRIME_AGENT_KERNEL_VENV", &venv)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).process_group(0);
        let mut child = tokio::process::Command::from(command).spawn().unwrap();
        let group = KillGroup(i32::try_from(child.id().unwrap()).unwrap());
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("worker exited before sync readiness");
                if line == "sync-ready" {
                    break;
                }
            }
        })
        .await
        .expect("worker must reach the real uv process boundary");
        let intent: UpgradeIntent =
            serde_json::from_slice(&std::fs::read(intent_path(&venv)).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(intent.backup.join("user-package")).unwrap(),
            "keep me"
        );
        drop(group);
        child.wait().await.unwrap();
        let _lock = crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&venv)
            .await
            .unwrap();
        recover_interrupted_upgrade(&venv, &identity('b')).unwrap();
        let uv = uv_fixture(root.path(), "exit 37");
        assert!(super::super::rebuild_kernel_venv(
            uv.to_str().unwrap(),
            &venv,
            root.path(),
            &identity('b'),
            &[],
            &EnsureKernelPythonOptions::default()
        )
        .await
        .is_err());
        assert_eq!(
            std::fs::read(venv.join(".bootstrap-version")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(venv.join("user-package")).unwrap(),
            "keep me"
        );
        assert!(!intent_path(&venv).exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn failed_post_sync_readiness_restores_the_working_predecessor() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    environment(&venv, &identity('a'));
    let uv = uv_fixture(
        root.path(),
        "printf '#!/bin/sh\\nexit 1\\n' > \"$UV_PROJECT_ENVIRONMENT/bin/python\"",
    );
    assert!(super::super::rebuild_kernel_venv(
        uv.to_str().unwrap(),
        &venv,
        root.path(),
        &identity('b'),
        &[],
        &EnsureKernelPythonOptions::default()
    )
    .await
    .is_err());
    assert!(kernel_base_ready(
        &kernel_venv_python(&venv).to_string_lossy(),
        &venv,
        &identity('a')
    ));
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
}

#[cfg(unix)]
fn checkpoint_skills(root: &Path) -> Vec<super::super::BootstrapPythonSkill> {
    ["alpha", "beta", "gamma"]
        .iter()
        .map(|name| super::super::BootstrapPythonSkill {
            import_name: (*name).to_string(),
            package_path: root.join(name).to_string_lossy().to_string(),
            pyproject_path: root
                .join(name)
                .join("pyproject.toml")
                .to_string_lossy()
                .to_string(),
            pyproject_hash: "new".to_string(),
        })
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn killed_skill_sync_resumes_after_last_durable_checkpoint() {
    use std::os::unix::process::CommandExt;
    use tokio::io::AsyncBufReadExt;
    if let Some(root) = std::env::var_os("PA_BOOTSTRAP_CRASH_FIXTURE") {
        let root = PathBuf::from(root);
        let venv = root.join("kernel");
        let _lock = crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&venv)
            .await
            .unwrap();
        super::super::sync_python_skills(
            root.join("uv").to_str().unwrap(),
            &venv,
            &kernel_venv_python(&venv),
            &root,
            &identity('a'),
            &checkpoint_skills(&root),
            &EnsureKernelPythonOptions::default(),
        )
        .await
        .unwrap();
        panic!("worker must stop in gamma install until killed");
    } else {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel");
        environment(&venv, &identity('a'));
        for name in ["alpha", "beta", "gamma"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let requested = checkpoint_skills(root.path());
        let mut prior = requested[..2].to_vec();
        prior[0].pyproject_hash = "old".to_string();
        write_bootstrap_version(&venv, &identity('a'), &prior).unwrap();
        let uv = uv_fixture(
            root.path(),
            "case \"$*\" in *gamma*) printf 'sync-ready\\n'; kill -STOP $$;; esac",
        );
        let script = std::fs::read_to_string(&uv)
            .unwrap()
            .replace("= sync ]; then", "= pip ]; then");
        std::fs::write(&uv, script).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.path()).env("TMPDIR", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config")).env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_STATE_HOME", root.path().join("state")).env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.path().join("agent"))
            .env("RLM_SESSION_DIR", root.path().join("sessions"))
            .env("RLM_HARNESS_STATE_DIR", root.path().join("harness"))
            .env("RLM_GLOBAL_HARNESS_STATE_DIR", root.path().join("global-harness"))
            .env("PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET", root.path().join("daemon.sock"))
            .env("PRIME_AGENT_KERNEL_VENV", &venv).env("PA_BOOTSTRAP_CRASH_FIXTURE", root.path())
            .args(["--exact", "kernel::bootstrap::venv::recovery::tests::killed_skill_sync_resumes_after_last_durable_checkpoint", "--nocapture"])
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).process_group(0);
        let mut child = tokio::process::Command::from(command).spawn().unwrap();
        let group = KillGroup(i32::try_from(child.id().unwrap()).unwrap());
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("worker exited before skill install readiness");
                if line == "sync-ready" {
                    break;
                }
            }
        })
        .await
        .expect("worker must reach gamma install");
        assert_eq!(
            read_bootstrap_version(&venv).unwrap().python_skills,
            Some(requested[..2].to_vec())
        );
        drop(group);
        child.wait().await.unwrap();
        let _lock = crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&venv)
            .await
            .unwrap();
        let uv = uv_fixture(root.path(), "exit 0");
        super::super::sync_python_skills(
            uv.to_str().unwrap(),
            &venv,
            &kernel_venv_python(&venv),
            root.path(),
            &identity('a'),
            &requested,
            &EnsureKernelPythonOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            read_bootstrap_version(&venv).unwrap().python_skills,
            Some(requested)
        );
        let calls = std::fs::read_to_string(root.path().join("uv.log")).unwrap();
        assert_eq!(
            calls.lines().filter(|call| call.contains("alpha")).count(),
            1
        );
        assert_eq!(
            calls.lines().filter(|call| call.contains("beta")).count(),
            0
        );
        assert_eq!(
            calls.lines().filter(|call| call.contains("gamma")).count(),
            2
        );
        assert!(!calls.contains("venv "));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn committed_replacement_finalizes_intent_even_with_a_legacy_corrupt_predecessor() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    environment(&venv, &identity('a'));
    std::fs::write(
        venv.join(".bootstrap-version"),
        r#"{"schema":4,"pythonSkills":[{"importName":"edit"}]}"#,
    )
    .unwrap();
    let uv = uv_fixture(root.path(), "exit 0");
    super::super::rebuild_kernel_venv(
        uv.to_str().unwrap(),
        &venv,
        root.path(),
        &identity('b'),
        &[],
        &EnsureKernelPythonOptions::default(),
    )
    .await
    .unwrap();
    recover_interrupted_upgrade(&venv, &identity('b')).unwrap();
    assert!(!intent_path(&venv).exists());
    assert!(kernel_base_ready(
        &kernel_venv_python(&venv).to_string_lossy(),
        &venv,
        &identity('b')
    ));
    assert_eq!(
        std::fs::read_to_string(venv.join("user-package")).unwrap(),
        "keep me"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fresh_kernel_syncs_locked_group_without_seed_or_inexact() {
    let root = tempfile::tempdir().unwrap();
    let venv = root.path().join("kernel");
    let uv = uv_fixture(
        root.path(),
        "test -d \"$UV_PROJECT_ENVIRONMENT/bin\" || exit 5",
    );
    let mut script = std::fs::read_to_string(&uv).unwrap();
    script = script.replace("if [ \"$1\" = sync ]; then", "if [ \"$1\" = venv ]; then\nmkdir -p \"$2/bin\"\nprintf '#!/bin/sh\\nexit 0\\n' > \"$2/bin/python\"\nchmod +x \"$2/bin/python\"\nfi\nif [ \"$1\" = sync ]; then");
    std::fs::write(&uv, script).unwrap();
    super::super::rebuild_kernel_venv(
        uv.to_str().unwrap(),
        &venv,
        root.path(),
        &identity('a'),
        &[],
        &EnsureKernelPythonOptions::default(),
    )
    .await
    .unwrap();
    let calls = std::fs::read_to_string(root.path().join("uv.log")).unwrap();
    assert!(calls.contains("python install 3.11"));
    assert!(calls.contains(&format!("venv {} --python 3.11", venv.display())));
    assert!(
        calls.contains("sync --locked --no-default-groups --group kernel --no-config --project")
    );
    assert!(!calls.contains("--seed"));
    assert!(!calls.contains("--inexact"));
    assert!(kernel_base_ready(
        &kernel_venv_python(&venv).to_string_lossy(),
        &venv,
        &identity('a')
    ));
    assert!(!intent_path(&venv).exists());
}
