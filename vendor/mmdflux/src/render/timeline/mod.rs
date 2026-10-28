//! Timeline-family rendering (sequence diagrams).

pub mod svg;
pub mod svg_layout;
pub mod text;

use crate::graph::measure::ProportionalTextMetrics;
use crate::render::svg::theme::ResolvedSvgTheme;
use crate::render::text::CharSet;
use crate::timeline::sequence::layout::SequenceLayout;
use crate::timeline::sequence::model::Sequence;

pub fn render(layout: &SequenceLayout, charset: &CharSet) -> String {
    text::render(layout, charset)
}

pub fn render_svg(
    model: &Sequence,
    metrics: &ProportionalTextMetrics,
    font_family: &str,
    theme: Option<&ResolvedSvgTheme>,
) -> String {
    let layout = svg_layout::layout(model, metrics, font_family);
    svg::render(&layout, theme)
}
