//! Proportional layout engine for sequence diagram SVG rendering.
//!
//! Parallel to the character-grid `timeline::sequence::layout` but produces
//! f64 pixel coordinates suitable for SVG output. Consumes the shared
//! `Sequence` model and `ProportionalTextMetrics` for text measurement.

use crate::graph::measure::ProportionalTextMetrics;
use crate::timeline::sequence::model::{
    ArrowHead, BlockDividerKind, BlockKind, LineStyle, NotePlacement, ParticipantBox,
    ParticipantKind, Sequence, SequenceEvent,
};

// ---------------------------------------------------------------------------
// Layout constants (pixels)
// ---------------------------------------------------------------------------

/// Horizontal padding inside participant boxes.
const PARTICIPANT_PADDING_X: f64 = 16.0;
/// Vertical padding inside participant boxes.
const PARTICIPANT_PADDING_Y: f64 = 10.0;
/// Padding around grouped participant columns.
const PARTICIPANT_GROUP_PADDING_X: f64 = 18.0;
/// Vertical padding around grouped participant columns.
const PARTICIPANT_GROUP_PADDING_Y: f64 = 14.0;
/// Minimum gap between adjacent participant centers.
const MIN_PARTICIPANT_GAP: f64 = 150.0;
/// Vertical space between the bottom of the header and the first event.
const HEADER_MARGIN: f64 = 20.0;
/// Vertical distance between successive events.
const EVENT_SPACING: f64 = 50.0;
/// Width of the self-message loop arm extending right of the lifeline.
const SELF_MSG_ARM: f64 = 30.0;
/// Vertical height of the self-message loop.
const SELF_MSG_HEIGHT: f64 = 30.0;
/// Horizontal padding inside note boxes.
const NOTE_PADDING_X: f64 = 10.0;
/// Vertical padding inside note boxes.
const NOTE_PADDING_Y: f64 = 8.0;
/// Gap between a lifeline and an adjacent note box.
const NOTE_GAP: f64 = 10.0;
/// Width of activation bars drawn on lifelines.
const ACTIVATION_WIDTH: f64 = 10.0;
/// Padding around the entire diagram.
const DIAGRAM_PADDING: f64 = 10.0;
/// Vertical reservation for an optional diagram title.
const TITLE_VERTICAL_SPACE: f64 = 34.0;
/// Extra gap between message label and arrow line.
const LABEL_ABOVE_GAP: f64 = 4.0;
/// Vertical reservation for block headers, dividers, and footers.
const BLOCK_ROW_SPACING: f64 = 64.0;
/// Horizontal padding between frame borders and enclosed content.
const BLOCK_SIDE_PADDING: f64 = 32.0;
/// Character-width estimate for operator tabs.
const BLOCK_TAB_CHAR_WIDTH: f64 = 8.0;
/// Horizontal padding inside operator tabs.
const BLOCK_TAB_PADDING_X: f64 = 8.0;
/// Minimum width for operator tabs.
const BLOCK_TAB_MIN_WIDTH: f64 = 42.0;
/// Gap between the operator tab and centered guard text.
const BLOCK_HEADER_GAP: f64 = 32.0;

// ---------------------------------------------------------------------------
// Layout output types
// ---------------------------------------------------------------------------

/// Complete proportional layout for SVG rendering.
#[derive(Debug)]
pub struct SvgSequenceLayout {
    pub title: Option<SvgTitle>,
    pub participant_boxes: Vec<SvgParticipantBox>,
    pub participants: Vec<SvgParticipant>,
    pub lifelines: Vec<SvgLifeline>,
    pub destroy_markers: Vec<SvgDestroyMarker>,
    pub rows: Vec<SvgRow>,
    pub blocks: Vec<SvgBlock>,
    pub activations: Vec<SvgActivation>,
    pub width: f64,
    pub height: f64,
    pub font_family: String,
    pub font_size: f64,
}

/// A centered diagram title rendered above the participants.
#[derive(Debug)]
pub struct SvgTitle {
    pub text: String,
    pub y: f64,
}

/// A positioned participant header box.
#[derive(Debug)]
pub struct SvgParticipant {
    pub center_x: f64,
    pub rect: SvgRect,
    pub label: String,
    pub kind: ParticipantKind,
}

/// A vertical grouping behind one or more participant columns.
#[derive(Debug)]
pub struct SvgParticipantBox {
    pub rect: SvgRect,
    pub label: Option<String>,
    pub color: Option<String>,
}

/// Simple rectangle.
#[derive(Debug, Clone)]
pub struct SvgRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A vertical lifeline segment.
#[derive(Debug)]
pub struct SvgLifeline {
    pub x: f64,
    pub y_start: f64,
    pub y_end: f64,
}

#[derive(Debug)]
pub struct SvgDestroyMarker {
    pub x: f64,
    pub y: f64,
}

/// A positioned event in the layout.
#[derive(Debug)]
pub enum SvgRow {
    Message(SvgMessage),
    SelfMessage(SvgSelfMessage),
    Note(SvgNote),
}

/// A message arrow between two lifelines.
#[derive(Debug)]
pub struct SvgMessage {
    pub from_x: f64,
    pub to_x: f64,
    pub y: f64,
    pub label: String,
    pub label_x: f64,
    pub label_y: f64,
    pub line_style: LineStyle,
    pub arrow_head: ArrowHead,
}

/// A self-referencing message (loop to the right of one lifeline).
#[derive(Debug)]
pub struct SvgSelfMessage {
    pub x: f64,
    pub y: f64,
    pub arm_width: f64,
    pub height: f64,
    pub label: String,
    pub label_x: f64,
    pub label_y: f64,
    pub line_style: LineStyle,
    pub arrow_head: ArrowHead,
}

/// A positioned note box.
#[derive(Debug)]
pub struct SvgNote {
    pub rect: SvgRect,
    pub text: String,
}

/// An activation bar on a lifeline.
#[derive(Debug)]
pub struct SvgActivation {
    pub x: f64,
    pub y_start: f64,
    pub y_end: f64,
    pub width: f64,
    pub depth: usize,
}

#[derive(Debug)]
pub struct SvgBlockDivider {
    pub y: f64,
    pub kind: BlockDividerKind,
    pub label: String,
}

#[derive(Debug)]
pub struct SvgBlock {
    pub rect: SvgRect,
    pub depth: usize,
    pub kind: BlockKind,
    pub label: String,
    pub dividers: Vec<SvgBlockDivider>,
}

// ---------------------------------------------------------------------------
// Layout engine
// ---------------------------------------------------------------------------

/// Count how many ActivateStart events for `participant` appear at the head
/// of `events` before the next non-activation event. This handles the
/// shorthand `A->>+B: msg` where the activation starts on the same message.
fn pending_activations(events: &[SequenceEvent], participant: usize) -> usize {
    events
        .iter()
        .take_while(|e| {
            matches!(
                e,
                SequenceEvent::ActivateStart { .. } | SequenceEvent::ActivateEnd { .. }
            )
        })
        .filter(
            |e| matches!(e, SequenceEvent::ActivateStart { participant: p } if *p == participant),
        )
        .count()
}

/// Compute the x position for a message endpoint on a participant.
///
/// If the participant has an active activation (depth > 0), returns the
/// edge of the topmost activation box. Otherwise returns the lifeline center.
fn activation_edge(center_x: f64, depth: usize, right_side: bool) -> f64 {
    if depth == 0 {
        return center_x;
    }
    let top_depth = depth - 1;
    let box_x = center_x - ACTIVATION_WIDTH / 2.0 + (top_depth as f64 * 3.0);
    if right_side {
        box_x + ACTIVATION_WIDTH
    } else {
        box_x
    }
}

fn message_target_x(
    participants: &[SvgParticipant],
    from_idx: usize,
    to_idx: usize,
    created_participant: Option<usize>,
    to_depth: usize,
) -> f64 {
    let target = &participants[to_idx];
    if created_participant == Some(to_idx) && from_idx != to_idx {
        if target.center_x > participants[from_idx].center_x {
            target.rect.x
        } else {
            target.rect.x + target.rect.width
        }
    } else {
        activation_edge(
            target.center_x,
            to_depth,
            target.center_x < participants[from_idx].center_x,
        )
    }
}

/// Compute a proportional SVG layout for a sequence model.
pub fn layout(
    model: &Sequence,
    metrics: &ProportionalTextMetrics,
    font_family: &str,
) -> SvgSequenceLayout {
    if model.participants.is_empty() {
        return SvgSequenceLayout {
            title: None,
            participant_boxes: Vec::new(),
            participants: Vec::new(),
            lifelines: Vec::new(),
            destroy_markers: Vec::new(),
            rows: Vec::new(),
            blocks: Vec::new(),
            activations: Vec::new(),
            width: 0.0,
            height: 0.0,
            font_family: font_family.to_string(),
            font_size: metrics.font_size,
        };
    }

    let title = model.title.as_ref().map(|text| SvgTitle {
        text: text.clone(),
        y: DIAGRAM_PADDING + metrics.font_size / 2.0,
    });
    let title_offset = title.as_ref().map(|_| TITLE_VERTICAL_SPACE).unwrap_or(0.0);

    // 1. Measure participant boxes
    let box_sizes: Vec<(f64, f64)> = model
        .participants
        .iter()
        .map(|p| {
            metrics.measure_text_with_padding(
                &p.label,
                PARTICIPANT_PADDING_X,
                PARTICIPANT_PADDING_Y,
            )
        })
        .collect();

    let header_height = box_sizes.iter().map(|(_, h)| *h).fold(0.0_f64, f64::max);
    let participant_box_header_height =
        participant_box_header_height(model, metrics, model.participant_boxes.is_empty());
    let participant_box_y = DIAGRAM_PADDING + title_offset;
    let participant_y = participant_box_y + participant_box_header_height;

    // 2. Compute participant gap from message labels
    let participant_gap = compute_participant_gap(model, metrics);

    // 3. Compute left margin for left-of notes
    let left_margin = compute_left_note_margin(model, metrics, &box_sizes, participant_gap);

    // 4. Position participants horizontally
    let mut participants = Vec::with_capacity(model.participants.len());
    let mut x = DIAGRAM_PADDING + left_margin;

    for (i, p) in model.participants.iter().enumerate() {
        let (bw, _bh) = box_sizes[i];
        let center_x = x + bw / 2.0;

        participants.push(SvgParticipant {
            center_x,
            rect: SvgRect {
                x,
                y: participant_y,
                width: bw,
                height: header_height,
            },
            label: p.label.clone(),
            kind: p.kind.clone(),
        });

        if i < model.participants.len() - 1 {
            let next_bw = box_sizes[i + 1].0;
            x = center_x + participant_gap - next_bw / 2.0;
        }
    }

    // 5. Walk events and build rows
    let mut rows = Vec::new();
    let mut blocks = Vec::new();
    let mut cursor_y = participant_y + header_height + HEADER_MARGIN;

    let num_participants = model.participants.len();
    let mut activation_stacks: Vec<Vec<(f64, usize)>> = vec![Vec::new(); num_participants];
    let mut activation_depth: Vec<usize> = vec![0; num_participants];
    let mut activations: Vec<SvgActivation> = Vec::new();
    let mut open_blocks: Vec<OpenSvgBlock> = Vec::new();
    let mut pending_create: Option<usize> = None;
    let mut lifeline_end_by_participant: Vec<Option<f64>> = vec![None; num_participants];
    let mut destroy_markers: Vec<SvgDestroyMarker> = Vec::new();
    let mut last_message_y = cursor_y;

    for (ev_idx, event) in model.events.iter().enumerate() {
        match event {
            SequenceEvent::CreateParticipant { participant } => {
                pending_create = Some(*participant);
            }
            SequenceEvent::Message {
                from,
                to,
                line_style,
                arrow_head,
                text,
                number,
            } => {
                let created_participant = pending_create.take();
                if let Some(participant_idx) = created_participant {
                    let participant = &mut participants[participant_idx];
                    participant.rect.y = cursor_y - participant.rect.height / 2.0;
                    update_open_svg_block_extents(
                        &mut open_blocks,
                        participant.rect.x,
                        participant.rect.x + participant.rect.width,
                    );
                }

                let label = format_label(text, number);
                let from_cx = participants[*from].center_x;
                let to_cx = participants[*to].center_x;

                // Peek ahead: if the next events activate from/to participants,
                // include those in the depth calculation so the arrow terminates
                // at the activation box edge rather than the lifeline.
                let from_depth = activation_depth[*from]
                    + pending_activations(&model.events[ev_idx + 1..], *from);
                let to_depth =
                    activation_depth[*to] + pending_activations(&model.events[ev_idx + 1..], *to);

                last_message_y = cursor_y;

                if from == to {
                    // Self-message: start from activation edge if active
                    let self_x = activation_edge(
                        from_cx, from_depth, true, // right side
                    );
                    let label_x = self_x + SELF_MSG_ARM + 8.0;
                    let label_y = cursor_y + LABEL_ABOVE_GAP;

                    rows.push(SvgRow::SelfMessage(SvgSelfMessage {
                        x: self_x,
                        y: cursor_y,
                        arm_width: SELF_MSG_ARM,
                        height: SELF_MSG_HEIGHT,
                        label,
                        label_x,
                        label_y,
                        line_style: *line_style,
                        arrow_head: *arrow_head,
                    }));
                    let (left, right) = svg_row_extent(rows.last().unwrap(), metrics);
                    update_open_svg_block_extents(&mut open_blocks, left, right);

                    cursor_y += SELF_MSG_HEIGHT + EVENT_SPACING;
                } else {
                    // Adjust endpoints to activation box edges when active
                    let left_to_right = to_cx > from_cx;
                    let from_x = activation_edge(
                        from_cx,
                        from_depth,
                        left_to_right, // right edge if going right
                    );
                    let to_x =
                        message_target_x(&participants, *from, *to, created_participant, to_depth);

                    let mid_x = (from_x + to_x) / 2.0;
                    let label_y = cursor_y - LABEL_ABOVE_GAP;

                    rows.push(SvgRow::Message(SvgMessage {
                        from_x,
                        to_x,
                        y: cursor_y,
                        label,
                        label_x: mid_x,
                        label_y,
                        line_style: *line_style,
                        arrow_head: *arrow_head,
                    }));
                    let (left, right) = svg_row_extent(rows.last().unwrap(), metrics);
                    update_open_svg_block_extents(&mut open_blocks, left, right);

                    cursor_y += EVENT_SPACING;
                }

                if let Some(participant_idx) = created_participant {
                    cursor_y += participants[participant_idx].rect.height / 2.0;
                }
            }
            SequenceEvent::Note {
                placement,
                participants: indices,
                text,
            } => {
                let (tw, th) =
                    metrics.measure_text_with_padding(text, NOTE_PADDING_X, NOTE_PADDING_Y);

                let rect = match placement {
                    NotePlacement::LeftOf => {
                        let cx = participants[indices[0]].center_x;
                        SvgRect {
                            x: cx - NOTE_GAP - tw,
                            y: cursor_y,
                            width: tw,
                            height: th,
                        }
                    }
                    NotePlacement::RightOf => {
                        let cx = participants[indices[0]].center_x;
                        SvgRect {
                            x: cx + NOTE_GAP,
                            y: cursor_y,
                            width: tw,
                            height: th,
                        }
                    }
                    NotePlacement::Over if indices.len() == 2 => {
                        let cx1 = participants[indices[0]].center_x;
                        let cx2 = participants[indices[1]].center_x;
                        let left = cx1.min(cx2);
                        let right = cx1.max(cx2);
                        let span_width = tw.max(right - left + 20.0);
                        let mid = (left + right) / 2.0;
                        SvgRect {
                            x: mid - span_width / 2.0,
                            y: cursor_y,
                            width: span_width,
                            height: th,
                        }
                    }
                    NotePlacement::Over => {
                        let cx = participants[indices[0]].center_x;
                        SvgRect {
                            x: cx - tw / 2.0,
                            y: cursor_y,
                            width: tw,
                            height: th,
                        }
                    }
                };

                rows.push(SvgRow::Note(SvgNote {
                    rect: rect.clone(),
                    text: text.clone(),
                }));
                update_open_svg_block_extents(&mut open_blocks, rect.x, rect.x + rect.width);

                cursor_y += rect.height + EVENT_SPACING;
            }
            SequenceEvent::ActivateStart { participant } => {
                let depth = activation_depth[*participant];
                activation_stacks[*participant].push((last_message_y, depth));
                activation_depth[*participant] += 1;
            }
            SequenceEvent::ActivateEnd { participant } => {
                if let Some((y_start, depth)) = activation_stacks[*participant].pop() {
                    let y_end = last_message_y.max(y_start);
                    let cx = participants[*participant].center_x;
                    activations.push(SvgActivation {
                        x: cx - ACTIVATION_WIDTH / 2.0 + (depth as f64 * 3.0),
                        y_start,
                        y_end,
                        width: ACTIVATION_WIDTH,
                        depth,
                    });
                    activation_depth[*participant] =
                        activation_depth[*participant].saturating_sub(1);
                }
            }
            SequenceEvent::DestroyParticipant { participant } => {
                lifeline_end_by_participant[*participant] = Some(last_message_y);
                destroy_markers.push(SvgDestroyMarker {
                    x: participants[*participant].center_x,
                    y: last_message_y,
                });
                cursor_y += participants[*participant].rect.height / 2.0;
            }
            SequenceEvent::BlockStart { kind, label } => {
                open_blocks.push(OpenSvgBlock {
                    top_y: cursor_y,
                    depth: open_blocks.len(),
                    kind: *kind,
                    label: label.clone(),
                    dividers: Vec::new(),
                    min_x: None,
                    max_x: None,
                });
                cursor_y += BLOCK_ROW_SPACING;
            }
            SequenceEvent::BlockDivider { kind, label } => {
                if let Some(block) = open_blocks.last_mut() {
                    block.dividers.push(SvgBlockDivider {
                        y: cursor_y,
                        kind: *kind,
                        label: label.clone(),
                    });
                }
                cursor_y += BLOCK_ROW_SPACING;
            }
            SequenceEvent::BlockEnd => {
                if let Some(block) = open_blocks.pop() {
                    let finalized =
                        finalize_svg_block(block, cursor_y, participants.as_slice(), metrics);
                    update_open_svg_block_extents(
                        &mut open_blocks,
                        finalized.rect.x,
                        finalized.rect.x + finalized.rect.width,
                    );
                    blocks.push(finalized);
                }
                cursor_y += BLOCK_ROW_SPACING;
            }
        }
    }

    // Close unclosed activations
    for (participant_index, stack) in activation_stacks.iter_mut().enumerate() {
        while let Some((y_start, depth)) = stack.pop() {
            let y_end = (cursor_y - EVENT_SPACING / 2.0).max(y_start);
            let cx = participants[participant_index].center_x;
            activations.push(SvgActivation {
                x: cx - ACTIVATION_WIDTH / 2.0 + (depth as f64 * 3.0),
                y_start,
                y_end,
                width: ACTIVATION_WIDTH,
                depth,
            });
        }
    }

    // Sort by depth so outer activations render first (behind inner ones).
    activations.sort_by_key(|a| a.depth);
    blocks.sort_by_key(|block| block.depth);

    // 6. Compute lifelines and participant grouping boxes
    let lifeline_end = cursor_y
        + if model.participant_boxes.is_empty() {
            0.0
        } else {
            PARTICIPANT_GROUP_PADDING_Y
        };
    let mut lifelines: Vec<SvgLifeline> = participants
        .iter()
        .enumerate()
        .map(|(idx, p)| SvgLifeline {
            x: p.center_x,
            y_start: p.rect.y + p.rect.height,
            y_end: lifeline_end_by_participant[idx].unwrap_or(lifeline_end),
        })
        .collect();
    let mut participant_boxes = layout_participant_boxes(
        &model.participant_boxes,
        &participants,
        metrics,
        participant_box_y,
        lifeline_end + PARTICIPANT_GROUP_PADDING_Y * 0.25,
    );

    normalize_sequence_layout_left_edge(
        SvgLeftEdgeTargets {
            participant_boxes: &mut participant_boxes,
            participants: &mut participants,
            lifelines: &mut lifelines,
            destroy_markers: &mut destroy_markers,
            rows: &mut rows,
            blocks: &mut blocks,
            activations: &mut activations,
        },
        metrics,
    );

    // 7. Compute diagram bounds
    let max_participant_box_right = participant_boxes
        .iter()
        .map(|participant_box| participant_box.rect.x + participant_box.rect.width)
        .fold(0.0_f64, f64::max);
    let max_right = participants
        .iter()
        .map(|p| p.rect.x + p.rect.width)
        .fold(0.0_f64, f64::max);

    let row_right = rows
        .iter()
        .map(|row| svg_row_extent(row, metrics).1)
        .fold(0.0_f64, f64::max);

    let block_right = blocks
        .iter()
        .map(|block| block.rect.x + block.rect.width)
        .fold(0.0_f64, f64::max);
    let title_width = title
        .as_ref()
        .map(|title| metrics.measure_text_with_padding(&title.text, 0.0, 0.0).0)
        .unwrap_or(0.0);

    let width = max_right
        .max(max_participant_box_right)
        .max(row_right)
        .max(block_right)
        .max(title_width + DIAGRAM_PADDING)
        + DIAGRAM_PADDING;
    let participant_box_bottom = participant_boxes
        .iter()
        .map(|participant_box| participant_box.rect.y + participant_box.rect.height)
        .fold(0.0_f64, f64::max);
    let height = lifeline_end.max(participant_box_bottom) + DIAGRAM_PADDING;

    SvgSequenceLayout {
        title,
        participant_boxes,
        participants,
        lifelines,
        destroy_markers,
        rows,
        blocks,
        activations,
        width,
        height,
        font_family: font_family.to_string(),
        font_size: metrics.font_size,
    }
}

struct SvgLeftEdgeTargets<'a> {
    participant_boxes: &'a mut [SvgParticipantBox],
    participants: &'a mut [SvgParticipant],
    lifelines: &'a mut [SvgLifeline],
    destroy_markers: &'a mut [SvgDestroyMarker],
    rows: &'a mut [SvgRow],
    blocks: &'a mut [SvgBlock],
    activations: &'a mut [SvgActivation],
}

fn normalize_sequence_layout_left_edge(
    targets: SvgLeftEdgeTargets<'_>,
    metrics: &ProportionalTextMetrics,
) {
    let SvgLeftEdgeTargets {
        participant_boxes,
        participants,
        lifelines,
        destroy_markers,
        rows,
        blocks,
        activations,
    } = targets;

    let participant_box_left = participant_boxes
        .iter()
        .map(|participant_box| participant_box.rect.x)
        .fold(f64::INFINITY, f64::min);
    let participant_left = participants
        .iter()
        .map(|participant| participant.rect.x)
        .fold(f64::INFINITY, f64::min);
    let row_left = rows
        .iter()
        .map(|row| svg_row_extent(row, metrics).0)
        .fold(f64::INFINITY, f64::min);
    let block_left = blocks
        .iter()
        .map(|block| block.rect.x)
        .fold(f64::INFINITY, f64::min);
    let activation_left = activations
        .iter()
        .map(|activation| activation.x)
        .fold(f64::INFINITY, f64::min);
    let destroy_left = destroy_markers
        .iter()
        .map(|marker| marker.x - 6.0)
        .fold(f64::INFINITY, f64::min);

    let min_left = participant_box_left
        .min(participant_left)
        .min(row_left)
        .min(block_left)
        .min(activation_left)
        .min(destroy_left);

    if !min_left.is_finite() || min_left >= DIAGRAM_PADDING {
        return;
    }

    let shift_x = DIAGRAM_PADDING - min_left;

    for participant_box in participant_boxes {
        participant_box.rect.x += shift_x;
    }

    for participant in participants {
        participant.center_x += shift_x;
        participant.rect.x += shift_x;
    }

    for lifeline in lifelines {
        lifeline.x += shift_x;
    }

    for marker in destroy_markers {
        marker.x += shift_x;
    }

    for row in rows {
        match row {
            SvgRow::Message(message) => {
                message.from_x += shift_x;
                message.to_x += shift_x;
                message.label_x += shift_x;
            }
            SvgRow::SelfMessage(message) => {
                message.x += shift_x;
                message.label_x += shift_x;
            }
            SvgRow::Note(note) => {
                note.rect.x += shift_x;
            }
        }
    }

    for block in blocks {
        block.rect.x += shift_x;
    }

    for activation in activations {
        activation.x += shift_x;
    }
}

#[derive(Debug)]
struct OpenSvgBlock {
    top_y: f64,
    depth: usize,
    kind: BlockKind,
    label: String,
    dividers: Vec<SvgBlockDivider>,
    min_x: Option<f64>,
    max_x: Option<f64>,
}

fn update_open_svg_block_extents(open_blocks: &mut [OpenSvgBlock], left: f64, right: f64) {
    for block in open_blocks {
        block.min_x = Some(block.min_x.map_or(left, |current| current.min(left)));
        block.max_x = Some(block.max_x.map_or(right, |current| current.max(right)));
    }
}

fn finalize_svg_block(
    block: OpenSvgBlock,
    bottom_y: f64,
    participants: &[SvgParticipant],
    metrics: &ProportionalTextMetrics,
) -> SvgBlock {
    let fallback_center = participants.first().map(|p| p.center_x).unwrap_or(20.0);
    let raw_left = block.min_x.unwrap_or(fallback_center - 12.0);
    let raw_right = block.max_x.unwrap_or(fallback_center + 12.0);
    let inset = block.depth as f64 * 8.0;

    let left_x = raw_left - BLOCK_SIDE_PADDING + inset;
    let mut right_x = raw_right + BLOCK_SIDE_PADDING - inset;

    let operator_width = block_tab_width(block.kind.keyword());
    let header_guard_width = format_fragment_guard(&block.label)
        .map(|guard| metrics.measure_text_with_padding(&guard, 0.0, 0.0).0)
        .unwrap_or(0.0);

    let min_width = block
        .dividers
        .iter()
        .map(|divider| {
            format_fragment_guard(&divider.label)
                .map(|guard| {
                    metrics
                        .measure_text_with_padding(&guard, BLOCK_SIDE_PADDING, 0.0)
                        .0
                })
                .unwrap_or(0.0)
        })
        .fold(
            operator_width + header_guard_width + BLOCK_HEADER_GAP + BLOCK_SIDE_PADDING,
            f64::max,
        )
        .max(operator_width + BLOCK_SIDE_PADDING * 2.0)
        .max(64.0);

    if right_x < left_x + min_width {
        right_x = left_x + min_width;
    }

    SvgBlock {
        rect: SvgRect {
            x: left_x,
            y: block.top_y,
            width: right_x - left_x,
            height: (bottom_y - block.top_y).max(BLOCK_ROW_SPACING),
        },
        depth: block.depth,
        kind: block.kind,
        label: block.label,
        dividers: block.dividers,
    }
}

fn svg_row_extent(row: &SvgRow, metrics: &ProportionalTextMetrics) -> (f64, f64) {
    match row {
        SvgRow::Message(msg) => {
            let (lw, _) = metrics.measure_text_with_padding(&msg.label, 0.0, 0.0);
            let label_left = msg.label_x - lw / 2.0;
            let label_right = msg.label_x + lw / 2.0;
            (
                msg.from_x.min(msg.to_x).min(label_left),
                msg.from_x.max(msg.to_x).max(label_right),
            )
        }
        SvgRow::SelfMessage(sm) => {
            let (lw, _) = metrics.measure_text_with_padding(&sm.label, 0.0, 0.0);
            (sm.x, (sm.x + sm.arm_width + 8.0 + lw).max(sm.label_x + lw))
        }
        SvgRow::Note(note) => (note.rect.x, note.rect.x + note.rect.width),
    }
}

/// Compute minimum gap between participant centers based on message labels.
fn compute_participant_gap(model: &Sequence, metrics: &ProportionalTextMetrics) -> f64 {
    let max_label_width = model
        .events
        .iter()
        .filter_map(|e| match e {
            SequenceEvent::Message {
                from,
                to,
                text,
                number,
                ..
            } if from != to => {
                let label = format_label(text, number);
                let (w, _) = metrics.measure_text_with_padding(&label, 0.0, 0.0);
                Some(w)
            }
            _ => None,
        })
        .fold(0.0_f64, f64::max);

    (max_label_width + 40.0).max(MIN_PARTICIPANT_GAP)
}

/// Compute extra left margin needed for left-of notes on the first participant.
fn compute_left_note_margin(
    model: &Sequence,
    metrics: &ProportionalTextMetrics,
    box_sizes: &[(f64, f64)],
    participant_gap: f64,
) -> f64 {
    if model.participants.is_empty() {
        return 0.0;
    }

    // Tentative center_x for each participant
    let mut centers = Vec::with_capacity(model.participants.len());
    let mut x = 0.0_f64;
    for (i, (bw, _)) in box_sizes.iter().enumerate() {
        centers.push(x + bw / 2.0);
        if i < box_sizes.len() - 1 {
            let next_bw = box_sizes[i + 1].0;
            x = centers[i] + participant_gap - next_bw / 2.0;
        }
    }

    let mut max_overhang = 0.0_f64;
    for event in &model.events {
        if let SequenceEvent::Note {
            placement: NotePlacement::LeftOf,
            participants: indices,
            text,
        } = event
        {
            let (tw, _) = metrics.measure_text_with_padding(text, NOTE_PADDING_X, NOTE_PADDING_Y);
            let center_x = centers[indices[0]];
            let needed = tw + NOTE_GAP;
            if needed > center_x {
                max_overhang = max_overhang.max(needed - center_x);
            }
        }
    }
    max_overhang
}

fn participant_box_header_height(
    model: &Sequence,
    metrics: &ProportionalTextMetrics,
    no_boxes: bool,
) -> f64 {
    if no_boxes {
        return 0.0;
    }

    let max_label_height = model
        .participant_boxes
        .iter()
        .filter_map(|participant_box| participant_box.label.as_deref())
        .map(|label| metrics.measure_text_with_padding(label, 0.0, 0.0).1)
        .fold(0.0_f64, f64::max);

    (max_label_height + PARTICIPANT_GROUP_PADDING_Y).max(PARTICIPANT_GROUP_PADDING_Y * 2.0)
}

fn layout_participant_boxes(
    participant_boxes: &[ParticipantBox],
    participants: &[SvgParticipant],
    metrics: &ProportionalTextMetrics,
    top_y: f64,
    bottom_y: f64,
) -> Vec<SvgParticipantBox> {
    participant_boxes
        .iter()
        .filter_map(|participant_box| {
            let first_idx = *participant_box.participants.first()?;
            let last_idx = *participant_box.participants.last()?;
            let first = participants.get(first_idx)?;
            let last = participants.get(last_idx)?;

            let mut left_x = first.rect.x - PARTICIPANT_GROUP_PADDING_X;
            let mut right_x = last.rect.x + last.rect.width + PARTICIPANT_GROUP_PADDING_X;

            if let Some(label) = participant_box.label.as_deref() {
                let (label_width, _) =
                    metrics.measure_text_with_padding(label, PARTICIPANT_GROUP_PADDING_X, 0.0);
                let current_width = right_x - left_x;
                if current_width < label_width {
                    let missing = label_width - current_width;
                    left_x -= missing / 2.0;
                    right_x += missing / 2.0;
                }
            }

            Some(SvgParticipantBox {
                rect: SvgRect {
                    x: left_x,
                    y: top_y,
                    width: right_x - left_x,
                    height: (bottom_y - top_y).max(PARTICIPANT_GROUP_PADDING_Y * 2.0),
                },
                label: participant_box.label.clone(),
                color: participant_box.color.clone(),
            })
        })
        .collect()
}

/// Format a message label with optional autonumber prefix.
fn format_label(text: &str, number: &Option<usize>) -> String {
    match number {
        Some(n) => {
            if text.is_empty() {
                format!("{n}.")
            } else {
                format!("{n}. {text}")
            }
        }
        None => text.to_string(),
    }
}

fn format_fragment_guard(label: &str) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(format!("[{trimmed}]"))
    }
}

fn block_tab_width(keyword: &str) -> f64 {
    ((keyword.len() as f64) * BLOCK_TAB_CHAR_WIDTH + BLOCK_TAB_PADDING_X * 2.0)
        .max(BLOCK_TAB_MIN_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::measure::ProportionalTextMetrics;
    use crate::timeline::sequence::model::{
        AutonumberState, Participant, ParticipantBox, ParticipantKind, Sequence,
    };

    fn test_metrics() -> ProportionalTextMetrics {
        ProportionalTextMetrics::new(16.0, 15.0, 15.0)
    }

    #[test]
    fn layout_empty_model() {
        let model = Sequence {
            title: None,
            participants: Vec::new(),
            participant_boxes: Vec::new(),
            events: Vec::new(),
            autonumber: AutonumberState::default(),
        };
        let layout = layout(&model, &test_metrics(), "sans-serif");
        assert_eq!(layout.width, 0.0);
        assert_eq!(layout.height, 0.0);
        assert!(layout.participants.is_empty());
    }

    #[test]
    fn layout_reserves_space_for_title() {
        let model = Sequence {
            title: Some("Authentication Flow".into()),
            participants: vec![Participant {
                id: "Alice".into(),
                label: "Alice".into(),
                kind: ParticipantKind::Participant,
            }],
            participant_boxes: Vec::new(),
            events: Vec::new(),
            autonumber: AutonumberState::default(),
        };

        let layout = layout(&model, &test_metrics(), "sans-serif");

        let title = layout.title.expect("title should be present");
        assert!(title.y >= DIAGRAM_PADDING);
        assert!(layout.participants[0].rect.y > DIAGRAM_PADDING);
    }

    #[test]
    fn layout_two_participants_one_message() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "Alice".into(),
                    label: "Alice".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "Bob".into(),
                    label: "Bob".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: Vec::new(),
            events: vec![SequenceEvent::Message {
                from: 0,
                to: 1,
                line_style: LineStyle::Solid,
                arrow_head: ArrowHead::Filled,
                text: "Hello".into(),
                number: None,
            }],
            autonumber: AutonumberState::default(),
        };
        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert_eq!(layout.participants.len(), 2);
        assert_eq!(layout.lifelines.len(), 2);
        assert_eq!(layout.rows.len(), 1);
        assert!(layout.width > 0.0);
        assert!(layout.height > 0.0);

        // First participant should be left of second
        assert!(layout.participants[0].center_x < layout.participants[1].center_x);
    }

    #[test]
    fn self_message_produces_self_message_row() {
        let model = Sequence {
            title: None,
            participants: vec![Participant {
                id: "A".into(),
                label: "A".into(),
                kind: ParticipantKind::Participant,
            }],
            participant_boxes: Vec::new(),
            events: vec![SequenceEvent::Message {
                from: 0,
                to: 0,
                line_style: LineStyle::Solid,
                arrow_head: ArrowHead::Filled,
                text: "self".into(),
                number: None,
            }],
            autonumber: AutonumberState::default(),
        };
        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert_eq!(layout.rows.len(), 1);
        assert!(matches!(layout.rows[0], SvgRow::SelfMessage(_)));
    }

    #[test]
    fn layout_tracks_svg_blocks() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "A".into(),
                    label: "A".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "B".into(),
                    label: "B".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: Vec::new(),
            events: vec![
                SequenceEvent::BlockStart {
                    kind: BlockKind::Alt,
                    label: "available".into(),
                },
                SequenceEvent::Message {
                    from: 0,
                    to: 1,
                    line_style: LineStyle::Solid,
                    arrow_head: ArrowHead::Filled,
                    text: "Request".into(),
                    number: None,
                },
                SequenceEvent::BlockDivider {
                    kind: BlockDividerKind::Else,
                    label: "busy".into(),
                },
                SequenceEvent::Message {
                    from: 1,
                    to: 0,
                    line_style: LineStyle::Dashed,
                    arrow_head: ArrowHead::None,
                    text: "Retry later".into(),
                    number: None,
                },
                SequenceEvent::BlockEnd,
            ],
            autonumber: AutonumberState::default(),
        };
        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert_eq!(layout.blocks.len(), 1);
        assert_eq!(layout.blocks[0].dividers.len(), 1);
        assert!(layout.blocks[0].rect.width > 0.0);
        assert!(layout.blocks[0].rect.height > 0.0);
        assert!(layout.blocks[0].rect.x >= DIAGRAM_PADDING);
    }

    #[test]
    fn layout_positions_participant_boxes_behind_headers() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "Alice".into(),
                    label: "Alice".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "Bob".into(),
                    label: "Bob".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: vec![ParticipantBox {
                label: Some("Frontend".into()),
                color: Some("lightblue".into()),
                participants: vec![0, 1],
            }],
            events: vec![SequenceEvent::Message {
                from: 0,
                to: 1,
                line_style: LineStyle::Solid,
                arrow_head: ArrowHead::Filled,
                text: "Hello".into(),
                number: None,
            }],
            autonumber: AutonumberState::default(),
        };

        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert_eq!(layout.participant_boxes.len(), 1);
        assert_eq!(
            layout.participant_boxes[0].label.as_deref(),
            Some("Frontend")
        );
        assert!(layout.participant_boxes[0].rect.x <= layout.participants[0].rect.x);
        assert!(layout.participants[0].rect.y > DIAGRAM_PADDING);
    }

    #[test]
    fn layout_moves_created_participant_header_to_creation_message() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "Alice".into(),
                    label: "Alice".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "Bob".into(),
                    label: "Bob".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: Vec::new(),
            events: vec![
                SequenceEvent::CreateParticipant { participant: 1 },
                SequenceEvent::Message {
                    from: 0,
                    to: 1,
                    line_style: LineStyle::Solid,
                    arrow_head: ArrowHead::Filled,
                    text: "Create".into(),
                    number: None,
                },
            ],
            autonumber: AutonumberState::default(),
        };

        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert!(layout.participants[1].rect.y > layout.participants[0].rect.y);
        assert!(layout.lifelines[1].y_start > layout.participants[1].rect.y);
        match &layout.rows[0] {
            SvgRow::Message(message) => {
                assert_eq!(message.to_x, layout.participants[1].rect.x);
                assert!(message.from_x < message.to_x);
            }
            _ => panic!("expected create message row"),
        }
    }

    #[test]
    fn layout_adds_destroy_marker_and_truncates_lifeline() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "Alice".into(),
                    label: "Alice".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "Bob".into(),
                    label: "Bob".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: Vec::new(),
            events: vec![
                SequenceEvent::Message {
                    from: 0,
                    to: 1,
                    line_style: LineStyle::Solid,
                    arrow_head: ArrowHead::Filled,
                    text: "Bye".into(),
                    number: None,
                },
                SequenceEvent::DestroyParticipant { participant: 1 },
            ],
            autonumber: AutonumberState::default(),
        };

        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert_eq!(layout.destroy_markers.len(), 1);
        assert_eq!(layout.lifelines[1].y_end, layout.destroy_markers[0].y);
    }

    #[test]
    fn layout_normalizes_negative_block_bounds_into_viewbox() {
        let model = Sequence {
            title: None,
            participants: vec![
                Participant {
                    id: "Alice".into(),
                    label: "Alice".into(),
                    kind: ParticipantKind::Participant,
                },
                Participant {
                    id: "Bob".into(),
                    label: "Bob".into(),
                    kind: ParticipantKind::Participant,
                },
            ],
            participant_boxes: Vec::new(),
            events: vec![
                SequenceEvent::BlockStart {
                    kind: BlockKind::Loop,
                    label: "Retry until ready".into(),
                },
                SequenceEvent::Message {
                    from: 0,
                    to: 1,
                    line_style: LineStyle::Solid,
                    arrow_head: ArrowHead::Filled,
                    text: "Check".into(),
                    number: None,
                },
                SequenceEvent::BlockStart {
                    kind: BlockKind::Alt,
                    label: "ready".into(),
                },
                SequenceEvent::Message {
                    from: 1,
                    to: 0,
                    line_style: LineStyle::Solid,
                    arrow_head: ArrowHead::Filled,
                    text: "Proceed".into(),
                    number: None,
                },
                SequenceEvent::BlockDivider {
                    kind: BlockDividerKind::Else,
                    label: "retry".into(),
                },
                SequenceEvent::Message {
                    from: 1,
                    to: 0,
                    line_style: LineStyle::Dashed,
                    arrow_head: ArrowHead::Filled,
                    text: "Wait".into(),
                    number: None,
                },
                SequenceEvent::BlockEnd,
                SequenceEvent::BlockEnd,
            ],
            autonumber: AutonumberState::default(),
        };

        let layout = layout(&model, &test_metrics(), "sans-serif");

        assert!(
            layout
                .blocks
                .iter()
                .all(|block| block.rect.x >= DIAGRAM_PADDING)
        );
        assert!(
            layout
                .participants
                .iter()
                .all(|participant| participant.rect.x >= DIAGRAM_PADDING)
        );
    }
}
