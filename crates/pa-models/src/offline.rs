//! Offline mode (`PI_OFFLINE`): no network at all, serve cache/bundled/compiled.

/// Whether the catalog subsystem must avoid every network request.
#[must_use]
pub fn is_catalog_offline() -> bool {
    std::env::var("PI_OFFLINE").is_ok_and(|value| is_offline_value(&value))
}

fn is_offline_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offline_values_without_changing_the_process_environment() {
        // Test the production predicate, not process-global set_var/remove_var:
        // parallel catalog tests may rely on the caller's offline policy.
        let before = std::env::var_os("PI_OFFLINE");
        for value in ["1", "true", "YES", " Yes "] {
            assert!(is_offline_value(value), "{value}");
        }
        for value in ["0", "no", "maybe", "", "  "] {
            assert!(!is_offline_value(value), "{value}");
        }
        assert_eq!(std::env::var_os("PI_OFFLINE"), before);
    }
}
