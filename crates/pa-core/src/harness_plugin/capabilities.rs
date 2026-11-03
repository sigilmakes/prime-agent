use super::migration::{migrate, LegacySource};
use super::store::{ExpectedRevision, RecordIdentity, Store};
use crate::plugins::{CapabilityFuture, CapabilityRequest, HostCapabilities, PluginHost};
use crate::session::manager::SessionManager;
use anyhow::{bail, Context};
use pa_agent::agent::Agent;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub(crate) struct HarnessStorage {
    store: Arc<Store>,
    global: PathBuf,
    local: Option<PathBuf>,
    session_id: String,
    host: Weak<PluginHost>,
    migrated: Arc<tokio::sync::Mutex<bool>>,
}
impl HarnessStorage {
    pub(crate) fn new(
        agent_dir: &std::path::Path,
        local: Option<PathBuf>,
        session_id: String,
        _session: Arc<tokio::sync::Mutex<SessionManager>>,
        host: &Arc<PluginHost>,
        _agent: &Arc<Agent>,
    ) -> Self {
        Self {
            store: Arc::new(Store::new(agent_dir.join("plugins/harness/store.json"))),
            global: agent_dir.join("harness"),
            local,
            session_id,
            host: Arc::downgrade(host),
            migrated: Arc::default(),
        }
    }
}
fn expected(input: &Value) -> anyhow::Result<ExpectedRevision> {
    match input.get("expected") {
        None | Some(Value::String(_)) if input.get("expected").is_none_or(|v| v == "any") => {
            Ok(ExpectedRevision::Any)
        }
        Some(Value::String(value)) if value == "absent" => Ok(ExpectedRevision::Absent),
        Some(Value::Number(value)) => Ok(ExpectedRevision::Revision(
            value.as_u64().context("invalid revision")?,
        )),
        _ => bail!("invalid expected revision"),
    }
}
impl HostCapabilities for HarnessStorage {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let store = Arc::clone(&self.store);
        let global = self.global.clone();
        let local = self.local.clone();
        let session_id = self.session_id.clone();
        let host = self.host.clone();
        let migrated = Arc::clone(&self.migrated);
        Box::pin(async move {
            let host = host.upgrade().ok_or("session ended")?;
            if request.plugin != "harness" || request.session != session_id {
                return Err("harness storage owner mismatch".into());
            }
            let target = host
                .snapshot()
                .contributions
                .iter()
                .find(|c| c.plugin == request.plugin && c.generation == request.generation)
                .cloned()
                .ok_or("retired harness")?;
            let mut initialized = migrated.lock().await;
            if !*initialized {
                let mut sources = vec![LegacySource::global(Some(
                    global.join("harness_state.json"),
                ))];
                sources.push(
                    LegacySource::session(
                        &session_id,
                        local.map(|dir| dir.join("harness_state.json")),
                    )
                    .map_err(|e| e.to_string())?,
                );
                let store = Arc::clone(&store);
                let host = Arc::clone(&host);
                let target = target.clone();
                let cancellation = cancellation.clone();
                tokio::task::spawn_blocking(move || {
                    host.with_current_contributions(&[&target], || {
                        if cancellation.is_cancelled() {
                            bail!("cancelled");
                        }
                        migrate(&store, &sources)
                    })
                    .map_err(anyhow::Error::from)?
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                *initialized = true;
            }
            drop(initialized);
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
                host.with_current_contributions(&[&target], || -> anyhow::Result<Value> {
                    if cancellation.is_cancelled() {
                        bail!("cancelled");
                    }
                    let input = &request.input;
                    match request.operation.as_str() {
                        "harness.store.enumerate" => Ok(serde_json::to_value(
                            store.enumerate(input.get("scope").and_then(Value::as_str))?,
                        )?),
                        "harness.store.inspect" => store.inspect(),
                        operation => {
                            let key: RecordIdentity =
                                serde_json::from_value(input["identity"].clone())?;
                            match operation {
                                "harness.store.read" => {
                                    Ok(serde_json::to_value(store.read(&key)?)?)
                                }
                                "harness.store.put" => Ok(serde_json::to_value(store.put(
                                    &key,
                                    input.get("value").context("value required")?.clone(),
                                    expected(input)?,
                                )?)?),
                                "harness.store.delete" => {
                                    Ok(serde_json::to_value(store.delete(&key, expected(input)?)?)?)
                                }
                                "harness.store.history" => {
                                    Ok(serde_json::to_value(store.history(&key)?)?)
                                }
                                "harness.store.rollback" => {
                                    Ok(serde_json::to_value(store.rollback(
                                        &key,
                                        input["revision"].as_u64().context("revision required")?,
                                        expected(input)?,
                                    )?)?)
                                }
                                _ => bail!("unsupported harness storage operation"),
                            }
                        }
                    }
                })
                .map_err(anyhow::Error::from)?
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string());
            result
        })
    }
}
