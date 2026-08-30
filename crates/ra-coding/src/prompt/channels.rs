//! Commentary and final response-channel rules.
//!
//! The runtime assigns output phases from settlement, so this section does not instruct the model
//! to override that mechanism. It tells the model what each user-visible channel is for: concise
//! working updates are useful while tools run, but the final delivery must stand on its own after
//! those updates are collapsed. Formatting belongs to `formatting`; keeping Markdown rules there
//! avoids maintaining duplicate render instructions in two cached sections.
//!
//! **This section owns placement and nothing else.** That a blocker must be reported at all is
//! [`autonomy`](super::autonomy)'s rule, and that the report must be truthful is
//! [`personality`](super::personality)'s. Restating either would put one instruction in two cached
//! sections, where editing one leaves the model holding both versions of it. What is left for this
//! section is which of the two channels each thing lands in.
//!
//! The cadence is anchored to tool batches rather than to a number of seconds. A model has no
//! clock, and the elapsed time a wall-clock rule would bound is spent *after* its message is
//! written, in tool execution it cannot observe — so the rule would be one the model cannot follow
//! and the runtime does not measure. The one number `autonomy` states is different in kind: it is a
//! real runtime default, and a test pins the prompt text to the constant that enforces it.
//!
//! The two channel identifiers are [`OutputPhase`](ra_core::item::OutputPhase)'s own labels, pinned
//! by the prompt-dump test so a rename cannot leave a cached prefix naming a channel that no longer
//! exists. One provider lowering diverges: the `OpenAI` Responses codec writes `final_answer` on
//! the wire and accepts both spellings back, so replayed history can show the model a label this
//! prefix does not use. Aligning the two belongs with that codec — spelling one vendor's wire
//! vocabulary here would put it in every role's cached prefix.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Cached-prefix allowance for this section, in estimated tokens.
///
/// An enumerated rule list: see the prefix budget note in [the module above](super).
const TOKEN_BUDGET: usize = 256;

/// Builder for the coding agent's dual-channel response section.
pub(crate) struct ChannelsPromptBuilder;

impl ChannelsPromptBuilder {
    /// Builds the stable section that assigns content to commentary and final responses.
    pub(crate) fn build_channels_section() -> Result<PromptSection> {
        let content = "Response Channels:\n\
                       - Use `commentary` for short, scannable progress updates while working: \
                         announce tool-based investigation, state useful partial findings, and \
                         explain the next in-scope action. Send one before your first tool call and \
                         again before each later batch, so no stretch of tool work is silent.\n\
                       - Use `final` only for the completed response to the user. Everything that \
                         response depends on belongs there: the outcome, the evidence behind it, and \
                         any blocker or decision you are handing back. Repeat what an earlier update \
                         already said rather than referring to it.\n\
                       - Commentary is progress, not delivery. Do not ask there for a decision you \
                         need before continuing, and do not make the user reconstruct the conclusion \
                         from earlier updates.";

        PromptSection::new(
            PromptSectionName::CHANNELS,
            "Commentary progress updates and self-contained final delivery",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .map(|section| section.with_token_budget(TOKEN_BUDGET))
    }
}
