//! The prompts memory generation runs with.
//!
//! A port of the generation half of the reference's `sandbox/memory/prompts.py`. The templates are
//! the reference's (MIT), carried verbatim: they are what the extraction and consolidation models
//! read, and a paraphrase would be a different instruction. The read prompt lives with the memory
//! capability in `ra-tools`. The license and the list of copied files are in the repository's
//! `THIRD_PARTY_NOTICES.md`.

use super::json::python_strip;
use super::storage::PhaseTwoInputSelection;

/// The reference's `memory_consolidation_prompt.md`, phase two's prompt.
pub const MEMORY_CONSOLIDATION_PROMPT_TEMPLATE: &str =
    include_str!("prompts/memory_consolidation_prompt.md");

/// The reference's `rollout_extraction_prompt.md`, phase one's instructions.
pub const ROLLOUT_EXTRACTION_PROMPT_TEMPLATE: &str =
    include_str!("prompts/rollout_extraction_prompt.md");

/// The reference's `rollout_extraction_user_message.md`, phase one's input.
pub const ROLLOUT_EXTRACTION_USER_MESSAGE_TEMPLATE: &str =
    include_str!("prompts/rollout_extraction_user_message.md");

const EXTRA_PROMPT_PLACEHOLDER: &str = "{{ extra_prompt_section }}";
const PHASE_TWO_INPUT_SELECTION_PLACEHOLDER: &str = "{{ phase_two_input_selection }}";
const MEMORY_ROOT_PLACEHOLDER: &str = "{{ memory_root }}";

const EXTRA_PROMPT_SECTION_TEMPLATE: &str = "\
============================================================
DEVELOPER-SPECIFIC EXTRA GUIDANCE
============================================================

The developer provided additional guidance for memory writing. Pay extra attention to
capturing these details when they would be useful for future runs, in addition to the
standard user preferences, failure recovery, and task summary signals. Keep following the
schema, safety, and evidence rules above.

{extra_prompt}
";

/// Phase two's prompt: the consolidation template with the memory root, what it is given, and the
/// developer's guidance filled in, in that order.
#[must_use]
pub fn render_memory_consolidation_prompt(
    memory_root: &str,
    selection: &PhaseTwoInputSelection,
    extra_prompt: Option<&str>,
) -> String {
    MEMORY_CONSOLIDATION_PROMPT_TEMPLATE
        .replace(MEMORY_ROOT_PLACEHOLDER, memory_root)
        .replace(
            PHASE_TWO_INPUT_SELECTION_PLACEHOLDER,
            &render_phase_two_input_selection(selection),
        )
        .replace(
            EXTRA_PROMPT_PLACEHOLDER,
            &render_extra_prompt_section(extra_prompt),
        )
}

/// Phase one's instructions, with the developer's guidance filled in.
#[must_use]
pub fn render_rollout_extraction_prompt(extra_prompt: Option<&str>) -> String {
    ROLLOUT_EXTRACTION_PROMPT_TEMPLATE.replace(
        EXTRA_PROMPT_PLACEHOLDER,
        &render_extra_prompt_section(extra_prompt),
    )
}

/// Phase one's input: the rollout's terminal metadata and its contents.
///
/// Filled in as the reference's `str.format` fills it, in one pass, so a placeholder spelled
/// inside one value is not replaced by the other.
#[must_use]
pub fn render_rollout_extraction_user_prompt(
    terminal_metadata_json: &str,
    rollout_contents: &str,
) -> String {
    format_placeholders(
        ROLLOUT_EXTRACTION_USER_MESSAGE_TEMPLATE,
        &[
            ("terminal_metadata_json", terminal_metadata_json),
            ("rollout_contents", rollout_contents),
        ],
    )
}

/// Python's `str.format` for a template whose only fields are the named ones: each `{name}` is
/// replaced by its value, `{{` and `}}` become single braces, and nothing substituted is read
/// again.
fn format_placeholders(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(index) = rest.find(['{', '}']) {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        if let Some(after) = rest.strip_prefix("{{") {
            out.push('{');
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("}}") {
            out.push('}');
            rest = after;
            continue;
        }
        let replaced = values.iter().find_map(|(name, value)| {
            rest.strip_prefix('{')
                .and_then(|after| after.strip_prefix(*name))
                .and_then(|after| after.strip_prefix('}'))
                .map(|after| (*value, after))
        });
        if let Some((value, after)) = replaced {
            out.push_str(value);
            rest = after;
        } else {
            out.push_str(&rest[..1]);
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The guidance section, or nothing when there is no guidance.
fn render_extra_prompt_section(extra_prompt: Option<&str>) -> String {
    let Some(extra_prompt) = extra_prompt
        .map(python_strip)
        .filter(|text| !text.is_empty())
    else {
        return String::new();
    };
    format!(
        "\n{}",
        format_placeholders(
            EXTRA_PROMPT_SECTION_TEMPLATE,
            &[("extra_prompt", extra_prompt)]
        )
    )
}

/// What phase two is told about its input: counts, then each selected and each removed raw memory.
fn render_phase_two_input_selection(selection: &PhaseTwoInputSelection) -> String {
    let retained = selection.retained_rollout_ids().len();
    let added = selection.selected().len().saturating_sub(retained);
    let selected_lines = if selection.selected().is_empty() {
        "- none".to_owned()
    } else {
        selection
            .selected()
            .iter()
            .map(|item| {
                let status = if selection.retained_rollout_ids().contains(item.rollout_id()) {
                    "retained"
                } else {
                    "added"
                };
                format!(
                    "- [{status}] rollout_id={}, rollout_summary_file={}, updated_at={}",
                    item.rollout_id(),
                    item.rollout_summary_file(),
                    or_unknown(item.updated_at())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let removed_lines = if selection.removed().is_empty() {
        "- none".to_owned()
    } else {
        selection
            .removed()
            .iter()
            .map(|item| {
                format!(
                    "- rollout_id={}, rollout_summary_file={}, updated_at={}",
                    item.rollout_id(),
                    item.rollout_summary_file(),
                    or_unknown(item.updated_at())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "- selected inputs this run: {}\n\
         - newly added since the last successful Phase 2 run: {added}\n\
         - retained from the last successful Phase 2 run: {retained}\n\
         - removed from the last successful Phase 2 run: {}\n\n\
         Current selected Phase 1 inputs:\n{selected_lines}\n\n\
         Removed from the last successful Phase 2 selection:\n{removed_lines}\n",
        selection.selected().len(),
        selection.removed().len(),
    )
}

fn or_unknown(updated_at: &str) -> &str {
    if updated_at.is_empty() {
        "unknown"
    } else {
        updated_at
    }
}
