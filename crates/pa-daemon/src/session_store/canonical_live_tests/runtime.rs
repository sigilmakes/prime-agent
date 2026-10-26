use super::*;
use pa_core::session::canonical::*;
use pa_core::session_engine::canonical_live::*;
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
enum AbandonReply {
    #[default]
    Exact,
    Lost,
    Foreign,
}
#[derive(Default)]
enum RunStartFault {
    #[default]
    None,
    OwnedAdvance,
    LedgerOnly(String),
    ExternalAdvance,
    ForeignOwner,
    ForgedPrefix,
    ForgedPosition,
    ForgedLeaf,
    ForgedGeneration,
}
#[derive(Default)]
struct Faults {
    run_start: RunStartFault,
    advance_at_provider: bool,
    lose_append_reply: bool,
    lose_applied_ack: bool,
    abandon_reply: AbandonReply,
    run_prepared_reply: AbandonReply,
    foreign_run_ack: bool,
    opening_operation: Option<CanonicalOperation>,
}
struct RealOwner {
    store: Mutex<SessionFile>,
    faults: Mutex<Faults>,
}
impl CanonicalLiveOwner for RealOwner {
    fn snapshot(&self) -> CanonicalFuture<CanonicalLiveSnapshot> {
        let result = self.store.lock().unwrap().canonical_snapshot();
        Box::pin(async move { result })
    }
    fn prepare(&self, request: CanonicalLivePrepareRequest) -> CanonicalFuture<CanonicalPrepared> {
        let result = self.store.lock().unwrap().prepare_canonical(request);
        Box::pin(async move { result })
    }
    fn append(
        &self,
        request: CanonicalLiveAppendRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition> {
        let result = self.store.lock().unwrap().append_canonical(request);
        let lose_reply = self.faults.lock().unwrap().lose_append_reply;
        Box::pin(async move {
            let result = result?;
            if lose_reply {
                Err(CanonicalStoreError {
                    kind: CanonicalFailureKind::AdoptionConflict,
                    requested: CanonicalRequestedWrite::Unknown,
                    source: CanonicalSourceDisposition::NeedsReconcile,
                    detail: "injected lost append receipt".into(),
                })
            } else {
                Ok(result)
            }
        })
    }
    fn acknowledge(
        &self,
        mut request: CanonicalAcknowledgement,
    ) -> CanonicalFuture<CanonicalLiveAcknowledged> {
        let opening =
            self.faults.lock().unwrap().opening_operation.as_ref() == Some(&request.operation);
        let faults = self.faults.lock().unwrap();
        if opening && faults.foreign_run_ack {
            request.operation = CanonicalOperation::new();
        }
        let reply = match request.phase {
            CanonicalAckPhase::AbandonPreparedOnly => faults.abandon_reply,
            CanonicalAckPhase::Prepared if opening => faults.run_prepared_reply,
            CanonicalAckPhase::AppliedReady if faults.lose_applied_ack => AbandonReply::Lost,
            CanonicalAckPhase::Prepared
            | CanonicalAckPhase::AppliedReady
            | CanonicalAckPhase::ReconciledBlocked
            | CanonicalAckPhase::Quarantined => AbandonReply::Exact,
        };
        drop(faults);
        let result = self.store.lock().unwrap().acknowledge_canonical(request);
        Box::pin(async move {
            let mut result = result?;
            match reply {
                AbandonReply::Lost => Err(CanonicalStoreError {
                    kind: CanonicalFailureKind::AdoptionConflict,
                    requested: CanonicalRequestedWrite::Unknown,
                    source: CanonicalSourceDisposition::NeedsReconcile,
                    detail: "injected lost acknowledgement reply".into(),
                }),
                AbandonReply::Foreign => {
                    result.operation = CanonicalOperation::new();
                    Ok(result)
                }
                AbandonReply::Exact => Ok(result),
            }
        })
    }
    fn register_run(
        &self,
        mut request: CanonicalLiveRunRequest,
    ) -> CanonicalFuture<CanonicalPrepared> {
        let mut store = self.store.lock().unwrap();
        match std::mem::take(&mut self.faults.lock().unwrap().run_start) {
            RunStartFault::None => {}
            RunStartFault::LedgerOnly(id) => {
                let before = store.canonical_snapshot().unwrap().checkpoint.cursor;
                store.register_delivered_user(&id).unwrap();
                let after = store.canonical_snapshot().unwrap();
                assert!(after.checkpoint.cursor.canonical_revision > before.canonical_revision);
                let mut expected = before;
                expected.canonical_revision = after.checkpoint.cursor.canonical_revision;
                assert_eq!(after.checkpoint.cursor, expected);
                assert_eq!(after.pending_users.front(), Some(&id));
            }
            RunStartFault::OwnedAdvance => {
                store
                    .persist_owner_metadata(
                        "custom",
                        json!({"customType":"child_finished", "data":{}}),
                    )
                    .unwrap();
            }
            RunStartFault::ExternalAdvance => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&store.path)
                    .unwrap()
                    .write_all(b"\n")
                    .unwrap();
            }
            RunStartFault::ForeignOwner => request.expected.owner = CanonicalOwnerScope::new(),
            RunStartFault::ForgedPrefix => request.expected.frontier.prefix_certificate[0] ^= 1,
            RunStartFault::ForgedPosition => request.expected.frontier.bytes -= 1,
            RunStartFault::ForgedLeaf => {
                request.expected.selected_leaf = Some("not-an-owner-row".into());
            }
            RunStartFault::ForgedGeneration => request.expected.raw_source_generation[0] ^= 1,
        }
        let result = store.register_canonical_run(request);
        if let Ok(prepared) = &result {
            self.faults.lock().unwrap().opening_operation = Some(prepared.operation.clone());
        }
        Box::pin(async move { result })
    }
    fn admit_user(
        &self,
        request: CanonicalLiveUserRequest,
    ) -> CanonicalFuture<CanonicalLiveUserPrepared> {
        let result = self.store.lock().unwrap().admit_canonical_user(request);
        Box::pin(async move { result })
    }
    fn admit_custom(
        &self,
        request: CanonicalLiveCustomRequest,
    ) -> CanonicalFuture<CanonicalLiveCustomPrepared> {
        let result = self.store.lock().unwrap().admit_canonical_custom(request);
        Box::pin(async move { result })
    }
    fn begin_provider_step(
        &self,
        request: CanonicalLiveProviderStepRequest,
    ) -> CanonicalFuture<()> {
        let mut store = self.store.lock().unwrap();
        if std::mem::take(&mut self.faults.lock().unwrap().advance_at_provider) {
            store
                .persist_owner_metadata("custom", json!({"customType":"child_finished", "data":{}}))
                .unwrap();
        }
        let result = store.begin_canonical_provider_step(request, false);
        Box::pin(async move { result })
    }
    fn admit_tool(&self, request: CanonicalLiveToolRequest) -> CanonicalFuture<()> {
        let result = self.store.lock().unwrap().admit_canonical_tool(request);
        Box::pin(async move { result })
    }
}
async fn runtime() -> (
    tempfile::TempDir,
    Arc<RealOwner>,
    SessionEngine,
    Arc<pa_agent::scripted::ScriptedProvider>,
) {
    runtime_with_unregistered_user(false).await
}
async fn runtime_with_unregistered_user(
    seed: bool,
) -> (
    tempfile::TempDir,
    Arc<RealOwner>,
    SessionEngine,
    Arc<pa_agent::scripted::ScriptedProvider>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({
            "harnessRefinements":false, "enableBuiltinSkills":false, "prewarmIpythonKernel":false,
            "contextStrategy":"default_context/rolling"
        })
        .to_string(),
    )
    .unwrap();
    let mut store = SessionFile::create(cwd.to_str().unwrap(), None, 0);
    store.set_path(dir.path().join("session.jsonl"));
    store.rewrite().unwrap();
    store.lease = Some(Arc::new(
        crate::lease::acquire_runtime_session_lease(&store.path, dir.path()).unwrap(),
    ));
    if seed {
        store
            .persist_entry(
                "message",
                json!({"message": {
                    "role":"user", "content":[{"type":"text","text":"ledger A"}], "timestamp":1,
                }}),
            )
            .unwrap();
    }
    store.attach_canonical_owner().unwrap();
    let manager = mirror(&store);
    let owner = Arc::new(RealOwner {
        store: Mutex::new(store),
        faults: Mutex::new(Faults::default()),
    });
    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id":"fake","name":"fake","api":"openai-completions","provider":"fake",
        "baseUrl":"http://127.0.0.1:9/v1","reasoning":false,"input":["text"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100_000,"maxTokens":100
    })).unwrap();
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
    provider.push_text_turn("next prompt succeeds");
    provider.push_text_turn("follow-up succeeds");
    let selection_owner = owner.clone();
    let engine = create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        session_manager: Some(manager),
        canonical_live_owner: Some(owner.clone()),
        on_initial_context_selection: Some(Arc::new(move |selection| {
            let result = selection_owner
                .store
                .lock()
                .unwrap()
                .persist_owner_metadata(
                    "custom",
                    json!({"customType":"context_strategy_selection","data":selection}),
                );
            Box::pin(async move {
                result?;
                Ok(())
            })
        })),
        stream_fn: Some(provider.stream_fn()),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    (dir, owner, engine, provider)
}

#[tokio::test]
async fn run_start_owned_advance_leaves_next_real_owner_prompt_ready() {
    let (_dir, owner, engine, provider) = runtime().await;
    owner.faults.lock().unwrap().run_start = RunStartFault::OwnedAdvance;
    engine
        .session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    engine
        .session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    engine
        .session
        .prompt("next", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 2);
    engine
        .session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
}

#[tokio::test]
async fn run_opening_honors_owner_api_completed_ledger_only_advance() {
    let (_dir, owner, engine, provider) = runtime_with_unregistered_user(true).await;
    let id = owner
        .store
        .lock()
        .unwrap()
        .entries()
        .iter()
        .find(|entry| entry.type_ == "message" && entry.fields["message"]["role"] == "user")
        .unwrap()
        .id
        .clone();
    let before = std::fs::read(&owner.store.lock().unwrap().path).unwrap();
    owner.faults.lock().unwrap().run_start = RunStartFault::LedgerOnly(id);
    engine
        .session
        .prompt("ledger A", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    assert!(owner
        .store
        .lock()
        .unwrap()
        .canonical_snapshot()
        .unwrap()
        .pending_users
        .is_empty());
    assert!(std::fs::read(&owner.store.lock().unwrap().path)
        .unwrap()
        .starts_with(&before));
    engine
        .session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
}

#[tokio::test]
async fn provider_owned_advance_leaves_next_real_owner_prompt_ready() {
    let (_dir, owner, engine, provider) = runtime().await;
    owner.faults.lock().unwrap().advance_at_provider = true;
    assert!(engine
        .session
        .prompt("first", PromptOptions::default())
        .await
        .is_err());
    assert!(provider.calls().is_empty());
    let path = owner.store.lock().unwrap().path.clone();
    let durable = SessionFile::open(&path).unwrap();
    let errors: Vec<pa_types::ai::AssistantMessage> = durable
        .entries()
        .iter()
        .filter_map(|row| {
            let message = row.fields.get("message")?;
            (message.get("role")?.as_str()? == "assistant"
                && message.get("stopReason")?.as_str()? == "error")
                .then(|| serde_json::from_value(message.clone()).unwrap())
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].usage, pa_types::ai::Usage::default());
    assert!(errors[0].diagnostics.as_ref().unwrap()[0].error.is_some());
    engine
        .session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    engine
        .session
        .prompt("next", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn prepared_context_owned_advance_requires_exact_abandon_before_next_prompt() {
    for reply in [
        AbandonReply::Exact,
        AbandonReply::Lost,
        AbandonReply::Foreign,
    ] {
        let (_dir, owner, engine, provider) = runtime().await;
        let live = engine.session.canonical_live_session().unwrap();
        let ticket = live
            .prepare_mutation(CanonicalMutationKind::ContextPolicy)
            .await
            .unwrap();
        owner
            .store
            .lock()
            .unwrap()
            .persist_owner_metadata("custom", json!({"customType":"child_finished", "data":{}}))
            .unwrap();
        let before = std::fs::read(&owner.store.lock().unwrap().path).unwrap();
        owner.faults.lock().unwrap().abandon_reply = reply;
        let draft = CanonicalEntryDraft::new(
            CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(json!({
                "type":"compaction", "summary":"paid once", "firstKeptEntryId":"x", "tokensBefore":1
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(ticket.commit(draft).await.is_err());
        assert_eq!(
            std::fs::read(&owner.store.lock().unwrap().path).unwrap(),
            before
        );
        assert!(provider.calls().is_empty());
        match reply {
            AbandonReply::Lost | AbandonReply::Foreign => assert!(live.ensure_ready().is_err()),
            AbandonReply::Exact => {
                live.ensure_ready().unwrap();
                engine
                    .session
                    .prompt("next", PromptOptions::default())
                    .await
                    .unwrap();
                assert_eq!(provider.calls().len(), 1);
            }
        }
    }
}

#[tokio::test]
async fn prepared_context_unowned_source_change_stays_blocked() {
    use std::io::Write;
    let (_dir, owner, engine, provider) = runtime().await;
    let live = engine.session.canonical_live_session().unwrap();
    let ticket = live
        .prepare_mutation(CanonicalMutationKind::ContextPolicy)
        .await
        .unwrap();
    let path = owner.store.lock().unwrap().path.clone();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let draft = CanonicalEntryDraft::new(
        CanonicalMutationKind::ContextPolicy,
        serde_json::from_value(json!({
            "type":"compaction", "summary":"never commit", "firstKeptEntryId":"x", "tokensBefore":1
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(ticket.commit(draft).await.is_err());
    assert!(live.ensure_ready().is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn run_start_unowned_source_or_foreign_owner_stays_blocked() {
    for fault in [
        RunStartFault::ExternalAdvance,
        RunStartFault::ForeignOwner,
        RunStartFault::ForgedPrefix,
        RunStartFault::ForgedPosition,
        RunStartFault::ForgedLeaf,
        RunStartFault::ForgedGeneration,
    ] {
        let (_dir, owner, engine, provider) = runtime().await;
        owner.faults.lock().unwrap().run_start = fault;
        assert!(engine
            .session
            .prompt("first", PromptOptions::default())
            .await
            .is_err());
        assert!(engine
            .session
            .canonical_live_session()
            .unwrap()
            .ensure_ready()
            .is_err());
        assert!(provider.calls().is_empty());
    }
}

#[tokio::test]
async fn precomputed_metadata_source_advance_is_certified_after_exact_abandon() {
    for reply in [
        AbandonReply::Exact,
        AbandonReply::Lost,
        AbandonReply::Foreign,
    ] {
        let (_dir, owner, engine, provider) = runtime().await;
        let live = engine.session.canonical_live_session().unwrap();
        live.observe().await.unwrap();
        let expected = owner
            .store
            .lock()
            .unwrap()
            .canonical_snapshot()
            .unwrap()
            .checkpoint
            .cursor;
        owner
            .store
            .lock()
            .unwrap()
            .persist_owner_metadata("custom", json!({"customType":"child_finished", "data":{}}))
            .unwrap();
        owner.faults.lock().unwrap().abandon_reply = reply;
        let before = std::fs::read(&owner.store.lock().unwrap().path).unwrap();
        let Err(error) = CanonicalMetadataWriter::append_metadata_from(
            live.clone(),
            custom_draft().into_entry(),
            expected,
        )
        .await
        else {
            panic!("stale metadata intent must not be appended");
        };
        assert_eq!(
            std::fs::read(&owner.store.lock().unwrap().path).unwrap(),
            before
        );
        match reply {
            AbandonReply::Exact => {
                assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
                assert_eq!(
                    error.source,
                    CanonicalSourceDisposition::ReadyForObservation
                );
                live.ensure_ready().unwrap();
                engine
                    .session
                    .prompt("next", PromptOptions::default())
                    .await
                    .unwrap();
                assert_eq!(provider.calls().len(), 1);
            }
            AbandonReply::Lost | AbandonReply::Foreign => {
                assert!(live.ensure_ready().is_err());
                assert_ne!(
                    error.source,
                    CanonicalSourceDisposition::ReadyForObservation
                );
                assert!(provider.calls().is_empty());
            }
        }
    }
}

#[test]
fn lifecycle_diagnostic_round_trips_through_canonical_error_schema() {
    let diagnostic = pa_agent::types::assistant_message_diagnostic(
        "agent_lifecycle_failure",
        &anyhow::anyhow!("inner").context("outer"),
        Some(json!({"source":"run_with_lifecycle"})),
    );
    let actual: pa_types::ai::AssistantMessageDiagnostic =
        serde_json::from_value(serde_json::to_value(&diagnostic).unwrap()).unwrap();
    assert_eq!(
        actual,
        pa_types::ai::AssistantMessageDiagnostic {
            type_: "agent_lifecycle_failure".into(),
            timestamp: diagnostic.timestamp.try_into().unwrap(),
            error: Some(pa_types::ai::DiagnosticErrorInfo {
                name: None,
                message: "outer: inner".into(),
                stack: None,
                code: None,
                rest: serde_json::Map::default(),
            }),
            details: Some(serde_json::from_value(json!({"source":"run_with_lifecycle"})).unwrap()),
            rest: serde_json::Map::default(),
        }
    );
}

#[tokio::test]
async fn committed_metadata_with_lost_receipt_or_ack_stays_blocked_without_replay() {
    for lose_append_reply in [true, false] {
        let (_dir, owner, engine, provider) = runtime().await;
        let live = engine.session.canonical_live_session().unwrap();
        let before = owner.store.lock().unwrap().entries().len();
        {
            let mut faults = owner.faults.lock().unwrap();
            faults.lose_append_reply = lose_append_reply;
            faults.lose_applied_ack = !lose_append_reply;
        }
        assert!(live
            .append(
                CanonicalMutationKind::OwnerMetadata,
                custom_draft().into_entry()
            )
            .await
            .is_err());
        assert!(live.ensure_ready().is_err());
        let path = owner.store.lock().unwrap().path.clone();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            SessionFile::open(&path).unwrap().entries().len(),
            before + 1
        );
        assert!(owner.store.lock().unwrap().canonical_snapshot().is_err());
        assert!(live
            .append(
                CanonicalMutationKind::OwnerMetadata,
                custom_draft().into_entry()
            )
            .await
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(provider.calls().is_empty());
    }
}

#[tokio::test]
async fn run_opening_lost_or_foreign_prepared_ack_never_reopens_or_replays() {
    for (reply, foreign_request) in [
        (AbandonReply::Lost, false),
        (AbandonReply::Foreign, false),
        (AbandonReply::Exact, true),
    ] {
        let (_dir, owner, engine, provider) = runtime().await;
        owner
            .store
            .lock()
            .unwrap()
            .persist_delivered_user(json!({
                "role":"user", "content":[{"type":"text","text":"accepted A"}], "timestamp":1,
            }))
            .unwrap();
        {
            let mut faults = owner.faults.lock().unwrap();
            faults.run_start = RunStartFault::OwnedAdvance;
            faults.run_prepared_reply = reply;
            faults.foreign_run_ack = foreign_request;
        }
        assert!(engine
            .session
            .prompt("accepted A", PromptOptions::default())
            .await
            .is_err());
        let live = engine.session.canonical_live_session().unwrap();
        assert!(live.ensure_ready().is_err());
        assert!(provider.calls().is_empty());
        let bytes = {
            let store = owner.store.lock().unwrap();
            assert!(store.canonical_snapshot().is_err());
            std::fs::read(&store.path).unwrap()
        };
        assert!(engine
            .session
            .prompt("different B", PromptOptions::default())
            .await
            .is_err());
        assert_eq!(
            std::fs::read(&owner.store.lock().unwrap().path).unwrap(),
            bytes
        );
        assert!(provider.calls().is_empty());
    }
}

#[tokio::test]
async fn accepted_user_survives_run_start_source_advance_before_a_different_followup() {
    let (_dir, owner, engine, provider) = runtime().await;
    owner
        .store
        .lock()
        .unwrap()
        .persist_delivered_user(json!({
            "role":"user", "content":[{"type":"text","text":"accepted A"}], "timestamp":1,
        }))
        .unwrap();
    owner.faults.lock().unwrap().run_start = RunStartFault::OwnedAdvance;
    engine
        .session
        .prompt("accepted A", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    owner
        .store
        .lock()
        .unwrap()
        .persist_delivered_user(json!({
            "role":"user", "content":[{"type":"text","text":"different B"}], "timestamp":2,
        }))
        .unwrap();
    engine
        .session
        .prompt("different B", PromptOptions::default())
        .await
        .unwrap();
    engine
        .session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    assert!(owner
        .store
        .lock()
        .unwrap()
        .canonical_snapshot()
        .unwrap()
        .pending_users
        .is_empty());
    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    let first = serde_json::to_value(&calls[0].messages)
        .unwrap()
        .to_string();
    let second = serde_json::to_value(&calls[1].messages)
        .unwrap()
        .to_string();
    assert_eq!(first.matches("accepted A").count(), 1);
    assert_eq!(first.matches("different B").count(), 0);
    assert_eq!(second.matches("accepted A").count(), 1);
    assert_eq!(second.matches("different B").count(), 1);
    let store = owner.store.lock().unwrap();
    let durable_users = store
        .entries()
        .iter()
        .filter(|entry| entry.type_ == "message" && entry.fields["message"]["role"] == "user")
        .map(|entry| entry.fields["message"].to_string())
        .collect::<Vec<_>>();
    assert_eq!(durable_users.len(), 2);
    assert_eq!(
        durable_users
            .iter()
            .filter(|row| row.contains("accepted A"))
            .count(),
        1
    );
    assert_eq!(
        durable_users
            .iter()
            .filter(|row| row.contains("different B"))
            .count(),
        1
    );
}
