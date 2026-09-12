//! Where lifecycle narration sits in the loop and in the dispatch chain, and what each scope hears.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    context::RunContext,
    error::{Error, Result},
    finish::FinishReason,
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    lifecycle::{
        AgentEndInput, AgentStartInput, HandoffInput, LifecycleEvent, LifecycleHook,
        LifecycleScope, LlmEndInput, LlmStartInput, ToolEndInput, ToolStartInput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    permission::PermissionMode,
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolGuardrailId, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::AgentBinding,
    hook::{UserHookRegistration, UserHooks},
    lifecycle::LifecycleHooks,
    permission::PermissionEngine,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
    tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
};
use serde_json::json;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A tool that counts how often it ran.
struct CountingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
    fails: bool,
    entered: Arc<tokio::sync::Notify>,
    hangs: bool,
}

impl CountingTool {
    fn new(name: &str, options: ToolOptions) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new(name).expect("a tool identity"),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .expect("a tool schema"),
            options,
            calls: Arc::new(AtomicUsize::new(0)),
            fails: false,
            entered: Arc::new(tokio::sync::Notify::new()),
            hangs: false,
        })
    }

    fn plain(name: &str) -> Arc<Self> {
        Self::new(name, ToolOptions::new())
    }

    /// A tool whose every call fails in a way the model is shown.
    fn failing(name: &str) -> Arc<Self> {
        let mut tool = Self::new(name, ToolOptions::new());
        Arc::get_mut(&mut tool).expect("a fresh handle").fails = true;
        tool
    }

    /// A tool that announces it started and then never finishes on its own.
    fn hanging(name: &str) -> Arc<Self> {
        let mut tool = Self::new(name, ToolOptions::new());
        Arc::get_mut(&mut tool).expect("a fresh handle").hangs = true;
        tool
    }

    fn approval_gated(name: &str) -> Arc<Self> {
        Self::new(
            name,
            ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic),
        )
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(true)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.hangs {
            std::future::pending::<()>().await;
        }
        if self.fails {
            return Err(Error::tool(
                ra_core::error::ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                "the tool could not finish",
            ));
        }
        Ok(ToolOutput::text("the tool ran"))
    }
}

/// Narration that writes down what it was told, and can be made to fail at one chosen moment.
struct Recording {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    fails_at: Option<LifecycleEvent>,
    /// Whether the scope is written into the log beside the moment.
    scoped: bool,
}

impl Recording {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            log: Arc::new(Mutex::new(Vec::new())),
            fails_at: None,
            scoped: false,
        })
    }

    /// Narration sharing one log with its neighbours, so their order is observable.
    fn sharing(name: &str, log: &Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            log: Arc::clone(log),
            fails_at: None,
            scoped: false,
        })
    }

    /// Narration that reports which of its two installations each firing came through.
    fn scoped(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            log: Arc::new(Mutex::new(Vec::new())),
            fails_at: None,
            scoped: true,
        })
    }

    fn failing_at(name: &str, event: LifecycleEvent) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            log: Arc::new(Mutex::new(Vec::new())),
            fails_at: Some(event),
            scoped: false,
        })
    }

    fn seen(&self) -> Vec<String> {
        self.log.lock().expect("no poisoned fixture").clone()
    }

    fn record(&self, event: LifecycleEvent, scope: LifecycleScope, detail: String) -> Result<()> {
        let entry = if self.scoped {
            format!("{}@{}:{detail}", event.code(), scope.code())
        } else {
            format!("{}:{detail}", event.code())
        };
        self.log.lock().expect("no poisoned fixture").push(entry);
        if self.fails_at == Some(event) {
            return Err(Error::caller("the collector could not reach its backend"));
        }
        Ok(())
    }
}

#[async_trait]
impl LifecycleHook for Recording {
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_agent_start(
        &self,
        scope: LifecycleScope,
        input: &AgentStartInput<'_>,
    ) -> Result<()> {
        self.record(
            LifecycleEvent::AgentStart,
            scope,
            format!(
                "{}/{}/{}",
                input.agent().id(),
                input.input().len(),
                if input.is_resumed() {
                    "resumed"
                } else {
                    "fresh"
                }
            ),
        )
    }

    async fn on_agent_end(&self, scope: LifecycleScope, input: &AgentEndInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::AgentEnd,
            scope,
            format!(
                "{}/{}",
                input.finish_reason().code(),
                input
                    .message()
                    .map_or_else(|| "<none>".to_owned(), Message::text_content)
            ),
        )
    }

    async fn on_llm_start(&self, scope: LifecycleScope, input: &LlmStartInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::LlmStart,
            scope,
            format!(
                "{}/{}",
                input.model().model().unwrap_or("<default>"),
                input.input().len()
            ),
        )
    }

    async fn on_llm_end(&self, scope: LifecycleScope, input: &LlmEndInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::LlmEnd,
            scope,
            format!(
                "{}/{}",
                input.response().output().len(),
                input.run().usage_totals().requests()
            ),
        )
    }

    async fn on_tool_start(&self, scope: LifecycleScope, input: &ToolStartInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::ToolStart,
            scope,
            input.origin().qualified_name().to_owned(),
        )
    }

    async fn on_tool_end(&self, scope: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::ToolEnd,
            scope,
            format!(
                "{}/{}",
                input.origin().qualified_name(),
                input.failure_code().unwrap_or("ok")
            ),
        )
    }

    async fn on_handoff(&self, scope: LifecycleScope, input: &HandoffInput<'_>) -> Result<()> {
        self.record(
            LifecycleEvent::Handoff,
            scope,
            format!("{}->{}", input.from().id(), input.to().id()),
        )
    }
}

/// A deciding hook that writes into the same log, so its position relative to the bracket shows.
struct Deciding {
    log: Arc<Mutex<Vec<String>>>,
    decision: HookDecision,
}

impl Deciding {
    fn new(log: &Arc<Mutex<Vec<String>>>, decision: HookDecision) -> Arc<Self> {
        Arc::new(Self {
            log: Arc::clone(log),
            decision,
        })
    }
}

/// A stop hook that holds the first delivery back and lets the second through.
struct BlockingOnce {
    prompts: Mutex<std::collections::VecDeque<HookDecision>>,
}

impl BlockingOnce {
    fn new(prompt: &str) -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(
                [HookDecision::Block {
                    prompt: prompt.to_owned(),
                }]
                .into(),
            ),
        })
    }
}

#[async_trait]
impl UserHook for BlockingOnce {
    fn name(&self) -> &str {
        "reviewer"
    }

    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        _event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        Ok(self
            .prompts
            .lock()
            .expect("no poisoned fixture")
            .pop_front()
            .unwrap_or_default())
    }
}

#[async_trait]
impl UserHook for Deciding {
    fn name(&self) -> &str {
        "policy"
    }

    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        let name = match event {
            HookEvent::PreToolUse(call) => call.origin().qualified_name().to_owned(),
            _ => "<other>".to_owned(),
        };
        self.log
            .lock()
            .expect("no poisoned fixture")
            .push(format!("pre_tool_use:{name}"));
        Ok(self.decision.clone())
    }
}

fn run_context() -> Arc<RunContext> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .build()
        .expect("an agent with an identity and a name");
    Arc::new(RunContext::new(RunId::new("run-lifecycle"), &agent))
}

fn installed(
    run: Vec<Arc<dyn LifecycleHook>>,
    agent: Vec<Arc<dyn LifecycleHook>>,
) -> LifecycleHooks {
    LifecycleHooks::installed(&run, &agent)
}

fn request(tool: Arc<dyn Tool>, lifecycle: LifecycleHooks) -> ToolDispatchRequest {
    request_in(tool, lifecycle, CancelScope::root())
}

fn request_in(
    tool: Arc<dyn Tool>,
    lifecycle: LifecycleHooks,
    cancel: CancelScope,
) -> ToolDispatchRequest {
    ToolDispatchRequest::new(
        tool,
        CallId::new("call-1"),
        json!({"path": "src/lib.rs"}),
        run_context(),
        cancel,
        CallHistory::default(),
        PermissionEngine::default(),
    )
    .with_lifecycle_hooks(lifecycle)
}

fn deny(message: &str) -> HookDecision {
    HookDecision::Deny {
        message: message.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// the tool bracket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_that_runs_is_bracketed_by_the_two_tool_moments() {
    let hooks = Recording::new("metrics");
    let tool = CountingTool::plain("apply_patch");
    let lifecycle = installed(vec![hooks.clone()], Vec::new());

    let outcome = dispatch_tool(request(tool.clone(), lifecycle))
        .await
        .expect("the call runs");

    assert!(matches!(outcome.dispatch(), ToolDispatch::Observed(_)));
    assert_eq!(tool.calls(), 1);
    assert_eq!(
        hooks.seen(),
        vec![
            "tool_start:apply_patch".to_owned(),
            "tool_end:apply_patch/ok".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_call_that_failed_still_closes_its_bracket_and_says_which_code() {
    // A bracket that only closed on success would not be a bracket.
    let hooks = Recording::new("metrics");
    let tool = CountingTool::failing("run_tests");
    let lifecycle = installed(vec![hooks.clone()], Vec::new());

    let _settled = dispatch_tool(request(tool, lifecycle))
        .await
        .expect("a model-visible failure is not a torn-down turn");

    assert_eq!(
        hooks.seen(),
        vec![
            "tool_start:run_tests".to_owned(),
            "tool_end:run_tests/tool.execution_failed".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_refused_call_raises_neither_tool_moment() {
    // No tool was about to start, so announcing one would make a host counting invocations count
    // decisions instead.
    let log = Arc::new(Mutex::new(Vec::new()));
    let hooks = Recording::sharing("metrics", &log);
    let refusing = Deciding::new(&log, deny("not this path"));
    let tool = CountingTool::plain("apply_patch");

    let outcome = dispatch_tool(
        request(tool.clone(), installed(vec![hooks], Vec::new())).with_user_hooks(
            UserHooks::default().with_hook(UserHookRegistration::new(
                HookEventName::PreToolUse,
                refusing,
            )),
        ),
    )
    .await
    .expect("a refusal is a value, not an error");

    assert!(matches!(outcome.dispatch(), ToolDispatch::Refused(_)));
    assert_eq!(tool.calls(), 0);
    assert_eq!(
        log.lock().expect("no poisoned fixture").clone(),
        vec!["pre_tool_use:apply_patch".to_owned()]
    );
}

#[tokio::test]
async fn a_call_waiting_on_a_person_raises_neither_tool_moment() {
    let hooks = Recording::new("metrics");
    let tool = CountingTool::approval_gated("apply_patch");
    let lifecycle = installed(vec![hooks.clone()], Vec::new());

    let outcome = dispatch_tool(request(tool.clone(), lifecycle))
        .await
        .expect("an interruption is a state");

    assert!(matches!(
        outcome.dispatch(),
        ToolDispatch::AwaitingApproval(_)
    ));
    assert_eq!(tool.calls(), 0);
    assert!(hooks.seen().is_empty());
}

#[tokio::test]
async fn the_deciding_hooks_are_asked_before_the_invocation_is_announced() {
    // The bracket is the innermost thing around the call: everything that could still refuse it
    // has already had its say by the time a tool start is announced.
    let log = Arc::new(Mutex::new(Vec::new()));
    let hooks = Recording::sharing("metrics", &log);
    let watching = Deciding::new(&log, HookDecision::Continue);
    let tool = CountingTool::plain("apply_patch");

    let _settled = dispatch_tool(
        request(tool, installed(vec![hooks], Vec::new())).with_user_hooks(
            UserHooks::default().with_hook(UserHookRegistration::new(
                HookEventName::PreToolUse,
                watching,
            )),
        ),
    )
    .await
    .expect("the call runs");

    assert_eq!(
        log.lock().expect("no poisoned fixture").clone(),
        vec![
            "pre_tool_use:apply_patch".to_owned(),
            "tool_start:apply_patch".to_owned(),
            "tool_end:apply_patch/ok".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_failing_lifecycle_hook_stops_the_call_and_names_the_hook_the_moment_and_the_scope() {
    let hooks = Recording::failing_at("metrics", LifecycleEvent::ToolStart);
    let tool = CountingTool::plain("apply_patch");
    let lifecycle = installed(vec![hooks], Vec::new());

    let error = dispatch_tool(request(tool.clone(), lifecycle))
        .await
        .expect_err("the host asked for this code to run");

    assert_eq!(tool.calls(), 0);
    let text = error.to_string();
    assert!(text.contains("metrics"), "{text}");
    assert!(text.contains("tool_start"), "{text}");
    assert!(text.contains("run scope"), "{text}");
}

#[tokio::test]
async fn a_cancelled_invocation_is_not_announced_as_one_that_settled() {
    // Narration runs inside the caller's scope. A hook told about a settlement after the run was
    // stopped would keep the run alive past the stop, with nothing else still running to blame.
    let hooks = Recording::new("metrics");
    let tool = CountingTool::hanging("apply_patch");
    let entered = Arc::clone(&tool.entered);
    let cancel = CancelScope::root();
    let lifecycle = installed(vec![hooks.clone()], Vec::new());

    let dispatch = tokio::spawn(dispatch_tool(request_in(tool, lifecycle, cancel.clone())));
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the tool starts");
    cancel.cancel(CancelReason::UserInterrupt);

    let error = dispatch
        .await
        .expect("the task finishes")
        .expect_err("a cancelled invocation stops the chain");
    assert!(error.is_cancelled(), "{error}");
    assert_eq!(hooks.seen(), vec!["tool_start:apply_patch".to_owned()]);
}

/// Blocks output settlement until the test releases it.
struct PausedOutputCheck {
    id: ToolGuardrailId,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ra_core::guardrail::ToolOutputGuardrail for PausedOutputCheck {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        _data: &ra_core::guardrail::ToolOutputGuardrailData<'_>,
    ) -> Result<ra_core::guardrail::ToolGuardrailFunctionOutput> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ra_core::guardrail::ToolGuardrailFunctionOutput::allow())
    }
}

#[tokio::test]
async fn invocation_end_precedes_a_blocked_output_guardrail() {
    let hooks = Recording::new("metrics");
    let id = ToolGuardrailId::new("output_check").expect("a guardrail identity");
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let check = Arc::new(PausedOutputCheck {
        id: id.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });
    let guardrails = ra_runtime::tool::guardrail::ToolGuardrails::install(
        Vec::new(),
        vec![check as Arc<dyn ra_core::guardrail::ToolOutputGuardrail>],
        false,
    )
    .expect("a registered output guardrail");
    let tool = CountingTool::new("apply_patch", ToolOptions::new().with_output_guardrail(id));
    let dispatch = tokio::spawn(dispatch_tool(
        request(tool, installed(vec![hooks.clone()], Vec::new())).with_tool_guardrails(guardrails),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the guardrail starts");
    let seen_while_blocked = hooks.seen();
    let was_blocked = !dispatch.is_finished();
    release.notify_one();
    let _settled = dispatch
        .await
        .expect("the task finishes")
        .expect("the output is admitted");
    assert!(was_blocked);
    assert_eq!(
        seen_while_blocked,
        vec!["tool_start:apply_patch", "tool_end:apply_patch/ok"]
    );
}

#[tokio::test]
async fn a_propagated_invocation_failure_is_announced_before_it_leaves() {
    let hooks = Recording::new("metrics");
    let mut tool = CountingTool::failing("run_tests");
    Arc::get_mut(&mut tool)
        .expect("an unshared fixture")
        .options =
        ToolOptions::new().with_failure_handling(ra_core::tool::ToolFailureHandling::Propagate);
    let error = dispatch_tool(request(tool, installed(vec![hooks.clone()], Vec::new())))
        .await
        .expect_err("the raw invocation error propagates");
    assert_eq!(error.code(), "tool.execution_failed");
    assert_eq!(
        hooks.seen(),
        vec![
            "tool_start:run_tests",
            "tool_end:run_tests/tool.execution_failed"
        ]
    );
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

struct ScriptedModel {
    responses: Mutex<std::collections::VecDeque<Result<ModelResponse>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedModel {
    fn next(&self) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses
            .lock()
            .expect("no poisoned fixture")
            .pop_front()
            .expect("a scripted response")
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next()
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        match self.next() {
            Ok(response) => {
                stream::iter(vec![Ok(ModelStreamEvent::Completed(Box::new(response)))]).boxed()
            }
            Err(error) => stream::iter(vec![Err(error)]).boxed(),
        }
    }
}

struct FixedResolver {
    model: Arc<dyn Model>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn answering(text: &str) -> Result<ModelResponse> {
    // The ID is derived from the text so two answers in one run stay distinguishable: history
    // reconciliation refuses the same record appearing in two turns.
    Ok(ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("answer-{}", text.replace(' ', "-"))),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
    .with_usage(Usage::from_request(RequestUsage::new(20, 6))))
}

fn calling(tool: &str, call_id: &str) -> Result<ModelResponse> {
    Ok(ModelResponse::new(vec![RunItem::new(
        ItemId::new(call_id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), tool, json!({}))),
    )])
    .with_usage(Usage::from_request(RequestUsage::new(20, 6))))
}

struct Run {
    request: RunRequest,
    model_calls: Arc<AtomicUsize>,
}

fn run(
    agent: Arc<AgentSpec>,
    responses: Vec<Result<ModelResponse>>,
    config: RunConfig,
    input: Vec<ModelInputItem>,
) -> Run {
    run_in(agent, responses, config, input, CancelScope::root())
}

fn run_in(
    agent: Arc<AgentSpec>,
    responses: Vec<Result<ModelResponse>>,
    config: RunConfig,
    input: Vec<ModelInputItem>,
    cancel: CancelScope,
) -> Run {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ScriptedModel {
        responses: Mutex::new(responses.into()),
        calls: Arc::clone(&model_calls),
    });
    Run {
        request: RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(FixedResolver { model }),
            RunId::new("run-lifecycle"),
            cancel,
            input,
        )
        .with_config(config),
        model_calls,
    }
}

fn agent_with(tools: Vec<Arc<dyn Tool>>, hooks: Vec<Arc<dyn LifecycleHook>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .instructions("do the thing")
        .tools(tools)
        .lifecycle_hooks(hooks)
        .build()
        .expect("an agent")
}

fn prompt() -> Vec<ModelInputItem> {
    vec![ModelInputItem::Message(Message::user("改一下文件"))]
}

#[tokio::test]
async fn a_completed_run_is_bracketed_and_the_end_carries_what_it_delivered() {
    let hooks = Recording::new("metrics");
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let started = run(
        agent_with(Vec::new(), Vec::new()),
        vec![answering("all done")],
        config,
        prompt(),
    );

    let result = Runner::run(started.request)
        .await
        .expect("the run completes");

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(started.model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        hooks.seen(),
        vec![
            "agent_start:lifecycle-agent/1/fresh".to_owned(),
            "llm_start:canonical-model/1".to_owned(),
            "llm_end:1/1".to_owned(),
            format!("agent_end:{}/all done", FinishReason::Final.code()),
        ]
    );
}

#[tokio::test]
async fn a_run_waiting_on_a_person_never_reports_an_agent_that_ended() {
    // The reference implementation raises the end only when a final output exists. A run handed
    // back with a decision outstanding has not produced one.
    let hooks = Recording::new("metrics");
    let tool = CountingTool::approval_gated("apply_patch");
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let started = run(
        agent_with(vec![tool as Arc<dyn Tool>], Vec::new()),
        vec![calling("apply_patch", "call-1")],
        config,
        prompt(),
    );

    let result = Runner::run(started.request)
        .await
        .expect("an interruption is a state");

    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
    let seen = hooks.seen();
    assert_eq!(
        seen.first().map(String::as_str),
        Some("agent_start:lifecycle-agent/1/fresh")
    );
    assert!(
        !seen.iter().any(|entry| entry.starts_with("agent_end")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_resumed_segment_says_it_is_a_continuation() {
    // The hook object is new in this process, so the activation is raised again — and a host that
    // sets something up there has to be able to tell the two apart.
    let hooks = Recording::new("metrics");
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let mut carried = RunState::start(RunId::new("run-lifecycle"));
    carried
        .begin_segment(AgentId::new("lifecycle-agent"), Vec::new())
        .expect("the checkpoint represents a started run");

    let started = run(
        agent_with(Vec::new(), Vec::new()),
        vec![answering("done")],
        config,
        Vec::new(),
    );
    Runner::run(started.request.with_state(carried))
        .await
        .expect("the run completes");

    assert_eq!(
        hooks.seen().first().map(String::as_str),
        Some("agent_start:lifecycle-agent/0/resumed")
    );
}

#[tokio::test]
async fn an_agent_scoped_hook_hears_the_same_moments_as_a_run_scoped_one() {
    // The reference implementation's two classes declare the same set. What differs is only where
    // the hook was installed, and that reaches the callback as the scope.
    let hooks = Recording::new("metrics");
    let started = run(
        agent_with(Vec::new(), vec![hooks.clone()]),
        vec![answering("all done")],
        RunConfig::new(),
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect("the run completes");

    assert_eq!(
        hooks.seen(),
        vec![
            "agent_start:lifecycle-agent/1/fresh".to_owned(),
            "llm_start:canonical-model/1".to_owned(),
            "llm_end:1/1".to_owned(),
            format!("agent_end:{}/all done", FinishReason::Final.code()),
        ]
    );
}

#[tokio::test]
async fn one_object_installed_at_both_scopes_is_told_once_for_each_and_can_tell_them_apart() {
    // Legitimate — whole-run totals and the per-agent split at once — which is why the scope is an
    // argument rather than something folded into the name.
    let hooks = Recording::scoped("metrics");
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let started = run(
        agent_with(Vec::new(), vec![hooks.clone()]),
        vec![answering("all done")],
        config,
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect("the run completes");

    let seen = hooks.seen();
    assert_eq!(
        seen.iter()
            .filter(|entry| entry.starts_with("llm_start@run:"))
            .count(),
        1
    );
    assert_eq!(
        seen.iter()
            .filter(|entry| entry.starts_with("llm_start@agent:"))
            .count(),
        1
    );
    // The run scope is always told first, so which of two failures a run reports is stable.
    assert!(
        seen[0].ends_with("@run:lifecycle-agent/1/fresh"),
        "{seen:?}"
    );
    assert!(
        seen[1].ends_with("@agent:lifecycle-agent/1/fresh"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn the_model_call_is_announced_once_per_turn_and_its_end_reads_the_spend_it_caused() {
    let hooks = Recording::new("metrics");
    let tool = CountingTool::plain("apply_patch");
    let config = RunConfig::new()
        .with_lifecycle_hook(hooks.clone())
        .with_permission_mode(PermissionMode::BypassPermissions);
    let started = run(
        agent_with(vec![tool.clone() as Arc<dyn Tool>], Vec::new()),
        vec![calling("apply_patch", "call-1"), answering("done")],
        config,
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect("the run completes");

    assert_eq!(tool.calls(), 1);
    let calls: Vec<String> = hooks
        .seen()
        .into_iter()
        .filter(|entry| entry.starts_with("llm_"))
        .collect();
    assert_eq!(
        calls,
        vec![
            "llm_start:canonical-model/1".to_owned(),
            // One request paid for by the time the first call is announced as finished.
            "llm_end:1/1".to_owned(),
            "llm_start:canonical-model/3".to_owned(),
            "llm_end:1/2".to_owned(),
        ]
    );
}

#[tokio::test]
async fn the_tool_bracket_reaches_a_call_the_loop_made_and_closes_before_the_next_model_call() {
    let hooks = Recording::new("metrics");
    let tool = CountingTool::plain("apply_patch");
    let config = RunConfig::new()
        .with_lifecycle_hook(hooks.clone())
        .with_permission_mode(PermissionMode::BypassPermissions);
    let started = run(
        agent_with(vec![tool as Arc<dyn Tool>], Vec::new()),
        vec![calling("apply_patch", "call-1"), answering("done")],
        config,
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect("the run completes");

    let seen = hooks.seen();
    let position = |entry: &str| {
        seen.iter()
            .position(|seen| seen == entry)
            .unwrap_or_else(|| panic!("`{entry}` is announced: {seen:?}"))
    };
    assert!(position("tool_start:apply_patch") < position("tool_end:apply_patch/ok"));
    assert!(
        position("tool_end:apply_patch/ok") < position("llm_start:canonical-model/3"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_failing_ending_turns_a_finished_run_into_a_failed_one() {
    // The sharp edge, stated rather than smoothed over: a callback that only wants to record
    // something swallows its own errors, and that stays a decision local to the callback.
    let hooks = Recording::failing_at("metrics", LifecycleEvent::AgentEnd);
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let started = run(
        agent_with(Vec::new(), Vec::new()),
        vec![answering("all done")],
        config,
        prompt(),
    );

    let error = Runner::run(started.request)
        .await
        .expect_err("a run whose narration is down is not a run that finished");

    let text = error.to_string();
    assert!(text.contains("metrics"), "{text}");
    assert!(text.contains("agent_end"), "{text}");
}

#[tokio::test]
async fn a_run_that_failed_is_never_announced_as_one_that_ended() {
    // It did not end, it broke. Drawing this line differently here than for the twelve-event hooks
    // would make "how many runs finished" depend on which family was asked.
    let hooks = Recording::new("metrics");
    let config = RunConfig::new().with_lifecycle_hook(hooks.clone());
    let started = run(
        agent_with(Vec::new(), Vec::new()),
        vec![Err(Error::caller("the provider refused the request"))],
        config,
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect_err("a failed model call ends the run as a failure");

    let seen = hooks.seen();
    assert_eq!(
        seen.first().map(String::as_str),
        Some("agent_start:lifecycle-agent/1/fresh")
    );
    assert!(
        !seen.iter().any(|entry| entry.starts_with("agent_end")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|entry| entry.starts_with("llm_end")),
        "a call that produced nothing did not end either: {seen:?}"
    );
}

#[tokio::test]
async fn a_cancelled_run_is_never_announced_as_one_that_ended() {
    let hooks = Recording::new("metrics");
    let tool = CountingTool::hanging("apply_patch");
    let entered = Arc::clone(&tool.entered);
    let cancel = CancelScope::root();
    let config = RunConfig::new()
        .with_lifecycle_hook(hooks.clone())
        .with_permission_mode(PermissionMode::BypassPermissions);
    let started = run_in(
        agent_with(vec![tool as Arc<dyn Tool>], Vec::new()),
        vec![calling("apply_patch", "call-1"), answering("done")],
        config,
        prompt(),
        cancel.clone(),
    );

    let running = tokio::spawn(Runner::run(started.request));
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the tool starts");
    cancel.cancel(CancelReason::UserInterrupt);

    let error = running
        .await
        .expect("the task finishes")
        .expect_err("a cancelled run has no result to report");
    assert!(error.is_cancelled(), "{error}");

    let seen = hooks.seen();
    assert!(
        seen.contains(&"tool_start:apply_patch".to_owned()),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|entry| entry.starts_with("tool_end")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|entry| entry.starts_with("agent_end")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn an_ending_the_stop_hook_takes_back_is_never_announced_as_one() {
    // The two families are separate on purpose: a deciding hook settles whether the run is really
    // over, and narration describes what actually happened. An observer told about the first
    // candidate delivery would report a run that ended twice.
    let hooks = Recording::new("metrics");
    let config = RunConfig::new()
        .with_lifecycle_hook(hooks.clone())
        .with_user_hook(UserHookRegistration::new(
            HookEventName::Stop,
            BlockingOnce::new("check the tests too"),
        ));
    let started = run(
        agent_with(Vec::new(), Vec::new()),
        vec![answering("first answer"), answering("second answer")],
        config,
        prompt(),
    );

    Runner::run(started.request)
        .await
        .expect("the run completes");

    assert_eq!(started.model_calls.load(Ordering::SeqCst), 2);
    let endings: Vec<String> = hooks
        .seen()
        .into_iter()
        .filter(|entry| entry.starts_with("agent_end"))
        .collect();
    assert_eq!(
        endings,
        vec![format!(
            "agent_end:{}/second answer",
            FinishReason::Final.code()
        )]
    );
}

// ---------------------------------------------------------------------------
// what a transfer does to the set
// ---------------------------------------------------------------------------

#[test]
fn a_transfer_replaces_the_agent_half_and_keeps_the_run_half() {
    // Settlement still refuses handoffs, so the loop cannot reach the moment end to end. What the
    // handoff arm does to the set is reachable, and it is the half that would silently misattribute
    // the arriving agent's work to the departing agent's hooks.
    let run_scoped: Arc<dyn LifecycleHook> = Recording::new("whole-run");
    let departing: Arc<dyn LifecycleHook> = Recording::new("departing");
    let arriving: Arc<dyn LifecycleHook> = Recording::new("arriving");

    let before = LifecycleHooks::installed(&[run_scoped], &[departing]);
    let after = before.rebound(&[arriving]);

    assert_eq!(
        format!("{after:?}"),
        "LifecycleHooks { run: [\"whole-run\"], agent: [\"arriving\"] }"
    );
    // A value rather than a mutation, so a call already holding the set cannot have it change
    // under it mid-turn.
    assert_eq!(
        format!("{before:?}"),
        "LifecycleHooks { run: [\"whole-run\"], agent: [\"departing\"] }"
    );

    assert!(LifecycleHooks::new().is_empty());
    assert!(!after.is_empty());
    // A run that installed nothing still narrates to whatever the arriving agent brought.
    assert!(
        !LifecycleHooks::new()
            .rebound(&[Recording::new("late") as Arc<dyn LifecycleHook>])
            .is_empty()
    );
}

/// Narration that writes down which installation heard a transfer, and checks both its sides.
struct TransferRecorder(&'static str, Arc<Mutex<Vec<String>>>);

#[async_trait]
impl LifecycleHook for TransferRecorder {
    fn name(&self) -> &str {
        self.0
    }

    async fn on_handoff(&self, scope: LifecycleScope, input: &HandoffInput<'_>) -> Result<()> {
        // The input still names the source, because the transfer has not taken effect yet.
        assert_eq!(input.from().id(), &AgentId::new("source"));
        assert_eq!(input.to().id(), &AgentId::new("target"));
        self.1
            .lock()
            .expect("no poisoned fixture")
            .push(format!("{}:{scope}", self.0));
        Ok(())
    }
}

#[tokio::test]
async fn a_transfer_is_announced_to_the_run_and_to_the_receiving_agent() {
    // The agent-scoped half of the moment belongs to the arriving agent, matching
    // `AgentHooksBase.on_handoff`, so the set is rebound before the moment is raised. The departing
    // agent's own record of losing control is the handoff item in the run's history.
    //
    // Driven through the dispatch entry rather than the loop: settlement refuses handoffs, so this
    // is the only reachable call site until control transfer lands.
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = |name| Arc::new(TransferRecorder(name, Arc::clone(&log))) as Arc<dyn LifecycleHook>;
    let source = AgentSpec::builder()
        .id(AgentId::new("source"))
        .name("Source")
        .lifecycle_hook(hook("source"))
        .build()
        .expect("an agent that narrates");
    let target = AgentSpec::builder()
        .id(AgentId::new("target"))
        .name("Target")
        .lifecycle_hook(hook("target"))
        .build()
        .expect("an agent that narrates");

    let installed = LifecycleHooks::installed(&[hook("whole-run")], source.lifecycle_hooks());
    let receiving = installed.rebound(target.lifecycle_hooks());
    let run = RunContext::new(RunId::new("run-lifecycle"), &source);
    ra_runtime::lifecycle::dispatch::handoff(
        &receiving,
        &HandoffInput::new(&run, &target),
        &CancelScope::root(),
    )
    .await
    .expect("an observer changes nothing");

    assert_eq!(
        log.lock().expect("no poisoned fixture").clone(),
        vec!["whole-run:run".to_owned(), "target:agent".to_owned()],
        "the departing agent's hooks are gone by the time the moment is raised"
    );
}

#[tokio::test]
async fn resumable_budget_stops_do_not_end_the_agent() {
    for budget in [
        ra_core::budget::BudgetLimit::new().with_max_turns(1),
        ra_core::budget::BudgetLimit::new()
            .with_max_turns(10)
            .with_max_tokens(1),
    ] {
        let hooks = Recording::new("metrics");
        let started = run(
            agent_with(vec![CountingTool::plain("read_file")], Vec::new()),
            vec![calling("read_file", "call-1")],
            RunConfig::new()
                .with_budget(budget)
                .with_lifecycle_hook(hooks.clone()),
            prompt(),
        );
        Runner::run(started.request)
            .await
            .expect("a soft budget stop");
        assert_eq!(started.model_calls.load(Ordering::SeqCst), 1);
        assert!(
            !hooks
                .seen()
                .iter()
                .any(|entry| entry.starts_with("agent_end"))
        );
    }
}

struct FinalToolOutput;

#[async_trait]
impl LifecycleHook for FinalToolOutput {
    fn name(&self) -> &str {
        "final-tool-output"
    }

    async fn on_agent_end(&self, _scope: LifecycleScope, input: &AgentEndInput<'_>) -> Result<()> {
        assert_eq!(input.finish_reason(), FinishReason::ToolStop);
        assert_eq!(input.tool_outputs().len(), 1);
        assert_eq!(
            input.tool_outputs()[0].output(),
            &serde_json::to_value(ToolOutput::text("the tool ran")).unwrap()
        );
        Ok(())
    }
}

#[tokio::test]
async fn a_tool_produced_answer_reaches_agent_end() {
    let hooks = Recording::new("metrics");
    let agent = agent_with(vec![CountingTool::plain("read_file")], Vec::new())
        .to_builder()
        .tool_use_behavior(ra_core::agent::ToolUseBehavior::StopOnFirstTool)
        .build()
        .expect("agent");
    let started = run(
        agent,
        vec![calling("read_file", "call-1")],
        RunConfig::new()
            .with_lifecycle_hook(Arc::new(FinalToolOutput))
            .with_lifecycle_hook(hooks.clone()),
        prompt(),
    );
    Runner::run(started.request).await.expect("tool answer");
    assert_eq!(
        hooks
            .seen()
            .iter()
            .filter(|entry| entry.starts_with("agent_end"))
            .count(),
        1
    );
}
