//! Snap task continuity through the real engine, using a local faux provider.
use super::*;

#[tokio::test]
async fn snap_handoff_updates_across_two_compactions() {
    let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
    let faux = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some(format!("snap-handoff-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    });
    let mut model = faux.get_model();
    model.input.push(pa_types::ai::ModelInput::Image);
    std::fs::write(f.agent.join("settings.json"), json!({
        "harnessRefinements":false,"enableBuiltinSkills":false,
        "plugins":{
            "default_context":{"config":{"compactors":["snapcompact/snapcompact"]}},
            "snapcompact":{"config":{"profile":"explicit","frame_size":128,"frame_tokens":1}}
        }
    }).to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    f.engine.session.set_model(&model, &model.provider, &model.id).await.unwrap();
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    for summary in [
        "Objective: repair the fixture. Authorizations: tests only; no deploy. Completed: diagnosis. Running jobs: handle17. Blockers: none. Next action: run OLD_NEXT.",
        "Objective: repair the fixture. Authorizations: tests only; no deploy. Completed: diagnosis and tests. Running jobs: none. Blockers: none. Next action: inspect NEW_NEXT.",
    ] {
        let captured = captured.clone();
        faux.append_responses(vec![pa_ai::faux::FauxResponseStep::Factory(Arc::new(move |context, options, _, _| {
            assert_eq!(options.unwrap().max_tokens, Some(1024));
            captured.lock().unwrap().push(serde_json::to_string(context).unwrap());
            Ok(pa_ai::faux::faux_assistant_text_message(summary, pa_ai::faux::FauxAssistantMessageOptions::default()))
        }))]);
    }
    f.engine.session.shared_persistence().lock().await.append_message(user(
        "Repair the fixture. You may run tests only; do not deploy."
    )).unwrap();
    let mut latest = String::new();
    for (step, instruction) in [
        ("diagnosis complete; running job handle17; next run OLD_NEXT", "focus on the repair"),
        ("tests complete; handle17 finished; next inspect NEW_NEXT", "keep new next action"),
    ] {
        f.engine.session.shared_persistence().lock().await.append_message(user(step)).unwrap();
        let CompactOutcome::Ran(run) = f.engine.session.compact(Some(instruction), &model, None, None).await.unwrap() else { panic!("Snap declined") };
        assert!(run.result.usage.is_some());
        let archive = run.entry.context_archive.unwrap();
        let ContextRepresentation::Snapcompact { archive, .. } = archive.representation else { panic!("wrong compactor") };
        let body: Value = serde_json::from_str(&archive).unwrap();
        latest = body["handoff"].as_str().expect("handoff missing").to_owned();
        let view = serde_json::to_string(&f.engine.session.agent().state().await.messages).unwrap();
        assert_eq!(view.matches(&latest).count(), 1);
    }
    assert!(latest.contains("NEW_NEXT") && !latest.contains("OLD_NEXT"));
    assert!(latest.contains("tests only; no deploy"));
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("OLD_NEXT") && requests[1].contains("NEW_NEXT"));
    assert!(requests[1].contains("keep new next action"));
    assert!(requests[1].contains("authorizations") && requests[1].contains("Do not invent"));
    assert!(!requests[1].contains("base64") && !requests[1].contains("iVBOR"));
}
