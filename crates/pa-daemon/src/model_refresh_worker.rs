//! Explicit model-catalog refresh commands. Network work stays in the registry;
//! the selected session model adopts metadata at the next safe turn boundary.

use serde::Deserialize;
use serde_json::Value;

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

#[derive(Default, Deserialize)]
struct RefreshModelsPayload {
    sources: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
struct ApplyModelSnapshotsPayload {
    since: Option<u64>,
}

impl Worker {
    pub(crate) async fn handle_refresh_models(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("refresh_models") {
            return response;
        }
        let payload = match serde_json::from_value::<RefreshModelsPayload>(payload.clone()) {
            Ok(payload) => payload,
            Err(error) => {
                return response_failure(None, "refresh_models", &error.to_string(), None)
            }
        };
        if let Some(engine) = &self.agent_engine {
            engine.capture_model_metadata_refresh_anchor();
        }
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        let report = registry
            .refresh_catalog_sources(payload.sources.as_deref())
            .await;
        if let Some(engine) = &self.agent_engine {
            engine.schedule_model_metadata_refresh();
        }
        match serde_json::to_value(report) {
            Ok(report) => response_success(None, "refresh_models", Some(report)),
            Err(error) => response_failure(None, "refresh_models", &error.to_string(), None),
        }
    }

    pub(crate) async fn handle_apply_model_snapshots(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("apply_model_snapshots") {
            return response;
        }
        let payload = match serde_json::from_value::<ApplyModelSnapshotsPayload>(payload.clone()) {
            Ok(payload) => payload,
            Err(error) => {
                return response_failure(None, "apply_model_snapshots", &error.to_string(), None)
            }
        };
        if let Some(engine) = &self.agent_engine {
            engine.capture_model_metadata_refresh_anchor();
        }
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        // Disk-only by contract: this must never call the network refresh API.
        let report = registry.apply_catalog_refresh(payload.since).await;
        if let Some(engine) = &self.agent_engine {
            engine.schedule_model_metadata_refresh();
        }
        match serde_json::to_value(report) {
            Ok(report) => response_success(None, "apply_model_snapshots", Some(report)),
            Err(error) => response_failure(None, "apply_model_snapshots", &error.to_string(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::WorkerConfig;
    use serde_json::json;

    fn worker(dir: &std::path::Path) -> Worker {
        let worker = Worker::new(
            WorkerConfig {
                socket_path: dir.join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::default(),
                token: "refresh-test".into(),
                worker_instance_id: String::new(),
                active_session_id: "refresh-session".into(),
                agent_dir: dir.join("agent"),
                recovery_journal_path: dir.join("recovery.jsonl"),
                telemetry_disabled: Some(true),
                script: Some(json!({"responses": ["ack"]})),
            },
            None,
        );
        worker.core.lock().unwrap().created = true;
        worker
    }

    #[tokio::test]
    async fn worker_refresh_reports_are_direct_and_empty_sources_do_not_fetch() {
        let dir = tempfile::TempDir::new().unwrap();
        let worker = worker(dir.path());
        let response = worker
            .dispatch("refresh_models", &json!({"sources": []}))
            .await;
        assert!(response.success, "{response:?}");
        let report: pa_types::model_refresh::ModelRefreshReport =
            serde_json::from_value(response.data.unwrap()).unwrap();
        assert_eq!(report.sources, Vec::new());
        assert!(report.finished_at >= report.started_at);
        assert_eq!(report.workers, None);
    }

    #[tokio::test]
    async fn worker_apply_snapshots_is_allowed_while_busy_and_keeps_current_turn() {
        let dir = tempfile::TempDir::new().unwrap();
        let worker = worker(dir.path());
        worker.core.lock().unwrap().busy = true;
        let response = worker
            .dispatch("apply_model_snapshots", &json!({"since": u64::MAX}))
            .await;
        assert!(response.success, "{response:?}");
        let _: pa_types::model_refresh::ModelRefreshApplyReport =
            serde_json::from_value(response.data.unwrap()).unwrap();
        let core = worker.core.lock().unwrap();
        assert_eq!((core.busy, core.abort_requested), (true, false));
    }

    #[tokio::test]
    async fn refresh_commands_reject_invalid_payloads_without_changing_turn() {
        let dir = tempfile::TempDir::new().unwrap();
        let worker = worker(dir.path());
        for (command, payload) in [
            ("refresh_models", json!({"sources": "openai"})),
            ("apply_model_snapshots", json!({"since": -1})),
        ] {
            let response = worker.dispatch(command, &payload).await;
            assert!(!response.success, "{response:?}");
            assert_eq!(response.data, None);
        }
    }
}
