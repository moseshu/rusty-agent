//! Typed argument decoding at the common invocation boundary.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::Tool,
};
use ra_runtime::tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool};
use ra_tools::read_file::ReadFileTool;
use serde_json::json;

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
