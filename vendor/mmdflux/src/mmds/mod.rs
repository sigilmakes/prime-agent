//! MMDS interchange contract and document-generation namespace.
//!
//! See the crate-level [Stability](crate#stability) section for the
//! variant-addition and field-addition policy on the public types in this module.
//!
//! This module owns the typed graph-family MMDS document, profile vocabulary,
//! Mermaid regeneration helpers, validation, and hydration to `Diagram` for
//! adapter workflows. Replay rendering lives in `runtime::mmds`.

pub(crate) mod detect;
pub mod diff;
pub(crate) mod document;
pub mod events;
pub(crate) mod hydrate;
mod mermaid;
pub(crate) mod parse;
pub(crate) mod sequence;
pub mod token;

use std::error::Error;
use std::fmt;

pub use detect::{
    SUPPORTED_OUTPUT_FORMATS, detect_diagram_type, is_mmds_input, resolve_logical_diagram_id,
    supports_format,
};
#[doc(hidden)]
#[allow(deprecated)]
pub use document::Output;
#[doc(hidden)]
#[allow(deprecated)]
pub use document::to_json_typed_with_routing;
// Schema types (public adapter contract).
pub use document::{
    Bounds, Defaults, Document, Edge, EdgeDefaults, Metadata, Node, NodeDefaults, Port, Position,
    Rect, Size, Subgraph,
};
// Profile vocabulary constants.
pub use document::{
    CORE_PROFILE, NODE_STYLE_EXTENSION_NAMESPACE, NODE_STYLE_PROFILE, SUPPORTED_PROFILES,
    SVG_PROFILE, TEXT_EXTENSION_NAMESPACE, TEXT_MEASUREMENTS_EXTENSION_NAMESPACE,
    TEXT_MEASUREMENTS_PROFILE, TEXT_METRICS_EXTENSION_NAMESPACE, TEXT_METRICS_PROFILE,
    TEXT_PROFILE,
};
pub(crate) use hydrate::hydrate_routed_geometry_from_document_with_provider;
pub use hydrate::{
    HydrationError, from_document, from_str, hydrate_graph_geometry_from_document_with_diagram,
    hydrate_routed_geometry_from_document,
};
#[doc(hidden)]
#[allow(deprecated)]
pub use hydrate::{
    from_output, hydrate_graph_geometry_from_output_with_diagram,
    hydrate_routed_geometry_from_output,
};
pub use mermaid::{GenerationError, generate_mermaid, generate_mermaid_from_str};
pub use parse::{parse_with_profiles, validate_input};
use serde_json::{Map, Value};
pub use token::{MmdsToken, MmdsTokenError};

#[cfg(test)]
mod regression_tests;

/// Subject associated with an MMDS model event or snapshot diff change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Document,
    Node(String),
    Edge(String),
    Subgraph(String),
}

impl Subject {
    #[cfg(test)]
    pub(crate) fn matches_id(&self, subject_id: &str) -> bool {
        match self {
            Self::Document => subject_id.is_empty(),
            Self::Node(id) | Self::Edge(id) | Self::Subgraph(id) => id == subject_id,
        }
    }
}

/// Parse-time error for MMDS input.
#[derive(Debug, Clone)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for ParseError {}

/// Result of profile capability evaluation for a parsed MMDS payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileNegotiation {
    /// Profiles recognized by the current runtime.
    pub supported: Vec<String>,
    /// Profiles declared by payload but unknown to this runtime.
    pub unknown: Vec<String>,
}

/// Parse graph-family MMDS JSON input into the typed document envelope.
///
/// Unlike a plain deserialize, this expands omitted node/edge fields using
/// the top-level `defaults` block before constructing [`Document`]. Callers
/// that prefer trait syntax can also use `input.parse::<Document>()` or
/// `Document::try_from(input)`.
pub fn parse_input(input: &str) -> Result<Document, ParseError> {
    let mut value: Value = serde_json::from_str(input)
        .map_err(|err| ParseError::new(format!("MMDS parse error: {err}")))?;

    expand_defaults_in_value(&mut value)?;

    serde_json::from_value::<Document>(value)
        .map_err(|err| ParseError::new(format!("MMDS parse error: {err}")))
}

impl std::str::FromStr for Document {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parse_input(input)
    }
}

impl TryFrom<&str> for Document {
    type Error = ParseError;

    fn try_from(input: &str) -> Result<Self, Self::Error> {
        parse_input(input)
    }
}

/// Evaluate declared profiles against runtime-known profile vocabulary.
///
/// This helper is advisory. Hydration remains permissive with unknown profiles.
pub fn evaluate_profiles(input: &str) -> Result<ProfileNegotiation, ParseError> {
    let output = parse_input(input)?;
    Ok(evaluate_profiles_for_document(&output))
}

/// Evaluate declared profiles for an already-parsed MMDS payload.
pub fn evaluate_profiles_for_document(output: &Document) -> ProfileNegotiation {
    let mut supported = Vec::new();
    let mut unknown = Vec::new();
    let mut seen_supported = std::collections::HashSet::new();
    let mut seen_unknown = std::collections::HashSet::new();

    for profile in &output.profiles {
        if SUPPORTED_PROFILES.contains(&profile.as_str()) {
            if seen_supported.insert(profile.clone()) {
                supported.push(profile.clone());
            }
            continue;
        }

        if seen_unknown.insert(profile.clone()) {
            unknown.push(profile.clone());
        }
    }

    ProfileNegotiation { supported, unknown }
}

/// Evaluate declared profiles for an already-parsed MMDS payload.
#[doc(hidden)]
#[deprecated(note = "use evaluate_profiles_for_document instead")]
pub fn evaluate_profiles_for_output(document: &Document) -> ProfileNegotiation {
    evaluate_profiles_for_document(document)
}

fn expand_defaults_in_value(value: &mut Value) -> Result<(), ParseError> {
    let root = value.as_object_mut().ok_or_else(|| {
        ParseError::new("MMDS parse error: top-level JSON value must be an object")
    })?;

    let node_shape = default_value(
        root,
        &["defaults", "node", "shape"],
        Value::String("rectangle".to_string()),
    );
    let edge_stroke = default_value(
        root,
        &["defaults", "edge", "stroke"],
        Value::String("solid".to_string()),
    );
    let edge_arrow_start = default_value(
        root,
        &["defaults", "edge", "arrow_start"],
        Value::String("none".to_string()),
    );
    let edge_arrow_end = default_value(
        root,
        &["defaults", "edge", "arrow_end"],
        Value::String("normal".to_string()),
    );
    let edge_minlen = default_value(root, &["defaults", "edge", "minlen"], Value::from(1));

    if let Some(nodes) = root.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            if let Some(node_obj) = node.as_object_mut() {
                insert_default(node_obj, "shape", &node_shape);
            }
        }
    }

    if let Some(edges) = root.get_mut("edges").and_then(Value::as_array_mut) {
        for edge in edges {
            if let Some(edge_obj) = edge.as_object_mut() {
                insert_default(edge_obj, "stroke", &edge_stroke);
                insert_default(edge_obj, "arrow_start", &edge_arrow_start);
                insert_default(edge_obj, "arrow_end", &edge_arrow_end);
                insert_default(edge_obj, "minlen", &edge_minlen);
            }
        }
    }

    Ok(())
}

fn default_value(root: &Map<String, Value>, path: &[&str], fallback: Value) -> Value {
    traverse_value(root, path).cloned().unwrap_or(fallback)
}

fn insert_default(object: &mut Map<String, Value>, key: &str, default: &Value) {
    object
        .entry(key.to_string())
        .or_insert_with(|| default.clone());
}

fn traverse_value<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = root.get(*first)?;
    for key in rest {
        current = current.get(*key)?;
    }
    Some(current)
}
