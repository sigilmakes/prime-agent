//! Raw-source Mermaid theme hint extraction.

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeHintInfo {
    theme: Option<String>,
    theme_only: bool,
}

/// Extract the first Mermaid theme hint from raw source.
pub(crate) fn extract_theme_hint(input: &str) -> Option<String> {
    let mut search_input = input;

    if let Some((frontmatter, rest)) = split_frontmatter(input) {
        let info = scan_frontmatter(frontmatter);
        if info.theme.is_some() {
            return info.theme;
        }
        search_input = rest;
    }

    for line in search_input.lines() {
        if let Some(info) = scan_init_directive(line) {
            return info.theme;
        }
    }

    None
}

/// Remove compatibility syntax that exists only to carry Mermaid `theme` hints.
pub(crate) fn strip_theme_only_compat_syntax(input: &str) -> Option<String> {
    let mut changed = false;
    let mut remaining = input;

    if let Some((frontmatter, rest)) = split_frontmatter(input)
        && scan_frontmatter(frontmatter).theme_only
    {
        remaining = rest;
        changed = true;
    }

    let mut stripped = String::with_capacity(remaining.len());
    for segment in remaining.split_inclusive('\n') {
        if scan_init_directive(segment.trim()).is_some_and(|info| info.theme_only) {
            changed = true;
            continue;
        }
        stripped.push_str(segment);
    }

    changed.then_some(stripped)
}

fn split_frontmatter(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    let after_open = &trimmed[3..];
    let after_first_newline = after_open
        .find('\n')
        .map(|position| &after_open[position + 1..])?;

    for (index, line) in after_first_newline.lines().enumerate() {
        if line.trim() == "---" {
            let block_len: usize = after_first_newline
                .lines()
                .take(index)
                .map(|content| content.len() + 1)
                .sum();
            let consumed_len: usize = after_first_newline
                .lines()
                .take(index + 1)
                .map(|content| content.len() + 1)
                .sum();
            let block_len = block_len.min(after_first_newline.len());
            let consumed_len = consumed_len.min(after_first_newline.len());
            return Some((
                &after_first_newline[..block_len],
                &after_first_newline[consumed_len..],
            ));
        }
    }

    None
}

fn scan_frontmatter(block: &str) -> ThemeHintInfo {
    let mut theme = None;
    let mut saw_config = false;
    let mut saw_other = false;
    let mut config_indent = 0usize;

    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let indent = line
            .chars()
            .take_while(|character| character.is_whitespace())
            .count();

        if !saw_config {
            if trimmed == "config:" {
                saw_config = true;
                config_indent = indent;
            } else {
                saw_other = true;
            }
            continue;
        }

        if indent <= config_indent {
            saw_other = true;
            continue;
        }

        let Some((key, value)) = split_key_value(trimmed) else {
            saw_other = true;
            continue;
        };

        if key.eq_ignore_ascii_case("theme") {
            theme = parse_scalar(value);
        } else {
            saw_other = true;
        }
    }

    ThemeHintInfo {
        theme: theme.clone(),
        theme_only: theme.is_some() && saw_config && !saw_other,
    }
}

fn scan_init_directive(line: &str) -> Option<ThemeHintInfo> {
    let body = extract_init_body(line)?;
    let members = split_top_level_members(body)?;
    let mut theme = None;
    let mut saw_other = false;

    for member in members {
        let Some((key, value)) = split_top_level_pair(member) else {
            saw_other = true;
            continue;
        };

        if normalize_key(key).eq_ignore_ascii_case("theme") {
            theme = parse_scalar(value);
        } else {
            saw_other = true;
        }
    }

    theme.as_ref()?;

    Some(ThemeHintInfo {
        theme: theme.clone(),
        theme_only: !saw_other,
    })
}

fn extract_init_body(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !(trimmed.starts_with("%%{") && trimmed.ends_with("}%%")) {
        return None;
    }

    let inner = trimmed.strip_prefix("%%{")?.strip_suffix("}%%")?.trim();

    let init_prefix = "init:";
    if inner.len() < init_prefix.len()
        || !inner[..init_prefix.len()].eq_ignore_ascii_case(init_prefix)
    {
        return None;
    }

    Some(inner[init_prefix.len()..].trim())
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let position = line.find(':')?;
    Some((line[..position].trim(), line[position + 1..].trim()))
}

fn split_top_level_members(body: &str) -> Option<Vec<&str>> {
    let trimmed = body.trim();
    let inner = trimmed.strip_prefix('{')?.strip_suffix('}')?;
    let mut members = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, character) in inner.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }

            if character == '\\' {
                escaped = true;
                continue;
            }

            if character == active_quote {
                quote = None;
            }
            continue;
        }

        match character {
            '"' | '\'' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let member = inner[start..index].trim();
                if !member.is_empty() {
                    members.push(member);
                }
                start = index + 1;
            }
            _ => {}
        }
    }

    let tail = inner[start..].trim();
    if !tail.is_empty() {
        members.push(tail);
    }

    Some(members)
}

fn split_top_level_pair(member: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, character) in member.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }

            if character == '\\' {
                escaped = true;
                continue;
            }

            if character == active_quote {
                quote = None;
            }
            continue;
        }

        match character {
            '"' | '\'' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => {
                return Some((member[..index].trim(), member[index + 1..].trim()));
            }
            _ => {}
        }
    }

    None
}

fn normalize_key(key: &str) -> &str {
    let trimmed = key.trim();
    match trimmed.chars().next() {
        Some('"') if trimmed.ends_with('"') && trimmed.len() >= 2 => &trimmed[1..trimmed.len() - 1],
        Some('\'') if trimmed.ends_with('\'') && trimmed.len() >= 2 => {
            &trimmed[1..trimmed.len() - 1]
        }
        _ => trimmed,
    }
}

fn parse_scalar(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    match trimmed.chars().next() {
        Some('"') if trimmed.ends_with('"') && trimmed.len() >= 2 => {
            Some(trimmed[1..trimmed.len() - 1].to_string())
        }
        Some('\'') if trimmed.ends_with('\'') && trimmed.len() >= 2 => {
            Some(trimmed[1..trimmed.len() - 1].to_string())
        }
        _ => Some(trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_theme_hint, strip_theme_only_compat_syntax};

    #[test]
    fn extracts_theme_from_frontmatter_config() {
        let input = "---\nconfig:\n  theme: dark\n---\ngraph TD\nA-->B\n";
        assert_eq!(extract_theme_hint(input), Some("dark".to_string()));
    }

    #[test]
    fn extracts_theme_from_init_directive() {
        let input = "%%{init: {\"theme\": \"forest\"}}%%\nstateDiagram-v2\n[*] --> Idle\n";
        assert_eq!(extract_theme_hint(input), Some("forest".to_string()));
    }

    #[test]
    fn strips_theme_only_frontmatter_for_strict_validation() {
        let input = "---\nconfig:\n  theme: dark\n---\ngraph TD\nA-->B\n";
        assert_eq!(
            strip_theme_only_compat_syntax(input),
            Some("graph TD\nA-->B\n".to_string())
        );
    }

    #[test]
    fn keeps_non_theme_init_directives_intact() {
        let input = "%%{init: {\"theme\": \"dark\", \"flowchart\": {\"curve\": \"basis\"}}}%%\ngraph TD\nA-->B\n";
        assert!(strip_theme_only_compat_syntax(input).is_none());
    }
}
