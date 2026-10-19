//! Explicit bounded control-plane projection. Bodies stay in invocation-local typed
//! Rust batches. Handles are never persisted, globally registered, or retargeted.

use crate::plugins::{ContextDataMode, Contribution};
use anyhow::{bail, Context};
use pa_types::session::{ContextArchive, ContextRepresentation, FileEntry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use uuid::Uuid;

const REFS_CONTROL_BYTES: usize = 512 * 1024;
const JSON_DATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_SPANS: usize = 4096;

/// The body contract is declared, never guessed from a plugin or route name.
/// Each frame is used for exactly one callback; the borrowed batch cannot change
/// while that callback runs. A returned reference resolves only in this frame.
pub(super) struct Frame<'a, T> {
    source: &'a [T],
    target: &'a Contribution,
    session: &'a str,
    stage: &'a str,
    leaf: Option<&'a str>,
    nonce: Uuid,
    messages: Uuid,
    entries: Uuid,
    archive: Uuid,
    coverage: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageReference {
    version: u32,
    kind: String,
    session: String,
    plugin: String,
    contribution: String,
    generation: String,
    contribution_kind: String,
    stage: String,
    leaf: Option<String>,
    frame: String,
    #[serde(rename = "ref")]
    token: String,
    count: usize,
    #[serde(default)]
    spans: Option<Vec<Span>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    /// Lua arrays use one-based indices. Zero-length ranges are explicit views.
    first: usize,
    count: usize,
}

struct SizeLimit {
    written: usize,
    limit: usize,
}
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self
            .written
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| std::io::Error::other("context JSON resource limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a, T> Frame<'a, T> {
    pub(super) fn new(
        session: &'a str,
        target: &'a Contribution,
        source: &'a [T],
        stage: &'a str,
        leaf: Option<&'a str>,
    ) -> Self {
        Self {
            source,
            target,
            session,
            stage,
            leaf,
            nonce: Uuid::new_v4(),
            messages: Uuid::new_v4(),
            entries: Uuid::new_v4(),
            archive: Uuid::new_v4(),
            coverage: Uuid::new_v4(),
        }
    }

    fn contribution_kind(&self) -> &'static str {
        match self.target.kind {
            crate::plugins::ContributionKind::ContextStrategy => "context_strategy",
            crate::plugins::ContributionKind::Compactor => "compactor",
            _ => "invalid_context_kind",
        }
    }

    fn reference(&self, kind: &str, token: Uuid, count: usize) -> Value {
        json!({"version":1,"kind":kind,"session":self.session,"plugin":self.target.plugin,
            "contribution":self.target.name,"generation":self.target.generation.to_string(),
            "contribution_kind":self.contribution_kind(),"stage":self.stage,"leaf":self.leaf,
            "frame":self.nonce.to_string(),"ref":token.to_string(),"count":count})
    }

    pub(super) fn messages(&self) -> anyhow::Result<Value>
    where
        T: Serialize,
    {
        match self.target.context_data {
            ContextDataMode::Json => serde_json::to_value(self.source).map_err(Into::into),
            ContextDataMode::RefsV1 => {
                Ok(self.reference("messages", self.messages, self.source.len()))
            }
        }
    }

    pub(super) fn entries(&self, source: &[FileEntry]) -> anyhow::Result<Value> {
        match self.target.context_data {
            ContextDataMode::Json => serde_json::to_value(source).map_err(Into::into),
            ContextDataMode::RefsV1 => Ok(self.reference("entries", self.entries, source.len())),
        }
    }

    pub(super) fn archive(&self, archive: Option<&ContextArchive>) -> anyhow::Result<Value> {
        if self.target.context_data == ContextDataMode::Json {
            return serde_json::to_value(archive).map_err(Into::into);
        }
        let Some(archive) = archive else {
            return Ok(Value::Null);
        };
        let kind = match &archive.representation {
            ContextRepresentation::Standard { .. } => "standard",
            ContextRepresentation::Responses { .. } => "responses",
            ContextRepresentation::Snapcompact { .. } => "snapcompact",
        };
        Ok(
            json!({"version":archive.version,"strategy":archive.strategy,
            "compactor":archive.compactor,"creatorGeneration":archive.creator_generation,
            "preparedLeaf":archive.prepared_leaf,
            "coveredEntryIds":self.reference("coverage", self.coverage, archive.covered_entry_ids.len()),
            "representation":{"kind":kind,"body":self.reference("archive",self.archive,1)}}),
        )
    }

    /// Limit control data without serializing bodies into an intermediate byte
    /// buffer. No field is truncated or silently removed on either contract.
    pub(super) fn checked(&self, value: Value) -> anyhow::Result<Value> {
        self.check(&value)?;
        Ok(value)
    }

    pub(super) fn check(&self, value: &Value) -> anyhow::Result<()> {
        let (limit, contract) = match self.target.context_data {
            ContextDataMode::Json => (JSON_DATA_BYTES, "json full-body callback"),
            ContextDataMode::RefsV1 => (REFS_CONTROL_BYTES, "refs_v1 control"),
        };
        serde_json::to_writer(SizeLimit { written: 0, limit }, value)
            .with_context(|| format!("context {contract} JSON exceeds {limit} byte resource limit; body data was not omitted"))
    }

    pub(super) fn assembled<'b>(&self, output: &'b Value) -> anyhow::Result<&'b Value> {
        self.check(output)?;
        if self.target.context_data == ContextDataMode::RefsV1
            && output
                .as_object()
                .is_none_or(|fields| fields.len() != 1 || !fields.contains_key("messages"))
        {
            bail!("refs_v1 assembly must return only messages, as a reference or generated array");
        }
        output
            .get("messages")
            .context("context strategy must return messages")
    }

    /// `None` means an explicitly generated JSON message array: callers use their
    /// existing typed decoder. References clone the original typed values only.
    pub(super) fn resolve(&self, output: &Value) -> anyhow::Result<Option<Vec<T>>>
    where
        T: Clone,
    {
        self.check(output)?;
        if self.target.context_data == ContextDataMode::Json || output.is_array() {
            return Ok(None);
        }
        let reference: MessageReference =
            serde_json::from_value(output.clone()).context("invalid context message reference")?;
        if reference.version != 1
            || reference.kind != "messages"
            || reference.session != self.session
            || reference.plugin != self.target.plugin
            || reference.contribution != self.target.name
            || reference.generation != self.target.generation.to_string()
            || reference.contribution_kind != self.contribution_kind()
            || reference.stage != self.stage
            || reference.leaf.as_deref() != self.leaf
            || reference.frame != self.nonce.to_string()
            || reference.token != self.messages.to_string()
            || reference.count != self.source.len()
        {
            bail!("context message reference is not from this session, contribution generation and invocation");
        }
        let Some(spans) = reference.spans else {
            return Ok(Some(self.source.to_vec()));
        };
        if spans.len() > MAX_SPANS {
            bail!("context message reference exceeds span resource limit");
        }
        let mut ranges = Vec::with_capacity(spans.len());
        let mut previous_end = 0;
        for span in spans {
            let start = span
                .first
                .checked_sub(1)
                .context("context message span indices are one-based")?;
            let end = start
                .checked_add(span.count)
                .filter(|end| *end <= self.source.len())
                .context("context message span exceeds its native batch")?;
            if start < previous_end {
                bail!("context message spans overlap or are out of order");
            }
            previous_end = end;
            ranges.push(start..end);
        }
        let mut selected = Vec::new();
        for range in ranges {
            selected.extend_from_slice(&self.source[range]);
        }
        Ok(Some(selected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{ContributionKind, InvocationMode};
    use std::sync::Arc;

    fn target(mode: ContextDataMode) -> Contribution {
        Contribution {
            plugin: "policy".into(),
            generation: Uuid::new_v4(),
            kind: ContributionKind::ContextStrategy,
            name: "view".into(),
            description: String::new(),
            mode: InvocationMode::Unary,
            context_data: mode,
        }
    }

    #[derive(Clone)]
    struct NativeBody(Arc<str>);
    impl Serialize for NativeBody {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("native body was serialized"))
        }
    }

    #[test]
    fn refs_clone_original_native_body_without_any_json_normalization() {
        let owner = target(ContextDataMode::RefsV1);
        let source = vec![NativeBody(Arc::from(r#"{ "n":1e+02,"same":1,"same":2 }"#))];
        let frame = Frame::new("session", &owner, &source, "assemble", Some("leaf"));
        let descriptor = frame.messages().unwrap();
        let selected = frame.resolve(&descriptor).unwrap().unwrap();
        assert!(Arc::ptr_eq(&source[0].0, &selected[0].0));
        assert_eq!(selected[0].0.as_ref(), r#"{ "n":1e+02,"same":1,"same":2 }"#);
        assert!(serde_json::to_string(&descriptor).unwrap().len() < 1024);
    }

    #[test]
    fn refs_reject_every_forged_scope_and_cross_call_or_typed_slot() {
        let owner = target(ContextDataMode::RefsV1);
        let source = vec![json!("body")];
        let frame = Frame::new("session", &owner, &source, "assemble", Some("leaf"));
        let original = frame.messages().unwrap();
        for (key, bad) in [
            ("version", json!(2)),
            ("kind", json!("entries")),
            ("session", json!("other")),
            ("plugin", json!("other")),
            ("contribution", json!("other")),
            ("generation", json!(Uuid::new_v4().to_string())),
            ("contribution_kind", json!("compactor")),
            ("stage", json!("model")),
            ("leaf", json!("other")),
            ("frame", json!(Uuid::new_v4().to_string())),
            ("ref", json!(Uuid::new_v4().to_string())),
            ("count", json!(2)),
        ] {
            let mut forged = original.clone();
            forged[key] = bad;
            assert!(frame.resolve(&forged).is_err(), "forged {key} accepted");
        }
        let later = Frame::new("session", &owner, &source, "assemble", Some("leaf"));
        assert!(later.resolve(&original).is_err());
        let mut wrong_slot = frame.entries(&[]).unwrap();
        wrong_slot["kind"] = json!("messages");
        wrong_slot["count"] = json!(1);
        assert!(frame.resolve(&wrong_slot).is_err());
        let mut unknown = original.clone();
        unknown["unexpected"] = Value::Null;
        assert!(frame.resolve(&unknown).is_err());
        assert!(frame
            .assembled(&json!({"messages":original,"messages_ref":wrong_slot}))
            .is_err());
    }

    #[test]
    fn refs_allow_only_bounded_ordered_nonoverlapping_spans() {
        let owner = target(ContextDataMode::RefsV1);
        let source = vec![json!("first"), json!("second"), json!("third")];
        let frame = Frame::new("session", &owner, &source, "assemble", None);
        let mut descriptor = frame.messages().unwrap();
        descriptor["spans"] = json!([{"first":2,"count":1},{"first":4,"count":0}]);
        assert_eq!(
            frame.resolve(&descriptor).unwrap().unwrap(),
            vec![json!("second")]
        );
        descriptor["spans"] = json!([]);
        assert!(frame.resolve(&descriptor).unwrap().unwrap().is_empty());
        for spans in [
            json!([{"first":0,"count":1}]),
            json!([{"first":4,"count":1}]),
            json!([{"first":1,"count":2},{"first":2,"count":1}]),
            json!([{"first":3,"count":0},{"first":2,"count":0}]),
            json!([{"first":1,"count":usize::MAX}]),
            json!([{"first":1,"count":0,"extra":null}]),
        ] {
            descriptor["spans"] = spans;
            assert!(frame.resolve(&descriptor).is_err());
        }
        descriptor["spans"] = json!(vec![json!({"first":1,"count":0}); MAX_SPANS + 1]);
        assert!(frame
            .resolve(&descriptor)
            .unwrap_err()
            .to_string()
            .contains("span resource"));
    }

    #[test]
    fn json_callbacks_keep_full_bodies_and_all_modes_reject_oversized_controls() {
        let owner = target(ContextDataMode::Json);
        let source = vec![json!({"role":"user","content":"UNIQUE BODY"})];
        let frame = Frame::new("session", &owner, &source, "assemble", None);
        assert_eq!(frame.messages().unwrap(), json!(source));
        assert!(frame.resolve(&frame.messages().unwrap()).unwrap().is_none());
        let large = json!({"content":"x".repeat(JSON_DATA_BYTES+1)});
        assert!(frame
            .checked(large)
            .unwrap_err()
            .to_string()
            .contains("json full-body callback"));
        let owner = target(ContextDataMode::RefsV1);
        let frame = Frame::new("session", &owner, &source, "model", None);
        assert!(frame
            .checked(json!({"instructions":"x".repeat(REFS_CONTROL_BYTES+1)}))
            .unwrap_err()
            .to_string()
            .contains("refs_v1 control"));
        assert!(frame
            .resolve(&json!([{"role":"user","content":"generated"}]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn archive_headers_keep_large_ciphertext_rest_and_coverage_native() {
        let owner = target(ContextDataMode::RefsV1);
        let source: Vec<Value> = vec![];
        let frame = Frame::new("session", &owner, &source, "restore", Some("leaf"));
        let mut rest = pa_types::JsonMap::new();
        rest.insert("future".into(), json!("SECRET BODY".repeat(1024 * 1024)));
        let archive = ContextArchive {
            version: 1,
            strategy: "policy/view".into(),
            compactor: "native/server".into(),
            creator_generation: "durable prior process".into(),
            prepared_leaf: "leaf".into(),
            covered_entry_ids: vec!["id".repeat(100); 10000],
            representation: ContextRepresentation::Responses {
                provider: "p".into(),
                endpoint: "http://127.0.0.1/v1/responses/compact".into(),
                model: "m".into(),
                archive: "CIPHERTEXT".repeat(4 * 1024 * 1024),
                rest,
            },
        };
        let before = archive.clone();
        let header = frame.archive(Some(&archive)).unwrap();
        let text = serde_json::to_string(&frame.checked(header).unwrap()).unwrap();
        assert!(text.len() < 2048);
        assert!(!text.contains("SECRET BODY"));
        assert!(!text.contains("CIPHERTEXT"));
        assert_eq!(archive, before);
    }
}
