//! Session-local plugin composition. Kernel callbacks capture only immutable identity
//! and the host Arc: they never wait for the engine/session mutex held by a tool turn.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    engine::SessionEngine,
    slash_commands::{parse_session_command, SessionSlashCommand},
    AgentSession,
};
use crate::{
    kernel::shared::{host_handler, HostRequestHandlers},
    plugins::{
        CapabilityFuture, CapabilityRequest, Contribution, ContributionKind, ContributionSnapshot,
        HostCapabilities, InvocationEvent, InvocationMode, InvocationOutcome, PluginHost,
        PluginSource,
    },
    settings::SettingsManager,
};

struct SessionFacts {
    info: Value,
    delegates: Mutex<BTreeMap<String, Arc<dyn HostCapabilities>>>,
}

impl HostCapabilities for SessionFacts {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let info = self.info.clone();
        let delegate = self
            .delegates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(prefix, _)| request.operation.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, delegate)| Arc::clone(delegate));
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err("cancelled".into());
            }
            if request.session != info["session"].as_str().unwrap_or_default() {
                return Err("plugin session mismatch".into());
            }
            match request.operation.as_str() {
                "session.info" => Ok(info),
                _ => match delegate {
                    Some(delegate) => delegate.call(request, cancellation).await,
                    None => Err(format!(
                        "unsupported plugin capability: {}",
                        request.operation
                    )),
                },
            }
        })
    }
}

/// A core command inventory entry, independent of daemon/TUI discovery metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommandInfo {
    pub name: String,
    pub description: String,
    pub plugin: Option<Contribution>,
}

/// One session's explicit settings paths, active Lua host, and reload diagnostic.
/// It never owns canonical history, credentials, or mutable global runtime state.
pub(crate) struct SessionPlugins {
    host: Arc<PluginHost>,
    session: String,
    cwd: PathBuf,
    agent_dir: PathBuf,
    diagnostic: Mutex<Option<String>>,
    context_selection: Mutex<Option<Contribution>>,
    // Rotated only while holding context_selection. This identifies one actual
    // selection registration, including away-then-back to the same generation.
    context_registration: Mutex<Arc<()>>,
    disabled_plugins: Mutex<std::collections::BTreeSet<String>>,
    pinned_context_strategy: Mutex<Option<String>>,
    optional_builtins: Mutex<std::collections::BTreeMap<String, String>>,
    capabilities: Arc<SessionFacts>,
}

#[derive(Clone)]
pub(super) struct ContextRegistration(Arc<()>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredPlugin {
    enabled: Option<bool>,
    path: Option<PathBuf>,
    #[serde(default = "empty_config")]
    config: Value,
}

fn empty_config() -> Value {
    json!({})
}

impl SessionPlugins {
    /// The settings scopes the plugin host resolves against; the lifecycle
    /// observers read the retry policy from the same pair.
    pub(crate) fn settings_dirs(&self) -> (&std::path::Path, &std::path::Path) {
        (&self.cwd, &self.agent_dir)
    }

    pub(crate) fn new(session: String, cwd: PathBuf, agent_dir: PathBuf, depth: u32) -> Self {
        // Command reservations belong to session composition, not the Lua runtime.
        let registry = super::slash_commands::SlashCommandRegistry::builtin();
        let reserved = registry
            .suggestion_candidates()
            .into_iter()
            .map(str::to_string);
        let capabilities = Arc::new(SessionFacts {
            info: json!({
                "session": session, "cwd": cwd, "role": if depth == 0 { "root" } else { "child" },
            }),
            delegates: Mutex::default(),
        });
        let host = Arc::new(PluginHost::new(
            session.clone(),
            capabilities.clone(),
            reserved,
        ));
        let snapshot = host
            .reconcile(crate::plugins::context::builtin_sources())
            .expect("compiled context plugins must activate");
        let context_selection = crate::plugins::context::resolve(
            &snapshot,
            ContributionKind::ContextStrategy,
            crate::plugins::context::DEFAULT_STRATEGY,
        )
        .expect("compiled default context strategy must register");
        Self {
            host,
            session,
            cwd,
            agent_dir,
            diagnostic: Mutex::default(),
            context_selection: Mutex::new(Some(context_selection)),
            context_registration: Mutex::new(Arc::new(())),
            disabled_plugins: Mutex::default(),
            pinned_context_strategy: Mutex::new(None),
            optional_builtins: Mutex::default(),
            capabilities,
        }
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn reload(self: &Arc<Self>) -> anyhow::Result<Arc<ContributionSnapshot>> {
        let manager = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || {
            let settings = SettingsManager::create(&manager.cwd, &manager.agent_dir);
            if let Some(error) = settings.errors().first() {
                bail!("plugin settings read failed: {}", error.message);
            }
            let legacy_enabled = settings.get_harness_refinements();
            let settings = settings.settings();
            let pinned = manager
                .pinned_context_strategy
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let strategy = match pinned.as_deref() {
                Some(strategy) => strategy,
                None => match settings.extra.get("contextStrategy") {
                    None => crate::plugins::context::DEFAULT_STRATEGY,
                    Some(value) => value
                        .as_str()
                        .context("contextStrategy must be plugin/name")?,
                },
            };
            let mut desired: std::collections::BTreeMap<_, _> =
                crate::plugins::context::builtin_sources()
                    .into_iter()
                    .map(|source| (source.name.clone(), source))
                    .collect();
            // Legacy compatibility affects default activation, never scope enrollment.
            // Explicit plugin configuration takes precedence.
            if legacy_enabled {
                desired.insert("harness".into(), crate::harness_plugin::source(settings));
            }
            // Retirement is a mandatory control operation, not an enabled-source
            // replacement. Resolve intent before reading or evaluating any source.
            let mut wanted: std::collections::BTreeSet<_> = desired.keys().cloned().collect();
            let mut disabled = manager
                .disabled_plugins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(value) = settings.plugins.as_ref() {
                let entries = value.as_object().context("plugins must be an object")?;
                for (name, entry) in entries {
                    if crate::plugins::validate_plugin_name(name).is_err() {continue;}
                    if !entry.is_object() {wanted.insert(name.clone());continue;}
                    let enabled = match entry.get("enabled") {
                        Some(Value::Bool(enabled)) => *enabled,
                        None => desired.contains_key(name),
                        // An invalid enabled setting is a replacement error, not
                        // an implicit request to retire the last valid source.
                        Some(_) => true,
                    };
                    if enabled {
                        wanted.insert(name.clone());
                    } else {
                        wanted.remove(name);
                        disabled.insert(name.clone());
                    }
                }
            }
            disabled.retain(|name| !wanted.contains(name));
            let removed;
            {
                let mut selection = manager
                    .context_selection
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                removed = manager.host.retain_plugins(&wanted)?;
                if selection
                    .as_ref()
                    .is_some_and(|target| removed.contains(&target.plugin))
                {
                    *selection = None;
                    *manager.context_registration.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(());
                }
                disabled.extend(removed.iter().cloned());
                manager.disabled_plugins.lock().unwrap_or_else(PoisonError::into_inner).clone_from(&disabled);
            }
            let enabled_result=(||->anyhow::Result<Arc<ContributionSnapshot>> {
            if let Some(value) = settings.plugins.as_ref() {
                let entries = value.as_object().context("plugins must be an object")?;
                for (name, entry) in entries {
                    crate::plugins::validate_plugin_name(name)?;
                    let entry: ConfiguredPlugin = serde_json::from_value(entry.clone())
                        .with_context(|| format!("invalid plugin configuration: {name}"))?;
                    let builtin = desired.remove(name).or_else(|| {
                        (name == "harness").then(|| crate::harness_plugin::source(settings))
                    });
                    if !entry.enabled.unwrap_or(builtin.is_some()) {
                        disabled.insert(name.clone());
                        continue;
                    }
                    if !entry.config.is_object() {
                        bail!("plugin {name} config must be an object");
                    }
                    let source = match entry.path {
                        Some(path) => {
                            // Relative explicit paths resolve from cwd. No ambient discovery.
                            let path = manager.cwd.join(path);
                            std::fs::read_to_string(&path).with_context(|| {
                                format!("read plugin {name} at {}", path.display())
                            })?
                        }
                        None => builtin
                            .map(|source| source.source)
                            .or_else(|| {
                                manager
                                    .optional_builtins
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .get(name)
                                    .cloned()
                            })
                            .with_context(|| format!("plugin {name} needs an explicit path"))?,
                    };
                    let mut plugin_config = if name == "harness" {
                        crate::harness_plugin::source(settings).config
                    } else {
                        json!({})
                    };
                    if let Some(config) = plugin_config.as_object_mut() {
                        config.extend(
                            entry
                                .config
                                .as_object()
                                .expect("validated config object")
                                .clone(),
                        );
                    }
                    desired.insert(
                        name.clone(),
                        PluginSource {
                            name: name.clone(),
                            source,
                            config: plugin_config,
                        },
                    );
                }
            }
            for contribution in &manager.host.snapshot().contributions {
                if !desired.contains_key(&contribution.plugin) {
                    disabled.insert(contribution.plugin.clone());
                }
            }
            let selected_plugin = strategy.split_once('/').map_or("", |(plugin, _)| plugin);
            let selection_disabled = disabled.contains(selected_plugin);
            // Compiled policy declares a static chain. Custom Lua strategies can
            // choose dependencies dynamically and must validate them at invocation.
            let builtin_compactors = desired
                .get("default_context")
                .filter(|source| {
                    strategy == crate::plugins::context::DEFAULT_STRATEGY
                        && source.source == crate::plugins::context::DEFAULT_SOURCE
                })
                .map(|source| {
                    source
                        .config
                        .get("compactors")
                        .cloned()
                        .unwrap_or_else(|| json!(["standard_compaction/standard"]))
                });
            // Readers take the same mutex before checking host authority. Selection and
            // candidate publication therefore become visible as one composition change.
            let mut selection = manager
                .context_selection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let snapshot =
                manager
                    .host
                    .reconcile_validated(desired.into_values().collect(), |candidate| {
                        if !selection_disabled {
                            crate::plugins::context::resolve(
                                candidate,
                                ContributionKind::ContextStrategy,
                                strategy,
                            )?;
                        }
                        if let Some(routes) = &builtin_compactors {
                            let routes = routes.as_array().ok_or_else(|| {
                                crate::plugins::PluginError::Invalid(
                                    "default context compactors must be an array".into(),
                                )
                            })?;
                            for route in routes {
                                let route = route.as_str().ok_or_else(|| {
                                    crate::plugins::PluginError::Invalid(
                                        "default context compactor routes must be strings".into(),
                                    )
                                })?;
                                let plugin = route.split_once('/').map_or("", |(plugin, _)| plugin);
                                if !disabled.contains(plugin) {
                                    crate::plugins::context::resolve(
                                        candidate,
                                        ContributionKind::Compactor,
                                        route,
                                    )?;
                                }
                            }
                        }
                        Ok(())
                    })?;
            let next_selection = if selection_disabled {
                None
            } else {
                Some(crate::plugins::context::resolve(
                    &snapshot,
                    ContributionKind::ContextStrategy,
                    strategy,
                )?)
            };
            if *selection != next_selection {
                *manager.context_registration.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(());
                *selection = next_selection;
            }
            *manager
                .disabled_plugins
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = disabled;
            Ok(snapshot)
            })();
            if removed.is_empty(){enabled_result}else{enabled_result.map_err(|error|{
                let detail=format!("disabled/removed plugins applied [{}]; enabled replacements rejected: {error:#}",removed.iter().cloned().collect::<Vec<_>>().join(", "));
                error.context(detail)
            })}
        })
        .await
        .context("plugin reload task failed")?;
        *self
            .diagnostic
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            result.as_ref().err().map(ToString::to_string);
        result
    }

    pub(super) fn host(&self) -> Arc<PluginHost> {
        Arc::clone(&self.host)
    }

    pub(super) fn set_native_capabilities(&self, delegate: Arc<dyn HostCapabilities>) {
        self.set_capability_delegate("native.", delegate);
    }

    /// Replace one namespace adapter without changing other registered namespaces.
    pub(super) fn set_capability_delegate(
        &self,
        prefix: &str,
        delegate: Arc<dyn HostCapabilities>,
    ) {
        self.capabilities
            .delegates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(prefix.to_owned(), delegate);
    }

    /// Register a compiled source without activating it. Only an explicitly
    /// enabled plugins entry can publish this optional builtin's contributions.
    // Optional first-party backends register sources at composition time; the
    // base engine intentionally links none of those optional sources by default.
    #[allow(dead_code)]
    pub(super) fn register_optional_builtin(&self, name: &str, source: &str) {
        self.optional_builtins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), source.to_owned());
    }

    pub(super) fn pin_context_strategy(&self, selection: Option<String>) {
        *self
            .pinned_context_strategy
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = selection;
    }

    pub(super) fn context_selection(&self) -> anyhow::Result<Contribution> {
        let selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let target=selection.as_ref().ok_or_else(||anyhow::anyhow!("selected context strategy is unavailable; re-enable its plugin, select another strategy, or start a new session"))?;
        self.host
            .with_current_contributions(&[target], || target.clone())
            .map_err(Into::into)
    }

    pub(super) fn context_contribution_is_current(&self, target: &Contribution) -> bool {
        self.host
            .with_current_contributions(&[target], || ())
            .is_ok()
    }

    pub(super) fn context_target(&self, route: &str) -> anyhow::Result<Contribution> {
        crate::plugins::context::resolve(
            &self.host.snapshot(),
            ContributionKind::ContextStrategy,
            route,
        )
        .map_err(Into::into)
    }

    pub(super) fn context_route_disabled(&self, route: &str) -> bool {
        route.split_once('/').is_some_and(|(plugin, _)| {
            self.disabled_plugins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(plugin)
        })
    }

    pub(super) fn commit_context_selection(
        &self,
        target: &Contribution,
        commit: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.host.with_current_contributions(&[target], || {
            commit()?;
            *self
                .pinned_context_strategy
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some(format!("{}/{}", target.plugin, target.name));
            *selection = Some(target.clone());
            *self
                .context_registration
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Arc::new(());
            Ok(())
        })?
    }

    pub(super) fn context_session_id(&self) -> &str {
        &self.session
    }

    pub(super) async fn invoke_strategy_candidate(
        &self,
        target: &Contribution,
        input: Value,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<Value> {
        if target.kind != ContributionKind::ContextStrategy || target.mode != InvocationMode::Unary
        {
            bail!("not a unary context strategy");
        }
        if !self.context_contribution_is_current(target) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        let output = invoke(&self.host, target, input, cancellation).await?;
        if !self.context_contribution_is_current(target) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        Ok(output)
    }

    pub(super) fn context_compactor(&self, route: &str) -> anyhow::Result<Contribution> {
        crate::plugins::context::resolve(&self.host.snapshot(), ContributionKind::Compactor, route)
            .map_err(Into::into)
    }

    pub(super) fn context_is_current(&self, target: &Contribution) -> bool {
        let selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (target.kind != ContributionKind::ContextStrategy || selection.as_ref() == Some(target))
            && self
                .host
                .with_current_contributions(&[target], || ())
                .is_ok()
    }

    pub(super) fn context_retirement(
        &self,
        target: &Contribution,
    ) -> anyhow::Result<CancellationToken> {
        self.host.retirement(target).map_err(Into::into)
    }

    pub(super) fn with_strategy_authority<T>(
        &self,
        target: &Contribution,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if selection.as_ref() != Some(target) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        self.host.with_current_contributions(&[target], commit)?
    }

    pub(super) fn capture_context_registration(
        &self,
        strategy: &Contribution,
    ) -> anyhow::Result<ContextRegistration> {
        self.with_strategy_authority(strategy, || {
            Ok(ContextRegistration(Arc::clone(
                &self
                    .context_registration
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            )))
        })
    }

    // A successful prewrite closure admits exactly one already-built mutation.
    // Retirement after admission may not cancel its receipt adoption/ACK, but
    // publication must pass this same registration guard again after ACK.
    pub(super) fn with_registered_context_authority<T>(
        &self,
        registration: &ContextRegistration,
        strategy: &Contribution,
        compactor: &Contribution,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = Arc::ptr_eq(
            &registration.0,
            &self
                .context_registration
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if !current || selection.as_ref() != Some(strategy) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        if compactor.kind != ContributionKind::Compactor {
            bail!("not a compactor contribution");
        }
        self.host
            .with_current_contributions(&[strategy, compactor], commit)?
    }

    pub(super) fn with_context_authority<T>(
        &self,
        strategy: &Contribution,
        compactor: &Contribution,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let selection = self
            .context_selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if selection.as_ref() != Some(strategy) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        if compactor.kind != ContributionKind::Compactor {
            bail!("not a compactor contribution");
        }
        self.host
            .with_current_contributions(&[strategy, compactor], commit)?
    }

    pub(super) async fn invoke_context(
        &self,
        target: &Contribution,
        input: Value,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<Value> {
        if !matches!(
            target.kind,
            ContributionKind::ContextStrategy | ContributionKind::Compactor
        ) || target.mode != InvocationMode::Unary
        {
            bail!("not a unary context contribution");
        }
        if !self.context_is_current(target) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        let output = invoke(&self.host, target, input, cancellation).await?;
        if !self.context_is_current(target) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        Ok(output)
    }

    fn prompt_tail(&self) -> String {
        let snapshot = self.host.snapshot();
        let exports: Vec<_> = snapshot.contributions.iter()
            .filter(|target| target.kind == ContributionKind::Export && target.mode == InvocationMode::Unary)
            .map(|target| json!({"call":format!("plugin.{}.{}(**kwargs)",target.plugin,target.name),"description":target.description})).collect();
        let commands: Vec<_> = snapshot.contributions.iter()
            .filter(|target| target.kind == ContributionKind::Command && target.mode == InvocationMode::Unary)
            .map(|target| json!({"command":format!("/{}",target.name),"description":target.description})).collect();
        if exports.is_empty() && commands.is_empty() {
            return String::new();
        }
        format!("\n\n## Active plugin inventory\nSession-local Python exports (keyword arguments only) and slash commands:\n{}", json!({"exports":exports,"commands":commands}))
    }

    pub(crate) fn handlers(&self) -> HostRequestHandlers {
        let mut handlers = HostRequestHandlers::new();
        let host = Arc::clone(&self.host);
        let session = self.session.clone();
        handlers.register("plugin.catalog", host_handler(move |payload| {
            let host = Arc::clone(&host);
            let session = session.clone();
            async move {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Request {
                    #[serde(rename = "type")]
                    kind: String,
                    session: String,
                }
                let request: Request = serde_json::from_value(payload.data)?;
                if request.kind != "plugin.catalog" || request.session != session { bail!("plugin session or request type mismatch"); }
                let snapshot = host.snapshot();
                let exports: Vec<_> = snapshot.contributions.iter()
                    .filter(|target| target.kind == ContributionKind::Export)
                    .map(|target| json!({
                        "plugin": target.plugin, "generation": target.generation.to_string(),
                        "name": target.name, "description": target.description,
                        "mode": match target.mode { InvocationMode::Unary => "unary", InvocationMode::Streaming => "streaming" },
                    })).collect();
                Ok(json!({"session": session, "revision": snapshot.revision, "exports": exports}))
            }
        }));
        let host = Arc::clone(&self.host);
        let session = self.session.clone();
        handlers.register(
            "plugin.invoke",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                let session = session.clone();
                async move {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Request {
                        #[serde(rename = "type")]
                        kind: String,
                        session: String,
                        generation: String,
                        plugin: String,
                        export: String,
                        input: serde_json::Map<String, Value>,
                    }
                    let request: Request = serde_json::from_value(payload.data)?;
                    if request.kind != "plugin.invoke" || request.session != session {
                        bail!("plugin session mismatch");
                    }
                    let generation = uuid::Uuid::parse_str(&request.generation)?;
                    let snapshot = host.snapshot();
                    let target = snapshot
                        .contributions
                        .iter()
                        .find(|target| {
                            target.kind == ContributionKind::Export
                                && target.plugin == request.plugin
                                && target.generation == generation
                                && target.name == request.export
                        })
                        .context("plugin export generation is not active")?;
                    if target.mode != InvocationMode::Unary {
                        bail!("Python plugin streaming invocation is unsupported");
                    }
                    invoke(
                        &host,
                        target,
                        Value::Object(request.input),
                        &CancellationToken::new(),
                    )
                    .await
                }
            }),
        );
        handlers
    }
}

async fn invoke(
    host: &PluginHost,
    target: &Contribution,
    input: Value,
    cancellation: &CancellationToken,
) -> anyhow::Result<Value> {
    let mut invocation = host.invoke(target, input, cancellation)?;
    // Observer updates are diagnostic events, not a client UI stream.
    while let Some(event) = invocation.next().await {
        match event {
            InvocationEvent::Update(value) => {
                tracing::debug!(plugin = %target.plugin, update = %value, "plugin observer update");
            }
            InvocationEvent::Finished(Ok(
                InvocationOutcome::Unary(value) | InvocationOutcome::Streamed(value),
            )) => return Ok(value),
            InvocationEvent::Finished(Err(error)) => return Err(error.into()),
        }
    }
    Err(anyhow!("plugin invocation ended without an outcome"))
}

/// Read active inventory only at the model-call seam. Mandatory prompt layers
/// keep their stable prefix; reload/disable changes the dynamic tail immediately.
pub(crate) fn wrap_stream_fn(
    plugins: Arc<SessionPlugins>,
    stream: pa_agent::stream::StreamFn,
) -> pa_agent::stream::StreamFn {
    Arc::new(move |model, mut context, options| {
        let tail = plugins.prompt_tail();
        if !tail.is_empty() {
            context
                .system_prompt
                .get_or_insert_with(String::new)
                .push_str(&tail);
        }
        stream(model, context, options)
    })
}

impl AgentSession {
    pub(crate) fn parse_core_session_command(&self, text: &str) -> Option<SessionSlashCommand> {
        if let Some(command) = parse_session_command(&self.slash_commands, text) {
            return Some(command);
        }
        let (name, args) = super::slash_commands::parse_slash_command(text)?;
        let plugins = self.plugins.as_ref()?;
        let target = plugins
            .host
            .snapshot()
            .contributions
            .iter()
            .find(|target| target.kind == ContributionKind::Command && target.name == name)?
            .clone();
        Some(SessionSlashCommand {
            name,
            args,
            text: text.to_string(),
            plugin: Some(target),
        })
    }

    /// Active core command inventory. Plugin entries disappear immediately on disable.
    /// This does not claim daemon/TUI command discovery support.
    pub fn session_command_inventory(&self) -> Vec<SessionCommandInfo> {
        let mut inventory: Vec<_> = self
            .slash_commands
            .all()
            .iter()
            .map(|command| SessionCommandInfo {
                name: command.name.to_string(),
                description: command.description.to_string(),
                plugin: None,
            })
            .collect();
        if let Some(plugins) = &self.plugins {
            inventory.extend(
                plugins
                    .host
                    .snapshot()
                    .contributions
                    .iter()
                    .filter(|target| target.kind == ContributionKind::Command)
                    .map(|target| SessionCommandInfo {
                        name: target.name.clone(),
                        description: target.description.clone(),
                        plugin: Some(target.clone()),
                    }),
            );
        }
        inventory
    }
}

impl SessionEngine {
    /// Current assembled prompt, including the active inventory tail used by model
    /// calls. `system_prompt` remains the cache-stable base captured at assembly.
    pub fn assembled_system_prompt(&self) -> String {
        let mut prompt = self.system_prompt.clone();
        if let Some(plugins) = &self.session.plugins {
            prompt.push_str(&plugins.prompt_tail());
        }
        prompt
    }

    /// Exact plugin handlers installed in this session's kernel transport. Captures
    /// no engine/session lock, so a kernel request during an active tool cannot deadlock.
    pub fn plugin_host_handlers(&self) -> HostRequestHandlers {
        self.session
            .plugins
            .as_ref()
            .map_or_else(HostRequestHandlers::new, |plugins| plugins.handlers())
    }

    /// Reconcile explicit global/project plugin configuration without rebuilding the
    /// engine, rewriting history, or deleting saved data. Failure retains the old host.
    ///
    /// # Errors
    /// Returns settings, source, activation, or reconciliation errors without retiring
    /// the previous activation.
    pub async fn reload_plugins(&self) -> anyhow::Result<Arc<ContributionSnapshot>> {
        self.session
            .plugins
            .as_ref()
            .context("session plugin host is unavailable")?
            .reload()
            .await
    }

    /// Latest activation/reload failure. Ordinary agent startup remains available.
    pub fn plugin_diagnostic(&self) -> Option<String> {
        self.session.plugins.as_ref().and_then(|plugins| {
            plugins
                .diagnostic
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        })
    }

    /// Invoke the generation-bound target returned by core command admission.
    ///
    /// # Errors
    /// Returns an error for unsupported modes, stale/disabled targets, cancellation,
    /// or a Lua/capability failure.
    pub async fn invoke_plugin_command(
        &self,
        target: &Contribution,
        args: &str,
    ) -> anyhow::Result<Value> {
        if target.kind != ContributionKind::Command {
            bail!("not a plugin command");
        }
        if target.mode != InvocationMode::Unary {
            bail!("streaming plugin commands require a streaming client");
        }
        let plugins = self
            .session
            .plugins
            .as_ref()
            .context("session plugin host is unavailable")?;
        invoke(
            &plugins.host,
            target,
            json!({"args": args}),
            &CancellationToken::new(),
        )
        .await
    }
}

/// Install a real settled-turn observer. Its JSON outcome is diagnostic only; it
/// cannot rewrite messages or decide engine control flow. Failures do not fail turns.
pub(crate) async fn observe_turns(
    agent: &Arc<pa_agent::agent::Agent>,
    plugins: Arc<SessionPlugins>,
) {
    agent.subscribe(move |event, _signal| {
        let host = Arc::clone(&plugins.host);
        let session = plugins.session.clone();
        Box::pin(async move {
            if let pa_agent::types::AgentEvent::TurnEnd { message, tool_results } = event {
                let input = json!({"stage": "turn_settled", "session": session, "message": message, "tool_results": tool_results});
                for target in &host.snapshot().contributions {
                    if target.kind != ContributionKind::Hook || target.name != "turn_settled" { continue; }
                    // Observers cannot hold turn completion indefinitely.
                    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1),
                        invoke(&host, target, input.clone(), &CancellationToken::new())).await;
                    match outcome {
                        Ok(Ok(value)) => tracing::debug!(plugin = %target.plugin, outcome = %value, "plugin turn observer settled"),
                        Ok(Err(error)) => tracing::warn!(plugin = %target.plugin, %error, "plugin turn observer failed"),
                        Err(error) => tracing::warn!(plugin = %target.plugin, %error, "plugin turn observer timed out"),
                    }
                }
            }
            Ok(())
        })
    }).await;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod context_selection_tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        manager: Arc<SessionPlugins>,
        settings: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("project");
            let agent = dir.path().join("agent");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&agent).unwrap();
            let settings = agent.join("settings.json");
            let manager = Arc::new(SessionPlugins::new("context-test".into(), cwd, agent, 0));
            Self {
                _dir: dir,
                manager,
                settings,
            }
        }

        fn configure(&self, value: &Value) {
            std::fs::write(&self.settings, value.to_string()).unwrap();
        }
    }

    #[tokio::test]
    async fn capability_namespaces_route_independently_and_keep_request_identity() {
        struct Capture {
            name: &'static str,
            requests:
                tokio::sync::mpsc::UnboundedSender<(String, CapabilityRequest, CancellationToken)>,
        }
        impl HostCapabilities for Capture {
            fn call(
                &self,
                request: CapabilityRequest,
                cancellation: CancellationToken,
            ) -> CapabilityFuture {
                self.requests
                    .send((self.name.into(), request, cancellation))
                    .unwrap();
                let name = self.name;
                Box::pin(async move { Ok(json!({"delegate":name})) })
            }
        }
        let f = Fixture::new();
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        f.manager.set_native_capabilities(Arc::new(Capture {
            name: "native",
            requests: sent.clone(),
        }));
        f.manager.set_capability_delegate(
            "harness.",
            Arc::new(Capture {
                name: "harness",
                requests: sent.clone(),
            }),
        );
        f.manager.set_capability_delegate(
            "native.specific.",
            Arc::new(Capture {
                name: "specific",
                requests: sent.clone(),
            }),
        );
        let request = CapabilityRequest {
            session: "context-test".into(),
            plugin: "identity".into(),
            generation: uuid::Uuid::new_v4(),
            invocation: uuid::Uuid::new_v4(),
            operation: String::new(),
            input: json!({"retained":true}),
        };
        for (operation, expected) in [
            ("native.call", "native"),
            ("harness.read", "harness"),
            ("native.specific.call", "specific"),
        ] {
            let request = CapabilityRequest {
                operation: operation.into(),
                ..request.clone()
            };
            let cancel = CancellationToken::new();
            assert_eq!(
                f.manager
                    .capabilities
                    .call(request.clone(), cancel.clone())
                    .await
                    .unwrap(),
                json!({"delegate":expected})
            );
            let (name, actual, delegated_cancel) = received.recv().await.unwrap();
            assert_eq!((name, actual), (expected.to_string(), request));
            cancel.cancel();
            assert!(delegated_cancel.is_cancelled());
        }
        // Replacing the native namespace cannot replace the independent harness adapter.
        f.manager.set_native_capabilities(Arc::new(Capture {
            name: "replacement",
            requests: sent,
        }));
        for (operation, expected) in [("native.call", "replacement"), ("harness.read", "harness")] {
            let request = CapabilityRequest {
                operation: operation.into(),
                ..request.clone()
            };
            assert_eq!(
                f.manager
                    .capabilities
                    .call(request, CancellationToken::new())
                    .await
                    .unwrap(),
                json!({"delegate":expected})
            );
            received.recv().await.unwrap();
        }
        for operation in ["unknown.call", "nativeish.call"] {
            let request = CapabilityRequest {
                operation: operation.into(),
                ..request.clone()
            };
            assert!(f
                .manager
                .capabilities
                .call(request, CancellationToken::new())
                .await
                .is_err());
            assert!(received.try_recv().is_err());
        }
        let info = CapabilityRequest {
            operation: "session.info".into(),
            ..request
        };
        assert_eq!(
            f.manager
                .capabilities
                .call(info, CancellationToken::new())
                .await
                .unwrap(),
            f.manager.capabilities.info
        );
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn compiled_default_and_full_chain_are_real_lua_contributions() {
        let f = Fixture::new();
        f.manager.reload().await.unwrap();
        let strategy = f.manager.context_selection().unwrap();
        let cancel = CancellationToken::new();
        let messages = json!([{"role":"user","content":"exact ordered history"}]);
        assert_eq!(
            f.manager
                .invoke_context(
                    &strategy,
                    json!({"stage":"assemble","messages":messages}),
                    &cancel
                )
                .await
                .unwrap(),
            json!({"messages":messages})
        );
        assert_eq!(
            f.manager
                .invoke_context(&strategy, json!({"stage":"prepare"}), &cancel)
                .await
                .unwrap(),
            json!({"policy":"rolling","compactors":["server_compaction/server","snapcompact/snapcompact","standard_compaction/standard"]})
        );
        for (route, expected) in [
            (
                "standard_compaction/standard",
                json!({"status":"native","operation":"standard"}),
            ),
            (
                "server_compaction/server",
                json!({"status":"native","operation":"server","config":{}}),
            ),
            (
                "snapcompact/snapcompact",
                json!({"status":"native","operation":"snapcompact","config":{}}),
            ),
        ] {
            let compactor = f.manager.context_compactor(route).unwrap();
            assert_eq!(
                f.manager
                    .invoke_context(&compactor, json!({}), &cancel)
                    .await
                    .unwrap(),
                expected
            );
        }
        f.configure(&json!({"plugins":{"default_context":{"config":{"policy":"full","compactors":["server_compaction/server","snapcompact/snapcompact","standard_compaction/standard"]}}}}));
        f.manager.reload().await.unwrap();
        assert!(!f.manager.context_is_current(&strategy));
        let strategy = f.manager.context_selection().unwrap();
        assert_eq!(
            f.manager
                .invoke_context(&strategy, json!({"stage":"prepare"}), &cancel)
                .await
                .unwrap(),
            json!({"policy":"full","compactors":["server_compaction/server","snapcompact/snapcompact","standard_compaction/standard"]})
        );
    }

    #[tokio::test]
    async fn archive_restore_validates_schema_without_requiring_creator_generation() {
        let f = Fixture::new();
        let strategy = f.manager.context_selection().unwrap();
        let cancel = CancellationToken::new();
        for (archive, accepted) in [
            (
                json!({"version":1,"strategy":"default_context/rolling","creatorGeneration":"old-process"}),
                true,
            ),
            (
                json!({"version":2,"strategy":"default_context/rolling"}),
                false,
            ),
            (json!({"version":1,"strategy":"other/strategy"}), false),
            (Value::Null, false),
        ] {
            assert_eq!(
                f.manager
                    .invoke_context(
                        &strategy,
                        json!({"stage":"restore","archive":archive}),
                        &cancel
                    )
                    .await
                    .unwrap(),
                json!({"accepted":accepted})
            );
        }
        assert!(f.manager.invoke_context(&strategy,json!({"stage":"assemble","messages":[],"archive":{"version":2,"strategy":"default_context/rolling"}}),&cancel).await.is_err());
    }

    #[tokio::test]
    async fn invalid_enabled_replacement_preserves_host_but_explicit_disable_retires_it() {
        let f = Fixture::new();
        let old = f.manager.context_selection().unwrap();
        let snapshot = f.manager.host.snapshot();
        for settings in [
            json!({"contextStrategy":"missing/strategy"}),
            json!({"contextStrategy":null}),
            json!({"plugins":{"default_context":{"config":{"policy":"unknown"}}}}),
        ] {
            f.configure(&settings);
            assert!(f.manager.reload().await.is_err());
            assert_eq!(f.manager.context_selection().unwrap(), old);
            assert!(Arc::ptr_eq(&snapshot, &f.manager.host.snapshot()));
        }
        let compactor = f
            .manager
            .context_compactor("standard_compaction/standard")
            .unwrap();
        assert_eq!(
            f.manager
                .with_context_authority(&old, &compactor, || Ok("durable commit"))
                .unwrap(),
            "durable commit"
        );
        f.configure(&json!({"plugins":{"standard_compaction":{"enabled":false},"default_context":{"config":{"compactors":[]}}}}));
        f.manager.reload().await.unwrap();
        assert!(f
            .manager
            .with_context_authority(&old, &compactor, || Ok(()))
            .is_err());
        assert!(f.manager.context_retirement(&compactor).is_err());
        let selected = f.manager.context_selection().unwrap();
        f.configure(&json!({"plugins":{"default_context":{"enabled":false}}}));
        let snapshot = f.manager.reload().await.unwrap();
        assert!(!snapshot
            .contributions
            .iter()
            .any(|target| target.plugin == "default_context"));
        assert!(f
            .manager
            .context_selection()
            .unwrap_err()
            .to_string()
            .contains("re-enable"));
        assert!(!f.manager.context_is_current(&selected));
        assert!(f.manager.context_retirement(&selected).is_err());
    }

    #[tokio::test]
    async fn invalid_builtin_chain_preserves_host_revision_and_selection() {
        let f = Fixture::new();
        let original = f.manager.host.snapshot();
        let strategy = f.manager.context_selection().unwrap();
        for settings in [
            json!({"plugins":{"default_context":{"config":{"compactors":["missing/compactor"]}}}}),
            json!({"plugins":{"default_context":{"config":{"compactors":[7]}}}}),
        ] {
            f.configure(&settings);
            assert!(f.manager.reload().await.is_err());
            assert_eq!(f.manager.context_selection().unwrap(), strategy);
            assert!(Arc::ptr_eq(&original, &f.manager.host.snapshot()));
        }
        f.configure(&json!({"plugins":{"standard_compaction":{"enabled":false},"default_context":{"config":{"compactors":[]}}}}));
        f.manager.reload().await.unwrap();
        let strategy = f.manager.context_selection().unwrap();
        assert_eq!(
            f.manager
                .invoke_context(
                    &strategy,
                    json!({"stage":"prepare"}),
                    &CancellationToken::new()
                )
                .await
                .unwrap(),
            json!({"policy":"rolling","compactors":[]})
        );
    }

    #[tokio::test]
    async fn custom_strategy_and_compactors_use_same_typed_invocation_and_retirement() {
        let f = Fixture::new();
        let source = r"return {
            context_strategies={{name='custom',run=function(input)
                if input.stage=='assemble' then return {messages=input.messages} end
                return {compactors={'custom/decline','custom/failure','custom/fatal','custom/success'},policy='full'}
            end}},
            compactors={
                {name='decline',run=function() return {status='decline'} end},
                {name='failure',run=function() return {status='failure',message='try next'} end},
                {name='fatal',run=function() return {status='fatal',message='stop'} end},
                {name='success',run=function() return {status='success',summary='summary',first_kept_entry_id='kept'} end}
            }}";
        std::fs::write(f.manager.cwd.join("custom.lua"), source).unwrap();
        f.configure(&json!({"contextStrategy":"custom/custom","plugins":{"custom":{"enabled":true,"path":"custom.lua"},"default_context":{"enabled":false}}}));
        f.manager.reload().await.unwrap();
        let strategy = f.manager.context_selection().unwrap();
        assert_eq!(strategy.plugin, "custom");
        let cancel = CancellationToken::new();
        for (status, expected) in [
            ("decline", json!({"status":"decline"})),
            ("failure", json!({"status":"failure","message":"try next"})),
            ("fatal", json!({"status":"fatal","message":"stop"})),
            (
                "success",
                json!({"status":"success","summary":"summary","first_kept_entry_id":"kept"}),
            ),
        ] {
            let target = f
                .manager
                .context_compactor(&format!("custom/{status}"))
                .unwrap();
            assert_eq!(
                f.manager
                    .invoke_context(&target, json!({}), &cancel)
                    .await
                    .unwrap(),
                expected
            );
        }
        let retired = f.manager.context_retirement(&strategy).unwrap();
        f.configure(&json!({}));
        f.manager.reload().await.unwrap();
        assert!(retired.is_cancelled());
        assert!(!f.manager.context_is_current(&strategy));
        assert!(f
            .manager
            .invoke_context(&strategy, json!({}), &cancel)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn context_registration_rejects_away_then_back_even_same_generation() {
        let f = Fixture::new();
        std::fs::write(f.manager.cwd.join("away.lua"), "return {context_strategies={{name='other',run=function(input) return {messages=input.messages} end}}}").unwrap();
        f.configure(&json!({"plugins":{"away":{"enabled":true,"path":"away.lua"}}}));
        f.manager.reload().await.unwrap();
        let strategy = f.manager.context_selection().unwrap();
        let compactor = f
            .manager
            .context_compactor("standard_compaction/standard")
            .unwrap();
        let registration = f.manager.capture_context_registration(&strategy).unwrap();
        f.manager
            .with_registered_context_authority(&registration, &strategy, &compactor, || Ok(()))
            .unwrap();
        let away = f.manager.context_target("away/other").unwrap();
        f.manager
            .commit_context_selection(&away, || Ok(()))
            .unwrap();
        f.manager
            .commit_context_selection(&strategy, || Ok(()))
            .unwrap();
        assert_eq!(f.manager.context_selection().unwrap(), strategy);
        let mut published = false;
        assert!(f
            .manager
            .with_registered_context_authority(&registration, &strategy, &compactor, || {
                published = true;
                Ok(())
            })
            .is_err());
        assert!(!published);
    }

    #[tokio::test]
    async fn context_commit_admission_does_not_publish_after_compactor_retirement() {
        let f = Fixture::new();
        let strategy = f.manager.context_selection().unwrap();
        let compactor = f
            .manager
            .context_compactor("standard_compaction/standard")
            .unwrap();
        let registration = f.manager.capture_context_registration(&strategy).unwrap();
        let admitted = f
            .manager
            .with_registered_context_authority(&registration, &strategy, &compactor, || {
                Ok("one frozen write")
            })
            .unwrap();
        f.configure(&json!({"plugins":{"standard_compaction":{"enabled":false},"default_context":{"config":{"compactors":[]}}}}));
        f.manager.reload().await.unwrap();
        // Owner work already admitted must still finish; only publication is refused.
        assert_eq!(admitted, "one frozen write");
        let mut published = false;
        assert!(f
            .manager
            .with_registered_context_authority(&registration, &strategy, &compactor, || {
                published = true;
                Ok(())
            })
            .is_err());
        assert!(!published);
    }
    #[tokio::test]
    async fn concrete_context_admission_checks_actual_registration_exact_draft_and_once() {
        let f = Fixture::new();
        let strategy = f.manager.context_selection().unwrap();
        let compactor = f
            .manager
            .context_compactor("standard_compaction/standard")
            .unwrap();
        let registration = f.manager.capture_context_registration(&strategy).unwrap();
        let draft = crate::session::canonical::CanonicalEntryDraft::new(crate::session::canonical::CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(json!({"type":"compaction","summary":"actual policy","firstKeptEntryId":"kept","tokensBefore":1})).unwrap()).unwrap();
        let cap = super::super::canonical_live::CanonicalLiveContextAdmission::new(
            f.manager.clone(),
            registration,
            strategy.clone(),
            compactor.clone(),
            draft.clone(),
        );
        let changed = crate::session::canonical::CanonicalEntryDraft::new(crate::session::canonical::CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(json!({"type":"compaction","summary":"different policy","firstKeptEntryId":"kept","tokensBefore":1})).unwrap()).unwrap();
        let mut entered = 0;
        assert!(cap
            .with_admission(&changed, || {
                entered += 1;
                Ok(())
            })
            .is_err());
        cap.with_admission(&draft, || {
            entered += 1;
            Ok(())
        })
        .unwrap();
        assert!(cap
            .with_admission(&draft, || {
                entered += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(entered, 1);
        let registration = f.manager.capture_context_registration(&strategy).unwrap();
        let cap = super::super::canonical_live::CanonicalLiveContextAdmission::new(
            f.manager.clone(),
            registration,
            strategy,
            compactor,
            draft.clone(),
        );
        f.configure(&json!({"plugins":{"standard_compaction":{"enabled":false},"default_context":{"config":{"compactors":[]}}}}));
        f.manager.reload().await.unwrap();
        assert!(cap
            .with_admission(&draft, || {
                entered += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(entered, 1);
    }
}
