//! Venv discovery, build, lock, and the shared `.bootstrap-version` cache:
//! the machine state behind [`super::ensure_kernel_python`]. The version
//! file is a cross-session cache, not a per-session manifest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{anyhow, Context};
use sha2::Digest;

use super::{
    default_rlm_extra_uv_args, EnsureKernelPythonOptions, KernelPythonSkill,
    DEFAULT_RLM_EXTRA_PACKAGES,
};

// The concern children (the flows + the shared record stay in the
// composition root).
mod layout;
mod probe;
pub(super) mod recovery;
mod runtime_source;
mod skills;
mod uv;
mod version;

use layout::home_dir;
pub(crate) use layout::{expand_home, resolve_writable_kernel_venv_dir};
pub use layout::{kernel_venv_dir, kernel_venv_python};
pub use probe::invalidate_runtime_probe_cache;
#[cfg(test)]
use probe::{installed_rlm_dir, lock_probe_memo, runtime_probe_key};
// The memo-clear helper and the live-probe package-dir walk exist only behind
// the unix tests (see their gates in probe.rs and tests.rs).
#[cfg(all(test, unix))]
use probe::{clear_in_process_probe_memo_for_tests, installed_package_dir};
pub(crate) use probe::{
    has_prime_agent_runtime, missing_python_skill_import_labels, missing_rlm_extra_import_labels,
};
use probe::{has_prime_agent_runtime_memoized, installed_runtime_identity};
use runtime_source::collect_python_files;
pub(super) use runtime_source::{hash_runtime_source, require_runtime_source_dir};
pub(super) use runtime_source::{package_dir, packaged_runtime_dir};
#[cfg(test)]
use skills::{
    file_content_hash, read_python_skill_dependency_names, read_python_skill_project_name,
};
pub(crate) use skills::{normalize_python_skills, BootstrapPythonSkill};
pub(crate) use uv::ensure_uv;
#[cfg(test)]
use uv::windows_executable_candidates;
use version::{
    bootstrap_base_version_current, bootstrap_skill_key, bootstrap_version_current,
    read_bootstrap_version, read_bootstrap_version_raw, write_bootstrap_version,
};
#[cfg(test)]
use version::{recorded_skills_cover, BOOTSTRAP_SCHEMA};

const PYTHON_VERSION: &str = "3.11";
pub(crate) const BOOTSTRAP_LOCK_NAME: &str = ".bootstrap.lock";
pub(crate) const BOOTSTRAP_LOCK_RETRY_MS: u64 = 100;
pub(crate) const BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS: u64 = 30_000;
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapVersion {
    schema: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "extra_uv_args")]
    extra_uv_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "python_skills")]
    python_skills: Option<Vec<BootstrapPythonSkill>>,
}

async fn run_async(command: &str, args: &[String]) -> anyhow::Result<()> {
    run_async_in_venv(command, args, None).await
}

async fn run_async_in_venv(
    command: &str,
    args: &[String],
    venv: Option<&Path>,
) -> anyhow::Result<()> {
    // Run on a blocking thread: the bootstrap is an IO-bound child process.
    let command = command.to_string();
    let args = args.to_vec();
    let venv = venv.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(&command);
        child.args(&args).stdin(Stdio::null());
        if let Some(venv) = venv {
            child.env("UV_PROJECT_ENVIRONMENT", venv);
        }
        // Hidden window on Windows (TS `spawnHidden`).
        crate::platform::process::set_no_window(&mut child);
        // CPython must read UTF-8 .pth files even under a Windows legacy
        // code page (TS bootstrap.ts:429).
        if cfg!(windows) {
            child.env("PYTHONUTF8", "1");
        }
        let status = child
            .status()
            .with_context(|| format!("failed to spawn {command}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "{} {} failed with exit code {}",
                command,
                args.join(" "),
                status.code().unwrap_or(-1)
            ))
        }
    })
    .await
    .map_err(|e| anyhow!("bootstrap task join failed: {e}"))?
}

#[derive(Clone, Copy)]
enum SyncMode {
    Fresh,
    PreserveInstalled,
}

async fn sync_locked_runtime(
    uv: &str,
    venv: &Path,
    source: &Path,
    mode: SyncMode,
) -> anyhow::Result<()> {
    let mut args = vec!["sync".to_string(), "--locked".to_string()];
    if matches!(mode, SyncMode::PreserveInstalled) {
        args.extend(
            ["--inexact", "--reinstall-package", "prime-agent-runtime"].map(str::to_string),
        );
    }
    args.extend(
        [
            "--no-default-groups",
            "--group",
            "kernel",
            "--no-config",
            "--project",
        ]
        .map(str::to_string),
    );
    args.push(source.to_string_lossy().to_string());
    args.extend(["--python", PYTHON_VERSION].map(str::to_string));
    run_async_in_venv(uv, &args, Some(venv)).await
}

/// Build a replacement at the original absolute path; never mutate its archive.
pub(super) async fn rebuild_kernel_venv(
    uv: &str,
    venv: &Path,
    source_dir: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(venv.parent().context("kernel venv has no parent")?)?;
    let previous = if venv.try_exists()? {
        Some(recovery::archive_for_upgrade(venv, runtime_identity)?)
    } else {
        None
    };
    let result = async {
        if let Some(previous) = previous.as_ref().filter(|p| kernel_venv_python(p).exists()) {
            recovery::copy_venv(previous, venv)?;
            sync_locked_runtime(uv, venv, source_dir, SyncMode::PreserveInstalled).await?;
        } else {
            run_async(
                uv,
                &[
                    "python".to_string(),
                    "install".to_string(),
                    PYTHON_VERSION.to_string(),
                ],
            )
            .await?;
            run_async(
                uv,
                &[
                    "venv".to_string(),
                    venv.to_string_lossy().to_string(),
                    "--python".to_string(),
                    PYTHON_VERSION.to_string(),
                ],
            )
            .await?;
            sync_locked_runtime(uv, venv, source_dir, SyncMode::Fresh).await?;
        }
        let recorded = previous
            .as_ref()
            .and_then(|archive| read_bootstrap_version(archive))
            .and_then(|v| v.python_skills)
            .unwrap_or_default();
        recovery::sync_tree(venv)?;
        // Checkpoint the base before installing skills, so a killed skill sync
        // resumes without rebuilding or discarding user packages.
        write_bootstrap_version(venv, runtime_identity, &recorded)?;
        sync_python_skills(
            uv,
            venv,
            &kernel_venv_python(venv),
            source_dir,
            runtime_identity,
            python_skills,
            options,
        )
        .await?;
        if !kernel_base_ready(
            &kernel_venv_python(venv).to_string_lossy(),
            venv,
            runtime_identity,
        ) {
            anyhow::bail!("locked Python runtime failed its readiness check");
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        if let Some(previous) = previous {
            recovery::rollback(venv, &previous)
                .context(format!("upgrade failed ({error:#}); restoring predecessor"))?;
        } else if venv.try_exists()? {
            // Preserve even a partial fresh environment for diagnosis/recovery.
            recovery::archive(venv)?;
        }
        return Err(error);
    }
    // Keep upgrade intent until the next successful startup validates this copy.
    Ok(())
}

/// Install/refresh the editable Python skills recorded in the version file. Only skills missing or
/// changed are installed. Per-skill failures warn and continue.
pub(crate) async fn sync_python_skills(
    uv: &str,
    venv: &Path,
    python: &Path,
    source_dir: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    let version = read_bootstrap_version(venv);
    // Previously installed skills still present on disk: their records carry over so
    // sessions with different skill sets share one venv cache.
    let current_python_skills: HashMap<String, BootstrapPythonSkill> = version
        .as_ref()
        .and_then(|v| v.python_skills.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|recorded| Path::new(&recorded.package_path).is_dir())
        .map(|s| (bootstrap_skill_key(&s), s))
        .collect();
    let python_str = python.to_string_lossy().to_string();
    let mut installed: HashMap<String, BootstrapPythonSkill> = current_python_skills;
    let mut missing: Vec<&BootstrapPythonSkill> = Vec::new();
    for skill in python_skills {
        let key = bootstrap_skill_key(skill);
        if installed.get(&key).is_some_and(|existing| {
            existing.pyproject_path == skill.pyproject_path
                && existing.pyproject_hash == skill.pyproject_hash
        }) {
            continue;
        }
        missing.push(skill);
    }
    // Install one skill at a time and checkpoint it immediately. A killed
    // process must not lose completed installs or records not yet visited.
    let mut pending = missing;
    while !pending.is_empty() {
        let index = pending
            .iter()
            .position(|skill| {
                let dependencies = skills::read_python_skill_dependency_names(skill);
                !pending.iter().any(|other| {
                    bootstrap_skill_key(other) != bootstrap_skill_key(skill)
                        && dependencies.contains(
                            &skills::read_python_skill_project_name(other)
                                .replace('_', "-")
                                .to_lowercase(),
                        )
                })
            })
            .unwrap_or(0);
        let skill = pending.remove(index);
        let mut install_args = vec![
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            python_str.clone(),
            source_dir.to_string_lossy().to_string(),
            "--editable".to_string(),
            skill.package_path.clone(),
        ];
        // Cyclic local dependencies cannot be ordered; offer their trusted
        // editable source too rather than resolving a namesake from an index.
        let dependencies = skills::read_python_skill_dependency_names(skill);
        let local_dependencies: Vec<_> = python_skills
            .iter()
            .filter(|other| {
                bootstrap_skill_key(other) != bootstrap_skill_key(skill)
                    && dependencies.contains(
                        &skills::read_python_skill_project_name(other)
                            .replace('_', "-")
                            .to_lowercase(),
                    )
                    && !installed
                        .get(&bootstrap_skill_key(other))
                        .is_some_and(|recorded| {
                            recorded.pyproject_path == other.pyproject_path
                                && recorded.pyproject_hash == other.pyproject_hash
                        })
            })
            .collect();
        for dependency in &local_dependencies {
            install_args.extend(["--editable".to_string(), dependency.package_path.clone()]);
        }
        match run_async(uv, &install_args).await {
            Ok(()) => {
                installed.insert(bootstrap_skill_key(skill), skill.clone());
                for dependency in local_dependencies {
                    installed.insert(bootstrap_skill_key(dependency), dependency.clone());
                }
                let mut checkpoint: Vec<_> = installed.values().cloned().collect();
                checkpoint.sort_by(|a, b| {
                    a.package_path
                        .cmp(&b.package_path)
                        .then(a.import_name.cmp(&b.import_name))
                });
                recovery::sync_tree(venv)?;
                write_bootstrap_version(venv, runtime_identity, &checkpoint)?;
            }
            Err(error) => {
                installed.remove(&bootstrap_skill_key(skill));
                options.report(&format!(
                    "Warning: Python skill {} failed to install and will be unavailable: {error}",
                    skill.import_name
                ));
            }
        }
        pending.retain(|skill| {
            !installed
                .get(&bootstrap_skill_key(skill))
                .is_some_and(|recorded| {
                    recorded.pyproject_path == skill.pyproject_path
                        && recorded.pyproject_hash == skill.pyproject_hash
                })
        });
    }
    let mut merged: Vec<BootstrapPythonSkill> = installed.into_values().collect();
    merged.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    write_bootstrap_version(venv, runtime_identity, &merged)
}

pub(crate) fn kernel_base_ready(python: &str, venv: &Path, runtime_identity: &str) -> bool {
    let (version, raw) = read_bootstrap_version_raw(venv);
    bootstrap_base_version_current(version, runtime_identity)
        && has_prime_agent_runtime_memoized(
            python,
            runtime_identity,
            &raw,
            &installed_runtime_identity(Path::new(python), venv),
            venv,
        )
}

pub(crate) fn kernel_ready(
    python: &str,
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    let (version, raw) = read_bootstrap_version_raw(venv);
    bootstrap_version_current(version.as_ref(), runtime_identity, python_skills)
        && has_prime_agent_runtime_memoized(
            python,
            runtime_identity,
            &raw,
            &installed_runtime_identity(Path::new(python), venv),
            venv,
        )
}

#[cfg(test)]
mod tests;
