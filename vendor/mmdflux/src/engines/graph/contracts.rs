//! Low-level graph-family engine contracts.
//!
//! Consumer-facing config, format, family, and error definitions live in the
//! crate's flat top-level public contract modules. This module keeps the
//! engine-side solve contracts and provides a focused import surface for
//! callers that manage graph-family solves directly.

use super::{EngineAlgorithmCapabilities, EngineAlgorithmId, LayoutConfig};
use crate::errors::RenderError;
use crate::format::RoutingStyle;
use crate::graph::GeometryLevel;
use crate::graph::measure::TextMetricsProvider;

/// Measurement mode controls whether layout uses grid-cell dimensions or
/// proportional float-space dimensions for node sizing.
#[derive(Clone, Copy)]
pub enum MeasurementMode<'a> {
    /// Grid-cell dimensions for discrete grid replay.
    Grid,
    /// Proportional dimensions for unitless float-space geometry.
    Proportional(&'a dyn TextMetricsProvider),
}

impl std::fmt::Debug for MeasurementMode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Grid => f.write_str("Grid"),
            // Keep provider details opaque so `TextMetricsProvider` does not
            // need a `Debug` super-trait or extra object-safety constraints.
            Self::Proportional(_) => f.write_str("Proportional(..)"),
        }
    }
}

/// Engine-specific configuration envelope.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineConfig {
    /// Layered (Sugiyama) layout engine configuration.
    Layered(crate::engines::graph::algorithms::layered::LayoutConfig),
}

impl From<LayoutConfig> for EngineConfig {
    fn from(config: LayoutConfig) -> Self {
        EngineConfig::Layered(config.into())
    }
}

/// How the engine should handle subgraph directions that are not explicitly set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubgraphDirectionPolicy {
    /// Alternate subgraph direction axes (TD↔LR). Matches Mermaid flowchart behavior.
    #[default]
    AlternateAxes,
    /// Preserve declared directions; no automatic alternation.
    Preserve,
}

/// Request parameters for a `GraphEngine::solve()` call.
#[derive(Debug, Clone)]
pub struct GraphSolveRequest<'a> {
    /// Measurement model used for node and edge label sizing.
    pub measurement_mode: MeasurementMode<'a>,
    /// Float-geometry contract requested by the caller.
    pub geometry_contract: GraphGeometryContract,
    /// Geometry detail level requested by the caller.
    pub geometry_level: GeometryLevel,
    /// Routing style requested by the caller (after preset resolution).
    pub routing_style: Option<RoutingStyle>,
    /// How the engine should handle implicit subgraph directions.
    pub subgraph_direction_policy: SubgraphDirectionPolicy,
}

/// Float-geometry contract requested from the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphGeometryContract {
    /// Plain float geometry for downstream routing/export.
    Canonical,
    /// Float geometry tuned for direct visual emission.
    Visual,
}

impl<'a> GraphSolveRequest<'a> {
    /// Build a solve request from explicit engine-owned solve instructions.
    pub fn new(
        measurement_mode: MeasurementMode<'a>,
        geometry_contract: GraphGeometryContract,
        geometry_level: GeometryLevel,
        routing_style: Option<RoutingStyle>,
        subgraph_direction_policy: SubgraphDirectionPolicy,
    ) -> Self {
        Self {
            measurement_mode,
            geometry_contract,
            geometry_level,
            routing_style,
            subgraph_direction_policy,
        }
    }
}

/// Result of a `GraphEngine::solve()` call.
pub struct GraphSolveResult {
    /// Which engine+algorithm produced this result.
    pub engine_id: EngineAlgorithmId,
    /// Positioned node and edge geometry.
    pub geometry: crate::graph::geometry::GraphGeometry,
    /// Routed edge paths (present when engine routes natively and routed level requested).
    pub routed: Option<crate::graph::geometry::RoutedGraphGeometry>,
}

/// Unified graph engine trait combining layout and optional routing.
pub trait GraphEngine: Send + Sync {
    /// Combined engine+algorithm identifier.
    fn id(&self) -> EngineAlgorithmId;

    /// Capabilities this engine+algorithm provides.
    #[allow(dead_code)]
    fn capabilities(&self) -> EngineAlgorithmCapabilities {
        self.id().capabilities()
    }

    /// Solve: layout and optionally route the diagram.
    fn solve(
        &self,
        diagram: &crate::graph::Graph,
        config: &EngineConfig,
        request: &GraphSolveRequest<'_>,
    ) -> Result<GraphSolveResult, RenderError>;
}
