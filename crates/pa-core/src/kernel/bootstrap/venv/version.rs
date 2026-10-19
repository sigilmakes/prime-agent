//! The version-file concern: the schema consts, the `.bootstrap-version` read/write.

use super::{default_rlm_extra_uv_args, BootstrapPythonSkill, BootstrapVersion, Path};

/// Schema of `.bootstrap-version`; a mismatch rebuilds the venv.
pub(super) const BOOTSTRAP_SCHEMA: u64 = 10;
pub(super) const STATE_SNAPSHOT_REQUIREMENT: &str = "dill";
const BOOTSTRAP_VERSION_FILE: &str = ".bootstrap-version";
pub(super) fn read_bootstrap_version(venv: &Path) -> Option<BootstrapVersion> {
    let raw = std::fs::read_to_string(venv.join(BOOTSTRAP_VERSION_FILE)).ok()?;
    let parsed: BootstrapVersion = serde_json::from_str(&raw).ok()?;
    (parsed.schema > 0).then_some(parsed)
}

fn extra_uv_args_match(a: Option<&[String]>, b: &[&str]) -> bool {
    match a {
        None => false,
        Some(a) => a.iter().map(String::as_str).eq(b.iter().copied()),
    }
}

/// Identity of one recorded skill: the install root is the package path, and
/// the editable install follows that path.
pub(super) fn bootstrap_skill_key(skill: &BootstrapPythonSkill) -> String {
    format!("{}\u{0}{}", skill.import_name, skill.package_path)
}

/// True when the recorded installs cover every current skill at the same path with the same
/// pyproject hash. Extra recorded skills from other sessions are fine: the venv is a shared cache.
pub(super) fn recorded_skills_cover(
    recorded: Option<&[BootstrapPythonSkill]>,
    current: &[BootstrapPythonSkill],
) -> bool {
    if current.is_empty() {
        return true;
    }
    let Some(recorded) = recorded else {
        return false;
    };
    current.iter().all(|skill| {
        recorded.iter().any(|entry| {
            bootstrap_skill_key(entry) == bootstrap_skill_key(skill)
                && entry.pyproject_path == skill.pyproject_path
                && entry.pyproject_hash == skill.pyproject_hash
        })
    })
}

pub(super) fn bootstrap_base_version_current(
    version: Option<BootstrapVersion>,
    runtime_identity: &str,
) -> bool {
    match version {
        Some(version) => {
            version.schema == BOOTSTRAP_SCHEMA
                && version.runtime.as_deref() == Some(runtime_identity)
                && version.snapshot.as_deref() == Some(STATE_SNAPSHOT_REQUIREMENT)
                && extra_uv_args_match(
                    version.extra_uv_args.as_deref(),
                    &default_rlm_extra_uv_args(),
                )
        }
        None => false,
    }
}

pub(super) fn bootstrap_version_current(
    version: Option<&BootstrapVersion>,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    version.is_some_and(|version| {
        bootstrap_base_version_current(Some(version.clone()), runtime_identity)
            && recorded_skills_cover(version.python_skills.as_deref(), python_skills)
    })
}

pub(crate) fn write_bootstrap_version(
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<()> {
    write_bootstrap_version_with_swap(venv, runtime_identity, python_skills, |from, to| {
        std::fs::rename(from, to)
    })
}

fn write_bootstrap_version_with_swap(
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    mut swap: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let version = BootstrapVersion {
        schema: BOOTSTRAP_SCHEMA,
        runtime: Some(runtime_identity.to_string()),
        snapshot: Some(STATE_SNAPSHOT_REQUIREMENT.to_string()),
        extra_uv_args: Some(
            default_rlm_extra_uv_args()
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        python_skills: Some(python_skills.to_vec()),
    };
    let marker = venv.join(BOOTSTRAP_VERSION_FILE);
    let temporary = venv.join(".bootstrap-version.tmp");
    let serialized = format!("{}\n", serde_json::to_string(&version)?);
    let mut last_error = None;
    for attempt in 0..3 {
        let result: anyhow::Result<()> = (|| {
            use std::io::Write;
            // A killed writer can leave a temp file. Unlink it instead of
            // following a stale symlink or truncating the published marker.
            match std::fs::remove_file(&temporary) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            crate::platform::perms::set_private_mode(&mut options);
            let mut file = options.open(&temporary)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            drop(file);
            swap(&temporary, &marker)?;
            super::recovery::sync_parent(&marker)
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    let _ = std::fs::remove_file(temporary);
    Err(last_error.expect("three marker attempts ran"))
}

/// The parsed `.bootstrap-version` plus its raw text (the probe-memo key
/// input), in one read.
pub(super) fn read_bootstrap_version_raw(venv: &Path) -> (Option<BootstrapVersion>, String) {
    let raw = std::fs::read_to_string(venv.join(BOOTSTRAP_VERSION_FILE)).unwrap_or_default();
    let parsed: Option<BootstrapVersion> = serde_json::from_str(&raw).ok();
    (parsed.filter(|v| v.schema > 0), raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_schema_ten_camel_case_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = serde_json::json!({
            "schema": 10, "runtime": "sha256:legacy", "snapshot": "dill",
            "extraUvArgs": default_rlm_extra_uv_args(),
            "pythonSkills": [{"importName": "edit", "packagePath": "/skills/edit", "pyprojectPath": "/skills/edit/pyproject.toml", "pyprojectHash": "hash"}]
        });
        std::fs::write(dir.path().join(BOOTSTRAP_VERSION_FILE), marker.to_string()).unwrap();
        let version = read_bootstrap_version(dir.path()).unwrap();
        assert!(bootstrap_base_version_current(
            Some(version.clone()),
            "sha256:legacy"
        ));
        write_bootstrap_version(
            dir.path(),
            "sha256:legacy",
            version.python_skills.as_ref().unwrap(),
        )
        .unwrap();
        let written: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join(BOOTSTRAP_VERSION_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(written, marker);
    }

    #[test]
    fn marker_retries_busy_swap_and_never_truncates_old_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_bootstrap_version(dir.path(), "old", &[]).unwrap();
        let old = std::fs::read(dir.path().join(BOOTSTRAP_VERSION_FILE)).unwrap();
        let mut attempts = 0;
        write_bootstrap_version_with_swap(dir.path(), "new", &[], |from, to| {
            attempts += 1;
            assert_eq!(std::fs::read(to).unwrap(), old);
            if attempts < 3 {
                Err(std::io::Error::other("busy marker"))
            } else {
                std::fs::rename(from, to)
            }
        })
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(
            read_bootstrap_version(dir.path())
                .unwrap()
                .runtime
                .as_deref(),
            Some("new")
        );
        let current = std::fs::read(dir.path().join(BOOTSTRAP_VERSION_FILE)).unwrap();
        assert!(
            write_bootstrap_version_with_swap(dir.path(), "never", &[], |_, _| Err(
                std::io::Error::other("busy marker")
            ))
            .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join(BOOTSTRAP_VERSION_FILE)).unwrap(),
            current
        );
        assert!(!dir.path().join(".bootstrap-version.tmp").exists());
    }

    #[test]
    fn failed_marker_swap_preserves_previous_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_bootstrap_version(dir.path(), "old", &[]).unwrap();
        let marker = dir.path().join(BOOTSTRAP_VERSION_FILE);
        let previous = std::fs::read(&marker).unwrap();
        std::fs::create_dir(dir.path().join(".bootstrap-version.tmp")).unwrap();
        assert!(write_bootstrap_version(dir.path(), "new", &[]).is_err());
        assert_eq!(std::fs::read(marker).unwrap(), previous);
    }
}
