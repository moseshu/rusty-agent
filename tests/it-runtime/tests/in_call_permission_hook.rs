//! Permission hooks answer an approval a tool asks for from inside its invocation, as Codex's
//! `request_approval` asks them before any approval reaches a reviewer: a grant continues the
//! invocation with the kept state, a denial refuses the call, and no decision leaves the approval
//! for the host. Every such request is put to the hooks.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall, ToolResumePoint},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    hook::UserHookRegistration,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::{Value, json};

struct ScriptedModel(Mutex<Vec<ModelResponse>>);

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(script)))
    }
    fn next(&self) -> Result<ModelResponse> {
        let mut script = self.0.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next()
    }
    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next()
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// Asks from inside its invocation once per entry of `asks`, each time with that entry's state,
/// and answers once it has been resumed past the last one.
struct AskingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    asks_before: bool,
    asks: Vec<Value>,
    invocations: Mutex<Vec<Option<Value>>>,
}

impl AskingTool {
    fn new(asks_before: bool, asks: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("staged").unwrap(),
            schema: ToolSchema::new(
                "staged",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            asks_before,
            asks,
            invocations: Mutex::new(Vec::new()),
        })
    }
    fn invocations(&self) -> Vec<Option<Value>> {
        self.invocations.lock().unwrap().clone()
    }
}

#[async_trait]
impl Tool for AskingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic)
    }
    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(self.asks_before)
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let resume = context.resume_state().cloned();
        self.invocations.lock().unwrap().push(resume.clone());
        let next = resume
            .as_ref()
            .and_then(|state| self.asks.iter().position(|ask| ask == state))
            .map_or(0, |index| index + 1);
        match self.asks.get(next) {
            Some(ask) => Err(ToolApprovalRequest::new(ask.clone()).into_error("staged")),
            None => Ok(ToolOutput::text(format!("ran after {resume:?}"))),
        }
    }
}

/// Answers each permission request with the next scripted decision and records the tool state
/// it was shown. Optionally cancels the run once it has answered.
struct PermissionHook {
    decisions: Mutex<VecDeque<HookDecision>>,
    seen: Mutex<Vec<Option<Value>>>,
    cancel_after: Option<CancelScope>,
}

impl PermissionHook {
    fn new(decisions: impl IntoIterator<Item = HookDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
            cancel_after: None,
        })
    }
    fn seen(&self) -> Vec<Option<Value>> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl UserHook for PermissionHook {
    fn name(&self) -> &str {
        "permission"
    }
    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        let HookEvent::PermissionRequest(call) = event else {
            return Ok(HookDecision::Continue);
        };
        self.seen
            .lock()
            .unwrap()
            .push(call.approval_state().cloned());
        let decision = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(HookDecision::Continue);
        if let Some(scope) = &self.cancel_after {
            scope.cancel(CancelReason::UserInterrupt);
        }
        Ok(decision)
    }
}

/// Both moments in order, and the answer each end was told about.
#[derive(Default)]
struct Narration(
    Mutex<Vec<String>>,
    Mutex<Vec<ra_core::item::ToolCallOutput>>,
);

#[async_trait]
impl LifecycleHook for Narration {
    fn name(&self) -> &str {
        "narration"
    }
    async fn on_tool_start(
        &self,
        _scope: LifecycleScope,
        _input: &ToolStartInput<'_>,
    ) -> Result<()> {
        self.0.lock().unwrap().push("start".to_owned());
        Ok(())
    }
    async fn on_tool_end(&self, _scope: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("end/{}", input.failure_code().unwrap_or("ok")));
        self.1.lock().unwrap().push(input.output().clone());
        Ok(())
    }
}

fn calls_staged() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "staged", json!({}))),
    )])
}

fn finishes() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(ra_core::item::Message::assistant(
            "done",
            ra_core::item::OutputPhase::Final,
        )),
    )])
}

fn request(
    tool: &Arc<AskingTool>,
    hook: &Arc<PermissionHook>,
    narration: &Arc<Narration>,
    script: Vec<ModelResponse>,
    cancel: CancelScope,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("do the thing")
        .tools(vec![Arc::clone(tool) as Arc<dyn Tool>])
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(ScriptedModel::new(script))),
        RunId::new("run-in-call"),
        cancel,
        Vec::new(),
    )
    .with_config(
        RunConfig::new()
            .with_lifecycle_hook(Arc::clone(narration) as Arc<dyn LifecycleHook>)
            .with_user_hook(UserHookRegistration::new(
                HookEventName::PermissionRequest,
                Arc::clone(hook) as Arc<dyn UserHook>,
            )),
    )
}

fn deny(message: &str) -> HookDecision {
    HookDecision::Deny {
        message: message.to_owned(),
    }
}

fn tool_output(items: &[RunItem]) -> &ra_core::item::ToolCallOutput {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .expect("the call has an output")
}

fn approval_resume(items: &[RunItem]) -> (ToolResumePoint, Value) {
    let [item] = items else {
        panic!("expected one approval, got {items:?}");
    };
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected a tool approval");
    };
    let resume = approval
        .resume()
        .expect("the approval keeps the tool state");
    (resume.point(), resume.state().clone())
}

#[tokio::test]
async fn a_granted_request_continues_the_invocation_without_an_approval() {
    let retry = json!({"stage": "retry"});
    let tool = AskingTool::new(false, vec![retry.clone()]);
    let hook = PermissionHook::new([HookDecision::Allow]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        CancelScope::root(),
    ))
    .await
    .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None, Some(retry.clone())]);
    assert_eq!(hook.seen(), [Some(retry)]);
    assert!(
        !result
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolApproval(_)))
    );
    assert!(!tool_output(result.new_items()).is_error());
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);
}

#[tokio::test]
async fn a_denied_request_refuses_the_call_and_closes_the_invocation() {
    let tool = AskingTool::new(false, vec![json!({"stage": "retry"})]);
    let hook = PermissionHook::new([deny("blocked by hook")]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        CancelScope::root(),
    ))
    .await
    .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None]);
    let output = tool_output(result.new_items());
    assert!(output.is_error());
    assert_eq!(
        output.output(),
        &serde_json::to_value(ToolOutput::text("blocked by hook")).unwrap()
    );
    // The refusal is the answer the started invocation settles into.
    assert_eq!(
        *narration.0.lock().unwrap(),
        ["start", "end/hook.permission_request"]
    );
    assert_eq!(*narration.1.lock().unwrap(), [output.clone()]);
}

#[tokio::test]
async fn without_a_decision_the_request_waits_for_the_host_and_is_not_asked_again() {
    let retry = json!({"stage": "retry"});
    let tool = AskingTool::new(false, vec![retry.clone()]);
    let hook = PermissionHook::new([]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged()],
        CancelScope::root(),
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("expected an approval, got {:?}", result.outcome());
    };
    assert_eq!(
        approval_resume(items),
        (ToolResumePoint::DuringCall, retry.clone())
    );
    let item = items[0].clone();
    let mut state = result.state().clone();
    state.approve(&item, false).unwrap();
    let resumed = Runner::run(
        request(
            &tool,
            &hook,
            &narration,
            vec![finishes()],
            CancelScope::root(),
        )
        .with_state(state),
    )
    .await
    .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None, Some(retry.clone())]);
    assert_eq!(hook.seen(), [Some(retry)]);
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);
}

/// A tool that keeps no state with the question asked before its call shows the hooks none
/// there; the request it raises from inside the call carries the state it kept with it.
#[tokio::test]
async fn a_tool_that_keeps_no_state_before_the_call_shows_the_hooks_none_there() {
    let retry = json!({"stage": "retry"});
    let tool = AskingTool::new(true, vec![retry.clone()]);
    let hook = PermissionHook::new([HookDecision::Allow, HookDecision::Allow]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        CancelScope::root(),
    ))
    .await
    .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(hook.seen(), [None, Some(retry.clone())]);
    assert_eq!(tool.invocations(), [None, Some(retry)]);
}

#[tokio::test]
async fn every_request_inside_one_invocation_is_put_to_the_hooks() {
    let first = json!({"stage": "first"});
    let second = json!({"stage": "second"});

    // Two grants: the invocation continues twice and completes.
    let tool = AskingTool::new(false, vec![first.clone(), second.clone()]);
    let hook = PermissionHook::new([HookDecision::Allow, HookDecision::Allow]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        tool.invocations(),
        [None, Some(first.clone()), Some(second.clone())]
    );
    assert_eq!(hook.seen(), [Some(first.clone()), Some(second.clone())]);
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);

    // A grant, then a denial: the second request is refused, not handed to the host.
    let tool = AskingTool::new(false, vec![first.clone(), second.clone()]);
    let hook = PermissionHook::new([HookDecision::Allow, deny("second refused")]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None, Some(first.clone())]);
    assert_eq!(
        tool_output(result.new_items()).output(),
        &serde_json::to_value(ToolOutput::text("second refused")).unwrap()
    );
    assert_eq!(narration.0.lock().unwrap().len(), 2);

    // A grant, then no decision: the host is asked about the second request with its state.
    let tool = AskingTool::new(false, vec![first.clone(), second.clone()]);
    let hook = PermissionHook::new([HookDecision::Allow]);
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged()],
        CancelScope::root(),
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("expected an approval, got {:?}", result.outcome());
    };
    assert_eq!(
        approval_resume(items),
        (ToolResumePoint::DuringCall, second.clone())
    );
    assert_eq!(hook.seen(), [Some(first.clone()), Some(second)]);
    assert_eq!(tool.invocations(), [None, Some(first)]);
}

#[tokio::test]
async fn a_call_cancelled_while_its_hooks_decide_is_not_invoked_again() {
    let retry = json!({"stage": "retry"});
    let tool = AskingTool::new(false, vec![retry.clone()]);
    let scope = CancelScope::root();
    let hook = Arc::new(PermissionHook {
        decisions: Mutex::new(VecDeque::from([HookDecision::Allow])),
        seen: Mutex::new(Vec::new()),
        cancel_after: Some(scope.clone()),
    });
    let narration = Arc::new(Narration::default());
    let result = Runner::run(request(
        &tool,
        &hook,
        &narration,
        vec![calls_staged(), finishes()],
        scope,
    ))
    .await;

    assert!(
        result.as_ref().is_err_and(Error::is_cancelled),
        "the run ends cancelled: {result:?}"
    );
    assert_eq!(hook.seen(), [Some(retry)]);
    assert_eq!(tool.invocations(), [None]);
}
