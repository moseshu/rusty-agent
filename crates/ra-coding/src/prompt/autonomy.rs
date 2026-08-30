//! Autonomous progress and stop-loss guidance.
//!
//! Scoped against `identity`, which already owns the role's authority, the requested deliverable,
//! and stating a limitation the role cannot get past. What is left here is *when* to act instead
//! of propose, and what a failing tool means. Restating identity's contract would put one
//! instruction in two cached sections, where editing either one leaves the model holding both
//! versions of it.
//!
//! The one number in the text is the runtime's default, not a threshold this section invents: a
//! tool may raise its own no-progress limit or turn the breaker off, which is why the text names
//! the default as a default. The prompt-dump test pins the spelled-out word to
//! [`DEFAULT_MAX_NO_PROGRESS_STREAK`](ra_core::tool::DEFAULT_MAX_NO_PROGRESS_STREAK), so a cached
//! prefix cannot go on claiming a limit the runtime stopped enforcing.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Cached-prefix allowance for this section, in estimated tokens.
///
/// An enumerated rule list: see the prefix budget note in [the module above](super).
const TOKEN_BUDGET: usize = 256;

/// Builder for the coding agent's autonomous-progress prompt section.
pub(crate) struct AutonomyPromptBuilder;

impl AutonomyPromptBuilder {
    /// Builds the stable section that directs work through completion or a concrete blocker.
    pub(crate) fn build_autonomy_section() -> Result<PromptSection> {
        let content = "Autonomous Progress and Stop-Loss:\n\
                       - Do not stop at analysis or a proposal when the change is one you may \
                         make: inspect, change, verify, and report within the current turn. \
                         Respect an explicit request to plan, answer, pause, or redirect \
                         instead.\n\
                       - Treat a failed tool call as evidence and let it change the next attempt. \
                         Do not repeat a call that keeps returning the same failure.\n\
                       - The runtime may refuse a tool call after its no-progress limit, by \
                         default three consecutive failures that returned nothing new. The \
                         refusal asks for a different approach; the tool is not lost and the task \
                         is not done.\n\
                       - Exhaust the safe, in-scope attempts before reporting a blocker, then say \
                         what evidence you have and what information or authority would unblock it.";

        PromptSection::new(
            PromptSectionName::AUTONOMY,
            "Autonomous completion, evidence-driven recovery, and bounded retries",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .map(|section| section.with_token_budget(TOKEN_BUDGET))
    }
}
