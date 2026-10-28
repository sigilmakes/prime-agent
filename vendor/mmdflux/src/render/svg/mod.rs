//! Shared SVG writing utilities used by both graph and sequence renderers.

pub(crate) mod theme;

use self::theme::SvgRootStyle;

pub(crate) fn fmt_f64(value: f64) -> String {
    let mut v = value;
    if v.abs() < 0.005 {
        v = 0.0;
    }
    format!("{:.2}", v)
}

pub(crate) fn escape_text(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) struct SvgWriter {
    buf: String,
    indent: usize,
}

impl SvgWriter {
    pub(crate) fn new() -> Self {
        Self {
            buf: String::new(),
            indent: 0,
        }
    }

    pub(crate) fn start_svg_with_root_style(
        &mut self,
        width: f64,
        height: f64,
        font_family: &str,
        font_size: f64,
        root_style: &SvgRootStyle,
    ) {
        let view_width = fmt_f64(width);
        let view_height = fmt_f64(height);
        let view_box = format!("0 0 {view_width} {view_height}");
        let mut style_parts = vec![
            format!("max-width: {view_width}px"),
            format!(
                "background-color: {}",
                root_style
                    .background_color
                    .as_deref()
                    .unwrap_or("transparent")
            ),
        ];
        style_parts.extend(
            root_style
                .css_variables
                .iter()
                .map(|(name, value)| format!("{name}:{value}")),
        );
        let style = format!("{};", style_parts.join("; "));
        let line = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100%\" viewBox=\"{view_box}\" style=\"{style}\" font-family=\"{font}\" font-size=\"{font_size}\">",
            view_box = view_box,
            style = style,
            font = escape_text(font_family),
            font_size = fmt_f64(font_size)
        );
        self.push_line(&line);
        self.indent += 1;

        if let Some(style_block) = &root_style.style_block {
            self.start_tag("<style>");
            for css_line in style_block.lines() {
                self.push_line(css_line);
            }
            self.end_tag("</style>");
        }
    }

    pub(crate) fn end_svg(&mut self) {
        self.indent = self.indent.saturating_sub(1);
        self.push_line("</svg>");
    }

    pub(crate) fn start_tag(&mut self, line: &str) {
        self.push_line(line);
        self.indent += 1;
    }

    pub(crate) fn end_tag(&mut self, line: &str) {
        self.indent = self.indent.saturating_sub(1);
        self.push_line(line);
    }

    pub(crate) fn start_group(&mut self, class_name: &str) {
        let line = format!("<g class=\"{class}\">", class = escape_text(class_name));
        self.start_tag(&line);
    }

    pub(crate) fn start_group_transform(&mut self, dx: f64, dy: f64) {
        let line = format!(
            "<g transform=\"translate({x},{y})\">",
            x = fmt_f64(dx),
            y = fmt_f64(dy)
        );
        self.start_tag(&line);
    }

    pub(crate) fn end_group(&mut self) {
        self.end_tag("</g>");
    }

    pub(crate) fn push_line(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.buf.push_str("  ");
        }
        self.buf.push_str(line);
        self.buf.push('\n');
    }

    pub(crate) fn finish(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::theme::SvgRootStyle;
    use super::{SvgWriter, escape_text, fmt_f64};

    #[test]
    fn fmt_f64_snaps_tiny_values_to_zero() {
        assert_eq!(fmt_f64(0.004), "0.00");
        assert_eq!(fmt_f64(-0.004), "0.00");
        assert_eq!(fmt_f64(12.345), "12.35");
    }

    #[test]
    fn escape_text_escapes_xml_significant_characters() {
        assert_eq!(
            escape_text("<tag attr=\"a&b\">it's</tag>"),
            "&lt;tag attr=&quot;a&amp;b&quot;&gt;it&apos;s&lt;/tag&gt;"
        );
    }

    #[test]
    fn start_svg_with_root_style_emits_background_variables_and_style_block() {
        let mut writer = SvgWriter::new();
        writer.start_svg_with_root_style(
            120.0,
            60.0,
            "Inter",
            16.0,
            &SvgRootStyle {
                background_color: Some("#ffffff".into()),
                css_variables: vec![
                    ("--bg".into(), "#ffffff".into()),
                    ("--fg".into(), "#27272a".into()),
                    ("--line".into(), "#939395".into()),
                ],
                style_block: Some("svg { --_text: var(--fg); }".into()),
            },
        );
        writer.end_svg();

        let output = writer.finish();

        assert!(output.contains("background-color: #ffffff;"));
        assert!(output.contains("--bg:#ffffff"));
        assert!(output.contains("--fg:#27272a"));
        assert!(output.contains("--line:#939395"));
        assert!(output.contains("<style>"));
        assert!(output.contains("svg { --_text: var(--fg); }"));
    }
}
