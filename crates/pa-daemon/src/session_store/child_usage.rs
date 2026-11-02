//! The idempotent child-usage attribution append: one durable row per stable
//! row identity, written through the same persist core as every other row.
use super::{SessionEntry, SessionFile};
use anyhow::{ensure, Context, Result};
use pa_core::session_engine::rlm_usage::{
    attribute_child_usage, child_usage_identity_matches, ChildUsageAppendResult,
};
use pa_types::{
    ai::Usage,
    session::{ChildUsageAttributionEntry, ChildUsageOrigin},
};
impl SessionFile {
    /// Append one attribution row once: the row id is the retry identity, so
    /// a re-delivered report finds its own row instead of billing the child
    /// twice. A fresh id persists through [`Self::persist_entry_at`]'s shared
    /// core, the same write-then-index contract every other row gets.
    ///
    /// # Errors
    ///
    /// Returns an error when the row id already names another row, the
    /// immutable identity changed, the target assistant is missing, or the
    /// durable append or its recovery confirmation fails. A complete owned
    /// row may be indexed after an error so later aggregates include it;
    /// acknowledgment still requires a successful durability confirmation.
    pub(crate) fn append_child_usage_once(
        &mut self,
        row_id: &str,
        target_id: &str,
        child_usage: Usage,
        origin: Option<ChildUsageOrigin>,
    ) -> Result<ChildUsageAppendResult> {
        ensure!(!row_id.is_empty(), "attribution ID must not be empty");
        if let Some(entry) = self.entry(row_id) {
            ensure!(
                entry.type_ == "child_usage_attributed",
                "attribution ID collision"
            );
            let payload: ChildUsageAttributionEntry = serde_json::from_value(entry.fields.clone())?;
            ensure!(
                child_usage_identity_matches(&payload, target_id, &child_usage, origin)?,
                "attribution ID collision"
            );
            let current = self.attribution_target_usage(target_id)?;
            return Ok(ChildUsageAppendResult::Existing(current));
        }
        let mut aggregate = self.attribution_target_usage(target_id)?;
        attribute_child_usage(&mut aggregate, &child_usage);
        let entry = SessionEntry {
            type_: "child_usage_attributed".to_owned(),
            id: row_id.to_owned(),
            parent_id: self.leaf_id.clone(),
            timestamp: crate::util::now_iso(),
            fields: serde_json::to_value(ChildUsageAttributionEntry {
                target_id: target_id.to_owned(),
                child_usage,
                aggregate_usage: aggregate.clone(),
                origin,
            })?,
        };
        self.persist_built_entry(entry)?;
        Ok(ChildUsageAppendResult::Created(aggregate))
    }

    /// The target assistant row's current usage: the running aggregate, since
    /// each indexed attribution folds its aggregate back into the row.
    fn attribution_target_usage(&self, target_id: &str) -> Result<Usage> {
        let message = self
            .entry(target_id)
            .and_then(|entry| entry.fields.get("message"))
            .context("child attribution target is missing")?;
        ensure!(
            message.get("role").and_then(serde_json::Value::as_str) == Some("assistant"),
            "child attribution target is not an assistant"
        );
        Ok(message
            .get("usage")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default())
    }
}
