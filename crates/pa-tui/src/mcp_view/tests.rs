use super::*;
use crate::keybindings::KeybindingsManager;
use crate::theme::{ColorMode, Theme};
use serde_json::json;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn frame_text(view: &mut McpView) -> Vec<String> {
    view.render(&theme(), 110, &kb())
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn response() -> serde_json::Value {
    json!({
        "connections": [
            {
                "server": "fixture-echo", "label": "fixture-echo",
                "connected": true, "usesOAuth": false,
                "authKind": "stdio", "transport": "stdio",
                "userDeclared": true, "generic": true
            },
            {
                "server": "linear", "label": "Linear",
                "connected": false, "usesOAuth": true,
                "authKind": "subscription", "transport": "http",
                "userDeclared": false, "generic": false
            }
        ]
    })
}

#[test]
fn renders_configured_servers_and_status() {
    let mut view = McpView::from_response(&response(), 19);
    let frame = frame_text(&mut view);
    let text = frame.join("\n");
    assert!(text.contains("Search MCP connections"), "{text}");
    assert!(text.contains("fixture-echo"), "{text}");
    assert!(text.contains("Linear"), "{text}");
    assert!(text.contains("Connected"), "{text}");
    assert!(text.contains("Connect"), "{text}");
    assert!(text.contains("stdio \u{b7} stdio"), "{text}");
}

#[test]
fn enter_selects_the_highlighted_server() {
    let mut view = McpView::from_response(&response(), 19);
    assert_eq!(
        view.handle_key("enter", &kb()),
        McpViewAction::Select {
            server: "fixture-echo".to_string(),
            label: "fixture-echo".to_string()
        }
    );
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        McpViewAction::Select {
            server: "linear".to_string(),
            label: "Linear".to_string()
        }
    );
}

#[test]
fn search_filters_by_server_and_transport() {
    let mut view = McpView::from_response(&response(), 19);
    view.set_search("linear");
    assert_eq!(view.selected_server(), Some("linear"));
    view.set_search("stdio");
    assert_eq!(view.selected_server(), Some("fixture-echo"));
    view.set_search("nothing");
    assert_eq!(view.selected_server(), None);
    assert!(frame_text(&mut view)
        .join("\n")
        .contains("No matching servers"));
}

#[test]
fn escape_and_ctrl_c_cancel() {
    let mut view = McpView::from_response(&response(), 19);
    assert_eq!(view.handle_key("escape", &kb()), McpViewAction::Cancel);
    assert_eq!(view.handle_key("ctrl+c", &kb()), McpViewAction::Cancel);
}

#[test]
fn empty_response_renders_the_empty_state() {
    let mut view = McpView::from_response(&json!({}), 19);
    let text = frame_text(&mut view).join("\n");
    assert!(text.contains("No MCP servers configured"), "{text}");
}
