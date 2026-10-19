use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;

const LEDGER: &str = include_str!("fixtures/ledger.lua");
const COUNTER: &str = include_str!("fixtures/counter.lua");

struct Echo;
impl HostCapabilities for Echo {
    fn call(&self, request: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
        Box::pin(async move {
            Ok(
                json!({"session": request.session, "plugin": request.plugin, "input": request.input}),
            )
        })
    }
}

fn source(name: &str, text: &str) -> PluginSource {
    PluginSource {
        name: name.into(),
        source: text.into(),
        config: Value::Null,
    }
}

fn host() -> PluginHost {
    PluginHost::new(
        "session-a".into(),
        Arc::new(Echo),
        ["resume".into(), "model".into()],
    )
}

fn target(host: &PluginHost, plugin: &str, kind: ContributionKind, name: &str) -> Contribution {
    host.snapshot()
        .contributions
        .iter()
        .find(|c| c.plugin == plugin && c.kind == kind && c.name == name)
        .unwrap()
        .clone()
}

async fn result(
    host: &PluginHost,
    plugin: &str,
    kind: ContributionKind,
    name: &str,
    input: Value,
) -> Result<InvocationOutcome, PluginError> {
    let mut invocation = host
        .invoke(
            &target(host, plugin, kind, name),
            input,
            &CancellationToken::new(),
        )
        .unwrap();
    let event = invocation.next().await.unwrap();
    assert!(invocation.next().await.is_none());
    match event {
        InvocationEvent::Finished(result) => result,
        InvocationEvent::Update(value) => panic!("unexpected update: {value}"),
    }
}

#[tokio::test]
async fn two_real_plugins_exports_commands_hooks_and_session_isolation() {
    let host = host();
    let desired = vec![source("ledger", LEDGER), source("counter", COUNTER)];
    let snapshot = host.reconcile(desired.clone()).unwrap();
    assert_eq!(snapshot.contributions.len(), 11);
    assert!(Arc::ptr_eq(
        &snapshot,
        &host.reconcile(desired.clone()).unwrap()
    ));
    for (plugin, expected) in [("ledger", 1), ("ledger", 2), ("counter", 101)] {
        assert_eq!(
            result(
                &host,
                plugin,
                ContributionKind::Export,
                "count",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(json!(expected)))
        );
    }
    assert_eq!(
        result(
            &host,
            "ledger",
            ContributionKind::Hook,
            "annotate",
            json!({"text": "hi"})
        )
        .await,
        Ok(InvocationOutcome::Unary(json!({"text": "ledger:hi"})))
    );
    let mut command = host
        .invoke(
            &target(&host, "ledger", ContributionKind::Command, "remember"),
            json!({"note": "hello"}),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        command.next().await,
        Some(InvocationEvent::Update(json!({"stage": "started"})))
    );
    assert_eq!(
        command.next().await,
        Some(InvocationEvent::Update(json!({"stage": "completed"})))
    );
    assert_eq!(
        command.next().await,
        Some(InvocationEvent::Finished(Ok(InvocationOutcome::Streamed(
            json!({"session": "session-a", "plugin": "ledger", "input": {"note": "hello"}})
        ))))
    );
    assert_eq!(command.next().await, None);
    assert_eq!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "environment",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(
            json!({"io": false, "os": false, "package": false, "require": false, "loadfile": false})
        ))
    );
    let other = PluginHost::new("session-b".into(), Arc::new(Echo), []);
    other.reconcile(desired).unwrap();
    assert_eq!(
        result(
            &other,
            "ledger",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(1)))
    );
    assert!(matches!(
        other.invoke(
            &target(&host, "ledger", ContributionKind::Export, "count"),
            Value::Null,
            &CancellationToken::new()
        ),
        Err(PluginError::Missing)
    ));
}

#[tokio::test]
async fn failed_reloads_preserve_whole_generation_success_replaces_and_disable_removes() {
    let host = host();
    let original = host
        .reconcile(vec![source("ledger", LEDGER), source("counter", COUNTER)])
        .unwrap();
    let old = target(&host, "ledger", ContributionKind::Export, "count");
    let counter = target(&host, "counter", ContributionKind::Export, "count");
    assert_eq!(
        result(
            &host,
            "ledger",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(1)))
    );
    for bad in [
        "this is not Lua",
        "return {exports={{name='same',run=function()end},{name='same',run=function()end}}}",
        "return {commands={{name='model',run=function()end}}}",
        "return {commands={{name='resume',run=function()end}}}",
        "return {exports={{name='_private',run=function()end}}}",
        "return {exports={[2]={name='hole',run=function()end}}}",
        "return {unsupported_contribution={}}",
        "return {exports={{name='valid', run=function()end, future_form=true}}}",
        "while true do end",
    ] {
        assert!(
            host.reconcile(vec![source("ledger", bad), source("counter", COUNTER)])
                .is_err(),
            "accepted {bad}"
        );
        assert!(Arc::ptr_eq(&original, &host.snapshot()));
    }
    assert!(host.reconcile(vec![source("core", LEDGER)]).is_err());
    assert!(host
        .reconcile(vec![source("ledger", LEDGER), source("ledger", LEDGER)])
        .is_err());
    assert!(host
        .reconcile(vec![source("ledger", LEDGER), source("second", LEDGER)])
        .is_err());
    assert!(Arc::ptr_eq(&original, &host.snapshot()));
    assert_eq!(
        result(
            &host,
            "ledger",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(2)))
    );
    let replacement = LEDGER
        .replace("local count = 0", "local count = 40")
        .replace("remember", "record");
    let current = host
        .reconcile(vec![
            source("ledger", &replacement),
            source("counter", COUNTER),
        ])
        .unwrap();
    assert_eq!(current.revision, original.revision + 1);
    assert!(!current.contributions.contains(&old));
    assert_eq!(
        target(&host, "counter", ContributionKind::Export, "count"),
        counter
    );
    assert!(matches!(
        host.invoke(&old, Value::Null, &CancellationToken::new()),
        Err(PluginError::Missing)
    ));
    assert_eq!(
        result(
            &host,
            "ledger",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(41)))
    );
    let disabled = host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    assert!(disabled.contributions.iter().all(|c| c.plugin == "counter"));
    assert!(host.reconcile(vec![]).unwrap().contributions.is_empty());
    // A zero-contribution generation still participates in reconciliation and retirement.
    let empty = host.reconcile(vec![source("empty", "return {}")]).unwrap();
    assert_eq!(host.reconcile(vec![]).unwrap().revision, empty.revision + 1);
}

#[tokio::test]
async fn config_is_private_owned_before_activation_and_changes_replace_the_generation() {
    let host = host();
    let mut configured = source(
        "configured",
        r"
        config.nested.count = config.nested.count + 1
        return {exports={{name=config.export_name, run=function()
            local count = config.nested.count
            config.nested.count = count + 1
            return {count=count, values=config.values}
        end}}}
        ",
    );
    configured.config = json!({
        "export_name": "read",
        "nested": {"count": 10},
        "values": [true, null, "private"]
    });
    let mut sibling = configured.clone();
    sibling.name = "sibling".into();
    let original = host
        .reconcile(vec![configured.clone(), sibling.clone()])
        .unwrap();
    let old = target(&host, "configured", ContributionKind::Export, "read");
    let sibling_target = target(&host, "sibling", ContributionKind::Export, "read");
    for expected in [11, 12] {
        assert_eq!(
            result(
                &host,
                "configured",
                ContributionKind::Export,
                "read",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(
                json!({"count": expected, "values": [true, null, "private"]})
            ))
        );
        // Mutating the Lua table never mutates the JSON used for generation reuse.
        assert!(Arc::ptr_eq(
            &original,
            &host
                .reconcile(vec![configured.clone(), sibling.clone()])
                .unwrap()
        ));
    }
    let other = PluginHost::new("other-session".into(), Arc::new(Echo), []);
    other.reconcile(vec![configured.clone()]).unwrap();
    for (instance, plugin) in [(&host, "sibling"), (&other, "configured")] {
        assert_eq!(
            result(
                instance,
                plugin,
                ContributionKind::Export,
                "read",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(
                json!({"count": 11, "values": [true, null, "private"]})
            ))
        );
    }
    // A config-only activation failure leaves the entire old snapshot current.
    let mut invalid = configured.clone();
    invalid.config["nested"]["count"] = json!("not a number");
    assert!(host.reconcile(vec![invalid, sibling.clone()]).is_err());
    assert!(Arc::ptr_eq(&original, &host.snapshot()));

    configured.config["nested"]["count"] = json!(40);
    let replacement = host.reconcile(vec![configured, sibling]).unwrap();
    assert_eq!(replacement.revision, original.revision + 1);
    assert!(!replacement.contributions.contains(&old));
    assert_eq!(
        target(&host, "sibling", ContributionKind::Export, "read"),
        sibling_target
    );
    assert!(matches!(
        host.invoke(&old, Value::Null, &CancellationToken::new()),
        Err(PluginError::Missing)
    ));
    assert_eq!(
        result(
            &host,
            "configured",
            ContributionKind::Export,
            "read",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(
            json!({"count": 41, "values": [true, null, "private"]})
        ))
    );
}

struct DropNotice(mpsc::UnboundedSender<()>);
impl Drop for DropNotice {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct Gate {
    started: mpsc::UnboundedSender<CapabilityRequest>,
    dropped: mpsc::UnboundedSender<()>,
}
impl HostCapabilities for Gate {
    fn call(&self, request: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
        let started = self.started.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _notice = DropNotice(dropped);
            started.send(request).unwrap();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn cancellation_retirement_and_drop_release_capability_futures_without_blocking_host() {
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let host = PluginHost::new(
        "private-session".into(),
        Arc::new(Gate {
            started: started_tx,
            dropped: dropped_tx,
        }),
        [],
    );
    host.reconcile(vec![source("counter", COUNTER), source("ledger", LEDGER)])
        .unwrap();
    for action in ["cancel", "reload", "disable", "drop"] {
        let cancellation = CancellationToken::new();
        let mut invocation = host
            .invoke(
                &target(&host, "counter", ContributionKind::Export, "wait"),
                json!({"action": action}),
                &cancellation,
            )
            .unwrap();
        let request = started.recv().await.unwrap();
        assert_eq!(
            request,
            CapabilityRequest {
                session: "private-session".into(),
                plugin: "counter".into(),
                generation: target(&host, "counter", ContributionKind::Export, "wait").generation,
                invocation: request.invocation,
                operation: "wait".into(),
                input: json!({"action": action})
            }
        );
        // Same VM and another plugin remain callable while the capability is pending.
        assert_eq!(
            result(
                &host,
                "counter",
                ContributionKind::Export,
                "count",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(json!(101)))
        );
        assert!(matches!(
            result(
                &host,
                "ledger",
                ContributionKind::Export,
                "count",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(_))
        ));
        match action {
            "cancel" => cancellation.cancel(),
            "reload" => {
                host.reconcile(vec![
                    source("counter", &(COUNTER.to_owned() + "\n-- replacement")),
                    source("ledger", LEDGER),
                ])
                .unwrap();
            }
            "disable" => {
                host.reconcile(vec![source("ledger", LEDGER)]).unwrap();
            }
            "drop" => {
                drop(invocation);
                dropped.recv().await.unwrap();
                break;
            }
            other => panic!("unexpected action {other}"),
        }
        let error = if action == "cancel" {
            PluginError::Cancelled
        } else {
            PluginError::Retired
        };
        assert_eq!(
            invocation.next().await,
            Some(InvocationEvent::Finished(Err(error)))
        );
        assert_eq!(invocation.next().await, None);
        dropped.recv().await.unwrap();
        // Force a fresh isolated counter for the next cancellation case.
        host.reconcile(vec![source("ledger", LEDGER)]).unwrap();
        host.reconcile(vec![source("counter", COUNTER), source("ledger", LEDGER)])
            .unwrap();
    }
}

#[tokio::test]
async fn pure_lua_busy_loop_yields_for_cancellation() {
    let host = host();
    host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    let cancel = CancellationToken::new();
    let mut busy = host
        .invoke(
            &target(&host, "counter", ContributionKind::Export, "busy"),
            Value::Null,
            &cancel,
        )
        .unwrap();
    assert_eq!(
        busy.next().await,
        Some(InvocationEvent::Update(json!("ready")))
    );
    // A runtime task must get CPU even though the Lua callback never finishes.
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(
        busy.next().await,
        Some(InvocationEvent::Finished(Err(PluginError::Cancelled)))
    );
}

#[tokio::test]
async fn callback_errors_and_retained_context_cannot_escape_the_invocation() {
    let host = host();
    host.reconcile(vec![source("counter", COUNTER), source("ledger", LEDGER)])
        .unwrap();
    for name in ["bad_emit", "bad_result"] {
        assert!(matches!(
            result(&host, "ledger", ContributionKind::Export, name, Value::Null).await,
            Err(PluginError::Lua(_))
        ));
    }
    assert_eq!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "retain",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!("saved")))
    );
    assert!(
        matches!(result(&host, "counter", ContributionKind::Export, "reuse", Value::Null).await, Err(PluginError::Lua(message)) if message.contains("closed") || message.contains("cancelled"))
    );
}

#[tokio::test]
async fn revoked_retained_context_never_constructs_the_adapter_future() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DispatchCounter(Arc<AtomicUsize>);
    impl HostCapabilities for DispatchCounter {
        fn call(&self, _: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
            // Dispatch can happen synchronously before the adapter returns its future.
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Value::Null) })
        }
    }
    let dispatches = Arc::new(AtomicUsize::new(0));
    let host = PluginHost::new(
        "session".into(),
        Arc::new(DispatchCounter(Arc::clone(&dispatches))),
        [],
    );
    host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    assert_eq!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "retain",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!("saved")))
    );
    assert!(matches!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "reuse",
            Value::Null
        )
        .await,
        Err(PluginError::Lua(_))
    ));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn host_drop_retires_invocations_and_drops_the_pending_capability_future() {
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let host = PluginHost::new(
        "session".into(),
        Arc::new(Gate {
            started: started_tx,
            dropped: dropped_tx,
        }),
        [],
    );
    host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    let mut invocation = host
        .invoke(
            &target(&host, "counter", ContributionKind::Export, "wait"),
            Value::Null,
            &CancellationToken::new(),
        )
        .unwrap();
    started.recv().await.unwrap();
    drop(host);
    assert_eq!(
        invocation.next().await,
        Some(InvocationEvent::Finished(Err(PluginError::Retired)))
    );
    dropped.recv().await.unwrap();
    assert_eq!(invocation.next().await, None);
}

#[tokio::test]
async fn optional_plugin_commands_are_reserved_only_by_the_consuming_host() {
    let host = host();
    let refine = "return {commands={{name='refine',run=function(input) return input end}}}";
    let snapshot = host.reconcile(vec![source("harness", refine)]).unwrap();
    assert_eq!(
        result(
            &host,
            "harness",
            ContributionKind::Command,
            "refine",
            json!({"data": "retained"})
        )
        .await,
        Ok(InvocationOutcome::Unary(json!({"data": "retained"})))
    );
    assert!(host
        .reconcile(vec![source("harness", refine), source("other", refine)])
        .is_err());
    assert!(Arc::ptr_eq(&snapshot, &host.snapshot()));
}

#[tokio::test]
async fn stream_backpressure_then_ordered_delivery_crosses_the_same_host_interface() {
    struct Observe(mpsc::UnboundedSender<String>);
    impl HostCapabilities for Observe {
        fn call(&self, request: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
            self.0.send(request.operation).unwrap();
            Box::pin(async { Ok(Value::Null) })
        }
    }
    let (observed_tx, mut observed) = mpsc::unbounded_channel();
    let host = PluginHost::new("session".into(), Arc::new(Observe(observed_tx)), []);
    host.reconcile(vec![source("burst", "return {exports={{name='run',mode='streaming',run=function(input,ctx) ctx.call('begun', null); for i=1,40 do ctx.emit(i) end; ctx.call('drained', null); return 'done' end}}}")]).unwrap();
    let mut invocation = host
        .invoke(
            &target(&host, "burst", ContributionKind::Export, "run"),
            Value::Null,
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(observed.recv().await.as_deref(), Some("begun"));
    // This single-threaded test resumes only after the producer yields. A callback
    // with an unbounded update queue would have dispatched 'drained' before yielding.
    assert_eq!(observed.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    for i in 1..=40 {
        assert_eq!(
            invocation.next().await,
            Some(InvocationEvent::Update(json!(i)))
        );
    }
    assert_eq!(
        invocation.next().await,
        Some(InvocationEvent::Finished(Ok(InvocationOutcome::Streamed(
            json!("done")
        ))))
    );
    assert_eq!(observed.recv().await.as_deref(), Some("drained"));
    assert_eq!(invocation.next().await, None);
}

#[tokio::test]
async fn buffered_terminal_result_is_fenced_after_observable_producer_completion() {
    use std::{
        future::Future,
        task::{Context, Poll, Wake, Waker},
    };
    use tokio::sync::Notify;

    struct Ready(Notify);
    impl Wake for Ready {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.notify_one();
        }
    }
    for action in ["disable", "cancel"] {
        let host = host();
        host.reconcile(vec![source("counter", COUNTER)]).unwrap();
        let cancellation = CancellationToken::new();
        let mut invocation = host
            .invoke(
                &target(&host, "counter", ContributionKind::Export, "count"),
                Value::Null,
                &cancellation,
            )
            .unwrap();
        let ready = Arc::new(Ready(Notify::new()));
        let waker = Waker::from(Arc::clone(&ready));
        let mut event = Box::pin(invocation.next());
        assert_eq!(
            event.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        // The producer has not run yet on this current-thread runtime. With no
        // cancellation/retirement, only its unary terminal enqueue wakes next().
        // Observe that wake without repolling/consuming the buffered terminal event.
        ready.0.notified().await;
        let error = if action == "disable" {
            host.reconcile(vec![]).unwrap();
            PluginError::Retired
        } else {
            cancellation.cancel();
            PluginError::Cancelled
        };
        assert_eq!(event.await, Some(InvocationEvent::Finished(Err(error))));
        assert_eq!(invocation.next().await, None);
    }
}

#[tokio::test]
async fn already_buffered_update_is_fenced_on_disable_and_cancellation() {
    for action in ["disable", "cancel"] {
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
        let host = PluginHost::new(
            "session".into(),
            Arc::new(Gate {
                started: started_tx,
                dropped: dropped_tx,
            }),
            [],
        );
        host.reconcile(vec![source("ledger", LEDGER)]).unwrap();
        let cancellation = CancellationToken::new();
        let mut invocation = host
            .invoke(
                &target(&host, "ledger", ContributionKind::Command, "remember"),
                Value::Null,
                &cancellation,
            )
            .unwrap();
        // ledger emits 'started' before calling the pending capability. This
        // observable readiness proves that its first update is already buffered.
        started.recv().await.unwrap();
        let error = if action == "disable" {
            host.reconcile(vec![]).unwrap();
            PluginError::Retired
        } else {
            cancellation.cancel();
            PluginError::Cancelled
        };
        assert_eq!(
            invocation.next().await,
            Some(InvocationEvent::Finished(Err(error)))
        );
        dropped.recv().await.unwrap();
        assert_eq!(invocation.next().await, None);
    }
}

#[tokio::test]
async fn typed_context_callbacks_require_unary_and_existing_plugin_name_grammar() {
    let host = host();
    let context =
        "return {context_strategies={{name='rolling',run=function(input) return input end}}}";
    let snapshot = host
        .reconcile(vec![source("default_context", context)])
        .unwrap();
    assert_eq!(
        result(
            &host,
            "default_context",
            ContributionKind::ContextStrategy,
            "rolling",
            json!({"stage":"assemble","messages":[]})
        )
        .await,
        Ok(InvocationOutcome::Unary(
            json!({"stage":"assemble","messages":[]})
        ))
    );
    for bad in [
        "return {context_strategies={{name='rolling',mode='streaming',run=function()end}}}",
        "return {compactors={{name='compact',mode='streaming',run=function()end}}}",
    ] {
        assert!(host
            .reconcile(vec![source("default_context", bad)])
            .is_err());
        assert!(Arc::ptr_eq(&snapshot, &host.snapshot()));
    }
}

#[tokio::test]
async fn provider_declarations_stream_and_reject_duplicate_ids_atomically() {
    let host = host();
    let provider = "return {providers={{name='custom-provider',run=function(input,ctx) ctx.emit(input); return {done=true} end}}}";
    let snapshot = host.reconcile(vec![source("custom", provider)]).unwrap();
    let target = target(
        &host,
        "custom",
        ContributionKind::Provider,
        "custom-provider",
    );
    assert_eq!(target.mode, InvocationMode::Streaming);
    let mut call = host
        .invoke(
            &target,
            json!({"request":"native"}),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        call.next().await,
        Some(InvocationEvent::Update(json!({"request":"native"})))
    );
    assert_eq!(
        call.next().await,
        Some(InvocationEvent::Finished(Ok(InvocationOutcome::Streamed(
            json!({"done":true})
        ))))
    );
    assert!(host
        .reconcile(vec![
            source("custom", provider),
            source("duplicate", provider)
        ])
        .is_err());
    assert!(Arc::ptr_eq(&snapshot, &host.snapshot()));
    assert!(host
        .reconcile(vec![source("invalid-name", provider)])
        .is_err());
    assert!(Arc::ptr_eq(&snapshot, &host.snapshot()));
}

#[tokio::test]
async fn candidate_policy_validation_preserves_previous_generation_and_live_state() {
    let host = host();
    let original = host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    let old = target(&host, "counter", ContributionKind::Export, "count");
    let retired = host.retirement(&old).unwrap();
    assert!(host
        .reconcile_validated(vec![source("replacement", LEDGER)], |_| Err(
            PluginError::Invalid("selection missing".into())
        ))
        .is_err());
    assert!(Arc::ptr_eq(&original, &host.snapshot()));
    assert!(!retired.is_cancelled());
    assert_eq!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(101)))
    );
    host.reconcile(vec![]).unwrap();
    assert!(retired.is_cancelled());
    let mut committed = false;
    assert_eq!(
        host.with_current_contributions(&[&old], || {
            committed = true;
        }),
        Err(PluginError::Missing)
    );
    assert!(!committed);
}

#[tokio::test]
async fn capability_requests_bind_each_callback_to_a_distinct_invocation() {
    struct Capture(mpsc::UnboundedSender<CapabilityRequest>);
    impl HostCapabilities for Capture {
        fn call(&self, request: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
            self.0.send(request).unwrap();
            Box::pin(async { Ok(Value::Null) })
        }
    }
    let (sent, mut received) = mpsc::unbounded_channel();
    let host = PluginHost::new("identity-test".into(), Arc::new(Capture(sent)), []);
    host.reconcile(vec![source(
        "identity",
        r"return {exports={{name='calls',run=function(input,ctx)
        ctx.call('identity', {})
        ctx.call('identity', {})
        return null
    end}}}",
    )])
    .unwrap();
    let mut calls = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            result(
                &host,
                "identity",
                ContributionKind::Export,
                "calls",
                Value::Null
            )
            .await,
            Ok(InvocationOutcome::Unary(Value::Null))
        );
        let first = received.recv().await.unwrap();
        assert_eq!(received.recv().await.unwrap(), first);
        calls.push(first);
    }
    assert_ne!(calls[0].invocation, calls[1].invocation);
    assert_eq!(
        calls[0],
        CapabilityRequest {
            session: "identity-test".into(),
            plugin: "identity".into(),
            generation: calls[1].generation,
            invocation: calls[0].invocation,
            operation: "identity".into(),
            input: json!({}),
        }
    );
}

#[tokio::test]
async fn context_data_declarations_are_explicit_and_do_not_rewrite_invocation_values() {
    let host = host();
    let source = source(
        "contract",
        r"
        local function echo(input) return input end
        return {
            exports={{name='plain',run=echo}},
            context_strategies={
                {name='plain',run=echo},
                {name='json',context_data='json',run=echo},
                {name='refs',context_data='refs_v1',run=echo}
            },
            compactors={
                {name='plain',run=echo},
                {name='json',context_data='json',run=echo},
                {name='refs',context_data='refs_v1',run=echo}
            }
        }
    ",
    );
    let snapshot = host.reconcile(vec![source]).unwrap();
    let actual: std::collections::BTreeMap<_, _> = snapshot
        .contributions
        .iter()
        .map(|target| ((target.kind, target.name.clone()), target.context_data))
        .collect();
    let expected = std::collections::BTreeMap::from([
        (
            (ContributionKind::Export, "plain".into()),
            ContextDataMode::Json,
        ),
        (
            (ContributionKind::ContextStrategy, "plain".into()),
            ContextDataMode::Json,
        ),
        (
            (ContributionKind::ContextStrategy, "json".into()),
            ContextDataMode::Json,
        ),
        (
            (ContributionKind::ContextStrategy, "refs".into()),
            ContextDataMode::RefsV1,
        ),
        (
            (ContributionKind::Compactor, "plain".into()),
            ContextDataMode::Json,
        ),
        (
            (ContributionKind::Compactor, "json".into()),
            ContextDataMode::Json,
        ),
        (
            (ContributionKind::Compactor, "refs".into()),
            ContextDataMode::RefsV1,
        ),
    ]);
    assert_eq!(actual, expected);
    for (kind, name) in expected.into_keys() {
        for input in [
            json!({"stage":"assemble","messages":[{"role":"user","content":"full body stays intact"}]}),
            json!({"stage":"compact","messages":{"ref":"opaque","count":100},"entries":{"ref":"canonical","count":200}}),
        ] {
            assert_eq!(
                result(&host, "contract", kind, &name, input.clone()).await,
                Ok(InvocationOutcome::Unary(input))
            );
        }
    }
}

#[tokio::test]
async fn invalid_context_data_modes_or_kinds_leave_the_previous_activation_intact() {
    let host = host();
    let original = host.reconcile(vec![source("counter", COUNTER)]).unwrap();
    let old = target(&host, "counter", ContributionKind::Export, "count");
    let retirement = host.retirement(&old).unwrap();
    let mut invalid = Vec::new();
    for field in ["context_strategies", "compactors"] {
        for data in ["'future'", "''", "123", "false", "{}"] {
            invalid.push(format!("return {{ {field} = {{ {{name='run',context_data={data},run=function(input) return input end}} }} }}"));
        }
        for data in ["json", "refs_v1"] {
            invalid.push(format!("return {{ {field} = {{ {{name='run',context_data='{data}',mode='streaming',run=function(input) return input end}} }} }}"));
        }
    }
    for field in ["exports", "commands", "hooks", "providers"] {
        for data in ["json", "refs_v1"] {
            invalid.push(format!("return {{ {field} = {{ {{name='run',context_data='{data}',run=function(input) return input end}} }} }}"));
        }
    }
    for declaration in invalid {
        assert!(
            host.reconcile(vec![source("counter", &declaration)])
                .is_err(),
            "accepted {declaration}"
        );
        assert!(Arc::ptr_eq(&original, &host.snapshot()));
        assert!(!retirement.is_cancelled());
    }
    assert_eq!(
        result(
            &host,
            "counter",
            ContributionKind::Export,
            "count",
            Value::Null
        )
        .await,
        Ok(InvocationOutcome::Unary(json!(101)))
    );
}
