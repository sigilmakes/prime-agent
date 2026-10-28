#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderChurnClass {
    NoMeaningfulChange,
    BoundsOnly,
    StyleOnly,
    RouteTopologyChanged,
    LabelRectChanged,
    EndpointFaceChanged,
    TextGridQuantizationOrGlyphChurn,
    Mixed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RenderMetricDelta {
    pub(crate) svg_viewbox_changed: bool,
    pub(crate) svg_path_count_changed: bool,
    pub(crate) path_topology_changed: bool,
    pub(crate) label_rect_changed: bool,
    pub(crate) endpoint_face_changed: bool,
    /// Style-only canaries should use default/lossless routed MMDS when this
    /// boolean comes from route churn, not the diagnostic `None` surface.
    pub(crate) style_only_changed: bool,
    pub(crate) text_dimensions_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextSurfaceMetrics {
    pub(crate) line_count: usize,
    pub(crate) max_line_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SvgSurfaceMetrics {
    pub(crate) viewbox_width: Option<f64>,
    pub(crate) viewbox_height: Option<f64>,
    pub(crate) path_count: usize,
}

pub(crate) fn classify_render_churn(delta: &RenderMetricDelta) -> RenderChurnClass {
    let route_changed = delta.path_topology_changed || delta.svg_path_count_changed;
    let real_change_count = [
        route_changed,
        delta.label_rect_changed,
        delta.endpoint_face_changed,
        delta.style_only_changed,
        delta.text_dimensions_changed,
    ]
    .into_iter()
    .filter(|changed| *changed)
    .count();

    if real_change_count > 1 {
        return RenderChurnClass::Mixed;
    }
    if route_changed {
        return RenderChurnClass::RouteTopologyChanged;
    }
    if delta.label_rect_changed {
        return RenderChurnClass::LabelRectChanged;
    }
    if delta.endpoint_face_changed {
        return RenderChurnClass::EndpointFaceChanged;
    }
    if delta.style_only_changed {
        return RenderChurnClass::StyleOnly;
    }
    if delta.text_dimensions_changed {
        return RenderChurnClass::TextGridQuantizationOrGlyphChurn;
    }
    if delta.svg_viewbox_changed {
        return RenderChurnClass::BoundsOnly;
    }

    RenderChurnClass::NoMeaningfulChange
}

pub(crate) fn collect_text_metrics(text: &str) -> TextSurfaceMetrics {
    let mut line_count = 0;
    let mut max_line_width = 0;
    for line in text.lines() {
        line_count += 1;
        max_line_width = max_line_width.max(line.chars().count());
    }

    TextSurfaceMetrics {
        line_count,
        max_line_width,
    }
}

pub(crate) fn collect_svg_metrics(svg: &str) -> SvgSurfaceMetrics {
    let (viewbox_width, viewbox_height) = parse_viewbox_dimensions(svg).unwrap_or((None, None));

    SvgSurfaceMetrics {
        viewbox_width,
        viewbox_height,
        path_count: svg.matches("<path").count(),
    }
}

fn parse_viewbox_dimensions(svg: &str) -> Option<(Option<f64>, Option<f64>)> {
    let value = attribute_value(svg, "viewBox")?;
    let parts = value
        .split_ascii_whitespace()
        .filter_map(|part| part.parse::<f64>().ok())
        .collect::<Vec<_>>();
    if parts.len() == 4 {
        Some((Some(parts[2]), Some(parts[3])))
    } else {
        Some((None, None))
    }
}

fn attribute_value<'a>(input: &'a str, name: &str) -> Option<&'a str> {
    let start = input.find(name)?;
    let after_name = &input[start + name.len()..];
    let equals = after_name.find('=')?;
    let after_equals = after_name[equals + 1..].trim_start();
    let quote = after_equals.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value_start = quote.len_utf8();
    let value_end = after_equals[value_start..].find(quote)?;
    Some(&after_equals[value_start..value_start + value_end])
}
