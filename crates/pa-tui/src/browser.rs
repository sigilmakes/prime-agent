//! Browser launch for clicked hyperlinks: darwin `open`, Windows
//! `rundll32 url.dll,FileProtocolHandler`, otherwise `xdg-open` — the TUI opens them itself because
//! terminals gate native link handling while mouse reporting is active. The opener always resolves
//! to an absolute path: a bare name would search the inherited `PATH`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The fixed `xdg-open` tool slots — searched in order, the first that
/// exists wins.
#[cfg(all(unix, not(target_os = "macos")))]
const XDG_OPEN_SLOTS: [&str; 4] = [
    "/usr/bin/xdg-open",
    "/usr/local/bin/xdg-open",
    "/bin/xdg-open",
    "/run/current-system/sw/bin/xdg-open",
];

/// Resolve only trusted absolute slots. No PATH search or shell is involved.
#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_opener(url: &str, slots: &[PathBuf]) -> Option<(PathBuf, Vec<String>)> {
    let path = slots
        .iter()
        .find(|path| path.is_absolute() && path.is_file())?;
    Some((path.clone(), vec![url.to_string()]))
}

/// The absolute opener path and its argument list for one URL, or `None`
/// when the platform's tool is not installed.
fn opener(url: &str) -> Option<(PathBuf, Vec<String>)> {
    #[cfg(target_os = "macos")]
    {
        Some((PathBuf::from("/usr/bin/open"), vec![url.to_string()]))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        xdg_opener(url, &XDG_OPEN_SLOTS.map(PathBuf::from))
    }
    #[cfg(windows)]
    {
        // Absolute System32 path (the TS dialog resolves it from
        // `SystemRoot`, defaulting to `C:\Windows`).
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let rundll32 = std::path::Path::new(&system_root)
            .join("System32")
            .join("rundll32.exe");
        Some((
            rundll32,
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        ))
    }
}

/// Open `url` in the user's browser. Fire-and-forget: a failed launch never fails the click.
/// The child is reaped on a parked thread — an unwaited spawn would leak one zombie per click in
/// the long-running TUI.
pub(crate) fn open_in_browser(url: &str) {
    let Some((program, args)) = opener(url) else {
        return;
    };
    let Ok(mut child) = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_opener_resolves_an_absolute_path_and_targets_the_url() {
        let dir = tempfile::tempdir().expect("owned opener slots");
        let missing = dir.path().join("missing");
        let first = dir.path().join("first-opener");
        let second = dir.path().join("second-opener");
        std::fs::write(&first, "fixture, never executed").expect("first slot");
        std::fs::write(&second, "fixture, never executed").expect("second slot");
        // Shell metacharacters stay in a single URL argument, not executable syntax.
        let url = "https://example.com/docs?q=one two&next=$(never-execute)";
        let slots = [
            PathBuf::from("relative-opener"),
            missing.clone(),
            first.clone(),
            second.clone(),
        ];
        let resolved = xdg_opener(url, &slots).expect("the owned opener");
        assert!(
            resolved.0.is_absolute(),
            "the opener never resolves through PATH"
        );
        assert_eq!(resolved, (first, vec![url.to_string()]));
        assert_eq!(
            xdg_opener(url, &[missing, second.clone()]),
            Some((second, vec![url.to_string()]))
        );
        // Map the actual trusted slots into our fixture tree. Only the Nix slot
        // exists first, then an earlier conventional slot takes precedence.
        let trusted_slots =
            XDG_OPEN_SLOTS.map(|slot| dir.path().join(slot.trim_start_matches('/')));
        let nix_slot = dir.path().join("run/current-system/sw/bin/xdg-open");
        std::fs::create_dir_all(nix_slot.parent().expect("Nix slot parent"))
            .expect("Nix fixture dir");
        std::fs::write(&nix_slot, "never executed").expect("Nix fixture opener");
        assert_eq!(
            xdg_opener(url, &trusted_slots),
            Some((nix_slot, vec![url.to_string()]))
        );
        let conventional_slot = dir.path().join("usr/bin/xdg-open");
        std::fs::create_dir_all(conventional_slot.parent().expect("conventional parent"))
            .expect("conventional fixture dir");
        std::fs::write(&conventional_slot, "never executed").expect("conventional fixture opener");
        assert_eq!(
            xdg_opener(url, &trusted_slots),
            Some((conventional_slot, vec![url.to_string()]))
        );
        assert_eq!(xdg_opener(url, &[]), None);
        assert_eq!(xdg_opener(url, &[dir.path().to_path_buf()]), None);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn the_opener_resolves_an_absolute_path_and_targets_the_url() {
        let url = "https://example.com/docs";
        let (program, args) = opener(url).expect("the platform opener");
        assert!(program.is_absolute());
        #[cfg(target_os = "macos")]
        assert_eq!(args, vec![url.to_string()]);
        #[cfg(windows)]
        assert_eq!(
            args,
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()]
        );
    }
}
