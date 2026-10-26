//! Native compaction capabilities. Algorithms and transport stay in their owned
//! modules; this adapter supplies immutable input and returns unpublished rows.

use super::{
    compaction_entry_for, CompactOptions, CompactionAttempt, CompactionDetails, CompactionResult,
    Preparation, PreparedCompaction, SessionPlugins,
};
use crate::session_engine::compact_session;
use anyhow::{bail, Context};
use pa_types::session::{AgentMessage, ContextArchive, ContextRepresentation, FileEntry};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub(super) fn coverage(
    preparation: &Preparation,
    candidate: &PreparedCompaction,
) -> anyhow::Result<Vec<String>> {
    let end = match candidate.entry.first_kept_entry_id.as_deref() {
        None => preparation.branch.len(),
        Some(boundary) => preparation
            .branch
            .iter()
            .position(|entry| entry.id() == Some(boundary))
            .context("compactor retained boundary is outside the prepared branch")?,
    };
    let ids: Vec<_> = preparation.branch[..end]
        .iter()
        .filter_map(|entry| entry.id().map(str::to_owned))
        .collect();
    if ids.is_empty() {
        bail!("compactor proposal covers no canonical entries");
    }
    Ok(ids)
}

pub(super) async fn expand(
    plugins: &SessionPlugins,
    archive: &ContextArchive,
    model: Option<&pa_types::ai::Model>,
) -> anyhow::Result<Vec<AgentMessage>> {
    match &archive.representation {
        ContextRepresentation::Standard { .. } => Ok(Vec::new()),
        ContextRepresentation::Responses {
            provider,
            endpoint,
            model: archived_model,
            archive: body,
            ..
        } => {
            let model = model.context("native context archive needs a resolved model")?;
            let target = plugins.context_compactor(&archive.compactor)?;
            let output = plugins
                .invoke_context(
                    &target,
                    serde_json::json!({"stage":"restore"}),
                    &CancellationToken::new(),
                )
                .await?;
            let Some(adapter) =
                server_adapter(model, output.get("config").unwrap_or(&Value::Null))?
            else {
                bail!("server context archive no longer has a supported resolved route");
            };
            let expected = pa_ai::compaction::CompactionRoute {
                provider: provider.clone(),
                endpoint: endpoint.clone(),
                model: archived_model.clone(),
            };
            let configured = configured_route(model, output.get("config").unwrap_or(&Value::Null))?
                .context("native archive route is not configured")?;
            if expected != configured {
                bail!("native context archive route or model mismatch");
            }
            let _ = pa_ai::compaction::CompactionCandidate::restore(
                body,
                &adapter,
                &CancellationToken::new(),
            )?;
            Ok(Vec::new())
        }
        ContextRepresentation::Snapcompact { archive: body, .. } => {
            let target = plugins.context_compactor(&archive.compactor)?;
            let output = plugins
                .invoke_context(
                    &target,
                    serde_json::json!({"stage":"restore"}),
                    &CancellationToken::new(),
                )
                .await?;
            let model = model
                .context("Snapcompact archive needs resolved model capabilities")?
                .clone();
            let body = body.clone();
            let config = output.get("config").cloned().unwrap_or(Value::Null);
            tokio::task::spawn_blocking(move || expand_snap(&body, Some(&model), &config)).await?
        }
    }
}

fn configured_route(
    model: &pa_types::ai::Model,
    config: &Value,
) -> anyhow::Result<Option<pa_ai::compaction::CompactionRoute>> {
    let inference_endpoint = model.base_url.trim_end_matches('/');
    let compact_endpoint = format!("{inference_endpoint}/responses/compact");
    let route = match config.get("route") {
        Some(Value::Null) => return Ok(None),
        Some(route) => serde_json::from_value::<pa_ai::compaction::CompactionRoute>(route.clone())
            .context("invalid explicit server compaction route")?,
        None if model.api == "openai-responses" => pa_ai::compaction::CompactionRoute {
            provider: model.provider.clone(),
            endpoint: compact_endpoint.clone(),
            model: model.id.clone(),
        },
        None => return Ok(None),
    };
    if model.api != "openai-responses"
        || route.provider != model.provider
        || route.model != model.id
        || route.endpoint != compact_endpoint
    {
        return Err(pa_ai::compaction::CompactionError::RouteMismatch.into());
    }
    Ok(Some(route))
}

pub(super) fn server_adapter(
    model: &pa_types::ai::Model,
    config: &Value,
) -> anyhow::Result<Option<pa_ai::compaction::ResponsesCompactor>> {
    configured_route(model, config)?
        .map(|route| {
            pa_ai::compaction::ResponsesCompactor::new(
                route.clone(),
                Some(route),
                pa_ai::compaction::CompactionLimits::default(),
            )
            .map_err(anyhow::Error::from)
        })
        .transpose()
}

pub(super) async fn native_input(
    plugins: &SessionPlugins,
    archive: &ContextArchive,
    model: &pa_types::ai::Model,
    messages: &[pa_types::ai::Message],
    cancel: &CancellationToken,
) -> anyhow::Result<pa_ai::compaction::NativeResponsesInput> {
    let ContextRepresentation::Responses { archive: body, .. } = &archive.representation else {
        bail!("not a native Responses archive");
    };
    let target = plugins.context_compactor(&archive.compactor)?;
    let output = plugins
        .invoke_context(&target, serde_json::json!({"stage":"restore"}), cancel)
        .await?;
    let adapter = server_adapter(model, output.get("config").unwrap_or(&Value::Null))?
        .context("server compaction support is disabled")?;
    let candidate = pa_ai::compaction::CompactionCandidate::restore(body, &adapter, cancel)?;
    let mut window = candidate.replay(&adapter, cancel)?;
    if !messages.is_empty() {
        let fresh = pa_ai::compaction::responses_input_json(model, messages)?;
        window = window.append_input(&fresh, pa_ai::compaction::CompactionLimits::default())?;
    }
    Ok(window.inference_input(&adapter, cancel)?)
}

pub(super) fn eligible(
    preparation: &Preparation,
    policy: &str,
    output: &Value,
    options: &CompactOptions<'_>,
) -> anyhow::Result<Result<(), &'static str>> {
    match output.get("operation").and_then(Value::as_str) {
        Some("standard") => {
            let mut settings = options.settings;
            if policy == "full" {
                settings.keep_recent_tokens = 0;
            }
            let standard_options = CompactOptions {
                settings,
                semantic_edges: None,
                ..clone_options(options)
            };
            let source = standard_source(preparation);
            Ok(compact_session::prepare_snapshot(
                &source,
                preparation.leaf.as_deref(),
                &standard_options,
            )
            .map(|_| ())
            .map_err(compact_session::CompactSkip::user_message))
        }
        Some("server")
            if server_adapter(&options.model, output.get("config").unwrap_or(&Value::Null))?
                .is_none() =>
        {
            Ok(Err("No eligible compactor"))
        }
        Some("snapcompact") => {
            let limits = snap_limits(&options.model, output.get("config").unwrap_or(&Value::Null));
            if !limits.vision || limits.profile.is_none() {
                Ok(Err("No eligible compactor"))
            } else {
                Ok(Ok(()))
            }
        }
        Some(_) | None => Ok(Ok(())),
    }
}

fn standard_source(preparation: &Preparation) -> Vec<FileEntry> {
    if preparation.archive.as_ref().is_some_and(|archive| {
        !matches!(
            archive.representation,
            ContextRepresentation::Standard { .. }
        )
    }) {
        preparation
            .branch
            .iter()
            .filter(|entry| !matches!(entry, FileEntry::Compaction { .. }))
            .cloned()
            .collect()
    } else {
        preparation.branch.clone()
    }
}

pub(super) async fn run(
    plugins: &SessionPlugins,
    preparation: &Preparation,
    policy: &str,
    output: &Value,
    transaction: &mut CompactionAttempt,
    options: &CompactOptions<'_>,
    refinements: bool,
) -> anyhow::Result<Option<PreparedCompaction>> {
    let tokens =
        crate::session_engine::compaction::estimate_context_tokens(&preparation.messages).tokens;
    let config = output.get("config").unwrap_or(&Value::Null);
    let operation = output.get("operation").and_then(Value::as_str);
    if operation == Some("standard") {
        let mut settings = options.settings;
        if policy == "full" {
            settings.keep_recent_tokens = 0;
        }
        let standard_options = CompactOptions {
            settings,
            semantic_edges: None,
            ..clone_options(options)
        };
        // A native archive cannot act as a portable iterative-summary anchor.
        // Rebuild from canonical conversation instead of silently summarizing an
        // empty archive placeholder or treating its model-specific items as text.
        let source = standard_source(preparation);
        let Ok(mut attempt) = compact_session::prepare_snapshot(
            &source,
            preparation.leaf.as_deref(),
            &standard_options,
        ) else {
            return Ok(None);
        };
        if !refinements {
            attempt.history =
                crate::session_engine::refinement_gate::without_refinements(attempt.history);
            attempt.turn_prefix_messages =
                crate::session_engine::refinement_gate::without_refinements(
                    attempt.turn_prefix_messages,
                );
        }
        attempt.semantic_compaction = transaction.semantic_compaction.take();
        let result = compact_session::summarize_attempt(&attempt, options).await;
        transaction.semantic_compaction = attempt.semantic_compaction.take();
        return result.map(Some);
    }
    let (summary, first_kept_entry_id, representation, usage) = match operation {
        Some("server") => {
            let Some(adapter) = server_adapter(&options.model, config)? else {
                return Ok(None);
            };
            let cancel = CancellationToken::new();
            let _drop_cancel = cancel.clone().drop_guard();
            let portable: Vec<pa_types::ai::Message> =
                crate::session_engine::messages::convert_to_llm(&preparation.messages)
                    .iter()
                    .map(|message| {
                        crate::session_engine::provider_adapter::json_round_trip(message)
                            .context("invalid portable compaction message")
                    })
                    .collect::<anyhow::Result<_>>()?;
            let input = match preparation
                .archive
                .as_ref()
                .map(|archive| &archive.representation)
            {
                Some(ContextRepresentation::Responses { archive, .. }) => {
                    let candidate = pa_ai::compaction::CompactionCandidate::restore(
                        archive, &adapter, &cancel,
                    )?;
                    let mut window = candidate.replay(&adapter, &cancel)?;
                    if !portable.is_empty() {
                        window = window.append_input(
                            &pa_ai::compaction::responses_input_json(&options.model, &portable)?,
                            pa_ai::compaction::CompactionLimits::default(),
                        )?;
                    }
                    window
                }
                Some(
                    ContextRepresentation::Standard { .. }
                    | ContextRepresentation::Snapcompact { .. },
                )
                | None => {
                    let route = configured_route(&options.model, config)?
                        .context("server route missing")?;
                    pa_ai::compaction::ResponsesWindow::from_stateless_input(
                        &route,
                        &pa_ai::compaction::responses_input_json(&options.model, &portable)?,
                        pa_ai::compaction::CompactionLimits::default(),
                    )?
                }
            };
            let headers = match options.auxiliary {
                Some(context) => {
                    super::super::auxiliary_model::session_fallback_with_headers(
                        context,
                        &options.model,
                        options.api_key.clone(),
                    )
                    .headers
                }
                None => options.model.headers.clone(),
            };
            let result = adapter
                .compact(
                    &input,
                    pa_ai::compaction::CompactOptions {
                        bearer_token: options.api_key.as_deref().context(
                            "server compaction needs explicitly resolved authentication",
                        )?,
                        headers: headers.as_ref(),
                        instructions: options.custom_instructions,
                        budget: pa_ai::compaction::InputBudget {
                            // Raw bytes plus framing/instructions bound the whole window;
                            // billed output usage is not the next prompt's context size.
                            input_tokens_upper_bound: u64::try_from(input.input_json().len())
                                .unwrap_or(u64::MAX)
                                .saturating_add(
                                    u64::try_from(
                                        options.custom_instructions.unwrap_or_default().len(),
                                    )
                                    .unwrap_or(u64::MAX),
                                )
                                .saturating_add(4096),
                            context_window_tokens: options.model.context_window,
                        },
                    },
                    &cancel,
                )
                .await;
            let candidate = match result {
                Ok(candidate) => candidate,
                // These are known preflight declines or explicit remote capability
                // rejections. Never replay an uncertain request via another strategy.
                Err(
                    pa_ai::compaction::CompactionError::Unsupported
                    | pa_ai::compaction::CompactionError::InputTooLarge
                    | pa_ai::compaction::CompactionError::Http {
                        kind: pa_ai::compaction::HttpFailureKind::Capability,
                        ..
                    },
                ) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let route =
                configured_route(&options.model, config)?.context("server route missing")?;
            let usage = candidate.usage();
            let usage = Some(pa_types::ai::Usage {
                input: usage.input_tokens,
                output: usage.output_tokens,
                total_tokens: usage.total_tokens,
                cache_read: usage.cached_input_tokens.unwrap_or(0),
                cache_write: usage.cache_write_tokens.unwrap_or(0),
                ..Default::default()
            });
            (
                "Provider-native context archive".to_owned(),
                None,
                ContextRepresentation::Responses {
                    rest: pa_types::JsonMap::new(),
                    provider: route.provider,
                    endpoint: route.endpoint,
                    model: route.model,
                    archive: candidate.serialize()?,
                },
                usage,
            )
        }
        Some("snapcompact") => {
            let Some(archive) =
                prepare_snap(preparation, &options.model, config, options.abort).await?
            else {
                return Ok(None);
            };
            (
                "Bitmap context archive".to_owned(),
                None,
                ContextRepresentation::Snapcompact {
                    rest: pa_types::JsonMap::new(),
                    archive,
                },
                None,
            )
        }
        Some(other) => bail!("unknown native compaction operation {other}"),
        None => {
            let summary = output
                .get("summary")
                .and_then(Value::as_str)
                .context("successful compactor needs summary")?
                .to_owned();
            if summary.trim().is_empty() {
                bail!("compactor returned an empty summary");
            }
            let first_kept_entry_id = output
                .get("first_kept_entry_id")
                .and_then(Value::as_str)
                .context("successful compactor needs retained boundary")?
                .to_owned();
            (
                summary,
                Some(first_kept_entry_id),
                ContextRepresentation::Standard {
                    rest: pa_types::JsonMap::new(),
                },
                None,
            )
        }
    };
    let result = CompactionResult {
        summary,
        first_kept_entry_id,
        tokens_before: tokens,
        usage,
    };
    let mut entry = compaction_entry_for(
        &result,
        &CompactionDetails::default(),
        options.custom_instructions,
        None,
        None,
    );
    entry.context_archive = Some(ContextArchive {
        version: 1,
        strategy: String::new(),
        compactor: String::new(),
        creator_generation: String::new(),
        prepared_leaf: String::new(),
        covered_entry_ids: Vec::new(),
        representation,
    });
    let _ = plugins;
    Ok(Some(PreparedCompaction { result, entry }))
}

fn clone_options<'a>(options: &CompactOptions<'a>) -> CompactOptions<'a> {
    CompactOptions {
        inference: options.inference.clone(),
        model: options.model.clone(),
        api_key: options.api_key.clone(),
        custom_instructions: options.custom_instructions,
        settings: options.settings,
        abort: options.abort,
        auxiliary: options.auxiliary,
        summary_delta: options.summary_delta.clone(),
        semantic_edges: options.semantic_edges.clone(),
    }
}

fn snap_limits(model: &pa_types::ai::Model, config: &Value) -> crate::snapcompact::ReplayLimits {
    use crate::snapcompact::RenderProfile;
    let profile = match config.get("profile").and_then(Value::as_str) {
        Some("claude") => Some(RenderProfile::Claude),
        Some("openai") => Some(RenderProfile::OpenAi),
        Some("google") => Some(RenderProfile::Google),
        Some("explicit") => Some(RenderProfile::Explicit {
            frame_size: config
                .get("frame_size")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(1568),
            frame_tokens: config
                .get("frame_tokens")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(5024),
        }),
        Some(_) => None,
        None => match model.api.as_str() {
            "anthropic-messages" => Some(RenderProfile::Claude),
            "google-generative-ai" | "google-vertex" => Some(RenderProfile::Google),
            "openai-completions"
            | "openai-responses"
            | "openai-codex-responses"
            | "azure-openai-responses" => {
                // A gateway's provider name says nothing about image encoding.
                // Use only recognized model lineages; aliases need an explicit profile.
                let lineage = model.id.rsplit('/').next().unwrap_or(&model.id);
                if lineage.starts_with("claude-") {
                    Some(RenderProfile::Claude)
                } else if lineage.starts_with("gemini-") {
                    Some(RenderProfile::Google)
                } else if lineage.starts_with("gpt-")
                    || lineage.starts_with("chatgpt-")
                    || ["o1", "o3", "o4"].iter().any(|family| {
                        lineage == *family || lineage.starts_with(&format!("{family}-"))
                    })
                {
                    Some(RenderProfile::OpenAi)
                } else {
                    None
                }
            }
            _ => None,
        },
    };
    let bound = |name: &str, default: usize| {
        config
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(default)
            .min(default)
    };
    crate::snapcompact::ReplayLimits {
        vision: model.input.contains(&pa_types::ai::ModelInput::Image),
        profile,
        max_images: bound("max_images", 80),
        max_image_bytes: bound("max_image_bytes", 8 * 1024 * 1024),
        max_payload_bytes: bound("max_payload_bytes", 24 * 1024 * 1024),
        available_tokens: bound(
            "available_tokens",
            usize::try_from(model.context_window.saturating_sub(20_480)).unwrap_or(usize::MAX),
        ),
        text_fallback_chars: config
            .get("text_fallback_chars")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok()),
    }
}

async fn prepare_snap(
    preparation: &Preparation,
    model: &pa_types::ai::Model,
    config: &Value,
    abort: Option<&pa_agent::abort::AbortSignal>,
) -> anyhow::Result<Option<String>> {
    let prior = match preparation
        .archive
        .as_ref()
        .map(|archive| &archive.representation)
    {
        Some(ContextRepresentation::Snapcompact { archive, .. }) => {
            Some(crate::snapcompact::Archive::from_json(archive)?)
        }
        Some(ContextRepresentation::Responses { .. } | ContextRepresentation::Standard { .. })
        | None => None,
    };
    // A prior Snap archive already owns the covered prefix. Feed only the current
    // uncovered continuation; never serialize expanded PNGs back into the source.
    let messages = if prior.is_some() {
        let covered: std::collections::HashSet<_> = preparation
            .archive
            .as_ref()
            .into_iter()
            .flat_map(|archive| archive.covered_entry_ids.iter().map(String::as_str))
            .collect();
        preparation
            .branch
            .iter()
            .filter(|entry| entry.id().is_some_and(|id| !covered.contains(id)))
            .filter_map(|entry| match entry {
                FileEntry::Message { message, .. } => Some(message.clone()),
                FileEntry::CustomMessage { payload, .. } => Some(AgentMessage::Custom(
                    crate::session::create_custom_message(payload, entry),
                )),
                _ => None,
            })
            .collect()
    } else if preparation.archive.as_ref().is_some_and(|archive| {
        matches!(
            archive.representation,
            ContextRepresentation::Responses { .. }
        )
    }) {
        // Provider-native prefixes are not portable text. A bitmap fallback must
        // rebuild from the canonical branch, not archive only the uncovered tail.
        preparation
            .branch
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Message { message, .. } => Some(message.clone()),
                FileEntry::CustomMessage { payload, .. } => Some(AgentMessage::Custom(
                    crate::session::create_custom_message(payload, entry),
                )),
                FileEntry::BranchSummary { payload, .. } => Some(AgentMessage::BranchSummary(
                    pa_types::session::BranchSummaryMessage {
                        rest: pa_types::JsonMap::new(),
                        summary: payload.summary.clone(),
                        from_id: payload.from_id.clone(),
                        timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
                    },
                )),
                _ => None,
            })
            .collect()
    } else {
        preparation.messages.clone()
    };
    let mut policy = crate::snapcompact::Config::default();
    if let Some(value) = config.get("max_frames").and_then(Value::as_u64) {
        policy.max_frames = usize::try_from(value).unwrap_or(80).min(80);
    }
    if let Some(value) = config.get("max_source_bytes").and_then(Value::as_u64) {
        policy.max_source_bytes = usize::try_from(value).unwrap_or(1_000_000).min(4_000_000);
    }
    let limits = snap_limits(model, config);
    let abort = abort.cloned();
    let retirement = CancellationToken::new();
    let _cancel_on_drop = retirement.clone().drop_guard();
    let result = tokio::task::spawn_blocking(move || {
        crate::snapcompact::prepare(prior.as_ref(), &messages, &[], &policy, &limits, &|| {
            retirement.is_cancelled()
                || abort
                    .as_ref()
                    .is_some_and(pa_agent::abort::AbortSignal::is_aborted)
        })
    })
    .await?;
    match result {
        Ok(archive) => Ok(Some(archive.to_json()?)),
        Err(crate::snapcompact::Error::Declined(_)) => Ok(None),
        Err(crate::snapcompact::Error::Cancelled) => Err(pa_agent::abort::aborted_error()),
        Err(error) => Err(error.into()),
    }
}

fn expand_snap(
    body: &str,
    model: Option<&pa_types::ai::Model>,
    config: &Value,
) -> anyhow::Result<Vec<AgentMessage>> {
    use base64::Engine as _;
    let model = model.context("Snapcompact archive needs resolved model capabilities")?;
    let archive = crate::snapcompact::Archive::from_json(body)?;
    let blocks = crate::snapcompact::replay(&archive, &snap_limits(model, config))?;
    let content = blocks
        .into_iter()
        .map(|block| match block {
            crate::snapcompact::ReplayBlock::Text(text) => {
                pa_types::ai::UserContentBlock::Text(pa_types::ai::TextContent {
                    text,
                    text_signature: None,
                    rest: pa_types::JsonMap::new(),
                })
            }
            crate::snapcompact::ReplayBlock::Image { png, .. } => {
                pa_types::ai::UserContentBlock::Image(pa_types::ai::ImageContent {
                    data: base64::engine::general_purpose::STANDARD.encode(png),
                    mime_type: "image/png".into(),
                    rest: pa_types::JsonMap::new(),
                })
            }
        })
        .collect();
    Ok(vec![AgentMessage::User(pa_types::ai::UserMessage {
        content: pa_types::ai::UserContent::Blocks(content),
        timestamp: 0,
        rest: pa_types::JsonMap::new(),
    })])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapcompact::RenderProfile;
    use serde_json::json;

    #[test]
    fn snap_profiles_follow_resolved_transport_and_lineage_not_provider_brand() {
        let mut model: pa_types::ai::Model = serde_json::from_value(json!({
            "id":"alias", "name":"Test", "api":"anthropic-messages", "provider":"gateway",
            "baseUrl":"http://127.0.0.1:9/v1", "reasoning":false, "input":["text","image"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
            "contextWindow":100_000,"maxTokens":1000
        }))
        .unwrap();
        for (api, provider, id, expected) in [
            (
                "anthropic-messages",
                "gateway",
                "alias",
                Some(RenderProfile::Claude),
            ),
            (
                "google-generative-ai",
                "gateway",
                "alias",
                Some(RenderProfile::Google),
            ),
            (
                "google-vertex",
                "gateway",
                "alias",
                Some(RenderProfile::Google),
            ),
            (
                "openai-completions",
                "gateway",
                "anthropic/claude-sonnet-4",
                Some(RenderProfile::Claude),
            ),
            (
                "openai-completions",
                "gateway",
                "google/gemini-2.5-pro",
                Some(RenderProfile::Google),
            ),
            (
                "openai-responses",
                "gateway",
                "openai/gpt-5",
                Some(RenderProfile::OpenAi),
            ),
            ("openai-completions", "openai", "unknown-vision-model", None),
            ("custom-transport", "openai", "gpt-5", None),
        ] {
            model.api = api.into();
            model.provider = provider.into();
            model.id = id.into();
            assert_eq!(
                snap_limits(&model, &Value::Null).profile,
                expected,
                "resolved model {provider}/{id} on {api}"
            );
        }
        // Explicit policy wins even for an otherwise recognized native route.
        model.api = "anthropic-messages".into();
        assert_eq!(
            snap_limits(&model, &json!({"profile":"google"})).profile,
            Some(RenderProfile::Google)
        );
        assert_eq!(
            snap_limits(&model, &json!({"profile":"unsupported"})).profile,
            None
        );
        model.input = vec![pa_types::ai::ModelInput::Text];
        let limits = snap_limits(&model, &Value::Null);
        assert!(!limits.vision);
    }

#[test]
fn snap_user_images_match_shared_usage_estimate() {
    let model: pa_types::ai::Model = serde_json::from_value(json!({
        "id":"gpt-5","name":"Test","api":"openai-completions","provider":"test",
        "baseUrl":"http://127.0.0.1:9/v1","reasoning":false,"input":["text","image"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100_000,"maxTokens":1000
    })).unwrap();
    let config = json!({"profile":"explicit","frame_size":128,"frame_tokens":1});
    let source: AgentMessage = serde_json::from_value(json!({"role":"user","content":"archive text ".repeat(100),"timestamp":0})).unwrap();
    let archive = crate::snapcompact::prepare(None, &[source], &[], &crate::snapcompact::Config::default(), &snap_limits(&model, &config), &||false).unwrap();
    let messages = expand_snap(&archive.to_json().unwrap(), Some(&model), &config).unwrap();
    assert!(archive.frame_count() > 0);
    let typed = crate::session_engine::compaction::estimate_context_tokens(&messages).tokens;
    let shared: u64 = messages.iter().map(|message|pa_types::usage::estimate_tokens(&serde_json::to_value(message).unwrap())).sum();
    assert_eq!(typed, shared);
    let text: AgentMessage = serde_json::from_value(json!({"role":"user","content":"12345678","timestamp":0})).unwrap();
    assert_eq!(crate::session_engine::compaction::estimate_tokens(&text), 2);
}
}
