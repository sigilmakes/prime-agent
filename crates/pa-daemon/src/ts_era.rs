//! Legacy TypeScript state belongs to its runtime and to rollback snapshots.
//! Ordinary Rust startup must not delete or migrate it, even with a separate socket.

use std::path::Path;

/// Retained boot hook: legacy state is deliberately left untouched.
/// Cleanup requires a separate, explicitly authorized maintenance operation.
pub(crate) fn sweep_ts_era_leftovers(_agent_dir: &Path) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    #[test]
    fn ordinary_boot_hook_preserves_legacy_runtime_and_shared_state() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        for (relative, contents) in [
            ("prime-inference-models-cache.json", "legacy catalog"),
            (
                "models/prime-inference-models-cache.json",
                "current catalog",
            ),
            ("worker-snapshot-cache/u-1/snapshot.json", "legacy snapshot"),
            ("kernel-venv/pyvenv.cfg", "legacy interpreter"),
            ("kernel-venv/.bootstrap-version", "legacy runtime identity"),
            ("agent-traces-outbox.json", "legacy inert outbox marker"),
            ("agent-traces-outbox/hash.json", "legacy inert outbox entry"),
            ("cron-jobs.json", "schedule"),
            ("harness/refinements.jsonl", "retained local history"),
        ] {
            let path = agent_dir.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let before = snapshot(&agent_dir);
        assert_eq!(sweep_ts_era_leftovers(&agent_dir), 0);
        assert_eq!(snapshot(&agent_dir), before);
        // Repeated startup is also non-destructive.
        assert_eq!(sweep_ts_era_leftovers(&agent_dir), 0);
        assert_eq!(snapshot(&agent_dir), before);
    }
}
