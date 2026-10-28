//! Shared sequence diagram model.
//!
//! The validated model used by the timeline layout engine. Produced by
//! compiling the raw parsed AST statements.

/// Participant type used by the sequence runtime model and layout engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipantKind {
    /// Box participant (default).
    Participant,
    /// Stick-figure actor.
    Actor,
}

/// A participant in the sequence diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    /// Unique identifier.
    pub id: String,
    /// Display label (alias if provided, otherwise id).
    pub label: String,
    /// Whether this is a participant box or actor stick-figure.
    pub kind: ParticipantKind,
}

/// A named and/or colored grouping around a contiguous set of participants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantBox {
    /// Optional display label for the grouping.
    pub label: Option<String>,
    /// Optional SVG fill color.
    pub color: Option<String>,
    /// Indices into `Sequence::participants`.
    pub participants: Vec<usize>,
}

/// Autonumber state tracked across sequence compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutonumberState {
    /// Whether autonumbering is currently enabled.
    pub enabled: bool,
    /// Next message number to assign when enabled.
    pub next: usize,
    /// Step between successive message numbers.
    pub step: usize,
}

impl Default for AutonumberState {
    fn default() -> Self {
        Self {
            enabled: false,
            next: 1,
            step: 1,
        }
    }
}

/// Line style for a message arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    /// Solid line.
    Solid,
    /// Dashed line.
    Dashed,
}

/// Arrowhead shape for a message arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowHead {
    /// No arrowhead (`-`).
    None,
    /// Filled arrowhead (`>>`).
    Filled,
    /// Cross terminal (`x`).
    Cross,
    /// Async open-chevron terminal (`)`).
    Async,
}

/// Placement of a note relative to participant(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotePlacement {
    /// Note positioned to the left of a participant's lifeline.
    LeftOf,
    /// Note positioned to the right of a participant's lifeline.
    RightOf,
    /// Note centered over one participant, or spanning between two.
    Over,
}

/// Block operator kind for interaction regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Loop,
    Alt,
    Opt,
    Par,
    Critical,
    Break,
    Rect,
}

impl BlockKind {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Alt => "alt",
            Self::Opt => "opt",
            Self::Par => "par",
            Self::Critical => "critical",
            Self::Break => "break",
            Self::Rect => "rect",
        }
    }
}

/// Divider kind used within block operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDividerKind {
    Else,
    And,
    Option,
}

impl BlockDividerKind {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Else => "else",
            Self::And => "and",
            Self::Option => "option",
        }
    }
}

/// An event in the sequence (message, note, or activation change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceEvent {
    /// A message between (or within) participants.
    Message {
        /// Index into `Sequence::participants`.
        from: usize,
        /// Index into `Sequence::participants`.
        to: usize,
        /// Line style (solid or dashed).
        line_style: LineStyle,
        /// Arrowhead shape.
        arrow_head: ArrowHead,
        /// Message text label.
        text: String,
        /// Optional autonumber prefix (1-indexed).
        number: Option<usize>,
    },
    /// A note positioned relative to one or two participants.
    Note {
        /// How the note is placed (left of, right of, or over).
        placement: NotePlacement,
        /// Participant indices (1 for left/right/over-single, 2 for spanning).
        participants: Vec<usize>,
        /// Note text.
        text: String,
    },
    /// Begin an activation on a participant's lifeline.
    ActivateStart {
        /// Index into `Sequence::participants`.
        participant: usize,
    },
    /// End an activation on a participant's lifeline.
    ActivateEnd {
        /// Index into `Sequence::participants`.
        participant: usize,
    },
    /// Marks the next message as the participant's creation point.
    CreateParticipant {
        /// Index into `Sequence::participants`.
        participant: usize,
    },
    /// Marks the next message as the participant's destruction point.
    DestroyParticipant {
        /// Index into `Sequence::participants`.
        participant: usize,
    },
    /// Start of a labeled interaction block (loop/alt/opt).
    BlockStart { kind: BlockKind, label: String },
    /// Divider between branches within a block (e.g. `else`).
    BlockDivider {
        kind: BlockDividerKind,
        label: String,
    },
    /// End of the current interaction block.
    BlockEnd,
}

/// The validated sequence diagram model.
///
/// Participants are in stable declaration order. Events reference participants
/// by index. This is the input to the timeline layout engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    /// Optional diagram title.
    pub title: Option<String>,
    /// Participants in declaration order.
    pub participants: Vec<Participant>,
    /// Visual groupings around participant columns.
    pub participant_boxes: Vec<ParticipantBox>,
    /// Events in source order.
    pub events: Vec<SequenceEvent>,
    /// Final autonumber state after compiling all statements.
    pub autonumber: AutonumberState,
}
