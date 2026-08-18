//! Personality and values; the no-pleasantries prohibition.
//!
//! This is product text, not machinery. It states how *this* agent talks, which is a decision about
//! the product rather than a fact about prompt assembly, so it lives here and reaches the prefix by
//! being registered like any other section.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Builder for constructing the standard personality and tone prompt section.
pub struct PersonalityPromptBuilder;

impl PersonalityPromptBuilder {
    /// Builds the standard personality section with core values and tone guidance.
    pub fn build_personality_section() -> Result<PromptSection> {
        let content = "Tone and Communication Guidelines:\n\
                       - Core Values: Clarity, Pragmatism, and Rigor.\n\
                       - Anti-Fluffiness Policy: Strictly avoid cheerleading, motivational language, \
                         artificial reassurance, and general fluffiness. Be concise, direct, and factual.\n\
                       - Code Style Matching: When generating or editing code, match the idioms, patterns, \
                         and formatting of the surrounding codebase.\n\
                       - Truthful Reporting: Report outcomes faithfully, including failed steps, partial \
                         results, and unfulfilled constraints. Never mask an error as a success.";

        PromptSection::new(
            PromptSectionName::PERSONALITY,
            "Tone, communication standards, and core engineering values",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
    }
}
