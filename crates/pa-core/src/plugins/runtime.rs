use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use mlua::{Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, VmState};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    CapabilityRequest, ContextDataMode, Contribution, ContributionKind, HostCapabilities,
    InvocationEvent, InvocationMode, InvocationOutcome, PluginError, PluginSource,
};

struct Registered {
    contribution: Contribution,
    callback: Function,
}

pub(super) struct Generation {
    pub source: String,
    pub config: Value,
    pub retirement: CancellationToken,
    lua: Lua,
    callbacks: BTreeMap<(ContributionKind, String), Registered>,
}

// Result::map_err needs an owned-error adapter; no Lua error escapes the module.
#[allow(clippy::needless_pass_by_value)]
fn lua_error(error: mlua::Error) -> PluginError {
    PluginError::Lua(error.to_string())
}

// Unknown fields must not silently imply an unimplemented contribution form.
fn validate_fields(table: &Table, allowed: &[&str]) -> Result<(), PluginError> {
    for key in table.clone().pairs::<String, mlua::Value>() {
        let (key, _) = key.map_err(lua_error)?;
        if !allowed.contains(&key.as_str()) {
            return Err(PluginError::Invalid(format!(
                "unknown declaration field {key}"
            )));
        }
    }
    Ok(())
}

impl Generation {
    pub fn load(source: &PluginSource) -> Result<Self, PluginError> {
        if source.source.len() > 1024 * 1024 {
            return Err(PluginError::Invalid("source exceeds 1 MiB".into()));
        }
        // Trusted local plugins, not a hostile-code security sandbox. All external
        // operations still require an explicit host capability; no ambient IO/package libs.
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )
        .map_err(lua_error)?;
        lua.set_memory_limit(16 * 1024 * 1024).map_err(lua_error)?;
        for name in ["dofile", "loadfile", "load", "print", "warn"] {
            lua.globals()
                .set(name, mlua::Value::Nil)
                .map_err(lua_error)?;
        }
        lua.globals().set("null", lua.null()).map_err(lua_error)?;
        // Serialize into this VM before activation. Lua mutations cannot change the
        // desired JSON retained for reconciliation or any other generation's config.
        let config = lua.to_value(&source.config).map_err(lua_error)?;
        lua.globals().set("config", config).map_err(lua_error)?;
        // Initialization is declarative and cannot await. Bound accidental busy loops.
        let instructions = AtomicUsize::new(0);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| {
                if instructions.fetch_add(1000, Ordering::Relaxed) >= 1_000_000 {
                    return Err(mlua::Error::RuntimeError(
                        "initialization instruction budget exceeded".into(),
                    ));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(lua_error)?;
        let declaration: Table = lua
            .load(&source.source)
            .set_name(&source.name)
            .eval()
            .map_err(lua_error)?;
        validate_fields(
            &declaration,
            &[
                "exports",
                "commands",
                "hooks",
                "context_strategies",
                "compactors",
                "providers",
            ],
        )?;
        let id = Uuid::new_v4();
        let mut callbacks = BTreeMap::new();
        for (field, kind) in [
            ("exports", ContributionKind::Export),
            ("commands", ContributionKind::Command),
            ("hooks", ContributionKind::Hook),
            ("context_strategies", ContributionKind::ContextStrategy),
            ("compactors", ContributionKind::Compactor),
            ("providers", ContributionKind::Provider),
        ] {
            let Some(entries) = declaration.get::<Option<Table>>(field).map_err(lua_error)? else {
                continue;
            };
            let mut indices = BTreeSet::new();
            for entry in entries.pairs::<usize, Table>() {
                let (index, entry) = entry.map_err(lua_error)?;
                if index == 0 || index > 128 {
                    return Err(PluginError::Invalid(
                        "declarations must be dense arrays of at most 128 callbacks".into(),
                    ));
                }
                indices.insert(index);
                validate_fields(
                    &entry,
                    &["name", "description", "mode", "context_data", "run"],
                )?;
                let context_data = entry
                    .get::<Option<String>>("context_data")
                    .map_err(lua_error)?;
                if context_data.is_some()
                    && !matches!(
                        kind,
                        ContributionKind::ContextStrategy | ContributionKind::Compactor
                    )
                {
                    return Err(PluginError::Invalid(format!(
                        "{field} callbacks cannot declare context_data"
                    )));
                }
                let context_data = match context_data.as_deref() {
                    None | Some("json") => ContextDataMode::Json,
                    Some("refs_v1") => ContextDataMode::RefsV1,
                    Some(other) => {
                        return Err(PluginError::Invalid(format!(
                            "unknown context data mode {other}"
                        )))
                    }
                };
                let name: String = entry.get("name").map_err(lua_error)?;
                super::names::validate_contribution_name(kind, &name)?;
                let mode = match entry
                    .get::<Option<String>>("mode")
                    .map_err(lua_error)?
                    .as_deref()
                {
                    None if kind == ContributionKind::Provider => InvocationMode::Streaming,
                    None | Some("unary") => InvocationMode::Unary,
                    Some("streaming") => InvocationMode::Streaming,
                    Some(other) => {
                        return Err(PluginError::Invalid(format!(
                            "unknown invocation mode {other}"
                        )))
                    }
                };
                if matches!(
                    kind,
                    ContributionKind::ContextStrategy | ContributionKind::Compactor
                ) && mode != InvocationMode::Unary
                {
                    return Err(PluginError::Invalid(format!(
                        "{field} callbacks must be unary"
                    )));
                }
                let registered = Registered {
                    contribution: Contribution {
                        plugin: source.name.clone(),
                        generation: id,
                        kind,
                        name: name.clone(),
                        description: entry
                            .get::<Option<String>>("description")
                            .map_err(lua_error)?
                            .unwrap_or_default(),
                        mode,
                        context_data,
                    },
                    callback: entry.get("run").map_err(lua_error)?,
                };
                if callbacks.insert((kind, name.clone()), registered).is_some() {
                    return Err(PluginError::Invalid(format!(
                        "duplicate {field} callback {name}"
                    )));
                }
            }
            if indices.iter().copied().ne(1..=indices.len()) {
                return Err(PluginError::Invalid(
                    "declarations must be dense arrays".into(),
                ));
            }
        }
        lua.remove_hook();
        // Every running coroutine cooperatively yields, even pure Lua busy loops.
        // mlua releases its internal VM lock on Pending; no registry/session lock
        // crosses an async capability wait. The caller/retirement select then wins.
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(10_000), |_, _| {
            Ok(VmState::Yield)
        })
        .map_err(lua_error)?;
        Ok(Self {
            source: source.source.clone(),
            config: source.config.clone(),
            retirement: CancellationToken::new(),
            lua,
            callbacks,
        })
    }

    pub fn contributions(&self) -> impl Iterator<Item = &Contribution> {
        self.callbacks
            .values()
            .map(|registered| &registered.contribution)
    }
}

#[derive(Clone)]
struct CallScope {
    session: String,
    invocation: Uuid,
    contribution: Contribution,
    capabilities: Arc<dyn HostCapabilities>,
    cancellation: CancellationToken,
    retirement: CancellationToken,
    closed: CancellationToken,
}

impl CallScope {
    fn context(&self, lua: &Lua, sender: mpsc::Sender<InvocationEvent>) -> mlua::Result<Table> {
        let context = lua.create_table()?;
        let call_scope = self.clone();
        context.set("call", lua.create_async_function(move |lua, (operation, input): (String, mlua::Value)| {
            let scope = call_scope.clone();
            async move {
                let request = CapabilityRequest {
                    session: scope.session.clone(), plugin: scope.contribution.plugin.clone(),
                    generation: scope.contribution.generation, invocation: scope.invocation, operation,
                    input: lua.from_value(input)?,
                };
                let output = tokio::select! {
                    biased;
                    () = scope.retirement.cancelled() => return Err(mlua::Error::RuntimeError("retired".into())),
                    () = scope.cancellation.cancelled() => return Err(mlua::Error::RuntimeError("cancelled".into())),
                    () = scope.closed.cancelled() => return Err(mlua::Error::RuntimeError("invocation closed".into())),
                    // Construct the adapter future only after the cancellation fences
                    // have been polled. select! eagerly evaluates branch expressions.
                    output = async { scope.capabilities.call(request, scope.cancellation.child_token()).await } => output.map_err(mlua::Error::RuntimeError)?,
                };
                lua.to_value(&output)
            }
        })?)?;
        let emit_scope = self.clone();
        context.set("emit", lua.create_async_function(move |lua, value: mlua::Value| {
            let scope = emit_scope.clone();
            let sender = sender.clone();
            async move {
                if scope.contribution.mode != InvocationMode::Streaming {
                    return Err(mlua::Error::RuntimeError("unary callbacks cannot emit".into()));
                }
                let value = lua.from_value(value)?;
                tokio::select! {
                    biased;
                    () = scope.retirement.cancelled() => Err(mlua::Error::RuntimeError("retired".into())),
                    () = scope.cancellation.cancelled() => Err(mlua::Error::RuntimeError("cancelled".into())),
                    () = scope.closed.cancelled() => Err(mlua::Error::RuntimeError("invocation closed".into())),
                    result = sender.send(InvocationEvent::Update(value)) => result.map_err(mlua::Error::external),
                }
            }
        })?)?;
        Ok(context)
    }
}

#[tracing::instrument(skip_all, fields(plugin = %contribution.plugin, generation = %contribution.generation))]
pub(super) async fn run(
    generation: Arc<Generation>,
    contribution: Contribution,
    input: Value,
    session: String,
    capabilities: Arc<dyn HostCapabilities>,
    cancellation: CancellationToken,
    sender: mpsc::Sender<InvocationEvent>,
) {
    let closed = CancellationToken::new();
    let _close_on_exit = closed.clone().drop_guard();
    // Retained Lua contexts must not keep working after their originating invocation.
    // Cancel this adapter token on both success and failure, independently of delivery.
    let work = cancellation.child_token();
    let _cancel_work_on_exit = work.clone().drop_guard();
    let scope = CallScope {
        session,
        invocation: Uuid::new_v4(),
        contribution: contribution.clone(),
        capabilities,
        cancellation: work,
        retirement: generation.retirement.clone(),
        closed,
    };
    let execution = async {
        let context = scope
            .context(&generation.lua, sender.clone())
            .map_err(lua_error)?;
        let input = generation.lua.to_value(&input).map_err(lua_error)?;
        let registered = generation
            .callbacks
            .get(&(contribution.kind, contribution.name.clone()))
            .ok_or(PluginError::Missing)?;
        // call_async owns a recyclable coroutine. Its drop resets the thread and
        // drops pending Rust capability futures on cancellation, not just their result.
        let output: mlua::Value = registered
            .callback
            .call_async((input, context))
            .await
            .map_err(lua_error)?;
        let output = generation.lua.from_value(output).map_err(lua_error)?;
        Ok(match contribution.mode {
            InvocationMode::Unary => InvocationOutcome::Unary(output),
            InvocationMode::Streaming => InvocationOutcome::Streamed(output),
        })
    };
    let result = tokio::select! {
        biased;
        () = generation.retirement.cancelled() => Err(PluginError::Retired),
        () = cancellation.cancelled() => Err(PluginError::Cancelled),
        result = execution => result,
    };
    scope.closed.cancel();
    scope.cancellation.cancel();
    tokio::select! {
        biased;
        () = generation.retirement.cancelled() => {},
        () = cancellation.cancelled() => {},
        result = sender.send(InvocationEvent::Finished(result)) => {
            if result.is_err() {
                tracing::debug!("plugin invocation consumer dropped");
            }
        }
    }
}
