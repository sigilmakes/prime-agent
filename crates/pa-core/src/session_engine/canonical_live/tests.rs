use super::*;
use crate::session::canonical::{
    CanonicalAppendReceipt, CanonicalCallerNotifier, CanonicalFrontier, CanonicalJournalRow,
    CanonicalOwnerScope,
};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

struct Notify;
impl CanonicalCallerNotifier for Notify {
    fn changed(&self) {}
}
// Independent fault-injection switches model distinct owner failures.
#[allow(clippy::struct_excessive_bools)]
struct State {
    header: CanonicalJournalRow,
    rows: Vec<CanonicalJournalRow>,
    cursor: CanonicalCursor,
    acks: Vec<CanonicalAckPhase>,
    appends: usize,
    fail_applied_ack: bool,
    fail_abandon_ack: bool,
    mismatch_abandon_ack: bool,
    catchup: bool,
    delivered_users: std::collections::VecDeque<String>,
    delivered_custom: std::collections::VecDeque<String>,
    steps: usize,
    tool_admissions: usize,
    bootstrap_preserves_owner_admission: bool,
    preparation_admissions: Vec<CanonicalAdmissionPolicy>,
    admitted_users: std::collections::HashSet<String>,
    snapshot_gate: Option<Arc<tokio::sync::Notify>>,
    snapshot_started: Arc<tokio::sync::Notify>,
}
struct Owner(Arc<Mutex<State>>);
impl Owner {
    fn new() -> Arc<Self> {
        Self::new_at(std::path::Path::new("/private"))
    }
    fn new_at(cwd: &std::path::Path) -> Arc<Self> {
        let raw: Arc<[u8]> = Arc::from(format!("{{ \"type\":\"session\",\"version\":3,\"id\":\"live-session\",\"timestamp\":\"t\",\"cwd\":{} }}\n", serde_json::to_string(cwd).unwrap()).into_bytes());
        let header = CanonicalJournalRow::parse(raw.clone(), 0, 0).unwrap();
        let hash = Sha256::digest(&raw).into();
        Arc::new(Self(Arc::new(Mutex::new(State {
            header,
            rows: Vec::new(),
            cursor: CanonicalCursor {
                owner: CanonicalOwnerScope::new(),
                session_id: "live-session".into(),
                source_path: "/private/live.jsonl".into(),
                raw_source_generation: hash,
                header_certificate: hash,
                canonical_revision: 0,
                branch_revision: 0,
                selected_leaf: None,
                frontier: CanonicalFrontier {
                    bytes: raw.len() as u64,
                    rows: 1,
                    prefix_certificate: hash,
                },
            },
            acks: Vec::new(),
            appends: 0,
            fail_applied_ack: false,
            fail_abandon_ack: false,
            mismatch_abandon_ack: false,
            catchup: false,
            delivered_users: std::collections::VecDeque::default(),
            delivered_custom: std::collections::VecDeque::default(),
            steps: 0,
            tool_admissions: 0,
            bootstrap_preserves_owner_admission: false,
            preparation_admissions: Vec::new(),
            admitted_users: std::collections::HashSet::new(),
            snapshot_gate: None,
            snapshot_started: Arc::new(tokio::sync::Notify::new()),
        }))))
    }
    fn seed(&self, mut value: serde_json::Value) -> String {
        let mut state = self.0.lock().unwrap();
        let id = format!("accepted-owner-{}", state.rows.len() + 1);
        value["id"] = serde_json::json!(id);
        value["parentId"] = serde_json::json!(state.cursor.selected_leaf);
        value["timestamp"] = serde_json::json!("2026-01-01T00:00:00Z");
        let mut raw = serde_json::to_vec(&value).unwrap();
        raw.push(b'\n');
        let row = CanonicalJournalRow::parse(
            Arc::from(raw),
            state.cursor.frontier.bytes,
            state.cursor.frontier.rows,
        )
        .unwrap();
        state.cursor.frontier.bytes += row.raw().len() as u64;
        state.cursor.frontier.rows += 1;
        state.cursor.canonical_revision += 1;
        state.cursor.selected_leaf = Some(id.clone());
        state.rows.push(row);
        let mut hash = Sha256::new();
        hash.update(state.header.raw());
        for row in &state.rows {
            hash.update(row.raw());
        }
        let hash = hash.finalize().into();
        state.cursor.frontier.prefix_certificate = hash;
        state.cursor.raw_source_generation = hash;
        id
    }
    fn checkpoint(state: &State) -> CanonicalCheckpoint {
        CanonicalCheckpoint {
            cursor: state.cursor.clone(),
            header: state.header.clone(),
            source: CanonicalSource::Full(state.rows.clone()),
        }
    }
}
impl CanonicalLiveOwner for Owner {
    fn bootstrap_preserves_owner_admission(&self) -> bool {
        self.0.lock().unwrap().bootstrap_preserves_owner_admission
    }
    fn snapshot(&self) -> CanonicalFuture<CanonicalLiveSnapshot> {
        let state = self.0.lock().unwrap();
        let pending: std::collections::HashSet<_> = state
            .delivered_users
            .iter()
            .chain(state.delivered_custom.iter())
            .cloned()
            .collect();
        let safe_visible_leaf = state
            .rows
            .iter()
            .find(|row| row.entry().id().is_some_and(|id| pending.contains(id)))
            .map_or_else(
                || state.cursor.selected_leaf.clone(),
                |row| row.entry().parent_id().map(str::to_owned),
            );
        let snapshot = CanonicalLiveSnapshot {
            checkpoint: Self::checkpoint(&state),
            admitted_users: state.admitted_users.clone(),
            pending_users: state.delivered_users.clone(),
            pending_custom: state.delivered_custom.clone(),
            safe_visible_leaf,
        };
        let gate = state.snapshot_gate.clone();
        let started = state.snapshot_started.clone();
        Box::pin(async move {
            started.notify_one();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(snapshot)
        })
    }
    fn register_run(&self, request: CanonicalLiveRunRequest) -> CanonicalFuture<()> {
        assert_eq!(request.expected, self.0.lock().unwrap().cursor);
        Box::pin(async { Ok(()) })
    }
    fn prepare(&self, request: CanonicalLivePrepareRequest) -> CanonicalFuture<CanonicalPrepared> {
        let mut state = self.0.lock().unwrap();
        state.preparation_admissions.push(request.admission.clone());
        match &request.admission {
            CanonicalAdmissionPolicy::Append(ids) | CanonicalAdmissionPolicy::Replace(ids) => {
                state.admitted_users.extend(ids.iter().cloned());
            }
            CanonicalAdmissionPolicy::Preserve => {}
        }
        if let Some(expected) = request.expected {
            assert_eq!(expected.owner, state.cursor.owner);
            assert_eq!(expected.branch_revision, state.cursor.branch_revision);
            assert!(expected.canonical_revision <= state.cursor.canonical_revision);
        }
        let checkpoint = Self::checkpoint(&state);
        Box::pin(async move {
            Ok(CanonicalPrepared {
                caller: CanonicalCallerGuard::new(Arc::new(Notify)).0,
                operation: CanonicalOperation::new(),
                checkpoint,
            })
        })
    }
    fn append(
        &self,
        request: CanonicalLiveAppendRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition> {
        let mut state = self.0.lock().unwrap();
        assert_eq!(request.expected, state.cursor);
        state.appends += 1;
        let id = format!("actual-owner-{}", state.appends);
        let entry = if state.catchup {
            serde_json::json!({"type":"custom","customType":"preflight","data":{"native":"AQ=="}})
        } else {
            serde_json::to_value(request.draft.into_entry()).unwrap()
        };
        let mut entry = entry;
        entry["id"] = serde_json::json!(id);
        entry["parentId"] = serde_json::json!(state.cursor.selected_leaf);
        entry["timestamp"] = serde_json::json!("2026-01-01T00:00:00Z");
        let mut raw = serde_json::to_vec(&entry).unwrap();
        raw.push(b'\n');
        let row = CanonicalJournalRow::parse(
            Arc::from(raw),
            state.cursor.frontier.bytes,
            state.cursor.frontier.rows,
        )
        .unwrap();
        let from = state.cursor.clone();
        state.cursor.frontier.bytes += row.raw().len() as u64;
        state.cursor.frontier.rows += 1;
        state.cursor.canonical_revision += 1;
        state.cursor.selected_leaf = Some(id.clone());
        state.rows.push(row.clone());
        let mut hash = Sha256::new();
        hash.update(state.header.raw());
        for row in &state.rows {
            hash.update(row.raw());
        }
        let hash = hash.finalize().into();
        state.cursor.frontier.prefix_certificate = hash;
        state.cursor.raw_source_generation = hash;
        let receipt = CanonicalAppendReceipt {
            operation: request.operation,
            from,
            to: state.cursor.clone(),
            rows: vec![row],
            committed_entry_id: (!state.catchup).then_some(id),
            replacement_window: None,
        };
        let result = if state.catchup {
            CanonicalCommitDisposition::NeedsCatchUp(receipt)
        } else {
            CanonicalCommitDisposition::Committed(receipt)
        };
        Box::pin(async move { Ok(result) })
    }
    fn acknowledge(
        &self,
        request: CanonicalAcknowledgement,
    ) -> CanonicalFuture<CanonicalLiveAcknowledged> {
        let mut state = self.0.lock().unwrap();
        state.acks.push(request.phase);
        assert_eq!(request.cursor, state.cursor);
        if let Some(adoption) = request.adoption {
            assert_eq!(adoption.operation(), &request.operation);
            assert_eq!(adoption.cursor(), &request.cursor);
        }
        let fail = (state.fail_applied_ack && request.phase == CanonicalAckPhase::AppliedReady)
            || (state.fail_abandon_ack && request.phase == CanonicalAckPhase::AbandonPreparedOnly);
        let mismatch =
            state.mismatch_abandon_ack && request.phase == CanonicalAckPhase::AbandonPreparedOnly;
        let pending: std::collections::HashSet<_> = state
            .delivered_users
            .iter()
            .chain(state.delivered_custom.iter())
            .cloned()
            .collect();
        let mut visible_leaf = state.cursor.selected_leaf.clone();
        let mut current = state.cursor.selected_leaf.as_deref();
        while let Some(id) = current {
            let row = state
                .rows
                .iter()
                .find(|row| row.entry().id() == Some(id))
                .unwrap();
            if pending.contains(id) {
                visible_leaf = row.entry().parent_id().map(str::to_owned);
            }
            current = row.entry().parent_id();
        }
        let proof = CanonicalLiveAcknowledged {
            operation: if mismatch {
                CanonicalOperation::new()
            } else {
                request.operation
            },
            cursor: request.cursor,
            visible_leaf,
        };
        Box::pin(async move {
            if fail {
                Err(failure(
                    "injected ACK loss",
                    CanonicalRequestedWrite::Committed,
                ))
            } else {
                Ok(proof)
            }
        })
    }
    fn admit_user(
        &self,
        _: CanonicalLiveUserRequest,
    ) -> CanonicalFuture<CanonicalLiveUserPrepared> {
        let mut state = self.0.lock().unwrap();
        let id = state
            .delivered_users
            .pop_front()
            .expect("explicit delivered FIFO");
        state.admitted_users.insert(id.clone());
        let checkpoint = Self::checkpoint(&state);
        Box::pin(async move {
            Ok(CanonicalLiveUserPrepared {
                prepared: CanonicalPrepared {
                    caller: CanonicalCallerGuard::new(Arc::new(Notify)).0,
                    operation: CanonicalOperation::new(),
                    checkpoint,
                },
                admitted_entry_id: id,
            })
        })
    }
    fn admit_custom(
        &self,
        _: CanonicalLiveCustomRequest,
    ) -> CanonicalFuture<CanonicalLiveCustomPrepared> {
        let mut state = self.0.lock().unwrap();
        let id = state
            .delivered_custom
            .pop_front()
            .expect("explicit delivered custom FIFO");
        let checkpoint = Self::checkpoint(&state);
        Box::pin(async move {
            Ok(CanonicalLiveCustomPrepared {
                prepared: CanonicalPrepared {
                    caller: CanonicalCallerGuard::new(Arc::new(Notify)).0,
                    operation: CanonicalOperation::new(),
                    checkpoint,
                },
                admitted_entry_id: id,
            })
        })
    }
    fn begin_provider_step(
        &self,
        request: CanonicalLiveProviderStepRequest,
    ) -> CanonicalFuture<()> {
        let mut state = self.0.lock().unwrap();
        if request.expected != state.cursor {
            return Box::pin(async {
                Err(failure(
                    "projection source drifted before actual inference",
                    CanonicalRequestedWrite::NotWritten,
                ))
            });
        }
        state.steps += 1;
        Box::pin(async { Ok(()) })
    }
    fn admit_tool(&self, _: CanonicalLiveToolRequest) -> CanonicalFuture<()> {
        self.0.lock().unwrap().tool_admissions += 1;
        Box::pin(async { Ok(()) })
    }
}
fn manager(owner: &Owner) -> Arc<tokio::sync::Mutex<SessionManager>> {
    let state = owner.0.lock().unwrap();
    let FileEntry::Header { header } = state.header.entry() else {
        unreachable!()
    };
    Arc::new(tokio::sync::Mutex::new(
        SessionManager::in_memory_from_header(
            std::path::Path::new(&header.cwd),
            std::path::Path::new("/private/sessions"),
            header.clone(),
        )
        .unwrap(),
    ))
}
fn tagged_user_intent(value: serde_json::Value) -> serde_json::Result<pa_types::ai::UserMessage> {
    match serde_json::from_value::<AgentMessage>(value)? {
        AgentMessage::User(message) => Ok(message),
        _ => panic!("fixture must contain a tagged User"),
    }
}
fn tagged_custom_intent(
    value: serde_json::Value,
) -> serde_json::Result<pa_types::session::CustomMessage> {
    match serde_json::from_value::<AgentMessage>(value)? {
        AgentMessage::Custom(message) => Ok(message),
        _ => panic!("fixture must contain a tagged Custom"),
    }
}

fn metadata() -> FileEntry {
    serde_json::from_value(serde_json::json!({"type":"custom","customType":"opaque","data":{"id":"nested-payload-id","native":"AAE=","future":[1,true]}})).unwrap()
}
#[tokio::test]
async fn live_receipt_adopts_exact_owner_id_and_raw_bytes_before_ready() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let header = session
        .lock()
        .await
        .canonical_raw_header()
        .unwrap()
        .raw()
        .to_vec();
    let id = live
        .append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .unwrap();
    assert_eq!(id, "actual-owner-1");
    live.ensure_ready().unwrap();
    let session = session.lock().await;
    assert_eq!(session.canonical_raw_header().unwrap().raw(), header);
    assert_eq!(session.canonical_visible_leaf(), Some(id.as_str()));
    assert_eq!(
        session.canonical_raw_retained_rows().unwrap()[0].raw(),
        owner.0.lock().unwrap().rows[0].raw()
    );
    let phases = owner.0.lock().unwrap().acks.clone();
    assert_eq!(
        phases,
        vec![CanonicalAckPhase::Prepared, CanonicalAckPhase::AppliedReady]
    );
}
#[tokio::test]
async fn lost_applied_ack_keeps_raw_row_but_never_publishes_or_reopens() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    owner.0.lock().unwrap().fail_applied_ack = true;
    assert!(live
        .append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .is_err());
    let session = session.lock().await;
    assert!(session.canonical_raw_entry("actual-owner-1").is_some());
    assert_eq!(session.canonical_visible_leaf(), None);
    drop(session);
    assert!(live.observe().await.is_err());
    assert_eq!(owner.0.lock().unwrap().appends, 1);
}
#[tokio::test]
async fn catchup_adopts_acks_and_stops_without_retrying_the_draft() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    owner.0.lock().unwrap().catchup = true;
    let error = live.append(CanonicalMutationKind::ContextPolicy, serde_json::from_value(serde_json::json!({"type":"compaction","summary":"paid once","firstKeptEntryId":"x","tokensBefore":1})).unwrap()).await.unwrap_err();
    assert_eq!(error.requested, CanonicalRequestedWrite::NotWritten);
    assert_eq!(owner.0.lock().unwrap().appends, 1);
    assert!(session
        .lock()
        .await
        .canonical_raw_entry("actual-owner-1")
        .is_some());
    live.ensure_ready().unwrap();
}
#[tokio::test]
async fn dropping_a_prepared_ticket_blocks_before_another_owner_call() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session);
    live.observe().await.unwrap();
    let ticket = live
        .prepare_mutation(CanonicalMutationKind::ContextPolicy)
        .await
        .unwrap();
    drop(ticket);
    assert!(live.observe().await.is_err());
    assert_eq!(owner.0.lock().unwrap().appends, 0);
}
#[tokio::test]
async fn bound_manager_refuses_both_local_and_retained_identity_minting() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner, session.clone());
    live.observe().await.unwrap();
    let mut session = session.lock().await;
    let before = session.retained_entries().to_vec();
    assert!(session.append_custom_entry("bypass", None).is_err());
    assert!(session
        .append_custom_entry_retained("bypass", None)
        .1
        .is_some());
    assert_eq!(session.retained_entries(), before);
}
#[tokio::test]
async fn owner_delivered_user_reuses_exact_id_and_keeps_canonical_timestamp() {
    let owner = Owner::new();
    let value = serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"same content"}],"timestamp":7,"future":{"opaque":"AA=="}}});
    let id = owner.seed(value);
    owner
        .0
        .lock()
        .unwrap()
        .delivered_users
        .push_back(id.clone());
    let session = manager(&owner);
    {
        let entries = {
            let state = owner.0.lock().unwrap();
            state.rows.iter().map(|row| row.entry().clone()).collect()
        };
        let mut guard = session.lock().await;
        guard.adopt_entries(entries);
        guard.replay_admitted_user_rows(vec![id.clone()]).unwrap();
    }
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    assert!(session.lock().await.active_context().messages.is_empty());
    let intent = tagged_user_intent(serde_json::json!({"role":"user","content":[{"type":"text","text":"same content"}],"timestamp":999,"future":{"opaque":"AA=="}})).unwrap();
    live.persist_message(AgentMessage::User(intent))
        .await
        .unwrap();
    let guard = session.lock().await;
    assert_eq!(guard.canonical_visible_leaf(), Some(id.as_str()));
    assert!(guard.canonical_pending_admitted_users().is_empty());
    let FileEntry::Message {
        message: AgentMessage::User(user),
        ..
    } = guard.canonical_raw_entry(&id).unwrap().entry()
    else {
        unreachable!()
    };
    assert_eq!(user.timestamp, 7);
    assert_eq!(guard.canonical_raw_retained_rows().unwrap().len(), 1);
    assert_eq!(owner.0.lock().unwrap().appends, 0);
}
#[tokio::test]
async fn user_delivery_never_searches_for_an_equal_later_body() {
    let owner = Owner::new();
    let first = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":"first","timestamp":7}}));
    let second = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":"wanted","timestamp":8}}));
    owner
        .0
        .lock()
        .unwrap()
        .delivered_users
        .extend([first, second]);
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let intent =
        tagged_user_intent(serde_json::json!({"role":"user","content":"wanted","timestamp":999}))
            .unwrap();
    assert!(live
        .persist_message(AgentMessage::User(intent))
        .await
        .is_err());
    assert!(live.ensure_ready().is_err());
    assert_eq!(owner.0.lock().unwrap().appends, 0);
}
#[tokio::test]
async fn delivered_custom_uses_owner_identity_and_does_not_double_append() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"custom_message","customType":"goal_context","content":"injected","display":false,"details":{"opaque":"AA=="}}));
    owner
        .0
        .lock()
        .unwrap()
        .delivered_custom
        .push_back(id.clone());
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let custom = tagged_custom_intent(serde_json::json!({"role":"custom","customType":"goal_context","content":"injected","display":false,"details":{"opaque":"AA=="},"timestamp":999})).unwrap();
    live.persist_message(AgentMessage::Custom(custom))
        .await
        .unwrap();
    assert_eq!(
        session.lock().await.canonical_visible_leaf(),
        Some(id.as_str())
    );
    assert_eq!(owner.0.lock().unwrap().appends, 0);
}
#[tokio::test]
async fn queued_custom_caps_metadata_publication_until_named_owner_delivery() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"custom_message","customType":"goal_context","content":"queued","display":false}));
    owner.0.lock().unwrap().delivered_custom.push_back(id);
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    assert_eq!(session.lock().await.canonical_visible_leaf(), None);
    let metadata_id = live
        .append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .unwrap();
    {
        let session = session.lock().await;
        assert_eq!(session.canonical_visible_leaf(), None);
        assert!(session.active_context().messages.is_empty());
        assert!(session.canonical_raw_entry(&metadata_id).is_some());
    }
    let message = tagged_custom_intent(serde_json::json!({"role":"custom","customType":"goal_context","content":"queued","display":false,"timestamp":999})).unwrap();
    live.persist_message(AgentMessage::Custom(message))
        .await
        .unwrap();
    assert_eq!(
        session.lock().await.canonical_visible_leaf(),
        Some(metadata_id.as_str())
    );
}
#[tokio::test]
async fn receipt_adoption_uses_captured_revision_not_the_current_revision() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let ticket = live
        .prepare_mutation(CanonicalMutationKind::OwnerMetadata)
        .await
        .unwrap();
    session
        .lock()
        .await
        .invalidate_canonical_local_revision()
        .unwrap();
    let draft = CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, metadata()).unwrap();
    assert!(ticket.commit(draft).await.is_err());
    assert!(live.ensure_ready().is_err());
    assert!(session
        .lock()
        .await
        .canonical_raw_entry("actual-owner-1")
        .is_none());
    assert_eq!(
        owner.0.lock().unwrap().acks,
        vec![CanonicalAckPhase::Prepared]
    );
}
fn model() -> pa_agent::types::Model {
    serde_json::from_value(serde_json::json!({"id":"test","name":"test","api":"openai-completions","provider":"test","baseUrl":"http://localhost","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":100})).unwrap()
}
async fn actual_loop(
    owner: Arc<Owner>,
    conversion: Option<pa_agent::agent_loop::ConvertToLlmFn>,
) -> (
    super::super::AgentSession,
    Arc<pa_agent::scripted::ScriptedProvider>,
) {
    actual_loop_with_tool(owner, conversion, None).await
}

async fn actual_loop_with_tool(
    owner: Arc<Owner>,
    conversion: Option<pa_agent::agent_loop::ConvertToLlmFn>,
    tool: Option<Arc<dyn pa_agent::types::AgentTool>>,
) -> (
    super::super::AgentSession,
    Arc<pa_agent::scripted::ScriptedProvider>,
) {
    let session = manager(&owner);
    let entries = {
        let state = owner.0.lock().unwrap();
        state.rows.iter().map(|row| row.entry().clone()).collect()
    };
    {
        let mut guard = session.lock().await;
        guard.adopt_entries(entries);
        let ids = owner
            .0
            .lock()
            .unwrap()
            .delivered_users
            .iter()
            .cloned()
            .collect();
        guard.replay_admitted_user_rows(ids).unwrap();
    }
    let live = CanonicalLiveSession::new(owner, session.clone());
    live.attach_metadata_writer().await;
    live.observe().await.unwrap();
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model()));
    if let Some(tool) = &tool {
        provider.push_tool_call_turn(
            None,
            vec![("post-ack-tool", tool.name(), serde_json::json!({}))],
        );
    } else {
        provider.push_text_turn("ok");
    }
    let agent = Arc::new(pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions {
        before_event: Some(before_event(live.clone(), session.clone())),
        initial_state: pa_agent::agent::AgentInitialState {
            tools: tool.map(|tool| vec![tool]),
            model: Some(model()),
            ..Default::default()
        },
        stream_fn: Some(wrap_stream_fn(live.clone(), provider.stream_fn())),
        transform_context: Some(wrap_transform_context(
            live.clone(),
            super::super::request_timing::pass_through_transform(),
        )),
        convert_to_llm: Some(
            conversion.unwrap_or_else(super::super::refinement_gate::convert_to_llm),
        ),
        ..Default::default()
    }));
    let session =
        super::super::AgentSession::from_session_arc_live(agent, session, vec![], Some(live))
            .await
            .unwrap();
    (session, provider)
}
#[tokio::test]
async fn canonical_context_notice_keeps_accepted_custom_identity_and_raw_order() {
    for same_payload in [false, true] {
        let owner = Owner::new();
        let input = tagged_custom_intent(serde_json::json!({
            "role":"custom", "customType":"agent_message", "content":"accepted input",
            "display":true, "details":{"source":"test"}, "timestamp":999,
            "native":{"opaque":null}, "delivery":"untrusted-wire-field",
        }))
        .unwrap();
        let id = owner.seed(serde_json::json!({
            "type":"custom_message", "customType":input.custom_type,
            "content":input.content, "display":input.display, "details":input.details,
            "native":{"opaque":null}, "delivery":"untrusted-wire-field",
        }));
        owner
            .0
            .lock()
            .unwrap()
            .delivered_custom
            .push_back(id.clone());
        let original_raw = owner.0.lock().unwrap().rows[0].raw().to_vec();
        let (session, provider) = actual_loop(owner.clone(), None).await;
        let notice = if same_payload {
            input.clone()
        } else {
            tagged_custom_intent(serde_json::json!({
                "role":"custom", "customType":"python_skills_unavailable",
                "content":"host notice", "display":true, "details":{"skills":["test"]},
                "native":{"opaque":null}, "delivery":"untrusted-wire-field", "timestamp":888,
            }))
            .unwrap()
        };
        session.queue_next_turn_row(notice.clone());
        session.prompt_injected_message(&input).await.unwrap();
        session
            .canonical_live_session()
            .unwrap()
            .ensure_ready()
            .unwrap();
        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        let agent_state = session.agent().state().await;
        assert_eq!(agent_state.messages.len(), 3, "no duplicate SDK deliveries");
        let pa_agent::types::AgentMessage::Custom(sdk_notice) = &agent_state.messages[0] else {
            panic!("the SDK stream retains the producer's context event");
        };
        assert_eq!(
            sdk_notice.delivery,
            pa_agent::types::CustomMessageDelivery::Context
        );
        let pa_agent::types::AgentMessage::Custom(sdk_input) = &agent_state.messages[1] else {
            panic!("the SDK input still has its own delivery");
        };
        assert_eq!(
            sdk_input.delivery,
            pa_agent::types::CustomMessageDelivery::Input
        );
        let owner_state = owner.0.lock().unwrap();
        assert!(owner_state.delivered_custom.is_empty());
        assert_eq!(owner_state.steps, 1);
        assert_eq!(
            owner_state.appends, 2,
            "only the notice and Assistant append"
        );
        assert_eq!(owner_state.rows.len(), 3);
        assert_eq!(owner_state.rows[0].raw(), original_raw);
        assert_eq!(owner_state.rows[0].entry().id(), Some(id.as_str()));
        let FileEntry::CustomMessage { payload, .. } = owner_state.rows[1].entry() else {
            panic!("the notice is distinct owner metadata");
        };
        assert_ne!(owner_state.rows[1].entry().id(), Some(id.as_str()));
        assert_eq!(payload.custom_type, notice.custom_type);
        assert_eq!(payload.content, notice.content);
        assert_eq!(payload.display, notice.display);
        assert_eq!(payload.details, notice.details);
        assert_eq!(payload.rest, notice.rest);
        let expected = owner_state.rows[..2]
            .iter()
            .map(|row| {
                let FileEntry::CustomMessage { payload, .. } = row.entry() else {
                    panic!("two canonical custom rows");
                };
                pa_agent::types::Message::User(pa_agent::types::UserMessage {
                    content: pa_agent::types::UserContent::Parts(vec![
                        pa_agent::types::UserPart::Text(pa_agent::types::TextContent {
                            text: payload.content.text(),
                            text_signature: None,
                        }),
                    ]),
                    timestamp: crate::session::timestamp_to_millis(row.entry().timestamp()) as i64,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls[0].messages, expected,
            "provider sees exact canonical order and timestamps"
        );
    }
}

#[tokio::test]
async fn certified_model_view_omits_only_typed_legacy_harness_rows() {
    // Controlled TS-shaped payloads, not a captured producer transcript. The
    // owner supplies every canonical ID, parent, timestamp and raw certificate.
    let owner = Owner::new();
    for kind in ["harness_digest", "refinement_notice", "refinement_outcome"] {
        owner.seed(serde_json::json!({
            "type":"custom_message", "customType":kind, "content":"shared controlled text",
            "display":false, "details":{"digest":"controlled", "stateFingerprint":"controlled"},
            "opaque":{"preserve":[null, true, 3]},
        }));
    }
    let ordinary = [
        serde_json::json!({"role":"user", "content":[{"type":"text", "text":"source user"}], "timestamp":11}),
        serde_json::json!({"role":"assistant", "content":[{"type":"toolCall", "id":"fixture-tool-call", "name":"python_repl", "arguments":{"code":"pass"}}], "api":"openai-completions", "provider":"test", "model":"test", "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}, "stopReason":"toolUse", "timestamp":12, "responseId":"fixture-tool-response"}),
        serde_json::json!({"role":"toolResult", "toolCallId":"fixture-tool-call", "toolName":"python_repl", "content":[{"type":"text", "text":"controlled result"}], "details":{"opaque":[null,true]}, "isError":false, "timestamp":13}),
        serde_json::json!({"role":"assistant", "content":[{"type":"text", "text":"source assistant"}], "api":"openai-completions", "provider":"test", "model":"test", "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}, "stopReason":"stop", "timestamp":14, "responseId":"fixture-final-response"}),
        serde_json::json!({"role":"user", "content":[{"type":"text", "text":"[harness-digest] shared controlled text [refinement-notice]"}], "timestamp":15}),
    ];
    for message in &ordinary {
        owner.seed(serde_json::json!({"type":"message", "message":message, "opaque":{"preserve":"ordinary"}}));
    }
    let plugin_id = owner.seed(serde_json::json!({
        "type":"custom_message", "customType":"plugin.harness.context", "content":"shared controlled text",
        "display":false, "details":{"explicit":true}, "opaque":{"preserve":"plugin"},
    }));
    let continuation = serde_json::json!({"role":"user", "content":[{"type":"text", "text":"continue controlled history"}], "timestamp":16});
    let accepted = owner.seed(serde_json::json!({"type":"message", "message":continuation}));
    owner.0.lock().unwrap().delivered_users.push_back(accepted);
    let original = owner
        .0
        .lock()
        .unwrap()
        .rows
        .iter()
        .map(|row| row.raw().to_vec())
        .collect::<Vec<_>>();
    let (session, provider) = actual_loop(owner.clone(), None).await;
    session
        .prompt(
            "continue controlled history",
            super::super::PromptOptions::default(),
        )
        .await
        .unwrap();
    session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    let calls = provider.calls();
    assert_eq!(
        calls.len(),
        1,
        "typed model-view exclusions must not reject a certified continuation"
    );
    let mut expected = ordinary
        .iter()
        .cloned()
        .map(|message| serde_json::from_value::<pa_agent::types::Message>(message).unwrap())
        .collect::<Vec<_>>();
    let plugin_timestamp = {
        let state = owner.0.lock().unwrap();
        let row = state
            .rows
            .iter()
            .find(|row| row.entry().id() == Some(plugin_id.as_str()))
            .unwrap();
        crate::session::timestamp_to_millis(row.entry().timestamp()) as i64
    };
    expected.push(pa_agent::types::Message::User(
        pa_agent::types::UserMessage {
            content: pa_agent::types::UserContent::Parts(vec![pa_agent::types::UserPart::Text(
                pa_agent::types::TextContent {
                    text: "shared controlled text".into(),
                    text_signature: None,
                },
            )]),
            timestamp: plugin_timestamp,
        },
    ));
    expected.push(serde_json::from_value(continuation).unwrap());
    assert_eq!(
        calls[0].messages, expected,
        "ordinary history, user quotes and new plugin context retain exact model order"
    );
    let (physical, leaf, count) = {
        let state = owner.0.lock().unwrap();
        assert_eq!(state.steps, 1);
        assert_eq!(
            state.appends, 1,
            "only the continuation Assistant may append"
        );
        assert_eq!(state.rows.len(), original.len() + 1);
        for (row, before) in state.rows.iter().zip(&original) {
            assert_eq!(
                row.raw(),
                before,
                "every source byte and envelope remains unchanged"
            );
        }
        for (row, kind) in state.rows[..3].iter().zip([
            "harness_digest",
            "refinement_notice",
            "refinement_outcome",
        ]) {
            assert!(
                matches!(row.entry(), FileEntry::CustomMessage { payload, .. } if payload.custom_type == kind)
            );
        }
        let mut physical = state.header.raw().to_vec();
        for row in &state.rows {
            physical.extend_from_slice(row.raw());
        }
        (
            physical,
            state.cursor.selected_leaf.clone(),
            state.rows.len(),
        )
    };
    let persistence = session.shared_persistence();
    let manager = persistence.lock().await;
    assert_eq!(manager.canonical_binding().unwrap().selected_leaf, leaf);
    let visible = manager.canonical_visible_context();
    let legacy = visible
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(custom)
                if matches!(
                    custom.custom_type.as_str(),
                    "harness_digest" | "refinement_notice" | "refinement_outcome"
                ) =>
            {
                Some(custom.custom_type.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        legacy,
        vec!["harness_digest", "refinement_notice", "refinement_outcome"],
        "the policy must not hide legacy rows by altering canonical context assembly"
    );
    let rows = manager.canonical_raw_full_rows().unwrap();
    assert_eq!(rows.len(), count);
    let mut certified = manager.canonical_raw_header().unwrap().raw().to_vec();
    for row in rows {
        certified.extend_from_slice(row.raw());
    }
    assert_eq!(
        certified, physical,
        "model-only suppression must retain the complete certified raw history"
    );
}

#[tokio::test]
async fn certified_compaction_summary_scrubs_only_model_view_harness_fields() {
    let owner = Owner::new();
    let kept = owner.seed(serde_json::json!({"type":"message","message":{
        "role":"user","content":[{"type":"text","text":"kept controlled user"}],"timestamp":21,
    }}));
    owner.seed(serde_json::json!({"type":"compaction","summary":"controlled summary",
        "firstKeptEntryId":kept,"tokensBefore":40,"customInstructions":"controlled instructions",
        "harnessDigest":"controlled saved memory","harnessStateFingerprint":"controlled fingerprint",
        "opaque":{"preserve":[null,true,3]},
    }));
    let accepted = owner.seed(serde_json::json!({"type":"message","message":{
        "role":"user","content":[{"type":"text","text":"continue controlled summary"}],"timestamp":22,
    }}));
    owner.0.lock().unwrap().delivered_users.push_back(accepted);
    let original = owner
        .0
        .lock()
        .unwrap()
        .rows
        .iter()
        .map(|row| row.raw().to_vec())
        .collect::<Vec<_>>();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let capture = observed.clone();
    let convert = super::super::refinement_gate::convert_to_llm();
    let conversion: pa_agent::agent_loop::ConvertToLlmFn = Arc::new(move |messages| {
        let capture = capture.clone();
        let convert = convert.clone();
        Box::pin(async move {
            *capture.lock().unwrap() = messages.clone();
            convert(messages).await
        })
    });
    let (session, provider) = actual_loop(owner.clone(), Some(conversion)).await;
    let summary = {
        let manager = session.session_handle().lock().await;
        manager
            .canonical_visible_context()
            .messages
            .into_iter()
            .find_map(|message| match message {
                AgentMessage::CompactionSummary(summary) => Some(summary),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        summary.harness_digest.as_deref(),
        Some("controlled saved memory")
    );
    assert_eq!(
        summary.harness_state_fingerprint.as_deref(),
        Some("controlled fingerprint")
    );
    let mut expected = summary.clone();
    expected.harness_digest = None;
    expected.harness_state_fingerprint = None;
    session
        .prompt(
            "continue controlled summary",
            super::super::PromptOptions::default(),
        )
        .await
        .unwrap();
    session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    let summaries = {
        let observed = observed.lock().unwrap();
        observed
            .iter()
            .filter_map(|message| {
                match serde_json::from_value::<AgentMessage>(serde_json::to_value(message).unwrap())
                    .unwrap()
                {
                    AgentMessage::CompactionSummary(summary) => Some(summary),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        summaries,
        vec![expected],
        "only the two legacy harness fields may be scrubbed in the model view"
    );
    {
        let state = owner.0.lock().unwrap();
        assert_eq!(state.steps, 1);
        assert_eq!(state.appends, 1);
        assert_eq!(state.rows.len(), original.len() + 1);
        for (row, before) in state.rows.iter().zip(&original) {
            assert_eq!(row.raw(), before);
        }
    }
    let manager = session.session_handle().lock().await;
    let unchanged = manager
        .canonical_visible_context()
        .messages
        .into_iter()
        .find_map(|message| match message {
            AgentMessage::CompactionSummary(summary) => Some(summary),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        unchanged, summary,
        "canonical summary metadata must remain untouched"
    );
}

#[tokio::test]
async fn actual_loop_transform_uses_owner_timestamp_before_conversion() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let (session, provider) = actual_loop(owner.clone(), None).await;
    session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .unwrap();
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    let pa_agent::types::Message::User(user) = &calls[0].messages[0] else {
        panic!("expected canonical user")
    };
    assert_eq!(user.timestamp, 7);
    assert_eq!(owner.0.lock().unwrap().steps, 1);
}
#[tokio::test]
async fn actual_loop_transform_includes_owner_staged_bash_before_conversion() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let (session, provider) = actual_loop(owner.clone(), None).await;
    session.agent().subscribe(move |event, _| { let owner = owner.clone(); Box::pin(async move {
        if matches!(event, pa_agent::types::AgentEvent::MessageEnd { message: pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(_)) }) {
            owner.seed(serde_json::json!({"type":"message","message":{"role":"bashExecution","command":"echo staged","output":"owner-staged-output","exitCode":0,"cancelled":false,"truncated":false,"timestamp":8}}));
        }
        Ok(())
    }) }).await;
    session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .unwrap();
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert!(serde_json::to_string(&calls[0].messages)
        .unwrap()
        .contains("owner-staged-output"));
}
#[tokio::test]
async fn actual_loop_source_drift_after_transform_refuses_before_paid_stream() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let drift_owner = owner.clone();
    let convert = super::super::refinement_gate::convert_to_llm();
    let conversion: pa_agent::agent_loop::ConvertToLlmFn = Arc::new(move |messages| {
        let owner = drift_owner.clone();
        let convert = convert.clone();
        Box::pin(async move {
            owner.seed(serde_json::json!({"type":"custom","customType":"concurrent","data":{"changed":true}}));
            convert(messages).await
        })
    });
    let (session, provider) = actual_loop(owner.clone(), Some(conversion)).await;
    assert!(session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .is_err());
    assert!(provider.calls().is_empty());
    assert_eq!(owner.0.lock().unwrap().steps, 0);
}

#[tokio::test]
async fn actual_loop_local_revision_drift_after_transform_refuses_before_paid_stream() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let carrier: Arc<Mutex<Option<Arc<tokio::sync::Mutex<SessionManager>>>>> = Arc::default();
    let capture = carrier.clone();
    let convert = super::super::refinement_gate::convert_to_llm();
    let conversion: pa_agent::agent_loop::ConvertToLlmFn = Arc::new(move |messages| {
        let capture = capture.clone();
        let convert = convert.clone();
        Box::pin(async move {
            let manager = capture.lock().unwrap().clone().unwrap();
            manager.lock().await.invalidate_canonical_local_revision()?;
            convert(messages).await
        })
    });
    let (session, provider) = actual_loop(owner.clone(), Some(conversion)).await;
    *carrier.lock().unwrap() = Some(session.session_handle().clone());
    assert!(session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .is_err());
    assert!(provider.calls().is_empty());
    assert_eq!(owner.0.lock().unwrap().steps, 0);
}

#[tokio::test]
async fn actual_run_git_boundary_commits_once_and_dedupes_at_end() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("git")
        .args(["init", "-b", "canonical-live"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(result.status.success());
    let owner = Owner::new_at(dir.path());
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let (session, provider) = actual_loop(owner.clone(), None).await;
    session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    let state = owner.0.lock().unwrap();
    assert_eq!(
        state
            .rows
            .iter()
            .filter(|row| matches!(row.entry(), FileEntry::GitState { .. }))
            .count(),
        1
    );
    assert_eq!(
        state.appends, 2,
        "one GitState and one actual Assistant row"
    );
    assert_eq!(
        state.acks,
        vec![
            CanonicalAckPhase::Prepared,            // initial raw bootstrap
            CanonicalAckPhase::Prepared,            // GitState preparation
            CanonicalAckPhase::AppliedReady,        // durable GitState adoption
            CanonicalAckPhase::Prepared,            // named User delivery (not a second append)
            CanonicalAckPhase::Prepared,            // actual terminal preparation
            CanonicalAckPhase::AppliedReady,        // durable Assistant adoption
            CanonicalAckPhase::Prepared,            // AgentEnd GitState preparation
            CanonicalAckPhase::AbandonPreparedOnly, // unchanged GitState, no write
        ]
    );
}

#[tokio::test]
async fn repeated_immutable_observation_never_prepares_or_acknowledges() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    for _ in 0..3 {
        live.observe().await.unwrap();
    }
    live.ensure_ready().unwrap();
    assert!(owner.0.lock().unwrap().preparation_admissions.is_empty());
    assert!(owner.0.lock().unwrap().acks.is_empty());
    assert_eq!(owner.0.lock().unwrap().appends, 0);
    assert!(session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
    // Actual mutation still owns a real preparation and an explicit abandon.
    live.prepare_mutation(CanonicalMutationKind::OwnerMetadata)
        .await
        .unwrap()
        .abandon()
        .await
        .unwrap();
    assert_eq!(
        owner.0.lock().unwrap().acks,
        vec![
            CanonicalAckPhase::Prepared,
            CanonicalAckPhase::AbandonPreparedOnly
        ]
    );
}

#[tokio::test]
async fn assistant_commit_callback_runs_after_ack_without_owner_or_manager_guards() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let (session, provider) = actual_loop(owner.clone(), None).await;
    let live = session.canonical_live_session().unwrap();
    let weak = Arc::downgrade(&live);
    let manager = session.session_handle().clone();
    let callback_owner = owner.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let registration = live
        .register_after_assistant_commit(Arc::new(move || {
            let live = weak.upgrade().unwrap();
            let manager = manager.clone();
            let owner = callback_owner.clone();
            let calls = callback_calls.clone();
            Box::pin(async move {
                live.ensure_ready()?;
                assert!(
                    manager.try_lock().is_ok(),
                    "manager guard crossed callback phase"
                );
                {
                    let state = owner
                        .0
                        .try_lock()
                        .expect("owner guard crossed callback phase");
                    assert_eq!(state.acks.last(), Some(&CanonicalAckPhase::AppliedReady));
                    assert!(matches!(
                        state.rows.last().unwrap().entry(),
                        FileEntry::Message {
                            message: AgentMessage::Assistant(_),
                            ..
                        }
                    ));
                }
                live.append(CanonicalMutationKind::OwnerMetadata, metadata())
                    .await?;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }))
        .unwrap();
    session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(registration);
    assert!(live.after_assistant_commit.lock().unwrap().is_none());
}

#[tokio::test]
async fn lost_assistant_ack_never_calls_post_commit_work() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let callback_owner = owner.clone();
    let convert = super::super::refinement_gate::convert_to_llm();
    let conversion: pa_agent::agent_loop::ConvertToLlmFn = Arc::new(move |messages| {
        let owner = callback_owner.clone();
        let convert = convert.clone();
        Box::pin(async move {
            owner.0.lock().unwrap().fail_applied_ack = true;
            convert(messages).await
        })
    });
    let (session, provider) = actual_loop(owner.clone(), Some(conversion)).await;
    let live = session.canonical_live_session().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let _registration = live
        .register_after_assistant_commit(Arc::new(move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }))
        .unwrap();
    assert!(session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .is_err());
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(live.ensure_ready().is_err());
}

#[tokio::test]
async fn failed_post_commit_work_stops_agent_delivery_and_blocks_projection() {
    struct ProofTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        schema: serde_json::Value,
    }
    impl pa_agent::types::AgentTool for ProofTool {
        fn name(&self) -> &'static str {
            "proof_tool"
        }
        fn description(&self) -> &'static str {
            "Observe admission after the committed assistant."
        }
        fn parameters(&self) -> &serde_json::Value {
            &self.schema
        }
        fn execute(
            self: Arc<Self>,
            _id: String,
            _params: serde_json::Value,
            _signal: pa_agent::abort::AbortSignal,
            _update: pa_agent::types::AgentToolUpdateCallback,
        ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(pa_agent::types::AgentToolResult::text("executed")) })
        }
    }
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"canonical user"}],"timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
    let tool_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = Arc::new(ProofTool {
        calls: tool_calls.clone(),
        schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
    });
    let (session, provider) = actual_loop_with_tool(owner.clone(), None, Some(tool)).await;
    let live = session.canonical_live_session().unwrap();
    // Capture only safe event kinds, not messages or diagnostic error bodies.
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let observed = delivered.clone();
    let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_starts = tool_starts.clone();
    let subscription = session
        .agent()
        .subscribe(move |event, _signal| {
            match event {
                pa_agent::types::AgentEvent::MessageEnd {
                    message:
                        pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                            assistant,
                        )),
                } => {
                    let kinds: Vec<_> = assistant
                        .diagnostics
                        .unwrap_or_default()
                        .into_iter()
                        .map(|diagnostic| diagnostic.kind)
                        .collect();
                    observed
                        .lock()
                        .unwrap()
                        .push((assistant.stop_reason, kinds));
                }
                pa_agent::types::AgentEvent::ToolExecutionStart { .. } => {
                    observed_starts.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Box::pin(async { Ok(()) })
        })
        .await;
    let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let callback_count = callbacks.clone();
    let _registration = live
        .register_after_assistant_commit(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("post-ACK work refused") })
        }))
        .unwrap();
    assert!(session
        .prompt("canonical user", super::super::PromptOptions::default())
        .await
        .is_err());
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    {
        let state = owner.0.lock().unwrap();
        let assistants: Vec<_> = state
            .rows
            .iter()
            .filter_map(|row| match row.entry() {
                FileEntry::Message {
                    message: AgentMessage::Assistant(assistant),
                    ..
                } => Some(assistant),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistants.len(),
            1,
            "the authoritative committed assistant remains durable"
        );
        let assistant = serde_json::to_value(assistants[0]).unwrap();
        assert_eq!(assistant["stopReason"], "toolUse");
        assert_eq!(assistant["content"][0]["id"], "post-ack-tool");
        assert_eq!(assistant["content"][0]["name"], "proof_tool");
        assert_eq!(state.acks.last(), Some(&CanonicalAckPhase::AppliedReady));
        assert_eq!(state.tool_admissions, 0);
    }
    {
        let delivered = delivered.lock().unwrap();
        assert!(
            delivered.is_empty(),
            "unexpected Assistant terminal kinds: {delivered:?}"
        );
    }
    assert_eq!(tool_starts.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert!(live.ensure_ready().is_err());
    subscription.unsubscribe().await;
}

#[tokio::test]
async fn failed_local_install_attempts_exact_abandon_but_flight_stays_blocked() {
    use crate::session::canonical::CanonicalCallerState;
    for (fail, mismatch, expected_state) in [
        (false, false, CanonicalCallerState::Completed),
        (true, false, CanonicalCallerState::Abandoned),
        (false, true, CanonicalCallerState::Abandoned),
    ] {
        let owner = Owner::new();
        {
            let mut state = owner.0.lock().unwrap();
            state.fail_abandon_ack = fail;
            state.mismatch_abandon_ack = mismatch;
        }
        let session = manager(&owner);
        let live = CanonicalLiveSession::new(owner.clone(), session.clone());
        let flight = live.enter().unwrap();
        let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify));
        let prepared = CanonicalPrepared {
            caller,
            operation: CanonicalOperation::new(),
            checkpoint: Owner::checkpoint(&owner.0.lock().unwrap()),
        };
        let revision = session.lock().await.canonical_local_revision();
        let Err(error) = live
            .install(
                prepared,
                revision + 1,
                None,
                CanonicalAdmissionPolicy::Preserve,
            )
            .await
        else {
            panic!("injected revision mismatch must fail");
        };
        assert_eq!(error.detail, "canonical SDK local revision conflict");
        assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
        assert_eq!(signal.state(), expected_state);
        assert_eq!(
            owner.0.lock().unwrap().acks,
            vec![CanonicalAckPhase::AbandonPreparedOnly]
        );
        assert!(session.lock().await.canonical_binding().is_none());
        drop(flight);
        assert!(live.observe().await.is_err());
    }
}

#[tokio::test]
async fn rejected_immutable_snapshot_releases_busy_without_owner_ack_or_poison() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let cursor = owner.0.lock().unwrap().cursor.clone();
    owner.0.lock().unwrap().cursor.branch_revision += 1;
    assert!(live.observe().await.is_err());
    live.ensure_ready().unwrap();
    assert!(owner.0.lock().unwrap().acks.is_empty());
    assert!(owner.0.lock().unwrap().preparation_admissions.is_empty());
    owner.0.lock().unwrap().cursor = cursor;
    live.observe().await.unwrap();
    live.append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .unwrap();
    assert_eq!(owner.0.lock().unwrap().appends, 1);
}

#[tokio::test]
async fn cancelling_immutable_read_releases_busy_without_caller_or_ack_debt() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session);
    let started = {
        let mut state = owner.0.lock().unwrap();
        state.snapshot_gate = Some(Arc::new(tokio::sync::Notify::new()));
        state.snapshot_started.clone()
    };
    let observing = live.clone();
    let task = tokio::spawn(async move { observing.observe().await });
    started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    live.ensure_ready().unwrap();
    {
        let mut state = owner.0.lock().unwrap();
        assert!(state.acks.is_empty());
        assert!(state.preparation_admissions.is_empty());
        assert_eq!(state.appends, 0);
        state.snapshot_gate = None;
    }
    live.append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .unwrap();
    assert_eq!(owner.0.lock().unwrap().appends, 1);
}

#[tokio::test]
async fn metadata_from_rejects_original_ledger_cursor_after_snapshot_refresh_before_io() {
    let owner = Owner::new();
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    live.observe().await.unwrap();
    let expected = session.lock().await.canonical_binding().unwrap().clone();
    // Logical owner ledger changes without changing the physical file/frontier.
    owner.0.lock().unwrap().cursor.canonical_revision += 1;
    let Err(error) =
        CanonicalMetadataWriter::append_metadata_from(live.clone(), metadata(), expected).await
    else {
        panic!("original metadata cursor must remain exact");
    };
    assert_eq!(error.requested, CanonicalRequestedWrite::NotWritten);
    assert_eq!(owner.0.lock().unwrap().appends, 0);
    assert_eq!(
        owner.0.lock().unwrap().acks,
        vec![
            CanonicalAckPhase::Prepared,
            CanonicalAckPhase::AbandonPreparedOnly
        ]
    );
    live.ensure_ready().unwrap();
}

#[tokio::test]
async fn lazy_accepted_input_observes_without_preseeding_then_named_delivery_admits_once() {
    let owner = Owner::new();
    let id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":"lazy accepted","timestamp":7}}));
    owner
        .0
        .lock()
        .unwrap()
        .delivered_users
        .push_back(id.clone());
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    // Startup projection proof IDs are not an SDK execution queue.
    assert!(session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
    live.observe().await.unwrap();
    assert!(session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
    assert_eq!(session.lock().await.canonical_visible_leaf(), None);
    {
        let state = owner.0.lock().unwrap();
        assert!(state.admitted_users.is_empty());
        assert_eq!(state.delivered_users.front(), Some(&id));
        assert!(state.acks.is_empty());
        assert!(state.preparation_admissions.is_empty());
    }
    let canonical = live
        .admit_user(
            serde_json::from_value(serde_json::json!({"content":"lazy accepted","timestamp":99}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(canonical.timestamp, 7);
    assert!(session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
    assert_eq!(
        session.lock().await.canonical_visible_leaf(),
        Some(id.as_str())
    );
    let state = owner.0.lock().unwrap();
    assert_eq!(state.admitted_users, std::collections::HashSet::from([id]));
    assert!(state.delivered_users.is_empty());
    assert_eq!(state.acks, vec![CanonicalAckPhase::Prepared]);
    assert_eq!(state.appends, 0);
}

async fn install_pending_user_for_retirement(
    owner: &Arc<Owner>,
    live: &CanonicalLiveSession,
    message: pa_types::ai::UserMessage,
) {
    live.observe().await.unwrap();
    let (revision, binding) = {
        let session = live.session.lock().await;
        (
            session.canonical_local_revision(),
            session.canonical_binding().cloned(),
        )
    };
    let delivered = owner
        .admit_user(CanonicalLiveUserRequest {
            expected: binding.clone(),
            source: CanonicalSourceRequest::Full,
            message,
        })
        .await
        .unwrap();
    let (caller, _, _, _, _) = live
        .install(
            delivered.prepared,
            revision,
            binding,
            CanonicalAdmissionPolicy::Append(vec![delivered.admitted_entry_id]),
        )
        .await
        .unwrap();
    caller.complete();
}

#[tokio::test]
async fn completed_view_retirement_refuses_unconsumed_core_admission_and_stays_closed() {
    let owner = Owner::new();
    let accepted = owner.seed(serde_json::json!({"type":"message", "message":{"role":"user", "content":"unfinished", "timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(accepted);
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    install_pending_user_for_retirement(
        &owner,
        &live,
        tagged_user_intent(
            serde_json::json!({"role":"user", "content":"unfinished", "timestamp":7}),
        )
        .unwrap(),
    )
    .await;
    assert!(!session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
    let error = live.retire_completed_runtime().await.unwrap_err();
    assert!(error
        .detail
        .contains("Core still has admitted execution work"));
    assert!(live.observe().await.is_err());
    assert!(live
        .append(CanonicalMutationKind::OwnerMetadata, metadata())
        .await
        .is_err());
    assert!(!session
        .lock()
        .await
        .canonical_pending_admitted_users()
        .is_empty());
}

#[tokio::test]
async fn replacement_closed_view_refuses_admission_without_resetting_pending_facts() {
    let owner = Owner::new();
    let accepted = owner.seed(serde_json::json!({"type":"message", "message":{"role":"user", "content":"cancelled pending", "timestamp":7}}));
    owner.0.lock().unwrap().delivered_users.push_back(accepted);
    let session = manager(&owner);
    let live = CanonicalLiveSession::new(owner.clone(), session.clone());
    install_pending_user_for_retirement(
        &owner,
        &live,
        tagged_user_intent(
            serde_json::json!({"role":"user", "content":"cancelled pending", "timestamp":7}),
        )
        .unwrap(),
    )
    .await;
    let pending = session.lock().await.canonical_pending_admitted_users();
    let cursor = owner.0.lock().unwrap().cursor.clone();
    live.block_projection();
    assert!(live.observe().await.is_err());
    assert!(live
        .persist_message(
            tagged_user_intent(serde_json::json!({"role":"user", "content":"late", "timestamp":8}))
                .map(AgentMessage::User)
                .unwrap()
        )
        .await
        .is_err());
    assert_eq!(
        session.lock().await.canonical_pending_admitted_users(),
        pending
    );
    assert_eq!(owner.0.lock().unwrap().cursor, cursor);
}
