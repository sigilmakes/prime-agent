//! Display-only active-branch projection. Never use this sequence as model context.

use std::borrow::Cow;
use std::collections::HashMap;

use super::view::windowed_transcript;
use super::{json, Result, SessionFile, Value, WindowedTranscript};

impl SessionFile {
    /// Full chronological display transcript, including compacted-away messages.
    /// A trimmed store must be hydrated before this read, never silently projected.
    pub(crate) fn display_messages(&self) -> Result<Vec<Value>> {
        let mut messages = Vec::new();
        self.walk_display_values(|_, message| {
            messages.push(message.into_owned());
            true
        })?;
        Ok(messages)
    }

    /// Prefix in display-transcript coordinates (not compacted model coordinates).
    pub(crate) fn display_messages_before(&self, before: usize) -> Result<Vec<Value>> {
        let mut messages = Vec::new();
        self.walk_display_values(|index, message| {
            if index >= before {
                return false;
            }
            messages.push(message.into_owned());
            true
        })?;
        Ok(messages)
    }

    /// A tool-pair-safe display suffix with its omitted prefix count.
    pub(crate) fn windowed_display_messages(
        &self,
        tail_budget: usize,
    ) -> Result<WindowedTranscript> {
        let mut messages = Vec::new();
        self.walk_display_values(|_, message| {
            messages.push(message);
            true
        })?;
        Ok(windowed_transcript(messages, tail_budget))
    }

    fn walk_display_values<'a>(
        &'a self,
        mut visit: impl FnMut(usize, Cow<'a, Value>) -> bool,
    ) -> Result<()> {
        anyhow::ensure!(
            self.window.is_none(),
            "Full history required for display transcript"
        );
        let branch = self.branch();
        anyhow::ensure!(
            branch
                .first()
                .map_or(self.leaf_id().is_none(), |entry| entry.parent_id.is_none()),
            "Display transcript branch is incomplete or cyclic"
        );
        // Receipts can be appended after their tool result. Read the whole active
        // path before visiting even a prefix, and never look at sibling entries.
        let mut receipts: HashMap<&str, Vec<Value>> = HashMap::new();
        for entry in &branch {
            if entry.type_ != "custom"
                || entry.fields.get("customType").and_then(Value::as_str)
                    != Some("ipython_sent_agent_message")
            {
                continue;
            }
            let Some(data) = entry.fields.get("data") else {
                continue;
            };
            let Some(call) = data.get("toolCallId").and_then(Value::as_str) else {
                continue;
            };
            let Some(sent) = data.get("message") else {
                continue;
            };
            let Some(target) = sent.get("target") else {
                continue;
            };
            if sent.get("id").and_then(Value::as_str).is_none()
                || sent.get("message").and_then(Value::as_str).is_none()
                || !matches!(
                    sent.get("deliveryStatus").and_then(Value::as_str),
                    Some("delivered" | "queued")
                )
                || target
                    .get("activeSessionId")
                    .and_then(Value::as_str)
                    .is_none()
                || target.get("sessionId").and_then(Value::as_str).is_none()
            {
                continue;
            }
            let mut receipt = json!({
                "id": sent["id"], "message": sent["message"], "deliveryStatus": sent["deliveryStatus"],
                "target": {"activeSessionId": target["activeSessionId"], "sessionId": target["sessionId"]},
            });
            if let Some(name) = target.get("sessionName").and_then(Value::as_str) {
                receipt["target"]["sessionName"] = json!(name);
            }
            receipts.entry(call).or_default().push(receipt);
        }
        let mut index = 0;
        for entry in branch {
            let timestamp = crate::util::iso_to_unix_ms(&entry.timestamp).unwrap_or(0);
            let message = match entry.type_.as_str() {
                "message" => entry
                    .fields
                    .get("message")
                    .filter(|message| {
                        message.get("role").and_then(Value::as_str) != Some("custom")
                            || message.get("display").and_then(Value::as_bool) == Some(true)
                    })
                    .map(Cow::Borrowed),
                "custom_message"
                    if entry.fields.get("display").and_then(Value::as_bool) == Some(true) =>
                {
                    let mut message = json!({
                        "role": "custom", "customType": entry.fields["customType"],
                        "content": entry.fields["content"], "display": true,
                    });
                    if let Some(details) = entry.fields.get("details") {
                        message["details"] = details.clone();
                    }
                    message["timestamp"] = json!(timestamp);
                    Some(Cow::Owned(message))
                }
                "compaction" => {
                    let mut message = json!({
                        "role": "compactionSummary", "summary": entry.fields["summary"],
                        "tokensBefore": entry.fields.get("tokensBefore").cloned().unwrap_or(json!(0)),
                    });
                    if let Some(instructions) = entry.fields.get("customInstructions") {
                        message["customInstructions"] = instructions.clone();
                    }
                    message["timestamp"] = json!(timestamp);
                    Some(Cow::Owned(message))
                }
                "branch_summary"
                    if entry
                        .fields
                        .get("summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| !summary.is_empty()) =>
                {
                    Some(Cow::Owned(json!({
                        "role": "branchSummary", "summary": entry.fields["summary"],
                        "fromId": entry.fields["fromId"], "timestamp": timestamp,
                    })))
                }
                _ => None,
            };
            let Some(mut message) = message else { continue };
            let sent = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .filter(|_| {
                    message.get("role").and_then(Value::as_str) == Some("toolResult")
                        && message.get("toolName").and_then(Value::as_str) == Some("ipython")
                })
                .and_then(|call| receipts.get(call));
            if let Some(sent) = sent {
                let message = message.to_mut();
                let mut details = message
                    .get("details")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let mut current = details
                    .get("sentAgentMessages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for receipt in sent {
                    if !current
                        .iter()
                        .any(|existing| existing.get("id") == receipt.get("id"))
                    {
                        current.push(receipt.clone());
                    }
                }
                details.insert("sentAgentMessages".to_owned(), Value::Array(current));
                message["details"] = Value::Object(details);
            }
            if !visit(index, message) {
                break;
            }
            index += 1;
        }
        Ok(())
    }
}
