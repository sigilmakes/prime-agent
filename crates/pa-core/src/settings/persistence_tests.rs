use std::sync::Arc;

use serde_json::{json, Value};

use super::{InMemorySettingsStorage, SettingsManager, SettingsScope, SettingsStorage};

fn fixture(content: &str) -> (Arc<InMemorySettingsStorage>, SettingsManager) {
    let storage = Arc::new(InMemorySettingsStorage::default());
    storage
        .with_lock(SettingsScope::Global, &mut |_| Some(content.to_string()))
        .unwrap();
    let manager = SettingsManager::from_storage(storage.clone());
    (storage, manager)
}

fn document(storage: &dyn SettingsStorage) -> Value {
    serde_json::from_str(&storage.read(SettingsScope::Global).unwrap().unwrap()).unwrap()
}

#[test]
fn model_switch_preserves_a_document_that_failed_to_load() {
    for content in ["{\"theme\":\"light\",}", "\u{feff}{\"theme\":\"light\"}"] {
        let (storage, mut manager) = fixture(content);
        manager
            .set_default_model_and_provider("openai", "model")
            .unwrap();
        assert_eq!(
            storage.read(SettingsScope::Global).unwrap().as_deref(),
            Some(content)
        );
        assert!(manager
            .errors()
            .iter()
            .any(|error| error.message.contains("not saved")));
        assert_eq!(manager.get_default_model(), Some("model"));
    }
}

#[test]
fn chat_detail_save_changes_only_the_requested_setting() {
    let (storage, mut manager) = fixture(r#"{"theme":"light","pluginSetting":{"keep":true}}"#);
    manager.set_chat_detail("details").unwrap();
    assert_eq!(
        document(storage.as_ref()),
        json!({
            "theme":"light", "pluginSetting":{"keep":true}, "chatDetail":"details"
        })
    );
}

#[test]
fn stale_managers_preserve_other_writers_and_explicit_same_value_choices() {
    let (storage, mut first) =
        fixture(r#"{"theme":"light","telemetry":{"enabled":false,"futureSetting":"keep"}}"#);
    let mut second = SettingsManager::from_storage(storage.clone());
    second.set_theme("dark".into()).unwrap();
    first.set_theme("light".into()).unwrap();
    second.set_quiet_startup(true).unwrap();
    assert_eq!(
        document(storage.as_ref()),
        json!({
            "theme":"light", "quietStartup":true,
            "telemetry":{"enabled":false,"futureSetting":"keep"}
        })
    );
}

#[test]
fn saves_preserve_a_document_corrupted_after_load() {
    let (storage, mut manager) = fixture(r#"{"theme":"light"}"#);
    for content in ["{bad", "[]", "null"] {
        storage
            .with_lock(SettingsScope::Global, &mut |_| Some(content.into()))
            .unwrap();
        assert!(manager.set_theme("dark".into()).is_err());
        assert_eq!(
            storage.read(SettingsScope::Global).unwrap().as_deref(),
            Some(content)
        );
    }
    storage
        .with_lock(SettingsScope::Global, &mut |_| {
            Some(r#"{"external":"keep"}"#.into())
        })
        .unwrap();
    manager.set_quiet_startup(true).unwrap();
    assert_eq!(
        document(storage.as_ref()),
        json!({
            "external":"keep", "theme":"dark", "quietStartup":true
        })
    );
}

#[test]
fn resource_save_preserves_a_document_corrupted_after_load() {
    let (storage, mut manager) = fixture("{}");
    storage
        .with_lock(SettingsScope::Global, &mut |_| Some("{bad".into()))
        .unwrap();
    manager.set_global_resource_array("skills", vec!["new".into()]);
    assert_eq!(
        storage.read(SettingsScope::Global).unwrap().as_deref(),
        Some("{bad")
    );
    assert!(manager
        .errors()
        .iter()
        .any(|error| error.message.contains("not saved")));
}

#[test]
fn clearing_a_setting_removes_only_that_key() {
    let (storage, mut manager) = fixture(r#"{"enabledModels":["old"],"theme":"light"}"#);
    manager.set_enabled_models(None).unwrap();
    assert_eq!(document(storage.as_ref()), json!({"theme":"light"}));
}

#[test]
fn resource_save_survives_an_unrelated_later_save() {
    let (storage, mut manager) = fixture("{}");
    manager.set_packages(vec![json!("first")]);
    let mut other = SettingsManager::from_storage(storage.clone());
    other.set_packages(vec![json!("second")]);
    manager.set_theme("dark".into()).unwrap();
    assert_eq!(
        document(storage.as_ref()),
        json!({"packages":["second"],"theme":"dark"})
    );
}

#[test]
fn harness_refinements_persist_without_clobbering_other_settings() {
    let (storage, mut manager) =
        fixture(r#"{"theme":"light","autoRefine":{"turnInterval":3},"future":{"keep":true}}"#);
    assert!(manager.get_harness_refinements());
    manager.set_harness_refinements(false).unwrap();
    assert!(!manager.reopen().get_harness_refinements());
    assert_eq!(
        document(storage.as_ref()),
        json!({
            "theme":"light", "autoRefine":{"turnInterval":3}, "future":{"keep":true}, "harnessRefinements":false
        })
    );
    manager.set_harness_refinements(true).unwrap();
    assert!(manager.reopen().get_harness_refinements());
}

#[test]
fn harness_refinements_ignore_project_overrides_and_resolve_global_wrong_types() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let project_dir = dir.path().join(".prime/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"harnessRefinements":false}"#,
    )
    .unwrap();
    std::fs::write(
        project_dir.join("settings.json"),
        r#"{"harnessRefinements":true}"#,
    )
    .unwrap();
    let mut manager = SettingsManager::create(dir.path(), &agent_dir);
    assert!(!manager.get_harness_refinements());
    manager.set_harness_refinements(true).unwrap();
    std::fs::write(
        project_dir.join("settings.json"),
        r#"{"harnessRefinements":false}"#,
    )
    .unwrap();
    assert!(manager.reopen().get_harness_refinements());
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"harnessRefinements":"false"}"#,
    )
    .unwrap();
    assert!(manager.reopen().get_harness_refinements());
}

#[test]
fn compaction_percentage_preserves_siblings_and_survives_reload() {
    let (storage, mut manager) =
        fixture(r#"{"compaction":{"enabled":false,"reserveTokens":1234,"future":"keep"}}"#);
    assert_eq!(manager.get_compaction_threshold_percent(), 95.0);
    for percent in [1.0, 62.5, 95.0, 100.0] {
        manager.set_compaction_threshold_percent(percent).unwrap();
        manager.reload().unwrap();
        assert_eq!(manager.get_compaction_threshold_percent(), percent);
        assert_eq!(
            document(storage.as_ref()),
            json!({"compaction":{
                "enabled":false,"reserveTokens":1234,"future":"keep","thresholdPercent":percent
            }})
        );
    }
}

#[test]
fn invalid_compaction_percentage_never_mutates_or_saves() {
    let (storage, mut manager) = fixture(r#"{"compaction":{"thresholdPercent":80.0}}"#);
    for percent in [
        0.0,
        -1.0,
        100.01,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert!(manager.set_compaction_threshold_percent(percent).is_err());
        assert_eq!(manager.get_compaction_threshold_percent(), 80.0);
        assert_eq!(
            document(storage.as_ref()),
            json!({"compaction":{"thresholdPercent":80.0}})
        );
    }
}

#[test]
fn invalid_loaded_percentage_keeps_other_compaction_settings() {
    for invalid in [
        json!(null),
        json!("80"),
        json!(false),
        json!(-1),
        json!(0),
        json!(101),
        json!({}),
        json!([]),
    ] {
        let settings = super::load::from_value_lenient(&json!({"compaction":{
            "enabled":false,"reserveTokens":1234,"thresholdPercent":invalid
        }}));
        let manager = SettingsManager::in_memory(&settings);
        assert_eq!(manager.get_compaction_threshold_percent(), 95.0);
        assert!(!manager.get_compaction_enabled());
        assert_eq!(
            manager
                .settings()
                .compaction
                .as_ref()
                .unwrap()
                .reserve_tokens,
            Some(1234)
        );
    }
}

#[test]
fn project_percentage_overrides_global_and_stale_writes_preserve_external_settings() {
    let (storage, mut first) = fixture(r#"{"compaction":{"thresholdPercent":80.0}}"#);
    let mut second = SettingsManager::from_storage(storage.clone());
    second.set_compaction_enabled(false).unwrap();
    first.set_compaction_threshold_percent(70.0).unwrap();
    assert_eq!(
        document(storage.as_ref()),
        json!({"compaction":{"enabled":false,"thresholdPercent":70.0}})
    );
    storage
        .with_lock(SettingsScope::Project, &mut |_| {
            Some(r#"{"compaction":{"thresholdPercent":60.5}}"#.into())
        })
        .unwrap();
    first.reload().unwrap();
    assert_eq!(first.get_compaction_threshold_percent(), 60.5);
    first.set_compaction_threshold_percent(90.0).unwrap();
    first.reload().unwrap();
    assert_eq!(first.get_compaction_threshold_percent(), 60.5);
}

#[test]
fn aliased_settings_scopes_have_one_global_authority() {
    for alias in [
        "same",
        "directory",
        "file",
        "dangling-file",
        "missing-file",
        "missing-tree",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("cwd");
        let project = cwd.join(".prime/agent");
        let global = dir.path().join("global");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        let agent = match alias {
            "same" => project.clone(),
            #[cfg(unix)]
            "directory" | "missing-file" => {
                std::fs::create_dir_all(cwd.join(".prime")).unwrap();
                std::os::unix::fs::symlink(&global, &project).unwrap();
                global.clone()
            }
            #[cfg(unix)]
            "file" | "dangling-file" => {
                std::fs::create_dir_all(&project).unwrap();
                std::os::unix::fs::symlink(
                    global.join("settings.json"),
                    project.join("settings.json"),
                )
                .unwrap();
                global.clone()
            }
            #[cfg(unix)]
            "missing-tree" => {
                std::os::unix::fs::symlink(&cwd, dir.path().join("alias")).unwrap();
                dir.path().join("alias/.prime/agent")
            }
            _ => continue,
        };
        if !matches!(alias, "missing-file" | "missing-tree" | "dangling-file") {
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(
                agent.join("settings.json"),
                r#"{"theme":"old","terminal":{"showImages":true,"future":"keep"}}"#,
            )
            .unwrap();
        }
        let mut manager = SettingsManager::create(&cwd, &agent);
        assert_eq!(
            serde_json::to_value(manager.project_settings()).unwrap(),
            serde_json::to_value(super::Settings::default()).unwrap(),
            "{alias}"
        );
        manager.set_theme("new".into()).unwrap();
        manager.set_show_images(false).unwrap();
        assert_eq!(manager.get_theme(), Some("new"), "{alias}");
        assert!(!manager.get_show_images(), "{alias}");
        manager.set_project_packages(vec![json!("project")]);
        assert_eq!(
            manager.global_settings().packages,
            Some(vec![json!("project")]),
            "{alias}"
        );
        manager.set_packages(vec![json!("global")]);
        assert_eq!(
            manager.settings().packages,
            Some(vec![json!("global")]),
            "{alias}"
        );
        manager.set_project_resource_array("skills", vec!["project-skill".into()]);
        assert_eq!(
            manager.global_settings().skills,
            Some(vec!["project-skill".into()]),
            "{alias}"
        );
        manager.set_global_resource_array("skills", vec!["global-skill".into()]);
        assert_eq!(
            manager.settings().skills,
            Some(vec!["global-skill".into()]),
            "{alias}"
        );
        manager.reload().unwrap();
        manager.set_show_images(true).unwrap();
        manager.set_show_images(false).unwrap();
        let restarted = SettingsManager::create(&cwd, &agent);
        assert_eq!(restarted.get_theme(), Some("new"), "{alias}");
        assert!(!restarted.get_show_images(), "{alias}");
        assert_eq!(
            restarted.settings().packages,
            Some(vec![json!("global")]),
            "{alias}"
        );
        assert_eq!(
            restarted.settings().skills,
            Some(vec!["global-skill".into()]),
            "{alias}"
        );
        assert_eq!(
            serde_json::to_value(manager.project_settings()).unwrap(),
            serde_json::to_value(super::Settings::default()).unwrap(),
            "{alias}"
        );
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(project.join("settings.json")).unwrap())
                .unwrap();
        if !matches!(alias, "missing-file" | "missing-tree" | "dangling-file") {
            assert_eq!(saved["terminal"]["future"], "keep", "{alias}");
        }
        #[cfg(unix)]
        if matches!(alias, "file" | "dangling-file") {
            assert!(std::fs::symlink_metadata(project.join("settings.json"))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }
}

#[test]
#[cfg(unix)]
fn global_settings_file_symlink_survives_atomic_save() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let target = dir.path().join("target.json");
    std::fs::write(&target, r#"{"theme":"old","terminal":{"showImages":true}}"#).unwrap();
    std::os::unix::fs::symlink(&target, agent.join("settings.json")).unwrap();
    let mut manager = SettingsManager::create(dir.path().join("cwd"), &agent);
    manager.set_theme("new".into()).unwrap();
    manager.set_show_images(false).unwrap();
    assert!(std::fs::symlink_metadata(agent.join("settings.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(
        saved,
        json!({"theme":"new","terminal":{"showImages":false}})
    );
    assert_eq!(manager.reopen().get_theme(), Some("new"));
}

#[test]
fn tree_filter_falls_back_to_default_but_keeps_explicit_preferences() {
    let mut manager = SettingsManager::in_memory(&super::Settings::default());
    assert_eq!(manager.get_tree_filter_mode(), "default");
    for mode in ["user-only", "no-tools", "labeled-only", "all", "default"] {
        manager.set_tree_filter_mode(mode).unwrap();
        assert_eq!(manager.reopen().get_tree_filter_mode(), mode);
    }
    manager.set_tree_filter_mode("invalid").unwrap();
    assert_eq!(manager.reopen().get_tree_filter_mode(), "default");
}

#[test]
fn in_memory_scopes_remain_independent_by_default() {
    let (storage, mut manager) = fixture(r#"{"packages":["global"],"skills":["global-skill"]}"#);
    manager.set_project_packages(vec![json!("project")]);
    manager.set_project_resource_array("skills", vec!["project-skill".into()]);
    manager.set_packages(vec![json!("changed-global")]);
    manager.set_global_resource_array("skills", vec!["changed-global-skill".into()]);
    let restarted = SettingsManager::from_storage(storage);
    assert_eq!(
        restarted.global_settings().packages,
        Some(vec![json!("changed-global")])
    );
    assert_eq!(restarted.settings().packages, Some(vec![json!("project")]));
    assert_eq!(
        restarted.settings().skills,
        Some(vec!["project-skill".into()])
    );
}
