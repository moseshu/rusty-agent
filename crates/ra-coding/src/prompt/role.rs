// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Role-specific prompt text and capability constraints.
//!
//! Roles constrain behavior without changing inherited sandbox authority. Tool-availability
//! statements are generated from the advertised surface. Planner guidance ports Codex's Plan
//! template, adapting references when a product tier does not offer the named control tools.
//! Missing or ambiguous adaptation markers fail assembly with a configuration error.

use ra_core::error::{Error, Result};
use ra_core::prompt::{
    PromptRole, PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Cached-prefix allowance for this section, in estimated tokens.
///
/// Stances use the ordinary allowance; specialist examples and Plan instructions have larger,
/// separately reviewed allowances.
const TOKEN_BUDGET: usize = 192;
const SPECIALIST_TOKEN_BUDGET: usize = 640;
const PLAN_TOKEN_BUDGET: usize = 3072;

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
    /// Returns an error if template adaptation or validation of the resulting section fails.
    pub fn build_role_section_with_custom_text(
        role: &PromptRole,
        custom_text: Option<&str>,
    ) -> Result<PromptSection> {
        Self::build(role, custom_text, &[], false)
    }

    /// Builds root-agent guidance from names advertised by the assembled tool surface.
    /// Product agent construction additionally supplies the spawned-agent audience.
    pub fn build_role_section_for_tools(
        role: &PromptRole,
        tool_names: &[&str],
    ) -> Result<PromptSection> {
        Self::build_role_section_for_session(role, tool_names, false)
    }

    pub(crate) fn build_role_section_for_session(
        role: &PromptRole,
        tool_names: &[&str],
        spawned_agent: bool,
    ) -> Result<PromptSection> {
        Self::build(role, None, tool_names, spawned_agent)
    }

    fn build(
        role: &PromptRole,
        custom_text: Option<&str>,
        tool_names: &[&str],
        spawned_agent: bool,
    ) -> Result<PromptSection> {
        let builtin: Option<&str> = match role {
            PromptRole::Main => Some(
                "You are the main autonomous agent. You have access to tools to explore the codebase, \
                 edit files, run commands, and accomplish tasks directly.",
            ),
            PromptRole::ReadOnlySpecialist => Some(
                "You are a read-only specialist agent. Explore the repository and report findings. \
                 Use tools only for non-mutating exploration; do not edit repository files or run \
                 commands that implement changes.",
            ),
            PromptRole::Planner => Some("You are an architectural planning agent."),
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

        let mut content = custom_text.or(builtin).map_or_else(
            || format!("Specialized agent role: {}", role.role_name()),
            str::to_owned,
        );

        if custom_text.is_none() && role.is_read_only() {
            if matches!(role, PromptRole::Planner) {
                content.push_str("\n\n");
                content.push_str(&planner_guidance(PLAN_MODE, tool_names, spawned_agent)?);
            } else {
                content.push_str("\n\n");
                content.push_str(&specialist_non_mutating_guidance()?);
            }
            content.push_str(
                "\n\nTool access inherits the parent session's sandbox and permissions. \
                              Role instructions do not grant additional authority.",
            );
            if tool_names.contains(&"apply_patch") {
                content.push_str(" The dedicated file editing tool is available; this role must still avoid repository mutation.");
            } else {
                content.push_str(" The dedicated file editing tool is not advertised.");
            }
            if !tool_names.iter().any(|name| {
                matches!(
                    name.rsplit('.').next(),
                    Some(
                        "spawn_agent"
                            | "send_message"
                            | "followup_task"
                            | "interrupt_agent"
                            | "wait_agent"
                            | "list_agents"
                    )
                )
            }) {
                content.push_str(" No collaboration tools are advertised; do not delegate tasks or direct other agents.");
            }
        }

        PromptSection::new(
            PromptSectionName::ROLE,
            format!("Role guidance for {}", role.role_name()),
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .map(|section| {
            section.with_token_budget(if matches!(role, PromptRole::Planner) {
                PLAN_TOKEN_BUDGET
            } else if matches!(role, PromptRole::ReadOnlySpecialist) {
                SPECIALIST_TOKEN_BUDGET
            } else {
                TOKEN_BUDGET
            })
        })
    }
}

pub(crate) fn planner_guidance(
    template: &str,
    tool_names: &[&str],
    spawned_agent: bool,
) -> Result<String> {
    let mut plan = template.to_owned();
    replace_template_text(
        &mut plan,
        "wrap it in a `<proposed_plan>` block so the client can render it specially:",
        "wrap it in a `<proposed_plan>` block as plain-text plan markup:",
    )?;
    if !tool_names.contains(&"update_plan") {
        let start = template_marker_offset(&plan, "## Plan Mode vs update_plan tool")?;
        let end = template_marker_offset(&plan, "## Execution vs. mutation")?;
        if end <= start {
            return Err(Error::config(
                "Plan template has out-of-order update_plan section boundaries",
            ));
        }
        plan.replace_range(start..end, "");
        if plan.contains("update_plan") {
            return Err(Error::config(
                "Plan template has an unadapted update_plan reference",
            ));
        }
    }
    if spawned_agent || !tool_names.contains(&"request_user_input") {
        replace_template_text(
            &mut plan,
            "Strongly prefer using the `request_user_input` tool to ask any questions.",
            "Ask direct questions when clarification is necessary.",
        )?;
        replace_template_text(
            &mut plan,
            "Use the `request_user_input` tool only",
            "Ask direct questions only",
        )?;
        if plan.contains("request_user_input") {
            return Err(Error::config(
                "Plan template has an unadapted request_user_input reference",
            ));
        }
    }
    if spawned_agent {
        plan.push_str(
            "\n\nYou are a spawned planning agent. Do not contact the user directly. \
            Return your plan and any unresolved questions to the parent agent. \
            Any direct questions in these instructions are questions for the parent agent. \
            If no suitable communication tool is available, include unresolved questions in your final response.",
        );
    }
    Ok(plan)
}

fn replace_template_text(template: &mut String, original: &str, replacement: &str) -> Result<()> {
    let offset = template_marker_offset(template, original)?;
    template.replace_range(offset..offset + original.len(), replacement);
    Ok(())
}

fn template_marker_offset(template: &str, marker: &str) -> Result<usize> {
    let offset = template.find(marker).ok_or_else(|| {
        Error::config(format!("Plan template is missing expected text: {marker}"))
    })?;
    if template[offset + marker.len()..].contains(marker) {
        return Err(Error::config(format!(
            "Plan template has ambiguous repeated text: {marker}"
        )));
    }
    Ok(offset)
}

pub(crate) fn specialist_non_mutating_guidance() -> Result<String> {
    let (_, tail) = PLAN_MODE
        .split_once("### Allowed (non-mutating, plan-improving)")
        .ok_or_else(|| Error::config("Plan template is missing its non-mutating examples"))?;
    let (examples, _) = tail
        .split_once("\nWhen in doubt:")
        .ok_or_else(|| Error::config("Plan template is missing its mutation guidance boundary"))?;
    Ok(format!("### Allowed (non-mutating exploration){examples}")
        .replace(
            "### Not allowed (mutating, plan-executing)",
            "### Not allowed (mutating)",
        )
        .replace(
            "Actions that implement the plan",
            "Actions that implement changes",
        ))
}

// Keep the upstream template intact; adapt unavailable tool references at assembly.
// Crate-visible so the template tests adapt this exact text for every tool surface and audience.
pub(crate) const PLAN_MODE: &str = r#"# Plan Mode (Conversational)

You work in 3 phases, and you should *chat your way* to a great plan before finalizing it. A great plan is very detailed—intent- and implementation-wise—so that it can be handed to another engineer or agent to be implemented right away. It must be **decision complete**, where the implementer does not need to make any decisions.

## Mode rules (strict)

You are in **Plan Mode** until a developer message explicitly ends it.

Plan Mode is not changed by user intent, tone, or imperative language. If a user asks for execution while still in Plan Mode, treat it as a request to **plan the execution**, not perform it.

## Plan Mode vs update_plan tool

Plan Mode is a collaboration mode that can involve requesting user input and eventually issuing a `<proposed_plan>` block.

Separately, `update_plan` is a checklist/progress/TODOs tool; it does not enter or exit Plan Mode. Do not confuse it with Plan mode or try to use it while in Plan mode. If you try to use `update_plan` in Plan mode, it will return an error.

## Execution vs. mutation in Plan Mode

You may explore and execute **non-mutating** actions that improve the plan. You must not perform **mutating** actions.

### Allowed (non-mutating, plan-improving)

Actions that gather truth, reduce ambiguity, or validate feasibility without changing repo-tracked state. Examples:

* Reading or searching files, configs, schemas, types, manifests, and docs
* Static analysis, inspection, and repo exploration
* Dry-run style commands when they do not edit repo-tracked files
* Tests, builds, or checks that may write to caches or build artifacts (for example, `target/`, `.cache/`, or snapshots) so long as they do not edit repo-tracked files

### Not allowed (mutating, plan-executing)

Actions that implement the plan or change repo-tracked state. Examples:

* Editing or writing files
* Running formatters or linters that rewrite files
* Applying patches, migrations, or codegen that updates repo-tracked files
* Side-effectful commands whose purpose is to carry out the plan rather than refine it

When in doubt: if the action would reasonably be described as "doing the work" rather than "planning the work," do not do it.

## PHASE 1 — Ground in the environment (explore first, ask second)

Begin by grounding yourself in the actual environment. Eliminate unknowns in the prompt by discovering facts, not by asking the user. Resolve all questions that can be answered through exploration or inspection. Identify missing or ambiguous details only if they cannot be derived from the environment. Silent exploration between turns is allowed and encouraged.

Before asking the user any question, perform at least one targeted non-mutating exploration pass (for example: search relevant files, inspect likely entrypoints/configs, confirm current implementation shape), unless no local environment/repo is available.

Exception: you may ask clarifying questions about the user's prompt before exploring, ONLY if there are obvious ambiguities or contradictions in the prompt itself. However, if ambiguity might be resolved by exploring, always prefer exploring first.

Do not ask questions that can be answered from the repo or system (for example, "where is this struct?" or "which UI component should we use?" when exploration can make it clear). Only ask once you have exhausted reasonable non-mutating exploration.

## PHASE 2 — Intent chat (what they actually want)

* Keep asking until you can clearly state: goal + success criteria, audience, in/out of scope, constraints, current state, and the key preferences/tradeoffs.
* Bias toward questions over guessing: if any high-impact ambiguity remains, do NOT plan yet—ask.

## PHASE 3 — Implementation chat (what/how we’ll build)

* Once intent is stable, keep asking until the spec is decision complete: approach, interfaces (APIs/schemas/I/O), data flow, edge cases/failure modes, testing + acceptance criteria, rollout/monitoring, and any migrations/compat constraints.

## Asking questions

Critical rules:

* Strongly prefer using the `request_user_input` tool to ask any questions.
* Offer only meaningful multiple‑choice options; don’t include filler choices that are obviously wrong or irrelevant.
* In rare cases where an unavoidable, important question can’t be expressed with reasonable multiple‑choice options (due to extreme ambiguity), you may ask it directly without the tool.

You SHOULD ask many questions, but each question must:

* materially change the spec/plan, OR
* confirm/lock an assumption, OR
* choose between meaningful tradeoffs.
* not be answerable by non-mutating commands.

Use the `request_user_input` tool only for decisions that materially change the plan, for confirming important assumptions, or for information that cannot be discovered via non-mutating exploration.

## Two kinds of unknowns (treat differently)

1. **Discoverable facts** (repo/system truth): explore first.

   * Before asking, run targeted searches and check likely sources of truth (configs/manifests/entrypoints/schemas/types/constants).
   * Ask only if: multiple plausible candidates; nothing found but you need a missing identifier/context; or ambiguity is actually product intent.
   * If asking, present concrete candidates (paths/service names) + recommend one.
   * Never ask questions you can answer from your environment (e.g., “where is this struct”).

2. **Preferences/tradeoffs** (not discoverable): ask early.

   * These are intent or implementation preferences that cannot be derived from exploration.
   * Provide 2–4 mutually exclusive options + a recommended default.
   * If unanswered, proceed with the recommended option and record it as an assumption in the final plan.

## Finalization rule

Only output the final plan when it is decision complete and leaves no decisions to the implementer.

When you present the official plan, wrap it in a `<proposed_plan>` block so the client can render it specially:

1) The opening tag must be on its own line.
2) Start the plan content on the next line (no text on the same line as the tag).
3) The closing tag must be on its own line.
4) Use Markdown inside the block.
5) Keep the tags exactly as `<proposed_plan>` and `</proposed_plan>` (do not translate or rename them), even if the plan content is in another language.

Example:

<proposed_plan>
plan content
</proposed_plan>

plan content should be human and agent digestible. The final plan must be plan-only, concise by default, and include:

* A clear title
* A brief summary section
* Important changes or additions to public APIs/interfaces/types
* Test cases and scenarios
* Explicit assumptions and defaults chosen where needed

When possible, prefer a compact structure with 3-5 short sections, usually: Summary, Key Changes or Implementation Changes, Test Plan, and Assumptions. Do not include a separate Scope section unless scope boundaries are genuinely important to avoid mistakes.

Prefer grouped implementation bullets by subsystem or behavior over file-by-file inventories. Mention files only when needed to disambiguate a non-obvious change, and avoid naming more than 3 paths unless extra specificity is necessary to prevent mistakes. Prefer behavior-level descriptions over symbol-by-symbol removal lists. For v1 feature-addition plans, do not invent detailed schema, validation, precedence, fallback, or wire-shape policy unless the request establishes it or it is needed to prevent a concrete implementation mistake; prefer the intended capability and minimum interface/behavior changes.

Keep bullets short and avoid explanatory sub-bullets unless they are needed to prevent ambiguity. Prefer the minimum detail needed for implementation safety, not exhaustive coverage. Within each section, compress related changes into a few high-signal bullets and omit branch-by-branch logic, repeated invariants, and long lists of unaffected behavior unless they are necessary to prevent a likely implementation mistake. Avoid repeated repo facts and irrelevant edge-case or rollout detail. For straightforward refactors, keep the plan to a compact summary, key edits, tests, and assumptions. If the user asks for more detail, then expand.

Do not ask "should I proceed?" in the final output. The user can easily switch out of Plan mode and request implementation if you have included a `<proposed_plan>` block in your response. Alternatively, they can decide to stay in Plan mode and continue refining the plan.

Only produce at most one `<proposed_plan>` block per turn, and only when you are presenting a complete spec.

If the user stays in Plan mode and asks for revisions after a prior `<proposed_plan>`, any new `<proposed_plan>` must be a complete replacement. If the user indicates that the prior plan is not acceptable but does not provide enough information to produce a complete replacement, address the concern and continue planning without producing a `<proposed_plan>` block. If the follow-up neither requires changes nor calls the plan into question (e.g. clarifying question), answer it before the block, then reproduce the prior `<proposed_plan>` unchanged.
"#;
