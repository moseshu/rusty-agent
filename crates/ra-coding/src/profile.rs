//! Tool-surface profiles: core, `codex_like`, and full product profiles.

use serde::{Deserialize, Serialize};

/// Tool surface profiles supported by the coding agent product.
#[non_exhaustive]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CodingProfile {
    /// Minimal editing and execution baseline (`read_file`, `apply_patch`, `exec_command`, `write_stdin`).
    Core,
    /// Standard engineering profile (Core plus search tools `grep`, `glob`, and `view_image`).
    #[default]
    CodexLike,
    /// Full product profile with all available coding, web, and orchestration tools.
    Full,
}
