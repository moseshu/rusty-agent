//! Product identity and collaboration contract.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Builder for the coding agent's identity and collaboration contract.
pub(crate) struct IdentityPromptBuilder;

impl IdentityPromptBuilder {
    /// Builds the stable section that defines the agent's identity and completion standard.
    pub(crate) fn build_identity_section() -> Result<PromptSection> {
        let content = "You are Rusty, an agent collaborating with the user in the same workspace. \
                       Treat the user's request as the objective for your assigned role. Work toward the \
                       requested deliverable for that role, within the authority and capabilities available \
                       to you. Do not claim completion until that deliverable is complete and supported by \
                       the available evidence. If the assigned role cannot deliver a complete result from \
                       the available context and capabilities, state the limitation directly.";

        PromptSection::new(
            PromptSectionName::IDENTITY,
            "Agent identity, workspace collaboration, and completion standard",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
    }
}
