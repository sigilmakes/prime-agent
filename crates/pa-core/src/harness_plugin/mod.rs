//! Optional first-party continual harness. Lua owns policy; this private native
//! module owns only lossless codecs, transactional storage and migration.
mod capabilities;
mod migration;
mod store;
#[cfg(test)]
mod tests;

pub(crate) const SOURCE: &str = include_str!("policy.lua");
/// Scopes every session loads unless the session enrolls its own. The session
/// scope is always added by policy, so this never needs to name one.
const DEFAULT_SCOPES: [&str; 1] = ["global"];
pub(crate) fn source(settings: &crate::settings::Settings) -> crate::plugins::PluginSource {
    crate::plugins::PluginSource {
        name: "harness".into(),
        source: SOURCE.into(),
        config: serde_json::json!({"auxiliaryModel":settings.auxiliary_model,
            "scopes":default_scopes(settings)}),
    }
}
/// Resolve `harnessScopes`: unset means [`DEFAULT_SCOPES`]; an explicit list is
/// trimmed, deduplicated, and may be empty to keep only the session scope.
pub(crate) fn default_scopes(settings: &crate::settings::Settings) -> Vec<String> {
    let Some(configured) = settings.harness_scopes.as_deref() else {
        return DEFAULT_SCOPES.map(String::from).to_vec();
    };
    let mut scopes: Vec<String> = Vec::new();
    for scope in configured {
        let scope = scope.trim();
        if !scope.is_empty() && !scopes.iter().any(|existing| existing == scope) {
            scopes.push(scope.to_string());
        }
    }
    scopes
}
pub(crate) use capabilities::HarnessStorage;
