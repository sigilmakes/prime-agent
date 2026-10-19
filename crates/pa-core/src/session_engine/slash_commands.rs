//! Session-side slash-command handling: which commands execute in the
//! session engine and how their arguments parse; the command table itself
//! is shared vocabulary in `pa_types::slash_commands` (one table serves
//! every surface).

pub use pa_types::slash_commands::{
    find_slash_command_suggestion, is_session_slash_command_name, parse_slash_command,
    BuiltinSlashCommand, ResolvedSlashCommand, SlashCommandExecution, SlashCommandRegistry,
    SESSION_SLASH_COMMAND_NAMES,
};

/// A parsed session slash command (compact/goal/autonomous).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSlashCommand {
    pub name: String,
    /// Plugin commands retain the full generation-bound target from admission.
    pub plugin: Option<crate::plugins::Contribution>,
    pub args: String,
    pub text: String,
}

/// Parse a session command from input; None for non-session commands.
#[must_use]
pub fn parse_session_command(
    registry: &SlashCommandRegistry,
    text: &str,
) -> Option<SessionSlashCommand> {
    let resolved = registry.parse(text)?;
    if !is_session_slash_command_name(resolved.name) && resolved.name != "reload" {
        return None;
    }
    Some(SessionSlashCommand {
        name: resolved.name.to_string(),
        plugin: None,
        args: resolved.args,
        text: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_command_extraction() {
        let registry = SlashCommandRegistry::builtin();
        let command = parse_session_command(&registry, "/compact focus on tests").unwrap();
        assert_eq!(command.name, "compact");
        assert_eq!(command.args, "focus on tests");
        assert!(parse_session_command(&registry, "/model").is_none());
        assert!(parse_session_command(&registry, "/unknown x").is_none());
    }
}
