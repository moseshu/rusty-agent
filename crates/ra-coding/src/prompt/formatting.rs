//! Final-answer formatting rules.
//!
//! Scoped against [`personality`](super::personality), which already owns what the agent says and
//! how it sounds. What is left here is how that text renders: the product's UI reads
//! GitHub-flavored Markdown, so a nested list or a heading marker is a visible defect rather than a
//! matter of taste. Restating the tone rules would put one instruction in two cached sections,
//! where editing either one leaves the model holding both versions of it.
//!
//! This section uses the `final_answer` slot for its canonical placement in the stable prefix.
//! Other prompt topics choose their own section boundary when they are introduced.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Builder for the coding agent's final-answer formatting section.
pub(crate) struct FormattingPromptBuilder;

impl FormattingPromptBuilder {
    /// Builds the stable section that defines how responses render in the product UI.
    pub(crate) fn build_formatting_section() -> Result<PromptSection> {
        let content = "Output Formatting:\n\
                       - Write responses as GitHub-flavored Markdown.\n\
                       - Use a header only when it improves scannability. Write it as bold text, \
                         such as `**Short Header**`, in Title Case and one to three words; do not \
                         use Markdown heading syntax.\n\
                       - Keep lists flat. Do not nest bullets; when hierarchy is necessary, split \
                         it into separate lists or sections. For ordered lists, use only `1.` \
                         numbering, never `1)`.\n\
                       - When referencing a real local file, use a clickable Markdown link with a \
                         plain label and an absolute path, such as `[app.rs](/absolute/path/app.rs:12)`. \
                         Do not wrap the link or path in code spans, use file URIs, or cite a line range.\n\
                       - Do not use emoji or em dashes unless the user explicitly asks.";

        PromptSection::new(
            PromptSectionName::FINAL_ANSWER,
            "GitHub-flavored Markdown response structure and local file-reference rules",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
    }
}
