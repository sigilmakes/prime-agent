//! Explicit session-scoped history and auxiliary inference capabilities.
use super::inference_router::AuxiliaryInference;
use crate::plugins::{CapabilityFuture, CapabilityRequest, HostCapabilities};
use crate::session::manager::SessionManager;
use pa_agent::agent::Agent;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub(super) struct PluginServices {
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    agent: Weak<Agent>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    inference: Weak<AuxiliaryInference>,
}
impl PluginServices {
    pub(super) fn new(
        session: Arc<tokio::sync::Mutex<SessionManager>>,
        agent: &Arc<Agent>,
        cwd: PathBuf,
        agent_dir: PathBuf,
        inference: &Arc<AuxiliaryInference>,
    ) -> Self {
        Self {
            session,
            agent: Arc::downgrade(agent),
            cwd,
            agent_dir,
            inference: Arc::downgrade(inference),
        }
    }
}
impl HostCapabilities for PluginServices {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let session = Arc::clone(&self.session);
        let agent = self.agent.clone();
        let cwd = self.cwd.clone();
        let agent_dir = self.agent_dir.clone();
        let inference = self.inference.clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err("cancelled".into());
            }
            match request.operation.as_str() {
                "session.snapshot" => {
                    let (snapshot, branch) = {
                        let session = session.lock().await;
                        if session.get_session_id() != request.session {
                            return Err("session mismatch".into());
                        }
                        (
                            session.history_snapshot(),
                            session.get_leaf_id().map(str::to_owned),
                        )
                    };
                    let history = snapshot.await.map_err(|e| e.to_string())?;
                    Ok(json!({"history":history,"branch":branch}))
                }
                "inference.complete" => {
                    let agent = agent.upgrade().ok_or("session ended")?;
                    let state = agent.state().await;
                    let model = &state.model;
                    let full =
                        super::provider_adapter::native_model(model).map_err(|e| e.to_string())?;
                    let purpose = request.input["purpose"]
                        .as_str()
                        .unwrap_or("plugin auxiliary inference")
                        .to_owned();
                    let selector = request
                        .input
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let routed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        let context =
                            super::auxiliary_model::AuxiliaryModelContext { cwd, agent_dir };
                        match selector {
                            Some(selector) => {
                                super::auxiliary_model::resolve_selected_auxiliary_model(
                                    &context, &selector,
                                )
                            }
                            None => Ok(super::auxiliary_model::resolve_auxiliary_model(
                                &context, &purpose, &full, None, None,
                            )),
                        }
                    })
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                    let stop = pa_agent::abort::AbortController::new();
                    let signal = stop.signal();
                    let context = pa_types::ai::Context {
                        system_prompt: request.input["system"].as_str().map(str::to_owned),
                        messages: vec![pa_types::ai::Message::User(pa_types::ai::UserMessage {
                            content: pa_types::ai::UserContent::Text(
                                request.input["input"].to_string(),
                            ),
                            timestamp: 0,
                            rest: serde_json::Map::default(),
                        })],
                        tools: None,
                    };
                    let inference = inference.upgrade().ok_or("session ended")?;
                    let work = inference.complete(
                        &routed.model,
                        context,
                        pa_agent::stream::StreamRequestOptions {
                            signal,
                            api_key: routed.api_key,
                            headers: routed.headers,
                            ..Default::default()
                        },
                        None,
                    );
                    let work = super::provider_adapter::with_bounded_native_events(work);
                    let result = tokio::select! { biased;
                        () = cancellation.cancelled() => {stop.abort(); return Err("cancelled".into());},
                        result = work => result.map_err(|e|e.to_string())?,
                    };
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            pa_types::ai::AssistantContentBlock::Text(block) => {
                                Some(block.text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<String>();
                    Ok(
                        json!({"text":text,"model":format!("{}/{}",routed.model.provider,routed.model.id)}),
                    )
                }
                _ => Err("unsupported session capability".into()),
            }
        })
    }
}
