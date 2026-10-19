//! Deep-merge and migration.

use serde_json::Value;

use super::types::Settings;

/// Deep merge: overrides win; nested objects merge recursively; arrays and scalars replace.
pub fn deep_merge(base: &Settings, overrides: &Settings) -> Settings {
    let mut base_value = serde_json::to_value(base).unwrap_or_default();
    let overrides_value = serde_json::to_value(overrides).unwrap_or_default();
    merge_values(&mut base_value, &overrides_value);
    serde_json::from_value(base_value).unwrap_or_default()
}

fn merge_values(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base_obj), Value::Object(overrides_obj)) => {
            for (key, value) in overrides_obj {
                // Plugin JSON preserves explicit null (including malformed map/config
                // replacements), so reload can reject it without retiring active work.
                if key == "plugins" {
                    let existing = base_obj.entry(key.clone()).or_insert(Value::Null);
                    merge_plugin_values(existing, value);
                    continue;
                }
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

fn merge_plugin_values(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base), Value::Object(overrides)) => {
            for (key, value) in overrides {
                merge_plugin_values(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, value) => *base = value.clone(),
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
    use serde_json::json;

    #[test]
    fn plugin_merge_distinguishes_absence_from_explicit_null() {
        let base: Settings = serde_json::from_value(json!({
            "plugins":{"alpha":{"enabled":true,"path":"alpha.lua","config":{"keep":true,"nullable":1}}}
        })).unwrap();
        let empty = deep_merge(&base, &Settings::default());
        assert_eq!(empty.plugins, base.plugins);
        let overrides: Settings = serde_json::from_value(json!({
            "plugins":{"alpha":{"config":{"nullable":null}}}
        }))
        .unwrap();
        assert_eq!(
            deep_merge(&base, &overrides).plugins,
            Some(json!({
                "alpha":{"enabled":true,"path":"alpha.lua","config":{"keep":true,"nullable":null}}
            }))
        );
        let malformed: Settings = serde_json::from_value(json!({"plugins":null})).unwrap();
        assert_eq!(deep_merge(&base, &malformed).plugins, Some(Value::Null));
        assert_eq!(
            deep_merge(&Settings::default(), &Settings::default()).plugins,
            None
        );
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
        assert_eq!(map.get("skills"), Some(&json!(["/x"])));
        assert_eq!(
            map.get("retry"),
            Some(&json!({ "provider": { "maxRetryDelayMs": 5 } }))
        );
    }
}
