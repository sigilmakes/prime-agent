use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::tree::{Cache, Merge, Tree, View, MAX_MERGE_INPUT_BYTES, MAX_PLAN_REPLY_BYTES};
use crate::{
    plugins::{CapabilityFuture, CapabilityRequest, HostCapabilities},
    session::manager::SessionManager,
};

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    revision: u64,
    cache: Cache,
    /// Cache provenance is not an active-view authority or a replacement journal.
    source_hash: String,
    branch: Option<String>,
    /// Creator identity is provenance, never authority for a restored cache.
    creator_invocation: String,
    cache_only: bool,
}
struct Draft {
    tree: Tree,
    budget: usize,
    revision: u64,
    cancellation: CancellationToken,
    expected_leaf: Option<String>,
    // Preserve the original ready frontier across byte-sized reply slices.
    frontier: Vec<usize>,
}
#[derive(Default)]
struct State {
    drafts: HashMap<Uuid, Draft>,
    revision: u64,
    last_prepared: Option<View>,
    cache: Cache,
}

/// Explicitly wired per session, with private plugin-owned artifact storage. No
/// ambient home discovery.
pub(crate) struct OptChatServices {
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    path: Option<PathBuf>,
    state: Arc<Mutex<State>>,
}

impl OptChatServices {
    pub(crate) fn new(
        session: Arc<tokio::sync::Mutex<SessionManager>>,
        artifact_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            session,
            path: artifact_dir.map(|root| root.join("plugins/optchat/tree-v1.json")),
            state: Arc::default(),
        }
    }
}

// A conservative byte/token upper bound, not a tokenizer claim. The exact
// enriched system prompt and tool schemas are charged before history layout.
// Output and framing reserves are never borrowed to make a tree appear to fit.
fn call_budget(input: &Value, configured: usize) -> anyhow::Result<usize> {
    if input["phase"] != "model" {
        return Ok(configured);
    }
    let window = input["model"]["contextWindow"]
        .as_u64()
        .context("model contextWindow required")?;
    let output = input["output_tokens"]
        .as_u64()
        .or_else(|| input["model"]["maxTokens"].as_u64())
        .context("model output token reserve required")?;
    let instructions = match input
        .get("instructions")
        .context("exact model instructions required")?
    {
        Value::Null => 0,
        Value::String(text) => u64::try_from(text.len())?,
        _ => bail!("model instructions must be a string or null"),
    };
    let tools = input
        .get("tools")
        .context("exact model tool definitions required")?;
    let tools = if tools.is_null() {
        0
    } else {
        u64::try_from(serde_json::to_vec(tools)?.len())?
    };
    let fixed = instructions
        .checked_add(tools)
        .and_then(|n| n.checked_add(output))
        .and_then(|n| n.checked_add(1024))
        .context("model context budget overflow")?;
    let available = window.checked_sub(fixed).context("OptChat full model context budget exhausted by instructions/tools/output; no model call sent")?;
    let budget = configured.min(usize::try_from(available)?);
    if budget < 1024 {
        bail!("OptChat full model context leaves less than 1024 history bytes; no model call sent");
    }
    Ok(budget)
}

fn read_store(path: Option<&Path>) -> anyhow::Result<Stored> {
    let Some(path) = path else {
        return Ok(Stored::default());
    };
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("invalid OptChat cache; original history is unchanged"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Stored::default()),
        Err(error) => Err(error).context("read OptChat cache"),
    }
}

// Fully serialize and fsync away from session/registry locks. Publication is a
// single guarded rename. The persisted document contains reusable cache only.
struct PreparedFile {
    temporary: PathBuf,
    destination: PathBuf,
}
impl PreparedFile {
    fn write(path: &Path, stored: &Stored) -> anyhow::Result<Self> {
        let parent = path.parent().context("OptChat artifact parent missing")?;
        fs::create_dir_all(parent)?;
        crate::platform::perms::restrict_dir(parent)?;
        let temporary = parent.join(format!(".tree-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temporary)?;
        let prepared = Self {
            temporary,
            destination: path.to_owned(),
        };
        serde_json::to_writer(&mut file, stored)?;
        file.flush()?;
        file.sync_all()?;
        Ok(prepared)
    }
    fn publish(&self) -> anyhow::Result<()> {
        fs::rename(&self.temporary, &self.destination)?;
        File::open(
            self.destination
                .parent()
                .context("artifact parent missing")?,
        )?
        .sync_all()?;
        Ok(())
    }
}
impl Drop for PreparedFile {
    fn drop(&mut self) {
        // The unique temporary belongs to this call. A successful rename removed it.
        match fs::remove_file(&self.temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "OptChat temporary cleanup failed"),
        }
    }
}

impl HostCapabilities for OptChatServices {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let session = Arc::clone(&self.session);
        let path = self.path.clone();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let work = async {
                if cancellation.is_cancelled() {
                    bail!("cancelled");
                }
                let key = request.invocation;
                match request.operation.as_str() {
                    "optchat.begin" => {
                        let budget = request.input["budget_bytes"]
                            .as_u64()
                            .context("budget_bytes required")?;
                        let budget = usize::try_from(budget)?;
                        if !(1024..=131_072).contains(&budget) {
                            bail!("OptChat view budget must be 1024..131072 bytes");
                        }
                        let budget = call_budget(&request.input, budget)?;
                        let summary_bytes = usize::try_from(
                            request.input["summary_bytes"].as_u64().unwrap_or(512),
                        )?;
                        let (session_id, snapshot, branch) = {
                            let guard = session.lock().await;
                            (
                                guard.get_session_id().to_owned(),
                                guard.history_snapshot(),
                                guard.get_leaf_id().map(str::to_owned),
                            )
                        };
                        let expected_leaf = branch.clone();
                        let branch = match request.input.get("select_leaf") {
                            None => branch,
                            Some(Value::Null) => None,
                            Some(Value::String(leaf)) => Some(leaf.clone()),
                            Some(_) => bail!("select_leaf must be a canonical entry ID or null"),
                        };
                        let history = snapshot.await?;
                        let stored = if path.is_some() {
                            tokio::task::spawn_blocking(move || read_store(path.as_deref()))
                                .await??
                        } else {
                            let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                            Stored {
                                revision: guard.revision,
                                cache: guard.cache.clone(),
                                ..Default::default()
                            }
                        };
                        let tree =
                            Tree::new(&session_id, history, branch, stored.cache, summary_bytes)?;
                        tree.validate_projection()?;
                        let source_hash = tree.source_hash().to_owned();
                        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                        // Remove cancelled/retired drafts; bound concurrent preparation state.
                        guard
                            .drafts
                            .retain(|_, draft| !draft.cancellation.is_cancelled());
                        if guard.drafts.len() >= 8 {
                            bail!("OptChat preparation slots busy");
                        }
                        guard.revision = guard.revision.max(stored.revision);
                        let revision = guard.revision;
                        guard.drafts.insert(
                            key,
                            Draft {
                                tree,
                                budget,
                                revision,
                                cancellation: cancellation.clone(),
                                expected_leaf,
                                frontier: Vec::new(),
                            },
                        );
                        Ok(json!({"source_hash":source_hash}))
                    }
                    "optchat.plan" => {
                        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                        let draft = guard
                            .drafts
                            .get_mut(&key)
                            .context("OptChat preparation missing")?;
                        let tree = &draft.tree;
                        draft.frontier.retain(|index| !tree.summarized(*index));
                        // Finish the original frontier before checking for a new
                        // layout, exactly as the former unsliced Lua batch did.
                        let view = if draft.frontier.is_empty() {
                            tree.view(draft.budget).ok()
                        } else {
                            None
                        };
                        let merges = if view.is_some() {
                            Vec::new()
                        } else {
                            if draft.frontier.is_empty() {
                                draft.frontier = tree.ready_frontier(64)?;
                            }
                            tree.plan_frontier(
                                &draft.frontier,
                                MAX_MERGE_INPUT_BYTES,
                                MAX_PLAN_REPLY_BYTES,
                            )?
                        };
                        let reply = json!({"ready":view.is_some(),"merges":merges});
                        if serde_json::to_vec(&reply)?.len() > MAX_PLAN_REPLY_BYTES {
                            bail!("OptChat serialized plan reply exceeds byte limit; source retained and active view unchanged");
                        }
                        Ok(reply)
                    }
                    "optchat.accept" => {
                        let merge: Merge = serde_json::from_value(request.input["merge"].clone())?;
                        let text = request.input["text"]
                            .as_str()
                            .context("summary text required")?
                            .to_owned();
                        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                        guard
                            .drafts
                            .get_mut(&key)
                            .context("OptChat preparation missing")?
                            .tree
                            .accept(&merge, text)?;
                        Ok(json!({"accepted":true}))
                    }
                    "optchat.finish" => {
                        let (draft, view) = {
                            let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                            let draft = guard
                                .drafts
                                .remove(&key)
                                .context("OptChat preparation missing")?;
                            let view = draft.tree.view(draft.budget)?;
                            (draft, view)
                        };
                        let stored = Stored {
                            revision: draft
                                .revision
                                .checked_add(1)
                                .context("cache revision overflow")?,
                            cache: draft.tree.cache().clone(),
                            source_hash: draft.tree.source_hash().to_owned(),
                            branch: draft.tree.branch().map(str::to_owned),
                            creator_invocation: request.invocation.to_string(),
                            cache_only: true,
                        };
                        let prepared = tokio::task::spawn_blocking(move || {
                            path.as_deref()
                                .map(|path| PreparedFile::write(path, &stored))
                                .transpose()
                        })
                        .await??;
                        let guard = session.lock().await;
                        if guard.get_leaf_id() != draft.expected_leaf.as_deref() {
                            bail!(
                                "OptChat canonical branch changed during preparation; retry required"
                            );
                        }
                        let result = (|| -> anyhow::Result<Value> {
                            if cancellation.is_cancelled() {
                                bail!("cancelled");
                            }
                            let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                            if guard.revision != draft.revision {
                                bail!("OptChat cache revision changed; retry required");
                            }
                            if let Some(prepared) = &prepared {
                                prepared.publish()?;
                            }
                            if cancellation.is_cancelled() {
                                bail!("cancelled; valid source-linked cache retained without active publication");
                            }
                            guard.revision += 1;
                            guard.cache = draft.tree.cache().clone();
                            guard.last_prepared = Some(view.clone());
                            Ok(
                                json!({"text":view.text,"bytes":view.bytes,"entries":view.entries,
                                "source_hash":view.source_hash,"spans":view.spans}),
                            )
                        })()?;
                        Ok(result)
                    }
                    "optchat.zoom" | "optchat.search" => {
                        let (session_id, snapshot, branch) = {
                            let guard = session.lock().await;
                            (
                                guard.get_session_id().to_owned(),
                                guard.history_snapshot(),
                                guard.get_leaf_id().map(str::to_owned),
                            )
                        };
                        let history = snapshot.await?;
                        let stored = if path.is_some() {
                            tokio::task::spawn_blocking(move || read_store(path.as_deref()))
                                .await??
                        } else {
                            let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                            Stored {
                                revision: guard.revision,
                                cache: guard.cache.clone(),
                                ..Default::default()
                            }
                        };
                        // Recall validates all source-linked cache nodes, independently of
                        // the current model budget. No canonical source is stored in cache.
                        let tree = Tree::new(&session_id, history, branch, stored.cache, 16_384)?;
                        let limit = usize::try_from(request.input["limit"].as_u64().unwrap_or(32))?;
                        if request.operation == "optchat.zoom" {
                            let id = request.input["id"].as_str().context("recall id required")?;
                            let offset =
                                usize::try_from(request.input["offset"].as_u64().unwrap_or(0))?;
                            Ok(
                                json!({"entries":tree.recall(id, offset, limit)?,"offset":offset,"limit":limit}),
                            )
                        } else {
                            let query = request.input["query"]
                                .as_str()
                                .context("search query required")?;
                            Ok(
                                json!({"matches":tree.search(query, request.input["since"].as_str(), limit)?}),
                            )
                        }
                    }
                    "optchat.status" => {
                        let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                        Ok(json!({"last_prepared":guard.last_prepared,"revision":guard.revision}))
                    }
                    _ => bail!("unknown OptChat operation"),
                }
            };
            work.await.map_err(|error: anyhow::Error| error.to_string())
        })
    }
}
