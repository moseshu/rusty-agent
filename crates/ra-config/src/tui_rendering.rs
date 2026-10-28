// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! TUI content rendering preferences, independent of animation effects.

#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Optional rich renderers. Disabled renderers preserve the original source.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, JsonSchema)]
#[serde(default)]
#[schemars(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Preserve independent fields in the source configuration schema."
)]
pub struct TuiRendering {
    /// Render Mermaid code blocks as diagrams.
    pub mermaid: bool,
    /// Render math expressions using Unicode notation.
    pub math: bool,
    /// Render pipe tables, including tables inside Markdown fences.
    pub tables: bool,
    /// Render Markdown bullets and task-list markers as Unicode symbols.
    pub lists: bool,
}

impl Default for TuiRendering {
    fn default() -> Self {
        Self {
            mermaid: true,
            math: true,
            tables: true,
            lists: true,
        }
    }
}
