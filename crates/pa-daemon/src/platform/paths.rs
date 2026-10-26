//! Per-OS Rust daemon endpoint naming: Unix socket files under
//! `<tmpdir>/prime-agent-rust-<uid>/`; Windows named pipes in the `\\.\\pipe\\`
//! namespace. Rust defaults stay separate from the TypeScript runtime.
use std::path::{Path, PathBuf};

use crate::paths::hash_key;

/// Default directory holding daemon socket files (Unix).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = pa_core::platform::process::current_user_id();
    let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    socket_dir_for(&tmp, uid)
}

/// Reserve the longest generated endpoint before selecting a directory: a socket path the
/// kernel cannot address is useless to every client that does not re-anchor through /proc,
/// which includes the tests and any non-Rust reader of the descriptor. Only `/proc`-relative
/// `bind`/`connect` survives a deep `TMPDIR`, so the directory moves to `/tmp` instead.
#[cfg(unix)]
fn socket_dir_for(tmp: &Path, uid: u32) -> PathBuf {
    let name = format!("prime-agent-rust-{uid}");
    let dir = tmp.join(&name);
    if std::os::unix::net::SocketAddr::from_pathname(
        dir.join("worker-000000000000-000000000000.sock"),
    )
    .is_err()
    {
        return Path::new("/tmp").join(name);
    }
    dir
}

/// The socket-dir half of a discovery state root on Windows. Keep Rust
/// discovery state separate from the TypeScript runtime.
#[cfg(not(unix))]
#[must_use]
pub fn socket_dir() -> PathBuf {
    std::env::temp_dir().join("prime-agent-rust-user")
}

/// Default supervisor endpoint: `daemon.sock` in the socket dir (Unix) or
/// the fixed daemon pipe name (Windows).
#[cfg(unix)]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join("daemon.sock")
}

#[cfg(not(unix))]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    PathBuf::from(r"\\.\pipe\prime-agent-rust-daemon")
}

/// Worker endpoint next to the supervisor's: hashed supervisor key plus the
/// worker id prefix (TS `workerSocketPath`).
#[cfg(unix)]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    socket_dir().join(format!(
        "worker-{key}-{}.sock",
        &worker_id[..12.min(worker_id.len())]
    ))
}

#[cfg(not(unix))]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    PathBuf::from(format!(
        r"\\.\pipe\prime-agent-rust-worker-{key}-{}",
        &worker_id[..12.min(worker_id.len())]
    ))
}

// Socket-filesystem identity is the shared platform contract
// `pa_types::platform::socket_identity`: the same helper serves stale-file
// cleanup here and direct-transport ticket validation in the clients.

pub use pa_types::daemon::SocketIdentity;
pub use pa_types::platform::socket_identity;
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn default_endpoints_are_separate_from_typescript_for_the_real_uid() {
        let uid = pa_core::platform::process::current_user_id();
        let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let dir = socket_dir_for(&tmp, uid);
        let ts_dir = tmp.join(format!("prime-agent-{uid}"));
        let ts_daemon = ts_dir.join("daemon.sock");
        let worker_id = "0123456789abcdef";
        // Use the same supervisor input for both runtimes: namespace separation
        // must not rely on their supervisor hashes happening to differ.
        let key = hash_key(&ts_daemon.to_string_lossy(), 12);
        let worker_name = format!("worker-{key}-0123456789ab.sock");
        let daemon = default_daemon_socket_path();
        let worker = worker_socket_path(&ts_daemon, worker_id);

        assert_eq!(
            (socket_dir(), daemon.clone(), worker.clone()),
            (dir.clone(), dir.join("daemon.sock"), dir.join(&worker_name))
        );
        assert_ne!(dir, ts_dir);
        assert_ne!(daemon, ts_daemon);
        assert_ne!(worker, ts_dir.join(worker_name));
    }

    #[cfg(unix)]
    #[test]
    fn generated_endpoints_fit_even_when_tmpdir_is_deep() {
        use std::os::unix::net::SocketAddr;

        for length in [20, 65, 120] {
            let tmp = PathBuf::from(format!("/{}", "d".repeat(length - 1)));
            let dir = socket_dir_for(&tmp, 501);
            let worker = dir.join("worker-000000000000-000000000000.sock");
            assert!(SocketAddr::from_pathname(&worker).is_ok());
            if length >= 65 {
                assert_eq!(dir, Path::new("/tmp/prime-agent-rust-501"));
            } else {
                assert_eq!(dir, tmp.join("prime-agent-rust-501"));
            }
        }
    }

    /// `/proc/self/fd` re-anchoring only helps a caller that holds the parent descriptor;
    /// a client that connects by the descriptor's path string cannot address a deep socket,
    /// so a deep `TMPDIR` loses to a connectable directory on every Unix.
    #[cfg(target_os = "linux")]
    #[test]
    fn deep_tmpdir_yields_to_a_connectable_directory() {
        let tmp = PathBuf::from(format!("/{}", "d".repeat(120)));
        assert_eq!(
            socket_dir_for(&tmp, 501),
            Path::new("/tmp/prime-agent-rust-501")
        );
    }

    #[cfg(unix)]
    #[test]
    fn short_tmpdir_keeps_the_rust_socket_directory() {
        assert_eq!(
            socket_dir_for(Path::new("/tmp"), 501),
            Path::new("/tmp/prime-agent-rust-501")
        );
    }

    #[test]
    fn worker_socket_names_are_deterministic_and_supervisor_scoped() {
        let supervisor = Path::new("/fixture/first/daemon.sock");
        let other_supervisor = Path::new("/fixture/second/daemon.sock");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let b = worker_socket_path(supervisor, "fedcba9876543210");
        assert_ne!(a, b);
        assert_ne!(a, worker_socket_path(other_supervisor, "0123456789abcdef"));
        // Only the first 12 id characters key the name.
        assert_eq!(a, worker_socket_path(supervisor, "0123456789abffff"));
        #[cfg(unix)]
        assert!(a.starts_with(socket_dir()));
    }

    #[test]
    #[cfg(windows)]
    fn windows_default_endpoints_are_separate_from_typescript() {
        let ts_dir = std::env::temp_dir().join("prime-agent-user");
        let ts_daemon = Path::new(r"\\.\pipe\prime-agent-daemon");
        let key = hash_key(&ts_daemon.to_string_lossy(), 12);
        let daemon = default_daemon_socket_path();
        let worker = worker_socket_path(ts_daemon, "0123456789abcdef");

        assert_eq!(
            (socket_dir(), daemon.clone(), worker.clone()),
            (
                std::env::temp_dir().join("prime-agent-rust-user"),
                PathBuf::from(r"\\.\pipe\prime-agent-rust-daemon"),
                PathBuf::from(format!(
                    r"\\.\pipe\prime-agent-rust-worker-{key}-0123456789ab"
                ))
            )
        );
        assert_ne!(socket_dir(), ts_dir);
        assert_ne!(daemon, ts_daemon);
        assert_ne!(
            worker,
            PathBuf::from(format!(r"\\.\pipe\prime-agent-worker-{key}-0123456789ab"))
        );
    }
}
