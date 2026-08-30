//! Tool-use guidance for the advertised surface.
//!
//! The tool inventory belongs to `tool_surface`; this section deliberately does not repeat it.
//! Names change whenever the host's advertised capabilities change, while the selection rule is
//! stable: choose from the entries the request actually carries, prefer structured observations
//! when they exist, and let a command-execution entry cover the long tail instead of proliferating
//! action-specific tools. Keeping those concerns separate means a schema or inventory change does
//! not rewrite the behavior contract along with it.
//!
//! Separate, but not independent: the two are assembled together, because a rule about which entry
//! to pick is addressed to an agent that has entries. The text below therefore points at the
//! `Available tools` heading the inventory renders, and says so in the inventory's own words.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Cached-prefix allowance for this section, in estimated tokens.
///
/// The behavior is intentionally short: it names no tool and the inventory it complements has its
/// own allowance. The two are always assembled together, so together they occupy one rule-list
/// share of the prefix budget — never more, and never one half on its own.
const TOKEN_BUDGET: usize = 128;

/// Builder for the stable tool-selection contract.
pub(crate) struct ToolUsePromptBuilder;

impl ToolUsePromptBuilder {
    /// Builds the stable rules that connect tool choice to the advertised surface.
    ///
    /// Takes no tool list on purpose. The rules hold for any surface, and the caller adds them only
    /// where an inventory exists — a section that reasoned about the names would be a second copy
    /// of the inventory, invalidated by every change to it.
    pub(crate) fn build_tool_use_section() -> Result<PromptSection> {
        PromptSection::new(
            PromptSectionName::TOOL_USE,
            "Tool-selection discipline for the advertised surface",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            "Tool Use:\n\
             - Use only the entries listed under Available tools. Choose the entry whose \
             documented capability and schema fit the task; do not infer or invoke an unlisted \
             capability.\n\
             - Prefer an advertised specialist that returns the needed structured observation. \
             When the surface offers a command-execution entry, use it for long-tail command-line \
             work rather than searching for separate tools for individual commands or utilities.",
        )
        .map(|section| section.with_token_budget(TOKEN_BUDGET))
    }
}
