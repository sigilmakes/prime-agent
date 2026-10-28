//! Sequence diagram instance implementation.

use super::compiler;
use crate::errors::{ParseDiagnostic, RenderError};
use crate::mermaid::sequence::parse_sequence;
use crate::registry::{DiagramInstance, ParsedDiagram};
use crate::timeline::Sequence;

/// Sequence diagram instance.
///
/// Parses sequence diagram syntax, compiles to `Sequence`, then
/// renders through the timeline-family pipeline (layout + text renderer).
#[derive(Default)]
pub struct SequenceInstance;

impl SequenceInstance {
    /// Create a new sequence diagram instance.
    pub fn new() -> Self {
        Self
    }
}

impl DiagramInstance for SequenceInstance {
    fn parse(
        self: Box<Self>,
        input: &str,
    ) -> Result<Box<dyn ParsedDiagram>, Box<dyn std::error::Error + Send + Sync>> {
        let result = parse_sequence(input)?;
        Ok(Box::new(ParsedSequence {
            model: compiler::compile(&result.statements)?,
        }))
    }

    fn validation_warnings(&self, input: &str) -> Vec<ParseDiagnostic> {
        match parse_sequence(input) {
            Ok(result) => result.warnings,
            Err(_) => Vec::new(),
        }
    }
}

struct ParsedSequence {
    model: Sequence,
}

impl ParsedDiagram for ParsedSequence {
    fn into_payload(self: Box<Self>) -> Result<crate::payload::Diagram, RenderError> {
        Ok(crate::payload::Diagram::Sequence(self.model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_instance_builds_sequence_payload() {
        let payload = Box::new(SequenceInstance::new())
            .parse("sequenceDiagram\nparticipant A\nparticipant B\nA->>B: hello")
            .expect("sequence input should parse")
            .into_payload()
            .expect("sequence input should build a payload");
        let crate::payload::Diagram::Sequence(sequence) = payload else {
            panic!("sequence should yield a sequence payload");
        };
        assert_eq!(sequence.participants.len(), 2);
        assert_eq!(sequence.events.len(), 1);
    }

    #[test]
    fn sequence_instance_reports_warnings_for_unknown_lines() {
        let instance = SequenceInstance::new();
        let warnings =
            instance.validation_warnings("sequenceDiagram\nunsupported directive\nparticipant B");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("unsupported directive"));
    }

    #[test]
    fn sequence_instance_accepts_interaction_operators_without_warnings() {
        let instance = SequenceInstance::new();
        let warnings = instance
            .validation_warnings("sequenceDiagram\nalt ready\nparticipant B\nelse later\nend");
        assert!(warnings.is_empty());
    }

    #[test]
    fn sequence_instance_accepts_additional_block_operators_without_warnings() {
        let instance = SequenceInstance::new();
        let warnings = instance.validation_warnings(
            "sequenceDiagram\npar notify\nparticipant A\nand\nparticipant B\nend\ncritical connect\noption timeout\nend\nbreak stop\nend",
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn sequence_instance_accepts_participant_boxes_without_warnings() {
        let instance = SequenceInstance::new();
        let warnings = instance.validation_warnings(
            "sequenceDiagram\nbox blue Frontend\nparticipant A\nactor B\nend\nA->>B: hello",
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn sequence_instance_no_warnings_for_clean_input() {
        let instance = SequenceInstance::new();
        let warnings = instance.validation_warnings("sequenceDiagram\nparticipant A\nA->>B: hello");
        assert!(warnings.is_empty());
    }

    // Engine selection rejection and format support are now tested at the
    // runtime/registry level (see tests/sequence_instance.rs).
}
