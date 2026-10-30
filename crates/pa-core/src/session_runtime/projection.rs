//! The model-facing view of one branch: provider messages, rebuilt per call.

use serde_json::Value;

use super::Entry;

/// One message as the provider list takes it, with the row's role verbatim.
///
/// Mapping a role onto a provider's own vocabulary is the caller's job; this
/// projection only fixes the order and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMessage {
    /// `user`, `assistant`, `toolResult`, or whatever the row named.
    pub role: String,
    /// The message text: a string content, or its text parts joined by newlines.
    pub content: String,
}

/// The projection of `branch` (root first): message rows with text, in log
/// order. Every other row -- usage, labels, selections, tool bookkeeping -- is
/// not a message and does not reach the provider.
pub(crate) fn messages(branch: &[&Entry]) -> Vec<ProviderMessage> {
    branch
        .iter()
        .filter_map(|entry| message(&entry.body))
        .collect()
}

fn message(body: &Value) -> Option<ProviderMessage> {
    if body.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = body.get("message")?;
    let role = message.get("role")?.as_str()?;
    let content = text(message.get("content")?)?;
    Some(ProviderMessage {
        role: role.to_owned(),
        content,
    })
}

fn text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn part_text(part: &Value) -> Option<&str> {
    if part.get("type")?.as_str()? != "text" {
        return None;
    }
    part.get("text")?.as_str()
}
