//! Durable upgrade intent, predecessor archives, and offline source rollback.

use super::{kernel_base_ready, kernel_venv_python, version::read_bootstrap_version};
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
struct UpgradeIntent {
    backup: PathBuf,
    target: String,
}

pub(in crate::kernel::bootstrap) fn intent_path(venv: &Path) -> PathBuf {
    sibling(venv, ".bootstrap-upgrade")
}

fn sibling(venv: &Path, suffix: &str) -> PathBuf {
    venv.with_file_name(format!(
        "{}{suffix}",
        venv.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn backup_directory(venv: &Path) -> anyhow::Result<PathBuf> {
    let directory = sibling(venv, ".bootstrap-backups");
    std::fs::create_dir_all(&directory)?;
    let info = std::fs::symlink_metadata(&directory)?;
    if !info.is_dir() || info.file_type().is_symlink() {
        bail!(
            "unsafe Python kernel backup directory: {}",
            directory.display()
        );
    }
    Ok(directory)
}

fn backup_name() -> anyhow::Result<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok(format!("{}-{}", now.as_millis(), uuid::Uuid::new_v4()))
}

fn valid_backup_name(name: &str) -> bool {
    name.get(..13)
        .is_some_and(|time| time.bytes().all(|c| c.is_ascii_digit()))
        && name.as_bytes().get(13) == Some(&b'-')
        && name
            .get(14..)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id))
}

pub(super) fn sync_parent(path: &Path) -> anyhow::Result<()> {
    // Windows cannot open directories as regular files for fsync.
    #[cfg(not(windows))]
    std::fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

fn remove_intent(venv: &Path) -> anyhow::Result<()> {
    let intent = intent_path(venv);
    match std::fs::remove_file(&intent) {
        Ok(()) => sync_parent(&intent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn archive(venv: &Path) -> anyhow::Result<PathBuf> {
    let destination = backup_directory(venv)?.join(backup_name()?);
    std::fs::rename(venv, &destination)?;
    sync_parent(&destination)?;
    sync_parent(venv)?;
    Ok(destination)
}

pub(super) fn archive_for_upgrade(venv: &Path, identity: &str) -> anyhow::Result<PathBuf> {
    let active = std::fs::symlink_metadata(venv)?;
    if !active.is_dir() || active.file_type().is_symlink() {
        bail!("unsafe Python kernel directory: {}", venv.display());
    }
    let backup = backup_directory(venv)?.join(backup_name()?);
    let path = intent_path(venv);
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let intent = UpgradeIntent {
        backup: backup.clone(),
        target: identity.to_string(),
    };
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temporary)?;
        writeln!(file, "{}", serde_json::to_string(&intent)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        sync_parent(&path)?;
        std::fs::rename(venv, &backup)?;
        sync_parent(&backup)?;
        sync_parent(venv)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        // If the archive exists, leave the durable intent to replay it.
        if !backup.exists() {
            remove_intent(venv)?;
        }
        return Err(error);
    }
    Ok(backup)
}

fn activate_archive(venv: &Path, backup: &Path) -> anyhow::Result<()> {
    // Preserve the incomplete or newer active copy, too. A repeated crash
    // can replay the same intent until the predecessor is active again.
    let displaced = if venv.try_exists()? {
        Some(archive(venv)?)
    } else {
        None
    };
    if let Err(error) = std::fs::rename(backup, venv) {
        if let Some(displaced) = displaced {
            std::fs::rename(displaced, venv)?;
        }
        return Err(error.into());
    }
    sync_parent(backup)?;
    sync_parent(venv)
}

pub(in crate::kernel::bootstrap) fn recover_interrupted_upgrade(
    venv: &Path,
    identity: &str,
) -> anyhow::Result<()> {
    let path = intent_path(venv);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let invalid = || format!("invalid Python kernel upgrade intent: {}", path.display());
    let intent: UpgradeIntent = serde_json::from_slice(&raw).with_context(invalid)?;
    let directory = backup_directory(venv)?;
    if intent.backup.parent() != Some(directory.as_path())
        || !intent
            .backup
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(valid_backup_name)
        || !intent.target.strip_prefix("sha256:").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
    {
        bail!("{}", invalid());
    }
    // A legacy/incomplete predecessor can lack a parseable marker or Python.
    // Once the replacement is durably committed, its readiness is sufficient
    // to finalize intent; the archive stays preserved for manual recovery.
    if intent.target == identity
        && kernel_base_ready(&kernel_venv_python(venv).to_string_lossy(), venv, identity)
    {
        return remove_intent(venv);
    }
    let info = match std::fs::symlink_metadata(&intent.backup) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return remove_intent(venv),
        Err(error) => return Err(error.into()),
    };
    if !info.is_dir()
        || info.file_type().is_symlink()
        || read_bootstrap_version(&intent.backup).is_none()
        || !kernel_venv_python(&intent.backup).exists()
    {
        bail!(
            "invalid archived Python kernel for upgrade: {}",
            intent.backup.display()
        );
    }
    if intent.target != identity
        || !kernel_base_ready(&kernel_venv_python(venv).to_string_lossy(), venv, identity)
    {
        activate_archive(venv, &intent.backup)?;
    }
    remove_intent(venv)
}

pub(in crate::kernel::bootstrap) fn restore(venv: &Path, identity: &str) -> anyhow::Result<bool> {
    let directory = sibling(venv, ".bootstrap-backups");
    if !directory.try_exists()? {
        return Ok(false);
    }
    let directory = backup_directory(venv)?;
    let mut entries = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries.into_iter().rev() {
        if !entry.file_name().to_str().is_some_and(valid_backup_name)
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let candidate = entry.path();
        if !kernel_base_ready(
            &kernel_venv_python(&candidate).to_string_lossy(),
            &candidate,
            identity,
        ) {
            continue;
        }
        activate_archive(venv, &candidate)?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn rollback(venv: &Path, previous: &Path) -> anyhow::Result<()> {
    activate_archive(venv, previous)?;
    remove_intent(venv)
}

/// Flush regular files and directory entries before publishing readiness.
pub(super) fn sync_tree(directory: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            #[cfg(not(windows))]
            std::fs::File::open(entry.path())?.sync_all()?;
            #[cfg(windows)]
            {
                // FlushFileBuffers needs GENERIC_WRITE. Preserve read-only
                // package files while temporarily opening the copied file.
                let permissions = entry.metadata()?.permissions();
                let readonly = permissions.readonly();
                if readonly {
                    let mut writable = permissions.clone();
                    writable.set_readonly(false);
                    std::fs::set_permissions(entry.path(), writable)?;
                }
                let result = std::fs::OpenOptions::new()
                    .write(true)
                    .open(entry.path())
                    .and_then(|file| file.sync_all());
                if readonly {
                    std::fs::set_permissions(entry.path(), permissions)?;
                }
                result?;
            }
        }
    }
    #[cfg(not(windows))]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// Copy without hard links: uv may overwrite package files, and a predecessor
/// must stay immutable. Preserve symlink targets verbatim (venv interpreters).
pub(super) fn copy_venv(source: &Path, destination: &Path) -> anyhow::Result<()> {
    copy_directory(source, destination, source)
}

fn copy_directory(source: &Path, destination: &Path, root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        // Publish readiness only after the entire copy and locked sync are
        // durable. Never copy a predecessor's ready marker into a partial tree.
        if entry.path() == root.join(".bootstrap-version") {
            continue;
        }
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, target)?;
            #[cfg(windows)]
            if entry.path().is_dir() {
                std::os::windows::fs::symlink_dir(link, target)?;
            } else {
                std::os::windows::fs::symlink_file(link, target)?;
            }
        } else if kind.is_dir() {
            copy_directory(&entry.path(), &target, root)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            bail!(
                "unsupported file in Python kernel: {}",
                entry.path().display()
            );
        }
    }
    std::fs::set_permissions(destination, std::fs::metadata(source)?.permissions())?;
    Ok(())
}

#[cfg(test)]
mod tests;
