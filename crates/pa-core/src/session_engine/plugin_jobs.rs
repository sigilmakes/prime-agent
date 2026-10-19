//! Generation-owned deferred invocations. Admission is intentionally detached from
//! the observer; execution waits for quiescence and never retargets a replacement.
use crate::plugins::{
    CapabilityFuture, CapabilityRequest, ContributionKind, HostCapabilities, InvocationEvent,
    InvocationOutcome, PluginHost,
};
use crate::session::manager::SessionManager;
use pa_agent::agent::Agent;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) struct PluginJobs {
    host: Weak<PluginHost>,
    agent: Weak<Agent>,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
    command_barrier: Arc<tokio::sync::Mutex<()>>,
    settled: Arc<tokio::sync::Notify>,
}
struct Job {
    plugin: String,
    generation: Uuid,
    cancellation: CancellationToken,
    state: Value,
}
impl PluginJobs {
    pub(super) fn new(
        host: &Arc<PluginHost>,
        agent: &Arc<Agent>,
        session: Arc<tokio::sync::Mutex<SessionManager>>,
        command_barrier: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            host: Arc::downgrade(host),
            agent: Arc::downgrade(agent),
            session,
            jobs: Arc::default(),
            command_barrier,
            settled: Arc::default(),
        }
    }
    pub(super) fn checkpoint(&self) -> Vec<String> {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
    pub(super) async fn drain(&self) -> Vec<Value> {
        loop {
            let changed = self.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let terminal = {
                let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
                if jobs
                    .values()
                    .any(|job| job.state["status"] == "queued" || job.state["status"] == "running")
                {
                    None
                } else {
                    Some(jobs.iter().map(|(id,job)|json!({"id":id,"plugin":job.plugin,"generation":job.generation.to_string(),"state":job.state})).collect())
                }
            };
            if let Some(terminal) = terminal {
                return terminal;
            }
            changed.await;
        }
    }
}
impl Drop for PluginJobs {
    fn drop(&mut self) {
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            job.cancellation.cancel();
        }
    }
}
impl HostCapabilities for PluginJobs {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let host = self.host.clone();
        let agent = self.agent.clone();
        let session = Arc::clone(&self.session);
        let jobs = Arc::clone(&self.jobs);
        let command_barrier = Arc::clone(&self.command_barrier);
        let settled = Arc::clone(&self.settled);
        Box::pin(async move {
            let host = host.upgrade().ok_or("session ended")?;
            if cancellation.is_cancelled() {
                return Err("cancelled".into());
            }
            match request.operation.as_str() {
                "jobs.enqueue" => {
                    let name = request.input["hook"].as_str().ok_or("job hook required")?;
                    let target = host
                        .snapshot()
                        .contributions
                        .iter()
                        .find(|target| {
                            target.plugin == request.plugin
                                && target.generation == request.generation
                                && target.kind == ContributionKind::Hook
                                && target.name == name
                        })
                        .cloned()
                        .ok_or("job target is not active")?;
                    let retirement = host.retirement(&target).map_err(|e| e.to_string())?;
                    let branch = match command_barrier.try_lock().ok() {
                        Some(_permit) => {
                            Some(session.lock().await.get_leaf_id().map(str::to_owned))
                        }
                        None => None,
                    };
                    let id = Uuid::new_v4().to_string();
                    let stop = CancellationToken::new();
                    jobs.lock().unwrap_or_else(PoisonError::into_inner).insert(
                        id.clone(),
                        Job {
                            plugin: request.plugin,
                            generation: request.generation,
                            cancellation: stop.clone(),
                            state: json!({"status":"queued"}),
                        },
                    );
                    let input = request
                        .input
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let job_id = id.clone();
                    let host = Arc::downgrade(&host);
                    tokio::spawn(async move {
                        let result: Result<Value, String> = async {
                            let agent = agent.upgrade().ok_or("session ended")?;
                            let permit = tokio::select! {biased;
                                ()=retirement.cancelled()=>return Err("retired".into()),
                                ()=stop.cancelled()=>return Err("cancelled".into()),
                                permit=command_barrier.lock_owned()=>permit,
                            };
                            let branch = match branch {
                                Some(branch) => branch,
                                None => session.lock().await.get_leaf_id().map(str::to_owned),
                            };
                            drop(permit);
                            tokio::select! {biased;
                                ()=retirement.cancelled()=>return Err("retired".into()),
                                ()=stop.cancelled()=>return Err("cancelled".into()),
                                ()=agent.wait_for_idle()=>{}
                            }
                            if session.lock().await.get_leaf_id().map(str::to_owned) != branch {
                                return Err("branch changed before job execution".into());
                            }
                            jobs.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get_mut(&job_id)
                                .ok_or("job disappeared")?
                                .state = json!({"status":"running"});
                            let host = host.upgrade().ok_or("session ended")?;
                            let mut invocation = host
                                .invoke(&target, input, &stop)
                                .map_err(|e| e.to_string())?;
                            drop(host);
                            drop(agent);
                            while let Some(event) = invocation.next().await {
                                match event {
                                    InvocationEvent::Finished(Ok(
                                        InvocationOutcome::Unary(value)
                                        | InvocationOutcome::Streamed(value),
                                    )) => return Ok(value),
                                    InvocationEvent::Finished(Err(error)) => {
                                        return Err(error.to_string());
                                    }
                                    InvocationEvent::Update(_) => {}
                                }
                            }
                            Err("job ended without result".into())
                        }
                        .await;
                        if let Some(job) = jobs
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .get_mut(&job_id)
                        {
                            job.state = match result {
                                Ok(value) => json!({"status":"completed","result":value}),
                                Err(error) => json!({"status":"failed","error":error}),
                            };
                        }
                        settled.notify_waiters();
                    });
                    Ok(json!({"scheduled":true,"id":id}))
                }
                "jobs.status" | "jobs.cancel" => {
                    let id = request.input["id"].as_str().ok_or("job id required")?;
                    let mut jobs = jobs.lock().unwrap_or_else(PoisonError::into_inner);
                    let job = jobs.get_mut(id).ok_or("unknown job")?;
                    if job.plugin != request.plugin || job.generation != request.generation {
                        return Err("job generation mismatch".into());
                    }
                    if request.operation == "jobs.cancel" {
                        job.cancellation.cancel();
                    }
                    Ok(job.state.clone())
                }
                _ => Err("unsupported job operation".into()),
            }
        })
    }
}
