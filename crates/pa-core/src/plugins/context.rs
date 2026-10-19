//! Explicit compiled first-party sources and generation-bound context route lookup.
//! Loading a known builtin never scans packages, directories, or ambient Lua paths.

use serde_json::json;

use super::{Contribution, ContributionKind, ContributionSnapshot, PluginError, PluginSource};

pub(crate) const DEFAULT_STRATEGY: &str = "default_context/rolling";
pub(crate) const DEFAULT_SOURCE: &str = include_str!("builtins/default-context.lua");

pub(crate) fn builtin_sources() -> Vec<PluginSource> {
    [
        ("default_context", DEFAULT_SOURCE),
        (
            "standard_compaction",
            include_str!("builtins/standard-compaction.lua"),
        ),
        (
            "server_compaction",
            include_str!("builtins/server-compaction.lua"),
        ),
        (
            "snapcompact",
            include_str!("builtins/snapcompact-compaction.lua"),
        ),
    ]
    .into_iter()
    .map(|(name, source)| PluginSource {
        name: name.into(),
        source: source.into(),
        config: json!({}),
    })
    .collect()
}

pub(crate) fn resolve(
    snapshot: &ContributionSnapshot,
    kind: ContributionKind,
    route: &str,
) -> Result<Contribution, PluginError> {
    let (plugin, name) = route.split_once('/').ok_or_else(|| {
        PluginError::Invalid(format!("context route must be plugin/name: {route}"))
    })?;
    snapshot
        .contributions
        .iter()
        .find(|target| target.kind == kind && target.plugin == plugin && target.name == name)
        .cloned()
        .ok_or_else(|| PluginError::Invalid(format!("context contribution is not active: {route}")))
}
