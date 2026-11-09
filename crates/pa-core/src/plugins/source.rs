//! Shared source resolution for settings-declared plugins. The session host and
//! the session-free fact pass read the same explicit path; there is no ambient
//! discovery and relative paths resolve against the agent directory.

use std::path::{Path, PathBuf};

use super::PluginError;

/// Read one explicitly declared plugin chunk. Returns the resolved path so a
/// caller can key caches by the actual file and report failures precisely.
pub(crate) fn read(
    name: &str,
    path: &Path,
    agent_dir: &Path,
) -> Result<(PathBuf, String), PluginError> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        agent_dir.join(path)
    };
    let source = std::fs::read_to_string(&resolved).map_err(|error| {
        PluginError::Invalid(format!(
            "plugin {name}: read {}: {error}",
            resolved.display()
        ))
    })?;
    Ok((resolved, source))
}
