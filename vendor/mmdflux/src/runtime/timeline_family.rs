//! Runtime rendering for timeline-family payloads (sequence diagrams).
//!
//! Parallel to `graph_family` — computes SVG layout and serializes to MMDS
//! JSON via the types defined in `mmds::sequence`.

use crate::graph::GeometryLevel;
use crate::graph::measure::ProportionalTextMetrics;
use crate::mmds::sequence::{
    self, Activation, ArrowHead, Block, BlockDivider, BlockDividerKind, BlockKind, Bounds,
    LineStyle, Message, Note, NotePlacement, Participant, ParticipantBox, ParticipantKind,
    Position, Rect, SequenceDiagramType, SequenceDocument, SequenceMetadata, Size,
};
use crate::render::timeline::svg_layout::{
    self as svg_layout, SvgBlock, SvgRow, SvgSequenceLayout,
};
use crate::timeline::sequence::model::{
    ArrowHead as TimelineArrowHead, BlockDividerKind as TimelineBlockDividerKind,
    BlockKind as TimelineBlockKind, LineStyle as TimelineLineStyle,
    ParticipantKind as TimelineParticipantKind, Sequence,
};

// ---------------------------------------------------------------------------
// MMDS vocabulary conversion helpers
// ---------------------------------------------------------------------------

fn mmds_line_style(s: TimelineLineStyle) -> LineStyle {
    match s {
        TimelineLineStyle::Solid => LineStyle::Solid,
        TimelineLineStyle::Dashed => LineStyle::Dashed,
    }
}

fn mmds_arrow_head(a: TimelineArrowHead) -> ArrowHead {
    match a {
        TimelineArrowHead::None => ArrowHead::None,
        TimelineArrowHead::Filled => ArrowHead::Filled,
        TimelineArrowHead::Cross => ArrowHead::Cross,
        TimelineArrowHead::Async => ArrowHead::Async,
    }
}

fn mmds_participant_kind(k: &TimelineParticipantKind) -> ParticipantKind {
    match k {
        TimelineParticipantKind::Participant => ParticipantKind::Participant,
        TimelineParticipantKind::Actor => ParticipantKind::Actor,
    }
}

fn mmds_block_kind(k: TimelineBlockKind) -> BlockKind {
    match k {
        TimelineBlockKind::Loop => BlockKind::Loop,
        TimelineBlockKind::Alt => BlockKind::Alt,
        TimelineBlockKind::Opt => BlockKind::Opt,
        TimelineBlockKind::Par => BlockKind::Par,
        TimelineBlockKind::Critical => BlockKind::Critical,
        TimelineBlockKind::Break => BlockKind::Break,
        TimelineBlockKind::Rect => BlockKind::Rect,
    }
}

fn mmds_block_divider_kind(k: TimelineBlockDividerKind) -> BlockDividerKind {
    match k {
        TimelineBlockDividerKind::Else => BlockDividerKind::Else,
        TimelineBlockDividerKind::And => BlockDividerKind::And,
        TimelineBlockDividerKind::Option => BlockDividerKind::Option,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Serialize a sequence diagram model to MMDS JSON using proportional layout
/// positions from the SVG layout engine.
pub(in crate::runtime) fn to_json(model: &Sequence, metrics: &ProportionalTextMetrics) -> String {
    let svg = svg_layout::layout(model, metrics, "sans-serif");
    let document = build_document(model, &svg);
    sequence::serialize(&document)
}

fn build_document(model: &Sequence, svg: &SvgSequenceLayout) -> SequenceDocument {
    let participants = build_participants(model, svg);
    let (messages, notes) = build_messages_and_notes(svg);
    let activations = build_activations(svg);
    let blocks = build_blocks(svg);
    let participant_boxes = build_participant_boxes(model, svg);

    SequenceDocument {
        version: 1,
        geometry_level: GeometryLevel::Layout,
        metadata: SequenceMetadata {
            diagram_type: SequenceDiagramType::Sequence,
            bounds: Bounds {
                width: svg.width,
                height: svg.height,
            },
        },
        nodes: Vec::new(),
        edges: Vec::new(),
        participants,
        messages,
        notes,
        activations,
        blocks,
        participant_boxes,
    }
}

fn build_participants(model: &Sequence, svg: &SvgSequenceLayout) -> Vec<Participant> {
    model
        .participants
        .iter()
        .zip(svg.participants.iter())
        .map(|(p, sp)| Participant {
            id: p.id.clone(),
            label: p.label.clone(),
            kind: mmds_participant_kind(&p.kind),
            position: Position {
                x: sp.rect.x,
                y: sp.rect.y,
            },
            size: Size {
                width: sp.rect.width,
                height: sp.rect.height,
            },
            lifeline_x: sp.center_x,
        })
        .collect()
}

fn build_messages_and_notes(svg: &SvgSequenceLayout) -> (Vec<Message>, Vec<Note>) {
    let mut messages = Vec::new();
    let mut notes = Vec::new();
    let mut msg_idx = 0usize;

    let lifeline_xs: Vec<f64> = svg.participants.iter().map(|p| p.center_x).collect();

    for row in &svg.rows {
        match row {
            SvgRow::Message(m) => {
                let from = nearest_participant(&lifeline_xs, m.from_x);
                let to = nearest_participant(&lifeline_xs, m.to_x);
                messages.push(Message {
                    id: format!("m{msg_idx}"),
                    from,
                    to,
                    line_style: mmds_line_style(m.line_style),
                    arrow_head: mmds_arrow_head(m.arrow_head),
                    text: m.label.clone(),
                    y: m.y,
                });
                msg_idx += 1;
            }
            SvgRow::SelfMessage(sm) => {
                let from = nearest_participant(&lifeline_xs, sm.x);
                messages.push(Message {
                    id: format!("m{msg_idx}"),
                    from,
                    to: from,
                    line_style: mmds_line_style(sm.line_style),
                    arrow_head: mmds_arrow_head(sm.arrow_head),
                    text: sm.label.clone(),
                    y: sm.y,
                });
                msg_idx += 1;
            }
            SvgRow::Note(n) => {
                let participants = participants_for_note(&lifeline_xs, n);
                let placement = placement_for_note(&lifeline_xs, n, &participants);
                notes.push(Note {
                    placement,
                    participants,
                    text: n.text.clone(),
                    position: Position {
                        x: n.rect.x,
                        y: n.rect.y,
                    },
                    size: Size {
                        width: n.rect.width,
                        height: n.rect.height,
                    },
                });
            }
        }
    }

    (messages, notes)
}

/// Find the participant index whose lifeline is closest to the given x.
fn nearest_participant(lifeline_xs: &[f64], x: f64) -> usize {
    lifeline_xs
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (x - *a).abs().partial_cmp(&(x - *b).abs()).unwrap())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Determine which participants a note is associated with based on its position.
fn participants_for_note(lifeline_xs: &[f64], note: &svg_layout::SvgNote) -> Vec<usize> {
    let note_left = note.rect.x;
    let note_right = note.rect.x + note.rect.width;

    // Check if the note spans between two participants (over A,B).
    let mut covered: Vec<usize> = lifeline_xs
        .iter()
        .enumerate()
        .filter(|(_, lx)| **lx >= note_left && **lx <= note_right)
        .map(|(i, _)| i)
        .collect();

    if covered.is_empty() {
        let note_center = note.rect.x + note.rect.width / 2.0;
        covered.push(nearest_participant(lifeline_xs, note_center));
    }

    covered
}

/// Determine note placement from its position relative to participants.
fn placement_for_note(
    lifeline_xs: &[f64],
    note: &svg_layout::SvgNote,
    participants: &[usize],
) -> NotePlacement {
    if participants.len() > 1 {
        return NotePlacement::Over;
    }
    let p_idx = participants[0];
    let lx = lifeline_xs[p_idx];
    let note_center = note.rect.x + note.rect.width / 2.0;
    if note_center < lx {
        NotePlacement::LeftOf
    } else if note_center > lx {
        NotePlacement::RightOf
    } else {
        NotePlacement::Over
    }
}

fn build_activations(svg: &SvgSequenceLayout) -> Vec<Activation> {
    let lifeline_xs: Vec<f64> = svg.participants.iter().map(|p| p.center_x).collect();
    svg.activations
        .iter()
        .map(|a| {
            let participant = nearest_participant(&lifeline_xs, a.x);
            Activation {
                participant,
                y_start: a.y_start,
                y_end: a.y_end,
                depth: a.depth,
            }
        })
        .collect()
}

fn build_blocks(svg: &SvgSequenceLayout) -> Vec<Block> {
    svg.blocks
        .iter()
        .map(|b: &SvgBlock| Block {
            kind: mmds_block_kind(b.kind),
            label: b.label.clone(),
            rect: Rect {
                x: b.rect.x,
                y: b.rect.y,
                width: b.rect.width,
                height: b.rect.height,
            },
            dividers: b
                .dividers
                .iter()
                .map(|d| BlockDivider {
                    y: d.y,
                    kind: mmds_block_divider_kind(d.kind),
                    label: d.label.clone(),
                })
                .collect(),
        })
        .collect()
}

fn build_participant_boxes(model: &Sequence, svg: &SvgSequenceLayout) -> Vec<ParticipantBox> {
    model
        .participant_boxes
        .iter()
        .zip(svg.participant_boxes.iter())
        .map(|(pb, spb)| ParticipantBox {
            label: pb.label.clone(),
            color: pb.color.clone(),
            participants: pb.participants.clone(),
            rect: Rect {
                x: spb.rect.x,
                y: spb.rect.y,
                width: spb.rect.width,
                height: spb.rect.height,
            },
        })
        .collect()
}
