//! Deterministic assembly of stable prefix prompt sections.

use std::collections::{HashMap, HashSet};
use std::fmt;

use ra_core::error::{Error, Result};
use ra_core::prompt::{ContentHash, PromptSection, PromptSectionName};

/// Assembled stable system instructions with prefix hash and section metadata.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq)]
pub struct StablePrefix {
    system_instructions: String,
    prefix_hash: ContentHash,
    sections: Vec<PromptSection>,
    token_estimate: usize,
}

impl StablePrefix {
    fn new(
        system_instructions: String,
        prefix_hash: ContentHash,
        sections: Vec<PromptSection>,
        token_estimate: usize,
    ) -> Self {
        Self {
            system_instructions,
            prefix_hash,
            sections,
            token_estimate,
        }
    }

    /// The combined system instructions text.
    #[must_use]
    pub fn system_instructions(&self) -> &str {
        &self.system_instructions
    }

    /// SHA-256 hash of the complete stable prefix text.
    #[must_use]
    pub const fn prefix_hash(&self) -> &ContentHash {
        &self.prefix_hash
    }

    /// Prompt sections included in this stable prefix in assembly order.
    #[must_use]
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Estimated token count for the assembled prefix.
    #[must_use]
    pub const fn token_estimate(&self) -> usize {
        self.token_estimate
    }
}

impl fmt::Debug for StablePrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StablePrefix")
            .field("bytes", &self.system_instructions.len())
            .field("prefix_hash", &self.prefix_hash)
            .field("sections_count", &self.sections.len())
            .field("token_estimate", &self.token_estimate)
            .finish_non_exhaustive()
    }
}

/// Canonical section order, first to last in the assembled prefix.
///
/// **This list is the order of the cached artifact**, so moving an entry invalidates every
/// provider's cached prefix exactly as rewriting a section would. Introducing a topic is an
/// insertion here rather than a renumbering of ranks, which keeps the diff that reviewers see equal
/// to the change that was actually made.
static CANONICAL_SECTION_ORDER: [PromptSectionName; 12] = [
    PromptSectionName::IDENTITY,
    PromptSectionName::CORE_BEHAVIOR,
    PromptSectionName::TOOL_USE,
    PromptSectionName::TOOL_SURFACE,
    PromptSectionName::SAFETY,
    PromptSectionName::EDITING_VERIFICATION,
    PromptSectionName::AUTONOMY,
    PromptSectionName::CHANNELS,
    PromptSectionName::FINAL_ANSWER,
    PromptSectionName::CONTEXT_DURABILITY,
    PromptSectionName::PERSONALITY,
    PromptSectionName::ROLE,
];

/// Canonical section order priority.
///
/// A name with no canonical position sorts after every name that has one, then by the name itself,
/// so an unregistered topic lands at the end of the prefix deterministically rather than wherever a
/// hash iteration happened to put it.
fn section_order_rank(name: &PromptSectionName) -> (usize, &str) {
    let rank = CANONICAL_SECTION_ORDER
        .iter()
        .position(|canonical| canonical == name)
        .unwrap_or(CANONICAL_SECTION_ORDER.len());
    (rank, name.as_str())
}

/// Assembler that combines stable prompt sections into a deterministic stable prefix.
#[derive(Clone, Default)]
pub struct PromptAssembler {
    sections: HashMap<PromptSectionName, PromptSection>,
}

impl PromptAssembler {
    /// Creates a new, empty prompt assembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sections: HashMap::new(),
        }
    }

    /// Adds a prompt section, replacing any section already registered under that name.
    ///
    /// Only stable prefix sections belong here. A tail section is rejected at insertion rather
    /// than filtered out at assembly: accepting it and returning `Ok` would tell the caller their
    /// text is in the prefix right up until they notice it is not in the output. Tail content
    /// travels through the reminder channel instead.
    ///
    /// Replacement is the point of the single-section form — swapping one role's guidance for
    /// another's is an override that reads as one at the call site. Registering a whole list is a
    /// different act, and [`Self::with_sections`] refuses a name that is already registered for the
    /// reason above: the losing section would be gone from the prefix with nothing said about it.
    ///
    /// # Errors
    ///
    /// Returns an error if the section is volatile or is not positioned in the prefix.
    pub fn add_section(mut self, section: PromptSection) -> Result<Self> {
        if !section.position().is_prefix() {
            return Err(Error::config(format!(
                "cannot add section `{}` at position `{}` to the prefix assembler; it assembles \
                 the stable prefix only",
                section.name(),
                section.position()
            )));
        }
        if section.stability().is_volatile() {
            return Err(Error::config(format!(
                "cannot add volatile section `{}` to prefix assembler",
                section.name()
            )));
        }
        self.sections.insert(section.name().clone(), section);
        Ok(self)
    }

    /// Adds multiple prompt sections, rejecting any name that is already registered.
    ///
    /// A batch is a registration list. A name appearing twice in one, or landing on a section an
    /// earlier call installed, means two builders claim the same slot; [`Self::add_section`]'s
    /// replacement would resolve that by dropping one of them, leaving a section that was built,
    /// validated and never sent — visible only as a line missing from the next prompt dump.
    /// Deliberate replacement is still available by calling `add_section` separately.
    ///
    /// # Errors
    ///
    /// Returns an error if a section name is already registered, or for the per-section failures
    /// described on [`Self::add_section`].
    pub fn with_sections(
        mut self,
        sections: impl IntoIterator<Item = PromptSection>,
    ) -> Result<Self> {
        let mut claimed: HashSet<PromptSectionName> = self.sections.keys().cloned().collect();
        for section in sections {
            if !claimed.insert(section.name().clone()) {
                return Err(Error::config(format!(
                    "prompt section `{}` is already registered; use `add_section` for an explicit \
                     replacement",
                    section.name()
                )));
            }
            self = self.add_section(section)?;
        }
        Ok(self)
    }

    /// Assembles all registered prefix sections into a [`StablePrefix`].
    ///
    /// Sections are ordered deterministically by canonical priority and joined with double newlines.
    pub fn assemble(&self) -> Result<StablePrefix> {
        let mut prefix_sections: Vec<PromptSection> = self
            .sections
            .values()
            .filter(|s| s.position().is_prefix() && s.stability().is_stable())
            .cloned()
            .collect();

        prefix_sections.sort_by(|a, b| {
            let rank_a = section_order_rank(a.name());
            let rank_b = section_order_rank(b.name());
            rank_a.cmp(&rank_b)
        });

        let system_instructions = prefix_sections
            .iter()
            .map(|s| s.content().trim())
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        let prefix_hash = ContentHash::compute(system_instructions.as_bytes());

        // Sum the sections rather than re-estimating the joined text. The dump reports both this
        // total and the per-section rows, and re-estimating makes the two disagree by rounding
        // alone — while also discarding any exact count `with_token_estimate` supplied from a real
        // tokenizer, which is the only reason that override exists.
        let token_estimate = prefix_sections
            .iter()
            .map(PromptSection::token_estimate)
            .sum();

        Ok(StablePrefix::new(
            system_instructions,
            prefix_hash,
            prefix_sections,
            token_estimate,
        ))
    }
}
