//! Deferred plugin-job outcomes use the existing custom-message wire surface.
use crate::protocol::{create_daemon_event_meta, DaemonOutbound};
use crate::worker::{EventPump, OutboundFrame, SessionCore};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub(crate) fn publish(
    core: &Arc<Mutex<SessionCore>>,
    events: &Arc<EventPump>,
    session: &str,
    job: &Value,
) {
    let mut core = core.lock().unwrap();
    // A replaced session must not inherit a retired session's background work.
    if core
        .store
        .as_ref()
        .is_none_or(|store| store.session_id() != session)
    {
        return;
    }
    let plugin = job["plugin"].as_str().unwrap_or("plugin");
    let state = &job["state"];
    let content = if state["status"] == "failed" {
        format!(
            "Plugin {plugin} job failed: {}",
            state["error"].as_str().unwrap_or("unknown error")
        )
    } else {
        let summary = state["result"]["summary"].as_str().unwrap_or("completed");
        format!("Plugin {plugin} job: {summary}")
    };
    let mut message = json!({"role":"custom","customType":"plugin_job_outcome",
        "content":content,"display":true,"details":job});
    if state["status"] == "completed" {
        if let Some(projected) = state["result"].get("message") {
            if projected
                .get("customType")
                .and_then(Value::as_str)
                .is_some_and(|kind| !kind.is_empty())
                && projected.get("content").is_some_and(Value::is_string)
                && projected.get("display").is_some_and(Value::is_boolean)
                && projected.get("details").is_none_or(Value::is_object)
            {
                message = json!({"role":"custom","customType":projected["customType"],
                    "content":projected["content"],"display":projected["display"],
                    "details":projected.get("details")});
            } else {
                let diagnostic = "Plugin job returned an invalid custom-message projection";
                eprintln!("pa-daemon: {diagnostic}");
                message["content"] = json!(format!("{content}\n{diagnostic}"));
                message["details"]["projectionError"] = json!(diagnostic);
            }
        }
    }
    if let Err(error) = crate::worker::persist_custom_row(&mut core, &message) {
        // The job already committed. Surface the persistence diagnostic without
        // re-running it or concealing the original terminal outcome.
        eprintln!("pa-daemon: cannot persist plugin job outcome: {error:#}");
        message["content"] = json!(format!(
            "{}\nCould not save job outcome: {error:#}",
            message["content"].as_str().unwrap_or_default()
        ));
        message["details"]["persistenceError"] = json!(format!("{error:#}"));
    }
    let frames: Vec<_> = ["message_start", "message_end"]
        .into_iter()
        .map(|kind| {
            core.last_event_sequence += 1;
            let meta = create_daemon_event_meta(
                &core.active_session_id,
                core.last_event_sequence,
                None,
                Some(&core.generation),
            );
            let outbound = DaemonOutbound::SessionEvent {
                active_session_id: core.active_session_id.clone(),
                event: json!({"type":kind,"message":message}),
                meta: Some(meta),
                rest: Map::default(),
            };
            OutboundFrame::session_event(serde_json::to_vec(&outbound).expect("JSON job outcome"))
        })
        .collect();
    // Both frames carry the identity checked above, even if replacement follows.
    drop(core);
    for frame in frames {
        events.send(frame);
    }
}
