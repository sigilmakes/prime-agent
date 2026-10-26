//! Host request handling: execute-side requests answered by the host
//! (harness/goal/etc.) and their settle/exit waits.

#[cfg(all(test, unix))]
mod fixture_executable {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-support/executable.rs"
    ));
}

use super::{
    anyhow, json, lock, AbortSignal, Arc, AsyncWriteExt, Duration, HostRequestPayload, Inner,
    Value, MAX_HANDLED_HOST_REQUEST_IDS,
};

impl Inner {
    /// Dispatch one typed request from kernel code to the registered handler
    /// and reply over the protocol. Unhandled requests answer with an error.
    pub(crate) fn start_host_request(self: &Arc<Self>, request_id: &str, data: Value) {
        let mut g = lock(&self.guarded);
        if g.handled_host_request_ids.0.contains(request_id)
            || g.host_inflight.contains_key(request_id)
        {
            return;
        }
        let (seen, order) = &mut g.handled_host_request_ids;
        seen.insert(request_id.to_string());
        order.push_back(request_id.to_string());
        while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
            if let Some(oldest) = order.pop_front() {
                seen.remove(&oldest);
            }
        }
        let inner = Arc::downgrade(self);
        let request_id = request_id.to_string();
        let cancellation = Arc::new(AbortSignal::new());
        // Construct before spawning: even cancellation before the first poll cleans up.
        let completion = HostRequestCompletion {
            inner: Arc::downgrade(self),
            request_id: request_id.clone(),
            cancellation: Arc::clone(&cancellation),
        };
        let task_cancellation = Arc::clone(&cancellation);
        let task_id = request_id.clone();
        tokio::spawn(async move {
            if task_cancellation.is_aborted() {
                return;
            }
            let future = {
                let Some(inner) = inner.upgrade() else { return };
                inner.handle_host_request(&data)
            };
            let result = tokio::select! {
                biased;
                () = task_cancellation.cancelled() => return,
                result = async {
                    match future {
                        Ok(future) => future.await,
                        Err(error) => Err(error),
                    }
                } => result,
            };
            let Some(inner) = inner.upgrade() else { return };
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {task_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            let frame = json!({ "type": "host_reply", "id": task_id, "data": reply });
            let stdin = lock(&inner.child).as_ref().map(|child| child.stdin.clone());
            // A pending writer must not keep the manager alive after its last owner drops.
            drop(inner);
            let Some(stdin) = stdin else { return };
            let mut writer = tokio::select! {
                biased;
                () = task_cancellation.cancelled() => return,
                writer = stdin.lock() => writer,
            };
            if task_cancellation.is_aborted() {
                return;
            }
            let Some(writer) = writer.as_mut() else {
                return;
            };
            let mut line = frame.to_string();
            line.push('\n');
            // Native work is finished and this frame is admitted. Complete the write
            // even on cancellation: dropping write_all mid-frame would corrupt stdin.
            // The Python caller has retired its exact ID and ignores any late reply.
            let result = async {
                writer.write_all(line.as_bytes()).await?;
                writer.flush().await
            }
            .await;
            if let Err(error) = result {
                if let Some(inner) = completion.inner.upgrade() {
                    inner.append_diagnostic(&format!(
                        "failed to send host request reply for {task_id}: {error:#}"
                    ));
                }
            }
        });
        // Keep registration atomic with dispatch and completion, including on multi-thread runtimes.
        g.host_inflight.insert(request_id, cancellation);
    }

    // Keep entries until their completion guard runs. Even after dedup eviction,
    // an ID cannot be reused while an old guard could still remove its entry.
    pub(crate) fn cancel_host_request(&self, request_id: &str) {
        if let Some(cancellation) = lock(&self.guarded).host_inflight.get(request_id) {
            cancellation.abort();
        }
    }

    pub(crate) fn cancel_host_requests(&self) {
        for cancellation in lock(&self.guarded).host_inflight.values() {
            cancellation.abort();
        }
    }

    fn handle_host_request(
        &self,
        data: &Value,
    ) -> anyhow::Result<crate::kernel::shared::HostHandlerFuture> {
        let Some(obj) = data.as_object() else {
            return Err(anyhow!("host request payload must be an object"));
        };
        let request_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("host request payload must have a string type"))?;
        let handler = self
            .options
            .host_handlers
            .get(request_type)
            .ok_or_else(|| {
                anyhow!("host request type \"{request_type}\" is not available in this session")
            })?
            .clone();
        Ok(handler(HostRequestPayload { data: data.clone() }))
    }

    /// Wait (bounded) for the in-flight host request tasks to settle.
    pub(crate) async fn wait_for_host_requests_to_settle(&self, timeout_ms: u64) {
        let all = async {
            loop {
                let settled = self.host_settled.notified();
                tokio::pin!(settled);
                settled.as_mut().enable();
                if lock(&self.guarded).host_inflight.is_empty() {
                    break;
                }
                settled.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    pub(crate) async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Remove terminal tasks immediately, including handler panic and cancellation.
struct HostRequestCompletion {
    inner: std::sync::Weak<Inner>,
    request_id: String,
    cancellation: Arc<AbortSignal>,
}

impl Drop for HostRequestCompletion {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut guarded = lock(&inner.guarded);
            if guarded
                .host_inflight
                .get(&self.request_id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
            {
                guarded.host_inflight.remove(&self.request_id);
            }
            drop(guarded);
            inner.host_settled.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{
        manager::ReplKernelManager,
        shared::{host_handler, HostRequestHandlers, KernelManagerOptions},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct DropNotice(mpsc::UnboundedSender<()>);
    impl Drop for DropNotice {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[tokio::test]
    async fn exact_cancel_is_idempotent_and_cleans_map_without_affecting_other_requests() {
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handlers = HostRequestHandlers::new();
        let count = calls.clone();
        handlers.register(
            "wait",
            host_handler(move |_| {
                let started = started_tx.clone();
                let dropped = dropped_tx.clone();
                let count = count.clone();
                async move {
                    let _notice = DropNotice(dropped);
                    count.fetch_add(1, Ordering::SeqCst);
                    started.send(()).unwrap();
                    std::future::pending().await
                }
            }),
        );
        let manager = ReplKernelManager::new(KernelManagerOptions {
            host_handlers: handlers,
            ..Default::default()
        });
        let inner = &manager.inner;
        inner.start_host_request("a", json!({"type":"wait"}));
        inner.start_host_request("a", json!({"type":"wait"}));
        inner.start_host_request("b", json!({"type":"wait"}));
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(2), started.recv())
                .await
                .unwrap()
                .unwrap();
        }
        inner.cancel_host_request("unknown");
        inner.cancel_host_request("a");
        inner.cancel_host_request("a");
        tokio::time::timeout(Duration::from_secs(2), dropped.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            lock(&inner.guarded)
                .host_inflight
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        inner.cancel_host_request("a");
        inner.start_host_request("a", json!({"type":"wait"}));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        inner.cancel_host_requests();
        tokio::time::timeout(
            Duration::from_secs(2),
            inner.wait_for_host_requests_to_settle(1_000),
        )
        .await
        .unwrap();
        assert!(lock(&inner.guarded).host_inflight.is_empty());
    }

    #[tokio::test]
    async fn dropping_last_manager_aborts_handler_instead_of_leaking_kernel_owner() {
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "wait",
            host_handler(move |_| {
                let started = started_tx.clone();
                let dropped = dropped_tx.clone();
                async move {
                    let _notice = DropNotice(dropped);
                    started.send(()).unwrap();
                    std::future::pending().await
                }
            }),
        );
        let manager = ReplKernelManager::new(KernelManagerOptions {
            host_handlers: handlers,
            ..Default::default()
        });
        let owner = Arc::downgrade(&manager.inner);
        manager
            .inner
            .start_host_request("a", json!({"type":"wait"}));
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .unwrap()
            .unwrap();
        drop(manager);
        tokio::time::timeout(Duration::from_secs(2), dropped.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(owner.upgrade().is_none());
    }

    #[tokio::test]
    async fn completion_failure_panic_and_pre_poll_cancel_remove_task_entries() {
        let mut handlers = HostRequestHandlers::new();
        handlers.register("ok", host_handler(|_| async { Ok(json!(42)) }));
        handlers.register("panic", host_handler(|_| async { panic!("handler panic") }));
        let manager = ReplKernelManager::new(KernelManagerOptions {
            host_handlers: handlers,
            ..Default::default()
        });
        let inner = &manager.inner;
        inner.start_host_request("ok", json!({"type":"ok"}));
        inner.start_host_request("missing", json!({"type":"unregistered"}));
        inner.start_host_request("panic", json!({"type":"panic"}));
        inner.start_host_request("cancel", json!({"type":"ok"}));
        inner.cancel_host_request("cancel");
        tokio::time::timeout(
            Duration::from_secs(2),
            inner.wait_for_host_requests_to_settle(1_000),
        )
        .await
        .unwrap();
        assert!(lock(&inner.guarded).host_inflight.is_empty());
        inner.cancel_host_request("ok");
        inner.cancel_host_request("missing");
        inner.cancel_host_request("panic");
        inner.cancel_host_request("cancel");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_large_reply_finishes_admitted_frame_and_keeps_stdin_reusable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gate.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let python = dir.path().join("backpressured-kernel");
        let script = r"#!/usr/bin/env python3
import json, socket, sys
barrier = socket.socket(socket.AF_UNIX)
barrier.connect(SOCKET_PATH)
def send(frame):
    print(json.dumps(frame), flush=True)
send({'event': 'ready', 'protocol': 3, 'python': '3.14.0'})
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'execute':
        if request['code'] == 'large':
            send({'event': 'host_request', 'id': 'big', 'data': {'type': 'large'}})
            prefix = sys.stdin.read(32)
            send({'event': 'host_cancel', 'id': 'big'})
            send({'event': 'host_request', 'id': 'ack', 'data': {'type': 'ack'}})
            # The test releases this only after native cancellation is observed.
            # The 2 MiB writer is blocked with an already-read frame prefix.
            assert barrier.recv(1) == b'!'
            reply = json.loads(prefix + sys.stdin.readline())
            assert reply['id'] == 'big'
            assert len(reply['data']['result']) == 2 * 1024 * 1024
            text = 'framed'
        else:
            text = 'next'
        send({'event': 'result', 'id': request['id'], 'text': text})
        send({'event': 'done', 'id': request['id'], 'status': 'ok'})
    elif request['type'] == 'shutdown':
        send({'event': 'done', 'id': request['id'], 'status': 'ok'})
        break
";
        std::fs::write(
            &python,
            super::fixture_executable::script(
                &script.replace("SOCKET_PATH", &serde_json::to_string(&socket).unwrap()),
                "python3",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (ack_tx, mut ack) = mpsc::unbounded_channel();
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "large",
            host_handler(|_| async { Ok(json!("x".repeat(2 * 1024 * 1024))) }),
        );
        handlers.register(
            "ack",
            host_handler(move |_| {
                let ack = ack_tx.clone();
                async move {
                    ack.send(()).unwrap();
                    Ok(Value::Null)
                }
            }),
        );
        let manager = ReplKernelManager::new(KernelManagerOptions {
            python: Some(python),
            host_handlers: handlers,
            ..Default::default()
        });
        let executing = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .execute("large", crate::kernel::shared::ExecuteOptions::default())
                    .await
            })
        };
        let (mut gate, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), ack.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            lock(&manager.inner.guarded)
                .host_inflight
                .get("big")
                .unwrap()
                .is_aborted(),
            "cancel processed while the reply is partially written"
        );
        gate.write_all(b"!").await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), executing).await;
        manager.inner.cancel_host_request("ack");
        let result = match result {
            Ok(result) => result.unwrap().unwrap(),
            Err(error) => {
                manager.kill();
                panic!("cancel tore an admitted reply frame: {error}")
            }
        };
        assert_eq!(result.result.as_deref(), Some("framed"));
        let pid = manager.process_id();
        let next = tokio::time::timeout(
            Duration::from_secs(3),
            manager.execute("next", crate::kernel::shared::ExecuteOptions::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(next.result.as_deref(), Some("next"));
        assert_eq!(manager.process_id(), pid);
        manager.kill();
    }
}
