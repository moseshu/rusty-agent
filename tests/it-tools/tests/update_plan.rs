//! `update_plan`: the plan board, and the four shapes it refuses to record.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{PermissionScope, Tool, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_tools::update_plan::{PlanLimits, UpdatePlanTool};
use serde_json::{Value, json};

/// The run a direct call is made inside. The entry reads neither the run nor host state, but a call
/// always belongs to one, and the context is what says so.
fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-update-plan"), agent.as_ref())
}

/// Runs the call and, when it fails, the tool's own failure shaping — the path the dispatcher takes
/// for a tool declaring `ToolFailureHandling::Custom`.
async fn observe(tool: &UpdatePlanTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-plan");
    let run = run();
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("update_plan shapes every failure it produces"),
    }
}

fn body(output: &ToolOutput) -> &str {
    output.as_text().expect("a single text block")
}

fn guidance(output: &ToolOutput) -> String {
    output.metadata().guidance().join(" ")
}

fn step(text: &str, status: &str) -> Value {
    json!({ "step": text, "status": status })
}

/// The entry advertises what it is, and its schema refuses anything it did not name.
#[tokio::test]
async fn test_the_plan_entry_advertises_a_strict_schema_and_a_read_only_scope() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "update_plan");
    assert!(tool.schema().strict_json_schema());
    assert_eq!(tool.schema().input_schema()["additionalProperties"], false);
    assert_eq!(tool.schema().input_schema()["required"], json!(["plan"]));

    // Read scope and parallel execution, because recording a plan reaches nothing: no workspace, no
    // process, no network. A role that withholds everything with a side effect keeps this one.
    assert_eq!(tool.options().permission_scope(), PermissionScope::Read);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
}

/// A recorded plan is answered with its own counts and the step being worked on.
#[tokio::test]
async fn test_a_recorded_plan_is_summarized_rather_than_echoed() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    let output = observe(
        &tool,
        &json!({
            "plan": [
                step("read the failing test", "completed"),
                step("fix the parser", "in_progress"),
                step("run the suite", "pending"),
            ]
        }),
    )
    .await;

    let text = body(&output);
    assert!(text.contains("3 steps"), "{text}");
    assert!(text.contains("1 completed"), "{text}");
    assert!(text.contains("In progress: fix the parser"), "{text}");
    // The plan itself is in the call's arguments, which the model can already see. Repeating it
    // would put two copies of one list in one turn, and pay for the longer one on every request
    // that carries the history afterwards.
    assert!(
        !text.contains("run the suite"),
        "the result echoed the whole plan: {text}"
    );
}

/// A plan with work left and nobody working is recorded, and told what is missing.
///
/// A refusal would be wrong here: "I have a plan and have not started" is a state a run passes
/// through legitimately, on the turn it plans and before the turn it acts.
#[tokio::test]
async fn test_a_plan_with_nothing_in_progress_is_recorded_with_a_note() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    let output = observe(
        &tool,
        &json!({ "plan": [step("read the failing test", "pending")] }),
    )
    .await;

    assert!(body(&output).contains("1 steps"), "{}", body(&output));
    assert!(
        guidance(&output).contains("in_progress"),
        "{}",
        guidance(&output)
    );
}

/// A finished plan is recorded without being told to start something.
#[tokio::test]
async fn test_a_finished_plan_is_recorded_without_a_note() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    let output = observe(
        &tool,
        &json!({ "plan": [step("read the failing test", "completed")] }),
    )
    .await;

    assert!(body(&output).contains("1 completed"), "{}", body(&output));
    assert!(
        guidance(&output).is_empty(),
        "a completed plan was told to start something: {}",
        guidance(&output)
    );
}

/// Two steps in progress is refused, because the board would then describe two agents.
#[tokio::test]
async fn test_two_steps_in_progress_are_refused() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    let output = observe(
        &tool,
        &json!({
            "plan": [
                step("fix the parser", "in_progress"),
                step("run the suite", "in_progress"),
            ]
        }),
    )
    .await;

    assert!(body(&output).contains("in_progress"), "{}", body(&output));
    assert!(
        guidance(&output).contains("exactly one"),
        "{}",
        guidance(&output)
    );
}

/// An empty plan, a blank step, and an oversized one are each refused with their own next step.
#[tokio::test]
async fn test_a_plan_nobody_could_act_on_is_refused() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");

    let empty = observe(&tool, &json!({ "plan": [] })).await;
    assert!(body(&empty).contains("no steps"), "{}", body(&empty));

    let blank = observe(&tool, &json!({ "plan": [step("   ", "pending")] })).await;
    assert!(body(&blank).contains("Step 1"), "{}", body(&blank));

    let narrow = UpdatePlanTool::new()
        .expect("update_plan builds")
        .with_limits(PlanLimits::new().with_max_step_chars(8));
    let long = observe(
        &narrow,
        &json!({ "plan": [step("a step far longer than eight", "pending")] }),
    )
    .await;
    assert!(body(&long).contains("over the limit"), "{}", body(&long));

    let crowded = UpdatePlanTool::new()
        .expect("update_plan builds")
        .with_limits(PlanLimits::new().with_max_steps(1));
    let many = observe(
        &crowded,
        &json!({ "plan": [step("one", "pending"), step("two", "pending")] }),
    )
    .await;
    assert!(body(&many).contains("2 steps"), "{}", body(&many));
}

/// Arguments the schema does not name are refused before the entry runs.
#[tokio::test]
async fn test_arguments_outside_the_schema_are_refused_with_the_decoder_s_words() {
    let tool = UpdatePlanTool::new().expect("update_plan builds");
    let arguments = json!({ "plan": [step("a step", "pending")], "explanation": "why" });

    let error = tool
        .decode_input(&arguments)
        .expect_err("an unknown field must fail before the tool body runs");
    let run = run();
    let output = tool
        .handle_failure(
            &ToolContext::new(&run, &tool, &CallId::new("call-invalid"), &arguments),
            &error,
        )
        .await
        .expect("failure shaping must not fail")
        .expect("update_plan shapes a decode failure");

    assert!(
        body(&output).contains("Invalid arguments"),
        "{}",
        body(&output)
    );
}
