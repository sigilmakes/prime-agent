//! Package-manager ownership: self-update must never bypass a Nix source pin.

use std::path::Path;

const NIX_INSTRUCTION: &str = "Prime Agent is managed by Nix. Run `cd /config && just update-prime`, then review and deploy the source pin through /config. Self-update and rollback are disabled.";

/// Explain why mutation through the upstream installer is forbidden.
/// Both client surfaces and installer boundaries consult this before any I/O.
#[must_use]
pub fn self_update_refusal() -> Option<&'static str> {
    refusal(
        std::env::var("PRIME_AGENT_INSTALL_MANAGED_BY")
            .ok()
            .as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

fn refusal(owner: Option<&str>, executable: Option<&Path>) -> Option<&'static str> {
    (owner == Some("nix") || executable.is_some_and(|path| path.starts_with("/nix/store")))
        .then_some(NIX_INSTRUCTION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix_owner_and_store_payloads_refuse_self_update_without_prefix_collisions() {
        assert_eq!(
            refusal(Some("nix"), Some(Path::new("/opt/prime-agent"))),
            Some(NIX_INSTRUCTION)
        );
        assert_eq!(
            refusal(
                None,
                Some(Path::new("/nix/store/hash-prime/bin/prime-agent"))
            ),
            Some(NIX_INSTRUCTION)
        );
        assert_eq!(
            refusal(None, Some(Path::new("/nix/storefront/prime-agent"))),
            None
        );
        assert_eq!(refusal(None, Some(Path::new("/opt/prime-agent"))), None);
        assert_eq!(refusal(None, None), None);
    }
}
