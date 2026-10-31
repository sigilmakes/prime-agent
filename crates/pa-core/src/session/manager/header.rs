//! The header + rlm-depth concern: the first-line header read, the depth
//! validation, and the root depth from the environment.

use super::{Path, SessionHeader};

#[must_use]
pub fn read_session_header(file_path: &Path) -> Option<SessionHeader> {
    use std::io::BufRead;
    let mut first = Vec::new();
    std::io::BufReader::new(std::fs::File::open(file_path).ok()?)
        .read_until(b'\n', &mut first).ok()?;
    if first.pop() != Some(b'\n') {
        return None;
    }
    let row = crate::session_log::Row::decode(&first, 0).ok()?;
    match serde_json::from_value::<pa_types::session::FileEntry>(row.body).ok()? {
        pa_types::session::FileEntry::Header { header } => Some(header),
        _ => None,
    }
}

pub(super) fn is_valid_rlm_depth(value: Option<u64>) -> bool {
    value.is_some_and(|depth| depth < u64::MAX)
}

pub(super) fn root_rlm_depth_from_env() -> u64 {
    match std::env::var("RLM_DEPTH") {
        Ok(value) if value.is_empty() => 0,
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|depth| is_valid_rlm_depth(Some(*depth)))
            .unwrap_or_else(|| panic!("RLM_DEPTH must be a non-negative integer")),
        Err(_) => 0,
    }
}

pub(super) fn resolve_session_rlm_depth(header: &SessionHeader, _session_path: &Path) -> u64 {
    if is_valid_rlm_depth(header.rlm_depth) {
        return header.rlm_depth.unwrap();
    }
    0
}
