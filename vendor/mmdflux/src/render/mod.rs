//! Output production namespaces.
//!
//! The top-level `render` module owns all output production for the crate:
//! - [`crate::render::graph`] for graph-family rendering backends
//! - [`crate::render::timeline`] for timeline-family rendering backends
//! - [`crate::render::svg`] for shared SVG writing utilities
//! - [`crate::render::text`] for shared text-output canvas and character sets

pub mod graph;
pub(crate) mod svg;
pub mod text;
pub mod timeline;
