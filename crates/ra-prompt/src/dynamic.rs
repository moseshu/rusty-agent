//! Dynamic prompt evaluation, resolution, and tail message lowering.
//!
//! The placement rule — a generated prompt may only occupy volatile tail messages — is enforced by
//! [`ResolvedPrompt`] itself rather than here. Turn preparation has to apply the same rule and
//! cannot reach this crate, so a copy of the check living here would be a second implementation
//! free to disagree with the one that actually guards the model request.

use ra_core::context::RunContext;
use ra_core::error::Result;
use ra_core::item::ModelInputItem;
use ra_core::prompt::{DynamicPromptHandler, PromptSection, ResolvedPrompt};

/// Evaluates a dynamic prompt generator against the live runtime context.
///
/// Provenance metadata rides on the returned [`ResolvedPrompt`]; use
/// [`ResolvedPrompt::provenance_record`] to retain it once the text has been lowered.
pub async fn resolve_dynamic_prompt(
    handler: &dyn DynamicPromptHandler,
    context: &RunContext,
) -> Result<ResolvedPrompt> {
    handler.resolve(context).await
}

/// Converts a dynamic resolved prompt into volatile tail message items.
///
/// # Errors
///
/// Returns an error if the prompt asks for the stable prefix.
pub fn resolved_to_tail_input_items(resolved: &ResolvedPrompt) -> Result<Vec<ModelInputItem>> {
    resolved.lower_to_tail_items()
}

/// Converts a dynamic resolved prompt into a collection of volatile prompt sections.
///
/// # Errors
///
/// Returns an error if the prompt asks for the stable prefix or declares a stable section.
pub fn resolved_to_volatile_sections(resolved: &ResolvedPrompt) -> Result<Vec<PromptSection>> {
    resolved.volatile_tail_sections()
}
