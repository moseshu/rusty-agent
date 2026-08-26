//! Permission-policy evaluation at the common tool-dispatch boundary.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId},
    permission::{PermissionDecision, PermissionMode, PermissionRule, PermissionScope},
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolNamespace, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use ra_runtime::{
    permission::PermissionEngine,
    runner::RunConfig,
    tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
};
use serde_json::json;

struct CountingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
}

impl CountingTool {
    fn new(name: &str, options: ToolOptions, calls: Arc<AtomicUsize>) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options,
            calls,
        }
    }

    fn namespaced(
        name: &str,
        namespace: &str,
        options: ToolOptions,
        calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            origin: ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options,
            calls,
        }
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("ran"))
    }
}

fn run() -> Arc<RunContext> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("permission-agent"))
        .name("Permission agent")
        .build()
        .unwrap();
    Arc::new(RunContext::new(RunId::new("run-permission"), &agent))
}

async fn dispatch(tool: Arc<dyn Tool>, permission: PermissionEngine) -> ToolDispatch {
    dispatch_tool(ToolDispatchRequest::new(
        tool,
        CallId::new("call-permission"),
        json!({}),
        run(),
        CancelScope::root(),
        CallHistory::default(),
        permission,
    ))
    .await
    .expect("permission decisions must settle as a dispatch result")
}

#[tokio::test]
async fn test_permission_engine_01_explicit_deny_beats_bypass() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new().with_permission_scope(PermissionScope::Execute),
        Arc::clone(&calls),
    ));
    let permission = PermissionEngine::new(PermissionMode::BypassPermissions)
        .with_rules([PermissionRule::new(PermissionDecision::Deny).with_tool_name("exec_command")]);

    let result = dispatch(tool, permission).await;
    let ToolDispatch::Refused(refusal) = result else {
        panic!("an explicit deny must refuse the call");
    };
    assert_eq!(refusal.code(), "tool.permission_denied");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_permission_engine_02_last_matching_rule_overrides_broad_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "apply_patch",
        ToolOptions::new()
            .with_approval(ToolApprovalPolicy::Always)
            .with_permission_scope(PermissionScope::Edit),
        Arc::clone(&calls),
    ));
    let permission = PermissionEngine::new(PermissionMode::Default).with_rules([
        PermissionRule::new(PermissionDecision::Deny),
        PermissionRule::new(PermissionDecision::Allow).with_tool_name("apply_patch"),
    ]);

    let result = dispatch(tool, permission).await;
    assert!(matches!(result, ToolDispatch::Observed(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_permission_engine_03_modes_apply_to_declared_scope() {
    let edit_calls = Arc::new(AtomicUsize::new(0));
    let edit: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "apply_patch",
        ToolOptions::new()
            .with_approval(ToolApprovalPolicy::Always)
            .with_permission_scope(PermissionScope::Edit),
        Arc::clone(&edit_calls),
    ));
    let accepted = dispatch(edit, PermissionEngine::new(PermissionMode::AcceptEdits)).await;
    assert!(matches!(accepted, ToolDispatch::Observed(_)));
    assert_eq!(edit_calls.load(Ordering::SeqCst), 1);

    let command_calls = Arc::new(AtomicUsize::new(0));
    let command: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new().with_permission_scope(PermissionScope::Execute),
        Arc::clone(&command_calls),
    ));
    let planned = dispatch(
        command,
        PermissionEngine::new(PermissionMode::Plan)
            .with_rules([
                PermissionRule::new(PermissionDecision::Allow).with_tool_name("exec_command")
            ]),
    )
    .await;
    assert!(matches!(planned, ToolDispatch::Refused(_)));
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);

    let read_calls = Arc::new(AtomicUsize::new(0));
    let read: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "read_file",
        ToolOptions::new()
            .with_approval(ToolApprovalPolicy::Always)
            .with_permission_scope(PermissionScope::Read),
        read_calls,
    ));
    let readable = dispatch(read, PermissionEngine::new(PermissionMode::Plan)).await;
    assert!(matches!(readable, ToolDispatch::AwaitingApproval(_)));
}

#[tokio::test]
async fn test_permission_engine_04_dont_ask_denies_pending_approval() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
        Arc::clone(&calls),
    ));

    let result = dispatch(tool, PermissionEngine::new(PermissionMode::DontAsk)).await;
    assert!(matches!(result, ToolDispatch::Refused(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_permission_engine_05_run_config_keeps_rules_when_switching_modes() {
    let rule = PermissionRule::new(PermissionDecision::Deny).with_tool_name("exec_command");
    let config = RunConfig::new()
        .with_permission_rules([rule.clone()])
        .with_permission_mode(PermissionMode::BypassPermissions);

    assert_eq!(
        config.permission().mode(),
        PermissionMode::BypassPermissions
    );
    assert_eq!(config.permission().rules(), [rule]);
}

#[tokio::test]
async fn test_permission_engine_06_namespace_rules_match_bare_model_names() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::namespaced(
        "search",
        "remote",
        ToolOptions::new(),
        Arc::clone(&calls),
    ));
    let permission =
        PermissionEngine::new(PermissionMode::BypassPermissions).with_rules([PermissionRule::new(
            PermissionDecision::Deny,
        )
        .with_tool_name("search")
        .with_namespace("remote")]);

    let result = dispatch(tool, permission).await;
    assert!(matches!(result, ToolDispatch::Refused(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_permission_engine_07_non_edit_scopes_keep_the_tool_policy_in_accept_edits() {
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new()
            .with_approval(ToolApprovalPolicy::Always)
            .with_permission_scope(PermissionScope::Execute),
        Arc::new(AtomicUsize::new(0)),
    ));

    let result = dispatch(tool, PermissionEngine::new(PermissionMode::AcceptEdits)).await;
    assert!(matches!(result, ToolDispatch::AwaitingApproval(_)));
}

#[tokio::test]
async fn test_permission_engine_08_ask_rule_overrides_bypass_mode() {
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new(),
        Arc::new(AtomicUsize::new(0)),
    ));
    let permission = PermissionEngine::new(PermissionMode::BypassPermissions)
        .with_rules([PermissionRule::new(PermissionDecision::Ask).with_tool_name("exec_command")]);

    let result = dispatch(tool, permission).await;
    assert!(matches!(result, ToolDispatch::AwaitingApproval(_)));
}
