//! Credential-free results of explicit catalog refresh and local snapshot adoption.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelRefreshStatus {
    Updated,
    Unchanged,
    Skipped,
    Stale,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelRefreshReason {
    Offline,
    Unconfigured,
    AuthRejected,
    Network,
    InvalidCatalog,
    Cancelled,
    Superseded,
    Unsupported,
    CacheWrite,
    WorkerUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRefreshSourceResult {
    pub source: String,
    pub status: ModelRefreshStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ModelRefreshReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRefreshWorkers {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRefreshReport {
    pub started_at: u64,
    pub finished_at: u64,
    pub sources: Vec<ModelRefreshSourceResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<ModelRefreshWorkers>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRefreshApplyReport {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub missing: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_wire_shape_matches_refresh_contract() {
        let wire = serde_json::json!({"startedAt": 1, "finishedAt": 2,
            "sources": [{"source":"anthropic", "status":"failed", "reason":"auth-rejected"}],
            "workers": {"total": 2,"succeeded":1,"failed":1}});
        let report: ModelRefreshReport = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(report).unwrap(), wire);
        let apply =
            serde_json::json!({"applied":["curated"],"skipped":["openai"],"missing":["default"]});
        let report: ModelRefreshApplyReport = serde_json::from_value(apply.clone()).unwrap();
        assert_eq!(serde_json::to_value(report).unwrap(), apply);
    }
}
