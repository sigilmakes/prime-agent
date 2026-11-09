use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use mlua::UserData;
use serde_json::{json, Value};

use super::*;

fn entry(path: &str, config: Value) -> PluginEntry {
    PluginEntry {
        enabled: Some(true),
        path: Some(PathBuf::from(path)),
        config,
    }
}

fn plugin_map(entries: &[(&str, PluginEntry)]) -> BTreeMap<String, PluginEntry> {
    entries
        .iter()
        .map(|(name, entry)| ((*name).to_owned(), entry.clone()))
        .collect()
}

fn write(root: &Path, relative: &str, source: &str) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, source).unwrap();
    path
}

struct Fixture {
    _dir: tempfile::TempDir,
    agent: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Self {
            _dir: dir,
            agent,
            cwd,
        }
    }
}

#[test]
fn facts_load_from_relative_sources_and_keep_extra_data() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "plugins/local.lua",
        r#"
        return {providers = {
          {name = "llama-swap", url = "http://127.0.0.1:8090/v1", protocol = "openai",
           auth = null,
           headers = {["x-local"] = "1"},
           defaults = {reasoning = false},
           limits = {context = 32768},
           unknown = {nested = {"v"}}}
        }}
        "#,
    );
    let plugins = plugin_map(&[("local", entry("plugins/local.lua", json!({"unused": true})))]);
    let catalog = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(catalog.providers.len(), 1);
    let fact = &catalog.providers[0];
    assert_eq!(fact.plugin, "local");
    assert_eq!(fact.name, "llama-swap");
    assert_eq!(fact.url.as_deref(), Some("http://127.0.0.1:8090/v1"));
    assert_eq!(fact.protocol.as_deref(), Some("openai"));
    assert_eq!(fact.auth, Some(Value::Null));
    assert_eq!(fact.headers, Some(json!({"x-local": "1"})));
    assert_eq!(fact.defaults, Some(json!({"reasoning": false})));
    assert_eq!(fact.limits, Some(json!({"context": 32768})));
    assert_eq!(fact.extra.get("unknown"), Some(&json!({"nested": ["v"]})));
    assert_ne!(catalog.revision, 0);
    assert!(!fixture.cwd.join("plugins/local.lua").exists());
}

#[test]
fn fact_pass_has_no_host_capability() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "capabilities.lua",
        r#"
        assert(ctx == nil, "ctx capability was exposed")
        assert(host == nil, "host capability was exposed")
        assert(session == nil, "session capability was exposed")
        assert(io ~= nil and os ~= nil and load ~= nil and print ~= nil, "standard library was absent")
        assert(debug == nil, "debug library was exposed")
        return {providers = {{name = "safe", url = "http://localhost", protocol = "openai"}}}
        "#,
    );
    let plugins = plugin_map(&[("safe", entry("capabilities.lua", json!({})))]);
    let catalog = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(catalog.providers.len(), 1);

    write(
        &fixture.agent,
        "calls.lua",
        r#"ctx.call("session.info", {}) return {}"#,
    );
    let plugins = plugin_map(&[("calls", entry("calls.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    assert!(error.to_string().contains("ctx"), "{error}");
}

#[test]
fn rejects_function_values_in_a_chunk() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "bad.lua",
        r#"return {providers = {{name = "bad", url = "http://localhost", protocol = "openai", limits = {callback = function() return true end}}}}"#,
    );
    let plugins = plugin_map(&[("bad_plugin", entry("bad.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("bad_plugin"), "{message}");
    assert!(
        message.contains("providers[1].limits.callback"),
        "{message}"
    );
    assert!(message.contains("found a function"), "{message}");
}

struct Opaque;
impl UserData for Opaque {}

#[test]
fn rejects_thread_and_userdata_values() {
    let lua = mlua::Lua::new();
    let thread = lua
        .create_thread(lua.create_function(|_, ()| Ok(())).unwrap())
        .unwrap();
    let error = data_value(
        LuaValue::Thread(thread),
        "providers[1].limits.worker",
        "plugin-thread",
        0,
        &mut Budget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("plugin-thread"));
    assert!(error.to_string().contains("providers[1].limits.worker"));
    assert!(error.to_string().contains("found a thread"));

    let userdata = lua.create_userdata(Opaque).unwrap();
    let error = data_value(
        LuaValue::UserData(userdata),
        "providers[1].auth",
        "plugin-userdata",
        0,
        &mut Budget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("plugin-userdata"));
    assert!(error.to_string().contains("providers[1].auth"));
    assert!(error.to_string().contains("userdata"));
}

#[test]
fn bounds_depth_and_bytes() {
    let fixture = Fixture::new();
    let mut nested = "{}".to_owned();
    for _ in 0..=MAX_PROVIDER_FACT_DEPTH {
        nested = format!("{{a = {nested}}}");
    }
    write(
        &fixture.agent,
        "deep.lua",
        &format!(
            "return {{providers = {{{{name = 'deep', url = 'http://localhost', protocol = 'openai', auth = {nested}}}}}}}"
        ),
    );
    let plugins = plugin_map(&[("deep", entry("deep.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    assert!(error.to_string().contains("maximum provider fact depth"));

    write(
        &fixture.agent,
        "wide.lua",
        &format!(
            "return {{providers = {{{{name = 'wide', url = 'http://localhost', protocol = 'openai', auth = string.rep('x', {})}}}}}}",
            MAX_PROVIDER_FACT_BYTES + 1
        ),
    );
    let plugins = plugin_map(&[("wide", entry("wide.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    assert!(error.to_string().contains("provider fact budget"));
}

#[test]
fn cache_invalidates_on_source_config_and_enabled_changes() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "cached.lua",
        r#"return {providers = {{name = "one", url = config.url or "http://one", protocol = "openai"}}}"#,
    );
    let mut plugins = plugin_map(&[(
        "cached",
        entry("cached.lua", json!({"url": "http://config-a"})),
    )]);
    let first = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(first.providers[0].url.as_deref(), Some("http://config-a"));
    let again = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(first.revision, again.revision);

    plugins.get_mut("cached").unwrap().config = json!({"url": "http://config-b"});
    let configured = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(
        configured.providers[0].url.as_deref(),
        Some("http://config-b")
    );
    assert_ne!(first.revision, configured.revision);

    write(
        &fixture.agent,
        "cached.lua",
        r#"return {providers = {{name = "two", url = "http://two", protocol = "openai"}}}"#,
    );
    let replaced = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(replaced.providers[0].name, "two");
    assert_ne!(configured.revision, replaced.revision);

    plugins.get_mut("cached").unwrap().enabled = Some(false);
    let disabled = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(disabled.providers.len(), 0);
    assert_ne!(replaced.revision, disabled.revision);
}

#[test]
fn only_enabled_file_backed_entries_contribute_facts() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "enabled.lua",
        r#"return {providers = {{name = "enabled", url = "http://localhost", protocol = "openai"}}}"#,
    );
    write(
        &fixture.agent,
        "disabled.lua",
        r#"return {providers = {{name = "disabled", url = "http://localhost", protocol = "openai"}}}"#,
    );
    let mut plugins = plugin_map(&[
        ("enabled", entry("enabled.lua", json!({}))),
        ("disabled", entry("disabled.lua", json!({}))),
        (
            "builtin_config",
            PluginEntry {
                enabled: Some(true),
                path: None,
                config: json!({"compactors": []}),
            },
        ),
    ]);
    plugins.get_mut("disabled").unwrap().enabled = Some(false);
    let catalog = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap();
    assert_eq!(catalog.providers.len(), 1);
    assert_eq!(catalog.providers[0].name, "enabled");
}

#[test]
fn typed_provider_fields_and_missing_names_are_reported() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "bad-url.lua",
        r#"return {providers = {{name = "bad", url = 42, protocol = "openai"}}}"#,
    );
    let plugins = plugin_map(&[("bad_url", entry("bad-url.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("bad_url"), "{message}");
    assert!(message.contains("providers[1].url"), "{message}");
    assert!(message.contains("must be a string"), "{message}");

    write(
        &fixture.agent,
        "nameless.lua",
        r#"return {providers = {{url = "http://localhost", protocol = "openai"}}}"#,
    );
    let plugins = plugin_map(&[("nameless", entry("nameless.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    assert!(error.to_string().contains("providers[1].name is required"));
}

#[test]
fn non_data_tables_are_reported() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "mixed.lua",
        r#"return {providers = {{name = "mixed", url = "http://localhost", protocol = "openai", headers = {[1] = "a", ["b"] = "c"}}}}"#,
    );
    let plugins = plugin_map(&[("mixed", entry("mixed.lua", json!({})))]);
    let error = ManifestCatalog::load(&fixture.agent, Some(&plugins)).unwrap_err();
    assert!(error.to_string().contains("providers[1].headers"));
    assert!(error.to_string().contains("must not mix"));
}

#[test]
fn project_scope_plugin_entries_never_reach_the_fact_pass() {
    let fixture = Fixture::new();
    write(
        &fixture.agent,
        "global.lua",
        r#"return {providers = {{name = "glob", url = "http://glob", protocol = "openai"}}}"#,
    );
    write(
        &fixture.agent,
        "ignored.lua",
        r#"return {providers = {{name = "ignored", url = "http://ignored", protocol = "openai"}}}"#,
    );
    write(
        &fixture.cwd,
        ".prime/agent/global.lua",
        r#"return {providers = {{name = "project", url = "http://project", protocol = "openai"}}}"#,
    );
    std::fs::write(
        fixture.agent.join("settings.json"),
        json!({"plugins":{"glob_plugin":{"enabled":true,"path":"global.lua"}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        fixture.cwd.join(".prime/agent/settings.json"),
        json!({"plugins":{
            "glob_plugin":{"enabled":true,"path":"global.lua"},
            "ignored":{"enabled":true,"path":"ignored.lua"}
        }})
        .to_string(),
    )
    .unwrap();
    let manager = crate::settings::SettingsManager::create(&fixture.cwd, &fixture.agent);
    let catalog = ManifestCatalog::from_settings(&fixture.agent, manager.settings()).unwrap();
    assert_eq!(catalog.providers.len(), 1);
    assert_eq!(catalog.providers[0].plugin, "glob_plugin");
    assert_eq!(catalog.providers[0].name, "glob");
}

#[test]
fn malformed_plugin_map_is_rejected_instead_of_yielding_an_empty_catalog() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.agent.join("settings.json"),
        json!({"plugins":{"bad":{"enabled":true,"unknown":1}}}).to_string(),
    )
    .unwrap();
    let manager = crate::settings::SettingsManager::create(&fixture.cwd, &fixture.agent);
    let error = ManifestCatalog::from_settings(&fixture.agent, manager.settings()).unwrap_err();
    assert!(error.to_string().contains("enabled?, path?, config?"));
}
