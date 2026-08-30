//! Engineering judgment for changes in an existing codebase.
//!
//! Scoped against [`personality`](super::personality), which already owns matching the surrounding
//! code's idioms and formatting. What is left here is the judgment that decides *what* to build:
//! reuse over reinvention, the smallest change that works, and no abstraction without a call for
//! one. Restating the style rule would put one instruction in two cached sections, where editing
//! either one leaves the model holding both versions of it.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Cached-prefix allowance for this section, in estimated tokens.
///
/// A stance, not a rule list: see the prefix budget note in [the module above](super).
const TOKEN_BUDGET: usize = 192;

/// Builder for the coding agent's engineering-judgment prompt section.
pub(crate) struct EngineeringPromptBuilder;

impl EngineeringPromptBuilder {
    /// Builds the stable section that guides implementation choices.
    pub(crate) fn build_engineering_section() -> Result<PromptSection> {
        let content = "Engineering Judgment:\n\
                       - Reuse the repository's existing public APIs, helpers, and \
                         mechanisms before adding new ones; extend them when they fit the \
                         requested behavior.\n\
                       - Prefer the smallest change that solves the stated problem while \
                         preserving established boundaries and compatibility.\n\
                       - Do not add a parallel abstraction, configuration layer, or \
                         general-purpose mechanism without a concrete need in the current \
                         work.\n\
                       - Make tradeoffs explicit when they materially affect the result, \
                         and validate changes in proportion to their risk.";

        PromptSection::new(
            PromptSectionName::CORE_BEHAVIOR,
            "Engineering judgment for repository changes and API evolution",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .map(|section| section.with_token_budget(TOKEN_BUDGET))
    }
}
