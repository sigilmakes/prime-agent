//! MMDS document types for sequence diagrams.
//!
//! Defines the serde-serializable envelope for sequence MMDS JSON. The builder
//! that populates these types from layout data lives in `runtime` (which has
//! access to both `render` and `timeline`).

use serde::Serialize;

use crate::graph::GeometryLevel;

// ---------------------------------------------------------------------------
// Sequence MMDS vocabulary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SequenceDiagramType {
    Sequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParticipantKind {
    Participant,
    Actor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LineStyle {
    Solid,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArrowHead {
    None,
    Filled,
    Cross,
    Async,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotePlacement {
    LeftOf,
    RightOf,
    Over,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BlockKind {
    Loop,
    Alt,
    Opt,
    Par,
    Critical,
    Break,
    Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BlockDividerKind {
    Else,
    And,
    Option,
}

// ---------------------------------------------------------------------------
// Document types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct SequenceDocument {
    pub version: u32,
    pub geometry_level: GeometryLevel,
    pub metadata: SequenceMetadata,
    // Envelope compat — always empty for sequence diagrams.
    pub nodes: Vec<()>,
    pub edges: Vec<()>,
    // Sequence-specific body.
    pub participants: Vec<Participant>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub activations: Vec<Activation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<Block>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub participant_boxes: Vec<ParticipantBox>,
}

#[derive(Serialize)]
pub(crate) struct SequenceMetadata {
    pub diagram_type: SequenceDiagramType,
    pub bounds: Bounds,
}

#[derive(Serialize)]
pub(crate) struct Bounds {
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize)]
pub(crate) struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize)]
pub(crate) struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize)]
pub(crate) struct Participant {
    pub id: String,
    pub label: String,
    pub kind: ParticipantKind,
    pub position: Position,
    pub size: Size,
    pub lifeline_x: f64,
}

#[derive(Serialize)]
pub(crate) struct Message {
    pub id: String,
    pub from: usize,
    pub to: usize,
    pub line_style: LineStyle,
    pub arrow_head: ArrowHead,
    pub text: String,
    pub y: f64,
}

#[derive(Serialize)]
pub(crate) struct Note {
    pub placement: NotePlacement,
    pub participants: Vec<usize>,
    pub text: String,
    pub position: Position,
    pub size: Size,
}

#[derive(Serialize)]
pub(crate) struct Activation {
    pub participant: usize,
    pub y_start: f64,
    pub y_end: f64,
    pub depth: usize,
}

#[derive(Serialize)]
pub(crate) struct BlockDivider {
    pub y: f64,
    pub kind: BlockDividerKind,
    pub label: String,
}

#[derive(Serialize)]
pub(crate) struct Block {
    pub kind: BlockKind,
    pub label: String,
    pub rect: Rect,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dividers: Vec<BlockDivider>,
}

#[derive(Serialize)]
pub(crate) struct ParticipantBox {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub participants: Vec<usize>,
    pub rect: Rect,
}

/// Serialize a pre-built sequence document to pretty-printed JSON.
pub(crate) fn serialize(document: &SequenceDocument) -> String {
    serde_json::to_string_pretty(document).expect("sequence MMDS serialization should not fail")
}
