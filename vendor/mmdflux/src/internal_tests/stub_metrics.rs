//! Shared text-metrics providers for crate-local canaries.

use crate::graph::measure::{ProportionalTextMetrics, TextMetricsProvider};

macro_rules! impl_stub_provider {
    (
        $provider:ty,
        m_width: $m_width:expr,
        other_width: $other_width:expr,
        label_padding_x: $label_padding_x:expr,
        label_padding_y: $label_padding_y:expr
    ) => {
        impl TextMetricsProvider for $provider {
            fn measure_line_width(&self, text: &str) -> f64 {
                text.chars().map(|ch| self.measure_scalar_width(ch)).sum()
            }

            fn measure_scalar_width(&self, ch: char) -> f64 {
                if ch == 'm' { $m_width } else { $other_width }
            }

            fn font_size(&self) -> f64 {
                16.0
            }

            fn line_height(&self) -> f64 {
                20.0
            }

            fn node_padding_x(&self) -> f64 {
                10.0
            }

            fn node_padding_y(&self) -> f64 {
                6.0
            }

            fn label_padding_x(&self) -> f64 {
                $label_padding_x
            }

            fn label_padding_y(&self) -> f64 {
                $label_padding_y
            }
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedWidthProvider;

impl_stub_provider!(
    FixedWidthProvider,
    m_width: 30.0,
    other_width: 5.0,
    label_padding_x: 4.0,
    label_padding_y: 2.0
);

#[derive(Debug, Clone, Copy)]
pub(crate) struct WideMProvider;

impl_stub_provider!(
    WideMProvider,
    m_width: 40.0,
    other_width: 5.0,
    label_padding_x: 4.0,
    label_padding_y: 2.0
);

#[derive(Debug, Clone, Copy)]
pub(crate) struct PaddedProvider;

impl_stub_provider!(
    PaddedProvider,
    m_width: 40.0,
    other_width: 5.0,
    label_padding_x: 11.0,
    label_padding_y: 7.0
);

pub(crate) struct NonCloneProvider(ProportionalTextMetrics);

impl NonCloneProvider {
    pub(crate) fn new(metrics: ProportionalTextMetrics) -> Self {
        Self(metrics)
    }
}

impl TextMetricsProvider for NonCloneProvider {
    fn measure_line_width(&self, text: &str) -> f64 {
        ProportionalTextMetrics::measure_line_width(&self.0, text)
    }

    fn measure_scalar_width(&self, ch: char) -> f64 {
        ProportionalTextMetrics::measure_scalar_width(&self.0, ch)
    }

    fn font_size(&self) -> f64 {
        self.0.font_size
    }

    fn line_height(&self) -> f64 {
        self.0.line_height
    }

    fn node_padding_x(&self) -> f64 {
        self.0.node_padding_x
    }

    fn node_padding_y(&self) -> f64 {
        self.0.node_padding_y
    }

    fn label_padding_x(&self) -> f64 {
        self.0.label_padding_x
    }

    fn label_padding_y(&self) -> f64 {
        self.0.label_padding_y
    }
}
