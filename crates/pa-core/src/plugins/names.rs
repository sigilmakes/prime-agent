//! Declaration names shared by staging and registration. Python-visible names
//! must fit the kwargs-only plugin namespace; commands/hooks remain native names.
use super::{ContributionKind, PluginError};

fn validate_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Err(PluginError::Invalid(format!("invalid name {name:?}")));
    }
    Ok(())
}

fn validate_python_name(name: &str) -> Result<(), PluginError> {
    // Python keyword.iskeyword(), excluding uppercase True/False/None which the
    // lowercase native grammar already rejects. Soft keywords remain valid.
    if [
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield",
    ]
    .contains(&name)
    {
        return Err(PluginError::Invalid(format!(
            "Python keyword is not an export name: {name}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_plugin_name(name: &str) -> Result<(), PluginError> {
    validate_name(name)?;
    validate_python_name(name)?;
    if [
        "core",
        "host",
        "plugin",
        "rlm",
        "mcp",
        "python",
        "refresh",
        "inventory",
        "session",
        "revision",
    ]
    .contains(&name)
    {
        return Err(PluginError::Invalid(format!("reserved plugin {name}")));
    }
    Ok(())
}

pub(super) fn validate_contribution_name(
    kind: ContributionKind,
    name: &str,
) -> Result<(), PluginError> {
    match kind {
        ContributionKind::Provider => validate_name(&name.replace('-', "_"))?,
        ContributionKind::Export
        | ContributionKind::Command
        | ContributionKind::Hook
        | ContributionKind::ContextStrategy
        | ContributionKind::Compactor => validate_name(name)?,
    }
    if kind == ContributionKind::Export {
        validate_python_name(name)?;
    }
    Ok(())
}
