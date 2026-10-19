//! Authoritative branch restores. History preparation runs off the paint loop; only a
//! result for the currently mounted epoch can replace the view or request native replay.
use super::{
    attach_capabilities, DaemonCommand, Map, SessionUi, TranscriptBackfillNote,
    TranscriptReplacement, Value,
};

impl SessionUi {
    pub(super) fn restore_branch_transcript(&mut self) {
        self.transcript_epoch = self.transcript_epoch.wrapping_add(1);
        self.transcript_backfill = None;
        self.pending_backfill = None;
        self.native_replay_epoch = None;
        self.transcript_incomplete = true;
        let epoch = self.transcript_epoch;
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let notes = self.backfill_notes.clone();
        let generation = self.attach_event_generation.clone();
        let task = tokio::spawn(async move {
            let state_command = DaemonCommand::GetConnectionState {
                id: None,
                active_session_id: active_session_id.clone(),
                rest: Map::default(),
            };
            let state = client.request_ok(state_command.clone()).await.ok();
            let leaf = state
                .as_ref()
                .and_then(|state| state.get("leafId"))
                .and_then(Value::as_str);
            let mut rest = leaf
                .map(|leaf| {
                    Map::from_iter([("historyLeafId".to_string(), Value::String(leaf.to_string()))])
                })
                .unwrap_or_default();
            if !generation.is_empty() {
                rest.insert(
                    "historyGeneration".to_string(),
                    Value::String(generation.clone()),
                );
            }
            let result = client
                .request_ok(DaemonCommand::GetMessages {
                    id: None,
                    active_session_id: active_session_id.clone(),
                    before: None,
                    capabilities: Some(attach_capabilities()),
                    rest,
                })
                .await
                .and_then(|data| {
                    if data.get("displayTranscript").and_then(Value::as_bool) == Some(true) {
                        let before = state
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("missing selected-branch state"))?;
                        for key in ["sessionId", "leafId"] {
                            anyhow::ensure!(
                                before.get(key).and_then(Value::as_str).is_some()
                                    && data.get(key) == before.get(key),
                                "selected history identity changed"
                            );
                        }
                        anyhow::ensure!(
                            data.get("historyLeafId").and_then(Value::as_str) == leaf,
                            "unverified selected history branch anchor"
                        );
                        anyhow::ensure!(
                            generation.is_empty()
                                || data.get("generation").and_then(Value::as_str)
                                    == Some(generation.as_str()),
                            "selected history worker generation changed"
                        );
                    }
                    Ok(data)
                });
            let (messages, complete, warning) = match result {
                Ok(data) if data.get("messages").and_then(Value::as_array).is_some() => {
                    let complete =
                        data.get("displayTranscript").and_then(Value::as_bool) == Some(true);
                    (
                        data["messages"].as_array().cloned().unwrap_or_default(),
                        complete,
                        None,
                    )
                }
                result => {
                    let reason = result.err().map_or_else(
                        || "invalid history response".to_string(),
                        |error| error.to_string(),
                    );
                    let fallback = client
                        .request_ok(DaemonCommand::GetSessionContext {
                            id: None,
                            active_session_id: active_session_id.clone(),
                            rest: Map::default(),
                        })
                        .await;
                    let messages = fallback
                        .ok()
                        .and_then(|data| {
                            data.get("context")
                                .and_then(|context| context.get("messages"))
                                .and_then(Value::as_array)
                                .cloned()
                        })
                        .unwrap_or_default();
                    (messages, false, Some(format!("Full session history unavailable: {reason}. Showing selected-branch context only; /reload retries full history.")))
                }
            };
            let after = client.request_ok(state_command).await.ok();
            // Appends are harmless to a prefix, but a complete branch replacement must still
            // address exactly the captured branch. A competing navigation fails closed.
            let unchanged = match (&state, &after) {
                (Some(before), Some(after)) => ["sessionId", "activeSessionId", "leafId"]
                    .iter()
                    .all(|key| {
                        (!complete || before.get(key).and_then(Value::as_str).is_some())
                            && before.get(key) == after.get(key)
                    }),
                _ => !complete,
            };
            let entries = if unchanged {
                tokio::task::spawn_blocking(move || {
                    if complete {
                        crate::snapshot::display_transcript_to_entries(&messages)
                    } else {
                        crate::snapshot::transcript_to_entries(&messages)
                    }
                })
                .await
                .map_err(|error| format!("History rendering failed: {error}"))
            } else {
                Err(
                    "Selected branch changed while loading history; /reload retries full history."
                        .to_string(),
                )
            };
            let _ = notes.send(TranscriptBackfillNote {
                epoch,
                entries,
                replacement: Some(TranscriptReplacement {
                    complete: complete && unchanged,
                    warning,
                    state: after,
                }),
            });
        });
        self.transcript_backfill = Some(tokio_util::task::AbortOnDropHandle::new(task));
    }

    pub(crate) fn take_native_replay(&mut self) -> bool {
        if self.native_replay_epoch != Some(self.transcript_epoch)
            || self.transcript_incomplete
            || self.pending_backfill.is_some()
            || self.transcript_backfill.is_some()
        {
            return false;
        }
        self.native_replay_epoch = None;
        true
    }
}
