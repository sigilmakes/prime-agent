use super::*;

#[tokio::test]
async fn owned_advance_between_snapshot_and_prepare_or_delivery_keeps_next_operation_ready() {
    enum Request {
        Mutation,
        User,
        Custom,
    }
    for request in [Request::Mutation, Request::User, Request::Custom] {
        let owner = Owner::new();
        let (agent_session, provider) = actual_loop(owner.clone(), None).await;
        let session = agent_session.session_handle().clone();
        let live = agent_session.canonical_live_session().unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        {
            let mut state = owner.0.lock().unwrap();
            state.snapshot_gate = Some(gate.clone());
            state.snapshot_started = started.clone();
        }
        let work_live = live.clone();
        let work = tokio::spawn(async move {
            match request {
                Request::Mutation => work_live.prepare_mutation(CanonicalMutationKind::OwnerMetadata).await.map(|_| ()),
                Request::User => work_live.admit_user(tagged_user_intent(serde_json::json!({"role":"user","content":[{"type":"text","text":"next"}],"timestamp":0})).unwrap()).await.map(|_| ()),
                Request::Custom => work_live.admit_custom(tagged_custom_intent(serde_json::json!({"role":"custom","customType":"test","content":"next","display":false,"timestamp":0})).unwrap()).await.map(|_| ()),
            }
        });
        started.notified().await;
        let id = owner.seed(serde_json::json!({"type":"custom","customType":"owned-advance","data":{"exact":"AA=="}}));
        let prefix: Vec<_> = owner
            .0
            .lock()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.raw().to_vec())
            .collect();
        owner.0.lock().unwrap().snapshot_gate = None;
        gate.notify_one();
        let error = work.await.unwrap().unwrap_err();
        assert_eq!(
            error.source,
            CanonicalSourceDisposition::ReadyForObservation
        );
        assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
        live.ensure_ready().unwrap();
        assert!(owner.0.lock().unwrap().acks.is_empty());
        live.observe().await.unwrap();
        assert_eq!(
            session
                .lock()
                .await
                .canonical_binding()
                .unwrap()
                .selected_leaf
                .as_deref(),
            Some(id.as_str())
        );
        live.append(CanonicalMutationKind::OwnerMetadata, metadata())
            .await
            .unwrap();
        live.ensure_ready().unwrap();
        {
            let state = owner.0.lock().unwrap();
            assert_eq!(state.appends, 1);
            assert_eq!(state.rows.len(), prefix.len() + 1);
            for (before, after) in prefix.iter().zip(&state.rows) {
                assert_eq!(before.as_slice(), after.raw());
            }
        }
        assert!(provider.calls().is_empty());
        let user_id = owner.seed(serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"next prompt"}],"timestamp":7}}));
        owner
            .0
            .lock()
            .unwrap()
            .delivered_users
            .push_back(user_id.clone());
        agent_session
            .prompt("next prompt", super::super::super::PromptOptions::default())
            .await
            .unwrap();
        assert_eq!(provider.calls().len(), 1);
        live.ensure_ready().unwrap();
        let state = owner.0.lock().unwrap();
        assert!(state.delivered_users.is_empty());
        assert!(state.admitted_users.contains(&user_id));
        assert_eq!(
            state
                .rows
                .iter()
                .filter(|row| row.entry().id() == Some(user_id.as_str()))
                .count(),
            1
        );
        for (before, after) in prefix.iter().zip(&state.rows) {
            assert_eq!(before.as_slice(), after.raw());
        }
    }
}

#[tokio::test]
async fn cancelled_provider_preflight_keeps_next_same_session_prompt_ready() {
    let owner = Owner::new();
    let input = serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"first"}],"timestamp":7}});
    let id = owner.seed(input);
    owner.0.lock().unwrap().delivered_users.push_back(id);
    owner.0.lock().unwrap().cancel_next_provider = true;
    let (session, provider) = actual_loop(owner.clone(), None).await;
    let error = session
        .prompt("first", super::super::super::PromptOptions::default())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("provider cancelled before work"),
        "{error:#}"
    );
    assert!(!owner.0.lock().unwrap().cancel_next_provider);
    assert!(provider.calls().is_empty());
    assert_eq!(owner.0.lock().unwrap().steps, 0);
    session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
    let input = serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"next"}],"timestamp":8}});
    let id = owner.seed(input);
    owner.0.lock().unwrap().delivered_users.push_back(id);
    session
        .prompt("next", super::super::super::PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(provider.calls().len(), 1);
    session
        .canonical_live_session()
        .unwrap()
        .ensure_ready()
        .unwrap();
}

#[test]
fn unproven_refusal_never_reopens_flight() {
    for requested in [
        CanonicalRequestedWrite::NotWritten,
        CanonicalRequestedWrite::Unknown,
        CanonicalRequestedWrite::Committed,
    ] {
        let owner = Owner::new();
        let live = CanonicalLiveSession::new(owner.clone(), manager(&owner));
        let mut flight = live.enter().unwrap();
        let _ = flight.refused(CanonicalStoreError {
            kind: CanonicalFailureKind::SourceChanged,
            requested,
            source: CanonicalSourceDisposition::ReadyForObservation,
            detail: "not a proven unstarted refusal".into(),
        });
        drop(flight);
        assert!(live.ensure_ready().is_err());
    }
}
