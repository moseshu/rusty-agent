//! Role-specific prompt text and capability constraints.
//!
//! The read-only text is the clearest reason this belongs to the product rather than to the prompt
//! crate: it enumerates `touch`, `rm`, `mv`, redirection, heredocs and `/tmp`, which are constants
//! of an agent that runs a shell. A product that runs no shell would inherit guidance about one.
//!
//! It is also load bearing for the tool surface — the read-only text states which capabilities are
//! absent, and it has to agree with the tools actually advertised. That agreement is a product
//! invariant, and it is checked where both halves are known: here.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptRole, PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Builder for generating role-specific prompt sections.
pub struct RolePromptBuilder;

impl RolePromptBuilder {
    /// Generates the standard prompt section for the given [`PromptRole`].
    pub fn build_role_section(role: &PromptRole) -> Result<PromptSection> {
        Self::build_role_section_with_custom_text(role, None)
    }

    /// Generates a role section, overriding the guidance text.
    ///
    /// `custom_text` wins over the builtin guidance whenever it is supplied. The earlier form
    /// preferred the builtin and dropped the argument for the five named roles, so a caller
    /// tailoring the `Main` role got the stock text with no indication their override had gone
    /// nowhere — the same silent fallback the dynamic-prompt contract rejects, one layer down.
    ///
    /// # Errors
    ///
    /// Returns an error if the resulting section fails validation.
    pub fn build_role_section_with_custom_text(
        role: &PromptRole,
        custom_text: Option<&str>,
    ) -> Result<PromptSection> {
        let builtin: Option<&str> = match role {
            PromptRole::Main => Some(
                "You are the main autonomous agent. You have access to tools to explore the codebase, \
                 edit files, run commands, and accomplish tasks directly.",
            ),
            PromptRole::ReadOnlySpecialist => Some(
                "You are a read-only specialist agent. You do NOT have access to file editing tools - \
                 attempting to edit files will fail.\n\n\
                 Command execution constraints: You must not execute state-altering or file-writing \
                 commands. Specifically, commands using `touch`, `rm`, `mv`, `cp`, file output \
                 redirection (`>`, `>>`), pipeline writes, heredocs, or creating temporary files in \
                 `/tmp` or other directories are strictly forbidden.",
            ),
            PromptRole::Planner => Some(
                "You are an architectural planning agent. You explore the repository in read-only \
                 mode, understand design constraints, and produce actionable, structured plans \
                 without modifying source files directly.",
            ),
            PromptRole::OneOffAnswer => Some(
                "You are answering a standalone, one-off question without tool execution.\n\n\
                 Constraints:\n\
                 1. Do not refer to previous in-progress work, interruptions, or context switches.\n\
                 2. Do not promise future actions (e.g., do not say 'Let me check...', 'I will investigate...', or 'Let me try...').\n\
                 3. If the answer is unknown or not present in context, state that directly without offering to search or investigate.",
            ),
            PromptRole::Coordinator => Some(
                "You are a coordinator agent responsible for breaking down high-level objectives, \
                 dispatching work to specialized subagents, and synthesizing results into a unified outcome.",
            ),
            // `Custom` has no builtin text, and `PromptRole` is non-exhaustive, so a variant added
            // upstream lands here too. Both fall back to the caller's text rather than to guidance
            // written for a different role.
            _ => None,
        };

        let content = custom_text.or(builtin).map_or_else(
            || format!("Specialized agent role: {}", role.role_name()),
            str::to_owned,
        );

        PromptSection::new(
            PromptSectionName::ROLE,
            format!("Role guidance for {}", role.role_name()),
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
    }
}
