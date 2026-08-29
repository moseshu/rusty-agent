//! Registration of every `PromptSection`, and the assembly of the agent's stable prefix.
//!
//! This module is the assembly layer for prompts. The loop kernel cannot reach `ra-prompt` — it
//! depends on `ra-core` alone, by design — so nothing below this crate can turn sections into a
//! prefix. If assembly did not happen here, an agent's system instructions would be whatever bare
//! string its declaration happened to carry, and none of the sectioning, ordering, prefix hashing,
//! or dump governance would apply to what the model actually receives.
//!
//! The per-topic modules alongside this one hold the product's own prompt text. Seven are written:
//! `identity`, `engineering`, `editing`, `autonomy`, [`personality`], [`role`], and the generated
//! `tool_surface`. The rest are still empty registration slots: writing that text is a separate
//! piece of work, and filling them with placeholders would freeze wording nobody has decided on.
//! Each one lands by adding a section to [`assemble_stable_prefix_for_tools`], which is why the
//! assembly is a list rather than a hardcoded concatenation.
//!
//! A module and the section it builds are named separately, and the section name is the one that
//! has to be unique — `engineering` builds `core_behavior`. Two modules reaching for the same
//! section name is caught at assembly rather than silently resolved, which is what makes claiming a
//! shared slot safe.

pub(crate) mod autonomy;
pub(crate) mod channels;
pub(crate) mod editing;
pub(crate) mod engineering;
pub(crate) mod formatting;
pub(crate) mod frontend;
pub(crate) mod identity;
pub mod personality;
pub mod role;
mod tool_surface;
pub(crate) mod tool_use;

use std::sync::Arc;

use ra_core::error::Result;
use ra_core::prompt::PromptRole;
use ra_core::tool::Tool;
use ra_prompt::assembler::{PromptAssembler, StablePrefix};

use self::autonomy::AutonomyPromptBuilder;
use self::editing::EditingPromptBuilder;
use self::engineering::EngineeringPromptBuilder;
use self::identity::IdentityPromptBuilder;
use self::personality::PersonalityPromptBuilder;
use self::role::RolePromptBuilder;
use self::tool_surface::ToolSurfacePromptBuilder;

/// Assembles the stable system-instruction prefix for one role.
///
/// **Order is part of the cached artifact, not a presentation choice.** The prefix is the span
/// every provider's prompt cache holds, so reordering two sections invalidates it exactly as
/// rewriting one would. The order does not come from the registration list below: the assembler
/// sorts by a canonical rank keyed on the section *name*, so a section registered without a rank of
/// its own lands at the end of the prefix no matter where its call sits. Adding a section therefore
/// means adding its rank next to the others in `ra-prompt`, and the prompt-dump snapshot is what
/// locks the result.
///
/// # Errors
///
/// Propagates section construction and assembly failures, including a section that asks for a
/// placement the stable prefix cannot give it.
pub fn assemble_stable_prefix(role: &PromptRole) -> Result<StablePrefix> {
    assemble_stable_prefix_for_tools(role, &[])
}

/// Assembles the stable system-instruction prefix for one role and its real tool surface.
///
/// Tool declarations must be passed from the same agent construction path that installs them.
/// Building the inventory from a separate list would let the prompt promise a name the runtime
/// cannot dispatch, or omit one the provider has advertised.
///
/// The section this adds lists only the advertised names. The schema fingerprint that makes a
/// description-only edit reviewable belongs to [`tool_surface_snapshot`], which is not
/// model-visible and therefore does not spend cached-prefix tokens on a digest.
///
/// # Errors
///
/// Propagates section construction and assembly failures, and rejects two tools that advertise the
/// same model-facing name — the prompt would name one entry for a dispatch the agent will refuse
/// to build.
pub fn assemble_stable_prefix_for_tools(
    role: &PromptRole,
    tools: &[Arc<dyn Tool>],
) -> Result<StablePrefix> {
    let apply_patch_is_advertised =
        ToolSurfacePromptBuilder::contains_advertised_tool(tools, "apply_patch");
    let mut sections = vec![
        IdentityPromptBuilder::build_identity_section()?,
        EngineeringPromptBuilder::build_engineering_section()?,
        EditingPromptBuilder::build_editing_section(role, apply_patch_is_advertised)?,
        AutonomyPromptBuilder::build_autonomy_section()?,
        PersonalityPromptBuilder::build_personality_section()?,
        RolePromptBuilder::build_role_section(role)?,
    ];
    if let Some(tool_surface) = ToolSurfacePromptBuilder::build_tool_surface_section(tools)? {
        sections.push(tool_surface);
    }

    PromptAssembler::new().with_sections(sections)?.assemble()
}

/// Renders the committed review record of one agent's advertised tool surface.
///
/// Deliberately separate from [`assemble_stable_prefix_for_tools`]: this text is written down for
/// review, not sent to the model, so it carries the revision and the schema fingerprint that the
/// prompt section leaves out. `None` when nothing is advertised.
///
/// # Errors
///
/// Propagates advertised-schema rendering failures.
pub fn tool_surface_snapshot(tools: &[Arc<dyn Tool>]) -> Result<Option<String>> {
    ToolSurfacePromptBuilder::build_surface_digest(tools)
}
