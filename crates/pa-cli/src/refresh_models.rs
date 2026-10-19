//! Explicit one-shot catalog refresh. This path never starts a daemon or a session.

use pa_types::daemon::DaemonCommand;
use pa_types::model_refresh::ModelRefreshReport;
use serde_json::Value;

use crate::args::Args;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const USAGE_ERROR: &str =
    "--refresh accepts only --daemon-socket and --offline; no session or prompt options.";

pub(crate) fn run(argv: &[String], parsed: &Args) -> Result<i32, String> {
    if !refresh_only_arguments(argv) {
        return Err(USAGE_ERROR.to_string());
    }
    let result = (|| -> anyhow::Result<ModelRefreshReport> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let socket =
                crate::interactive_mode::resolve_socket_path(parsed.daemon_socket.as_deref());
            if let Some(report) = if parsed.offline {
                None
            } else {
                refresh_resident(&socket).await?
            } {
                if report
                    .workers
                    .as_ref()
                    .is_none_or(|workers| workers.total != 0)
                {
                    return Ok(report);
                }
            }
            let agent_dir = crate::config::get_agent_dir();
            let auth = pa_core::auth::AuthStorage::create(&agent_dir);
            let mut registry =
                pa_core::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
            Ok(registry.refresh_catalogs().await)
        })
    })();
    if let Ok(report) = result {
        for line in format_report(&report) {
            println!("{line}");
        }
        Ok(i32::from(refresh_failed(&report)))
    } else {
        eprintln!("Model catalog refresh failed. Check the agent log for details.");
        Ok(1)
    }
}

/// A cancelled async connect drops its socket; no blocking task can outlive the deadline.
async fn refresh_resident(socket: &std::path::Path) -> anyhow::Result<Option<ModelRefreshReport>> {
    use pa_types::daemon::{DaemonProtocolInfo, DaemonResponse};
    let connect = tokio::time::timeout(
        Duration::from_millis(250),
        pa_types::platform::transport::connect_transport(socket),
    )
    .await?;
    let stream = match connect {
        Ok(stream) => stream,
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let (reader, mut writer) = stream.split();
    let mut lines = BufReader::new(reader).lines();
    let hello = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(line) = lines.next_line().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] == "daemon_hello" {
                return Ok(value);
            }
        }
        anyhow::bail!("daemon closed before hello")
    })
    .await??;
    if !refresh_supported(&hello) {
        return Ok(None);
    }
    let protocol: DaemonProtocolInfo = serde_json::from_value(hello["protocol"].clone())?;
    // No retry or standalone fallback after sending: the refresh might already be running.
    tokio::time::timeout(Duration::from_secs(120), async {
        let id = "refresh_1";
        let command = DaemonCommand::RefreshAllModels {
            id: Some(id.to_string()),
            rest: serde_json::Map::default(),
        };
        let mut line = serde_json::to_vec(&serde_json::json!({
            "type": "command", "id": id, "protocol": protocol,
            "clientId": format!("refresh-client:{}", uuid::Uuid::new_v4()), "command": command,
        }))?;
        line.push(b'\n');
        writer.write_all(&line).await?;
        writer.flush().await?;
        while let Some(line) = lines.next_line().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] != "response" || value["id"] != id {
                continue;
            }
            let response: DaemonResponse = serde_json::from_value(value)?;
            if !response.success {
                anyhow::bail!("daemon model refresh failed");
            }
            return Ok(Some(serde_json::from_value(response.data.ok_or_else(
                || anyhow::anyhow!("missing model refresh report"),
            )?)?));
        }
        anyhow::bail!("daemon closed before refresh response")
    })
    .await?
}

fn refresh_only_arguments(argv: &[String]) -> bool {
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--refresh" | "--offline" => {}
            "--daemon-socket"
                if argv
                    .get(index + 1)
                    .is_some_and(|value| !value.starts_with('-')) =>
            {
                index += 1;
            }
            _ => return false,
        }
        index += 1;
    }
    true
}

fn refresh_supported(hello: &Value) -> bool {
    let capabilities: Vec<String> = hello
        .get("serverCapabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    hello["protocol"]["name"] == "prime-agent.daemon"
        && hello["protocol"]["version"].as_u64() == Some(pa_types::daemon::DAEMON_PROTOCOL_VERSION)
        && pa_types::daemon::compatibility::supports_optional_command(
            "refresh_all_models",
            hello.get("schemaRevision").and_then(Value::as_u64),
            &capabilities,
        )
}

/// Only credential-free fixed fields can reach the terminal, matching the fork's summary.
fn format_report(report: &pa_types::model_refresh::ModelRefreshReport) -> Vec<String> {
    use pa_types::model_refresh::{ModelRefreshReason, ModelRefreshStatus};
    report
        .sources
        .iter()
        .map(|result| {
            let source = if !result.source.is_empty()
                && result.source.len() <= 64
                && result
                    .source
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                result.source.as_str()
            } else {
                "unknown-source"
            };
            let status = match result.status {
                ModelRefreshStatus::Updated => "updated",
                ModelRefreshStatus::Unchanged => "unchanged",
                ModelRefreshStatus::Skipped => "skipped",
                ModelRefreshStatus::Stale => "stale",
                ModelRefreshStatus::Failed => "failed",
            };
            let count = result
                .model_count
                .filter(|count| (*count as u128) <= 9_007_199_254_740_991)
                .map(|count| format!(" ({count} models)"))
                .unwrap_or_default();
            let reason = result
                .reason
                .map(|reason| match reason {
                    ModelRefreshReason::Offline => "offline",
                    ModelRefreshReason::Unconfigured => "unconfigured",
                    ModelRefreshReason::AuthRejected => "auth-rejected",
                    ModelRefreshReason::Network => "network",
                    ModelRefreshReason::InvalidCatalog => "invalid-catalog",
                    ModelRefreshReason::Cancelled => "cancelled",
                    ModelRefreshReason::Superseded => "superseded",
                    ModelRefreshReason::Unsupported => "unsupported",
                    ModelRefreshReason::CacheWrite => "cache-write",
                    ModelRefreshReason::WorkerUnavailable => "worker-unavailable",
                })
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            format!("{source}: {status}{count}{reason}")
        })
        .collect()
}

fn refresh_failed(report: &pa_types::model_refresh::ModelRefreshReport) -> bool {
    use pa_types::model_refresh::{ModelRefreshReason, ModelRefreshStatus};
    report
        .workers
        .as_ref()
        .is_some_and(|workers| workers.failed != 0)
        || report.sources.iter().any(|source| {
            source.status == ModelRefreshStatus::Failed
                || (source.status == ModelRefreshStatus::Stale
                    && source.reason != Some(ModelRefreshReason::Offline))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_only_validation_keeps_socket_values_but_rejects_session_options() {
        let argv = |args: &[&str]| {
            args.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        };
        assert!(refresh_only_arguments(&argv(&[
            "--refresh",
            "--daemon-socket",
            "local.sock",
            "--offline"
        ])));
        for args in [
            vec!["--refresh", "--model", "x"],
            vec!["--refresh", "hello"],
            vec!["--refresh", "--daemon-socket"],
        ] {
            assert!(!refresh_only_arguments(&argv(&args)));
        }
    }

    #[test]
    fn summaries_validate_source_names_and_safe_integer_counts() {
        let report: ModelRefreshReport = serde_json::from_value(serde_json::json!({
            "startedAt": 1, "finishedAt": 2, "sources": [
                { "source": "bad\u{1b}[31m", "status": "updated", "modelCount": 9_007_199_254_740_992_u64 },
                { "source": "valid-Provider", "status": "stale", "modelCount": 0, "reason": "offline" }
            ]
        })).unwrap();
        assert_eq!(
            format_report(&report),
            vec![
                "unknown-source: updated",
                "valid-Provider: stale (0 models) (offline)"
            ]
        );
        assert!(!refresh_failed(&report));
    }
    #[test]
    fn only_the_current_protocol_may_receive_a_refresh() {
        for (name, version, supported) in [
            ("prime-agent.daemon", 7, true),
            ("prime-agent.daemon", 6, false),
            ("prime-agent.daemon", 8, false),
            ("another.daemon", 7, false),
        ] {
            assert_eq!(
                refresh_supported(&serde_json::json!({
                    "protocol": { "name": name, "version": version }, "schemaRevision": 32,
                    "serverCapabilities": ["model_refresh"],
                })),
                supported
            );
        }
    }
}
