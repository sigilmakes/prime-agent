//! The restored branch-summary card (TS `BranchSummaryMessageComponent`): the custom-message
//! box, a clickable `[branch]` label, and either the collapsed label or the full markdown summary.

use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::{Line, Span};

/// Render the branch card inside its one-column, one-row padded custom-message box.
pub(crate) fn render_branch_summary(
    summary: &str,
    expanded: bool,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let header = vec![crate::custom_message::render::custom_message_label(
        "branch", theme,
    )];
    let mut rows = vec![Vec::new()];
    rows.extend(crate::width::wrap_line(&header, content_width));
    rows.push(Vec::new());
    if expanded {
        rows.extend(crate::markdown::render_markdown(
            &format!("**Branch Summary**\n\n{summary}"),
            content_width,
            &markdown_style(theme),
        ));
    } else {
        // TS expandCollapseHint("app.tools.expand", false) returns the empty string: the
        // conversation dock owns the effective expand-key hint, not each individual card.
        rows.extend(crate::width::wrap_line(
            &vec![Span::styled(
                "Branch summary",
                theme.fg_style(ThemeColor::CustomMessageText),
            )],
            content_width,
        ));
    }
    rows.push(Vec::new());
    let background = theme.bg_style(ThemeBg::CustomMessageBg);
    rows.into_iter()
        .map(|row| {
            let mut padded = vec![Span::styled(" ", background)];
            padded.extend(
                row.into_iter()
                    .map(|span| Span::styled(span.content, span.style.patch(background))),
            );
            let clipped = crate::width::truncate_line(&padded, width, "");
            crate::chat::pad_to(clipped, width, background)
        })
        .collect()
}

pub(crate) fn count_branch_summary(
    summary: &str,
    expanded: bool,
    theme: &Theme,
    width: usize,
) -> usize {
    let content_width = width.saturating_sub(2).max(1);
    // Top padding, label, spacer, body, bottom padding. The label/body wrap independently.
    3 + crate::width::wrapped_text_count("[branch]", content_width)
        + if expanded {
            crate::markdown::markdown_row_count(
                &format!("**Branch Summary**\n\n{summary}"),
                content_width,
                &markdown_style(theme),
            )
        } else {
            crate::width::wrapped_text_count("Branch summary", content_width)
        }
}

fn markdown_style(theme: &Theme) -> crate::markdown::MarkdownStyle {
    let mut style = crate::markdown::MarkdownStyle::from_theme(theme);
    style.body = theme.fg_style(ThemeColor::CustomMessageText);
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn text(rows: &[Line]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn branch_summary_matches_the_ts_collapsed_and_expanded_card() {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let collapsed = render_branch_summary("restored **branch context**", false, &theme, 60);
        assert_eq!(
            text(&collapsed),
            ["", " [branch]", "", " Branch summary", ""]
        );
        let expanded = render_branch_summary("restored **branch context**", true, &theme, 60);
        let rendered = text(&expanded).join("\n");
        assert!(
            rendered.contains(" [branch]\n\n Branch Summary\n\n restored branch context"),
            "{rendered}"
        );
        assert!(!rendered.contains("**"));
        assert!(expanded
            .iter()
            .flatten()
            .all(|span| span.style.bg == theme.bg_color(ThemeBg::CustomMessageBg)));
    }

    #[test]
    fn branch_summary_geometry_matches_paint_at_narrow_and_wrapped_widths() {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        for summary in [
            "",
            "wide 界 emoji 🙂",
            "## Heading\nbody\n\n- item",
            "```rust\nfn main() {}\n```",
            "one two ".repeat(100).as_str(),
        ] {
            for expanded in [false, true] {
                for width in 0..80 {
                    let rows = render_branch_summary(summary, expanded, &theme, width);
                    assert_eq!(
                        rows.len(),
                        count_branch_summary(summary, expanded, &theme, width),
                        "width={width}, summary={summary:?}, expanded={expanded}"
                    );
                    assert!(rows
                        .iter()
                        .all(|row| crate::width::line_width(row) <= width));
                }
            }
        }
    }
}
