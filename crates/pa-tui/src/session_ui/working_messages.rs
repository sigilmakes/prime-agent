//! Optional native loader labels. Installing working-messages.json opts in; no extension runtime.
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Default)]
pub(super) struct WorkingMessages {
    messages: Vec<String>,
    selected: Option<String>,
}

impl WorkingMessages {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let messages: Vec<String> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {} as a JSON string array", path.display()))?;
        anyhow::ensure!(
            messages.iter().all(|message| !message.trim().is_empty() && !message.chars().any(char::is_control)),
            "{}: working messages must be nonempty single-line text without control characters",
            path.display()
        );
        Ok(Self {
            messages,
            selected: None,
        })
    }

    pub(super) fn start_turn(&mut self) {
        self.selected = if self.messages.is_empty() {
            None
        } else {
            let random = u128::from_le_bytes(*uuid::Uuid::new_v4().as_bytes());
            Some(self.messages[(random % self.messages.len() as u128) as usize].clone())
        };
    }

    pub(super) fn reset(&mut self) {
        self.selected = None;
    }

    pub(super) fn selected(&self) -> Option<String> {
        self.selected.clone()
    }
}
