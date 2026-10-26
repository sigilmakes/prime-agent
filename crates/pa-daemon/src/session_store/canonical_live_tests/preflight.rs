use super::*;
use pa_core::session::canonical::{CanonicalRequestedWrite, CanonicalSourceDisposition};

#[test]
fn owned_advance_refuses_unstarted_prepare_and_delivery_without_adoption_debt() {
    let (_dir, mut store) = fixture();
    let old = store.attach_canonical_owner().unwrap();
    let id = store
        .persist_delivered_custom(
            json!({"customType":"agent_message","content":"child done","display":true}),
        )
        .unwrap();
    let exact = std::fs::read(&store.path).unwrap();
    let mut prepare = request(CanonicalPrepareMode::Mutation(
        CanonicalMutationKind::OwnerMetadata,
    ));
    prepare.expected = Some(old.clone());
    let error = store.prepare_canonical(prepare).err().unwrap();
    assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
    assert_eq!(
        error.source,
        CanonicalSourceDisposition::ReadyForObservation
    );
    let pa_types::session::AgentMessage::User(user) =
        serde_json::from_value(json!({"role":"user","content":"next","timestamp":1})).unwrap()
    else {
        unreachable!()
    };
    let error = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(old.clone()),
            source: CanonicalSourceRequest::Full,
            message: user,
        })
        .err()
        .unwrap();
    assert_eq!(
        error.source,
        CanonicalSourceDisposition::ReadyForObservation
    );
    assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
    let error = store.admit_canonical_custom(pa_core::session_engine::canonical_live::CanonicalLiveCustomRequest {
        expected: Some(old),
        source: CanonicalSourceRequest::Full,
        message: serde_json::from_value(json!({"customType":"agent_message","content":"child done","display":true,"timestamp":1})).unwrap(),
    }).err().unwrap();
    assert_eq!(
        error.source,
        CanonicalSourceDisposition::ReadyForObservation
    );
    assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
    let snapshot = store.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot.pending_custom,
        std::collections::VecDeque::from([id])
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), exact);
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    adopt_prepared(&mut store, &mut manager, prepared).complete();
    assert_eq!(std::fs::read(&store.path).unwrap(), exact);
}

#[test]
fn preflight_refusal_never_certifies_changed_owner_or_unacknowledged_work() {
    for pending in [false, true] {
        let (_dir, mut store) = fixture();
        let mut cursor = store.attach_canonical_owner().unwrap();
        let prepared = if pending {
            Some(
                store
                    .prepare_canonical(request(CanonicalPrepareMode::Mutation(
                        CanonicalMutationKind::OwnerMetadata,
                    )))
                    .unwrap(),
            )
        } else {
            cursor.owner = pa_core::session::canonical::CanonicalOwnerScope::new();
            None
        };
        let bytes = std::fs::read(&store.path).unwrap();
        let mut next = request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        ));
        next.expected = Some(cursor);
        let error = store.prepare_canonical(next).err().unwrap();
        assert_ne!(
            error.source,
            CanonicalSourceDisposition::ReadyForObservation
        );
        assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        drop(prepared);
    }
}
