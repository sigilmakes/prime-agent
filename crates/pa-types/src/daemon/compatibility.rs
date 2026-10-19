//! Optional wire additions. Protocol 7 peers may omit these features.

/// Minimum schema revision and negotiated server capability for optional commands.
#[must_use]
pub fn command_compatibility(command: &str) -> Option<(u64, &'static str)> {
    match command {
        "refresh_models" | "apply_model_snapshots" | "refresh_all_models" => {
            Some((32, "model_refresh"))
        }
        "plugin_command" => Some((32, "plugin_commands")),
        _ => None,
    }
}

/// A command is usable only when both its revision and capability were negotiated.
#[must_use]
pub fn supports_optional_command(
    command: &str,
    schema_revision: Option<u64>,
    capabilities: &[String],
) -> bool {
    command_compatibility(command).is_some_and(|(revision, capability)| {
        schema_revision.is_some_and(|actual| actual >= revision)
            && capabilities.iter().any(|actual| actual == capability)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_clients_degrade_with_old_daemons_and_old_clients_keep_old_commands() {
        for command in [
            "refresh_models",
            "apply_model_snapshots",
            "refresh_all_models",
        ] {
            assert!(!supports_optional_command(command, Some(31), &[]));
            assert!(!supports_optional_command(command, Some(32), &[]));
            assert!(!supports_optional_command(
                command,
                None,
                &["model_refresh".into()]
            ));
            assert!(!supports_optional_command(
                command,
                Some(31),
                &["model_refresh".into()]
            ));
            assert!(supports_optional_command(
                command,
                Some(32),
                &["model_refresh".into()]
            ));
        }
        for (revision, capabilities, expected) in [
            (None, vec!["plugin_commands".into()], false),
            (Some(31), vec!["plugin_commands".into()], false),
            (Some(32), vec![], false),
            (Some(32), vec!["plugin_commands".into()], true),
        ] {
            assert_eq!(
                supports_optional_command("plugin_command", revision, &capabilities),
                expected
            );
        }
        super::super::rt::<super::super::DaemonCommand>(
            r#"{"type":"plugin_command","activeSessionId":"s","text":"/hello"}"#,
        );
        assert_eq!(command_compatibility("get_available_models"), None);
        super::super::rt::<super::super::DaemonCommand>(
            r#"{"type":"get_available_models","activeSessionId":"s"}"#,
        );
        super::super::rt::<super::super::DaemonCommand>(
            r#"{"type":"refresh_models","activeSessionId":"s","sources":["curated"]}"#,
        );
        super::super::rt::<super::super::DaemonCommand>(
            r#"{"type":"apply_model_snapshots","activeSessionId":"s","since":42}"#,
        );
        super::super::rt::<super::super::DaemonCommand>(r#"{"type":"refresh_all_models"}"#);
    }
}
