//! Real kernel -> Python plugin namespace -> native handler -> Lua capability cancellation.
#![cfg(unix)]
#![allow(clippy::large_futures, clippy::too_many_lines)]

use pa_core::{
    kernel::{
        cancellation::AbortSignal,
        manager::ReplKernelManager,
        shared::{
            host_handler, ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions,
            KernelShutdownOptions,
        },
    },
    plugins::{
        CapabilityFuture, CapabilityRequest, HostCapabilities, InvocationEvent, InvocationOutcome,
        PluginHost, PluginSource,
    },
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

struct DropNotice(mpsc::UnboundedSender<()>);
impl Drop for DropNotice {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
struct Gate {
    started: mpsc::UnboundedSender<oneshot::Sender<()>>,
    dropped: mpsc::UnboundedSender<()>,
    writes: Arc<AtomicUsize>,
}
impl HostCapabilities for Gate {
    fn call(&self, _: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
        let started = self.started.clone();
        let dropped = self.dropped.clone();
        let writes = self.writes.clone();
        Box::pin(async move {
            let _notice = DropNotice(dropped);
            let (release, wait) = oneshot::channel();
            started.send(release).unwrap();
            wait.await.map_err(|e| e.to_string())?;
            // Deferred authoritative native work: cancellation must drop this future first.
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!("published"))
        })
    }
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("bounded seam operation")
}

#[tokio::test]
async fn per_call_cancellation_drops_native_plugin_work_without_retiring_generation() {
    let Some(python) = std::env::var_os("PA_CORE_KERNEL_PYTHON") else {
        eprintln!(
            "set PA_CORE_KERNEL_PYTHON to an isolated kernel interpreter to run this verifier"
        );
        return;
    };
    let python = PathBuf::from(python);
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let writes = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(PluginHost::new(
        "cancel-test".into(),
        Arc::new(Gate {
            started: started_tx,
            dropped: dropped_tx,
            writes: writes.clone(),
        }),
        [],
    ));
    let snapshot = host
        .reconcile(vec![PluginSource {
            name: "gate".into(),
            config: Value::Null,
            source: r"local n=0
return {exports={
{name='wait', run=function(input,ctx) return ctx.call('wait',input) end},
{name='count', run=function() n=n+1; return n end}
}}"
            .into(),
        }])
        .unwrap();
    let mut handlers = HostRequestHandlers::new();
    let catalog_host = host.clone();
    handlers.register(
        "plugin.catalog",
        host_handler(move |_| {
            let snapshot = catalog_host.snapshot();
            async move {
                Ok(
                    json!({"session":"cancel-test", "revision":snapshot.revision,
            "exports": snapshot.contributions.iter().map(|c| json!({
                "plugin":c.plugin, "generation":c.generation.to_string(), "name":c.name,
                "description":c.description, "mode":"unary"
            })).collect::<Vec<_>>() }),
                )
            }
        }),
    );
    let invoke_host = host.clone();
    handlers.register(
        "plugin.invoke",
        host_handler(move |payload| {
            let host = invoke_host.clone();
            async move {
                let target = host
                    .snapshot()
                    .contributions
                    .iter()
                    .find(|c| {
                        payload.data["export"] == c.name
                            && payload.data["generation"] == c.generation.to_string()
                    })
                    .expect("exact active target")
                    .clone();
                let mut invocation = host.invoke(
                    &target,
                    payload.data["input"].clone(),
                    &CancellationToken::new(),
                )?;
                match invocation.next().await {
                    Some(InvocationEvent::Finished(Ok(InvocationOutcome::Unary(value)))) => {
                        Ok(value)
                    }
                    other => anyhow::bail!("unexpected plugin result {other:?}"),
                }
            }
        }),
    );
    let control = Arc::new(Semaphore::new(0));
    let control_handler = control.clone();
    handlers.register(
        "test.control",
        host_handler(move |_| {
            let control = control_handler.clone();
            async move {
                control.acquire().await.unwrap().forget();
                Ok(Value::Null)
            }
        }),
    );
    let dir = tempfile::tempdir().unwrap();
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python.clone()),
        cwd: Some(dir.path().into()),
        host_handlers: handlers.clone(),
        ..Default::default()
    });
    let bootstrap_code = "import asyncio\nfrom rlm import host_request\nfrom rlm.plugins import PluginNamespace\nplugin=PluginNamespace('cancel-test')\n_prime_agent_plugin_namespace=plugin\nawait plugin.refresh()";
    let bootstrap = bounded(manager.execute(
        bootstrap_code,
        ExecuteOptions {
            internal: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(bootstrap.status, ExecuteStatus::Ok, "{:?}", bootstrap.error);
    let pid = manager.process_id();
    for action in ["task", "interrupt"] {
        let signal = AbortSignal::new();
        let code = if action == "task" {
            "pending=asyncio.create_task(plugin.gate.wait())\nawait host_request('test.control', {})\npending.cancel()\ntry:\n    await pending\nexcept asyncio.CancelledError:\n    pass"
        } else {
            "await plugin.gate.wait()"
        };
        let executing = {
            let manager = manager.clone();
            let signal = signal.clone();
            tokio::spawn(async move {
                manager
                    .execute(
                        code,
                        ExecuteOptions {
                            signal: Some(signal),
                            ..Default::default()
                        },
                    )
                    .await
            })
        };
        let release = bounded(started.recv()).await.unwrap();
        if action == "task" {
            control.add_permits(1);
        } else {
            signal.abort();
        }
        let result = bounded(executing).await.unwrap().unwrap();
        if action == "task" {
            assert_eq!(result.status, ExecuteStatus::Ok, "{:?}", result.error);
        }
        bounded(dropped.recv())
            .await
            .expect("cancel must drop native capability future");
        assert!(
            release.send(()).is_err(),
            "cancelled native work cannot resume after its barrier opens"
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(
            Arc::ptr_eq(&snapshot, &host.snapshot()),
            "same enabled generation"
        );
        assert_eq!(manager.process_id(), pid, "no kernel restart");
        let result =
            bounded(manager.execute("await plugin.gate.count()", ExecuteOptions::default()))
                .await
                .unwrap();
        assert_eq!(result.status, ExecuteStatus::Ok, "{:?}", result.error);
        assert_eq!(
            result.result.as_deref(),
            Some(if action == "task" { "1" } else { "2" })
        );
    }
    // A detached background request is not owned by whichever cell is active later.
    let result = bounded(manager.execute(
        "background=asyncio.create_task(plugin.gate.wait())",
        ExecuteOptions::default(),
    ))
    .await
    .unwrap();
    assert_eq!(result.status, ExecuteStatus::Ok);
    let background_release = bounded(started.recv()).await.unwrap();
    let signal = AbortSignal::new();
    let executing = {
        let manager = manager.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    "await plugin.gate.wait()",
                    ExecuteOptions {
                        signal: Some(signal),
                        ..Default::default()
                    },
                )
                .await
        })
    };
    let foreground_release = bounded(started.recv()).await.unwrap();
    signal.abort();
    bounded(executing).await.unwrap().unwrap();
    bounded(dropped.recv()).await.unwrap();
    assert!(foreground_release.send(()).is_err());
    background_release
        .send(())
        .expect("unrelated background request survives");
    bounded(dropped.recv()).await.unwrap();
    let result = bounded(manager.execute("await background", ExecuteOptions::default()))
        .await
        .unwrap();
    assert_eq!(result.result.as_deref(), Some("'published'"));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(manager.process_id(), pid);
    assert!(Arc::ptr_eq(&snapshot, &host.snapshot()));
    // Shutdown must cancel a still-pending detached caller too, not detach native work.
    bounded(manager.execute(
        "background=asyncio.create_task(plugin.gate.wait())",
        ExecuteOptions::default(),
    ))
    .await
    .unwrap();
    let release = bounded(started.recv()).await.unwrap();
    bounded(manager.shutdown(KernelShutdownOptions::default()))
        .await
        .unwrap();
    bounded(dropped.recv()).await.unwrap();
    assert!(release.send(()).is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    // EOF alone must drop native handlers, even if the kernel process stays alive.
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        cwd: Some(dir.path().into()),
        host_handlers: handlers,
        ..Default::default()
    });
    let boot = bounded(manager.execute(
        bootstrap_code,
        ExecuteOptions {
            internal: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(boot.status, ExecuteStatus::Ok);
    bounded(manager.execute(
        "background=asyncio.create_task(plugin.gate.wait())",
        ExecuteOptions::default(),
    ))
    .await
    .unwrap();
    let release = bounded(started.recv()).await.unwrap();
    let closing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    "import os\nfrom rlm.repl import _protocol_fd\nos.close(_protocol_fd)",
                    ExecuteOptions::default(),
                )
                .await
        })
    };
    bounded(dropped.recv())
        .await
        .expect("stdout EOF must cancel native handler");
    assert!(release.send(()).is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    manager.kill();
    let _ = bounded(closing).await.unwrap();
}
