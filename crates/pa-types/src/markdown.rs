//! Markdown presentation vocabulary shared by the settings store and every client.
//!
//! The TS product types these values in `packages/coding-agent/src/core/settings-manager.ts`
//! and consumes them in the TUI; the Rust split puts the vocabulary here in `pa-types` so
//! `pa-core` (settings) and `pa-tui` (rendering) name the same values.

use serde::{Deserialize, Serialize};

/// `markdown.mermaid`: when a fenced mermaid block renders as a diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MermaidRenderingMode {
    /// Always the fenced source.
    Off,
    /// Only once the message settles — a streaming diagram is usually half-written.
    Final,
    /// Every frame, source permitting (TS default).
    #[default]
    Streaming,
}

impl MermaidRenderingMode {
    /// The settings wire spelling (`off`/`final`/`streaming`). Unknown values fall back to
    /// the TS access-time default (streaming) rather than failing a read.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value.trim() {
            "off" => Self::Off,
            "final" => Self::Final,
            _ => Self::Streaming,
        }
    }

    /// The settings wire spelling.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Final => "final",
            Self::Streaming => "streaming",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MermaidRenderingMode;

    #[test]
    fn wire_names_round_trip() {
        for mode in [
            MermaidRenderingMode::Off,
            MermaidRenderingMode::Final,
            MermaidRenderingMode::Streaming,
        ] {
            assert_eq!(MermaidRenderingMode::from_wire(mode.wire_name()), mode);
        }
        assert_eq!(
            MermaidRenderingMode::from_wire("nonsense"),
            MermaidRenderingMode::Streaming
        );
    }
}
