//! Custom-theme regressions use disposable files and real render paths.
use super::*;
use crate::settings_menu::{SettingsCurrentValues, SettingsMenu, SettingsMenuAction};
use serde_json::{json, Value};

fn custom_json() -> Value {
    let mut value: Value = serde_json::from_str(PRIME_JSON).unwrap();
    value["name"] = json!("Terminal native");
    value["vars"] = json!({"ansi": 4, "accentAlias": "ansi", "native": "", "resetAlias": "native"});
    let colors = value["colors"].as_object_mut().unwrap();
    for color in colors.values_mut() {
        *color = json!("resetAlias");
    }
    colors.insert("accent".to_string(), json!("accentAlias"));
    colors.insert("border".to_string(), json!(2));
    colors.insert("selectedBg".to_string(), json!(4));
    value
}

fn write_theme(dir: &std::path::Path, name: &str, value: &Value) -> std::path::PathBuf {
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path
}

#[test]
fn custom_numeric_vars_and_resets_render_in_both_color_modes() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_theme(dir.path(), "terminal-native", &custom_json());
    for mode in [ColorMode::TrueColor, ColorMode::Color256] {
        let theme = load_theme_from_path(&path, mode).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 1)).unwrap();
        let line = vec![
            theme.fg(ThemeColor::Accent, "ANSI"),
            theme.fg(ThemeColor::Text, "reset"),
            crate::Span::styled("BG".to_string(), theme.bg_style(ThemeBg::UserMessageBg)),
        ];
        terminal
            .draw(|frame| {
                frame.render_widget(crate::markdown::to_ratatui_line(&line), frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(4));
        assert_eq!(buffer[(4, 0)].fg, Color::Reset);
        assert_eq!(buffer[(9, 0)].bg, Color::Reset);
        assert_eq!(theme.bg_color(ThemeBg::SelectedBg), Some(Color::Indexed(4)));
    }
}

#[test]
fn custom_validation_rejects_bad_values_and_references_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        json!(-1),
        json!(256),
        json!(1.5),
        Value::Null,
        json!(true),
        json!([]),
        json!({}),
        json!("#gg0000"),
        json!("#éabc"),
        json!("#aéabc"),
        json!("missing"),
    ] {
        let mut value = custom_json();
        value["colors"]["accent"] = bad.clone();
        let path = write_theme(dir.path(), "bad", &value);
        let error = load_theme_from_path(&path, ColorMode::TrueColor).unwrap_err();
        assert!(format!("{error:#}").contains("accent"), "{bad}: {error:#}");
    }
    for vars in [
        json!({"a": "a"}),
        json!({"a": "b", "b": "a"}),
        json!({"unused": 999}),
        json!({"unused": "missing"}),
    ] {
        let mut value = custom_json();
        value["vars"] = vars;
        let path = write_theme(dir.path(), "bad-vars", &value);
        let error = load_theme_from_path(&path, ColorMode::TrueColor).unwrap_err();
        assert!(format!("{error:#}").contains("vars token"));
    }
    let mut incomplete = custom_json();
    incomplete["colors"].as_object_mut().unwrap().remove("text");
    let path = write_theme(dir.path(), "incomplete", &incomplete);
    assert!(load_theme_from_path(&path, ColorMode::TrueColor)
        .unwrap_err()
        .to_string()
        .contains("text"));
    std::fs::write(&path, "{bad json").unwrap();
    assert!(load_theme_from_path(&path, ColorMode::TrueColor).is_err());
}

#[test]
fn custom_optional_slots_use_the_documented_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = custom_json();
    let colors = value["colors"].as_object_mut().unwrap();
    for name in ["mdBody", "refinementHeader", "refinementSummary"] {
        colors.remove(name);
    }
    let path = write_theme(dir.path(), "optional", &value);
    let theme = load_theme_from_path(&path, ColorMode::TrueColor).unwrap();
    assert_eq!(
        theme.fg_style(ThemeColor::MdBody),
        theme.fg_style(ThemeColor::Text)
    );
    assert_eq!(
        theme.fg_style(ThemeColor::RefinementHeader),
        Theme::builtin("prime", ColorMode::TrueColor).fg_style(ThemeColor::RefinementHeader)
    );
}

#[test]
fn custom_discovery_is_sorted_unique_and_ignores_non_theme_entries() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["z-last", "terminal-native", "dark"] {
        write_theme(dir.path(), name, &custom_json());
    }
    std::fs::write(dir.path().join("notes.txt"), "not a theme").unwrap();
    std::fs::create_dir(dir.path().join("directory.json")).unwrap();
    assert_eq!(
        available_theme_names_in(dir.path()).unwrap(),
        vec!["dark", "light", "prime", "terminal-native", "z-last"]
    );
    assert_eq!(
        available_theme_names_in(&dir.path().join("missing")).unwrap(),
        vec!["dark", "light", "prime"]
    );
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, "x").unwrap();
    assert!(available_theme_names_in(&file).is_err());
}

/// Run the product name loader with isolated process environment. No test
/// mutates HOME or the agent-directory override in the shared test process.
#[test]
fn custom_product_path_settings_preview_and_frame_use_native_colors() {
    const CHILD: &str = "PA_TUI_CUSTOM_THEME_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let override_dir = dir.path().join("override");
        for agent_dir in [home.join(".prime/agent"), override_dir.clone()] {
            std::fs::create_dir_all(agent_dir.join("themes")).unwrap();
            write_theme(&agent_dir.join("themes"), "terminal-native", &custom_json());
            // A builtin collision cannot replace the builtin's palette.
            write_theme(&agent_dir.join("themes"), "prime", &custom_json());
        }
        for use_override in [false, true] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.env_clear()
                .args(["--exact", "theme::custom_tests::custom_product_path_settings_preview_and_frame_use_native_colors", "--nocapture"])
                .env(CHILD, "1")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
                .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
                .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
                .env("XDG_STATE_HOME", dir.path().join("xdg-state"))
                .env("XDG_RUNTIME_DIR", dir.path().join("xdg-runtime"))
                .env("TMPDIR", dir.path())
                .env("TEMP", dir.path())
                .env("TMP", dir.path())
                .env("COLORTERM", "truecolor");
            if use_override {
                command.env(pa_types::platform::dirs::ENV_AGENT_DIR, &override_dir);
                // Prove the override wins, rather than accidentally reading HOME.
                std::fs::rename(
                    home.join(".prime/agent/themes/terminal-native.json"),
                    home.join("home-theme.saved"),
                )
                .unwrap();
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let names = available_theme_names().unwrap();
    assert_eq!(names, vec!["dark", "light", "prime", "terminal-native"]);
    let theme = crate::app::load_theme("terminal-native");
    assert_eq!(
        load_named_theme("prime", ColorMode::TrueColor).unwrap(),
        Theme::builtin("prime", ColorMode::TrueColor)
    );
    for name in [
        "../escape",
        "dir/theme",
        r"dir\theme",
        ".",
        "..",
        "bad\u{1b}",
        "not-found",
    ] {
        assert!(load_named_theme(name, ColorMode::TrueColor).is_err());
        assert_eq!(
            crate::app::load_theme(name),
            Theme::builtin("prime", ColorMode::TrueColor)
        );
    }
    assert_eq!(
        crate::app::load_theme(""),
        Theme::builtin("prime", ColorMode::TrueColor)
    );
    let kb = crate::keybindings::KeybindingsManager::new();
    let values = SettingsCurrentValues {
        available_themes: names,
        theme: "prime".to_string(),
        ..Default::default()
    };
    let mut menu = SettingsMenu::new(crate::settings_menu::settings_menu_rows(&values));
    menu.handle_key("3", &kb);
    menu.handle_key("enter", &kb);
    let lines = menu.render(&theme, 80, &kb);
    assert!(lines.iter().any(|line| line
        .iter()
        .any(|span| span.content.contains("terminal-native"))));
    for _ in 0..3 {
        menu.handle_key("down", &kb);
    }
    let mut view = crate::view::AgentView::new(theme);
    view.settings_menu = Some(menu);
    view.editor.set_text("native prompt");
    let frame = view.render_frame(100, 40);
    assert!(frame
        .iter()
        .flatten()
        .any(|span| span.style.fg == Some(Color::Indexed(4))));
    assert!(frame
        .iter()
        .flatten()
        .any(|span| span.style.bg == Some(Color::Indexed(4))));
    assert!(!frame
        .iter()
        .flatten()
        .any(|span| matches!(span.style.fg, Some(Color::Rgb(_, _, _)))));
    assert_eq!(view.theme.name, "terminal-native");
    assert_eq!(
        view.settings_menu
            .as_mut()
            .unwrap()
            .handle_key("enter", &kb),
        SettingsMenuAction::Change {
            id: "theme",
            value: "terminal-native".to_string()
        }
    );
    // A file that goes bad after discovery is rejected at apply/reload time.
    let dir = pa_types::platform::agent_dir().unwrap();
    std::fs::write(dir.join("themes/terminal-native.json"), "{bad json").unwrap();
    assert!(load_named_theme("terminal-native", ColorMode::TrueColor).is_err());
    assert_eq!(
        crate::app::load_theme("terminal-native"),
        Theme::builtin("prime", ColorMode::TrueColor)
    );
}
