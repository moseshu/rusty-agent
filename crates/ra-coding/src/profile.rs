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
//!
//! **A tier and a role are different questions.** A tier says what this product advertises; a role
//! says which capabilities an agent installs, and a read-only one installs nothing that writes. The
//! role-aware constructor below is where the two meet, by subtracting withheld entries from the
//! tier's selection and withheld *advertised* entries from its band. What it does not do is relax
//! the tier: an entry that is missing because nobody wrote it is in no role's withheld set, so it
//! still fails.

use std::collections::BTreeSet;

use ra_core::{error::Result, prompt::PromptRole, tool::ToolLookupKey};
use ra_runtime::tool::profile::{ToolProfile, ToolProfileId, ToolSurfaceBudget};
use serde::{Deserialize, Serialize};

/// What the coding agent's whole advertised surface may cost per turn.
///
/// Codex's 16 entries measure 19.8 KB, so this is the same order of magnitude rather than an
/// aspiration. It applies to every tier: a smaller surface made of larger entries costs the same.
const MAX_ADVERTISED_BYTES: usize = 20 * 1024;

/// The longest aggregate model-facing name list a coding prompt inventories.
///
/// A tool schema is sent in the provider's tool table, while the cached prefix repeats only its
/// name. The two costs therefore need independent bounds. Twenty-four names of up to 64
/// characters cover common provider limits and qualified MCP-style names without allowing an
/// unconstrained host registration to grow the reusable instruction span indefinitely.
pub(crate) const MAX_ADVERTISED_NAME_CHARS: usize = 24 * 64;

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
    /// The tiers this product ships, from the narrowest surface to the widest.
    ///
    /// Written out rather than derived, because the enum is `#[non_exhaustive]` and nothing can
    /// enumerate it. Adding a tier therefore means adding it here as well: a tier absent from this
    /// list assembles perfectly well and never appears in the committed assembly record, which is
    /// the one place what it advertises and what that costs would have been written down.
    pub const SHIPPED: [Self; 3] = [Self::Core, Self::CodexLike, Self::Full];

    /// Builds the framework profile this tier describes.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if a declared tool name is not a valid lookup key or if the
    /// tier's own bounds are inconsistent — both are mistakes in the constants above.
    pub fn to_tool_profile(self) -> Result<ToolProfile> {
        self.narrowed(
            self.tier_name().to_owned(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
    }

    /// Builds the profile for a role that installs only some of what this tier names.
    ///
    /// A role selects capabilities, not tools, and a capability it does not install takes its
    /// entries with it. Without this the read-only roles would be judged against a band measured
    /// for an agent holding everything, and every one of them would fail the tier's floor — the
    /// check whose whole purpose is to catch a surface that lost an entry nobody meant to drop.
    ///
    /// **The narrowing is subtraction, not a second list.** What the role withheld is derived from
    /// the capabilities it did not install, so a tool added to one of them is withheld the same day
    /// it lands. The tier's own list stays the specification for what the product advertises: an
    /// entry that no capability provides at all is in neither set, so it survives the subtraction
    /// and still fails assembly loudly.
    ///
    /// # Errors
    ///
    /// Propagates the failures described on [`Self::to_tool_profile`].
    pub(crate) fn to_tool_profile_for_role(
        self,
        role: &PromptRole,
        withheld: &BTreeSet<ToolLookupKey>,
        withheld_advertised: &BTreeSet<ToolLookupKey>,
    ) -> Result<ToolProfile> {
        if withheld.is_empty() {
            return self.to_tool_profile();
        }
        // A narrowed tier is a different profile and says so. The identity reaches assembly errors
        // and the surface's own record, and `core` naming three entries would read there as the
        // tier having quietly shrunk.
        self.narrowed(
            format!("{}-{}", self.tier_name(), role.role_name()),
            withheld,
            withheld_advertised,
        )
    }

    /// Profile identity of the tier as declared, before any role narrows it.
    pub(crate) const fn tier_name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::CodexLike => "codex_like",
            Self::Full => "full",
        }
    }

    /// The advertised-entry band this tier was measured into.
    ///
    /// Every tier is a band rather than an exact count, so that adding one entry is a decision
    /// about the surface rather than an edit in two places — a bound that has to be raised for each
    /// ordinary addition teaches whoever makes it to raise it without looking. The bands are wide
    /// enough to breathe and narrow enough that a surface which dropped several entries lands
    /// outside one.
    const fn band(self) -> (usize, usize) {
        match self {
            Self::Core => (6, 8),
            Self::CodexLike => (14, 16),
            // The ceiling is Claude Code's 24; the floor stays the standard surface's, because
            // `full` is that surface plus whatever else the host installed.
            Self::Full => (14, 24),
        }
    }

    /// The lookup keys this tier names, or `None` when it takes whatever is registered.
    fn declared_keys(self) -> Result<Option<Vec<ToolLookupKey>>> {
        Ok(match self {
            Self::Core => Some(lookup_keys(&CORE)?),
            Self::CodexLike => Some(
                lookup_keys(&CORE)?
                    .into_iter()
                    .chain(lookup_keys(&CODEX_LIKE_EXTRA)?)
                    .collect(),
            ),
            Self::Full => None,
        })
    }

    /// Builds this tier's profile with withheld entries removed from its selection and budget.
    ///
    /// Both halves, because a selection and a band that disagree are worse than either mistake
    /// alone: dropping an advertised entry without moving the floor fails every narrowed surface,
    /// and moving the floor without dropping it fails on a tool the role was never going to
    /// install. Hidden entries remain part of selection but never consumed a budget slot, so they
    /// do not move the band.
    fn narrowed(
        self,
        id: String,
        withheld: &BTreeSet<ToolLookupKey>,
        withheld_advertised: &BTreeSet<ToolLookupKey>,
    ) -> Result<ToolProfile> {
        let declared = self.declared_keys()?;
        let (floor, ceiling) = self.band();
        // What the tier would have advertised and this role does not. `full` names nothing of its
        // own, so every withheld advertised entry is one it would otherwise have taken.
        let given_up = match &declared {
            Some(keys) => keys
                .iter()
                .filter(|key| withheld_advertised.contains(key))
                .count(),
            None => withheld_advertised.len(),
        };
        // The byte and name-character ceilings do not shift: they bound what one turn may cost, and
        // a smaller surface can only be further under them.
        let budget = ToolSurfaceBudget::new(
            floor.saturating_sub(given_up),
            ceiling.saturating_sub(given_up),
        )?
        .with_max_advertised_bytes(MAX_ADVERTISED_BYTES)
        .with_max_advertised_name_chars(MAX_ADVERTISED_NAME_CHARS);

        let builder = ToolProfile::builder(ToolProfileId::new(id)?);
        let builder = match declared {
            Some(keys) => {
                builder.include_all(keys.into_iter().filter(|key| !withheld.contains(key)))
            }
            None => builder.all_registered(),
        };
        builder.budget(budget).build()
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
