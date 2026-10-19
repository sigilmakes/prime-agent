//! Read the session-wide context choice from loaded rows, including sparse metadata.

use super::SessionFile;
use pa_types::session::{ContextSelection, CONTEXT_SELECTION_CUSTOM_TYPE};

impl SessionFile {
    /// Return the newest recorded choice, not the newest valid choice.
    ///
    /// Windowed opens already retain choice rows as sparse metadata. This read
    /// never walks the active branch or loads discarded message bodies.
    pub(crate) fn latest_context_selection(&self) -> anyhow::Result<Option<ContextSelection>> {
        let Some(entry) = self.entries().iter().rev().find(|entry| {
            entry.type_ == "custom"
                && entry
                    .fields
                    .get("customType")
                    .and_then(serde_json::Value::as_str)
                    == Some(CONTEXT_SELECTION_CUSTOM_TYPE)
        }) else {
            return Ok(None);
        };
        let data = entry
            .fields
            .get("data")
            .ok_or_else(|| anyhow::anyhow!("context strategy selection lacks data"))?;
        let selection: ContextSelection = serde_json::from_value(data.clone())?;
        if selection.version != 1 || selection.strategy.split_once('/').is_none() {
            anyhow::bail!("unsupported session context selection schema");
        }
        Ok(Some(selection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn append_selection(store: &mut SessionFile, data: Value) -> String {
        let fields = serde_json::Map::from_iter([
            (
                "customType".to_owned(),
                json!(CONTEXT_SELECTION_CUSTOM_TYPE),
            ),
            ("data".to_owned(), data),
        ]);
        store.append_entry("custom", Value::Object(fields))
    }

    fn selection(strategy: &str) -> ContextSelection {
        ContextSelection {
            version: 1,
            strategy: strategy.to_owned(),
        }
    }

    #[test]
    fn absent_choice_returns_none() {
        let mut store = SessionFile::create("/repo", None, 0);
        assert_eq!(store.latest_context_selection().unwrap(), None);
        store.append_message(&json!({"role": "user", "content": "hello"}));
        store.append_entry("custom", json!({"customType": "unrelated", "data": null}));
        assert_eq!(store.latest_context_selection().unwrap(), None);
    }

    #[test]
    fn valid_latest_choice_wins_without_parsing_older_data() {
        let mut store = SessionFile::create("/repo", None, 0);
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/older"}),
        );
        // Invalid older rows do not matter when the last matching row is valid.
        append_selection(&mut store, Value::Null);
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "other/latest"}),
        );
        assert_eq!(
            store.latest_context_selection().unwrap(),
            Some(selection("other/latest"))
        );
    }

    #[test]
    fn latest_malformed_choice_does_not_skip_older_choice() {
        for fields in [
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE}),
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": null}),
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": []}),
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": {}}),
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE,
                   "data": {"version": "1", "strategy": "fixture/latest"}}),
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE,
                   "data": {"version": 1, "strategy": 42}}),
        ] {
            let mut store = SessionFile::create("/repo", None, 0);
            append_selection(
                &mut store,
                json!({"version": 1, "strategy": "fixture/older"}),
            );
            store.append_entry("custom", fields.clone());
            assert!(
                store.latest_context_selection().is_err(),
                "latest malformed choice was ignored: {fields}"
            );
        }
    }

    #[test]
    fn unsupported_choice_version_is_rejected() {
        let mut store = SessionFile::create("/repo", None, 0);
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/older"}),
        );
        append_selection(
            &mut store,
            json!({"version": 2, "strategy": "fixture/latest"}),
        );
        assert!(store.latest_context_selection().is_err());
    }

    #[test]
    fn unknown_choice_field_is_rejected() {
        let mut store = SessionFile::create("/repo", None, 0);
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/older"}),
        );
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/latest", "unexpected": true}),
        );
        assert!(store.latest_context_selection().is_err());
    }

    #[test]
    fn choice_strategy_requires_a_slash() {
        let mut store = SessionFile::create("/repo", None, 0);
        append_selection(&mut store, json!({"version": 1, "strategy": "unqualified"}));
        assert!(store.latest_context_selection().is_err());
    }

    #[test]
    fn choice_strategy_matches_the_existing_split_once_validation() {
        // Schema validation is not strategy lookup: do not add route policy here.
        for strategy in ["/", "/tail", "owner/", "owner/nested/route"] {
            let mut store = SessionFile::create("/repo", None, 0);
            append_selection(&mut store, json!({"version": 1, "strategy": strategy}));
            assert_eq!(
                store.latest_context_selection().unwrap(),
                Some(selection(strategy))
            );
        }
    }

    #[test]
    fn only_matching_custom_entry_type_is_read() {
        let mut store = SessionFile::create("/repo", None, 0);
        // Matching customType on another entry type is not a choice.
        store.append_entry(
            "custom_message",
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": null}),
        );
        assert_eq!(store.latest_context_selection().unwrap(), None);
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/choice"}),
        );
        store.append_entry(
            "custom_message",
            json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": null}),
        );
        store.append_entry("custom", json!({"customType": "unrelated", "data": null}));
        store.append_entry("custom", json!({"customType": null, "data": null}));
        store.append_entry("custom", json!({"data": null}));
        assert_eq!(
            store.latest_context_selection().unwrap(),
            Some(selection("fixture/choice"))
        );
    }

    #[test]
    fn choice_is_session_wide_not_limited_to_the_active_branch() {
        let mut store = SessionFile::create("/repo", None, 0);
        let older = append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/older"}),
        );
        append_selection(
            &mut store,
            json!({"version": 1, "strategy": "fixture/latest"}),
        );
        // The latest row is not an ancestor of the current leaf.
        store.leaf_id = Some(older);
        assert_eq!(
            store.latest_context_selection().unwrap(),
            Some(selection("fixture/latest"))
        );
    }

    #[test]
    fn choice_outside_retained_window_is_read_from_sparse_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut full = SessionFile::create("/repo", None, 0);
        full.set_path(path.clone());
        let discarded = full
            .append_message(&json!({"role": "user", "content": "discarded body", "timestamp": 0}));
        append_selection(
            &mut full,
            json!({"version": 1, "strategy": "fixture/older"}),
        );
        let choice_id = append_selection(
            &mut full,
            json!({"version": 1, "strategy": "fixture/sparse"}),
        );
        let mut kept = String::new();
        for i in 0..220 {
            let id = full.append_message(
                &json!({"role": "user", "content": format!("message {i}"), "timestamp": i}),
            );
            if i == 210 {
                kept = id;
            }
        }
        full.append_entry(
            "compaction",
            json!({"summary": "summary", "firstKeptEntryId": kept, "tokensBefore": 10000}),
        );
        full.append_message(&json!({"role": "user", "content": "after", "timestamp": 221}));
        full.rewrite().unwrap();

        // Read-only open: like the existing window fixtures, no append lease is needed.
        let mut windowed = SessionFile::open_windowed(&path).unwrap();
        let window = windowed.window.as_ref().expect("window reader must engage");
        assert!(!window.retained_ids.contains(&choice_id));
        assert!(
            windowed.entry(&choice_id).is_some(),
            "choice is sparse metadata"
        );
        assert!(
            windowed.entry(&discarded).is_none(),
            "old message body stays unloaded"
        );
        let loaded_count = windowed.entries().len();
        assert!(loaded_count < full.entries().len());

        // The reader must use the loaded metadata, not reopen a source file.
        let unavailable_path = dir.path().join("not-written.jsonl");
        assert!(!unavailable_path.exists());
        windowed.set_path(unavailable_path);
        assert_eq!(
            windowed.latest_context_selection().unwrap(),
            Some(selection("fixture/sparse"))
        );
        assert!(windowed.window.is_some());
        assert_eq!(windowed.entries().len(), loaded_count);
        assert!(windowed.entry(&discarded).is_none());
    }
}
