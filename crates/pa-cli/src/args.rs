//! CLI argument parsing, a faithful port of `cli/args.ts`. The parser is
//! intentionally hand-written: `--resume`/`--print` optionally consume the
//! next token and diagnostics carry their own formatting, none of which a
//! stock derive-based parser can express.

use pa_types::ai::ModelThinkingLevel;

/// Supported reasoning-effort levels.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn parse_thinking_level(value: &str) -> Option<ModelThinkingLevel> {
    match value {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Value flags whose free-form text may legitimately start with a dash.
const FREEFORM_VALUE_FLAGS: [&str; 1] = ["--goal"];
/// Prompt value flags whose text may look like a long option (YAML frontmatter).
const PROMPT_VALUE_FLAGS: [&str; 2] = ["--system-prompt", "--append-system-prompt"];

/// Marker argv token that re-enables internal runtime-only flags
/// (`--export`, `--list-models`) after the public command rewrite.
pub const INTERNAL_RUNTIME_COMMAND_MARKER: &str = "\0prime-agent-runtime-command";

/// Output mode selected by `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
    Acp,
    Daemon,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Mode> {
        match value {
            "text" => Some(Mode::Text),
            "json" => Some(Mode::Json),
            "rpc" => Some(Mode::Rpc),
            "acp" => Some(Mode::Acp),
            "daemon" => Some(Mode::Daemon),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Text => "text",
            Mode::Json => "json",
            Mode::Rpc => "rpc",
            Mode::Acp => "acp",
            Mode::Daemon => "daemon",
        }
    }
}

/// A parse-time diagnostic: warnings are printed, errors also exit 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub is_error: bool,
    pub message: String,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Self {
        Diagnostic {
            is_error: true,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // the TS `Args` surface mirrored verbatim
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub cwd: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ModelThinkingLevel>,
    pub continue_: bool,
    /// `true` when `--resume` was given without a selector.
    pub resume_bare: bool,
    pub resume: Option<String>,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub daemon_socket: Option<String>,
    pub no_session: bool,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Vec<String>,
    pub prompt_templates: Vec<String>,
    pub no_prompt_templates: bool,
    pub themes: Vec<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub goal: Option<String>,
    pub goal_token_budget: Option<u32>,
    /// The `--list-models` flag's tri-state (None = absent,
    /// Some(None) = the bare flag, Some(Some(x)) = with a value).
    #[allow(clippy::option_option)] // the CLI flag's tri-state is genuinely a double Option
    pub list_models: Option<Option<String>>,
    pub refresh: bool,
    pub offline: bool,
    pub verbose: bool,
    pub messages: Vec<String>,
    #[allow(clippy::struct_field_names)]
    // the trailing _args matches the TS `fileArgs` wire surface
    pub file_args: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Args {
    /// True when `--resume` appeared (with or without a selector).
    pub fn has_resume(&self) -> bool {
        self.resume_bare || self.resume.is_some()
    }
}

fn parse_positive_u32(value: &str, flag: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<u32> {
    // Number() accepts arbitrary precision, so parse as i128; a value past
    // u32::MAX is a user error, never a silent truncation.
    match value.trim().parse::<i128>() {
        Ok(parsed) if parsed > 0 && parsed <= i128::from(u32::MAX) => u32::try_from(parsed).ok(),
        _ => {
            diagnostics.push(Diagnostic::error(format!(
                "{flag} must be a positive integer between 1 and u32::MAX (4294967295)"
            )));
            None
        }
    }
}

pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();
    let internal_runtime_command =
        args.first().map(String::as_str) == Some(INTERNAL_RUNTIME_COMMAND_MARKER);
    let first_arg_index = usize::from(internal_runtime_command);

    let mut end_of_options = false;
    let mut i = first_arg_index;
    while i < args.len() {
        let arg = args[i].as_str();

        if end_of_options {
            result.messages.push(arg.to_string());
            i += 1;
            continue;
        }
        if arg == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

        macro_rules! require_value {
            ($flag:expr) => {
                match take_required_value(args, i, $flag, &mut result.diagnostics) {
                    Some(value) => {
                        i += 1;
                        value
                    }
                    None => {
                        i += 1;
                        continue;
                    }
                }
            };
        }

        match arg {
            "--help" | "-h" => result.help = true,
            "--version" | "-v" => result.version = true,
            "--mode" => {
                let mode = require_value!(arg);
                if let Some(mode) = Mode::parse(&mode) {
                    result.mode = Some(mode);
                } else {
                    result.diagnostics.push(Diagnostic::error(format!(
                        "Invalid --mode \"{mode}\". Valid values: text, json, rpc, acp, daemon"
                    )));
                }
            }
            "--daemon-socket" => {
                result.daemon_socket = Some(require_value!(arg));
            }
            "--continue" | "-c" => result.continue_ = true,
            "--resume" | "-r" => match args.get(i + 1) {
                Some(next)
                    if !next.starts_with('-') && !next.starts_with('@') && !next.is_empty() =>
                {
                    result.resume = Some(next.clone());
                    i += 1;
                }
                Some(next) if next.is_empty() => {
                    result.resume_bare = true;
                    i += 1;
                }
                _ => result.resume_bare = true,
            },
            "--provider" => result.provider = Some(require_value!(arg)),
            "--model" => result.model = Some(require_value!(arg)),
            "--api-key" => result.api_key = Some(require_value!(arg)),
            "--cwd" => result.cwd = Some(require_value!(arg)),
            "--system-prompt" => result.system_prompt = Some(require_value!(arg)),
            "--append-system-prompt" => {
                let value = require_value!(arg);
                result.append_system_prompt.push(value);
            }
            "--no-session" => result.no_session = true,
            "--fork" => result.fork = Some(require_value!(arg)),
            "--session-dir" => result.session_dir = Some(require_value!(arg)),
            "--models" => {
                let value = require_value!(arg);
                result.models = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .map(str::to_string)
                        .collect(),
                );
            }
            "--thinking" => {
                let level = require_value!(arg);
                if let Some(level) = parse_thinking_level(&level) {
                    result.thinking = Some(level);
                } else {
                    result.diagnostics.push(Diagnostic::error(format!(
                        "Invalid thinking level \"{level}\". Valid values: {}",
                        THINKING_LEVELS.join(", ")
                    )));
                }
            }
            "--print" | "-p" => {
                result.print = true;
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---"))
                    {
                        result.messages.push(next.clone());
                        i += 1;
                    }
                }
            }
            "--export" => {
                if !internal_runtime_command {
                    result.diagnostics.push(Diagnostic::error(
                        "--export was removed. Use \"wv session export <file> [output]\".",
                    ));
                    if let Some(next) = args.get(i + 1) {
                        if !next.starts_with('-') {
                            i += 1;
                        }
                    }
                } else if let Some(next) = args.get(i + 1) {
                    result.export = Some(next.clone());
                    i += 1;
                } else {
                    result
                        .diagnostics
                        .push(Diagnostic::error("--export requires a value"));
                }
            }
            "--skill" => {
                let value = require_value!(arg);
                result.skills.push(value);
            }
            "--prompt-template" => {
                let value = require_value!(arg);
                result.prompt_templates.push(value);
            }
            "--theme" => {
                let value = require_value!(arg);
                result.themes.push(value);
            }
            "--no-skills" | "-ns" => result.no_skills = true,
            "--no-prompt-templates" | "-np" => result.no_prompt_templates = true,
            "--no-themes" => result.no_themes = true,
            "--no-context-files" | "-nc" => result.no_context_files = true,
            "--goal" => {
                let value = require_value!(arg);
                if value.trim().is_empty() {
                    result
                        .diagnostics
                        .push(Diagnostic::error("--goal requires a non-empty objective"));
                } else {
                    result.goal = Some(value);
                }
            }
            "--goal-token-budget" => {
                let value = require_value!(arg);
                result.goal_token_budget = parse_positive_u32(&value, arg, &mut result.diagnostics);
            }
            "--list-models" => {
                let has_search = args
                    .get(i + 1)
                    .is_some_and(|next| !next.starts_with('-') && !next.starts_with('@'));
                if !internal_runtime_command {
                    result.diagnostics.push(Diagnostic::error(
                        "--list-models was removed. Use \"wv model list [search]\".",
                    ));
                    if has_search {
                        i += 1;
                    }
                } else if has_search {
                    result.list_models = Some(Some(args[i + 1].clone()));
                    i += 1;
                } else {
                    result.list_models = Some(None);
                }
            }
            "--verbose" => result.verbose = true,
            "--offline" => result.offline = true,
            "--refresh" => result.refresh = true,
            _ if arg.starts_with("--resume=") => {
                let value = &arg["--resume=".len()..];
                if value.is_empty() {
                    result.resume_bare = true;
                } else {
                    result.resume = Some(value.to_string());
                }
            }
            _ if arg.starts_with("--export=") => {
                result.diagnostics.push(Diagnostic::error(
                    "--export was removed. Use \"wv session export <file> [output]\".",
                ));
            }
            _ if arg.starts_with("--list-models=") => {
                result.diagnostics.push(Diagnostic::error(
                    "--list-models was removed. Use \"wv model list [search]\".",
                ));
            }
            _ if arg.starts_with('@') => {
                result.file_args.push(arg[1..].to_string());
            }
            _ if arg.starts_with('-') => {
                result
                    .diagnostics
                    .push(Diagnostic::error(format!("Unknown option: {arg}")));
            }
            _ => result.messages.push(arg.to_string()),
        }
        i += 1;
    }

    if result.goal_token_budget.is_some() && result.goal.is_none() {
        result
            .diagnostics
            .push(Diagnostic::error("--goal-token-budget requires --goal"));
    }

    result
}

/// The value flag either consumes the next token or records a
/// "requires a value" error.
fn take_required_value(
    args: &[String],
    index: usize,
    flag: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    let Some(next) = args.get(index + 1) else {
        diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
        return None;
    };
    let value_may_start_with_dash = FREEFORM_VALUE_FLAGS.contains(&flag);
    let value_is_arbitrary_prompt_text = PROMPT_VALUE_FLAGS.contains(&flag);
    if next == "--"
        || (!value_is_arbitrary_prompt_text
            && next.starts_with(if value_may_start_with_dash { "--" } else { "-" }))
    {
        diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
        return None;
    }
    Some(next.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        let args: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
        parse_args(&args)
    }

    fn last_error(args: &Args) -> &str {
        args.diagnostics
            .iter()
            .rev()
            .find(|d| d.is_error)
            .map(|d| d.message.as_str())
            .unwrap_or_default()
    }

    #[test]
    fn unknown_short_option_is_an_error() {
        let parsed = parse(&["-x"]);
        assert_eq!(last_error(&parsed), "Unknown option: -x");
    }

    #[test]
    fn unknown_long_flag_is_an_error() {
        let parsed = parse(&["--bogus-flag"]);
        assert_eq!(last_error(&parsed), "Unknown option: --bogus-flag");
        let parsed = parse(&["--bogus-flag", "value"]);
        assert_eq!(last_error(&parsed), "Unknown option: --bogus-flag");
    }

    #[test]
    fn print_consumes_next_message() {
        let parsed = parse(&["-p", "hello", "world"]);
        assert!(parsed.print);
        assert_eq!(parsed.messages, vec!["hello", "world"]);
    }

    #[test]
    fn resume_accepts_selector_bare_and_equals() {
        assert_eq!(parse(&["-r", "abc"]).resume.as_deref(), Some("abc"));
        assert!(parse(&["--resume", "--verbose"]).resume_bare);
        assert_eq!(parse(&["--resume=abc"]).resume.as_deref(), Some("abc"));
        assert!(parse(&["--resume="]).resume_bare);
        // A selector starting with - or @ stays bare.
        assert!(parse(&["-r", "-x"]).resume_bare);
    }

    #[test]
    fn end_of_options_marker() {
        let parsed = parse(&["--", "-x", "--mode"]);
        assert_eq!(parsed.messages, vec!["-x", "--mode"]);
    }

    #[test]
    fn export_removed_without_internal_marker() {
        let parsed = parse(&["--export", "foo"]);
        assert_eq!(
            last_error(&parsed),
            "--export was removed. Use \"wv session export <file> [output]\"."
        );
        let marker: Vec<String> = vec![
            INTERNAL_RUNTIME_COMMAND_MARKER.into(),
            "--export".into(),
            "foo".into(),
        ];
        let parsed = parse_args(&marker);
        assert_eq!(parsed.export.as_deref(), Some("foo"));
    }

    #[test]
    fn goal_budget_requires_goal() {
        let parsed = parse(&["--goal-token-budget", "5"]);
        assert_eq!(last_error(&parsed), "--goal-token-budget requires --goal");
    }

    #[test]
    fn positive_int_rejects_zero_and_text() {
        let parsed = parse(&["--goal", "x", "--goal-token-budget", "0"]);
        assert_eq!(
            last_error(&parsed),
            "--goal-token-budget must be a positive integer between 1 and u32::MAX (4294967295)"
        );
        let parsed = parse(&["--goal", "x", "--goal-token-budget", "abc"]);
        assert_eq!(
            last_error(&parsed),
            "--goal-token-budget must be a positive integer between 1 and u32::MAX (4294967295)"
        );
    }

    #[test]
    fn positive_int_rejects_past_the_field_range() {
        // 2**32: the u32 flag rejects it out loud instead of silently
        // truncating into the flag's u32 field.
        let parsed = parse(&["--goal", "x", "--goal-token-budget", "4294967296"]);
        assert_eq!(
            last_error(&parsed),
            "--goal-token-budget must be a positive integer between 1 and u32::MAX (4294967295)"
        );
    }
}
