use std::path::{Path, PathBuf};
use std::{fmt, fs};

use super::mutations;
use crate::engines::graph::algorithms::layered::kernel::trace::{self, LayeredPhaseTrace};
use crate::graph::GeometryLevel;
use crate::graph::routing::trace::{self as route_trace, RoutingTrace};
use crate::simplification::PathSimplification;
use crate::{OutputFormat, RenderConfig, mmds, render_diagram};

#[derive(Debug)]
pub(crate) struct RenderedDiagram {
    pub(crate) source: String,
    pub(crate) text: String,
    pub(crate) svg: String,
    pub(crate) layout_mmds: mmds::Document,
    pub(crate) routed_mmds: mmds::Document,
    pub(crate) phase_trace: LayeredPhaseTrace,
    pub(crate) route_trace: RoutingTrace,
}

#[derive(Debug)]
pub(crate) struct RenderedPair {
    pub(crate) before: RenderedDiagram,
    pub(crate) after: RenderedDiagram,
}

#[derive(Debug)]
pub(crate) enum RenderSurfaceError {
    Source {
        pair_id: &'static str,
        side: &'static str,
        path: PathBuf,
        message: String,
    },
    Render {
        pair_id: &'static str,
        side: &'static str,
        surface: &'static str,
        message: String,
    },
    MmdsParse {
        pair_id: &'static str,
        side: &'static str,
        geometry_level: GeometryLevel,
        message: String,
    },
}

impl fmt::Display for RenderSurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderSurfaceError::Source {
                pair_id,
                side,
                path,
                message,
            } => write!(
                f,
                "failed to resolve {side} source for {pair_id} from {}: {message}",
                path.display()
            ),
            RenderSurfaceError::Render {
                pair_id,
                side,
                surface,
                message,
            } => write!(
                f,
                "failed to render {surface} surface for {pair_id} {side}: {message}"
            ),
            RenderSurfaceError::MmdsParse {
                pair_id,
                side,
                geometry_level,
                message,
            } => write!(
                f,
                "failed to parse {geometry_level} MMDS for {pair_id} {side}: {message}"
            ),
        }
    }
}

pub(crate) fn render_pair(
    pair: &mutations::MutationPair,
) -> Result<RenderedPair, RenderSurfaceError> {
    let before_source = resolve_input(pair.id, "before", pair.base)?;
    let after_source = resolve_input(pair.id, "after", pair.mutated)?;

    Ok(RenderedPair {
        before: render_input_with_context(pair.id, "before", &before_source)?,
        after: render_input_with_context(pair.id, "after", &after_source)?,
    })
}

pub(crate) fn render_lossless_routed_mmds(
    source: &str,
) -> Result<(String, mmds::Document), RenderSurfaceError> {
    render_mmds_with_simplification(
        "<direct>",
        "input",
        source,
        GeometryLevel::Routed,
        PathSimplification::Lossless,
    )
}

fn render_input_with_context(
    pair_id: &'static str,
    side: &'static str,
    source: &str,
) -> Result<RenderedDiagram, RenderSurfaceError> {
    let text = render_surface(
        pair_id,
        side,
        "text",
        source,
        OutputFormat::Text,
        &default_config(),
    )?;
    let svg = render_surface(
        pair_id,
        side,
        "svg",
        source,
        OutputFormat::Svg,
        &default_config(),
    )?;
    let (_, layout_mmds) = render_mmds_with_simplification(
        pair_id,
        side,
        source,
        GeometryLevel::Layout,
        PathSimplification::None,
    )?;
    trace::begin_capture();
    route_trace::begin_capture();
    let routed_result = render_mmds_with_simplification(
        pair_id,
        side,
        source,
        GeometryLevel::Routed,
        PathSimplification::None,
    );
    let phase_trace = trace::finish_capture();
    let route_trace = route_trace::finish_capture();
    let (_, routed_mmds) = routed_result?;

    Ok(RenderedDiagram {
        source: source.to_string(),
        text,
        svg,
        layout_mmds,
        routed_mmds,
        phase_trace,
        route_trace,
    })
}

fn render_mmds_with_simplification(
    pair_id: &'static str,
    side: &'static str,
    source: &str,
    geometry_level: GeometryLevel,
    path_simplification: PathSimplification,
) -> Result<(String, mmds::Document), RenderSurfaceError> {
    let config = RenderConfig {
        geometry_level,
        path_simplification,
        ..RenderConfig::default()
    };
    let json = render_surface(pair_id, side, "mmds", source, OutputFormat::Mmds, &config)?;
    let output = mmds::parse_input(&json).map_err(|error| RenderSurfaceError::MmdsParse {
        pair_id,
        side,
        geometry_level,
        message: error.to_string(),
    })?;

    Ok((json, output))
}

fn render_surface(
    pair_id: &'static str,
    side: &'static str,
    surface: &'static str,
    source: &str,
    format: OutputFormat,
    config: &RenderConfig,
) -> Result<String, RenderSurfaceError> {
    render_diagram(source, format, config).map_err(|error| RenderSurfaceError::Render {
        pair_id,
        side,
        surface,
        message: error.to_string(),
    })
}

fn resolve_input(
    pair_id: &'static str,
    side: &'static str,
    input: mutations::MutationInput,
) -> Result<String, RenderSurfaceError> {
    match input {
        mutations::MutationInput::Inline(source) => Ok(source.to_string()),
        mutations::MutationInput::Fixture { family, name } => {
            let path = fixture_path(family, name);
            fs::read_to_string(&path).map_err(|error| RenderSurfaceError::Source {
                pair_id,
                side,
                path,
                message: error.to_string(),
            })
        }
    }
}

fn fixture_path(family: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(family)
        .join(name)
}

fn default_config() -> RenderConfig {
    RenderConfig::default()
}
