//! The `/export` and `/share` client commands: the `/export` path-argument
//! parsing, the GitHub CLI gating and gist spawn for `/share`, the gist
//! URL parse, and the share viewer URL. The exported file itself is the
//! daemon-side exporter; this module owns only the client-side pieces.

use std::path::Path;

/// Parse the path argument of `/export`: `None` for no argument; a
/// quoted argument runs to its closing quote; an unquoted one ends at
/// the first whitespace.
pub fn path_command_argument(text: &str, command: &str) -> Option<String> {
    if text == command {
        return None;
    }
    let rest = text.strip_prefix(&format!("{command} "))?;
    let rest = rest.trim_start();
    let first = rest.chars().next()?;
    if first == '"' || first == '\'' {
        let quoted = rest.chars().skip(1).collect::<String>();
        let close = quoted.find(first)?;
        return Some(quoted[..close].to_string());
    }
    match rest.find(char::is_whitespace) {
        Some(index) => Some(rest[..index].to_string()),
        None => Some(rest.to_string()),
    }
}

/// The share viewer URL for a gist id: the `PI_SHARE_VIEWER_URL`
/// override, else the product default, with the id as the fragment. The
/// default URL and the env var are the TS product's wire identifiers;
/// they stay byte-identical until the product renames them.
#[must_use]
pub fn share_viewer_url(gist_id: &str) -> String {
    let base = std::env::var("PI_SHARE_VIEWER_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_VIEWER_URL.to_string());
    format!("{base}#{gist_id}")
}

const DEFAULT_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";

/// The gist id from the URL `gh gist create` prints (the last path
/// segment): an empty segment (a trailing slash) is no gist id.
#[must_use]
pub fn gist_id_from_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let segment = url.rsplit('/').next()?;
    (!segment.is_empty()).then_some(segment)
}

/// The GitHub CLI availability (TS `gh auth status` probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhAuthStatus {
    /// `gh` exists and reports a logged-in account.
    Ok,
    /// `gh` exists but no account is logged in.
    NotLoggedIn,
    /// `gh` is not installed.
    NotInstalled,
}

/// Probe the GitHub CLI with `gh auth status`: a non-zero exit means
/// not logged in, a spawn failure means not installed. The probe never
/// opens a window (hidden spawn).
#[must_use]
pub fn probe_gh_auth() -> GhAuthStatus {
    // No inherited fds: a probe must never hold the terminal the TUI owns
    // (the fd-set audit's rule — no child holds /dev/tty).
    let mut command = gh_probe_command();
    command.stdin(std::process::Stdio::null());
    let Ok(output) = command.args(["auth", "status"]).output() else {
        return GhAuthStatus::NotInstalled;
    };
    if output.status.success() {
        GhAuthStatus::Ok
    } else {
        GhAuthStatus::NotLoggedIn
    }
}

/// The result of one `gh gist create` run.
#[derive(Debug, Clone, PartialEq)]
pub struct GistOutcome {
    /// The gist URL `gh` printed on success.
    pub gist_url: String,
    /// The share viewer URL derived from the gist id.
    pub preview_url: String,
}

/// Wait for a spawned `gh gist create` and turn its output into the
/// share result (stdout is the gist URL; stderr is the failure message).
/// Both pipes drain concurrently (`wait_with_output`), so a chatty `gh`
/// cannot deadlock the wait.
///
/// # Errors
///
/// Returns `Err` with the wait failure, the trimmed `gh` stderr on a
/// non-zero exit, or a parse failure.
pub async fn gist_outcome(child: tokio::process::Child) -> Result<GistOutcome, String> {
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message.trim();
        return Err(if message.is_empty() {
            "Unknown error".to_string()
        } else {
            message.to_string()
        });
    }
    let gist_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let gist_id = gist_id_from_url(&gist_url)
        .map(str::to_string)
        .ok_or_else(|| "Failed to parse gist ID from gh output".to_string())?;
    Ok(GistOutcome {
        gist_url,
        preview_url: share_viewer_url(&gist_id),
    })
}

/// Spawn `gh gist create --public=false <file>`: output is piped, no
/// terminal window on Windows. The child is killed when dropped
/// mid-wait, so aborting the upload task terminates `gh`.
///
/// # Errors
///
/// Returns `Err` when the OS cannot spawn the `gh` process.
pub fn spawn_gist_create(file: &Path) -> std::io::Result<tokio::process::Child> {
    gh_command()
        .args(["gist", "create", "--public=false"])
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// The async `gh` command with Windows hidden-window creation flags
/// applied (a no-op on Unix).
#[cfg(windows)]
fn gh_command() -> tokio::process::Command {
    // CREATE_NO_WINDOW: the loader surfaces the wait, not a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = tokio::process::Command::new("gh");
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(unix)]
fn gh_command() -> tokio::process::Command {
    tokio::process::Command::new("gh")
}

/// The blocking `gh` probe command, hidden on Windows the same way.
#[cfg(windows)]
fn gh_probe_command() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new("gh");
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(unix)]
fn gh_probe_command() -> std::process::Command {
    std::process::Command::new("gh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_arguments_parse() {
        assert_eq!(path_command_argument("/export", "/export"), None);
        assert_eq!(
            path_command_argument("/export out.html", "/export"),
            Some("out.html".to_string())
        );
        assert_eq!(
            path_command_argument("/export   out.html  ", "/export"),
            Some("out.html".to_string())
        );
        assert_eq!(
            path_command_argument("/export \"my file.html\"", "/export"),
            Some("my file.html".to_string())
        );
        assert_eq!(
            path_command_argument("/export 'my file.jsonl' extra", "/export"),
            Some("my file.jsonl".to_string())
        );
        assert_eq!(
            path_command_argument("/export a b c", "/export"),
            Some("a".to_string())
        );
        assert_eq!(path_command_argument("/export \"unclosed", "/export"), None);
    }

    #[test]
    fn gist_ids_parse() {
        assert_eq!(
            gist_id_from_url("https://gist.github.com/user/abc123"),
            Some("abc123")
        );
        assert_eq!(gist_id_from_url("abc123"), Some("abc123"));
        assert_eq!(
            gist_id_from_url("https://gist.github.com/user/abc123/"),
            None
        );
    }

    #[test]
    fn share_viewer_urls() {
        // SAFETY: single-threaded test setup for a std::env var.
        std::env::remove_var("PI_SHARE_VIEWER_URL");
        assert_eq!(share_viewer_url("abc123"), "https://pi.dev/session/#abc123");
        std::env::set_var("PI_SHARE_VIEWER_URL", "https://example.test/view/");
        assert_eq!(
            share_viewer_url("abc123"),
            "https://example.test/view/#abc123"
        );
        std::env::set_var("PI_SHARE_VIEWER_URL", "");
        assert_eq!(share_viewer_url("abc123"), "https://pi.dev/session/#abc123");
        std::env::remove_var("PI_SHARE_VIEWER_URL");
    }

    /// Exercise the real probe and upload against a local executable, never an installed gh.
    /// The exact-test child owns its entire environment, so parallel tests cannot alter PATH
    /// or the viewer URL during a probe or upload.
    #[cfg(unix)]
    #[tokio::test]
    async fn gist_spawn_against_stub_gh() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "PA_TUI_GH_FIXTURE";
        const MARKER: &str = "PA_TUI_GH_FIXTURE_OK";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = std::path::PathBuf::from(dir);
            assert_eq!(probe_gh_auth(), GhAuthStatus::Ok);
            let child = spawn_gist_create(&dir.join("session.html")).expect("spawn stub gh");
            assert_eq!(
                gist_outcome(child).await.expect("gist outcome"),
                GistOutcome {
                    gist_url: "https://gist.github.com/testuser/abc123".to_string(),
                    preview_url: "https://pi.dev/session/#abc123".to_string(),
                }
            );
            let child = spawn_gist_create(&dir.join("nope.html")).expect("spawn stub gh");
            assert_eq!(
                gist_outcome(child)
                    .await
                    .expect_err("missing upload must fail"),
                "gist: upload file missing"
            );
            println!("{MARKER}");
            return;
        }
        let dir = tempfile::TempDir::new().expect("temp dir");
        // Nix supplies CONFIG_SHELL as an absolute store path. Else resolve sh once
        // from the test toolchain; the child's PATH will contain only our mock.
        let shell = std::env::var_os("CONFIG_SHELL")
            .map(std::path::PathBuf::from)
            .into_iter()
            .chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .map(|path| path.join("sh")),
            )
            .find(|path| path.is_absolute() && path.is_file())
            .expect("an absolute test shell")
            .canonicalize()
            .expect("resolve pinned test shell");
        let stub = format!(
            r#"#!{}
case "$1" in
  auth) exit 0 ;;
  gist)
    if [ $# -ne 4 ] || [ "$2" != create ] || [ "$3" != --public=false ] || [ ! -f "$4" ]; then
      echo "gist: upload file missing" >&2
      exit 1
    fi
    echo "https://gist.github.com/testuser/abc123"
    ;;
  *) echo "unsupported: $1" >&2; exit 1 ;;
esac
"#,
            shell.display()
        );
        let executable = dir.path().join("gh");
        std::fs::write(&executable, stub).expect("write stub");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        std::fs::write(dir.path().join("session.html"), "<html></html>").expect("write upload");
        let mut command =
            tokio::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .env_clear()
            .args([
                "--exact",
                "export_share::tests::gist_spawn_against_stub_gh",
                "--nocapture",
            ])
            .env(CHILD, dir.path())
            .env("PATH", dir.path())
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_RUNTIME_DIR", dir.path())
            .env(
                "LD_LIBRARY_PATH",
                std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
            )
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
            .await
            .expect("bounded fixture child")
            .expect("run fixture child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}{stderr}");
        assert!(
            stdout.lines().any(|line| line == MARKER),
            "fixture did not execute: {stdout}{stderr}"
        );
    }
}
