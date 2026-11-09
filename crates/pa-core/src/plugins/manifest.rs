//! Session-free provider facts declared by global plugin chunks.
//!
//! Facts are read-only data available before any session exists. This pass
//! evaluates an explicit chunk in a fresh VM with no host capabilities, extracts
//! the returned `providers` table, and rejects anything that is not pure data.
//! Functions, commands, hooks, and other callable contributions remain the
//! session host's concern.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

use mlua::{Table, Value as LuaValue};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::settings::{PluginEntry, Settings};

use super::{
    runtime::{self, EvaluatedChunk},
    ContributionKind, PluginError, PluginSource,
};

/// Nesting bound for one provider fact tree, counting the provider entry as 0.
pub const MAX_PROVIDER_FACT_DEPTH: usize = 32;
/// Byte bound for all provider facts declared by one plugin.
pub const MAX_PROVIDER_FACT_BYTES: usize = 1024 * 1024;

/// One provider declaration. `name`, `url`, and `protocol` are the fields the
/// fact layer interprets; `auth`, `headers`, `defaults`, `limits`, and unknown
/// fields stay owned pure JSON for the later provider adapter.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderFact {
    pub plugin: String,
    pub name: String,
    /// Resolved chunk path, used by registry publication guards. It is not fact
    /// content, so it never participates in the revision digest.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One immutable fact snapshot. `revision` is a content digest, so equal facts
/// always carry the same revision and any settings or source change that alters
/// facts changes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestCatalog {
    pub revision: u64,
    pub providers: Vec<ProviderFact>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    path: PathBuf,
    plugin: String,
}

struct CachedFacts {
    source: [u8; 32],
    config: [u8; 32],
    facts: Vec<ProviderFact>,
}

static FACT_CACHE: OnceLock<Mutex<HashMap<CacheKey, CachedFacts>>> = OnceLock::new();

fn fact_cache() -> &'static Mutex<HashMap<CacheKey, CachedFacts>> {
    FACT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl ManifestCatalog {
    /// Load every explicitly enabled, file-backed global plugin and collect its
    /// provider facts. There is no session id and no host capability. A chunk is
    /// evaluated only when its file bytes or config differ from the cached pass;
    /// disabling or removing an entry removes its facts from the catalog.
    ///
    /// # Errors
    /// Returns an error when an enabled declaration is invalid, its source cannot
    /// be read, its chunk fails, or a provider fact is not bounded pure data.
    pub fn load(
        agent_dir: &Path,
        plugins: Option<&BTreeMap<String, PluginEntry>>,
    ) -> Result<Self, PluginError> {
        let mut providers = Vec::new();
        if let Some(plugins) = plugins {
            for (name, entry) in plugins {
                if entry.enabled != Some(true) {
                    continue;
                }
                super::validate_plugin_name(name)?;
                // Embedded/optional builtin chunks are session composition, not
                // file facts; they currently declare no providers.
                let Some(path) = entry.path.as_deref() else {
                    continue;
                };
                if !entry.config.is_object() {
                    return Err(PluginError::Invalid(format!(
                        "plugin {name}: config must be an object"
                    )));
                }
                let (resolved, source_text) = super::source::read(name, path, agent_dir)?;
                let source_digest = digest(source_text.as_bytes());
                let config_bytes = serde_json::to_vec(&entry.config).unwrap_or_default();
                let config_digest = digest(&config_bytes);
                let key = CacheKey {
                    path: resolved.clone(),
                    plugin: name.clone(),
                };
                if let Some(cached) = fact_cache()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&key)
                    .filter(|cached| {
                        cached.source == source_digest && cached.config == config_digest
                    })
                {
                    providers.extend(cached.facts.iter().cloned());
                    continue;
                }
                let facts = extract(name, &source_text, &entry.config, Some(&resolved))?;
                fact_cache()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        key,
                        CachedFacts {
                            source: source_digest,
                            config: config_digest,
                            facts: facts.clone(),
                        },
                    );
                providers.extend(facts);
            }
        }
        Ok(Self {
            revision: revision(&providers),
            providers,
        })
    }

    /// Load from a settings document, rejecting a malformed global plugin map
    /// instead of silently returning an empty catalog.
    ///
    /// # Errors
    /// Returns an error for the same conditions as [`Self::load`], and when the
    /// settings document retained a plugin map that did not match the typed shape.
    pub fn from_settings(agent_dir: &Path, settings: &Settings) -> Result<Self, PluginError> {
        if settings.extra.contains_key("plugins") {
            return Err(PluginError::Invalid(
                "plugins must be an object of {enabled?, path?, config?} entries".into(),
            ));
        }
        Self::load(agent_dir, settings.plugins.as_ref())
    }

    /// Load the global plugin facts for an agent directory. This is the single
    /// settings-reading path for session-free consumers: the manager opens the
    /// agent-scoped document, and the merged settings drop project-scope plugin
    /// entries before the fact pass runs.
    ///
    /// # Errors
    /// Returns an error for the same conditions as [`Self::from_settings`].
    pub fn from_agent_dir(agent_dir: &Path) -> Result<Self, PluginError> {
        let manager = crate::settings::SettingsManager::create(agent_dir, agent_dir);
        Self::from_settings(agent_dir, manager.settings())
    }
}

fn revision(providers: &[ProviderFact]) -> u64 {
    let bytes = serde_json::to_vec(providers).unwrap_or_default();
    let digest = digest(&bytes);
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("sha256 prefix is eight bytes"),
    )
}

fn extract(
    name: &str,
    source_text: &str,
    config: &Value,
    source_path: Option<&Path>,
) -> Result<Vec<ProviderFact>, PluginError> {
    let source = PluginSource {
        name: name.into(),
        source: source_text.into(),
        config: config.clone(),
    };
    let EvaluatedChunk { lua, declaration } = runtime::evaluate(&source)?;
    let result = (|| {
        let Some(providers) = declaration
            .get::<Option<Table>>("providers")
            .map_err(|error| lua_fact_error(name, &error))?
        else {
            return Ok(Vec::new());
        };
        let entries = runtime::dense_array(&providers, "providers")
            .map_err(|error| fact_error(name, "providers", &error))?;
        let mut budget = Budget::default();
        let mut facts = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let path = format!("providers[{}]", index + 1);
            let value = data_value(LuaValue::Table(entry), &path, name, 0, &mut budget)?;
            let Value::Object(fields) = value else {
                unreachable!("a table converts to an object");
            };
            facts.push(ProviderFact::from_fields(name, &path, fields, source_path)?);
        }
        Ok(facts)
    })();
    lua.remove_hook();
    result
}

fn lua_fact_error(plugin: &str, error: &mlua::Error) -> PluginError {
    PluginError::Invalid(format!("plugin {plugin}: {error}"))
}

fn fact_error(plugin: &str, path: &str, error: &PluginError) -> PluginError {
    PluginError::Invalid(format!("plugin {plugin}: {path}: {error}"))
}

struct Budget {
    remaining: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            remaining: MAX_PROVIDER_FACT_BYTES,
        }
    }
}

impl Budget {
    fn charge(&mut self, plugin: &str, path: &str, bytes: usize) -> Result<(), PluginError> {
        if bytes > self.remaining {
            return Err(PluginError::Invalid(format!(
                "plugin {plugin}: {path} exceeds the {MAX_PROVIDER_FACT_BYTES}-byte provider fact budget"
            )));
        }
        self.remaining -= bytes;
        Ok(())
    }
}

fn child_path(path: &str, key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        format!("{path}.{key}")
    } else {
        format!("{path}[{key:?}]")
    }
}

fn data_value(
    value: LuaValue,
    path: &str,
    plugin: &str,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value, PluginError> {
    if value.is_null() {
        budget.charge(plugin, path, 1)?;
        return Ok(Value::Null);
    }
    match value {
        LuaValue::Nil => {
            budget.charge(plugin, path, 1)?;
            Ok(Value::Null)
        }
        LuaValue::Boolean(value) => {
            budget.charge(plugin, path, 1)?;
            Ok(Value::Bool(value))
        }
        LuaValue::Integer(value) => {
            budget.charge(plugin, path, 8)?;
            Ok(Value::Number(Number::from(value)))
        }
        LuaValue::Number(value) => {
            budget.charge(plugin, path, 8)?;
            let number = Number::from_f64(value).ok_or_else(|| {
                PluginError::Invalid(format!("plugin {plugin}: {path} must be a finite number"))
            })?;
            Ok(Value::Number(number))
        }
        LuaValue::String(value) => {
            let text = value.to_str().map_err(|_| {
                PluginError::Invalid(format!("plugin {plugin}: {path} must be a UTF-8 string"))
            })?;
            budget.charge(plugin, path, text.len().saturating_add(1))?;
            Ok(Value::String(text.to_owned()))
        }
        LuaValue::Table(table) => {
            if depth >= MAX_PROVIDER_FACT_DEPTH {
                return Err(PluginError::Invalid(format!(
                    "plugin {plugin}: {path} exceeds the maximum provider fact depth of {MAX_PROVIDER_FACT_DEPTH}"
                )));
            }
            table_value(&table, path, plugin, depth + 1, budget)
        }
        LuaValue::Function(_) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path} must be pure data, found a function"
        ))),
        LuaValue::Thread(_) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path} must be pure data, found a thread"
        ))),
        LuaValue::UserData(_) | LuaValue::LightUserData(_) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path} must be pure data, found userdata"
        ))),
        LuaValue::Error(error) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path} must be pure data, found an error value: {error}"
        ))),
        LuaValue::Other(_) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path} must be pure data"
        ))),
    }
}

fn table_value(
    table: &Table,
    path: &str,
    plugin: &str,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value, PluginError> {
    let mut entries = Vec::new();
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair.map_err(|error| lua_fact_error(plugin, &error))?;
        entries.push((key, value));
    }
    if entries.is_empty() {
        budget.charge(plugin, path, 2)?;
        return Ok(Value::Object(Map::new()));
    }
    let all_integers = entries
        .iter()
        .all(|(key, _)| matches!(key, LuaValue::Integer(_)));
    let all_strings = entries
        .iter()
        .all(|(key, _)| matches!(key, LuaValue::String(_)));
    if all_integers {
        let mut indexed = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let LuaValue::Integer(index) = key else {
                unreachable!("keys were checked as integers");
            };
            indexed.push((index, value));
        }
        indexed.sort_by_key(|(index, _)| *index);
        if indexed
            .iter()
            .map(|(index, _)| *index)
            .ne(1..=i64::try_from(indexed.len()).expect("bounded array length"))
        {
            return Err(PluginError::Invalid(format!(
                "plugin {plugin}: {path} must use string keys or dense integer indices starting at 1"
            )));
        }
        let mut values = Vec::with_capacity(indexed.len());
        for (index, value) in indexed {
            let child = format!("{path}[{index}]");
            budget.charge(plugin, &child, 1)?;
            values.push(data_value(value, &child, plugin, depth, budget)?);
        }
        return Ok(Value::Array(values));
    }
    if all_strings {
        let mut fields = Map::new();
        for (key, value) in entries {
            let LuaValue::String(key) = key else {
                unreachable!("keys were checked as strings");
            };
            let key = key.to_str().map_err(|_| {
                PluginError::Invalid(format!("plugin {plugin}: {path} has a non-UTF-8 key"))
            })?;
            let child = child_path(path, &key);
            budget.charge(plugin, &child, key.len().saturating_add(1))?;
            fields.insert(
                key.to_owned(),
                data_value(value, &child, plugin, depth, budget)?,
            );
        }
        return Ok(Value::Object(fields));
    }
    Err(PluginError::Invalid(format!(
        "plugin {plugin}: {path} must not mix string keys and array indices"
    )))
}

impl ProviderFact {
    fn from_fields(
        plugin: &str,
        path: &str,
        mut fields: Map<String, Value>,
        source_path: Option<&Path>,
    ) -> Result<Self, PluginError> {
        let name = match fields.remove("name") {
            Some(Value::String(name)) => name,
            Some(_) => {
                return Err(PluginError::Invalid(format!(
                    "plugin {plugin}: {path}.name must be a string"
                )))
            }
            None => {
                return Err(PluginError::Invalid(format!(
                    "plugin {plugin}: {path}.name is required"
                )))
            }
        };
        if let Err(error) =
            super::names::validate_contribution_name(ContributionKind::Provider, &name)
        {
            return Err(PluginError::Invalid(format!(
                "plugin {plugin}: {path}.name is invalid: {error}"
            )));
        }
        Ok(Self {
            plugin: plugin.to_owned(),
            name,
            source_path: source_path.map(Path::to_path_buf),
            url: string_field(&mut fields, "url", plugin, path)?,
            protocol: string_field(&mut fields, "protocol", plugin, path)?,
            auth: fields.remove("auth"),
            headers: fields.remove("headers"),
            defaults: fields.remove("defaults"),
            limits: fields.remove("limits"),
            extra: fields,
        })
    }
}

fn string_field(
    fields: &mut Map<String, Value>,
    key: &str,
    plugin: &str,
    path: &str,
) -> Result<Option<String>, PluginError> {
    match fields.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(PluginError::Invalid(format!(
            "plugin {plugin}: {path}.{key} must be a string"
        ))),
    }
}

#[cfg(test)]
mod tests;
