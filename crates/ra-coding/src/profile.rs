//! The coding agent's three tool surfaces, and the numbers each of them holds itself to.
//!
//! These counts are this product's policy, not a framework limit. They come from measuring two
//! coding agents that work: Codex advertises 16 entries per request and Claude Code 24, and the
//! comparison point is a 43-entry surface that spent 11.9k tokens per turn on schemas and cost
//! 3.4x more on the same task. A read-only assistant or a graph-orchestration product assembles
//! its own [`ToolProfile`] with its own numbers, and nothing here applies to it.
//!
//! **The lists are the specification.** Most of the entries named below have not been written yet,
//! and naming them anyway is the point: assembly fails against a registry that is missing one, so
//! a tool that is planned but absent is loud rather than quietly missing from the surface. The
//! alternative — assembling whatever happens to exist — is a product that ships with a smaller
//! tool surface than its prompt describes and no failure anywhere.

use ra_core::{error::Result, tool::ToolLookupKey};
use ra_runtime::tool::profile::{ToolProfile, ToolProfileId, ToolSurfaceBudget};
use serde::{Deserialize, Serialize};

/// What the coding agent's whole advertised surface may cost per turn.
///
/// Codex's 16 entries measure 19.8 KB, so this is the same order of magnitude rather than an
/// aspiration. It applies to every tier: a smaller surface made of larger entries costs the same.
const MAX_ADVERTISED_BYTES: usize = 20 * 1024;

/// The entries that do the work, plus the two that produce structured observations.
///
/// Six rather than four because `grep` and `glob` are not conveniences over `exec_command`: they
/// return match counts, skip reasons, and truncation causes, and a run that has to shell out for
/// search gets raw text that neither the context budget nor offline evaluation can read.
const CORE: [&str; 6] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "read_file",
    "grep",
    "glob",
];

/// What the standard surface adds: planning, the multimodal and web entries, and the two folded
/// namespaces.
///
/// `agent` and `mcp` are single entries, not families. Each is one advertised schema that routes
/// internally — `agent` covers spawn / output / followup / interrupt / stop, `mcp` covers the
/// three resource operations — which is why the sub-tools do not appear here and do not each cost
/// a slot.
const CODEX_LIKE_EXTRA: [&str; 9] = [
    "update_plan",
    "view_image",
    "web_search",
    "web_fetch",
    "ask_user",
    "skill",
    "tool_search",
    "agent",
    "mcp",
];

/// Tool surface profiles supported by the coding agent product.
#[non_exhaustive]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CodingProfile {
    /// Editing, execution, and structured search only: six entries, no orchestration.
    Core,
    /// The standard fifteen-entry surface, sized against Codex's sixteen.
    #[default]
    CodexLike,
    /// Everything the host registered, whatever that turned out to be.
    ///
    /// This is the tier that exists for tools nobody could list in advance — an MCP server's
    /// exports arrive when it connects. It keeps the standard surface's floor and raises only the
    /// ceiling, to Claude Code's 24.
    Full,
}

impl CodingProfile {
    /// Builds the framework profile this tier describes.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if a declared tool name is not a valid lookup key or if the
    /// tier's own bounds are inconsistent — both are mistakes in the constants above.
    pub fn to_tool_profile(self) -> Result<ToolProfile> {
        let id = ToolProfileId::new(match self {
            Self::Core => "core",
            Self::CodexLike => "codex_like",
            Self::Full => "full",
        })?;

        let builder = ToolProfile::builder(id);
        let builder = match self {
            // Every tier is a band rather than an exact count, so that adding one entry is a
            // decision about the surface rather than an edit in two places — a bound that has to
            // be raised for each ordinary addition teaches whoever makes it to raise it without
            // looking. The bands are wide enough to breathe and narrow enough that a surface which
            // dropped several entries lands outside one.
            Self::Core => builder.include_all(lookup_keys(&CORE)?).budget(
                ToolSurfaceBudget::new(6, 8)?.with_max_advertised_bytes(MAX_ADVERTISED_BYTES),
            ),
            Self::CodexLike => builder
                .include_all(lookup_keys(&CORE)?)
                .include_all(lookup_keys(&CODEX_LIKE_EXTRA)?)
                .budget(
                    ToolSurfaceBudget::new(14, 16)?.with_max_advertised_bytes(MAX_ADVERTISED_BYTES),
                ),
            Self::Full => builder.all_registered().budget(
                ToolSurfaceBudget::new(14, 24)?.with_max_advertised_bytes(MAX_ADVERTISED_BYTES),
            ),
        };
        builder.build()
    }
}

/// Turns declared tool names into the routing identities a profile selects by.
///
/// Every entry here is a bare key. A folded namespace such as `agent` is one top-level tool whose
/// arguments choose the operation, not a namespace holding five separately routed tools — a
/// namespaced key would advertise the sub-tool's name to the model and would cost five slots
/// instead of one.
fn lookup_keys(names: &[&str]) -> Result<Vec<ToolLookupKey>> {
    names.iter().copied().map(ToolLookupKey::bare).collect()
}
