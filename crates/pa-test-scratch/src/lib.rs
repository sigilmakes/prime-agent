//! Test-only scratch ownership: every test binary that links this crate gets a private `TMPDIR`,
//! removed when the binary exits.
//!
//! The suites create their fixtures with `std::env::temp_dir()`, and a fixture a test never removes
//! (a panicking test, an early return, a child that outlives its assertion) used to land in the
//! shared `/tmp` and stay there. Pointing `TMPDIR` at one directory per test binary makes the
//! kernel's own temporary-directory contract do the cleanup: the root is removed at exit, whatever
//! the tests left behind.
//!
//! Linking is the switch: the hook below is a `.init_array` entry, so it runs before `main` and
//! needs no call site. A test target opts in with `use pa_test_scratch as _;`, which is why the
//! crate is a dev-dependency that no test code names.

#[cfg(unix)]
mod hook {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// Prefix of the per-binary root. Short on purpose: Unix socket paths under `TMPDIR` must fit
    /// `sun_path`.
    const PREFIX: &str = "pa-tmp-";

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    /// The private root, created on first use with `TMPDIR` pointing at it. Idempotent: the hook
    /// and any later call share one root.
    pub(crate) fn root() -> &'static PathBuf {
        ROOT.get_or_init(|| {
            let root = std::env::temp_dir().join(format!("{PREFIX}{}", std::process::id()));
            let _ = std::fs::create_dir_all(&root);
            // The scratch root is the new temporary directory for this process and every child it
            // spawns, so a test that writes a fixture without naming a path still lands here.
            std::env::set_var("TMPDIR", &root);
            // Statics are not dropped at exit, so the removal rides `atexit`.
            unsafe {
                libc::atexit(remove);
            }
            root
        })
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
        let expected = format!("pa-tmp-{}", std::process::id());
        assert!(
            root.ends_with(&expected),
            "TMPDIR is {root:?}, not {expected}"
        );
    }
}
