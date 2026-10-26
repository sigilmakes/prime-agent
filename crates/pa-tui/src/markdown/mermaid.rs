//! Mermaid fenced blocks drawn as Unicode diagrams.
//!
//! Replaces the TS renderer (`packages/coding-agent/src/modes/interactive/components/mermaid.ts`,
//! which wrapped the `grok-mermaid` npm package) with `mmdflux`: pure Rust, no browser and
//! no Node, with a terminal text renderer as a first-class output. Flowchart, sequence,
//! class, and state diagrams render; anything else falls back to the fenced source, which
//! is also the fallback when the art is wider than the transcript.

use super::MarkdownStyle;
use crate::width::line_width;
use crate::{Line, Span};
use pa_types::markdown::MermaidRenderingMode;

/// Whether a block whose message is `streaming` or settled draws as a diagram.
#[must_use]
pub(crate) fn renders(mode: MermaidRenderingMode, streaming: bool) -> bool {
    match mode {
        MermaidRenderingMode::Off => false,
        MermaidRenderingMode::Final => !streaming,
        MermaidRenderingMode::Streaming => true,
    }
}

/// The fence info string names mermaid (TS matched its first word, case-insensitively).
pub(super) fn is_mermaid(lang: &str) -> bool {
    lang.split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("mermaid"))
}

/// The largest source drawn. Past it the layout cost (tens of milliseconds for a few hundred
/// nodes) would stall a streaming frame, and the fenced source is the honest fallback: the
/// input is model-authored and unbounded.
const SOURCE_LIMIT: usize = 8 * 1024;

thread_local! {
    /// The last laid-out diagram: source text and its plain art. The paint and the row count
    /// both ask for the uncached final block on the same frame, and repaints between tokens
    /// reuse it; layout does not depend on width, so the source alone keys the entry, while
    /// styling stays per call (the dim thinking palette differs from the chat text palette).
    static LAST_ART: std::cell::RefCell<Option<(String, Option<String>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The block's diagram rows, or `None` when the source does not render (unknown diagram
/// type, syntax error, glyphs whose drawn width disagrees with the transcript's) or the art
/// would not fit `width` — the caller keeps the fenced source, matching every TS fallback.
pub(super) fn rows(source: &str, width: usize, style: &MarkdownStyle) -> Option<Vec<Line>> {
    if source.trim().is_empty() || source.len() > SOURCE_LIMIT {
        return None;
    }
    let art = art(source)?;
    // A diagram that is still only its header lays out as nothing; the source is the honest
    // row until the first node arrives.
    if art.trim().is_empty() || !aligned(&art) {
        return None;
    }
    let rows: Vec<Line> = art.lines().map(|line| styled_row(line, style)).collect();
    rows.iter()
        .all(|row| line_width(row) <= width)
        .then_some(rows)
}

/// The plain art for `source`, replayed from the previous call when the source is unchanged.
fn art(source: &str) -> Option<String> {
    LAST_ART.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((cached, art)) = slot.as_ref() {
            if cached == source {
                return art.clone();
            }
        }
        let art = mmdflux::render_diagram(
            source,
            mmdflux::OutputFormat::Text,
            &mmdflux::RenderConfig::default(),
        )
        .ok();
        *slot = Some((source.to_string(), art.clone()));
        art
    })
}

/// Whether the drawn rows agree with the transcript about their width. `mmdflux` lays a label
/// out by summing per-character advances, so a ZWJ sequence, keycap, or combining mark inside
/// one counts twice there and once here; such a canvas closes its border inside the glyph.
/// Falling back to the source beats drawing a broken diagram.
fn aligned(art: &str) -> bool {
    art.lines().all(|line| {
        let line = line.trim_end();
        crate::width::str_width(line)
            == line
                .chars()
                .map(mmdflux::format::char_display_width)
                .sum::<usize>()
    })
}

/// One art row: runs of drawing glyphs take the border slot, labels take the code slot.
fn styled_row(line: &str, style: &MarkdownStyle) -> Line {
    let mut spans: Line = Vec::new();
    let mut run = String::new();
    let mut run_is_frame = false;
    for ch in line.trim_end().chars() {
        let is_frame = is_frame_char(ch);
        if !run.is_empty() && is_frame != run_is_frame {
            spans.push(run_span(std::mem::take(&mut run), run_is_frame, style));
        }
        run_is_frame = is_frame;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(run_span(run, run_is_frame, style));
    }
    spans
}

fn run_span(text: String, is_frame: bool, style: &MarkdownStyle) -> Span {
    Span::styled(
        text,
        if is_frame {
            style.code_block_border
        } else {
            style.code_block
        },
    )
}

/// The drawing glyphs `mmdflux` emits in its Unicode charset (`render/text/chars.rs`): the
/// box-drawing block, plus the arrows and node markers outside it. `<` and `>` are its
/// diamond-node sides and horizontal arrow terminals, so a literal `<` or `>` inside a label
/// takes the border style too; telling them apart would need column tracking the renderer
/// does not expose.
fn is_frame_char(ch: char) -> bool {
    (0x2500..=0x257f).contains(&u32::from(ch))
        || matches!(
            ch,
            '▲' | '▼'
                | '◄'
                | '►'
                | '△'
                | '▽'
                | '◁'
                | '▷'
                | '◆'
                | '◇'
                | '●'
                | '◉'
                | '⊗'
                | '<'
                | '>'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_gates_rendering_by_message_state() {
        use MermaidRenderingMode::{Final, Off, Streaming};
        assert!(!renders(Off, false));
        assert!(!renders(Off, true));
        assert!(!renders(Final, true));
        assert!(renders(Final, false));
        assert!(renders(Streaming, true));
        assert!(renders(Streaming, false));
    }

    #[test]
    fn only_the_first_info_word_names_mermaid() {
        assert!(is_mermaid("mermaid"));
        assert!(is_mermaid("Mermaid title=x"));
        assert!(!is_mermaid("mermaidx"));
        assert!(!is_mermaid("text mermaid"));
        assert!(!is_mermaid(""));
    }

    #[test]
    fn flowchart_renders_and_oversized_art_falls_back() {
        let style = MarkdownStyle::default();
        let source = "flowchart LR\n    A[Build] --> B[Test]";
        let art = rows(source, 80, &style).expect("flowchart renders");
        assert!(art.len() > 1);
        assert!(crate::width::line_width(&art[0]) > 0);
        assert!(rows(source, 5, &style).is_none());
    }

    #[test]
    fn a_header_only_diagram_keeps_the_source() {
        let style = MarkdownStyle::default();
        for source in ["sequenceDiagram", "classDiagram", "stateDiagram-v2"] {
            assert!(rows(source, 80, &style).is_none(), "{source}");
        }
    }

    #[test]
    fn unsupported_diagram_keeps_the_source() {
        let style = MarkdownStyle::default();
        assert!(rows("pie title Pets\n    \"Dogs\" : 2", 80, &style).is_none());
        assert!(rows("not a diagram", 80, &style).is_none());
        assert!(rows("   ", 80, &style).is_none());
    }

    #[test]
    fn frame_glyphs_take_the_border_style_and_labels_the_code_style() {
        let style = MarkdownStyle::default();
        assert_eq!(
            styled_row("┌──────┐", &style),
            vec![Span::styled(
                "┌──────┐".to_string(),
                style.code_block_border
            )]
        );
        assert_eq!(
            styled_row("│ Build │", &style),
            vec![
                Span::styled("│".to_string(), style.code_block_border),
                Span::styled(" Build ".to_string(), style.code_block),
                Span::styled("│".to_string(), style.code_block_border),
            ]
        );
    }

    fn flat(rows: &[crate::Line]) -> String {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn edge_glyphs_take_the_border_style() {
        let style = MarkdownStyle {
            mermaid: true,
            ..MarkdownStyle::default()
        };
        let art = rows("flowchart LR\n    A[One] --> B[Two]", 80, &style).expect("LR flowchart");
        // Flowcharts draw `►`, sequence messages `>`; both are drawing glyphs.
        let edge = art
            .iter()
            .find(|row| {
                row.iter().any(|span| {
                    span.style == style.code_block_border
                        && span.content.chars().any(|ch| matches!(ch, '►' | '>'))
                })
            })
            .unwrap_or_else(|| panic!("no arrow row: {}", flat(&art)));
        assert!(
            edge.iter().all(|span| span.style == style.code_block_border
                || !span.content.chars().any(is_frame_char)),
            "label text keeps the code style, drawing glyphs the border style: {edge:?}"
        );
    }

    #[test]
    fn mermaid_fences_draw_as_diagrams_only_when_the_style_allows() {
        use crate::markdown::render_markdown;
        let source = "```mermaid\nflowchart LR\n    A[Build] --> B[Test]\n```";
        let drawn = flat(&render_markdown(
            source,
            80,
            &MarkdownStyle {
                mermaid: true,
                ..MarkdownStyle::default()
            },
        ));
        assert!(drawn.contains('\u{250c}'), "drawn: {drawn:?}");
        assert!(!drawn.contains("flowchart LR"), "drawn: {drawn:?}");

        let raw = flat(&render_markdown(source, 80, &MarkdownStyle::default()));
        assert!(raw.contains("flowchart LR"), "raw: {raw:?}");
        assert!(!raw.contains('\u{250c}'), "raw: {raw:?}");
    }

    #[test]
    fn mermaid_diagrams_count_exactly_what_they_paint() {
        use crate::markdown::{markdown_row_count, render_markdown};
        let style = MarkdownStyle {
            mermaid: true,
            ..MarkdownStyle::default()
        };
        let text = "intro\n\n```mermaid\nflowchart TD\n    A[File] --> B{Size?}\n    B -->|no| C[Stream]\n```\n\nafter";
        for width in 1..90 {
            assert_eq!(
                markdown_row_count(text, width, &style),
                render_markdown(text, width, &style).len(),
                "count==paint at width {width}"
            );
        }
    }

    #[test]
    fn mermaid_art_wider_than_the_transcript_keeps_the_source() {
        use crate::markdown::render_markdown;
        let style = MarkdownStyle {
            mermaid: true,
            ..MarkdownStyle::default()
        };
        let text = "```mermaid\nflowchart LR\n    A[Build] --> B[Test]\n```";
        let narrow = flat(&render_markdown(text, 8, &style));
        // The source survives (wrapped at this width); no art is drawn.
        assert!(narrow.contains("Build"), "narrow: {narrow:?}");
        assert!(!narrow.contains('\u{250c}'), "narrow: {narrow:?}");
    }

    #[test]
    fn mermaid_rows_never_cross_between_cache_entries_of_different_modes() {
        use crate::markdown::{render_markdown_tagged, MarkdownBlockCache};
        let mut cache = MarkdownBlockCache::default();
        // A trailing paragraph closes the fence, making the block cacheable.
        let text = "```mermaid\nflowchart LR\n    A[Build] --> B[Test]\n```\n\ntail";
        let drawn = render_markdown_tagged(
            text,
            80,
            &MarkdownStyle {
                mermaid: true,
                ..MarkdownStyle::default()
            },
            "",
            &mut cache,
        );
        let raw = render_markdown_tagged(text, 80, &MarkdownStyle::default(), "", &mut cache);
        assert!(flat(&drawn).contains('\u{250c}'));
        assert!(flat(&raw).contains("flowchart LR"));
    }

    #[test]
    fn a_source_past_the_limit_keeps_the_fenced_source() {
        use std::fmt::Write as _;
        let style = MarkdownStyle::default();
        let mut source = String::from("flowchart LR\n");
        for i in 0..400 {
            let _ = writeln!(source, "    A{i}[node {i}] --> A{}", i + 1);
        }
        assert!(source.len() > SOURCE_LIMIT);
        assert!(rows(&source, 400, &style).is_none());
    }

    #[test]
    fn labels_whose_drawn_width_disagrees_with_the_transcript_fall_back() {
        let style = MarkdownStyle::default();
        // mmdflux counts per scalar, the transcript per grapheme cluster: these labels would
        // close the border inside the glyph.
        for label in ["👨‍👩‍👧", "1️⃣", "e\u{301}"] {
            let source = format!("flowchart LR\n    A[{label}] --> B[Test]");
            assert!(rows(&source, 80, &style).is_none(), "{label:?}");
        }
        // Grapheme-cluster width and per-scalar width agree for these.
        for label in ["数据库", "🇨🇦", "🚀", "👍🏽", "Build"] {
            let source = format!("flowchart LR\n    A[{label}] --> B[Test]");
            assert!(rows(&source, 80, &style).is_some(), "{label:?}");
        }
    }

    #[test]
    fn repeated_renders_replay_the_art_under_each_callers_style() {
        let source = "flowchart LR\n    A[Build] --> B[Test]";
        let text = styled_art(source, &MarkdownStyle::default());
        let dim = styled_art(
            source,
            &MarkdownStyle {
                code_block: ratatui::style::Style::default(),
                code_block_border: ratatui::style::Style::default(),
                ..MarkdownStyle::default()
            },
        );
        assert_eq!(
            flat(&text),
            flat(&dim),
            "the memo holds raw art, not styled rows"
        );
        assert_ne!(text[0][0].style, dim[0][0].style);
    }

    fn styled_art(source: &str, style: &MarkdownStyle) -> Vec<crate::Line> {
        let mut style = style.clone();
        style.mermaid = true;
        rows(source, 80, &style).expect("art")
    }
}
