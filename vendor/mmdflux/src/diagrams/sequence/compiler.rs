//! Sequence diagram compiler.
//!
//! Compiles raw parsed AST statements into a validated `Sequence`.
//! Resolves participant references, assigns stable ordering, and
//! applies autonumbering.

use std::collections::{HashMap, HashSet};

use crate::mermaid::sequence::ast::{ActivationModifier, AutonumberMode, SequenceStatement};
use crate::timeline::sequence::model::{
    AutonumberState, BlockDividerKind, BlockKind, NotePlacement, Participant, ParticipantBox,
    ParticipantKind, Sequence, SequenceEvent,
};

/// Compile parsed sequence statements into a validated model.
///
/// Participants are ordered by first appearance (explicit declarations first,
/// then implicit from message endpoints). Unknown participant references in
/// notes produce an error.
pub fn compile(
    statements: &[SequenceStatement],
) -> Result<Sequence, Box<dyn std::error::Error + Send + Sync>> {
    let mut participants: Vec<Participant> = Vec::new();
    let mut participant_boxes: Vec<ParticipantBox> = Vec::new();
    let mut participant_index: HashMap<String, usize> = HashMap::new();
    let mut participant_box_index: HashMap<String, usize> = HashMap::new();
    let mut created_participants: HashSet<usize> = HashSet::new();
    let mut events: Vec<SequenceEvent> = Vec::new();
    let mut block_stack: Vec<BlockKind> = Vec::new();
    let mut open_participant_box: Option<OpenParticipantBox> = None;
    let mut title: Option<String> = None;

    // First pass: collect explicit participant declarations (preserving order)
    for stmt in statements {
        match stmt {
            SequenceStatement::Participant { kind, id, alias } => {
                let idx = register_participant(
                    &mut participants,
                    &mut participant_index,
                    kind,
                    id,
                    alias.as_deref(),
                    false,
                )?;

                created_participants.remove(&idx);
                handle_open_participant_box(
                    &participant_boxes,
                    &participant_box_index,
                    &participants,
                    open_participant_box.as_mut(),
                    id,
                    idx,
                )?;
            }
            SequenceStatement::CreateParticipant { kind, id, alias } => {
                let idx = register_participant(
                    &mut participants,
                    &mut participant_index,
                    kind,
                    id,
                    alias.as_deref(),
                    true,
                )?;
                created_participants.insert(idx);

                handle_open_participant_box(
                    &participant_boxes,
                    &participant_box_index,
                    &participants,
                    open_participant_box.as_mut(),
                    id,
                    idx,
                )?;
            }
            SequenceStatement::ParticipantBoxStart { color, label } => {
                if open_participant_box.is_some() {
                    return Err("nested participant boxes are not supported".into());
                }
                open_participant_box = Some(OpenParticipantBox {
                    color: color.clone(),
                    label: label.clone(),
                    participants: Vec::new(),
                });
            }
            SequenceStatement::ParticipantBoxEnd => {
                let open_box = open_participant_box.take().ok_or_else(
                    || -> Box<dyn std::error::Error + Send + Sync> {
                        "encountered participant box `end` without an open box".into()
                    },
                )?;
                if open_box.participants.is_empty() {
                    return Err("participant boxes must declare at least one participant".into());
                }
                let box_idx = participant_boxes.len();
                for participant_idx in &open_box.participants {
                    let participant_id = participants[*participant_idx].id.clone();
                    participant_box_index.insert(participant_id, box_idx);
                }
                participant_boxes.push(ParticipantBox {
                    label: open_box.label,
                    color: open_box.color,
                    participants: open_box.participants,
                });
            }
            SequenceStatement::Title(next_title) => {
                title = Some(next_title.clone());
            }
            _ => {}
        }
    }

    if open_participant_box.is_some() {
        return Err("unclosed participant box".into());
    }

    // Second pass: process messages and notes, creating implicit participants as needed
    let mut autonumber = AutonumberState::default();
    let mut available_participants: HashSet<usize> = (0..participants.len())
        .filter(|idx| !created_participants.contains(idx))
        .collect();
    let mut destroyed_participants: HashSet<usize> = HashSet::new();
    let mut pending_create: Option<usize> = None;
    let mut pending_destroy: Option<usize> = None;
    for stmt in statements {
        match stmt {
            SequenceStatement::CreateParticipant { id, .. } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "create participant",
                )?;
                let idx = participant_index.get(id.as_str()).copied().ok_or_else(
                    || -> Box<dyn std::error::Error + Send + Sync> {
                        format!("Created participant not found: {id}").into()
                    },
                )?;
                pending_create = Some(idx);
                events.push(SequenceEvent::CreateParticipant { participant: idx });
            }
            SequenceStatement::Message {
                from,
                to,
                line_style,
                arrow_head,
                text,
                activate,
            } => {
                let from_idx = ensure_participant(
                    &mut participants,
                    &mut participant_index,
                    &mut available_participants,
                    from,
                );
                let to_idx = ensure_participant(
                    &mut participants,
                    &mut participant_index,
                    &mut available_participants,
                    to,
                );
                validate_message_endpoint(
                    &participants,
                    &available_participants,
                    &destroyed_participants,
                    pending_create,
                    from_idx,
                    EndpointRole::Sender,
                )?;
                validate_message_endpoint(
                    &participants,
                    &available_participants,
                    &destroyed_participants,
                    pending_create,
                    to_idx,
                    EndpointRole::Receiver,
                )?;
                if let Some(created_idx) = pending_create
                    && to_idx != created_idx
                {
                    return Err(format!(
                        "the created participant `{}` does not have an associated creating message after its declaration",
                        participants[created_idx].id
                    )
                    .into());
                }
                if let Some(destroyed_idx) = pending_destroy
                    && from_idx != destroyed_idx
                    && to_idx != destroyed_idx
                {
                    return Err(format!(
                        "the destroyed participant `{}` does not have an associated destroying message after its declaration",
                        participants[destroyed_idx].id
                    )
                    .into());
                }

                events.push(SequenceEvent::Message {
                    from: from_idx,
                    to: to_idx,
                    line_style: *line_style,
                    arrow_head: *arrow_head,
                    text: text.clone(),
                    number: next_message_number(&mut autonumber),
                });

                // Emit activation events from +/- shorthand.
                // + activates the target; - deactivates the source.
                match activate {
                    Some(ActivationModifier::Activate) => {
                        events.push(SequenceEvent::ActivateStart {
                            participant: to_idx,
                        });
                    }
                    Some(ActivationModifier::Deactivate) => {
                        events.push(SequenceEvent::ActivateEnd {
                            participant: from_idx,
                        });
                    }
                    None => {}
                }

                if let Some(created_idx) = pending_create.take() {
                    available_participants.insert(created_idx);
                }
                if let Some(destroyed_idx) = pending_destroy.take() {
                    events.push(SequenceEvent::DestroyParticipant {
                        participant: destroyed_idx,
                    });
                    available_participants.remove(&destroyed_idx);
                    destroyed_participants.insert(destroyed_idx);
                }
            }
            SequenceStatement::Activate { participant: id } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "activate",
                )?;
                let idx = ensure_participant(
                    &mut participants,
                    &mut participant_index,
                    &mut available_participants,
                    id,
                );
                validate_participant_state(
                    &participants,
                    &available_participants,
                    &destroyed_participants,
                    idx,
                )?;
                events.push(SequenceEvent::ActivateStart { participant: idx });
            }
            SequenceStatement::Deactivate { participant: id } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "deactivate",
                )?;
                let idx = ensure_participant(
                    &mut participants,
                    &mut participant_index,
                    &mut available_participants,
                    id,
                );
                validate_participant_state(
                    &participants,
                    &available_participants,
                    &destroyed_participants,
                    idx,
                )?;
                events.push(SequenceEvent::ActivateEnd { participant: idx });
            }
            SequenceStatement::DestroyParticipant { participant: id } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "destroy participant",
                )?;
                let idx = participant_index.get(id.as_str()).copied().ok_or_else(
                    || -> Box<dyn std::error::Error + Send + Sync> {
                        format!("Destroy references unknown participant: {id}").into()
                    },
                )?;
                validate_participant_state(
                    &participants,
                    &available_participants,
                    &destroyed_participants,
                    idx,
                )?;
                pending_destroy = Some(idx);
            }
            SequenceStatement::Note {
                placement,
                participants: names,
                text,
            } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "note",
                )?;
                // Validate participant count for each placement
                match placement {
                    NotePlacement::LeftOf | NotePlacement::RightOf => {
                        if names.len() != 1 {
                            return Err(format!(
                                "Note left/right of requires exactly 1 participant, got {}",
                                names.len()
                            )
                            .into());
                        }
                    }
                    NotePlacement::Over => {
                        if names.len() > 2 {
                            return Err(format!(
                                "Note over supports at most 2 participants, got {}",
                                names.len()
                            )
                            .into());
                        }
                    }
                }

                let mut indices = Vec::with_capacity(names.len());
                for name in names {
                    let idx = participant_index.get(name.as_str()).copied().ok_or_else(
                        || -> Box<dyn std::error::Error + Send + Sync> {
                            format!("Note references unknown participant: {name}").into()
                        },
                    )?;
                    validate_participant_state(
                        &participants,
                        &available_participants,
                        &destroyed_participants,
                        idx,
                    )?;
                    indices.push(idx);
                }

                events.push(SequenceEvent::Note {
                    placement: *placement,
                    participants: indices,
                    text: text.clone(),
                });
            }
            SequenceStatement::Participant { .. }
            | SequenceStatement::ParticipantBoxStart { .. }
            | SequenceStatement::ParticipantBoxEnd
            | SequenceStatement::Title(_) => {
                // Already handled in first pass
            }
            SequenceStatement::Autonumber(mode) => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    "autonumber",
                )?;
                apply_autonumber_mode(&mut autonumber, *mode);
            }
            SequenceStatement::BlockStart { kind, label } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    kind.keyword(),
                )?;
                block_stack.push(*kind);
                events.push(SequenceEvent::BlockStart {
                    kind: *kind,
                    label: label.clone(),
                });
            }
            SequenceStatement::BlockDivider { kind, label } => {
                ensure_no_pending_lifecycle(
                    &participants,
                    pending_create,
                    pending_destroy,
                    kind.keyword(),
                )?;
                validate_block_divider(&block_stack, *kind)?;
                events.push(SequenceEvent::BlockDivider {
                    kind: *kind,
                    label: label.clone(),
                });
            }
            SequenceStatement::BlockEnd => {
                ensure_no_pending_lifecycle(&participants, pending_create, pending_destroy, "end")?;
                block_stack
                    .pop()
                    .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                        "encountered `end` without an open block".into()
                    })?;
                events.push(SequenceEvent::BlockEnd);
            }
        }
    }

    if !block_stack.is_empty() {
        let unclosed = block_stack
            .iter()
            .map(|kind| kind.keyword())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("unclosed sequence block(s): {unclosed}").into());
    }

    ensure_no_pending_lifecycle(
        &participants,
        pending_create,
        pending_destroy,
        "end of diagram",
    )?;

    Ok(Sequence {
        title,
        participants,
        participant_boxes,
        events,
        autonumber,
    })
}

#[derive(Debug)]
struct OpenParticipantBox {
    color: Option<String>,
    label: Option<String>,
    participants: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointRole {
    Sender,
    Receiver,
}

fn register_participant(
    participants: &mut Vec<Participant>,
    participant_index: &mut HashMap<String, usize>,
    kind: &ParticipantKind,
    id: &str,
    alias: Option<&str>,
    error_on_existing: bool,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(&existing) = participant_index.get(id) {
        if error_on_existing {
            return Err(format!(
                "participant `{id}` is already declared and cannot be recreated; use aliases to simulate recreation"
            )
            .into());
        }
        return Ok(existing);
    }

    let next = participants.len();
    participants.push(Participant {
        id: id.to_string(),
        label: alias.unwrap_or(id).to_string(),
        kind: kind.clone(),
    });
    participant_index.insert(id.to_string(), next);
    Ok(next)
}

fn handle_open_participant_box(
    participant_boxes: &[ParticipantBox],
    participant_box_index: &HashMap<String, usize>,
    participants: &[Participant],
    open_box: Option<&mut OpenParticipantBox>,
    participant_id: &str,
    participant_idx: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(open_box) = open_box else {
        return Ok(());
    };

    if let Some(existing_box_idx) = participant_box_index.get(participant_id) {
        let existing = &participant_boxes[*existing_box_idx];
        let existing_label = existing.label.as_deref().unwrap_or("unnamed box");
        return Err(format!(
            "participant `{participant_id}` cannot belong to multiple participant boxes (already assigned to `{existing_label}`)"
        )
        .into());
    }

    if participants.get(participant_idx).is_none() {
        return Err(format!("participant `{participant_id}` was not registered").into());
    }

    if !open_box.participants.contains(&participant_idx) {
        open_box.participants.push(participant_idx);
    }

    Ok(())
}

fn ensure_no_pending_lifecycle(
    participants: &[Participant],
    pending_create: Option<usize>,
    pending_destroy: Option<usize>,
    context: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(idx) = pending_create {
        return Err(format!(
            "the created participant `{}` does not have an associated creating message after its declaration before `{context}`",
            participants[idx].id
        )
        .into());
    }
    if let Some(idx) = pending_destroy {
        return Err(format!(
            "the destroyed participant `{}` does not have an associated destroying message after its declaration before `{context}`",
            participants[idx].id
        )
        .into());
    }
    Ok(())
}

fn validate_participant_state(
    participants: &[Participant],
    available_participants: &HashSet<usize>,
    destroyed_participants: &HashSet<usize>,
    participant_idx: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if destroyed_participants.contains(&participant_idx) {
        return Err(format!(
            "participant `{}` cannot be referenced after it is destroyed",
            participants[participant_idx].id
        )
        .into());
    }
    if !available_participants.contains(&participant_idx) {
        return Err(format!(
            "participant `{}` cannot be referenced before it is created",
            participants[participant_idx].id
        )
        .into());
    }
    Ok(())
}

fn validate_message_endpoint(
    participants: &[Participant],
    available_participants: &HashSet<usize>,
    destroyed_participants: &HashSet<usize>,
    pending_create: Option<usize>,
    participant_idx: usize,
    role: EndpointRole,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if destroyed_participants.contains(&participant_idx) {
        return Err(format!(
            "participant `{}` cannot be referenced after it is destroyed",
            participants[participant_idx].id
        )
        .into());
    }

    if available_participants.contains(&participant_idx) {
        return Ok(());
    }

    if pending_create == Some(participant_idx) && role == EndpointRole::Receiver {
        return Ok(());
    }

    Err(format!(
        "participant `{}` cannot be referenced before it is created",
        participants[participant_idx].id
    )
    .into())
}

fn validate_block_divider(
    block_stack: &[BlockKind],
    divider: BlockDividerKind,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(current) = block_stack.last() else {
        return Err(format!("encountered `{}` without an open block", divider.keyword()).into());
    };

    match (current, divider) {
        (BlockKind::Alt, BlockDividerKind::Else)
        | (BlockKind::Par, BlockDividerKind::And)
        | (BlockKind::Critical, BlockDividerKind::Option) => Ok(()),
        _ => Err(format!(
            "`{}` is not valid inside `{}` blocks",
            divider.keyword(),
            current.keyword()
        )
        .into()),
    }
}

/// Ensure a participant exists, creating an implicit one if needed.
/// Returns the participant index.
fn ensure_participant(
    participants: &mut Vec<Participant>,
    index: &mut HashMap<String, usize>,
    available_participants: &mut HashSet<usize>,
    id: &str,
) -> usize {
    if let Some(&idx) = index.get(id) {
        return idx;
    }
    let idx = participants.len();
    participants.push(Participant {
        id: id.to_string(),
        label: id.to_string(),
        kind: ParticipantKind::Participant,
    });
    index.insert(id.to_string(), idx);
    available_participants.insert(idx);
    idx
}

fn apply_autonumber_mode(state: &mut AutonumberState, mode: AutonumberMode) {
    match mode {
        AutonumberMode::On { start, step } => {
            if let Some(start) = start {
                state.next = start;
            }
            state.step = step.unwrap_or(1);
            state.enabled = true;
        }
        AutonumberMode::Off => {
            state.enabled = false;
        }
    }
}

fn next_message_number(state: &mut AutonumberState) -> Option<usize> {
    if !state.enabled {
        return None;
    }

    let number = state.next;
    state.next = state.next.saturating_add(state.step.max(1));
    Some(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid::sequence::parse_sequence;
    use crate::timeline::sequence::model::{ArrowHead, LineStyle};

    fn compile_input(input: &str) -> Sequence {
        let result = parse_sequence(input).unwrap();
        compile(&result.statements).unwrap()
    }

    #[test]
    fn compile_empty_diagram() {
        let model = compile_input("sequenceDiagram\n");
        assert!(model.participants.is_empty());
        assert!(model.events.is_empty());
        assert_eq!(model.autonumber, AutonumberState::default());
    }

    #[test]
    fn compile_participants_in_order() {
        let model = compile_input("sequenceDiagram\nparticipant B\nparticipant A");
        assert_eq!(model.participants.len(), 2);
        assert_eq!(model.participants[0].id, "B");
        assert_eq!(model.participants[1].id, "A");
    }

    #[test]
    fn compile_participant_alias() {
        let model = compile_input("sequenceDiagram\nparticipant A as Alice");
        assert_eq!(model.participants[0].id, "A");
        assert_eq!(model.participants[0].label, "Alice");
    }

    #[test]
    fn compile_actor_kind() {
        let model = compile_input("sequenceDiagram\nactor B as Bob");
        assert_eq!(model.participants[0].kind, ParticipantKind::Actor);
    }

    #[test]
    fn compile_participant_boxes_track_membership() {
        let model = compile_input(
            "\
sequenceDiagram
    box blue Frontend
        participant A as Alice
        actor B as Bob
    end
    participant C as Charlie",
        );

        assert_eq!(model.participant_boxes.len(), 1);
        let participant_box = &model.participant_boxes[0];
        assert_eq!(participant_box.color.as_deref(), Some("blue"));
        assert_eq!(participant_box.label.as_deref(), Some("Frontend"));
        assert_eq!(participant_box.participants, vec![0, 1]);
    }

    #[test]
    fn compile_participant_box_without_label_preserves_color() {
        let model = compile_input(
            "\
sequenceDiagram
    box aqua
        participant A
        participant B
    end",
        );

        assert_eq!(model.participant_boxes.len(), 1);
        assert_eq!(model.participant_boxes[0].color.as_deref(), Some("aqua"));
        assert_eq!(model.participant_boxes[0].label, None);
    }

    #[test]
    fn compile_participant_in_multiple_boxes_errors() {
        let result = parse_sequence(
            "\
sequenceDiagram
    box Frontend
        participant A
    end
    box Backend
        participant A
    end",
        )
        .unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("multiple participant boxes"));
    }

    #[test]
    fn compile_message_resolves_indices() {
        let model = compile_input("sequenceDiagram\nparticipant A\nparticipant B\nA->>B: hi");
        assert_eq!(model.events.len(), 1);
        match &model.events[0] {
            SequenceEvent::Message { from, to, .. } => {
                assert_eq!(*from, 0);
                assert_eq!(*to, 1);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn compile_create_participant_emits_lifecycle_event_before_message() {
        let model =
            compile_input("sequenceDiagram\nparticipant A\ncreate participant B\nA->>B: hi");

        assert_eq!(model.participants.len(), 2);
        assert_eq!(model.participants[1].id, "B");
        assert_eq!(model.events.len(), 2);
        assert!(matches!(
            model.events[0],
            SequenceEvent::CreateParticipant { participant: 1 }
        ));
        assert!(matches!(
            model.events[1],
            SequenceEvent::Message { from: 0, to: 1, .. }
        ));
    }

    #[test]
    fn compile_create_participant_requires_next_message_to_target_created_participant() {
        let result = parse_sequence(
            "sequenceDiagram\nparticipant A\ncreate participant B\nA->>A: still here",
        )
        .unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("created participant `B`"));
    }

    #[test]
    fn compile_destroy_participant_emits_lifecycle_event_after_destroying_message() {
        let model =
            compile_input("sequenceDiagram\nparticipant A\nparticipant B\ndestroy B\nA->>B: bye");

        assert_eq!(model.events.len(), 2);
        assert!(matches!(
            model.events[0],
            SequenceEvent::Message { from: 0, to: 1, .. }
        ));
        assert!(matches!(
            model.events[1],
            SequenceEvent::DestroyParticipant { participant: 1 }
        ));
    }

    #[test]
    fn compile_destroyed_participant_cannot_be_referenced_after_destruction() {
        let result = parse_sequence(
            "sequenceDiagram\nparticipant A\nparticipant B\ndestroy B\nA->>B: bye\nB->>A: back",
        )
        .unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("cannot be referenced after it is destroyed"));
    }

    #[test]
    fn compile_implicit_participants_from_messages() {
        let model = compile_input("sequenceDiagram\nA->>B: hi");
        assert_eq!(model.participants.len(), 2);
        assert_eq!(model.participants[0].id, "A");
        assert_eq!(model.participants[1].id, "B");
    }

    #[test]
    fn compile_explicit_before_implicit() {
        let model = compile_input("sequenceDiagram\nparticipant B\nA->>B: hi\nA->>C: hello");
        // B is explicit (index 0), A and C are implicit (1, 2)
        assert_eq!(model.participants[0].id, "B");
        assert_eq!(model.participants[1].id, "A");
        assert_eq!(model.participants[2].id, "C");
    }

    #[test]
    fn compile_self_message() {
        let model = compile_input("sequenceDiagram\nparticipant A\nA->>A: think");
        match &model.events[0] {
            SequenceEvent::Message { from, to, .. } => {
                assert_eq!(from, to);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn compile_note_resolves_participant() {
        let model = compile_input("sequenceDiagram\nparticipant A\nNote over A: done");
        assert_eq!(model.events.len(), 1);
        match &model.events[0] {
            SequenceEvent::Note {
                placement,
                participants,
                text,
            } => {
                assert_eq!(*placement, NotePlacement::Over);
                assert_eq!(participants, &[0]);
                assert_eq!(text, "done");
            }
            _ => panic!("expected note"),
        }
    }

    #[test]
    fn compile_note_unknown_participant_errors() {
        let result = parse_sequence("sequenceDiagram\nNote over X: oops").unwrap();
        let compile_result = compile(&result.statements);
        assert!(compile_result.is_err());
        let err = compile_result.unwrap_err().to_string();
        assert!(err.contains("unknown participant"));
    }

    #[test]
    fn compile_autonumber() {
        let model = compile_input(
            "sequenceDiagram\nautonumber\nparticipant A\nparticipant B\nA->>B: first\nB->>A: second",
        );
        assert!(model.autonumber.enabled);
        assert_eq!(model.autonumber.next, 3);
        assert_eq!(model.autonumber.step, 1);
        match &model.events[0] {
            SequenceEvent::Message { number, .. } => assert_eq!(*number, Some(1)),
            _ => panic!("expected message"),
        }
        match &model.events[1] {
            SequenceEvent::Message { number, .. } => assert_eq!(*number, Some(2)),
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn compile_no_autonumber_no_numbers() {
        let model = compile_input("sequenceDiagram\nA->>B: hi");
        assert_eq!(model.autonumber, AutonumberState::default());
        match &model.events[0] {
            SequenceEvent::Message { number, .. } => assert_eq!(*number, None),
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn compile_autonumber_start_step_off_and_resume() {
        let model = compile_input(
            "\
sequenceDiagram
    autonumber 5 2
    participant A
    participant B
    A->>B: first
    B->>A: second
    autonumber off
    A->>B: third
    autonumber
    A->>B: fourth",
        );

        let numbers: Vec<_> = model
            .events
            .iter()
            .filter_map(|event| match event {
                SequenceEvent::Message { number, .. } => Some(*number),
                _ => None,
            })
            .collect();
        assert_eq!(numbers, vec![Some(5), Some(7), None, Some(9)]);
        assert_eq!(
            model.autonumber,
            AutonumberState {
                enabled: true,
                next: 10,
                step: 1,
            }
        );
    }

    #[test]
    fn compile_title() {
        let model = compile_input(
            "\
sequenceDiagram
    title Authentication Flow
    A->>B: Login",
        );
        assert_eq!(model.title.as_deref(), Some("Authentication Flow"));
    }

    #[test]
    fn compile_line_style_mapping() {
        let model = compile_input("sequenceDiagram\nA->>B: solid\nA-->>B: dashed");
        match &model.events[0] {
            SequenceEvent::Message {
                line_style,
                arrow_head,
                ..
            } => {
                assert_eq!(*line_style, LineStyle::Solid);
                assert_eq!(*arrow_head, ArrowHead::Filled);
            }
            _ => panic!("expected message"),
        }
        match &model.events[1] {
            SequenceEvent::Message {
                line_style,
                arrow_head,
                ..
            } => {
                assert_eq!(*line_style, LineStyle::Dashed);
                assert_eq!(*arrow_head, ArrowHead::Filled);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn compile_all_arrow_heads() {
        let model =
            compile_input("sequenceDiagram\nA->>B: filled\nA->B: sync\nA-xB: cross\nA-)B: async");
        let heads: Vec<_> = model
            .events
            .iter()
            .map(|e| match e {
                SequenceEvent::Message { arrow_head, .. } => *arrow_head,
                _ => panic!("expected message"),
            })
            .collect();
        assert_eq!(
            heads,
            vec![
                ArrowHead::Filled,
                ArrowHead::None,
                ArrowHead::Cross,
                ArrowHead::Async
            ]
        );
    }

    #[test]
    fn compile_full_mvp() {
        let model = compile_input(
            "\
sequenceDiagram
    autonumber
    participant A as Alice
    participant B as Bob
    A->>B: hello
    B-->>A: hi back
    A->>A: think
    Note over A: done",
        );
        assert_eq!(model.participants.len(), 2);
        assert_eq!(model.participants[0].label, "Alice");
        assert_eq!(model.participants[1].label, "Bob");
        assert_eq!(model.events.len(), 4);
        assert!(model.autonumber.enabled);
    }

    #[test]
    fn compile_note_left_of() {
        let model =
            compile_input("sequenceDiagram\nparticipant A\nparticipant B\nNote left of A: left");
        match &model.events[0] {
            SequenceEvent::Note {
                placement,
                participants,
                text,
            } => {
                assert_eq!(*placement, NotePlacement::LeftOf);
                assert_eq!(participants, &[0]);
                assert_eq!(text, "left");
            }
            _ => panic!("expected note"),
        }
    }

    #[test]
    fn compile_note_right_of() {
        let model =
            compile_input("sequenceDiagram\nparticipant A\nparticipant B\nNote right of B: right");
        match &model.events[0] {
            SequenceEvent::Note {
                placement,
                participants,
                text,
            } => {
                assert_eq!(*placement, NotePlacement::RightOf);
                assert_eq!(participants, &[1]);
                assert_eq!(text, "right");
            }
            _ => panic!("expected note"),
        }
    }

    #[test]
    fn compile_note_spanning() {
        let model =
            compile_input("sequenceDiagram\nparticipant A\nparticipant B\nNote over A,B: spanning");
        match &model.events[0] {
            SequenceEvent::Note {
                placement,
                participants,
                text,
            } => {
                assert_eq!(*placement, NotePlacement::Over);
                assert_eq!(participants, &[0, 1]);
                assert_eq!(text, "spanning");
            }
            _ => panic!("expected note"),
        }
    }

    #[test]
    fn compile_note_spanning_unknown_participant_errors() {
        let stmts = parse_sequence("sequenceDiagram\nparticipant A\nNote over A,X: oops")
            .unwrap()
            .statements;
        let result = compile(&stmts);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown participant"));
    }

    #[test]
    fn compile_interaction_operators_preserve_event_order() {
        let model = compile_input(
            "\
sequenceDiagram
    participant A
    participant B
    alt available
        A->>B: Request
    else busy
        B->>A: Retry later
    end",
        );
        assert!(matches!(
            &model.events[0],
            SequenceEvent::BlockStart {
                kind: BlockKind::Alt,
                label
            } if label == "available"
        ));
        assert!(matches!(&model.events[1], SequenceEvent::Message { .. }));
        assert!(matches!(
            &model.events[2],
            SequenceEvent::BlockDivider {
                kind: BlockDividerKind::Else,
                label
            } if label == "busy"
        ));
        assert!(matches!(&model.events[3], SequenceEvent::Message { .. }));
        assert_eq!(model.events[4], SequenceEvent::BlockEnd);
    }

    #[test]
    fn compile_else_outside_alt_errors() {
        let result = parse_sequence("sequenceDiagram\nloop retry\nelse nope\nend").unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("not valid inside `loop`"));
    }

    #[test]
    fn compile_unmatched_end_errors() {
        let result = parse_sequence("sequenceDiagram\nend").unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("without an open block"));
    }

    #[test]
    fn compile_unclosed_block_errors() {
        let result = parse_sequence("sequenceDiagram\nalt available").unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("unclosed sequence block"));
    }

    #[test]
    fn compile_par_and_preserve_event_order() {
        let model = compile_input(
            "\
sequenceDiagram
    participant Alice
    participant Bob
    participant Charlie
    par Notifications
        Alice->>Bob: Email
    and
        Alice->>Charlie: SMS
    end",
        );
        assert!(matches!(
            &model.events[0],
            SequenceEvent::BlockStart {
                kind: BlockKind::Par,
                label
            } if label == "Notifications"
        ));
        assert!(matches!(&model.events[1], SequenceEvent::Message { .. }));
        assert!(matches!(
            &model.events[2],
            SequenceEvent::BlockDivider {
                kind: BlockDividerKind::And,
                label
            } if label.is_empty()
        ));
        assert!(matches!(&model.events[3], SequenceEvent::Message { .. }));
        assert_eq!(model.events[4], SequenceEvent::BlockEnd);
    }

    #[test]
    fn compile_critical_option_preserve_event_order() {
        let model = compile_input(
            "\
sequenceDiagram
    participant Alice
    participant Bob
    critical Establish connection
        Alice->>Bob: Connect
    option Timeout
        Alice->>Alice: Retry
    end",
        );
        assert!(matches!(
            &model.events[0],
            SequenceEvent::BlockStart {
                kind: BlockKind::Critical,
                label
            } if label == "Establish connection"
        ));
        assert!(matches!(&model.events[1], SequenceEvent::Message { .. }));
        assert!(matches!(
            &model.events[2],
            SequenceEvent::BlockDivider {
                kind: BlockDividerKind::Option,
                label
            } if label == "Timeout"
        ));
        assert!(matches!(&model.events[3], SequenceEvent::Message { .. }));
        assert_eq!(model.events[4], SequenceEvent::BlockEnd);
    }

    #[test]
    fn compile_break_preserves_event_order() {
        let model = compile_input(
            "\
sequenceDiagram
    participant A
    participant B
    loop Retries
        A->>B: Try
        break Success
            B->>A: Done
        end
    end",
        );
        assert!(matches!(
            &model.events[2],
            SequenceEvent::BlockStart {
                kind: BlockKind::Break,
                label
            } if label == "Success"
        ));
        assert!(matches!(&model.events[3], SequenceEvent::Message { .. }));
        assert_eq!(model.events[4], SequenceEvent::BlockEnd);
        assert_eq!(model.events[5], SequenceEvent::BlockEnd);
    }

    #[test]
    fn compile_and_outside_par_errors() {
        let result = parse_sequence("sequenceDiagram\ncritical establish\nand\nend").unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("not valid inside `critical`"));
    }

    #[test]
    fn compile_option_outside_critical_errors() {
        let result = parse_sequence("sequenceDiagram\npar notify\noption fallback\nend").unwrap();
        let err = compile(&result.statements).unwrap_err().to_string();
        assert!(err.contains("not valid inside `par`"));
    }
}
