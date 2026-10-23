// Compile the private adapter directly so template drift can be injected without a public API.
#[allow(dead_code)]
#[path = "../../../crates/ra-coding/src/prompt/role.rs"]
mod role;

use ra_core::error::Error;
use role::{PLAN_MODE, planner_guidance, specialist_non_mutating_guidance};

const RENDERING: &str =
    "wrap it in a `<proposed_plan>` block so the client can render it specially:";
const SECTION_START: &str = "## Plan Mode vs update_plan tool";
const SECTION_END: &str = "## Execution vs. mutation";
const QUESTION_PREFERENCE: &str =
    "Strongly prefer using the `request_user_input` tool to ask any questions.";
const QUESTION_POLICY: &str = "Use the `request_user_input` tool only";
const TEMPLATE: &str = "# Plan Mode

## Plan Mode vs update_plan tool
Do not use `update_plan` in Plan Mode.

## Execution vs. mutation in Plan Mode
Explore without changing repository files.

* Strongly prefer using the `request_user_input` tool to ask any questions.
Use the `request_user_input` tool only for decisions that change the plan.

When presenting a plan, wrap it in a `<proposed_plan>` block so the client can render it specially:
<proposed_plan>Plan contents</proposed_plan>
";
const TOOL_SETS: &[&[&str]] = &[
    &[],
    &["update_plan"],
    &["request_user_input"],
    &["update_plan", "request_user_input"],
];

fn assert_config_error(template: &str, tools: &[&str], spawned: bool, expected: &str) {
    let error = planner_guidance(template, tools, spawned)
        .expect_err("template drift must fail prompt assembly");
    match error {
        Error::Config { message, .. } => assert_eq!(message, expected),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[test]
fn adapts_valid_templates_for_each_tool_surface_and_audience() {
    for tools in TOOL_SETS {
        for spawned in [false, true] {
            let plan = planner_guidance(TEMPLATE, tools, spawned).expect("valid template");
            assert!(plan.contains("block as plain-text plan markup:"));
            assert!(!plan.contains("render it specially"));
            assert!(plan.contains("## Execution vs. mutation in Plan Mode"));
            assert!(plan.contains("Explore without changing repository files."));
            assert!(plan.contains("<proposed_plan>Plan contents</proposed_plan>"));
            assert_eq!(plan.contains("update_plan"), tools.contains(&"update_plan"));
            let root_questions = !spawned && tools.contains(&"request_user_input");
            assert_eq!(plan.contains(QUESTION_PREFERENCE), root_questions);
            assert_eq!(plan.contains(QUESTION_POLICY), root_questions);
            assert_eq!(plan.contains("request_user_input"), root_questions);
            assert_eq!(
                plan.contains("Ask direct questions when clarification is necessary."),
                !root_questions
            );
            assert_eq!(plan.contains("Ask direct questions only"), !root_questions);
            assert_eq!(
                plan.contains("Return your plan and any unresolved questions to the parent agent."),
                spawned
            );
        }
    }
}

#[test]
fn rejects_changed_rendering_guidance_for_all_surfaces() {
    let template = TEMPLATE.replace(RENDERING, "render the proposed plan specially:");
    for tools in TOOL_SETS {
        for spawned in [false, true] {
            assert_config_error(
                &template,
                tools,
                spawned,
                &format!("Plan template is missing expected text: {RENDERING}"),
            );
        }
    }
}

#[test]
fn rejects_missing_section_boundaries_when_removing_update_plan_guidance() {
    for boundary in [SECTION_START, SECTION_END] {
        let template = TEMPLATE.replace(boundary, "## Revised section heading");
        for tools in TOOL_SETS
            .iter()
            .filter(|tools| !tools.contains(&"update_plan"))
        {
            for spawned in [false, true] {
                assert_config_error(
                    &template,
                    tools,
                    spawned,
                    &format!("Plan template is missing expected text: {boundary}"),
                );
            }
        }
    }
}

#[test]
fn rejects_reversed_section_boundaries_without_panicking() {
    let template = TEMPLATE
        .replace(SECTION_START, "SECTION_PLACEHOLDER")
        .replace(SECTION_END, SECTION_START)
        .replace("SECTION_PLACEHOLDER", SECTION_END);
    assert_config_error(
        &template,
        &[],
        false,
        "Plan template has out-of-order update_plan section boundaries",
    );
}

#[test]
fn rejects_changed_question_guidance_when_direct_questions_are_required() {
    for original in [QUESTION_PREFERENCE, QUESTION_POLICY] {
        let template = TEMPLATE.replace(original, "Ask the user using `request_user_input`.");
        for tools in TOOL_SETS {
            for spawned in [false, true] {
                if spawned || !tools.contains(&"request_user_input") {
                    assert_config_error(
                        &template,
                        tools,
                        spawned,
                        &format!("Plan template is missing expected text: {original}"),
                    );
                }
            }
        }
    }
}

#[test]
fn rejects_ambiguous_repeated_adaptation_markers() {
    for marker in [
        RENDERING,
        SECTION_START,
        SECTION_END,
        QUESTION_PREFERENCE,
        QUESTION_POLICY,
    ] {
        let template = format!("{TEMPLATE}\n{marker}");
        assert_config_error(
            &template,
            &[],
            false,
            &format!("Plan template has ambiguous repeated text: {marker}"),
        );
    }
}

#[test]
fn rejects_new_unadapted_tool_references() {
    for tool in ["update_plan", "request_user_input"] {
        let template = format!("{TEMPLATE}\nAdditional instructions for `{tool}`.");
        assert_config_error(
            &template,
            &[],
            false,
            &format!("Plan template has an unadapted {tool} reference"),
        );
    }
    let template = format!("{TEMPLATE}\nAdditional instructions for `request_user_input`.");
    assert_config_error(
        &template,
        &["update_plan", "request_user_input"],
        true,
        "Plan template has an unadapted request_user_input reference",
    );
}

#[test]
fn adapts_the_embedded_upstream_template_for_each_tool_surface_and_audience() {
    let rendering = "wrap it in a `<proposed_plan>` block as plain-text plan markup:";
    for tools in TOOL_SETS {
        for spawned in [false, true] {
            let plan = planner_guidance(PLAN_MODE, tools, spawned)
                .unwrap_or_else(|error| panic!("{tools:?} spawned={spawned}: {error}"));
            assert!(plan.contains(rendering));
            assert!(!plan.contains("render it specially"));
            for heading in [
                "## Mode rules (strict)",
                "## Execution vs. mutation in Plan Mode",
                "## PHASE 1",
                "## PHASE 2",
                "## PHASE 3",
                "## Asking questions",
                "## Finalization rule",
                "<proposed_plan>\nplan content\n</proposed_plan>",
            ] {
                assert!(
                    plan.contains(heading),
                    "{tools:?} spawned={spawned}: {heading}"
                );
            }
            assert_eq!(plan.contains("update_plan"), tools.contains(&"update_plan"));
            assert_eq!(
                plan.contains("## Plan Mode vs update_plan tool"),
                tools.contains(&"update_plan")
            );
            let root_questions = !spawned && tools.contains(&"request_user_input");
            assert_eq!(plan.contains("request_user_input"), root_questions);
            assert_eq!(
                plan.contains("Ask direct questions when clarification is necessary."),
                !root_questions
            );
            assert_eq!(
                plan.contains("Return your plan and any unresolved questions to the parent agent."),
                spawned
            );
            // A root planner with both tools receives the upstream text with one adaptation.
            if root_questions && tools.contains(&"update_plan") {
                assert_eq!(plan, PLAN_MODE.replace(RENDERING, rendering));
            }
        }
    }
}

#[test]
fn extracts_the_embedded_upstream_non_mutating_examples_for_specialists() {
    let guidance = specialist_non_mutating_guidance().expect("embedded template boundaries");
    assert!(guidance.starts_with("### Allowed (non-mutating exploration)"));
    for expected in [
        "### Not allowed (mutating)",
        "Actions that implement changes or change repo-tracked state.",
        "* Reading or searching files, configs, schemas, types, manifests, and docs",
        "* Dry-run style commands when they do not edit repo-tracked files",
        "* Tests, builds, or checks that may write to caches or build artifacts",
        "* Running formatters or linters that rewrite files",
        "* Applying patches, migrations, or codegen that updates repo-tracked files",
    ] {
        assert!(guidance.contains(expected), "{expected}");
    }
    for excluded in [
        "plan-improving",
        "plan-executing",
        "Actions that implement the plan",
        "When in doubt",
        "## PHASE 1",
        "request_user_input",
        "update_plan",
    ] {
        assert!(!guidance.contains(excluded), "{excluded}");
    }
}
