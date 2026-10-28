//! Canonical MMDS string token contracts for graph-family enums.
//!
//! See the crate-level [Stability](crate#stability) section for the
//! variant-addition and field-addition policy on the public types in this module.

use std::error::Error;
use std::fmt;

use crate::graph::attachment::PortFace;
use crate::graph::geometry::EdgeLabelSide;
use crate::graph::{Arrow, Direction, GeometryLevel, Shape, Stroke};

/// Conversion contract between graph-family Rust enums and MMDS schema tokens.
pub trait MmdsToken: Sized {
    /// Parse a value from its canonical MMDS string token.
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError>;

    /// Return this value's canonical MMDS string token.
    fn as_mmds_str(&self) -> &'static str;
}

/// Error returned when an MMDS string token does not match a graph vocabulary.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmdsTokenError {
    /// Vocabulary being parsed, such as `"shape"` or `"direction"`.
    pub kind: &'static str,
    /// Rejected token value.
    pub value: String,
}

impl MmdsTokenError {
    pub fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

impl fmt::Display for MmdsTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} MMDS token: {:?}", self.kind, self.value)
    }
}

impl Error for MmdsTokenError {}

impl MmdsToken for Shape {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "rectangle" => Ok(Shape::Rectangle),
            "round" => Ok(Shape::Round),
            "stadium" => Ok(Shape::Stadium),
            "subroutine" => Ok(Shape::Subroutine),
            "cylinder" => Ok(Shape::Cylinder),
            "document" => Ok(Shape::Document),
            "documents" => Ok(Shape::Documents),
            "tagged_document" => Ok(Shape::TaggedDocument),
            "card" => Ok(Shape::Card),
            "tagged_rect" => Ok(Shape::TaggedRect),
            "diamond" => Ok(Shape::Diamond),
            "hexagon" => Ok(Shape::Hexagon),
            "trapezoid" => Ok(Shape::Trapezoid),
            "inv_trapezoid" => Ok(Shape::InvTrapezoid),
            "parallelogram" => Ok(Shape::Parallelogram),
            "inv_parallelogram" => Ok(Shape::InvParallelogram),
            "manual_input" => Ok(Shape::ManualInput),
            "asymmetric" => Ok(Shape::Asymmetric),
            "circle" => Ok(Shape::Circle),
            "double_circle" => Ok(Shape::DoubleCircle),
            "small_circle" => Ok(Shape::SmallCircle),
            "framed_circle" => Ok(Shape::FramedCircle),
            "crossed_circle" => Ok(Shape::CrossedCircle),
            "text_block" => Ok(Shape::TextBlock),
            "fork_join" => Ok(Shape::ForkJoin),
            "note_rect" => Ok(Shape::NoteRect),
            _ => Err(MmdsTokenError::new("shape", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            Shape::Rectangle => "rectangle",
            Shape::Round => "round",
            Shape::Stadium => "stadium",
            Shape::Subroutine => "subroutine",
            Shape::Cylinder => "cylinder",
            Shape::Document => "document",
            Shape::Documents => "documents",
            Shape::TaggedDocument => "tagged_document",
            Shape::Card => "card",
            Shape::TaggedRect => "tagged_rect",
            Shape::Diamond => "diamond",
            Shape::Hexagon => "hexagon",
            Shape::Trapezoid => "trapezoid",
            Shape::InvTrapezoid => "inv_trapezoid",
            Shape::Parallelogram => "parallelogram",
            Shape::InvParallelogram => "inv_parallelogram",
            Shape::ManualInput => "manual_input",
            Shape::Asymmetric => "asymmetric",
            Shape::Circle => "circle",
            Shape::DoubleCircle => "double_circle",
            Shape::SmallCircle => "small_circle",
            Shape::FramedCircle => "framed_circle",
            Shape::CrossedCircle => "crossed_circle",
            Shape::TextBlock => "text_block",
            Shape::ForkJoin => "fork_join",
            Shape::NoteRect => "note_rect",
        }
    }
}

impl MmdsToken for Direction {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "TD" | "TB" => Ok(Direction::TopDown),
            "BT" => Ok(Direction::BottomTop),
            "LR" => Ok(Direction::LeftRight),
            "RL" => Ok(Direction::RightLeft),
            _ => Err(MmdsTokenError::new("direction", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            Direction::TopDown => "TD",
            Direction::BottomTop => "BT",
            Direction::LeftRight => "LR",
            Direction::RightLeft => "RL",
        }
    }
}

impl MmdsToken for Stroke {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "solid" => Ok(Stroke::Solid),
            "dotted" => Ok(Stroke::Dotted),
            "dashed" => Ok(Stroke::Dashed),
            "thick" => Ok(Stroke::Thick),
            "invisible" => Ok(Stroke::Invisible),
            _ => Err(MmdsTokenError::new("stroke", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            Stroke::Solid => "solid",
            Stroke::Dotted => "dotted",
            Stroke::Dashed => "dashed",
            Stroke::Thick => "thick",
            Stroke::Invisible => "invisible",
        }
    }
}

impl MmdsToken for Arrow {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "normal" => Ok(Arrow::Normal),
            "none" => Ok(Arrow::None),
            "cross" => Ok(Arrow::Cross),
            "circle" => Ok(Arrow::Circle),
            "open_triangle" => Ok(Arrow::OpenTriangle),
            "diamond" => Ok(Arrow::Diamond),
            "open_diamond" => Ok(Arrow::OpenDiamond),
            _ => Err(MmdsTokenError::new("arrow", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            Arrow::Normal => "normal",
            Arrow::None => "none",
            Arrow::Cross => "cross",
            Arrow::Circle => "circle",
            Arrow::OpenTriangle => "open_triangle",
            Arrow::Diamond => "diamond",
            Arrow::OpenDiamond => "open_diamond",
        }
    }
}

impl MmdsToken for GeometryLevel {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "layout" => Ok(GeometryLevel::Layout),
            "routed" => Ok(GeometryLevel::Routed),
            _ => Err(MmdsTokenError::new("geometry level", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            GeometryLevel::Layout => "layout",
            GeometryLevel::Routed => "routed",
        }
    }
}

impl MmdsToken for PortFace {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "top" => Ok(PortFace::Top),
            "bottom" => Ok(PortFace::Bottom),
            "left" => Ok(PortFace::Left),
            "right" => Ok(PortFace::Right),
            _ => Err(MmdsTokenError::new("port face", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        self.as_str()
    }
}

impl MmdsToken for EdgeLabelSide {
    fn parse_mmds(value: &str) -> Result<Self, MmdsTokenError> {
        match value {
            "above" => Ok(EdgeLabelSide::Above),
            "below" => Ok(EdgeLabelSide::Below),
            "center" => Ok(EdgeLabelSide::Center),
            _ => Err(MmdsTokenError::new("edge label side", value)),
        }
    }

    fn as_mmds_str(&self) -> &'static str {
        match self {
            EdgeLabelSide::Above => "above",
            EdgeLabelSide::Below => "below",
            EdgeLabelSide::Center => "center",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_mmds_tokens_round_trip() {
        for (token, shape) in [
            ("rectangle", Shape::Rectangle),
            ("stadium", Shape::Stadium),
            ("tagged_document", Shape::TaggedDocument),
            ("fork_join", Shape::ForkJoin),
        ] {
            assert_eq!(Shape::parse_mmds(token), Ok(shape));
            assert_eq!(shape.as_mmds_str(), token);
        }
        assert_eq!(Shape::parse_mmds("shape").unwrap_err().kind, "shape");
    }

    #[test]
    fn direction_mmds_tokens_round_trip() {
        for (token, direction) in [
            ("TD", Direction::TopDown),
            ("BT", Direction::BottomTop),
            ("LR", Direction::LeftRight),
            ("RL", Direction::RightLeft),
        ] {
            assert_eq!(Direction::parse_mmds(token), Ok(direction));
            assert_eq!(direction.as_mmds_str(), token);
        }
        assert_eq!(Direction::parse_mmds("TB"), Ok(Direction::TopDown));
        assert_eq!(Direction::parse_mmds("TB").unwrap().as_mmds_str(), "TD");
        assert_eq!(
            Direction::parse_mmds("top_down").unwrap_err().kind,
            "direction"
        );
    }

    #[test]
    fn stroke_mmds_tokens_round_trip() {
        for (token, stroke) in [
            ("solid", Stroke::Solid),
            ("dotted", Stroke::Dotted),
            ("dashed", Stroke::Dashed),
            ("thick", Stroke::Thick),
            ("invisible", Stroke::Invisible),
        ] {
            assert_eq!(Stroke::parse_mmds(token), Ok(stroke));
            assert_eq!(stroke.as_mmds_str(), token);
        }
        assert_eq!(Stroke::parse_mmds("double").unwrap_err().kind, "stroke");
    }

    #[test]
    fn arrow_mmds_tokens_round_trip() {
        for (token, arrow) in [
            ("normal", Arrow::Normal),
            ("none", Arrow::None),
            ("circle", Arrow::Circle),
            ("open_diamond", Arrow::OpenDiamond),
        ] {
            assert_eq!(Arrow::parse_mmds(token), Ok(arrow));
            assert_eq!(arrow.as_mmds_str(), token);
        }
        assert_eq!(Arrow::parse_mmds("triangle").unwrap_err().kind, "arrow");
    }

    #[test]
    fn geometry_level_mmds_tokens_round_trip() {
        for (token, level) in [
            ("layout", GeometryLevel::Layout),
            ("routed", GeometryLevel::Routed),
        ] {
            assert_eq!(GeometryLevel::parse_mmds(token), Ok(level));
            assert_eq!(level.as_mmds_str(), token);
        }
        assert_eq!(
            GeometryLevel::parse_mmds("full").unwrap_err().kind,
            "geometry level"
        );
    }

    #[test]
    fn port_face_mmds_tokens_round_trip() {
        for (token, face) in [
            ("top", PortFace::Top),
            ("bottom", PortFace::Bottom),
            ("left", PortFace::Left),
            ("right", PortFace::Right),
        ] {
            assert_eq!(PortFace::parse_mmds(token), Ok(face));
            assert_eq!(face.as_mmds_str(), token);
        }
        assert_eq!(PortFace::parse_mmds("north").unwrap_err().kind, "port face");
    }

    #[test]
    fn edge_label_side_mmds_tokens_round_trip() {
        for (token, side) in [
            ("above", EdgeLabelSide::Above),
            ("below", EdgeLabelSide::Below),
            ("center", EdgeLabelSide::Center),
        ] {
            assert_eq!(EdgeLabelSide::parse_mmds(token), Ok(side));
            assert_eq!(side.as_mmds_str(), token);
        }
        assert_eq!(
            EdgeLabelSide::parse_mmds("sideways").unwrap_err().kind,
            "edge label side"
        );
    }
}
