//! Single-flight, bounded explicit refresh of ready resident workers only.
use super::{
    anyhow, bail, join_all, json, util, worker_lifecycle, Arc, DaemonWorkerLifecycle, Duration,
    ResidentWorker, Result, RouteAdmission, Supervisor, Value,
};
use pa_types::model_refresh::{
    ModelRefreshApplyReport, ModelRefreshReason, ModelRefreshReport, ModelRefreshSourceResult,
    ModelRefreshStatus, ModelRefreshWorkers,
};
use tokio::time::Instant;

const WORKER_TIMEOUT: Duration = Duration::from_secs(25);
const SWEEP_TIMEOUT: Duration = Duration::from_secs(110);
const FANOUT_CONCURRENCY: usize = 4;

impl Supervisor {
    pub(super) async fn refresh_resident_models(self: &Arc<Self>) -> ModelRefreshReport {
        let mut receiver = {
            let mut pending = self.model_refresh.lock().expect("model refresh lock");
            if let Some(receiver) = pending
                .as_ref()
                .filter(|receiver| receiver.borrow().is_none())
            {
                receiver.clone()
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                *pending = Some(receiver.clone());
                let supervisor = Arc::clone(self);
                tokio::spawn(async move {
                    let report = supervisor.run_resident_model_refresh().await;
                    // Publish before clearing: waiters retain their own receiver.
                    let mut pending = supervisor.model_refresh.lock().expect("model refresh lock");
                    pending.take();
                    sender.send_replace(Some(report));
                });
                receiver
            }
        };
        loop {
            if let Some(report) = receiver.borrow_and_update().clone() {
                return report;
            }
            // The sender belongs to the sweep task, not any client connection.
            receiver
                .changed()
                .await
                .expect("model refresh task completed");
        }
    }

    async fn refresh_worker_ready(&self, worker: &Arc<ResidentWorker>) -> bool {
        let state = worker.route_state();
        if !state.connected || !state.session_ready || state.retired || self.is_stopping(worker) {
            return false;
        }
        let descriptor = worker.descriptor.lock().await;
        if descriptor.lifecycle != DaemonWorkerLifecycle::Ready || descriptor.pid == 0 {
            return false;
        }
        let (pid, start) = (descriptor.pid, descriptor.process_start_id.clone());
        drop(descriptor);
        !worker_lifecycle::worker_process_gone(pid, start).await
            && worker.cmd_tx.lock().await.is_some()
    }

    async fn model_refresh_request<T: serde::de::DeserializeOwned>(
        &self,
        worker: &Arc<ResidentWorker>,
        command: &str,
        mut payload: Value,
        deadline: Instant,
    ) -> Result<T> {
        if Instant::now() >= deadline
            || !self.refresh_worker_ready(worker).await
            || !worker.supports_model_refresh()
        {
            bail!("Resident worker unavailable");
        }
        payload["activeSessionId"] = json!(worker.descriptor.lock().await.root_active_session_id);
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(WORKER_TIMEOUT);
        if remaining.is_zero() {
            bail!("Model refresh sweep deadline elapsed");
        }
        let response = self
            .route_command_typed(
                worker,
                command,
                payload,
                remaining.as_millis() as u64,
                RouteAdmission::SupervisorInternal,
            )
            .await?;
        if !response.success {
            bail!("Resident worker rejected model refresh");
        }
        Ok(serde_json::from_value(
            response
                .data
                .ok_or_else(|| anyhow!("Missing refresh report"))?,
        )?)
    }

    async fn run_resident_model_refresh(&self) -> ModelRefreshReport {
        let started_at = util::now_ms();
        let deadline = Instant::now() + SWEEP_TIMEOUT;
        let mut workers = Vec::new();
        for worker in self.registry.list().await {
            if self.refresh_worker_ready(&worker).await {
                workers.push(worker);
            }
        }
        workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        let mut sources = Vec::new();
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut leader = None;
        for (index, worker) in workers.iter().enumerate() {
            match self
                .model_refresh_request::<ModelRefreshReport>(
                    worker,
                    "refresh_models",
                    json!({}),
                    deadline,
                )
                .await
            {
                Ok(report) => {
                    record_sources(&mut sources, report.sources);
                    succeeded += 1;
                    leader = Some(index);
                    break;
                }
                Err(_) => failed += 1,
            }
        }
        if let Some(leader) = leader {
            for batch in workers[leader + 1..].chunks(FANOUT_CONCURRENCY) {
                let results = join_all(batch.iter().map(|worker| async {
                    let snapshot: ModelRefreshApplyReport = self
                        .model_refresh_request(
                            worker,
                            "apply_model_snapshots",
                            json!({"since": started_at}),
                            deadline,
                        )
                        .await?;
                    let mut results = Vec::new();
                    if !snapshot.missing.is_empty() {
                        let refreshed: ModelRefreshReport = self
                            .model_refresh_request(
                                worker,
                                "refresh_models",
                                json!({"sources": snapshot.missing}),
                                deadline,
                            )
                            .await?;
                        results = refreshed.sources;
                        self.model_refresh_request::<ModelRefreshApplyReport>(
                            worker,
                            "apply_model_snapshots",
                            json!({"since": started_at}),
                            deadline,
                        )
                        .await?;
                    }
                    Ok::<_, anyhow::Error>(results)
                }))
                .await;
                for result in results {
                    match result {
                        Ok(results) => {
                            record_sources(&mut sources, results);
                            succeeded += 1;
                        }
                        Err(_) => failed += 1,
                    }
                }
            }
        }
        if failed > 0 {
            // Do not expose worker identities, provider errors, or credentials.
            let result = ModelRefreshSourceResult {
                source: "daemon-workers".into(),
                status: ModelRefreshStatus::Failed,
                reason: Some(ModelRefreshReason::WorkerUnavailable),
                model_count: None,
            };
            record_sources(&mut sources, vec![result]);
        }
        ModelRefreshReport {
            started_at,
            finished_at: util::now_ms(),
            sources,
            workers: Some(ModelRefreshWorkers {
                total: workers.len(),
                succeeded,
                failed,
            }),
        }
    }
}

fn record_sources(
    target: &mut Vec<ModelRefreshSourceResult>,
    results: Vec<ModelRefreshSourceResult>,
) {
    let rank = |status: &ModelRefreshStatus| match status {
        ModelRefreshStatus::Failed => 5,
        ModelRefreshStatus::Stale => 4,
        ModelRefreshStatus::Updated => 3,
        ModelRefreshStatus::Unchanged => 2,
        ModelRefreshStatus::Skipped => 1,
    };
    for mut result in results {
        // A model count may refer to a different credential scope on each worker.
        result.model_count = None;
        if let Some(previous) = target
            .iter_mut()
            .find(|previous| previous.source == result.source)
        {
            if rank(&result.status) > rank(&previous.status) {
                *previous = result;
            }
        } else {
            target.push(result);
        }
    }
}

#[cfg(test)]
mod tests;
