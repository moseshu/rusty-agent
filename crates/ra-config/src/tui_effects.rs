// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Individual TUI effect preferences, subordinate to the animation master switch.

#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Optional visual effects. Disabling an effect preserves its underlying activity.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, JsonSchema)]
#[serde(default)]
#[schemars(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Preserve independent fields in the source configuration schema."
)]
pub struct TuiEffects {
    /// Animate the composer starfield.
    pub starfield: bool,
    /// Shimmer status and loading text.
    pub shimmer: bool,
    /// Animate the welcome artwork.
    pub welcome: bool,
    /// Animate reasoning-effort changes in the composer and footer.
    pub effort: bool,
    /// Animate activity bullets and loading spinners.
    pub progress: bool,
    /// Blink the terminal-title indicator when user action is required.
    pub title: bool,
}

impl Default for TuiEffects {
    fn default() -> Self {
        Self {
            starfield: true,
            shimmer: true,
            welcome: true,
            effort: true,
            progress: true,
            title: true,
        }
    }
}
