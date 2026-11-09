//! Deep-merge and migration.

use std::collections::BTreeMap;

use serde_json::Value;

use super::types::{PluginEntry, Settings};

/// Merge project scope onto the global scope. Plugin entries stay global-only:
/// a cloned repository's `.prime/agent/settings.json` must not execute Lua.
#[must_use]
pub fn merge_scopes(global: &Settings, project: &Settings) -> Settings {
    let mut merged = deep_merge(global, project);
    merged.plugins.clone_from(&global.plugins);
    // A malformed plugin map is retained in `extra` for a loud reload failure.
    // Project scope must not be able to inject one either.
    let malformed_plugins = global.extra.get("plugins").cloned();
    merged.extra.remove("plugins");
    if let Some(raw) = malformed_plugins {
        merged.extra.insert("plugins".into(), raw);
    }
    merged
}

/// Deep merge: overrides win; nested objects merge recursively; arrays and scalars replace.
pub fn deep_merge(base: &Settings, overrides: &Settings) -> Settings {
    let mut base_value = serde_json::to_value(base).unwrap_or_default();
    let overrides_value = serde_json::to_value(overrides).unwrap_or_default();
    merge_values(&mut base_value, &overrides_value);
    // A malformed plugin map lives in `extra` and cannot pass the typed field.
    // Keep it out of the typed round-trip, then restore it as raw data.
    let malformed_plugins = base_value
        .get("plugins")
        .filter(|value| {
            !value.is_null()
                && serde_json::from_value::<BTreeMap<String, PluginEntry>>((*value).clone())
                    .is_err()
        })
        .cloned();
    if malformed_plugins.is_some() {
        base_value.as_object_mut().unwrap().remove("plugins");
    }
    let mut merged: Settings = serde_json::from_value(base_value).unwrap_or_default();
    if let Some(raw) = malformed_plugins {
        merged.extra.insert("plugins".into(), raw);
    }
    merged
}

fn merge_values(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base_obj), Value::Object(overrides_obj)) => {
            for (key, value) in overrides_obj {
                // `null` means the key is unset (TS documents omit undefined
                // keys); it must never override a real value.
                if value.is_null() {
                    continue;
                }
                match base_obj.get_mut(key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        merge_values(existing, value);
                    }
                    _ => {
                        base_obj.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base_slot, value) => {
            if !value.is_null() {
                *base_slot = value.clone();
            }
        }
    }
}

/// Migrate legacy settings shapes (queueMode, websockets, skills object,
/// retry.maxDelayMs, bad markdown).
pub fn migrate(document: &mut serde_json::Map<String, Value>) {
    if let Some(queue_mode) = document.remove("queueMode") {
        document.entry("steeringMode").or_insert(queue_mode);
    }
    if !document.contains_key("transport") {
        if let Some(websockets) = document.remove("websockets") {
            if websockets.is_boolean() {
                let transport = if websockets == Value::Bool(true) {
                    "websocket"
                } else {
                    "sse"
                };
                document.insert("transport".into(), Value::String(transport.into()));
            }
        }
    }
    if let Some(skills) = document.remove("skills") {
        if let Value::Object(mut skills_obj) = skills {
            if let Some(flag) = skills_obj.remove("enableSkillCommands") {
                if document.get("enableSkillCommands").is_none() {
                    document.insert("enableSkillCommands".into(), flag);
                }
            }
            match skills_obj.remove("customDirectories") {
                Some(Value::Array(dirs)) if !dirs.is_empty() => {
                    document.insert("skills".into(), Value::Array(dirs));
                }
                _ => {}
            }
        } else {
            document.insert("skills".into(), skills);
        }
    }
    if let Some(Value::Object(retry)) = document.get_mut("retry") {
        let max_delay = retry.remove("maxDelayMs");
        if let Some(Value::Number(delay)) = max_delay {
            let provider = retry
                .entry("provider")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(provider_obj) = provider.as_object_mut() {
                provider_obj
                    .entry("maxRetryDelayMs")
                    .or_insert(Value::Number(delay));
            }
        }
    }
    if let Some(markdown) = document.get("markdown") {
        if !markdown.is_object() {
            document.remove("markdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::TransportSetting;
    use serde_json::json;

    #[test]
    fn project_plugins_are_dropped_but_other_project_fields_merge() {
        let global: Settings = serde_json::from_value(json!({
            "theme": "prime",
            "plugins": {
                "alpha": {
                    "enabled": true,
                    "path": "alpha.lua",
                    "config": {"keep": true}
                }
            }
        }))
        .unwrap();
        let project: Settings = serde_json::from_value(json!({
            "theme": "dark",
            "plugins": {
                "alpha": {"enabled": true, "path": "evil.lua"},
                "beta": {"enabled": true, "path": "beta.lua"}
            }
        }))
        .unwrap();
        let merged = merge_scopes(&global, &project);
        assert_eq!(merged.theme.as_deref(), Some("dark"));
        let plugins = merged.plugins.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins["alpha"], global.plugins.unwrap()["alpha"]);
        assert!(!plugins.contains_key("beta"));
    }

    #[test]
    fn malformed_plugin_raw_survives_merge_without_losing_other_fields() {
        let mut global = Settings {
            theme: Some("prime".into()),
            ..Settings::default()
        };
        global.extra.insert(
            "plugins".into(),
            json!({"alpha": {"enabled": true, "unknown": 1}}),
        );
        let merged = deep_merge(&global, &Settings::default());
        assert_eq!(merged.theme.as_deref(), Some("prime"));
        assert_eq!(merged.extra.get("plugins"), global.extra.get("plugins"));
        assert_eq!(merged.plugins, None);
    }

    #[test]
    fn project_scope_cannot_inject_a_malformed_plugin_map() {
        let global: Settings = serde_json::from_value(json!({
            "plugins": {"alpha": {"enabled": true, "path": "alpha.lua"}}
        }))
        .unwrap();
        let mut project: Settings = Settings::default();
        project.extra.insert(
            "plugins".into(),
            json!({"evil": {"enabled": true, "path": "evil.lua"}}),
        );
        let merged = merge_scopes(&global, &project);
        assert_eq!(merged.plugins, global.plugins);
        assert!(merged.extra.get("plugins").is_none());
    }

    #[test]
    fn settings_without_plugins_merge_to_no_entries() {
        assert_eq!(
            deep_merge(&Settings::default(), &Settings::default()).plugins,
            None
        );
        let global: Settings = serde_json::from_value(json!({
            "plugins": {"alpha": {"enabled": true, "path": "alpha.lua"}}
        }))
        .unwrap();
        assert_eq!(
            merge_scopes(&global, &Settings::default()).plugins,
            global.plugins
        );
        assert_eq!(merge_scopes(&Settings::default(), &global).plugins, None);
    }

    #[test]
    fn merge_prefers_overrides_and_merges_nested() {
        let base: Settings = serde_json::from_value(json!({
            "theme": "prime", "compaction": { "enabled": true, "reserveTokens": 1024 }
        }))
        .unwrap();
        let overrides: Settings = serde_json::from_value(json!({
            "compaction": { "reserveTokens": 2048 }
        }))
        .unwrap();
        let merged = deep_merge(&base, &overrides);
        assert_eq!(merged.theme.as_deref(), Some("prime"));
        assert_eq!(merged.compaction.as_ref().unwrap().enabled, Some(true));
        assert_eq!(
            merged.compaction.as_ref().unwrap().reserve_tokens,
            Some(2048)
        );
    }

    #[test]
    fn migration_rewrites_legacy_shapes() {
        let mut doc = json!({
            "queueMode": "all",
            "websockets": true,
            "skills": { "enableSkillCommands": false, "customDirectories": ["/x"] },
            "retry": { "maxDelayMs": 5 }
        });
        let mut map = doc.as_object_mut().unwrap().clone();
        migrate(&mut map);
        assert_eq!(map.get("steeringMode"), Some(&json!("all")));
        assert_eq!(map.get("transport"), Some(&json!("websocket")));
        let migrated: Settings = serde_json::from_value(Value::Object(map.clone())).unwrap();
        assert_eq!(migrated.transport, Some(TransportSetting::WebSocket));
        assert_eq!(map.get("skills"), Some(&json!(["/x"])));
        assert_eq!(
            map.get("retry"),
            Some(&json!({ "provider": { "maxRetryDelayMs": 5 } }))
        );
    }
}
