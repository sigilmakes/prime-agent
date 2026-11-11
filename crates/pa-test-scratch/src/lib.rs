//! Test-only scratch ownership: every test binary that links this crate gets a private `TMPDIR`,
//! removed when the binary exits.
//!
//! The suites create their fixtures with `std::env::temp_dir()`, and a fixture a test never removes
//! (a panicking test, an early return, a child that outlives its assertion) used to land in the
//! shared `/tmp` and stay there. Pointing `TMPDIR` at one directory per test binary makes the
//! kernel's own temporary-directory contract do the cleanup: the root is removed at exit, whatever
//! the tests left behind.
//!
//! The root stays short on purpose. Unix socket paths must fit `sun_path` (103 portable bytes) and
//! the daemon relocates its endpoints to `/tmp` when a `TMPDIR`-relative probe overruns that
//! budget, so a deep root silently changes which socket every fixture uses. The workspace gate
//! passes its own per-target root in `PA_TEST_SCRATCH_ROOT`; otherwise the root is
//! `<temp_dir>/pa-t-<pid>` for a short temp directory and `/tmp/pa-t-<pid>` for a deep one.
//!
//! Linking is the switch: the hook below is a `.init_array` entry, so it runs before `main` and
//! needs no call site. A test target opts in with `use pa_test_scratch as _;`, which is why the
//! crate is a dev-dependency that no test code names.

#[cfg(unix)]
mod hook {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// Environment variable carrying a root the runner already owns, such as the workspace
    /// gate's per-target directory. Used verbatim so the runner's own cleanup still applies.
    const RUNNER_ROOT: &str = "PA_TEST_SCRATCH_ROOT";

    /// The longest `temp_dir()` that can host a scratch root without eating the `sun_path`
    /// budget: `/tmp`, `/var/tmp`, and short `/run/user/<uid>` paths qualify.
    const SHORT_TEMP_DIR: usize = 16;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    /// The private root, created on first use with `TMPDIR` pointing at it. Idempotent: the hook
    /// and any later call share one root.
    pub(crate) fn root() -> &'static PathBuf {
        ROOT.get_or_init(|| {
            let (root, owned) = select();
            let _ = std::fs::create_dir_all(&root);
            // The scratch root is the new temporary directory for this process and every child it
            // spawns, so a test that writes a fixture without naming a path still lands here.
            std::env::set_var("TMPDIR", &root);
            // Statics are not dropped at exit, so the removal rides `atexit`. A runner-owned root
            // stays: one root can serve several binaries (doctests), and the runner removes it.
            if owned {
                unsafe {
                    libc::atexit(remove);
                }
            }
            root
        })
    }

    /// The root this process uses, and whether this process owns its removal.
    fn select() -> (PathBuf, bool) {
        if let Some(runner) = std::env::var_os(RUNNER_ROOT).filter(|value| !value.is_empty()) {
            return (PathBuf::from(runner), false);
        }
        // A child re-execs its own test binary for a fixture helper, and some of those children
        // are killed rather than exiting. They inherit the parent's root and use it as-is instead
        // of minting one of their own that no one is left to remove.
        let temp = std::env::temp_dir();
        if temp
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("pa-t-"))
        {
            return (temp, false);
        }
        let name = format!("pa-t-{}", std::process::id());
        let root = if temp.as_os_str().as_encoded_bytes().len() <= SHORT_TEMP_DIR {
            temp.join(name)
        } else {
            Path::new("/tmp").join(name)
        };
        (root, true)
    }

    extern "C" fn remove() {
        if let Some(root) = ROOT.get() {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    extern "C" fn install() {
        let _ = root();
    }

    #[used]
    #[cfg_attr(
        any(target_os = "linux", target_os = "android"),
        link_section = ".init_array"
    )]
    static INSTALL: extern "C" fn() = install;
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_test_binary_runs_with_its_own_temporary_directory() {
        let root = std::env::temp_dir();
        let own = format!("pa-t-{}", std::process::id());
        let runner = std::env::var_os("PA_TEST_SCRATCH_ROOT").map(std::path::PathBuf::from);
        assert!(
            root.ends_with(&own) || runner.as_deref() == Some(root.as_path()),
            "TMPDIR is {root:?}, neither a {own} root nor the runner's scratch root"
        );
    }
}
