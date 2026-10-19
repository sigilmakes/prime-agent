//! Deterministic, per-engine provider fixtures for the two distinct abort contracts.
//! This module is reachable only through the daemon's cfg(test) test tree.
use super::super::{AgentSessionEngine, TestStreamDecorator};
use crate::engine::SessionEngine;
use pa_agent::stream::{AssistantMessageEvent, ModelStream, StreamFn};
use pa_agent::types::{AssistantMessage, Model, StopReason, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(10);
pub(crate) const NATIVE_RESPONSE_ID: &str = "abort-fixture-native-empty";

#[derive(Clone, Copy)]
pub(crate) enum AbortCase {
    BeforeStream,
    NativeTerminal,
}

struct State {
    admitted: Notify,
    built: Notify,
    release: Notify,
    calls: AtomicUsize,
    identity: std::sync::Mutex<Option<(String, String, String)>>,
}

/// Keeps decoration and cancellation local to one engine. The decorator captures
/// only state/model identity/normal stream, never this guard or an engine/worker.
pub(crate) struct AbortFixture {
    engine: Arc<AgentSessionEngine>,
    state: Arc<State>,
}

impl AbortFixture {
    pub(crate) fn install(engine: &Arc<AgentSessionEngine>, case: AbortCase) -> Self {
        assert!(
            engine
                .session
                .try_lock()
                .expect("uncontended fresh session")
                .is_none(),
            "install the fixture before create or any SDK build"
        );
        let state = Arc::new(State {
            admitted: Notify::new(),
            built: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            identity: std::sync::Mutex::new(None),
        });
        let captured = Arc::clone(&state);
        let decorator: TestStreamDecorator = Arc::new(move |normal: StreamFn| {
            let state = Arc::clone(&captured);
            let stream_fn: StreamFn = Arc::new(
                move |model: Model,
                      context: pa_agent::stream::LlmContext,
                      options: pa_agent::stream::StreamRequestOptions| {
                    let state = Arc::clone(&state);
                    let normal = Arc::clone(&normal);
                    Box::pin(async move {
                        let identity =
                            (model.api.clone(), model.provider.clone(), model.id.clone());
                        let call = state.calls.fetch_add(1, Ordering::SeqCst);
                        {
                            let mut first = state.identity.lock().expect("fixture identity lock");
                            if call == 0 {
                                // Bind only when the normal provider is actually invoked.
                                // Installation never resolves a model or registers Faux.
                                *first = Some(identity);
                            } else {
                                anyhow::ensure!(
                                    first.as_ref() == Some(&identity),
                                    "fixture received another model"
                                );
                            }
                        }
                        match call {
                            // Preserve the real first response and its nonzero usage.
                            0 => normal(model, context, options).await,
                            1 => {
                                anyhow::ensure!(
                                    !options.signal.is_aborted(),
                                    "provider admitted after abort"
                                );
                                // This executes only when the actual provider future is polled.
                                state.admitted.notify_one();
                                match case {
                                    AbortCase::BeforeStream => {
                                        options.signal.aborted().await;
                                        Err(pa_agent::abort::aborted_error())
                                    }
                                    AbortCase::NativeTerminal => {
                                        state.release.notified().await;
                                        anyhow::ensure!(
                                            !options.signal.is_aborted(),
                                            "native control was externally aborted"
                                        );
                                        let (sender, stream) = pa_agent::stream::event_stream();
                                        sender.push(AssistantMessageEvent::Error {
                                            reason: StopReason::Aborted,
                                            error: native_error_message(&model),
                                        });
                                        sender.end(None);
                                        Ok(Box::new(stream) as Box<dyn ModelStream>)
                                    }
                                }
                            }
                            _ => anyhow::bail!("unexpected additional provider call"),
                        }
                    })
                },
            );
            captured.built.notify_one();
            stream_fn
        });
        let mut slot = engine
            .test_stream_decorator
            .lock()
            .expect("test decorator lock");
        assert!(slot.is_none(), "another decorator already owns this engine");
        *slot = Some(decorator);
        drop(slot);
        Self {
            engine: Arc::clone(engine),
            state,
        }
    }

    /// Fence the ordinary create-owned background SDK build, without resolving
    /// or registering any provider from fixture installation or readiness.
    pub(crate) async fn built(&self) -> anyhow::Result<()> {
        tokio::time::timeout(WAIT, async {
            self.state.built.notified().await;
            let _build = self.engine.session_build.lock().await;
            anyhow::ensure!(
                self.engine.session.lock().await.is_some(),
                "create build did not adopt SDK session"
            );
            Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("create SDK build did not settle within 10s"))?
    }

    pub(crate) async fn admitted(&self) -> anyhow::Result<()> {
        tokio::time::timeout(WAIT, self.state.admitted.notified())
            .await
            .map_err(|_| anyhow::anyhow!("the second actual provider call was not admitted"))
    }

    pub(crate) fn release(&self) {
        self.state.release.notify_one();
    }

    pub(crate) fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        self.engine.abort_in_flight_turn();
        self.release();
    }

    /// The synchronous test owns this run thread and explicitly settles it
    /// before making readiness/shape assertions. No blocking join runs in Drop.
    pub(crate) fn spawn(&self, run: impl FnOnce() + Send + 'static) -> OwnedTurn {
        let (completed, completion) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
            let _ = completed.send(outcome);
        });
        OwnedTurn {
            engine: Arc::clone(&self.engine),
            state: Arc::clone(&self.state),
            thread: Some(thread),
            completion,
        }
    }
}

impl Drop for AbortFixture {
    fn drop(&mut self) {
        self.cancel();
        self.engine
            .test_stream_decorator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

pub(crate) struct OwnedTurn {
    engine: Arc<AgentSessionEngine>,
    state: Arc<State>,
    thread: Option<std::thread::JoinHandle<()>>,
    completion: mpsc::Receiver<std::thread::Result<()>>,
}

impl OwnedTurn {
    pub(crate) fn finish(mut self) -> anyhow::Result<()> {
        // Both admission success and failure use the same bounded cleanup.
        self.engine.abort_in_flight_turn();
        self.state.release.notify_one();
        let outcome = self
            .completion
            .recv_timeout(WAIT)
            .map_err(|error| anyhow::anyhow!("held run did not settle within 10s: {error}"))?;
        // Completion is sent only after run_prompt returned or unwound. There
        // is no further async/provider work on this thread after the send.
        self.thread
            .take()
            .expect("owned run thread")
            .join()
            .map_err(|_| anyhow::anyhow!("held run thread panicked after completion"))?;
        outcome.map_err(|_| anyhow::anyhow!("held run panicked"))
    }
}

impl Drop for OwnedTurn {
    fn drop(&mut self) {
        self.engine.abort_in_flight_turn();
        self.state.release.notify_one();
        // finish owns the bounded join. Drop only signals cancellation: a panic
        // must never block an async test thread on an OS-thread join.
    }
}

fn native_error_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(), // NEGATIVE-CONTROL: mutate only this provider payload.
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: Some(NATIVE_RESPONSE_ID.to_owned()),
        diagnostics: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Aborted,
        stop_reason_raw: None,
        error_message: Some("Request was aborted".to_owned()),
        timestamp: 1_234,
    }
}
