// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product web-search access policy.
use serde::{Deserialize, Serialize};

/// Access mode selected by the coding product.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchMode {
    /// Do not advertise either coding-product search shape.
    Disabled,
    /// Search cached content without live fetches.
    #[default]
    Cached,
    /// Permit live fetches only for indexed URLs.
    Indexed,
    /// Permit live external content.
    Live,
}

impl WebSearchMode {
    /// Restricts access to that permitted by both modes, as Codex does.
    #[must_use]
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Disabled, _) | (_, Self::Disabled) => Self::Disabled,
            (Self::Cached, _) | (_, Self::Cached) => Self::Cached,
            (Self::Indexed, _) | (_, Self::Indexed) => Self::Indexed,
            (Self::Live, Self::Live) => Self::Live,
        }
    }
}
