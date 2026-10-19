use super::migration::{migrate, LegacySource};
use super::store::{ExpectedRevision, Record, RecordIdentity, Store};
use crate::plugins::{CapabilityFuture, CapabilityRequest, HostCapabilities, PluginHost};
use crate::session::manager::SessionManager;
use anyhow::{bail, Context};
use pa_agent::agent::Agent;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub(crate) struct HarnessStorage {
    store: Arc<Store>,
    global: PathBuf,
    local: Option<PathBuf>,
    session_id: String,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    host: Weak<PluginHost>,
    agent: Weak<Agent>,
    migrated: Arc<tokio::sync::Mutex<bool>>,
}
impl HarnessStorage {
    pub(crate) fn new(
        agent_dir: &std::path::Path,
        local: Option<PathBuf>,
        session_id: String,
        session: Arc<tokio::sync::Mutex<SessionManager>>,
        host: &Arc<PluginHost>,
        agent: &Arc<Agent>,
    ) -> Self {
        Self {
            store: Arc::new(Store::new(agent_dir.join("plugins/harness/store.json"))),
            global: agent_dir.join("harness"),
            local,
            session_id,
            session,
            host: Arc::downgrade(host),
            agent: Arc::downgrade(agent),
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
        let session = Arc::clone(&self.session);
        let host = self.host.clone();
        let agent = self.agent.clone();
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
                let snapshot = session.lock().await.history_snapshot();
                let canonical = snapshot
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter_map(|row| {
                        let value = serde_json::to_value(row).ok()?;
                        (value["type"] == "custom"
                            && value["customType"] == "prime-agent.refinement")
                            .then_some(value)
                    })
                    .collect();
                let mut sources = vec![LegacySource::global(
                    Some(global.join("harness_state.json")),
                    vec![
                        global.join("refinements.jsonl"),
                        global.join("refinement_history.jsonl"),
                    ],
                )];
                sources.push(
                    LegacySource::session(
                        &session_id,
                        local.map(|dir| dir.join("harness_state.json")),
                        vec![],
                        canonical,
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
            // A snapshot's branch is accepted only while the session is quiescent.
            // Hold the session identity lock through the transaction, and generation
            // authority through the atomic publication. Neither check is advisory.
            let session_guard = session.lock().await;
            if request.operation == "harness.store.apply" {
                if session_guard.get_leaf_id().map(str::to_owned)
                    != request
                        .input
                        .get("branch")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                {
                    return Err("branch changed before apply".into());
                }
                let agent = agent.upgrade().ok_or("session ended")?;
                if agent.state().await.is_streaming {
                    return Err("session is not quiescent".into());
                }
            }
            let result=tokio::task::spawn_blocking(move|| -> anyhow::Result<Value> {
                host.with_current_contributions(&[&target],|| -> anyhow::Result<Value> {
                    if cancellation.is_cancelled() {bail!("cancelled");}
                    let input=&request.input;
                    match request.operation.as_str() {
                        "harness.store.enumerate"=>Ok(serde_json::to_value(store.enumerate(input.get("scope").and_then(Value::as_str))?)?),
                        "harness.store.inspect"=>store.inspect(),
                        "harness.store.rollback_legacy"=>Ok(serde_json::to_value(super::migration::rollback_legacy(
                            &store,input["scope"].as_str().context("scope required")?,input["id"].as_str().context("history id required")?)?)?),
                        "harness.store.apply"=>{
                            let scope=input["scope"].as_str().context("scope required")?;
                            let baseline:Vec<Record>=serde_json::from_value(input["baseline"].clone())?;
                            let edits=input["proposal"]["edits"].as_array().context("edits required")?;
                            let mut changes=Vec::new();
                            for edit in edits {
                                let key=RecordIdentity{scope:scope.into(),kind:edit["kind"].as_str().context("kind required")?.into(),id:edit["id"].as_str().context("id required")?.into()};
                                let old=baseline.iter().find(|record|record.identity==key);
                                let (value,expected)=match edit["action"].as_str().context("action required")? {
                                    "create"=>(Some(edit.get("value").context("value required")?.clone()),ExpectedRevision::Absent),
                                    "update"=>(Some(edit.get("value").context("value required")?.clone()),ExpectedRevision::Revision(old.context("update baseline missing")?.revision)),
                                    "delete"=>(None,ExpectedRevision::Revision(old.context("delete baseline missing")?.revision)),
                                    _=>bail!("unknown edit action"),
                                };
                                changes.push((key,value,expected));
                            }
                            Ok(json!({"applied":store.batch(changes)?,"summary":input["proposal"]["summary"]}))
                        }
                        operation=>{
                            let key:RecordIdentity=serde_json::from_value(input["identity"].clone())?;
                            match operation {
                                "harness.store.read"=>Ok(serde_json::to_value(store.read(&key)?)?),
                                "harness.store.put"=>Ok(serde_json::to_value(store.put(&key,input.get("value").context("value required")?.clone(),expected(input)?)?)?),
                                "harness.store.delete"=>Ok(serde_json::to_value(store.delete(&key,expected(input)?)?)?),
                                "harness.store.history"=>Ok(serde_json::to_value(store.history(&key)?)?),
                                "harness.store.rollback"=>Ok(serde_json::to_value(store.rollback(&key,input["revision"].as_u64().context("revision required")?,expected(input)?)?)?),
                                _=>bail!("unsupported harness storage operation"),
                            }
                        }
                    }
                }).map_err(anyhow::Error::from)?
            }).await.map_err(|e|e.to_string())?.map_err(|e|e.to_string());
            drop(session_guard);
            result
        })
    }
}
