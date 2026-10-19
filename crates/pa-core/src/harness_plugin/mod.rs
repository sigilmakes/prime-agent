//! Optional first-party continual harness. Lua owns policy; this private native
//! module owns only lossless codecs, transactional storage and migration.
mod capabilities;
mod migration;
mod store;
#[cfg(test)]
mod tests;

pub(crate) const SOURCE: &str = include_str!("policy.lua");
pub(crate) fn source(settings: &crate::settings::Settings) -> crate::plugins::PluginSource {
    crate::plugins::PluginSource {
        name: "harness".into(),
        source: SOURCE.into(),
        config: serde_json::json!({"autoRefine":settings.auto_refine,
            "auxiliaryModel":settings.auxiliary_model}),
    }
}
pub(crate) use capabilities::HarnessStorage;
