//! Fluent builder and helpers for constructing validated prompt sections.

use ra_core::error::{Error, Result};
use ra_core::prompt::{
    ContentHash, PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

pub use ra_core::prompt::estimate_tokens;

/// Fluent builder for [`PromptSection`].
#[must_use]
pub struct PromptSectionBuilder {
    name: PromptSectionName,
    purpose: Option<String>,
    source: PromptSource,
    stability: SectionStability,
    position: SectionPosition,
    token_estimate: Option<usize>,
    token_budget: Option<usize>,
    content: Option<String>,
}

impl PromptSectionBuilder {
    /// Creates a new builder initialized with default stability and prefix position.
    pub fn new(name: impl Into<PromptSectionName>) -> Self {
        Self {
            name: name.into(),
            purpose: None,
            source: PromptSource::Builtin,
            stability: SectionStability::Stable,
            position: SectionPosition::Prefix,
            token_estimate: None,
            token_budget: None,
            content: None,
        }
    }

    /// Sets the section's purpose description.
    pub fn purpose(mut self, purpose: impl Into<String>) -> Self {
        self.purpose = Some(purpose.into());
        self
    }

    /// Sets the provenance source.
    pub fn source(mut self, source: PromptSource) -> Self {
        self.source = source;
        self
    }

    /// Marks the section as stable across turns.
    pub fn stable(mut self) -> Self {
        self.stability = SectionStability::Stable;
        self
    }

    /// Marks the section as volatile across turns.
    pub fn volatile(mut self) -> Self {
        self.stability = SectionStability::Volatile;
        self
    }

    /// Sets the stability classification directly.
    pub fn stability(mut self, stability: SectionStability) -> Self {
        self.stability = stability;
        self
    }

    /// Sets the position to the stable prefix.
    pub fn prefix(mut self) -> Self {
        self.position = SectionPosition::Prefix;
        self
    }

    /// Sets the position to the tail message.
    pub fn tail_message(mut self) -> Self {
        self.position = SectionPosition::TailMessage;
        self
    }

    /// Sets the position directly.
    pub fn position(mut self, position: SectionPosition) -> Self {
        self.position = position;
        self
    }

    /// Overrides the estimated token count.
    pub fn token_estimate(mut self, estimate: usize) -> Self {
        self.token_estimate = Some(estimate);
        self
    }

    /// Declares the largest share of the cached prefix this section may spend.
    ///
    /// See [`PromptSection::with_token_budget`] for what the declaration means, and
    /// [`PromptAssembler::assemble`](crate::assembler::PromptAssembler::assemble) for where it is
    /// enforced.
    pub fn token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Sets the text content for the section.
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// Builds the validated [`PromptSection`].
    ///
    /// # Errors
    ///
    /// Returns an error if content is missing, or if a volatile section is assigned
    /// to the prefix position.
    pub fn build(self) -> Result<PromptSection> {
        let content = self.content.ok_or_else(|| {
            Error::config(format!(
                "prompt section `{}` content is required",
                self.name
            ))
        })?;

        let purpose = self
            .purpose
            .unwrap_or_else(|| format!("Prompt section for {}", self.name));

        let mut section = PromptSection::new(
            self.name,
            purpose,
            self.source,
            self.stability,
            self.position,
            content,
        )?;

        if let Some(estimate) = self.token_estimate {
            section = section.with_token_estimate(estimate);
        }
        if let Some(budget) = self.token_budget {
            section = section.with_token_budget(budget);
        }

        Ok(section)
    }
}

/// Helper function to compute content hash for text.
#[must_use]
pub fn compute_content_hash(text: &str) -> ContentHash {
    ContentHash::compute(text.as_bytes())
}
