//! Typed argument decoding at the common invocation boundary.

use std::sync::Arc;

use async_trait::async_trait;
use ra_context::budget::ToolResultBudget;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{
        ArtifactRef, ModelExcerpt, ObservationMetadata, Tool, ToolContext, ToolOrigin, ToolOutput,
        ToolOutputBlock, ToolOutputProjection, ToolOutputProjector, ToolSchema, ToolServices,
        Truncation, TruncationStage,
    },
};
use ra_runtime::tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool};
use ra_tools::read_file::ReadFileTool;
use serde_json::json;

struct LongOutputTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

struct AppendingProjector;

impl ToolOutputProjector for AppendingProjector {
    fn project(
        &self,
        _run_id: &RunId,
        _call_id: &CallId,
        _output: &ToolOutput,
    ) -> ra_core::error::Result<ToolOutputProjection> {
        let excerpt = ModelExcerpt::new(
            vec![ToolOutputBlock::text("context excerpt")],
            ArtifactRef::new("tool-output/projection-test")?,
        )?;
        Ok(ToolOutputProjection::new()
            .with_model_excerpt(excerpt)
            .with_truncation(Truncation::new(TruncationStage::ContextBudget, 1_000, 64))
            .with_guidance("The context policy added this guidance."))
    }
}

impl LongOutputTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("long_output").expect("tool origin"),
            schema: ToolSchema::new(
                "long_output",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .expect("tool schema"),
        }
    }
}

#[async_trait]
impl Tool for LongOutputTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> ra_core::error::Result<ToolOutput> {
        Ok(
            ToolOutput::text("HEAD\n".to_owned() + &"middle\n".repeat(128) + "TAIL").with_metadata(
                ObservationMetadata::new()
                    .with_truncation(Truncation::new(TruncationStage::Tool, 2_000, 1_000))
                    .with_guidance("The tool added this guidance."),
            ),
        )
    }
}

#[tokio::test]
async fn typed_decode_failure_is_an_observation_not_a_stopped_run() {
    let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new().expect("read_file builds"));
    let agent = AgentSpec::builder()
        .id(AgentId::new("reader"))
        .name("Reader")
        .build()
        .expect("agent builds");
    let run = Arc::new(RunContext::new(RunId::new("run-typed-dispatch"), &agent));
    let request = ToolDispatchRequest::new(
        tool,
        CallId::new("call-invalid"),
        json!({ "path": 3, "offset": null, "limit": null }),
        run,
        CancelScope::root(),
        CallHistory::default(),
        Default::default(),
    );

    let result = dispatch_tool(request)
        .await
        .expect("invalid arguments are returned to the model");
    let ToolDispatch::Observed(observation) = result else {
        panic!("invalid arguments must be a model-visible observation");
    };
    assert_eq!(observation.failure_code(), Some("tool.invalid_input"));
    let rendered = observation.output().output().to_string();
    assert!(
        rendered
            .contains("Invalid arguments: argument `$.path` has type integer, expected string.")
    );
    assert!(
        !rendered.contains("工具 `"),
        "framework error display leaked into model-visible output: {rendered}"
    );
}

#[tokio::test]
async fn context_budget_projects_before_the_tool_output_enters_history() {
    let tool: Arc<dyn Tool> = Arc::new(LongOutputTool::new());
    let agent = AgentSpec::builder()
        .id(AgentId::new("budget-reader"))
        .name("Budget reader")
        .build()
        .expect("agent builds");
    let request = ToolDispatchRequest::new(
        tool,
        CallId::new("call-budget"),
        json!({}),
        Arc::new(RunContext::new(RunId::new("run-budget-dispatch"), &agent)),
        CancelScope::root(),
        CallHistory::default(),
        Default::default(),
    )
    .with_services(ToolServices::new().with_output_projector(Arc::new(
        ToolResultBudget::new(512, 128).expect("a valid budget"),
    )));

    let result = dispatch_tool(request).await.expect("dispatch succeeds");
    let ToolDispatch::Observed(observation) = result else {
        panic!("the tool completed with an observation");
    };
    let stored: ToolOutput = serde_json::from_value(observation.output().output().clone())
        .expect("the complete structured result is stored");

    assert!(stored.as_text().is_some_and(|text| text.contains("middle")));
    let excerpt = stored.model_excerpt().expect("the result was excerpted");
    // The reference names the run as well as the call: a call id is unique only inside the
    // conversation the provider issued it for.
    assert_eq!(
        excerpt.artifact_ref().as_str(),
        "tool-output/72756e2d6275646765742d6469737061746368/63616c6c2d627564676574"
    );
    assert!(
        stored
            .model_blocks()
            .last()
            .and_then(|block| block.as_text())
            .is_some_and(|text| text.contains("retained in the session record as artifact"))
    );
}

#[tokio::test]
async fn a_projector_can_only_add_to_the_complete_tool_output() {
    let tool: Arc<dyn Tool> = Arc::new(LongOutputTool::new());
    let agent = AgentSpec::builder()
        .id(AgentId::new("projector-reader"))
        .name("Projector reader")
        .build()
        .expect("agent builds");
    let request = ToolDispatchRequest::new(
        tool,
        CallId::new("call-discard"),
        json!({}),
        Arc::new(RunContext::new(
            RunId::new("run-projector-dispatch"),
            &agent,
        )),
        CancelScope::root(),
        CallHistory::default(),
        Default::default(),
    )
    .with_services(ToolServices::new().with_output_projector(Arc::new(AppendingProjector)));

    let result = dispatch_tool(request).await.expect("dispatch succeeds");
    let ToolDispatch::Observed(observation) = result else {
        panic!("the tool completed with an observation");
    };
    let stored: ToolOutput = serde_json::from_value(observation.output().output().clone())
        .expect("the complete structured result is stored");

    assert!(
        stored.as_text().is_some_and(|text| text.contains("middle")),
        "a projector cannot replace the complete blocks"
    );
    assert_eq!(
        stored
            .metadata()
            .truncations()
            .iter()
            .map(Truncation::stage)
            .collect::<Vec<_>>(),
        vec![TruncationStage::Tool, TruncationStage::ContextBudget],
        "a projector appends instead of replacing prior metadata"
    );
    assert_eq!(
        stored.metadata().guidance(),
        [
            "The tool added this guidance.",
            "The context policy added this guidance."
        ]
    );
}
