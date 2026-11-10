//! Tools ported from Codex handlers answer a call whose arguments were not JSON as those handlers
//! do: `failed to parse function arguments: {diagnostic}` from their shared `parse_arguments`, the
//! skills tools with the diagnostic alone, and after the refusals a handler gives before it reads
//! its arguments.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::{Error, ToolErrorKind},
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolArgumentDecodeError, ToolContext},
};
use ra_tools::{
    agent_ns::collaboration_tools,
    mcp_ns::resource_tools,
    request_permissions::{RequestPermissionsPolicy, RequestPermissionsTool},
    request_user_input::{ModeKind, RequestUserInputTool, request_user_input_available_modes},
    skills_ns::{
        SkillProviders, catalog::SkillCatalog, provider::ExecutorSkillProvider, skill_tools,
    },
    update_plan::UpdatePlanTool,
};
use serde_json::Value;

/// The parser's diagnostic for the argument text `{bad`.
const DIAGNOSTIC: &str = "key must be a string at line 1 column 2";
const REPLY: &str = "failed to parse function arguments: key must be a string at line 1 column 2";

/// The error the runtime hands a tool when the provider's argument text was not JSON.
fn unparsed(tool: &str) -> Error {
    let diagnostic = ToolArgumentDecodeError::InvalidJson {
        message: serde_json::from_str::<Value>("{bad")
            .unwrap_err()
            .to_string(),
    };
    assert_eq!(
        diagnostic,
        ToolArgumentDecodeError::InvalidJson {
            message: DIAGNOSTIC.to_owned()
        }
    );
    Error::tool(ToolErrorKind::InvalidInput, tool, diagnostic.to_string()).with_source(diagnostic)
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("root"))
        .name("Root")
        .build()
        .unwrap();
    RunContext::new(RunId::new("run-unparsed"), agent.as_ref())
}

/// What the tool shows the model for the unparsed call, in `run`.
async fn reply_in(tool: &dyn Tool, run: &RunContext) -> String {
    let error = unparsed(tool.origin().qualified_name());
    let call_id = CallId::new("call-unparsed");
    let arguments = Value::Null;
    tool.handle_failure(&ToolContext::new(run, tool, &call_id, &arguments), &error)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{} answers the call itself", tool.origin().qualified_name()))
        .as_text()
        .unwrap()
        .to_owned()
}

async fn reply(tool: &dyn Tool) -> String {
    reply_in(tool, &run()).await
}

struct UnusedPolicy;

#[async_trait]
impl RequestPermissionsPolicy for UnusedPolicy {
    async fn request_permissions(
        &self,
        _: &ToolContext<'_>,
    ) -> std::result::Result<String, String> {
        unreachable!("a call whose arguments were not JSON never reaches the policy")
    }
}

/// Codex's collaboration handlers parse their arguments first.
#[tokio::test]
async fn collaboration_tools_answer_as_codex_parse_arguments() {
    for tool in collaboration_tools().unwrap() {
        assert_eq!(
            reply(&tool).await,
            REPLY,
            "{}",
            tool.origin().qualified_name()
        );
    }
}

/// Codex's resource handlers parse their arguments first, for all three entries.
#[tokio::test]
async fn mcp_resource_tools_answer_as_codex_parse_arguments() {
    let tools = resource_tools(Vec::new()).unwrap();
    assert_eq!(tools.len(), 3);
    for tool in tools {
        assert_eq!(
            reply(tool.as_ref()).await,
            REPLY,
            "{}",
            tool.origin().qualified_name()
        );
    }
}

/// Codex's `request_permissions` handler parses its arguments first.
#[tokio::test]
async fn request_permissions_answers_as_codex_parse_arguments() {
    let tool = RequestPermissionsTool::new(Arc::new(UnusedPolicy)).unwrap();
    assert_eq!(reply(&tool).await, REPLY);
}

/// Codex's skills tools answer with the parser's diagnostic alone.
#[tokio::test]
async fn skills_tools_answer_with_the_bare_diagnostic() {
    let workspace = tempfile::tempdir().unwrap();
    let providers = SkillProviders::new().with_executor_provider(Arc::new(
        ExecutorSkillProvider::new(SkillCatalog::default()),
    ));
    let roots = [(
        "local".to_owned(),
        ra_exec::fs::Workspace::open(workspace.path()).unwrap(),
    )]
    .into();
    let tools = skill_tools(providers, roots, 4096).unwrap();
    assert!(!tools.is_empty());
    for tool in tools {
        assert_eq!(
            reply(tool.as_ref()).await,
            DIAGNOSTIC,
            "{}",
            tool.origin().qualified_name()
        );
    }
}

/// Codex's `update_plan` handler refuses Plan mode before it reads the arguments.
#[tokio::test]
async fn update_plan_refuses_plan_mode_before_the_arguments() {
    assert_eq!(reply(&UpdatePlanTool::new().unwrap()).await, REPLY);
    let planning = UpdatePlanTool::new().unwrap().with_mode(ModeKind::Plan);
    assert_eq!(
        reply(&planning).await,
        "update_plan is a TODO/checklist tool and is not allowed in Plan mode"
    );
}

/// Codex's `request_user_input` handler refuses a non-root thread, then an unavailable mode,
/// before it reads the arguments.
#[tokio::test]
async fn request_user_input_refuses_before_the_arguments() {
    let available = RequestUserInputTool::new(request_user_input_available_modes(true))
        .unwrap()
        .with_mode(ModeKind::Plan);
    assert_eq!(reply(&available).await, REPLY);

    let child = run().with_parent_run_id(RunId::new("parent"));
    assert_eq!(
        reply_in(&available, &child).await,
        "request_user_input can only be used by the root thread"
    );

    let unavailable = RequestUserInputTool::new(vec![ModeKind::Plan])
        .unwrap()
        .with_mode(ModeKind::Default);
    let refusal = reply(&unavailable).await;
    assert_ne!(refusal, REPLY);
    assert_eq!(
        Some(refusal),
        ra_tools::request_user_input::request_user_input_unavailable_message(
            ModeKind::Default,
            &[ModeKind::Plan]
        )
    );
    // The root-thread refusal comes first, as in Codex.
    assert_eq!(
        reply_in(&unavailable, &child).await,
        "request_user_input can only be used by the root thread"
    );
}

/// Only a call whose text was not JSON falls back: any other decode error reaches a tool through
/// its own decoding, so a bare one is not answered as unparsed arguments.
#[tokio::test]
async fn other_decode_errors_do_not_fall_back() {
    let tool = RequestPermissionsTool::new(Arc::new(UnusedPolicy)).unwrap();
    let diagnostic = ToolArgumentDecodeError::Deserialize {
        input_type: "Arguments",
        message: "missing field `reason`".to_owned(),
    };
    let error = Error::tool(
        ToolErrorKind::InvalidInput,
        "request_permissions",
        diagnostic.to_string(),
    )
    .with_source(diagnostic);
    let run = run();
    let call_id = CallId::new("call-shape");
    let arguments = Value::Null;
    assert!(
        tool.handle_failure(&ToolContext::new(&run, &tool, &call_id, &arguments), &error)
            .await
            .unwrap()
            .is_none()
    );
    // The same holds for a tool that renders the decode error as its own argument error.
    let read_file = ra_tools::read_file::ReadFileTool::new().unwrap();
    assert!(
        read_file
            .handle_failure(
                &ToolContext::new(&run, &read_file, &call_id, &arguments),
                &error
            )
            .await
            .unwrap()
            .is_none()
    );
}
