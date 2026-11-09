//! The worker arm behind the `get_mcp_connections` command.

use serde_json::{json, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

impl Worker {
    pub(crate) async fn handle_get_mcp_connections(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_mcp_connections") {
            return response;
        }
        let Some(manager) = self.engine.acp_mcp_manager() else {
            return response_failure(
                None,
                "get_mcp_connections",
                "MCP connections are not available in this session",
                None,
            );
        };
        let roster = match tokio::task::spawn_blocking(move || {
            let mut manager = manager.lock().unwrap();
            let _auth_reload = manager.reload_auth_storage();
            manager.refresh();
            manager.connection_roster()
        })
        .await
        {
            Ok(roster) => roster,
            Err(error) => {
                return response_failure(
                    None,
                    "get_mcp_connections",
                    &format!("MCP roster read failed: {error}"),
                    None,
                );
            }
        };
        let connections: Vec<Value> = roster
            .into_iter()
            .map(|entry| serde_json::to_value(&entry).unwrap_or(Value::Null))
            .collect();
        response_success(
            None,
            "get_mcp_connections",
            Some(json!({ "connections": connections })),
        )
    }
}
