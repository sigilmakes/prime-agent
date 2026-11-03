use serde_json::Value;

use crate::theme::ThemeColor;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct McpServerRow {
    pub(super) server: String,
    pub(super) label: String,
    connected: bool,
    uses_oauth: bool,
    auth_kind: String,
    transport: String,
}

impl McpServerRow {
    pub(super) fn from_value(value: &Value) -> Option<Self> {
        Some(McpServerRow {
            server: value.get("server")?.as_str()?.to_string(),
            label: value
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            connected: value
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            uses_oauth: value
                .get("usesOAuth")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            auth_kind: value
                .get("authKind")
                .and_then(Value::as_str)
                .unwrap_or("http")
                .to_string(),
            transport: value
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("http")
                .to_string(),
        })
    }

    pub(super) fn status_text(&self) -> (ThemeColor, String) {
        if self.connected {
            (ThemeColor::Success, "Connected".to_string())
        } else if self.uses_oauth {
            (ThemeColor::Text, "Connect".to_string())
        } else {
            (ThemeColor::Muted, "Not connected".to_string())
        }
    }

    pub(super) fn detail_text(&self) -> String {
        format!("{} \u{b7} {}", self.transport, self.auth_kind)
    }

    pub(super) fn action_text(&self) -> &'static str {
        if self.connected {
            "reconnect"
        } else {
            "connect"
        }
    }

    pub(super) fn matches(&self, tokens: &[String]) -> bool {
        let haystack = format!(
            "{} {} {} {}",
            self.label, self.server, self.auth_kind, self.transport
        )
        .to_lowercase();
        tokens.iter().all(|token| haystack.contains(token))
    }
}

pub(super) fn flatten_to_single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
