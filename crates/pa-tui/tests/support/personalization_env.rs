//! Test-owned process environment. Call only while holding the integration binary's run lock.
use std::ffi::OsString;
use std::path::Path;

pub struct IsolatedEnv {
    saved: Vec<(OsString, OsString)>,
}

impl IsolatedEnv {
    pub fn new(root: &Path) -> Self {
        let saved: Vec<_> = std::env::vars_os().collect();
        let path = std::env::var_os("PATH");
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        if let Some(path) = path {
            std::env::set_var("PATH", path);
        }
        for (key, relative) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("TMPDIR", "tmp"),
            ("PRIME_AGENT_CODING_AGENT_DIR", "agent"),
            ("RLM_GLOBAL_HARNESS_STATE_DIR", "harness-global"),
            ("RLM_HARNESS_STATE_DIR", "harness"),
            ("RLM_SESSION_DIR", "session"),
        ] {
            let dir = root.join(relative);
            std::fs::create_dir_all(&dir).expect("isolated test directory");
            std::env::set_var(key, dir);
        }
        std::env::set_var("DO_NOT_TRACK", "1");
        Self { saved }
    }
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        let current: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
        for key in current {
            std::env::remove_var(key);
        }
        for (key, value) in &self.saved {
            std::env::set_var(key, value);
        }
    }
}
