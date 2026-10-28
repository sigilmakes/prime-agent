use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// Error returned while resolving or materializing a view.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewError {
    /// A requested view feature is reserved for a later slice.
    NotImplementedYet {
        /// Human-readable feature family that is not available in v1.
        feature: String,
    },
    /// A selector referenced an anchor that does not exist in the payload.
    UnknownAnchor {
        /// Missing node or subgraph ID.
        id: String,
    },
}

impl Display for ViewError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplementedYet { feature } => {
                write!(f, "view feature is not implemented yet: {feature}")
            }
            Self::UnknownAnchor { id } => write!(f, "unknown view anchor: {id}"),
        }
    }
}

impl std::error::Error for ViewError {}
