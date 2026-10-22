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
    permission::{PermissionDecision, PermissionRule},
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolNamespace, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    permission::{ActionClassPolicy, PermissionEngine},
    runner::RunConfig,
    tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
};
use serde_json::json;

struct CountingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
    approval_checks: Option<Arc<AtomicUsize>>,
    dynamic_approval: bool,
    asks_during_call: bool,
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
            approval_checks: None,
            dynamic_approval: false,
            asks_during_call: false,
        }
    }

    fn dynamic_approval(calls: Arc<AtomicUsize>, approval_checks: Arc<AtomicUsize>) -> Self {
        let mut tool = Self::new(
            "dynamic",
            ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic),
            calls,
        );
        tool.approval_checks = Some(approval_checks);
        tool.dynamic_approval = true;
        tool
    }

    fn asks_during_call(calls: Arc<AtomicUsize>) -> Self {
        let mut tool = Self::new("internal_approval", ToolOptions::new(), calls);
        tool.asks_during_call = true;
        tool
    }

    fn with_action_class(mut self, class: &str) -> Self {
        self.options = self.options.with_action_classes([class]);
        self
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
            approval_checks: None,
            dynamic_approval: false,
            asks_during_call: false,
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
        if self.asks_during_call {
            return Err(ToolApprovalRequest::new(json!({"reason": "internal"}))
                .into_error("internal_approval"));
        }
        Ok(ToolOutput::text("ran"))
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        if let Some(checks) = &self.approval_checks {
            checks.fetch_add(1, Ordering::SeqCst);
        }
        Ok(self.dynamic_approval)
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
    .into_parts()
    .0
}

#[tokio::test]
async fn test_permission_engine_01_explicit_deny_beats_bypass() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new().with_action_classes([ra_tools::action_classes::EXECUTE]),
        Arc::clone(&calls),
    ));
    let permission = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_auto_allow_all(true))
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
            .with_action_classes([ra_tools::action_classes::EDIT]),
        Arc::clone(&calls),
    ));
    let permission = PermissionEngine::new().with_rules([
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
            .with_action_classes([ra_tools::action_classes::EDIT]),
        Arc::clone(&edit_calls),
    ));
    let accepted = dispatch(
        edit,
        PermissionEngine::new().with_policy(
            ActionClassPolicy::new().with_auto_allow([ra_tools::action_classes::EDIT]),
        ),
    )
    .await;
    assert!(matches!(accepted, ToolDispatch::Observed(_)));
    assert_eq!(edit_calls.load(Ordering::SeqCst), 1);

    let command_calls = Arc::new(AtomicUsize::new(0));
    let command: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new().with_action_classes([ra_tools::action_classes::EXECUTE]),
        Arc::clone(&command_calls),
    ));
    let planned = dispatch(
        command,
        PermissionEngine::new()
            .with_policy(ActionClassPolicy::new().with_allow_only([ra_tools::action_classes::READ]))
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
            .with_action_classes([ra_tools::action_classes::READ]),
        read_calls,
    ));
    let readable = dispatch(
        read,
        PermissionEngine::new().with_policy(
            ActionClassPolicy::new().with_allow_only([ra_tools::action_classes::READ]),
        ),
    )
    .await;
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

    let result = dispatch(
        tool,
        PermissionEngine::new().with_policy(ActionClassPolicy::new().with_prompts(false)),
    )
    .await;
    assert!(matches!(result, ToolDispatch::Refused(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_permission_engine_05_run_config_keeps_rules_when_switching_modes() {
    let rule = PermissionRule::new(PermissionDecision::Deny).with_tool_name("exec_command");
    let config = RunConfig::new()
        .with_permission_rules([rule.clone()])
        .with_permission_policy(ActionClassPolicy::new().with_auto_allow_all(true));

    assert_eq!(
        config
            .permission()
            .policy()
            .and_then(ActionClassPolicy::allow_only),
        None
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
    let permission = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_auto_allow_all(true))
        .with_rules([PermissionRule::new(PermissionDecision::Deny)
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
            .with_action_classes([ra_tools::action_classes::EXECUTE]),
        Arc::new(AtomicUsize::new(0)),
    ));

    let result = dispatch(
        tool,
        PermissionEngine::new().with_policy(
            ActionClassPolicy::new().with_auto_allow([ra_tools::action_classes::EDIT]),
        ),
    )
    .await;
    assert!(matches!(result, ToolDispatch::AwaitingApproval(_)));
}

#[tokio::test]
async fn test_permission_engine_08_ask_rule_overrides_bypass_mode() {
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
        "exec_command",
        ToolOptions::new(),
        Arc::new(AtomicUsize::new(0)),
    ));
    let permission = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_auto_allow_all(true))
        .with_rules([PermissionRule::new(PermissionDecision::Ask).with_tool_name("exec_command")]);

    let result = dispatch(tool, permission).await;
    assert!(matches!(result, ToolDispatch::AwaitingApproval(_)));
}

#[test]
fn class_policies_require_every_declared_class_and_treat_unclassified_actions_explicitly() {
    let origin = ToolOrigin::new("multi").unwrap();
    let agent_id = AgentId::new("permission-agent");
    let read = ra_core::permission::ActionClass::new("read");
    let edit = ra_core::permission::ActionClass::new("edit");
    let mixed = [read.clone(), edit.clone()];

    let allow_read = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_allow_only([ra_tools::action_classes::READ]));
    assert_eq!(
        allow_read.evaluate(&[read.clone()], &origin, &agent_id, PermissionDecision::Ask),
        PermissionDecision::Ask
    );
    assert_eq!(
        allow_read.evaluate(&mixed, &origin, &agent_id, PermissionDecision::Allow),
        PermissionDecision::Deny
    );
    assert_eq!(
        allow_read.evaluate(&[], &origin, &agent_id, PermissionDecision::Allow),
        PermissionDecision::Deny
    );

    let auto_read = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_auto_allow([ra_tools::action_classes::READ]));
    assert_eq!(
        auto_read.evaluate(&mixed, &origin, &agent_id, PermissionDecision::Ask),
        PermissionDecision::Ask
    );
    assert_eq!(
        auto_read.evaluate(&[], &origin, &agent_id, PermissionDecision::Ask),
        PermissionDecision::Ask
    );

    let auto_both = PermissionEngine::new()
        .with_policy(ActionClassPolicy::new().with_auto_allow(["edit", "read"]));
    assert_eq!(
        auto_both.evaluate(&mixed, &origin, &agent_id, PermissionDecision::Ask),
        PermissionDecision::Allow
    );
}

#[test]
fn disabling_prompts_preserves_category_rules_and_automatic_approval() {
    let origin = ToolOrigin::new("tool").unwrap();
    let agent_id = AgentId::new("permission-agent");
    let permission = PermissionEngine::new().with_policy(
        ActionClassPolicy::new()
            .with_allow_only(["read", "edit"])
            .with_auto_allow([ra_tools::action_classes::EDIT])
            .with_prompts(false),
    );

    assert_eq!(
        permission.evaluate(
            &["execute".into()],
            &origin,
            &agent_id,
            PermissionDecision::Ask
        ),
        PermissionDecision::Deny
    );
    assert_eq!(
        permission.evaluate(
            &["read".into()],
            &origin,
            &agent_id,
            PermissionDecision::Ask
        ),
        PermissionDecision::Deny
    );
    assert_eq!(
        permission.evaluate(
            &["edit".into()],
            &origin,
            &agent_id,
            PermissionDecision::Ask
        ),
        PermissionDecision::Allow
    );
}

#[tokio::test]
async fn no_class_policy_preserves_dynamic_tool_approval_and_fixed_auto_allow_skips_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(AtomicUsize::new(0));
    let dynamic: Arc<dyn Tool> = Arc::new(CountingTool::dynamic_approval(
        Arc::clone(&calls),
        Arc::clone(&checks),
    ));

    let awaiting = dispatch(dynamic, PermissionEngine::new()).await;
    assert!(matches!(awaiting, ToolDispatch::AwaitingApproval(_)));
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let calls = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(AtomicUsize::new(0));
    let dynamic: Arc<dyn Tool> = Arc::new(
        CountingTool::dynamic_approval(Arc::clone(&calls), Arc::clone(&checks))
            .with_action_class("edit"),
    );
    let allowed = dispatch(
        dynamic,
        PermissionEngine::new().with_policy(
            ActionClassPolicy::new().with_auto_allow([ra_tools::action_classes::EDIT]),
        ),
    )
    .await;
    assert!(matches!(allowed, ToolDispatch::Observed(_)));
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn action_class_bypass_does_not_suppress_approval_requested_inside_the_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(CountingTool::asks_during_call(Arc::clone(&calls)));
    let result = dispatch(
        tool,
        PermissionEngine::new().with_policy(ActionClassPolicy::new().with_auto_allow_all(true)),
    )
    .await;

    assert!(matches!(result, ToolDispatch::AwaitingApproval(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
