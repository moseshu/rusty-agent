//! Registration of every `PromptSection`, and the assembly of the agent's stable prefix.
//!
//! This module is the assembly layer for prompts. The loop kernel cannot reach `ra-prompt` — it
//! depends on `ra-core` alone, by design — so nothing below this crate can turn sections into a
//! prefix. If assembly did not happen here, an agent's system instructions would be whatever bare
//! string its declaration happened to carry, and none of the sectioning, ordering, prefix hashing,
//! or dump governance would apply to what the model actually receives.
//!
//! The per-topic modules alongside this one hold the product's own prompt text. Three are written:
//! `identity`, [`personality`], and [`role`]. The rest are still empty registration slots: writing
//! that text is a separate piece of work, and filling them with placeholders would freeze wording
//! nobody has decided on. Each one lands by adding a section to [`assemble_stable_prefix`], which
//! is why the assembly is a list rather than a hardcoded concatenation.

pub(crate) mod autonomy;
pub(crate) mod channels;
pub(crate) mod editing;
pub(crate) mod engineering;
pub(crate) mod formatting;
pub(crate) mod frontend;
pub(crate) mod identity;
pub mod personality;
pub mod role;
pub(crate) mod tool_use;

use ra_core::error::Result;
use ra_core::prompt::PromptRole;
use ra_prompt::assembler::{PromptAssembler, StablePrefix};

use self::identity::IdentityPromptBuilder;
use self::personality::PersonalityPromptBuilder;
use self::role::RolePromptBuilder;

/// Assembles the stable system-instruction prefix for one role.
///
/// **Order is part of the cached artifact, not a presentation choice.** The prefix is the span
/// every provider's prompt cache holds, so reordering two sections invalidates it exactly as
/// rewriting one would. The order is therefore fixed here and locked by the prompt-dump snapshot,
/// rather than emerging from the order registration calls happen to run in.
///
/// # Errors
///
/// Propagates section construction and assembly failures, including a section that asks for a
/// placement the stable prefix cannot give it.
pub fn assemble_stable_prefix(role: &PromptRole) -> Result<StablePrefix> {
    PromptAssembler::new()
        .with_sections([
            IdentityPromptBuilder::build_identity_section()?,
            PersonalityPromptBuilder::build_personality_section()?,
            RolePromptBuilder::build_role_section(role)?,
        ])?
        .assemble()
}
