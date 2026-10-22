//! Codex's plan schema, permissive default acceptance and argument-error contract.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    permission::ActionClass,
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_tools::update_plan::{PlanLimits, UpdatePlanTool};
use serde_json::{Value, json};

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .build()
        .unwrap();
    RunContext::new(RunId::new("run-update-plan"), agent.as_ref())
}

async fn observe(tool: &UpdatePlanTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-plan");
    let run = run();
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .unwrap()
            .unwrap(),
    }
}

fn step(text: &str, status: &str) -> Value {
    json!({"step":text, "status":status})
}

#[test]
fn schema_preserves_codex_optional_explanation_and_non_strictness() {
    let tool = UpdatePlanTool::new().unwrap();
    tool.validate().unwrap();
    assert_eq!(tool.origin().qualified_name(), "update_plan");
    assert!(!tool.schema().strict_json_schema());
    let schema = tool.schema().input_schema();
    assert_eq!(schema["required"], json!(["plan"]));
    assert_eq!(
        schema["properties"]["explanation"],
        json!({
            "type":"string", "description":"Optional explanation for this plan update."
        })
    );
    assert_eq!(
        schema["properties"]["plan"]["items"]["required"],
        json!(["step", "status"])
    );
    assert_eq!(
        tool.options().action_classes(),
        &[ActionClass::from("read")]
    );
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
}

#[tokio::test]
async fn optional_explanation_is_accepted_without_extra_guidance_or_plan_echo() {
    let tool = UpdatePlanTool::new().unwrap();
    for explanation in [None, Some(Value::Null), Some(json!("The parser changed."))] {
        let mut arguments = json!({"plan":[step("fix the parser", "in_progress")]});
        if let Some(explanation) = explanation {
            arguments["explanation"] = explanation;
        }
        let output = observe(&tool, &arguments).await;
        assert_eq!(output.as_text(), Some("Plan updated"));
        assert!(output.metadata().guidance().is_empty());
    }
}

#[tokio::test]
async fn default_acceptance_preserves_plans_codex_does_not_reject() {
    let tool = UpdatePlanTool::new().unwrap();
    let oversized_steps = (0..25)
        .map(|_| step(&"x".repeat(201), "pending"))
        .collect::<Vec<_>>();
    for plan in [
        json!([]),
        json!([step("   ", "pending")]),
        json!([step("first", "in_progress"), step("second", "in_progress")]),
        json!([step("waiting", "pending")]),
        json!([step("finished", "completed")]),
        json!(oversized_steps),
    ] {
        let output = observe(&tool, &json!({"plan":plan})).await;
        assert_eq!(output.as_text(), Some("Plan updated"));
        assert!(output.metadata().guidance().is_empty());
    }
}

#[tokio::test]
async fn malformed_arguments_return_codex_parse_error_prefix() {
    let tool = UpdatePlanTool::new().unwrap();
    for (arguments, reason) in [
        (json!({"plan":[], "other":true}), "unknown field `other`"),
        (json!({"explanation":"why"}), "missing field `plan`"),
        (
            json!({"plan":[step("x", "unknown")]}),
            "unknown variant `unknown`",
        ),
        (json!({"plan":[], "explanation":5}), "invalid type: integer"),
        (
            json!({"plan":[{"step":"x", "status":"pending", "other":true}]}),
            "unknown field `other`",
        ),
    ] {
        let output = observe(&tool, &arguments).await;
        let text = output.as_text().unwrap();
        assert!(
            text.starts_with("failed to parse function arguments: "),
            "{text}"
        );
        assert!(text.contains(reason), "{text}");
        assert!(output.metadata().guidance().is_empty());
    }
}

#[tokio::test]
async fn size_limits_are_only_enforced_when_the_host_selects_them() {
    let narrow = UpdatePlanTool::new()
        .unwrap()
        .with_limits(PlanLimits::new().with_max_step_chars(8));
    let long = observe(
        &narrow,
        &json!({"plan":[step("a long plan step", "pending")]}),
    )
    .await;
    assert!(long.as_text().unwrap().contains("over the limit of 8"));
    let crowded = UpdatePlanTool::new()
        .unwrap()
        .with_limits(PlanLimits::new().with_max_steps(1));
    let many = observe(
        &crowded,
        &json!({"plan":[step("one", "pending"),step("two", "pending")]}),
    )
    .await;
    assert!(many.as_text().unwrap().contains("over the limit of 1"));
}
