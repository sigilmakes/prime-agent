use super::tree::{Cache, Tree};
use pa_types::session::FileEntry;
use serde_json::json;

pub(super) fn history(count: usize) -> Vec<FileEntry> {
    (0..count).map(|i| serde_json::from_value(json!({
        "type":"message", "id":format!("e{i}"), "parentId":i.checked_sub(1).map(|n|format!("e{n}")),
        "timestamp":"2026-10-19T10:00:00Z",
        "message":{"role":"user","content":format!("original {i}: {}", "detail ".repeat(80)),"timestamp":i}
    })).unwrap()).collect()
}
fn tree(source: Vec<FileEntry>, cache: Cache) -> Tree {
    let branch = source.last().and_then(FileEntry::id).map(str::to_owned);
    Tree::new("session", source, branch, cache, 128).unwrap()
}
fn summarize(tree: &mut Tree) -> usize {
    let mut count = 0;
    loop {
        let plan = tree.plan(64).unwrap();
        if plan.is_empty() {
            break;
        }
        for merge in plan {
            tree.accept(&merge, "bounded summary of both source children".into())
                .unwrap();
            count += 1;
        }
    }
    count
}

#[test]
fn complete_bounded_view_balanced_merges_and_exact_recall() {
    let original = history(128);
    let mut t = tree(original.clone(), Cache::default());
    assert!(t.view(4096).unwrap_err().to_string().contains("pending"));
    assert_eq!(summarize(&mut t), 127);
    let view = t.view(4096).unwrap();
    assert!(view.bytes <= 4096);
    assert_eq!(view.bytes, view.text.len());
    assert_eq!(view.entries, original.len());
    let recalled: Vec<_> = view
        .spans
        .iter()
        .flat_map(|span| t.recall(&span.id, 0, 256).unwrap())
        .collect();
    assert_eq!(recalled, original);
    assert_eq!(view.spans.first().unwrap().start, 0);
    assert_eq!(view.spans.last().unwrap().end, original.len());
    assert!(view.spans.first().unwrap().summarized);
    assert!(!view.spans.last().unwrap().summarized);
    assert_eq!(summarize(&mut t), 0);
}

#[test]
fn descendants_reuse_nodes_and_siblings_cannot_recall_each_other() {
    let original = history(64);
    let mut t = tree(original.clone(), Cache::default());
    assert_eq!(summarize(&mut t), 63);
    let cache = t.cache().clone();
    let mut appended = tree(history(66), cache.clone());
    assert_eq!(summarize(&mut appended), 1);
    let old_id = t.view(4096).unwrap().spans[0].id.clone();
    assert!(!appended.recall(&old_id, 0, 256).unwrap().is_empty());
    let mut sibling_source = original[..32].to_vec();
    let sibling: FileEntry = serde_json::from_value(json!({"type":"custom","id":"sibling","parentId":"e31","timestamp":"2026-10-19T10:00:00Z","customType":"event","data":{"exact":"retained"}})).unwrap();
    sibling_source.push(sibling.clone());
    let mut all = original;
    all.push(sibling);
    let mut sibling = Tree::new("session", all, Some("sibling".into()), cache, 128).unwrap();
    summarize(&mut sibling);
    let view = sibling.view(4096).unwrap();
    let exact: Vec<_> = view
        .spans
        .iter()
        .flat_map(|span| sibling.recall(&span.id, 0, 256).unwrap())
        .collect();
    assert_eq!(exact, sibling_source);
    let foreign_id = appended.search("original 65", None, 1).unwrap()[0]
        .id
        .clone();
    assert!(sibling.recall(&foreign_id, 0, 256).is_err());
}

#[test]
fn source_hash_child_hash_summary_and_budget_are_validated() {
    let original = history(8);
    let mut t = tree(original.clone(), Cache::default());
    let request = t.plan(1).unwrap().remove(0);
    let mut corrupt = request.clone();
    corrupt.source_hash = "wrong source".into();
    assert!(t.accept(&corrupt, "summary".into()).is_err());
    corrupt = request.clone();
    corrupt.right = corrupt.left.clone();
    assert!(t.accept(&corrupt, "summary".into()).is_err());
    assert!(t.accept(&request, "x".repeat(129)).is_err());
    t.accept(&request, "correct summary".into()).unwrap();
    let mut cache = t.cache().clone();
    cache.summaries.get_mut(&request.id).unwrap().text = "forged summary".into();
    assert!(Tree::new("session", original.clone(), Some("e7".into()), cache, 128).is_err());
    let mut changed = original;
    if let FileEntry::Message { message, .. } = &mut changed[0] {
        *message = serde_json::from_value(
            json!({"role":"user","content":"different original","timestamp":0}),
        )
        .unwrap();
    }
    let mut changed = tree(changed, t.cache().clone());
    assert!(changed.accept(&request, "stale summary".into()).is_err());
    assert_ne!(changed.source_hash(), t.source_hash());
    let mut complete = tree(history(8), Cache::default());
    summarize(&mut complete);
    let mut cache = complete.cache().clone();
    let parent = cache
        .summaries
        .values_mut()
        .find(|summary| summary.left.starts_with("node:"))
        .unwrap();
    parent.left_digest = "forged child presentation".into();
    assert!(Tree::new("session", history(8), Some("e7".into()), cache, 128).is_err());
}

#[test]
fn canonical_tool_custom_date_search_and_fresh_session_isolation() {
    let mut source = history(3);
    source.push(serde_json::from_value(json!({"type":"message","id":"tool","parentId":"e2","timestamp":"2026-10-20T10:00:00Z","message":{"role":"toolResult","toolCallId":"call","toolName":"bash","content":[{"type":"text","text":"exact tool output"}],"isError":false,"timestamp":0}})).unwrap());
    source.push(serde_json::from_value(json!({"type":"custom","id":"custom","parentId":"tool","timestamp":"2026-10-21T10:00:00Z","customType":"event","data":{"secret":"exact original custom"}})).unwrap());
    let mut t = tree(source.clone(), Cache::default());
    summarize(&mut t);
    for term in ["exact tool output", "exact original custom"] {
        let hit = t.search(term, Some("2026-10-20"), 1).unwrap().remove(0);
        assert_eq!(
            t.recall(&hit.id, 0, 1).unwrap(),
            vec![source[hit.start].clone()]
        );
    }
    assert!(t
        .search("original 0", Some("2026-10-20"), 1)
        .unwrap()
        .is_empty());
    assert!(Tree::new(
        "different-session",
        history(5),
        Some("e4".into()),
        t.cache().clone(),
        128
    )
    .is_err());
    let fresh = Tree::new("fresh", vec![], None, Cache::default(), 128).unwrap();
    assert_eq!(fresh.view(4096).unwrap().entries, 0);
    assert!(fresh
        .recall(&t.view(4096).unwrap().spans[0].id, 0, 1)
        .is_err());
}

#[test]
fn missing_source_and_cycles_fail_instead_of_partial_coverage() {
    let mut missing = history(3);
    missing.remove(1);
    assert!(Tree::new("session", missing, Some("e2".into()), Cache::default(), 128).is_err());
    let cycle: FileEntry = serde_json::from_value(json!({"type":"custom","id":"cycle","parentId":"cycle","timestamp":"2026-10-19T10:00:00Z","customType":"event","data":{}})).unwrap();
    assert!(Tree::new(
        "session",
        vec![cycle],
        Some("cycle".into()),
        Cache::default(),
        128
    )
    .is_err());
    let mut t = tree(history(2), Cache::default());
    summarize(&mut t);
    assert!(t.view(10).is_err());
}

#[test]
fn typed_multimodal_projection_rejects_without_disabling_exact_recall() {
    let image = json!({"type":"image","data":"AA==","mimeType":"image/png"});
    let messages = [
        json!({"role":"user","content":[image],"timestamp":0}),
        json!({"role":"toolResult","toolCallId":"image-call","toolName":"image-tool","content":[image],"isError":false,"timestamp":0}),
        json!({"role":"custom","customType":"image","content":[image],"display":true,"timestamp":0}),
        json!({"role":"user","content":[{"type":"input_audio","data":"AA=="}],"timestamp":0}),
    ];
    for message in messages {
        let original:FileEntry=serde_json::from_value(json!({"type":"message","id":"image","parentId":null,"timestamp":"2026-10-20T10:00:00Z","message":message})).unwrap();
        let t = tree(vec![original.clone()], Cache::default());
        assert!(t
            .view(4096)
            .unwrap_err()
            .to_string()
            .contains("default_context/rolling"));
        assert!(t.plan(1).unwrap_err().to_string().contains("text/JSON"));
        let hit = t.search("AA==", None, 1).unwrap().remove(0);
        assert_eq!(t.recall(&hit.id, 0, 1).unwrap(), vec![original]);
    }
    let custom:FileEntry=serde_json::from_value(json!({"type":"custom_message","id":"image","parentId":null,"timestamp":"2026-10-20T10:00:00Z","customType":"image","content":[image],"display":true})).unwrap();
    assert!(tree(vec![custom], Cache::default()).view(4096).is_err());
    let mut text = history(1);
    if let FileEntry::Message { message, .. } = &mut text[0] {
        *message=serde_json::from_value(json!({"role":"user","content":"literal JSON {\"type\":\"image\",\"data\":\"AA==\"} is ordinary text","timestamp":0})).unwrap();
    }
    assert!(tree(text.clone(), Cache::default()).view(4096).is_ok());
    // Unsupported content on an unselected sibling does not poison this branch.
    let sibling:FileEntry=serde_json::from_value(json!({"type":"message","id":"foreign-image","parentId":null,"timestamp":"2026-10-20T10:00:00Z","message":{"role":"user","content":[{"type":"image","data":"AA==","mimeType":"image/png"}],"timestamp":0}})).unwrap();
    text.push(sibling);
    assert!(
        Tree::new("session", text, Some("e0".into()), Cache::default(), 128)
            .unwrap()
            .view(4096)
            .is_ok()
    );
}

pub(super) fn signed_source_messages() -> Vec<(pa_types::session::AgentMessage, &'static str)> {
    use pa_types::session::AgentMessage;
    let base = pa_ai::faux::faux_assistant_text_message(
        "visible portable words",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    );
    let mut cases = Vec::new();
    let marker = "opaque-native-text-reference";
    let mut message = base.clone();
    message.content=serde_json::from_value(json!([{"type":"text","text":"visible portable words","textSignature":json!({"v":1,"id":marker,"phase":"final_answer"}).to_string()}])).unwrap();
    cases.push((AgentMessage::Assistant(message), marker));
    let marker = "opaque-native-reasoning-ciphertext==";
    let mut message = base.clone();
    let reasoning =
        json!({"type":"reasoning","id":"rs_native","summary":[],"encrypted_content":marker});
    message.content=serde_json::from_value(json!([{"type":"thinking","thinking":"visible summary","thinkingSignature":reasoning.to_string()}])).unwrap();
    cases.push((AgentMessage::Assistant(message), marker));
    let marker = "redacted-native-thinking-marker";
    let mut message = base.clone();
    message.content =
        serde_json::from_value(json!([{"type":"thinking","thinking":marker,"redacted":true}]))
            .unwrap();
    cases.push((AgentMessage::Assistant(message), marker));
    let marker = "opaque-native-tool-thought-signature==";
    let mut message = base.clone();
    message.content=serde_json::from_value(json!([{"type":"toolCall","id":"signed-call","name":"provider_tool","arguments":{},"thoughtSignature":marker}])).unwrap();
    cases.push((AgentMessage::Assistant(message), marker));
    for api in [
        "openai-responses",
        "azure-openai-responses",
        "openai-codex-responses",
        "unattested-native-api",
    ] {
        let marker = "opaque-native-response-reference";
        let mut message = base.clone();
        message.api = api.into();
        message.response_id = Some(marker.into());
        cases.push((AgentMessage::Assistant(message), marker));
    }
    cases
}

#[test]
fn typed_opaque_signatures_and_route_ids_reject_projection_but_preserve_exact_index() {
    for (message, marker) in signed_source_messages() {
        let original:FileEntry=serde_json::from_value(json!({"type":"message","id":"signed","parentId":null,"timestamp":"2026-10-20T10:00:00Z","message":message})).unwrap();
        assert!(
            matches!(original, FileEntry::Message { .. }),
            "fixture did not decode as typed source"
        );
        let tree = tree(vec![original.clone()], Cache::default());
        let error = tree.view(4096).unwrap_err().to_string();
        assert!(error.contains("opaque signatures") && error.contains("default_context/rolling"));
        assert!(!error.contains(marker));
        assert!(tree.plan(1).is_err());
        let hit = tree.search(marker, None, 1).unwrap().remove(0);
        assert_eq!(tree.recall(&hit.id, 0, 1).unwrap(), vec![original]);
    }
    let original:FileEntry=serde_json::from_value(json!({"type":"message","id":"ordinary","parentId":null,"timestamp":"2026-10-20T10:00:00Z","message":{"role":"user","content":"ordinary literal textSignature encrypted_content words are not typed provider state","timestamp":0}})).unwrap();
    assert!(tree(vec![original], Cache::default()).view(4096).is_ok());
    let mut completion = pa_ai::faux::faux_assistant_text_message(
        "plain completion",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    );
    completion.api = "openai-completions".into();
    completion.response_id = Some("ordinary-completion-log-id".into());
    let original:FileEntry=serde_json::from_value(json!({"type":"message","id":"ordinary","parentId":null,"timestamp":"2026-10-20T10:00:00Z","message":pa_types::session::AgentMessage::Assistant(completion)})).unwrap();
    assert!(tree(vec![original], Cache::default()).view(4096).is_ok());
}

#[test]
fn serialized_plan_bound_counts_metadata_and_defers_an_ordered_prefix() {
    use super::tree::{MAX_MERGE_INPUT_BYTES, MAX_PLAN_REPLY_BYTES};
    let ids = (0..128)
        .map(|index| format!("{index:04}{}", "\0".repeat(768)))
        .collect::<Vec<_>>();
    let source=(0..128).map(|index|serde_json::from_value(json!({"type":"message","id":ids[index],"parentId":index.checked_sub(1).map(|previous|&ids[previous]),"timestamp":"2026-10-20T10:00:00Z","message":{"role":"user","content":"portable small text","timestamp":0}})).unwrap()).collect::<Vec<FileEntry>>();
    let tree = tree(source, Cache::default());
    let all = tree
        .plan_with_limits(64, MAX_MERGE_INPUT_BYTES, usize::MAX)
        .unwrap();
    assert_eq!(all.len(), 64);
    assert!(
        all.iter().map(|merge| merge.input.len()).sum::<usize>() < MAX_PLAN_REPLY_BYTES,
        "fixture raw inputs alone already exceeded the limit"
    );
    assert!(
        serde_json::to_vec(&json!({"ready":false,"merges":all}))
            .unwrap()
            .len()
            > MAX_PLAN_REPLY_BYTES,
        "fixture did not exercise encoded metadata/escaping"
    );
    let bounded = tree.plan(64).unwrap();
    assert!(!bounded.is_empty() && bounded.len() < 64);
    assert_eq!(
        serde_json::to_value(&bounded).unwrap(),
        serde_json::to_value(&all[..bounded.len()]).unwrap()
    );
    assert!(
        serde_json::to_vec(&json!({"ready":false,"merges":bounded}))
            .unwrap()
            .len()
            <= MAX_PLAN_REPLY_BYTES
    );
    assert_eq!(
        serde_json::to_value(tree.plan(64).unwrap()).unwrap(),
        serde_json::to_value(&bounded).unwrap(),
        "planning mutated pending work"
    );
    let one = serde_json::to_vec(&json!({"ready":false,"merges":[&all[0]]}))
        .unwrap()
        .len();
    assert!(tree
        .plan_with_limits(64, MAX_MERGE_INPUT_BYTES, one - 1)
        .unwrap_err()
        .to_string()
        .contains("serialized single merge"));
}

#[test]
fn oversized_single_plan_input_fails_explicitly_before_any_reply() {
    let source=(0..2).map(|index|serde_json::from_value(json!({"type":"message","id":format!("large-{index}"),"parentId":if index==0 {None}else{Some("large-0")},"timestamp":"2026-10-20T10:00:00Z","message":{"role":"user","content":"x".repeat(300*1024),"timestamp":0}})).unwrap()).collect::<Vec<FileEntry>>();
    let tree = tree(source.clone(), Cache::default());
    let before = tree.source_hash().to_owned();
    let error = tree.plan(64).unwrap_err().to_string();
    assert!(error.contains("single merge input exceeds 524288 bytes"));
    assert_eq!(tree.source_hash(), before);
    let id = tree.search("xxxx", None, 1).unwrap().remove(0).id;
    assert_eq!(tree.recall(&id, 0, 1).unwrap(), vec![source[0].clone()]);
}
