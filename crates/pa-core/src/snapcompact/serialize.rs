//! Conversation serialization adapted from upstream (assets/licenses/UPSTREAM-MIT).
use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
};

use pa_types::ai::{AssistantContentBlock, UserContent, UserContentBlock};
use pa_types::session::AgentMessage as Message;
use serde::{Deserialize, Serialize};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use super::renderer;

// The renderer/source limits are not enough if serialization first clones an
// arbitrarily large message. Count only portable fields before allocating.
struct InputBudget<'a> {
    remaining: usize,
    cancelled: &'a dyn Fn() -> bool,
}
impl Write for InputBudget<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other("Snapcompact cancelled"));
        }
        if bytes.len() > self.remaining {
            return Err(io::Error::other("portable input exceeds allocation bound"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn check_input(messages: &[Message], cancelled: &dyn Fn() -> bool) -> super::Result<()> {
    if messages.len() > 100_000 {
        return Err(super::Error::Budget("input message count".into()));
    }
    let mut budget = InputBudget {
        remaining: super::MAX_SOURCE_BYTES,
        cancelled,
    };
    let mut scan = || -> io::Result<()> {
        for message in messages {
            budget.write_all(&[0; 64])?;
            match message {
                Message::User(user) => write_content(&mut budget, &user.content)?,
                Message::Custom(custom) => {
                    write_content(&mut budget, &custom.content)?;
                    serde_json::to_writer(&mut budget, &custom.custom_type)?;
                }
                Message::BranchSummary(summary) => {
                    serde_json::to_writer(&mut budget, &summary.summary)?;
                }
                Message::CompactionSummary(summary) => {
                    serde_json::to_writer(&mut budget, &summary.summary)?;
                }
                Message::BashExecution(bash) => {
                    serde_json::to_writer(&mut budget, &bash.command)?;
                    serde_json::to_writer(&mut budget, &bash.output)?;
                }
                Message::ToolResult(result) => write_blocks(&mut budget, &result.content)?,
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        budget.write_all(&[0; 64])?;
                        match block {
                            AssistantContentBlock::Text(text) => {
                                serde_json::to_writer(&mut budget, &text.text)?;
                            }
                            AssistantContentBlock::Thinking(thinking) => {
                                serde_json::to_writer(&mut budget, &thinking.thinking)?;
                            }
                            AssistantContentBlock::ToolCall(call) => {
                                serde_json::to_writer(&mut budget, &call.name)?;
                                serde_json::to_writer(&mut budget, &call.arguments)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    };
    scan().map_err(|_| {
        if cancelled() {
            super::Error::Cancelled
        } else {
            super::Error::Budget("portable input exceeds allocation bound".into())
        }
    })
}
fn write_content(budget: &mut InputBudget<'_>, content: &UserContent) -> io::Result<()> {
    match content {
        UserContent::Text(text) => {
            serde_json::to_writer(budget, text)?;
            Ok(())
        }
        UserContent::Blocks(blocks) => write_blocks(budget, blocks),
    }
}
fn write_blocks(budget: &mut InputBudget<'_>, blocks: &[UserContentBlock]) -> io::Result<()> {
    for block in blocks {
        budget.write_all(&[0; 16])?;
        match block {
            UserContentBlock::Text(text) => serde_json::to_writer(&mut *budget, &text.text)?,
            UserContentBlock::Image(_) | UserContentBlock::Raw(_) => {}
        }
    }
    Ok(())
}

/// Explicit serializer policy. Caps include the omission marker itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializeOptions {
    pub include_thinking: bool,
    pub dim_tool_results: bool,
    pub tool_result_chars: usize,
    pub argument_chars: usize,
    pub call_chars: usize,
}
impl Default for SerializeOptions {
    fn default() -> Self {
        Self {
            include_thinking: true,
            dim_tool_results: true,
            tool_result_chars: 2000,
            argument_chars: 500,
            call_chars: 2000,
        }
    }
}

pub(super) fn truncate(text: &str, cap: usize) -> String {
    let len = text.chars().count();
    if len <= cap {
        return text.to_owned();
    }
    let marker = format!(" [...{len}ch bounded...] ");
    let remaining = cap.saturating_sub(marker.chars().count());
    let head = remaining * 3 / 5;
    let tail = remaining - head;
    if remaining == 0 {
        return marker.chars().take(cap).collect();
    }
    let prefix: String = text.chars().take(head).collect();
    let suffix: String = text.chars().skip(len - tail).collect();
    format!("{prefix}{marker}{suffix}")
}

// Scrub data URLs before any structure-blind slicing. Deliberately also scrub
// short/damaged atoms: a literal mention is less important than safe replay.
pub(super) fn scrub(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if !ch.is_control() || matches!(ch, '\n' | '\r' | '\t') {
            out.push(ch);
        }
    }
    let lower = out.to_ascii_lowercase();
    let mut clean = String::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("data:") {
        let start = cursor + relative;
        let Some(relative_end) = lower[start..].find(";base64,") else {
            break;
        };
        let header_end = start + relative_end + 8;
        if out[start..header_end].chars().any(char::is_whitespace) {
            clean.push_str(&out[cursor..start + 5]);
            cursor = start + 5;
            continue;
        }
        let mut end = header_end;
        for c in out[header_end..].chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=') {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        clean.push_str(&out[cursor..start]);
        clean.push_str("[data URL omitted]");
        cursor = end;
    }
    clean.push_str(&out[cursor..]);
    clean
}

fn result_text(blocks: &[UserContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text(text) => Some(text.text.as_str()),
            UserContentBlock::Image(_) | UserContentBlock::Raw(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Serialize only portable conversation text; images and provider-native raw
/// payloads are never copied. Calls and useful results share one call section.
/// This is not a lossless record of canonical history.
#[must_use]
pub fn serialize_conversation(messages: &[Message], options: &SerializeOptions) -> String {
    if check_input(messages, &|| false).is_err() {
        return "[Portable input omitted: serialization allocation bound exceeded]".into();
    }
    let mut useless = HashSet::new();
    let mut results = HashMap::new();
    for message in messages {
        if let Message::ToolResult(result) = message {
            if !result.is_error
                && result
                    .rest
                    .get("useless")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
                useless.insert(result.tool_call_id.as_str());
            } else {
                results.insert(result.tool_call_id.as_str(), result_text(&result.content));
            }
        }
    }
    let render_result = |text: &str| {
        let body = truncate(&scrub(text), options.tool_result_chars.min(2000));
        if options.dim_tool_results {
            format!("<out>\n\u{e}{body}\u{f}\n</out>")
        } else {
            format!("<out>\n{body}\n</out>")
        }
    };
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut merged = HashSet::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => result_text(blocks),
                };
                if !text.is_empty() {
                    parts.push(("¶user:".into(), scrub(&text)));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking = Vec::new();
                let mut text = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContentBlock::Thinking(block) => {
                            if options.include_thinking {
                                thinking.push(scrub(&block.thinking));
                            }
                        }
                        AssistantContentBlock::Text(block) => text.push(scrub(&block.text)),
                        AssistantContentBlock::ToolCall(call) => {
                            if useless.contains(call.id.as_str()) {
                                continue;
                            }
                            flush(&mut parts, &mut thinking, &mut text);
                            let arguments = call
                                .arguments
                                .iter()
                                .map(|(key, value)| {
                                    let value = truncate(
                                        &scrub(&value.to_string()),
                                        options.argument_chars.min(500),
                                    );
                                    format!("{key}={value}")
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let arguments = truncate(&arguments, options.call_chars.min(2000));
                            let mut body = format!("{}({arguments})", scrub(&call.name));
                            if let Some(result) = results.get(call.id.as_str()) {
                                body.push('\n');
                                body.push_str(&render_result(result));
                                merged.insert(call.id.as_str());
                            }
                            parts.push(("¶call:".into(), body));
                        }
                    }
                }
                flush(&mut parts, &mut thinking, &mut text);
            }
            Message::ToolResult(result) => {
                if !useless.contains(result.tool_call_id.as_str())
                    && !merged.contains(result.tool_call_id.as_str())
                {
                    if let Some(body) = results.get(result.tool_call_id.as_str()) {
                        parts.push(("¶call:".into(), render_result(body)));
                    }
                }
            }
            Message::Custom(custom) => {
                let body = match &custom.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => result_text(blocks),
                };
                parts.push((
                    "¶notice:".into(),
                    format!("{}: {}", scrub(&custom.custom_type), scrub(&body)),
                ));
            }
            Message::BranchSummary(summary) => parts.push((
                "¶notice:".into(),
                format!("Branch summary: {}", scrub(&summary.summary)),
            )),
            Message::CompactionSummary(summary) => parts.push((
                "¶notice:".into(),
                format!("Compaction summary: {}", scrub(&summary.summary)),
            )),
            Message::BashExecution(bash) => {
                if bash.exclude_from_context != Some(true) {
                    parts.push((
                        "¶call:".into(),
                        format!(
                            "bash({})\n{}",
                            scrub(&bash.command),
                            render_result(&bash.output)
                        ),
                    ));
                }
            }
        }
    }
    let mut out = String::new();
    let mut last = String::new();
    for (prefix, text) in parts {
        if text.trim().is_empty() {
            continue;
        }
        if prefix == last {
            out.push('\n');
        } else {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&prefix);
        }
        out.push_str(&text);
        last = prefix;
    }
    out
}

fn flush(parts: &mut Vec<(String, String)>, thinking: &mut Vec<String>, text: &mut Vec<String>) {
    if !thinking.is_empty() {
        parts.push(("¶think:".into(), thinking.join("\n")));
        thinking.clear();
    }
    if !text.is_empty() {
        parts.push(("¶ai:".into(), text.join("\n")));
        text.clear();
    }
}

// Internal serializer dim controls survive; raw content has already lost them.
pub(super) fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut whitespace = None;
    for ch in text.chars() {
        if ch.is_whitespace() {
            let newline = matches!(ch, '\n' | '\r');
            whitespace = Some(whitespace.unwrap_or(false) || newline);
            continue;
        }
        if matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            continue;
        }
        if let Some(newline) = whitespace.take() {
            if !out.is_empty() {
                out.push(if newline { '█' } else { ' ' });
            }
        }
        match ch {
            '\u{e}' | '\u{f}' => out.push(ch),
            '‘' | '’' => out.push('\''),
            '“' | '”' => out.push('"'),
            '–' | '—' | '−' | '\u{2500}' | '\u{2501}' => out.push('-'),
            '…' => out.push_str("..."),
            '\u{2502}' | '\u{2503}' => out.push('|'),
            '\u{2504}'..='\u{257f}' => out.push('+'),
            ch if ch.is_control() => {}
            ch if ch.is_ascii() || renderer::supports(ch) => out.push(ch),
            ch => {
                let folded: String = ch
                    .to_string()
                    .nfkd()
                    .filter(|c| !is_combining_mark(*c) && c.is_ascii())
                    .collect();
                if folded.is_empty() {
                    out.push('?');
                } else {
                    out.push_str(&folded);
                }
            }
        }
    }
    out
}

pub(super) fn plain(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, '\u{e}' | '\u{f}'))
        .map(|ch| if ch == '█' { '\n' } else { ch })
        .collect()
}
