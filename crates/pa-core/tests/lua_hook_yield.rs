//! Regression for count-hook yields with concurrent same-VM invocations.
use pa_core::plugins::*;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
struct NoCapabilities;
impl HostCapabilities for NoCapabilities {
    fn call(&self, _: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
        Box::pin(async { Err("unexpected capability".into()) })
    }
}
const LUA: &str = r#"
local calls=0
local released=false
return {providers={{name='aux',mode='streaming',run=function(input,ctx)
  calls=calls+1
  local review=string.find(input.context.system_prompt,'Review evidence',1,true)~=nil
  if not review then while not released do local tick=1 end end
  local text=review and '{"shouldRefine":true,"rationale":"fixture evidence"}' or '{"summary":"held","edits":[{"action":"create","kind":"memory","id":"late","value":{"content":"must not arrive stale"}}]}'
  local message={role='assistant',content={{type='text',text=text}},api=input.model.api,provider=input.model.provider,model=input.model.id,
    usage={input=0,output=0,cacheRead=0,cacheWrite=0,totalTokens=0,cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
  ctx.emit({type='start',partial=message});ctx.emit({type='done',reason='stop',message=message});return message
end}},exports={{name='calls',run=function() return calls end},{name='release',run=function() released=true return true end}}}
"#;
fn target(host: &PluginHost, kind: ContributionKind, name: &str) -> Contribution {
    host.snapshot()
        .contributions
        .iter()
        .find(|x| x.kind == kind && x.name == name)
        .unwrap()
        .clone()
}
async fn unary(host: &PluginHost, name: &str) -> Value {
    let mut call = host
        .invoke(
            &target(host, ContributionKind::Export, name),
            json!({}),
            &CancellationToken::new(),
        )
        .unwrap();
    match call.next().await.unwrap() {
        InvocationEvent::Finished(Ok(InvocationOutcome::Unary(value))) => value,
        other => panic!("export {name}: {other:?}"),
    }
}
async fn trial(round: usize) {
    let host = Arc::new(PluginHost::new(
        format!("probe-{round}"),
        Arc::new(NoCapabilities),
        [],
    ));
    host.reconcile(vec![PluginSource {
        name: "route".into(),
        source: LUA.into(),
        config: json!({}),
    }])
    .unwrap();
    let mut held = Vec::new();
    for _ in 0..1 {
        held.push(host.invoke(&target(&host,ContributionKind::Provider,"aux"),json!({"context":{"system_prompt":"nonreview task"},"model":{"api":"test","provider":"test","id":"selected"}}),&CancellationToken::new()).unwrap());
    }
    loop {
        if unary(&host, "calls").await.as_u64() == Some(1) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(unary(&host, "release").await, json!(true));
    for mut call in held {
        match call.next().await.unwrap() {
            InvocationEvent::Update(value) => assert_eq!(value["type"], json!("start")),
            other @ InvocationEvent::Finished(_) => {
                panic!("provider first round {round}: {other:?}");
            }
        }
        match call.next().await.unwrap() {
            InvocationEvent::Update(value) => assert_eq!(value["type"], json!("done")),
            other @ InvocationEvent::Finished(_) => {
                panic!("provider done round {round}: {other:?}");
            }
        }
        match call.next().await.unwrap() {
            InvocationEvent::Finished(Ok(InvocationOutcome::Streamed(value))) => {
                assert_eq!(value["model"], json!("selected"));
            }
            other => panic!("provider result round {round}: {other:?}"),
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_hook_yields_preserve_suspended_lua_frame_locals() {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        for batch in 0..1250 {
            let mut handles = Vec::new();
            for n in 0..8 {
                handles.push(tokio::spawn(trial(batch * 8 + n)));
            }
            for handle in handles {
                handle.await.unwrap();
            }
            if batch % 100 == 0 {
                eprintln!("completed {} trials", (batch + 1) * 8);
            }
        }
    })
    .await
    .expect("concurrent hook-yield regression exceeded deadline");
}
