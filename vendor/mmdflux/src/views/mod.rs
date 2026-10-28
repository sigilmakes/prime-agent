//! Materialized diagram views over canonical MMDS payloads.
//!
//! See the crate-level [Stability](crate#stability) section for the
//! variant-addition and field-addition policy on the public types in this module.
//!
//! This module owns the read-side `ViewSpec` contract. It intentionally works
//! over `mmds::Document` rather than renderer or engine internals.
//!
//! The v1 surface is intentionally small: project a filtered MMDS payload
//! from a `ViewSpec` and emit projection events for elements that leave the view.
//! Rendering remains a runtime concern; pass the returned document to
//! [`crate::render_document`] when a text, SVG, or MMDS rendering is
//! needed.
//!
//! # Example
//!
//! ```no_run
//! use mmdflux::mmds::Document;
//! use mmdflux::views::{
//!     AnchorRef, Selector, TraversalDirection, ViewSpec, ViewStatement, project,
//! };
//! use mmdflux::{OutputFormat, RenderConfig, materialize_diagram, render_document};
//!
//! let source = "\
//! graph TD
//! service_a[Service A] --> service_b[Service B]
//! external[External] --> service_a
//! service_b --> service_c[Service C]
//! service_c --> database[Database]
//! service_a --> audit[Audit]
//! ";
//!
//! let canonical: Document = materialize_diagram(source, &RenderConfig::default())?;
//! let spec = ViewSpec::new(vec![ViewStatement::Include(Selector::Traversal {
//!         anchor: AnchorRef::Node("service_a".to_string()),
//!         direction: TraversalDirection::Downstream,
//!         hops: 2,
//!     })]);
//!
//! let (view, events) = project(&canonical, &spec)?;
//! assert!(view.nodes.iter().any(|node| node.id == "service_c"));
//! assert!(events.iter().any(|event| matches!(
//!     event,
//!     mmdflux::views::ViewEvent::NodeLeftView { id, .. } if id == "external"
//! )));
//!
//! let text = render_document(&view, OutputFormat::Text, &RenderConfig::default())?;
//! assert!(text.contains("Service A"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod evaluate;
mod events;
mod project;
mod spec;

pub use error::ViewError;
pub use events::{ElisionReason, ViewEvent};
pub use project::{VIEW_EXTENSION_NAMESPACE, project};
pub use spec::{
    AnchorRef, BoundaryPolicy, CompoundPolicy, EdgeAnchor, LayoutMode, NodePredicate, Selector,
    TraversalDirection, ViewSpec, ViewStatement,
};
