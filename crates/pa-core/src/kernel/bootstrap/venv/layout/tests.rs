//! Process-isolated tests: no environment mutation can race other kernel tests.

use super::*;

#[test]
fn rust_kernel_namespaces_preserve_legacy_fixtures_and_private_overrides() {
    const CASE_ENV: &str = "PA_RUNTIME_NAMESPACING_CASE";
    if let Ok(case) = std::env::var(CASE_ENV) {
        let home = home_dir();
        let data = std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from);
        let legacy_home = home.join(".prime/agent/kernel-venv");
        let legacy_xdg = data.join("prime/agent/kernel-venv");
        let sentinel = b"TypeScript runtime: retain for rollback";
        let fallback = case.starts_with("fallback");
        if fallback {
            std::fs::write(home.join(".prime"), b"block primary parent").unwrap();
        } else {
            std::fs::create_dir_all(&legacy_home).unwrap();
            std::fs::write(legacy_home.join("sentinel"), sentinel).unwrap();
        }
        std::fs::create_dir_all(&legacy_xdg).unwrap();
        std::fs::write(legacy_xdg.join("sentinel"), sentinel).unwrap();

        let expected = match case.as_str() {
            "default" | "xdg-unset" => home.join(".prime/agent/kernel-venv-rust"),
            "fallback" | "fallback-xdg-unset" => data.join("prime/agent/kernel-venv-rust"),
            "override" => home.join("private-kernel"),
            "legacy-override" => legacy_home.clone(),
            _ => panic!("unknown private fixture case: {case}"),
        };
        if !fallback {
            assert_eq!(kernel_venv_dir(), expected);
        }
        assert_eq!(resolve_writable_kernel_venv_dir().unwrap(), expected);
        assert_eq!(
            xdg_kernel_venv_dir(),
            data.join("prime/agent/kernel-venv-rust")
        );
        if !fallback {
            assert_eq!(
                std::fs::read(legacy_home.join("sentinel")).unwrap(),
                sentinel
            );
        }
        assert_eq!(
            std::fs::read(legacy_xdg.join("sentinel")).unwrap(),
            sentinel
        );
        // Resolving a directory may create its parent, never a runtime or an upgrade backup.
        assert!(!expected.join("pyvenv.cfg").exists());
        assert!(!expected.join(".bootstrap-version").exists());
        assert!(!expected.with_extension("bootstrap.lock").exists());
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let test_name = format!(
        "{}::rust_kernel_namespaces_preserve_legacy_fixtures_and_private_overrides",
        module_path!().split_once("::").unwrap().1
    );
    for case in [
        "default",
        "fallback",
        "xdg-unset",
        "fallback-xdg-unset",
        "override",
        "legacy-override",
    ] {
        let root = fixture.path().join(case);
        let home = root.join("home");
        let data = root.join("data");
        std::fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &test_name, "--nocapture"])
            .env_clear()
            .env(CASE_ENV, case)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("TMPDIR", &root);
        if !case.ends_with("xdg-unset") {
            command.env("XDG_DATA_HOME", &data);
        }
        if case == "override" {
            command.env("PRIME_AGENT_KERNEL_VENV", "~/private-kernel");
        } else if case == "legacy-override" {
            // Explicit caller-owned overrides remain authoritative, even for the old name.
            command.env(
                "PRIME_AGENT_KERNEL_VENV",
                home.join(".prime/agent/kernel-venv"),
            );
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "private {case} fixture failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
