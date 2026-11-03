//! Session slash-command argument parsing and status formatting for `/goal`;
//! the strings are user-facing and byte-identical to TS.

use crate::goals::{validate_goal_objective, MAX_THREAD_GOAL_OBJECTIVE_CHARS};

/// A parsed `/goal` argument string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCommand {
    Status,
    Clear,
    Pause,
    Resume,
    Start {
        objective: String,
        token_budget: Option<u64>,
    },
}

/// Parse `/goal [status|clear|stop|pause|resume|[--budget <tokens>] <objective>]`.
///
/// # Errors
///
/// Returns a usage-error string when the budget or objective is missing or invalid.
///
/// # Panics
///
/// The `expect` on the `=` separator cannot fire: the branch is guarded by the flag-prefix checks.
pub fn parse_goal_command(args: &str) -> Result<GoalCommand, String> {
    let rest = args.trim();
    let normalized = rest.to_lowercase();
    if rest.is_empty() || normalized == "status" {
        return Ok(GoalCommand::Status);
    }
    if normalized == "clear" || normalized == "stop" {
        return Ok(GoalCommand::Clear);
    }
    if normalized == "pause" {
        return Ok(GoalCommand::Pause);
    }
    if normalized == "resume" {
        return Ok(GoalCommand::Resume);
    }
    let first_token = rest.split(char::is_whitespace).next().unwrap_or_default();
    let mut token_budget = None;
    let mut objective = rest.to_string();
    if first_token == "--budget"
        || first_token == "--token-budget"
        || first_token.starts_with("--budget=")
        || first_token.starts_with("--token-budget=")
    {
        let (value_text, objective_rest) =
            if first_token.starts_with("--budget=") || first_token.starts_with("--token-budget=") {
                let separator = first_token.find('=').expect("checked prefix contains =");
                let value_text = &first_token[separator + 1..];
                (
                    value_text.to_string(),
                    rest[first_token.len()..].trim().to_string(),
                )
            } else {
                let without_flag = rest[first_token.len()..].trim_start();
                match without_flag.find(char::is_whitespace) {
                    None => {
                        return Err("Usage: /goal [--budget <tokens>] <objective>".to_string());
                    }
                    Some(space) => (
                        without_flag[..space].to_string(),
                        without_flag[space + 1..].trim().to_string(),
                    ),
                }
            };
        token_budget = Some(parse_goal_budget_value(&value_text)?);
        objective = objective_rest;
    }
    let objective = validate_goal_objective(&objective).map_err(|error| format!("{error}"))?;
    let _ = MAX_THREAD_GOAL_OBJECTIVE_CHARS;
    Ok(GoalCommand::Start {
        objective,
        token_budget,
    })
}

/// `parseGoalBudgetValue`: a positive integer.
fn parse_goal_budget_value(value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) || value.starts_with('0') {
        return Err("Goal token budget must be a positive integer.".to_string());
    }
    match value.parse::<u64>() {
        Ok(budget) if budget > 0 => Ok(budget),
        _ => Err("Goal token budget must be a positive integer.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_command_kinds() {
        assert_eq!(parse_goal_command("").unwrap(), GoalCommand::Status);
        assert_eq!(parse_goal_command("status").unwrap(), GoalCommand::Status);
        assert_eq!(parse_goal_command("stop").unwrap(), GoalCommand::Clear);
        assert_eq!(parse_goal_command("PAUSE").unwrap(), GoalCommand::Pause);
        assert_eq!(parse_goal_command("resume").unwrap(), GoalCommand::Resume);
        assert_eq!(
            parse_goal_command("ship the feature").unwrap(),
            GoalCommand::Start {
                objective: "ship the feature".to_string(),
                token_budget: None,
            }
        );
        assert_eq!(
            parse_goal_command("--budget 5000 keep going").unwrap(),
            GoalCommand::Start {
                objective: "keep going".to_string(),
                token_budget: Some(5000),
            }
        );
        assert_eq!(
            parse_goal_command("--token-budget=300 fix it").unwrap(),
            GoalCommand::Start {
                objective: "fix it".to_string(),
                token_budget: Some(300),
            }
        );
        assert_eq!(
            parse_goal_command("--budget").unwrap_err(),
            "Usage: /goal [--budget <tokens>] <objective>"
        );
        assert_eq!(
            parse_goal_command("--budget 0x10 nope").unwrap_err(),
            "Goal token budget must be a positive integer."
        );
        assert_eq!(parse_goal_command("").unwrap(), GoalCommand::Status,);
    }
}
