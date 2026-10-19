use super::service::OptChatServices;
use crate::{
    plugins::{CapabilityRequest, HostCapabilities, PluginHost, PluginSource},
    session::manager::SessionManager,
};
use serde_json::{json, Value};
use sha2::Digest as _;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct NoCapabilities;
impl HostCapabilities for NoCapabilities {
    fn call(
        &self,
        _request: CapabilityRequest,
        _cancel: CancellationToken,
    ) -> crate::plugins::CapabilityFuture {
        Box::pin(async { Err("no default capabilities".into()) })
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    host: Arc<PluginHost>,
    service: OptChatServices,
    request: CapabilityRequest,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
        for row in super::tests::history(32) {
            if let pa_types::session::FileEntry::Message { message, .. } = row {
                session.append_message(message).unwrap();
            }
        }
        session.flush_now().unwrap();
        let id = session.get_session_id().to_owned();
        let artifact = session.get_session_artifact_dir().unwrap();
        let host = Arc::new(PluginHost::new(
            id.clone(),
            Arc::new(NoCapabilities),
            Vec::<String>::new(),
        ));
        let snapshot = host
            .reconcile(vec![PluginSource {
                name: "optchat".into(),
                source: "return {exports={{name='status',run=function()return {} end}}}".into(),
                config: json!({}),
            }])
            .unwrap();
        let generation = snapshot.contributions[0].generation;
        let session = Arc::new(tokio::sync::Mutex::new(session));
        let service = OptChatServices::new(session.clone(), &host, Some(artifact.clone()));
        Self {
            _dir: dir,
            session,
            host,
            service,
            request: CapabilityRequest {
                session: id,
                plugin: "optchat".into(),
                generation,
                invocation: uuid::Uuid::new_v4(),
                operation: String::new(),
                input: Value::Null,
            },
            path: artifact.join("plugins/optchat/tree-v1.json"),
        }
    }
    async fn call(&self, op: &str, input: Value) -> Result<Value, String> {
        let mut request = self.request.clone();
        request.operation = format!("optchat.{op}");
        request.input = input;
        self.service.call(request, CancellationToken::new()).await
    }
    async fn prepare(&self) {
        self.call("begin", json!({"budget_bytes":4096,"summary_bytes":128}))
            .await
            .unwrap();
        for _ in 0..64 {
            let plan = self.call("plan", json!({})).await.unwrap();
            if plan["ready"] == true {
                return;
            }
            for merge in plan["merges"].as_array().unwrap() {
                self.call(
                    "accept",
                    json!({"merge":merge,"text":"summary of both children"}),
                )
                .await
                .unwrap();
            }
        }
        panic!("bounded preparation did not finish")
    }
}
#[tokio::test]
async fn native_public_capability_roundtrip_durable_cache_and_exact_source() {
    let f = Fixture::new();
    let before = {
        let s = f.session.lock().await;
        std::fs::read(s.get_session_file().unwrap()).unwrap()
    };
    f.prepare().await;
    let view = f.call("finish", json!({})).await.unwrap();
    assert!(view["bytes"].as_u64().unwrap() <= 4096);
    assert_eq!(view["entries"], 32);
    let mut originals = Vec::new();
    for span in view["spans"].as_array().unwrap() {
        let recall = f
            .call("zoom", json!({"id":span["id"],"limit":256}))
            .await
            .unwrap();
        originals.extend(recall["entries"].as_array().unwrap().iter().cloned());
    }
    let snapshot = { f.session.lock().await.history_snapshot() }.await.unwrap();
    assert_eq!(
        Value::Array(originals),
        serde_json::to_value(
            snapshot
                .iter()
                .filter(|entry| !matches!(entry, pa_types::session::FileEntry::Header { .. }))
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
    let after = {
        let s = f.session.lock().await;
        std::fs::read(s.get_session_file().unwrap()).unwrap()
    };
    assert_eq!(after, before);
    assert!(f.path.exists());
    #[cfg(unix)]
    {
        assert_eq!(crate::platform::perms::file_mode(&f.path), Some(0o600));
        assert_eq!(
            crate::platform::perms::file_mode(f.path.parent().unwrap()),
            Some(0o700)
        );
    }
    // A new native service has no retained rows/tree state. It rebuilds from full
    // canonical source plus durable summary cache, without any summarizer calls.
    let artifact = f
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let resumed = OptChatServices::new(f.session.clone(), &f.host, Some(artifact));
    let mut request = f.request.clone();
    request.operation = "optchat.begin".into();
    request.input = json!({"budget_bytes":4096,"summary_bytes":128});
    resumed
        .call(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    request.operation = "optchat.plan".into();
    request.input = json!({});
    assert_eq!(
        resumed
            .call(request, CancellationToken::new())
            .await
            .unwrap()["ready"],
        true
    );
}
#[tokio::test]
async fn cancellation_retirement_branch_departure_and_persistence_failure_preserve_active_view() {
    for mode in ["cancel", "retire", "branch", "persist"] {
        let f = Fixture::new();
        f.prepare().await;
        f.call("finish", json!({})).await.unwrap();
        let previous = f.call("status", json!({})).await.unwrap();
        let bytes = std::fs::read(&f.path).unwrap();
        f.prepare().await;
        let mut request = f.request.clone();
        request.operation = "optchat.finish".into();
        request.input = json!({});
        let cancellation = CancellationToken::new();
        match mode {
            "cancel" => cancellation.cancel(),
            "retire" => {
                f.host.reconcile(vec![]).unwrap();
            }
            "branch" => {
                f.session
                    .lock()
                    .await
                    .append_message(
                        serde_json::from_value(
                            json!({"role":"user","content":"new tail survives","timestamp":0}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            "persist" => {
                std::fs::rename(&f.path, f.path.with_extension("saved")).unwrap();
                std::fs::create_dir(&f.path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            f.service.call(request, cancellation).await.is_err(),
            "{mode}"
        );
        if mode == "retire" {
            // Absence of every contribution is checked through the real host.
            assert!(f.host.snapshot().contributions.is_empty());
        } else {
            assert_eq!(
                f.call("status", json!({})).await.unwrap(),
                previous,
                "{mode}"
            );
        }
        if mode == "persist" {
            std::fs::remove_dir(&f.path).unwrap();
            std::fs::rename(f.path.with_extension("saved"), &f.path).unwrap();
        }
        assert_eq!(std::fs::read(&f.path).unwrap(), bytes, "{mode}");
    }
}

#[tokio::test]
async fn whole_call_budget_charges_exact_instructions_tools_and_output_before_layout() {
    let f = Fixture::new();
    let mut input = json!({"budget_bytes":4096,"summary_bytes":128,"phase":"model",
        "model":{"contextWindow":8192,"maxTokens":128},"instructions":"x".repeat(3000),
        "tools":[{"name":"tool","description":"y".repeat(500),"parameters":{"type":"object"}}],"output_tokens":256});
    f.call("begin", input.clone()).await.unwrap();
    for _ in 0..64 {
        let plan = f.call("plan", json!({})).await.unwrap();
        if plan["ready"] == true {
            break;
        }
        for merge in plan["merges"].as_array().unwrap() {
            f.call("accept", json!({"merge":merge,"text":"summary"}))
                .await
                .unwrap();
        }
    }
    let view = f.call("finish", json!({})).await.unwrap();
    let tools = serde_json::to_vec(&input["tools"]).unwrap().len();
    assert!(
        view["bytes"].as_u64().unwrap() + u64::try_from(3000 + tools + 256 + 1024).unwrap() <= 8192
    );
    input["instructions"] = json!("x".repeat(8192));
    assert!(f.call("begin", input).await.unwrap_err().contains("budget"));
}

#[derive(Default)]
struct PlanTrace {
    reply_bytes: Vec<usize>,
    max_input: usize,
    accepted: Vec<String>,
}
#[derive(Default)]
struct PlanRouter {
    service: std::sync::Mutex<Option<Arc<OptChatServices>>>,
    trace: Arc<std::sync::Mutex<PlanTrace>>,
}
impl HostCapabilities for PlanRouter {
    fn call(
        &self,
        request: CapabilityRequest,
        cancel: CancellationToken,
    ) -> crate::plugins::CapabilityFuture {
        let service = self.service.lock().unwrap().as_ref().unwrap().clone();
        let trace = self.trace.clone();
        let plan = request.operation == "optchat.plan";
        let accepted = (request.operation == "optchat.accept")
            .then(|| request.input["merge"]["id"].as_str().unwrap().to_owned());
        Box::pin(async move {
            let value = service.call(request, cancel).await?;
            if let Some(id) = accepted {
                trace.lock().unwrap().accepted.push(id);
            }
            if plan {
                let mut trace = trace.lock().unwrap();
                trace
                    .reply_bytes
                    .push(serde_json::to_vec(&value).unwrap().len());
                for merge in value["merges"].as_array().unwrap() {
                    trace.max_input = trace.max_input.max(merge["input"].as_str().unwrap().len());
                }
            }
            Ok(value)
        })
    }
}

#[tokio::test]
async fn actual_native_plan_reply_stays_within_lua_quota_without_shrinking_history() {
    use crate::plugins::{ContributionKind, InvocationEvent, InvocationOutcome};
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    for index in 0..128 {
        let message=serde_json::from_value(json!({"role":"user","content":format!("large-original-{index}:{}","x".repeat(200*1024)),"timestamp":0})).unwrap();
        manager.append_message(message).unwrap();
    }
    manager
        .append_message(pa_types::session::AgentMessage::Assistant(
            pa_ai::faux::faux_assistant_text_message(
                "small latest tail preserved verbatim",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        ))
        .unwrap();
    manager.flush_now().unwrap();
    let journal = manager.get_session_file().unwrap().to_owned();
    let before = sha2::Sha256::digest(std::fs::read(&journal).unwrap());
    let session_id = manager.get_session_id().to_owned();
    let artifact = manager.get_session_artifact_dir().unwrap();
    let branch = manager.get_leaf_id().map(str::to_owned);
    let source = manager.retained_entries().to_vec();
    let expected = super::tree::Tree::new(
        &session_id,
        source.clone(),
        branch,
        super::tree::Cache::default(),
        128,
    )
    .unwrap();
    let expected_hash = expected.source_hash().to_owned();
    let expected_order = expected
        .ready_frontier(64)
        .unwrap()
        .iter()
        .map(|index| expected.merge_id(*index).to_owned())
        .collect::<Vec<_>>();
    let session = Arc::new(tokio::sync::Mutex::new(manager));
    let router = Arc::new(PlanRouter::default());
    let host = Arc::new(PluginHost::new(
        session_id,
        router.clone(),
        Vec::<String>::new(),
    ));
    let service = Arc::new(OptChatServices::new(
        session.clone(),
        &host,
        Some(artifact.clone()),
    ));
    *router.service.lock().unwrap() = Some(service);
    let script = r"
return {exports={{name='prepare',run=function(_,ctx)
  ctx.call('optchat.begin',{budget_bytes=131072,summary_bytes=128})
  local batches,merges,max_batch=0,0,0
  while true do
    local plan=ctx.call('optchat.plan',{})
    if plan.ready then break end
    assert(#plan.merges>0,'preparation stalled')
    batches=batches+1;max_batch=math.max(max_batch,#plan.merges)
    assert(batches<1000,'preparation exceeded work limit')
    for _,merge in ipairs(plan.merges) do
      merges=merges+1
      ctx.call('optchat.accept',{merge=merge,text='deterministic bounded summary'})
    end
  end
  local view=ctx.call('optchat.finish',{})
  return {entries=view.entries,bytes=view.bytes,source_hash=view.source_hash,spans=view.spans,batches=batches,merges=merges,max_batch=max_batch}
end},{name='recall',run=function(input,ctx) return ctx.call('optchat.zoom',input) end}}}
";
    let snapshot = host
        .reconcile(vec![PluginSource {
            name: "optchat".into(),
            source: script.into(),
            config: json!({}),
        }])
        .unwrap();
    let target = snapshot
        .contributions
        .iter()
        .find(|target| target.kind == ContributionKind::Export && target.name == "prepare")
        .unwrap();
    let mut invocation = host
        .invoke(target, json!({}), &CancellationToken::new())
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(180), invocation.next())
        .await
        .unwrap()
        .unwrap();
    let result = match event {
        InvocationEvent::Finished(Ok(InvocationOutcome::Unary(value))) => value,
        other => {
            let trace = router.trace.lock().unwrap();
            panic!("actual Lua plan conversion failed: {other:?}; serialized native replies {:?}, max individual input {}",trace.reply_bytes,trace.max_input);
        }
    };
    assert_eq!(result["entries"], 129);
    assert_eq!(result["source_hash"], expected_hash);
    assert!(result["bytes"].as_u64().unwrap() <= 131_072);
    let mut covered = 0;
    for span in result["spans"].as_array().unwrap() {
        assert_eq!(span["start"], covered);
        covered = span["end"].as_u64().unwrap();
    }
    assert_eq!(covered, 129);
    {
        let trace = router.trace.lock().unwrap();
        assert!(trace.max_input <= 524_288);
        assert_eq!(
            trace.accepted, expected_order,
            "byte slicing changed the old eligible merge frontier/order"
        );
        assert_eq!(
            result["merges"], 64,
            "byte slicing added unneeded parent summaries"
        );
        assert!(
            trace
                .reply_bytes
                .iter()
                .all(|bytes| *bytes <= 2 * 1024 * 1024),
            "reply exceeded aggregate wire bound: {:?}",
            trace.reply_bytes
        );
    }
    // Resume through a NEW native service and the SAME unchanged Lua quota.
    *router.service.lock().unwrap() = Some(Arc::new(OptChatServices::new(
        session,
        &host,
        Some(artifact),
    )));
    let mut resumed = host
        .invoke(target, json!({}), &CancellationToken::new())
        .unwrap();
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(180), resumed.next())
        .await
        .unwrap()
        .unwrap();
    let InvocationEvent::Finished(Ok(InvocationOutcome::Unary(reused))) = resumed else {
        panic!("cache resume failed: {resumed:?}");
    };
    assert_eq!(reused["merges"], 0, "durable summaries were not reused");
    assert_eq!(reused["source_hash"], expected_hash);
    let recall = snapshot
        .contributions
        .iter()
        .find(|target| target.kind == ContributionKind::Export && target.name == "recall")
        .unwrap();
    let originals = source
        .iter()
        .filter(|entry| !matches!(entry, pa_types::session::FileEntry::Header { .. }))
        .collect::<Vec<_>>();
    for (span, offset, original) in [
        (&result["spans"][0], 0, originals[0]),
        (
            result["spans"].as_array().unwrap().last().unwrap(),
            0,
            *originals.last().unwrap(),
        ),
    ] {
        let mut invoked = host
            .invoke(
                recall,
                json!({"id":span["id"],"offset":offset,"limit":1}),
                &CancellationToken::new(),
            )
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(180), invoked.next())
            .await
            .unwrap()
            .unwrap();
        let InvocationEvent::Finished(Ok(InvocationOutcome::Unary(value))) = event else {
            panic!("exact recall failed: {event:?}");
        };
        assert_eq!(value["entries"][0], serde_json::to_value(original).unwrap());
    }
    assert_eq!(
        sha2::Sha256::digest(std::fs::read(&journal).unwrap()),
        before,
        "preparation rewrote canonical originals"
    );
}
