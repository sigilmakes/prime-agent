//! Operational coexistence policy, not a new OAuth lease protocol.
//!
//! The legacy TypeScript empty-directory stale reaper has a stat/rmdir race;
//! normal live-lock tests do not prove exactly-once refresh after stale recovery.
//! Keep rotating-grant refresh owned by TypeScript during coexistence. Separate
//! roots or copied files do not make duplicate use of the same grant safe.
//! A shared auth document also requires one nonsymlink canonical path spelling:
//! legacy TS sync writers otherwise lock an alias while async writers lock its target.

pub(super) const DISABLE_OAUTH_REFRESH_ENV: &str = "PRIME_AGENT_DISABLE_OAUTH_REFRESH";

fn is_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

pub(super) fn disabled() -> bool {
    is_disabled(std::env::var(DISABLE_OAUTH_REFRESH_ENV).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::is_disabled;

    #[test]
    fn explicit_truthy_values_disable_refresh_only() {
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("no"), false),
            (Some("on"), false),
            (Some("1"), true),
            (Some("true"), true),
            (Some("TrUe"), true),
            (Some("yes"), true),
            (Some("YES"), true),
        ] {
            assert_eq!(is_disabled(value), expected);
        }
    }
}
