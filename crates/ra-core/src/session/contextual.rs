// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Model-only context recognized when projecting or counting persisted user-role messages.
//!
//! The session protocol owns this recognition so storage and client projections share the same
//! rule without depending on a tool implementation. It does not select or load skills.

use super::interrupt::is_interrupted_turn_marker;
use crate::item::{Message, MessageRole};

/// Recognizes the selected-skill fragment, including legacy records without content annotations.
/// Matches Codex's contextual-fragment boundary check: trim whitespace and compare both markers
/// case-insensitively. Text merely containing these tags is not a fragment.
#[must_use]
pub fn is_skill_prompt_fragment(text: &str) -> bool {
    matches_fragment(text, "<skill>", "</skill>")
}

fn matches_fragment(text: &str, start_marker: &str, end_marker: &str) -> bool {
    let trimmed = text.trim_start();
    let starts_with_marker = trimmed
        .get(..start_marker.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(start_marker));
    let trimmed = trimmed.trim_end();
    let ends_with_marker = trimmed
        .get(trimmed.len().saturating_sub(end_marker.len())..)
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(end_marker));
    starts_with_marker && ends_with_marker
}

/// Whether a user-role message contains model-only skill, interruption, or subagent context.
/// A matching text block hides the entire message, as in Codex's user-message projection.
#[must_use]
pub fn is_contextual_user_message(message: &Message) -> bool {
    message.role() == MessageRole::User
        && (is_interrupted_turn_marker(message)
            || message
                .content()
                .iter()
                .filter_map(|block| block.as_text())
                .any(|text| {
                    is_skill_prompt_fragment(text)
                        || matches_fragment(
                            text,
                            "<subagent_notification>",
                            "</subagent_notification>",
                        )
                }))
}
