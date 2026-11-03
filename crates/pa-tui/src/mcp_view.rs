use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::menu_panel::{menu_list_layout, search_field_lines};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

use serde_json::Value;

mod rows;

use rows::{flatten_to_single_line, McpServerRow};

const SEARCH_PLACEHOLDER: &str = "Search MCP connections";
const PREFERRED_VISIBLE_SERVICES: usize = 8;
const SEARCH_FIELD_ROWS: usize = 3;
const HINT_ROWS: usize = 1;
const SCROLL_INDICATOR_ROWS: usize = 1;
const DETAIL_ROWS: usize = 1;
const DETAIL_SPACER_ROWS: usize = 1;
const MIN_ROWS_FOR_DETAIL: usize =
    SEARCH_FIELD_ROWS + HINT_ROWS + SCROLL_INDICATOR_ROWS + DETAIL_ROWS + DETAIL_SPACER_ROWS + 2;
const EMPTY_STATE_ROWS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpViewAction {
    Select { server: String, label: String },
    Cancel,
    None,
}

#[derive(Debug)]
pub struct McpView {
    rows: Vec<McpServerRow>,
    search: SearchInput,
    filtered: Vec<usize>,
    selected: usize,
    viewport_rows: usize,
    visible_items: usize,
    last_query: String,
}

impl McpView {
    pub fn from_response(data: &Value, viewport_rows: usize) -> Self {
        let rows: Vec<McpServerRow> = data
            .get("connections")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(McpServerRow::from_value)
                    .collect()
            })
            .unwrap_or_default();
        let mut view = McpView {
            rows,
            search: SearchInput::new(),
            filtered: Vec::new(),
            selected: 0,
            viewport_rows,
            visible_items: PREFERRED_VISIBLE_SERVICES,
            last_query: String::new(),
        };
        view.refilter();
        view
    }

    #[must_use]
    pub fn selected_server(&self) -> Option<&str> {
        self.rows
            .get(*self.filtered.get(self.selected)?)
            .map(|row| row.server.as_str())
    }

    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> McpViewAction {
        if key == "ctrl+c" {
            return McpViewAction::Cancel;
        }
        if kb.matches(key, "tui.select.up") {
            if !self.filtered.is_empty() {
                self.selected = self.selected.saturating_sub(1);
            }
            return McpViewAction::None;
        }
        if kb.matches(key, "tui.select.down") {
            if !self.filtered.is_empty() {
                self.selected = (self.selected + 1).min(self.filtered.len() - 1);
            }
            return McpViewAction::None;
        }
        if kb.matches(key, "tui.select.pageUp") || kb.matches(key, "tui.select.pageDown") {
            let direction = if kb.matches(key, "tui.select.pageUp") {
                -(self.visible_items as isize)
            } else {
                self.visible_items as isize
            };
            let count = self.filtered.len();
            if count > 0 {
                self.selected =
                    (self.selected as isize + direction).clamp(0, count as isize - 1) as usize;
            }
            return McpViewAction::None;
        }
        if kb.matches(key, "tui.select.confirm") {
            return self
                .filtered
                .get(self.selected)
                .and_then(|index| self.rows.get(*index))
                .map_or(McpViewAction::None, |row| McpViewAction::Select {
                    server: row.server.clone(),
                    label: row.label.clone(),
                });
        }
        if kb.matches(key, "tui.select.cancel")
            || (kb.matches(key, "app.modal.back") && self.search.cursor() == 0)
        {
            return McpViewAction::Cancel;
        }
        let previous = self.search.value().to_string();
        self.search.handle_key(key, kb);
        if self.search.value() != previous {
            self.refilter();
        }
        McpViewAction::None
    }

    pub fn set_search(&mut self, query: &str) {
        self.search.prefill(query);
        self.refilter();
    }

    pub fn paste(&mut self, text: &str) {
        let previous = self.search.value().to_string();
        self.search.paste(text);
        if self.search.value() != previous {
            self.refilter();
        }
    }

    pub fn render(&mut self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        self.visible_items = self.list_layout();

        let mut lines = search_field_lines(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            true,
            SEARCH_PLACEHOLDER,
        );

        let (start, end) = self.window();
        for index in start..end {
            let Some(&filtered_index) = self.filtered.get(index) else {
                continue;
            };
            let Some(row) = self.rows.get(filtered_index) else {
                continue;
            };
            let selected = index == self.selected;
            let primary: Line = vec![Span::raw(flatten_to_single_line(&row.label))];
            let status = row.status_text();
            let trailing = vec![(status.0, status.1.as_str())];
            lines.push(trailing_menu_row(
                theme, width, primary, &trailing, selected,
            ));
        }

        if self.visible_items > 0 && (start > 0 || end < self.filtered.len()) {
            let indicator = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            let line = vec![theme.fg_span(ThemeColor::Muted, indicator)];
            lines.push(crate::width::truncate_line(&line, width, ""));
        }

        if self.visible_items > 0 {
            if self.filtered.is_empty() {
                if self.visible_items >= EMPTY_STATE_ROWS {
                    let message = if self.rows.is_empty() {
                        "No MCP servers configured"
                    } else {
                        "No matching servers"
                    };
                    let line = vec![theme.fg_span(ThemeColor::Muted, format!("  {message}"))];
                    lines.push(crate::width::truncate_line(&line, width, ""));
                    lines.push(Vec::new());
                }
            } else if self.detail_rows() > 0 {
                lines.push(Vec::new());
                if let Some(row) = self
                    .filtered
                    .get(self.selected)
                    .and_then(|index| self.rows.get(*index))
                {
                    let text = flatten_to_single_line(&row.detail_text());
                    let line = vec![theme.fg_span(ThemeColor::Muted, format!(" {text}"))];
                    lines.push(crate::menu_panel::detail_row(theme, width, &line));
                }
            }
        }

        let action = self
            .filtered
            .get(self.selected)
            .and_then(|index| self.rows.get(*index))
            .map(McpServerRow::action_text);
        lines.push(hint_line(theme, width, kb, action));
        lines
    }

    fn list_layout(&self) -> usize {
        let reserved = SEARCH_FIELD_ROWS + HINT_ROWS + self.detail_rows();
        if self.viewport_rows <= reserved {
            return 0;
        }
        menu_list_layout(
            Some(self.viewport_rows),
            PREFERRED_VISIBLE_SERVICES,
            self.filtered.len(),
            reserved,
            SCROLL_INDICATOR_ROWS,
        )
    }

    fn detail_rows(&self) -> usize {
        if self.viewport_rows >= MIN_ROWS_FOR_DETAIL {
            DETAIL_ROWS + DETAIL_SPACER_ROWS
        } else {
            0
        }
    }

    fn window(&self) -> (usize, usize) {
        if self.visible_items == 0 {
            return (0, 0);
        }
        let max_visible = self.visible_items;
        let selected = self.selected.min(self.filtered.len().saturating_sub(1));
        let start = selected
            .saturating_sub(max_visible / 2)
            .min(self.filtered.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered.len());
        (start, end)
    }

    fn refilter(&mut self) {
        let query = self.search.value().to_string();
        let query_changed = query != self.last_query;
        self.last_query.clone_from(&query);
        let tokens: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.filtered = if tokens.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.matches(&tokens))
                .map(|(index, _)| index)
                .collect()
        };
        if query_changed {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        }
        self.visible_items = self.list_layout();
    }
}

fn trailing_menu_row(
    theme: &Theme,
    width: usize,
    primary: Line,
    trailing: &[(ThemeColor, &str)],
    selected: bool,
) -> Line {
    let inner_width = width.saturating_sub(2).max(1);
    let budget = inner_width.saturating_sub(5).max(1);
    let mut reduced: Vec<&(ThemeColor, &str)> = trailing
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .collect();
    let cluster = |segments: &[&(ThemeColor, &str)]| -> String {
        segments
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    };
    while reduced.len() > 1 && crate::width::str_width(&cluster(&reduced)) > budget {
        reduced.remove(0);
    }
    let mut trailing_spans: Line = if reduced.is_empty() {
        Vec::new()
    } else {
        let mut spans: Vec<Span> = Vec::with_capacity(reduced.len() * 2);
        for (index, (color, text)) in reduced.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" \u{b7} "));
            }
            spans.push(theme.fg_span(*color, *text));
        }
        spans
    };
    if !trailing_spans.is_empty() {
        trailing_spans = crate::width::truncate_line(&trailing_spans, budget, "\u{2026}");
    }
    let trailing_width = crate::width::spans_width(&trailing_spans);
    let gap = if trailing_width > 0 { 2 } else { 0 };
    let primary_width = inner_width.saturating_sub(trailing_width + gap).max(1);
    let mut primary = primary;
    if selected {
        primary = primary
            .into_iter()
            .map(|mut span| {
                span.style = span.style.add_modifier(ratatui::style::Modifier::BOLD);
                span
            })
            .collect();
    }
    let primary = crate::width::truncate_line(&primary, primary_width, "\u{2026}");
    let filler_width = inner_width
        .saturating_sub(crate::width::spans_width(&primary))
        .saturating_sub(trailing_width);
    let mut row: Line = Vec::with_capacity(primary.len() + trailing_spans.len() + 4);
    row.push(Span::raw(if selected { "\u{203a}" } else { " " }));
    row.push(Span::raw(" "));
    row.extend(primary);
    if filler_width > 0 {
        row.push(Span::raw(" ".repeat(filler_width)));
    }
    row.extend(trailing_spans);
    let mut row = crate::width::truncate_line(&row, width, "");
    let used = crate::width::spans_width(&row);
    if used < width {
        row.push(Span::raw(" ".repeat(width - used)));
    }
    if selected {
        let style = theme.soft_selection_style();
        row = row
            .into_iter()
            .map(|mut span| {
                span.style = span.style.patch(style);
                span
            })
            .collect();
    }
    row
}

fn hint_line(theme: &Theme, width: usize, kb: &KeybindingsManager, action: Option<&str>) -> Line {
    let select_key = kb
        .first_key("tui.select.confirm")
        .map_or_else(|| "Enter".to_string(), |key| format_key_text(&key));
    let close_key = kb
        .first_key("tui.select.cancel")
        .map_or_else(|| "Esc".to_string(), |key| format_key_text(&key));
    let action_segment = action
        .map(|action| format!("{select_key} {action} \u{b7} "))
        .unwrap_or_default();
    let hint = if width >= 70 {
        let navigation = format!(
            "{}/{} navigate \u{b7} ",
            kb.first_key("tui.select.up")
                .map_or_else(|| "\u{2191}".to_string(), |key| format_key_text(&key)),
            kb.first_key("tui.select.down")
                .map_or_else(|| "\u{2193}".to_string(), |key| format_key_text(&key))
        );
        format!("{navigation}{action_segment}{close_key} close")
    } else {
        format!("{action_segment}{close_key} close")
    };
    let line = vec![theme.fg_span(ThemeColor::Dim, format!(" {hint}"))];
    crate::width::truncate_line(&line, width, "")
}

#[cfg(test)]
mod tests;
