//! The question a call is asked about is prepared before anybody answers it, as Codex's handlers
//! prepare an approval before their orchestrator asks the hooks or the reviewer: the tool's
//! `approval_state` runs first, a call it finds it cannot run is not asked about, and the state it
//! keeps reaches the call whether a permission hook or the host approves. A hook's answer is
//! recorded as the approval's resolution, once per question.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result, ToolErrorKind},
    event::{ApprovalResolution, HostEventBody, HostEventSink, InMemoryHostEventSink},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall, ToolResumePoint},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolFailureHandling,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema, ToolServices,
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

/// What happened, in order, across the tool and the hook.
type Journal = Arc<Mutex<Vec<String>>>;

/// How the tool prepares its question.
#[derive(Clone)]
enum Prepares {
    /// Keeps this state with the question.
    State(Value),
    /// Keeps nothing.
    Nothing,
    /// Finds it cannot run the call.
    Fails,
}

/// A tool that asks before it runs, prepares its question as told, and can ask once more from
/// inside its invocation.
struct PreparingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    approval: ToolApprovalPolicy,
    prepares: Prepares,
    asks_inside: Option<Value>,
    journal: Journal,
    invocations: Mutex<Vec<Option<Value>>>,
}

impl PreparingTool {
    fn new(
        approval: ToolApprovalPolicy,
        prepares: Prepares,
        asks_inside: Option<Value>,
        journal: &Journal,
    ) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("prepared").unwrap(),
            schema: ToolSchema::new(
                "prepared",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            approval,
            prepares,
            asks_inside,
            journal: Arc::clone(journal),
            invocations: Mutex::new(Vec::new()),
        })
    }
    fn invocations(&self) -> Vec<Option<Value>> {
        self.invocations.lock().unwrap().clone()
    }
}

#[async_trait]
impl Tool for PreparingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_approval(self.approval)
            .with_failure_handling(ToolFailureHandling::Custom)
    }
    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(true)
    }
    async fn approval_state(&self, _context: &ToolContext<'_>) -> Result<Option<Value>> {
        self.journal
            .lock()
            .unwrap()
            .push("approval_state".to_owned());
        match &self.prepares {
            Prepares::State(state) => Ok(Some(state.clone())),
            Prepares::Nothing => Ok(None),
            Prepares::Fails => Err(Error::tool(
                ToolErrorKind::InvalidInput,
                "prepared",
                "cannot run this call",
            )),
        }
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let resume = context.resume_state().cloned();
        self.invocations.lock().unwrap().push(resume.clone());
        self.journal.lock().unwrap().push("call".to_owned());
        if let Some(ask) = &self.asks_inside
            && resume.as_ref() != Some(ask)
        {
            return Err(ToolApprovalRequest::new(ask.clone()).into_error("prepared"));
        }
        Ok(ToolOutput::text("ran"))
    }
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(Some(ToolOutput::text(format!("failed: {error}"))))
    }
}

/// Answers each permission request with the next scripted decision.
struct PermissionHook {
    decisions: Mutex<Vec<HookDecision>>,
    journal: Journal,
}

impl PermissionHook {
    fn new(decisions: Vec<HookDecision>, journal: &Journal) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions),
            journal: Arc::clone(journal),
        })
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
        if !matches!(event, HookEvent::PermissionRequest(_)) {
            return Ok(HookDecision::Continue);
        }
        self.journal.lock().unwrap().push("hook".to_owned());
        let mut decisions = self.decisions.lock().unwrap();
        Ok(if decisions.is_empty() {
            HookDecision::Continue
        } else {
            decisions.remove(0)
        })
    }
}

#[derive(Default)]
struct Narration(Mutex<Vec<String>>);

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
    async fn on_tool_end(&self, _scope: LifecycleScope, _input: &ToolEndInput<'_>) -> Result<()> {
        self.0.lock().unwrap().push("end".to_owned());
        Ok(())
    }
}

fn calls_prepared() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "prepared", json!({}))),
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

struct Case {
    tool: Arc<PreparingTool>,
    hook: Arc<PermissionHook>,
    narration: Arc<Narration>,
    sink: Arc<InMemoryHostEventSink>,
    journal: Journal,
    rules: Vec<PermissionRule>,
}

impl Case {
    fn new(
        approval: ToolApprovalPolicy,
        prepares: Prepares,
        asks_inside: Option<Value>,
        decisions: Vec<HookDecision>,
    ) -> Self {
        let journal = Journal::default();
        Self {
            tool: PreparingTool::new(approval, prepares, asks_inside, &journal),
            hook: PermissionHook::new(decisions, &journal),
            narration: Arc::new(Narration::default()),
            sink: Arc::new(InMemoryHostEventSink::new()),
            journal,
            rules: Vec::new(),
        }
    }

    fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    fn request(&self, script: Vec<ModelResponse>) -> RunRequest {
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(vec![Arc::clone(&self.tool) as Arc<dyn Tool>])
            .build()
            .unwrap();
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(ScriptedModel::new(script))),
            RunId::new("run-prepared"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_services(
            ToolServices::new().with_event_sink(Arc::clone(&self.sink) as Arc<dyn HostEventSink>),
        )
        .with_config(
            RunConfig::new()
                .with_permission_rules(self.rules.clone())
                .with_lifecycle_hook(Arc::clone(&self.narration) as Arc<dyn LifecycleHook>)
                .with_user_hook(UserHookRegistration::new(
                    HookEventName::PermissionRequest,
                    Arc::clone(&self.hook) as Arc<dyn UserHook>,
                )),
        )
    }

    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }

    fn resolutions(&self) -> Vec<ApprovalResolution> {
        self.sink
            .events()
            .iter()
            .filter_map(|event| match event.body() {
                HostEventBody::Approval(approval) => {
                    assert_eq!(approval.call_id().as_str(), "call-1");
                    Some(approval.resolution())
                }
                _ => None,
            })
            .collect()
    }

    fn narration(&self) -> Vec<String> {
        self.narration.0.lock().unwrap().clone()
    }
}

fn deny() -> HookDecision {
    HookDecision::Deny {
        message: "blocked by hook".to_owned(),
    }
}

fn output_text(items: &[RunItem]) -> String {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output.output().clone()),
            _ => None,
        })
        .map(|output| match ToolOutput::from_stored(&output) {
            Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
            _ => output.to_string(),
        })
        .expect("the call has an output")
}

fn the_two_sources_of_a_question() -> [(&'static str, ToolApprovalPolicy, Option<PermissionRule>); 2]
{
    [
        ("the tool's own answer", ToolApprovalPolicy::Dynamic, None),
        (
            "a fixed ask rule",
            ToolApprovalPolicy::Never,
            Some(PermissionRule::new(PermissionDecision::Ask).with_tool_name("prepared")),
        ),
    ]
}

/// A hook that approves hands the call the state the tool prepared, before the hook was asked.
#[tokio::test]
async fn a_hook_approval_runs_the_call_with_the_prepared_state() {
    for (source, approval, rule) in the_two_sources_of_a_question() {
        let state = json!({"prepared": true});
        let mut case = Case::new(
            approval,
            Prepares::State(state.clone()),
            None,
            vec![HookDecision::Allow],
        );
        if let Some(rule) = rule {
            case = case.with_rule(rule);
        }
        let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
            .await
            .unwrap();

        assert!(
            matches!(result.outcome(), RunOutcome::Completed { .. }),
            "{source}"
        );
        assert_eq!(
            case.journal(),
            ["approval_state", "hook", "call"],
            "{source}"
        );
        assert_eq!(case.tool.invocations(), [Some(state)], "{source}");
        assert_eq!(
            case.resolutions(),
            [ApprovalResolution::Approved],
            "{source}"
        );
        assert_eq!(case.narration(), ["start", "end"], "{source}");
    }
}

/// Undecided, the question goes to the host with the same state, and the approved call runs with
/// it; the runtime records no answer, as the host's answer is the host's to record.
#[tokio::test]
async fn an_undecided_question_reaches_the_host_with_the_prepared_state() {
    let state = json!({"prepared": "for the host"});
    let case = Case::new(
        ToolApprovalPolicy::Dynamic,
        Prepares::State(state.clone()),
        None,
        Vec::new(),
    );
    let first = Runner::run(case.request(vec![calls_prepared()]))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval, got {:?}", first.outcome());
    };
    let RunItemKind::ToolApproval(approval) = items[0].kind() else {
        panic!("expected a tool approval");
    };
    let resume = approval.resume().expect("the approval keeps the state");
    assert_eq!(
        (resume.point(), resume.state()),
        (ToolResumePoint::BeforeCall, &state)
    );
    assert_eq!(case.journal(), ["approval_state", "hook"]);
    assert!(case.resolutions().is_empty());

    let mut checkpoint: RunState = first.state().clone();
    checkpoint.approve(&items[0], false).unwrap();
    let resumed = Runner::run(case.request(vec![finishes()]).with_state(checkpoint))
        .await
        .unwrap();
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.tool.invocations(), [Some(state)]);
    assert_eq!(case.journal(), ["approval_state", "hook", "call"]);
}

/// A hook that refuses refuses the call, which never starts, and the refusal is recorded.
#[tokio::test]
async fn a_hook_refusal_is_recorded_and_the_call_never_starts() {
    let case = Case::new(
        ToolApprovalPolicy::Dynamic,
        Prepares::State(json!({"prepared": true})),
        None,
        vec![deny()],
    );
    let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
        .await
        .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.journal(), ["approval_state", "hook"]);
    assert!(case.tool.invocations().is_empty());
    assert_eq!(output_text(result.new_items()), "blocked by hook");
    assert_eq!(case.resolutions(), [ApprovalResolution::Declined]);
    assert!(case.narration().is_empty(), "the call never started");
}

/// A call the tool finds it cannot run while preparing its question ends as its failure, and
/// nobody — neither a hook nor the host — is asked about it.
#[tokio::test]
async fn a_call_that_cannot_be_prepared_is_not_asked_about() {
    for (source, approval, rule) in the_two_sources_of_a_question() {
        let mut case = Case::new(approval, Prepares::Fails, None, vec![HookDecision::Allow]);
        if let Some(rule) = rule {
            case = case.with_rule(rule);
        }
        let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
            .await
            .unwrap();

        assert!(
            matches!(result.outcome(), RunOutcome::Completed { .. }),
            "{source}"
        );
        assert_eq!(case.journal(), ["approval_state"], "{source}");
        assert!(
            output_text(result.new_items()).starts_with("failed: "),
            "{source}"
        );
        assert!(case.resolutions().is_empty(), "{source}");
        assert!(case.narration().is_empty(), "{source}");
    }
}

/// A tool that keeps nothing runs as it always did once a hook approves.
#[tokio::test]
async fn a_tool_that_keeps_nothing_runs_without_state() {
    let case = Case::new(
        ToolApprovalPolicy::Always,
        Prepares::Nothing,
        None,
        vec![HookDecision::Allow],
    );
    let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
        .await
        .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.tool.invocations(), [None]);
}

/// Each question of one call is answered and recorded on its own: the question before the call
/// and the one the tool asks from inside it make two answers under one call id.
#[tokio::test]
async fn every_hook_answer_of_one_call_is_recorded() {
    let before = json!({"prepared": "before"});
    let inside = json!({"asked": "inside"});

    let case = Case::new(
        ToolApprovalPolicy::Dynamic,
        Prepares::State(before.clone()),
        Some(inside.clone()),
        vec![HookDecision::Allow, HookDecision::Allow],
    );
    let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
        .await
        .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        case.tool.invocations(),
        [Some(before.clone()), Some(inside.clone())]
    );
    assert_eq!(
        case.resolutions(),
        [ApprovalResolution::Approved, ApprovalResolution::Approved]
    );

    let case = Case::new(
        ToolApprovalPolicy::Dynamic,
        Prepares::State(before.clone()),
        Some(inside),
        vec![HookDecision::Allow, deny()],
    );
    let result = Runner::run(case.request(vec![calls_prepared(), finishes()]))
        .await
        .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        case.resolutions(),
        [ApprovalResolution::Approved, ApprovalResolution::Declined]
    );
    assert_eq!(output_text(result.new_items()), "blocked by hook");
}
