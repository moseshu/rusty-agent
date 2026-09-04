//! R3-7 contracts for the agent loop and its two entry points.

use std::{
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use insta::assert_json_snapshot;
use ra_context::{
    compaction::{CompactionCapability, anchor::AnchorRetention},
    window::ContextWindowConfig,
};
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior, ToolUseBehaviorHandler, ToolUseResult},
    budget::{BudgetLimit, BudgetSnapshot},
    cancel::{CancelReason, CancelScope, Deadline},
    context::RunContext,
    error::{Error, ProviderErrorKind, Result, ToolErrorKind},
    filter::{ContextFilter, ContextFilterRequest, ModelInputData},
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelRetryAdviceRequest,
        ModelRetrySettings, ModelSelector, ModelSettings, ModelStream, ModelStreamEvent,
        NetworkErrorRetryPolicy, NormalizedProviderError, ProviderKey, RawResponseEvent,
        ReplaySafety, ResolvedModel, RetryAdvice, RetryBackoffSettings, ToolChoice,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{PendingControlRequest, RunId, RunState, ToolOutcome, ToolUse, WorkStateHandle},
    tool::{
        Tool, ToolApprovalPolicy, ToolAvailability, ToolCaller, ToolContext, ToolLookupKey,
        ToolNamespace, ToolOptions, ToolOrigin, ToolOutput, ToolSchema, ToolServices,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{
        ContinuationInput, RunConfig, RunErrorHandler, RunErrorHandlerInput, RunErrorHandlerResult,
        RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner, TurnRecord,
    },
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, oneshot},
    time::timeout,
};

/// Replays a fixed script of responses, one per turn, and records the input it was handed.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    inputs: Mutex<Vec<usize>>,
    input_items: Mutex<Vec<Vec<ModelInputItem>>>,
    instructions: Mutex<Vec<Option<String>>>,
    tool_choices: Mutex<Vec<Option<ToolChoice>>>,
    request_surfaces: Mutex<Vec<(usize, usize, bool, bool)>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            inputs: Mutex::new(Vec::new()),
            input_items: Mutex::new(Vec::new()),
            instructions: Mutex::new(Vec::new()),
            tool_choices: Mutex::new(Vec::new()),
            request_surfaces: Mutex::new(Vec::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn next_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(request.input().len());
        self.input_items
            .lock()
            .unwrap()
            .push(request.input().to_vec());
        self.instructions
            .lock()
            .unwrap()
            .push(request.system_instructions().map(str::to_owned));
        self.tool_choices
            .lock()
            .unwrap()
            .push(request.model_settings().tool_choice().cloned());
        self.request_surfaces.lock().unwrap().push((
            request.tools().len(),
            request.handoffs().len(),
            request.output_schema().is_some(),
            request.continuation().is_server_managed(),
        ));
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.next_response(request)
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next_response(request)
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

/// Fails only the non-streaming summary path while ordinary model turns continue to answer.
struct SummaryFailingModel {
    streamed: Mutex<Vec<ModelResponse>>,
    summary_calls: AtomicUsize,
}

impl SummaryFailingModel {
    fn new(streamed: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            streamed: Mutex::new(streamed),
            summary_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Model for SummaryFailingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.summary_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::provider(
            ProviderErrorKind::Network,
            "summary service unavailable",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        let mut streamed = self.streamed.lock().unwrap();
        let response = streamed.remove(0);
        stream::iter(vec![Ok(ModelStreamEvent::Completed(Box::new(response)))]).boxed()
    }
}

/// Records the filter calls without changing their model input.
#[derive(Default)]
struct RecordingContextFilter {
    calls: Arc<Mutex<Vec<(u64, usize)>>>,
    observed: Arc<Mutex<Vec<Vec<ModelInputItem>>>>,
    instructions: Arc<Mutex<Vec<Option<String>>>>,
}

impl ContextFilter for RecordingContextFilter {
    fn name(&self) -> &str {
        "recording"
    }

    fn filter_model_input(
        &self,
        request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        self.calls
            .lock()
            .unwrap()
            .push((request.current_turn(), data.input().len()));
        self.observed.lock().unwrap().push(data.input().to_vec());
        self.instructions
            .lock()
            .unwrap()
            .push(data.instructions().map(str::to_owned));
        Ok(data)
    }
}

/// Drops every tool result from the request, so the chain has something real to measure.
struct ToolOutputDroppingFilter {
    name: &'static str,
}

impl ContextFilter for ToolOutputDroppingFilter {
    fn name(&self) -> &str {
        self.name
    }

    fn filter_model_input(
        &self,
        _request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        let kept: Vec<ModelInputItem> = data
            .input()
            .iter()
            .filter(|item| !matches!(item, ModelInputItem::ToolCallOutput(_)))
            .cloned()
            .collect();
        Ok(data.with_input(kept))
    }
}

/// Answers on the streaming entry point, and records whether the non-streaming one was used.
///
/// The two counters are the point of the fixture: which entry point the loop opened is the thing
/// the partial-message switch decides, and it is not observable from the run's result.
struct StreamingModel {
    events: Mutex<Vec<Vec<ModelStreamEvent>>>,
    streamed_calls: Arc<AtomicUsize>,
    blocking_calls: Arc<AtomicUsize>,
}

/// Holds the terminal response until the tool started from its completed stream item runs.
struct DispatchingStreamingModel {
    response: ModelResponse,
    tool_started: Arc<Notify>,
}

impl DispatchingStreamingModel {
    fn new(response: ModelResponse, tool_started: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            response,
            tool_started,
        })
    }
}

#[async_trait]
impl Model for DispatchingStreamingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller(
            "this fixture only answers on the streaming entry point",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        let item = self.response.output()[0].clone();
        let response = self.response.clone();
        let tool_started = Arc::clone(&self.tool_started);
        stream::unfold(0_u8, move |stage| {
            let item = item.clone();
            let response = response.clone();
            let tool_started = Arc::clone(&tool_started);
            async move {
                match stage {
                    0 => Some((
                        Ok(ModelStreamEvent::RunItem(
                            ra_core::model::RunItemStreamEvent::new("tool_call", item),
                        )),
                        1,
                    )),
                    1 => {
                        tool_started.notified().await;
                        Some((Ok(ModelStreamEvent::Completed(Box::new(response))), 2))
                    }
                    _ => None,
                }
            }
        })
        .boxed()
    }
}

impl StreamingModel {
    fn new(events: Vec<Vec<ModelStreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(events),
            streamed_calls: Arc::new(AtomicUsize::new(0)),
            blocking_calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl Model for StreamingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.blocking_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::caller(
            "this fixture must use the streaming model entry point",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        self.streamed_calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.events.lock().unwrap();
        let events = if script.is_empty() {
            Vec::new()
        } else {
            script.remove(0)
        };
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

fn raw_event(event_type: &str) -> ModelStreamEvent {
    ModelStreamEvent::RawResponse(RawResponseEvent::new(
        ProviderKey::new("test-provider"),
        event_type,
        json!({"delta": "…"}),
    ))
}

fn streaming_request(model: &Arc<StreamingModel>, cancel: &CancelScope) -> RunRequest {
    model_request(Arc::clone(model) as Arc<dyn Model>, cancel)
}

fn model_request(model: Arc<dyn Model>, cancel: &CancelScope) -> RunRequest {
    RunRequest::new(
        agent(Vec::new()),
        Arc::new(SingleModelResolver { model }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

/// Settles a turn and then fails, which is the ordering the error-reporting rule is about.
struct SettledThenFailingModel;

#[async_trait]
impl Model for SettledThenFailingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller(
            "this fixture only answers on the streaming entry point",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            Ok(ModelStreamEvent::Completed(Box::new(ModelResponse::new(
                vec![message("msg-1", "答案")],
            )))),
            Err(Error::provider(
                ProviderErrorKind::Network,
                "connection reset while draining the stream",
            )),
        ])
        .boxed()
    }
}

/// A model call that only ends by being cancelled. Its drop notification makes cancellation of an
/// in-flight runner observable without relying on timing or a provider implementation.
struct PendingModel {
    started: Mutex<Option<oneshot::Sender<()>>>,
    dropped: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl PendingModel {
    fn new() -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Receiver<()>) {
        let (started_sender, started) = oneshot::channel();
        let (dropped_sender, dropped) = oneshot::channel();
        (
            Arc::new(Self {
                started: Mutex::new(Some(started_sender)),
                dropped: Arc::new(Mutex::new(Some(dropped_sender))),
            }),
            started,
            dropped,
        )
    }
}

struct NotifyWhenDropped(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl Drop for NotifyWhenDropped {
    fn drop(&mut self) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
}

#[async_trait]
impl Model for PendingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let _notify = NotifyWhenDropped(Arc::clone(&self.dropped));
        if let Some(sender) = self.started.lock().unwrap().take() {
            let _ = sender.send(());
        }
        futures::future::pending().await
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        let dropped = Arc::clone(&self.dropped);
        let started = self.started.lock().unwrap().take();
        stream::once(async move {
            let _notify = NotifyWhenDropped(dropped);
            if let Some(sender) = started {
                let _ = sender.send(());
            }
            futures::future::pending::<Result<ModelStreamEvent>>().await
        })
        .boxed()
    }
}

struct FixedResolver {
    model: Arc<ScriptedModel>,
    selectors: Arc<Mutex<Vec<Option<String>>>>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.selectors
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct SingleModelResolver {
    model: Arc<dyn Model>,
}

impl ModelResolver for SingleModelResolver {
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

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
    work_states: Arc<Mutex<Vec<Option<String>>>>,
    /// What each call was told about the run it belongs to: its ID, its public agent, and the
    /// host's own state.
    runs: Arc<Mutex<Vec<String>>>,
}

impl ScriptedTool {
    fn new(name: &str) -> Self {
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
            options: ToolOptions::new(),
            calls: Arc::new(AtomicUsize::new(0)),
            work_states: Arc::new(Mutex::new(Vec::new())),
            runs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }

    fn namespaced(namespace: &str, name: &str) -> Self {
        let mut tool = Self::new(name);
        tool.origin = ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap();
        tool
    }
}

#[async_trait]
impl Tool for ScriptedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.work_states.lock().unwrap().push(
            context
                .services()
                .work_state()
                .and_then(|state| state.as_any().downcast_ref::<TaskState>())
                .map(|state| state.plan.to_owned()),
        );
        self.runs
            .lock()
            .unwrap()
            .push(format!("call:{}", identity(context.run())));
        Ok(ToolOutput::text("done"))
    }

    /// Only reached by a tool that declares dynamic availability, which is the point: preparation
    /// asks this before the model call and dispatch calls `call` after it, so recording the same
    /// string from both is what shows the two stages read one run.
    async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        self.runs
            .lock()
            .unwrap()
            .push(format!("enabled:{}", identity(context)));
        Ok(true)
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

/// A regular tool that makes its start observable to the streaming-model fixture.
struct NotifyingTool {
    inner: ScriptedTool,
    started: Arc<Notify>,
}

impl NotifyingTool {
    fn new(name: &str, started: Arc<Notify>) -> Self {
        Self {
            inner: ScriptedTool::new(name),
            started,
        }
    }
}

#[async_trait]
impl Tool for NotifyingTool {
    fn origin(&self) -> &ToolOrigin {
        self.inner.origin()
    }

    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }

    fn options(&self) -> ToolOptions {
        self.inner.options()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.started.notify_one();
        self.inner.call(context).await
    }
}

/// Holds a streamed dispatch open and reports whether its call future was reaped.
struct PendingDropTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

impl PendingDropTool {
    fn new(name: &str, started: Arc<Notify>, dropped: Arc<AtomicUsize>) -> Self {
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
            started,
            dropped,
        }
    }
}

#[async_trait]
impl Tool for PendingDropTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        struct ReportDrop(Arc<AtomicUsize>);

        impl Drop for ReportDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        self.started.notify_one();
        let _report = ReportDrop(Arc::clone(&self.dropped));
        std::future::pending::<()>().await;
        unreachable!("a pending test tool can only leave through cancellation")
    }
}

/// The host's own state, reached through the run's one type-erased door.
struct HostState {
    workspace: &'static str,
}

/// Everything a stage can learn about the run it is part of, as one comparable string.
fn identity(run: &RunContext) -> String {
    format!(
        "{}/{}/{}",
        run.run_id(),
        run.agent_id(),
        run.app_context::<HostState>()
            .map_or("<none>", |host| host.workspace)
    )
}

const CLOSEOUT_TEXT: &str = "The budget was reached; save this progress and continue with a new \
                             allowance.";

/// Delivers a closeout message, optionally recording it in session history.
struct BudgetCloseoutHandler {
    write_to_history: bool,
}

#[async_trait]
impl RunErrorHandler for BudgetCloseoutHandler {
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        assert!(input.error().code().starts_with("budget."));
        assert!(input.data().turns() > 0);
        // A closeout speaks about spend, so the ledger that stopped the run reaches it, and on a
        // run that was never resumed it agrees with the calls this segment made.
        let from_responses: u64 = input
            .data()
            .model_responses()
            .iter()
            .map(|response| response.usage().total_tokens())
            .sum();
        assert_eq!(input.data().usage().total_tokens(), from_responses);
        // The public declaration, which is whose place the closeout speaks in.
        assert_eq!(input.data().last_agent().name(), "Coder");
        assert_eq!(input.data().last_agent().id().as_str(), "coder");
        Ok(Some(
            RunErrorHandlerResult::new(Message::assistant(CLOSEOUT_TEXT, OutputPhase::Final))
                .with_write_to_history(self.write_to_history),
        ))
    }
}

/// Returns something that is not a final answer, which the runner has to refuse.
struct MisbehavingCloseoutHandler;

#[async_trait]
impl RunErrorHandler for MisbehavingCloseoutHandler {
    async fn handle(
        &self,
        _input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        Ok(Some(RunErrorHandlerResult::new(Message::user(
            "please carry on",
        ))))
    }
}

/// Looks at the condition, decides it is not one it speaks for, and says nothing.
struct DecliningCloseoutHandler {
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl RunErrorHandler for DecliningCloseoutHandler {
    async fn handle(
        &self,
        _input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

/// A handler that makes entry observable and then waits for cancellation.
struct PendingCloseoutHandler {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl RunErrorHandler for PendingCloseoutHandler {
    async fn handle(
        &self,
        _input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        if let Some(sender) = self.started.lock().unwrap().take() {
            let _ = sender.send(());
        }
        futures::future::pending().await
    }
}

/// A tool that outlives any deadline a test would set, unless something cancels it.
struct SlowTool {
    inner: Arc<ScriptedTool>,
}

#[async_trait]
impl Tool for SlowTool {
    fn origin(&self) -> &ToolOrigin {
        self.inner.origin()
    }

    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }

    fn options(&self) -> ToolOptions {
        self.inner.options()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        self.inner.call(context).await
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.inner.needs_approval(context).await
    }
}

/// Stand-in for the cross-run task state R17-1 will supply (R3-13).
struct TaskState {
    plan: &'static str,
}

impl WorkStateHandle for TaskState {
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    message_with_phase(id, text, OutputPhase::Final)
}

fn message_with_phase(id: &str, text: &str, phase: OutputPhase) -> RunItem {
    item(id, RunItemKind::Message(Message::assistant(text, phase)))
}

/// The trailing user message one model call was handed, which is where a reminder belongs.
///
/// The role is part of the assertion: input history is lowered per provider, and a system message
/// in it is rejected outright by Anthropic, which takes system text only in its own top-level
/// field.
fn reminder_text(input: &[ModelInputItem]) -> String {
    match input.last() {
        Some(ModelInputItem::Message(message)) if message.role() == MessageRole::User => {
            message.text_content()
        }
        other => panic!("expected a trailing user reminder, found {other:?}"),
    }
}

/// The loop's own shape: what each settled turn decided, and the records it produced.
///
/// Read from the run rather than reconstructed from it. Deriving "the loop ran again" from there
/// being a later turn would assert the arithmetic instead of the decision, and a handoff — which
/// also continues the loop — would come out indistinguishable from a `RunAgain`.
fn loop_skeleton(result: &RunResult) -> Value {
    let turns = result
        .turn_records()
        .iter()
        .map(|record| {
            let items = result
                .turn_items(record)
                .iter()
                .map(|item| {
                    json!({
                        "id": item.id().as_str(),
                        "kind": item.kind().label(),
                        "phase": message_phase(item),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "turn": record.turn(),
                "agent": record.agent().as_str(),
                "next_step": record.next_step_code(),
                "finish_reason": record.finish_reason().map(FinishReason::code),
                "items": items,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "turn_count": result.turns(),
        "outcome": outcome_code(result.outcome()),
        "turns": turns,
    })
}

/// The run-level ending, as a stable code.
fn outcome_code(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Completed { reason } => format!("completed:{}", reason.code()),
        RunOutcome::Interrupted { items } => format!("interrupted:{}", items.len()),
        other => panic!("unhandled run outcome: {other:?}"),
    }
}

/// The channel a record was stamped with, and `None` for records that are not messages.
fn message_phase(item: &RunItem) -> Option<&'static str> {
    match item.kind() {
        RunItemKind::Message(message) => message.phase().map(OutputPhase::label),
        _ => None,
    }
}

/// The run's message channels in generation order (R3-10).
fn phases(result: &RunResult) -> Vec<OutputPhase> {
    result
        .new_items()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::Message(message) => message.phase(),
            _ => None,
        })
        .collect()
}

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    tool_call_with_arguments(id, call_id, name, json!({ "path": "a.txt" }))
}

fn tool_call_with_arguments(id: &str, call_id: &str, name: &str, arguments: Value) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )
}

/// One turn narrated as a completed tool call, then settled by `terminal`.
///
/// The two are separate arguments because the interesting cases are the ones where a provider's
/// terminal response disagrees with what its own stream already announced.
fn narrated_tool_call_turn(narrated: RunItem, terminal: ModelResponse) -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::RunItem(ra_core::model::RunItemStreamEvent::new(
            "tool_call",
            narrated,
        )),
        ModelStreamEvent::Completed(Box::new(terminal)),
    ]
}

fn agent(tools: Vec<Arc<dyn Tool>>) -> AgentBinding {
    agent_with_tool_use_behavior(tools, ToolUseBehavior::RunLlmAgain)
}

fn agent_with_tool_use_behavior(
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
) -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(tools)
            .tool_use_behavior(tool_use_behavior)
            .build()
            .unwrap(),
    )
}

type Selectors = Arc<Mutex<Vec<Option<String>>>>;

fn request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    request_recording(tools, model, cancel, Arc::new(Mutex::new(Vec::new())))
}

/// A request that continues a checkpoint whose first segment already ran.
///
/// It carries no input of its own, selecting automatic projection from the checkpoint's history.
fn resume_request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
    state: RunState,
) -> RunRequest {
    RunRequest::new(
        agent(tools),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors: Arc::new(Mutex::new(Vec::new())),
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        Vec::new(),
    )
    .with_state(state)
}

fn request_with_tool_use_behavior(
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    RunRequest::new(
        agent_with_tool_use_behavior(tools, tool_use_behavior),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors: Arc::new(Mutex::new(Vec::new())),
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

fn request_recording(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
    selectors: Selectors,
) -> RunRequest {
    RunRequest::new(
        agent(tools),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

fn pending_request(model: Arc<PendingModel>, cancel: &CancelScope) -> RunRequest {
    RunRequest::new(
        agent(Vec::new()),
        Arc::new(SingleModelResolver { model }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

#[tokio::test]
async fn loops_between_tool_calls_and_final_answer_until_model_requests_nothing() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let tool_calls = Arc::clone(&tool.calls);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::from_request(RequestUsage::new(10, 4))),
        ModelResponse::new(vec![message("msg-1", "改完了")])
            .with_usage(Usage::from_request(RequestUsage::new(20, 6))),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(result.turns(), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "改完了"
    );

    // The second request must include the first turn's calls and outputs. A missing paired output
    // makes the provider reject the request.
    let inputs = model.inputs.lock().unwrap().clone();
    assert_eq!(inputs, [1, 3]);

    // Usage is projected by summing each call, not stored as an incrementally accumulated field.
    assert_eq!(result.usage().input_tokens(), 30);
    assert_eq!(result.usage().output_tokens(), 10);
    assert_eq!(result.model_responses().len(), 2);

    // Each call keeps its own line in the ledger. The summed 30 cannot say whether the run made
    // one large request or two, which is the difference a cost report and a cache diagnosis both
    // turn on, and the run's own ledger agrees with the projection call for call.
    assert_eq!(result.usage().requests(), 2);
    let per_request: Vec<(u64, u64)> = result
        .usage()
        .request_usage_entries()
        .iter()
        .map(|entry| (entry.input_tokens(), entry.output_tokens()))
        .collect();
    assert_eq!(per_request, vec![(10, 4), (20, 6)]);
    assert_eq!(result.state().usage_totals(), &result.usage());
    assert_eq!(result.state().tokens_used(), 40);
}

#[tokio::test]
async fn a_context_filter_runs_before_each_request_and_records_settled_outputs() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let instructions = Arc::new(Mutex::new(Vec::new()));
    let config = RunConfig::new().with_context_filter(Arc::new(RecordingContextFilter {
        calls: Arc::clone(&calls),
        instructions: Arc::clone(&instructions),
        ..RecordingContextFilter::default()
    }));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("run succeeds");

    assert_eq!(*calls.lock().unwrap(), vec![(1, 1), (2, 3)]);
    assert_eq!(
        result
            .state()
            .tool_output_references()
            .last_referenced_turn(&CallId::new("call-1")),
        Some(1)
    );

    // The filter is handed the request's stable instructions even though it may not change them:
    // a policy that budgets a whole request has to be able to price the prefix it cannot touch.
    assert_eq!(
        *instructions.lock().unwrap(),
        vec![
            Some("do the thing".to_owned()),
            Some("do the thing".to_owned())
        ]
    );

    // A filter that changed nothing still leaves a record. "Installed and had nothing to do" and
    // "never installed" are different facts, and only the report distinguishes them.
    let reports: Vec<&str> = result
        .turn_records()
        .iter()
        .flat_map(|record| record.context_filter_reports())
        .map(|report| {
            assert!(!report.changed());
            assert_eq!(report.chars_saved(), 0);
            assert_eq!(report.token_estimate_delta(), 0);
            report.filter()
        })
        .collect();
    assert_eq!(reports, vec!["recording", "recording"]);
}

/// A report is measured across one filter's own step, so a chain says which filter saved what
/// rather than only what the request weighed at the end.
#[tokio::test]
async fn each_filter_is_measured_across_its_own_step_and_reported_in_chain_order() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let config = RunConfig::new()
        .with_context_filter(Arc::new(ToolOutputDroppingFilter { name: "dropper" }))
        .with_context_filter(Arc::new(RecordingContextFilter {
            observed: Arc::clone(&observed),
            ..RecordingContextFilter::default()
        }));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("run succeeds");

    // Second turn: the request carries the call, its result, and the first turn's input. The
    // dropper removes the result; the recorder that follows sees what the dropper produced.
    let second_turn = result.turn_records()[1].context_filter_reports();
    let names: Vec<&str> = second_turn.iter().map(|report| report.filter()).collect();
    assert_eq!(names, vec!["dropper", "recording"]);
    assert!(second_turn[0].changed());
    assert!(second_turn[0].chars_saved() > 0);
    assert!(second_turn[0].token_estimate_delta() > 0);

    // The second filter changed nothing, so its own report is zero even though the request it
    // handled had already lost characters. A chain-wide total could not say this.
    assert!(!second_turn[1].changed());
    assert_eq!(second_turn[1].chars_saved(), 0);

    let observed = observed.lock().unwrap().clone();
    assert!(
        !observed[1]
            .iter()
            .any(|item| matches!(item, ModelInputItem::ToolCallOutput(_))),
        "the second filter must observe the first filter's output, not the original request"
    );

    // The first turn had no tool result to drop, so the same filter reports no change there. The
    // per-turn record is what makes "did not fire this turn" readable.
    assert!(!result.turn_records()[0].context_filter_reports()[0].changed());
}

#[tokio::test]
async fn compaction_capability_summarizes_and_replaces_generated_history_before_the_next_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ])
        .with_usage(Usage::from_request(RequestUsage::new(10, 4))),
        ModelResponse::new(vec![message(
            "summary-1",
            r#"{
                "primary_request_and_intent":"Modify the requested files.",
                "key_technical_concepts":"Keep the existing tool contract.",
                "files_and_code_sections":"No file names were retained.",
                "errors_and_fixes":"None.",
                "problem_solving":"Two tool calls completed.",
                "pending_tasks":"Produce the final response.",
                "current_work":"The tool outputs were compacted.",
                "optional_next_step":"Review the results."
            }"#,
        )])
        .with_usage(Usage::from_request(RequestUsage::new(3, 2))),
        ModelResponse::new(vec![message("msg-1", "done")])
            .with_usage(Usage::from_request(RequestUsage::new(20, 6))),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(3),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new().with_context_processor(Arc::new(compaction));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the compacted run succeeds");

    assert_eq!(model.inputs.lock().unwrap().as_slice(), &[1, 6, 2]);
    assert_eq!(
        model.request_surfaces.lock().unwrap().as_slice(),
        &[(1, 0, false, false), (0, 0, true, false), (1, 0, false, false)],
        "the compaction request must not advertise the ordinary tool surface or inherit a server continuation"
    );
    assert!(result.new_items().iter().any(|item| matches!(
        item.kind(),
        RunItemKind::Compaction(_)
    )));
    assert_eq!(result.model_responses().len(), 3);
    assert_eq!(result.usage().requests(), 3);
    assert_eq!(result.usage().input_tokens(), 33);
    assert_eq!(result.usage().output_tokens(), 12);
    assert_eq!(result.state().usage_totals(), &result.usage());
}

/// A summary stands for the records it replaced, but those records stay in authoritative history
/// forever — compaction is a projection and never deletes them. Measuring the stored history
/// rather than the model-visible view therefore stays over the limit permanently, and the run pays
/// for a fresh summary on every later turn while the history it measures only grows.
#[tokio::test]
async fn a_compacted_history_is_not_summarized_again_on_the_following_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        // Eight history records, comfortably over the limit.
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
            tool_call("c-3", "call-3", "write_file"),
            tool_call("c-4", "call-4", "write_file"),
        ]),
        ModelResponse::new(vec![message("summary-1", COMPACTION_SUMMARY_JSON)]),
        // The turn right after the compaction asks for one more tool call, so the run has to
        // process context a third time with the summary already in history. The compacted view
        // plus that call stays under the limit, so nothing should need summarizing again.
        ModelResponse::new(vec![tool_call("c-5", "call-5", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(6),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new().with_context_processor(Arc::new(compaction));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the compacted run succeeds");

    let summaries = result
        .new_items()
        .iter()
        .filter(|item| matches!(item.kind(), RunItemKind::Compaction(_)))
        .count();
    assert_eq!(
        summaries, 1,
        "one summary must cover the history it replaced instead of one being written per turn"
    );
    assert_eq!(
        model.request_surfaces.lock().unwrap().as_slice(),
        &[
            (1, 0, false, false),
            (0, 0, true, false),
            (1, 0, false, false),
            (1, 0, false, false),
        ],
        "exactly one structured-output call may appear: a second is a second summary request"
    );
    assert_eq!(result.usage().requests(), 4);
}

#[tokio::test]
async fn a_later_compaction_summarizes_the_current_projected_view() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
            tool_call("c-3", "call-3", "write_file"),
            tool_call("c-4", "call-4", "write_file"),
        ]),
        ModelResponse::new(vec![message("summary-1", COMPACTION_SUMMARY_JSON)]),
        ModelResponse::new(vec![
            tool_call("c-5", "call-5", "write_file"),
            tool_call("c-6", "call-6", "write_file"),
        ]),
        ModelResponse::new(vec![message("summary-2", COMPACTION_SUMMARY_JSON)]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(4),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new().with_context_processor(Arc::new(compaction));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the run performs both compactions");

    assert_eq!(
        model.inputs.lock().unwrap().as_slice(),
        &[1, 10, 2, 7, 2],
        "the second summary receives the first compacted view, not its original eight records"
    );
    assert_eq!(
        result
            .new_items()
            .iter()
            .filter(|item| matches!(item.kind(), RunItemKind::Compaction(_)))
            .count(),
        2
    );
}

/// A window that cannot be resolved must leave the request alone rather than invent a capacity.
#[tokio::test]
async fn compaction_leaves_the_request_untouched_when_no_window_resolves() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ])
        .with_usage(Usage::from_request(RequestUsage::new(10, 4))),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    // Model-window driven only: the scripted run resolves no model name, so no limit resolves.
    let config = RunConfig::new().with_context_processor(Arc::new(CompactionCapability::default()));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the run succeeds without compaction");

    assert_eq!(
        model.inputs.lock().unwrap().as_slice(),
        &[1, 5],
        "an inert capability must not reshape the request"
    );
    assert!(
        !result
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Compaction(_)))
    );
    assert_eq!(result.usage().requests(), 2);
}

/// Compaction runs because the context is already large. Failing the run at that exact moment
/// throws away the most work it could possibly throw away, so an unreadable summary has to leave
/// the turn on its uncompacted input and let the provider judge whether it still fits.
#[tokio::test]
async fn an_unreadable_summary_leaves_the_run_going_on_uncompacted_input() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ])
        .with_usage(Usage::from_request(RequestUsage::new(10, 4))),
        // Prose where a JSON object was required, which is what a provider that ignores the
        // structured-output contract returns.
        ModelResponse::new(vec![message(
            "summary-1",
            "Sure! Here is a summary of the conversation so far.",
        )])
        .with_usage(Usage::from_request(RequestUsage::new(3, 2))),
        ModelResponse::new(vec![message("msg-1", "done")])
            .with_usage(Usage::from_request(RequestUsage::new(20, 6))),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(3),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new().with_context_processor(Arc::new(compaction));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("an unreadable summary must not end the run");

    assert!(
        !result
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Compaction(_))),
        "a summary that could not be read must not become an authoritative record"
    );
    assert_eq!(
        model.inputs.lock().unwrap().as_slice(),
        &[1, 6, 5],
        "the ordinary turn keeps the full history it would have sent without compaction"
    );
    assert_eq!(result.usage().requests(), 3);
    assert_eq!(result.usage().input_tokens(), 33);
    assert_eq!(result.usage().output_tokens(), 12);
}

#[tokio::test]
async fn an_unavailable_summary_leaves_the_run_going_on_uncompacted_input() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = SummaryFailingModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(3),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new().with_context_processor(Arc::new(compaction));
    let request = RunRequest::new(
        agent(vec![tool]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    );

    let result = Runner::run(request.with_config(config))
        .await
        .expect("an unavailable summary must not end the run");

    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.turns(), 2);
    assert!(
        !result
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Compaction(_)))
    );
}

/// Compaction reprojects whole regions of history; the filter chain trims what is left. Running a
/// filter first would only trim items that compaction then replaced with their untouched
/// originals, so the ordering is what makes both policies hold at once.
#[tokio::test]
async fn the_context_filter_chain_runs_after_context_processing() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ]),
        ModelResponse::new(vec![message("summary-1", COMPACTION_SUMMARY_JSON)]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let filter = Arc::new(RecordingContextFilter {
        observed: Arc::clone(&observed),
        ..RecordingContextFilter::default()
    });
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(3),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new()
        .with_context_processor(Arc::new(compaction))
        .with_context_filter(filter.clone());

    Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the compacted run succeeds");

    let observed = observed.lock().unwrap().clone();
    assert_eq!(
        observed.len(),
        2,
        "the chain runs once per ordinary request, not once per model call"
    );
    assert!(
        observed[1]
            .iter()
            .any(|item| matches!(item, ModelInputItem::Compaction(_))),
        "a filter must observe the compacted view, not the history compaction replaced"
    );
}

/// A complete nine-slot summary body, as a provider honouring the output schema would return it.
const COMPACTION_SUMMARY_JSON: &str = r#"{
    "primary_request_and_intent":"Modify the requested files.",
    "key_technical_concepts":"Keep the existing tool contract.",
    "files_and_code_sections":"No file names were retained.",
    "errors_and_fixes":"None.",
    "problem_solving":"Two tool calls completed.",
    "pending_tasks":"Produce the final response.",
    "current_work":"The tool outputs were compacted.",
    "optional_next_step":"Review the results."
}"#;

/// The reference ledger measures staleness in turns of the whole run, so a resumed segment must
/// keep counting where the last one stopped. A per-segment counter would restart at one, which
/// both re-opens a turn the ledger has already recorded and makes every earlier result look newer
/// than the segment now running.
#[tokio::test]
async fn a_resumed_segment_continues_the_reference_ledgers_turn_axis() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-2", "call-2", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let config = RunConfig::new().with_context_filter(Arc::new(RecordingContextFilter {
        calls: Arc::clone(&calls),
        ..RecordingContextFilter::default()
    }));
    let mut carried = RunState::start(RunId::new("run-loop"));
    carried
        .tool_output_references_mut()
        .record_turn(1, [CallId::new("call-1")], [])
        .expect("an earlier segment recorded its output");
    carried
        .tool_output_references_mut()
        .record_turn(2, [], [])
        .expect("a completed turn without a tool output advances the ledger");
    carried
        .tool_output_references_mut()
        .record_turn(3, [], [])
        .expect("the completed-turn high-water mark survives another empty turn");
    carried
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("the checkpoint represents a started run");

    let result = Runner::run(
        request(vec![tool], &model, &cancel)
            .with_config(config)
            .with_state(carried),
    )
    .await
    .expect("resuming a run with a populated ledger succeeds");

    assert_eq!(
        *calls.lock().unwrap(),
        vec![(4, 1), (5, 3)],
        "the resumed segment's turns continue the run's axis rather than restarting at one"
    );
    assert_eq!(
        result
            .state()
            .tool_output_references()
            .last_referenced_turn(&CallId::new("call-2")),
        Some(4)
    );
    assert_eq!(
        result
            .state()
            .tool_output_references()
            .last_referenced_turn(&CallId::new("call-1")),
        Some(1),
        "an earlier segment's retention facts survive the resume unchanged"
    );
}

/// A resumed run keeps counting from what earlier segments spent, and the two totals that describe
/// it stay distinguishable: the ledger covers the whole run, the result covers this segment.
#[tokio::test]
async fn a_resumed_runs_ledger_covers_every_segment_while_its_result_covers_this_one() {
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![message("msg-1", "改完了")])
            .with_usage(Usage::from_request(RequestUsage::new(20, 6))),
    ]);
    let cancel = CancelScope::root();
    let mut carried = RunState::start(RunId::new("run-loop"));
    carried.record_usage(&Usage::from_request(
        RequestUsage::new(1_000, 100).with_cached_input_tokens(900),
    ));

    let result = Runner::run(request(Vec::new(), &model, &cancel).with_state(carried))
        .await
        .unwrap();

    assert_eq!(result.usage().requests(), 1);
    assert_eq!(result.usage().input_tokens(), 20);

    let ledger = result.state().usage_totals();
    assert_eq!(ledger.requests(), 2);
    assert_eq!(ledger.input_tokens(), 1_020);
    assert_eq!(ledger.cached_input_tokens(), 900);
    assert_eq!(result.state().tokens_used(), 1_126);
}

#[tokio::test]
async fn loop_skeleton_keeps_its_per_turn_decisions_and_record_sequence() {
    // The regression floor for everything built on the loop. It is one snapshot rather than a
    // handful of assertions because the thing being locked *is* the shape: what each turn decided,
    // which records it produced, and in which order — a stage that changes any of those has to
    // change this file first, and the diff is what the review looks at.
    //
    // The script's provider marks every message final, including the two that request a tool. What
    // each turn settled on is what decides the channel; that the provider said otherwise never
    // reaches the record.
    let read = Arc::new(ScriptedTool::new("read_file"));
    let write = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            message("msg-1", "我先看看文件"),
            tool_call("c-1", "call-1", "read_file"),
        ]),
        ModelResponse::new(vec![
            message("msg-2", "找到要改的地方了"),
            tool_call("c-2", "call-2", "write_file"),
        ]),
        ModelResponse::new(vec![message("msg-3", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![read, write], &model, &cancel))
        .await
        .unwrap();

    // Keys are alphabetical: `assert_json_snapshot!` serializes through a sorted map.
    assert_json_snapshot!(loop_skeleton(&result), @r###"
    {
      "outcome": "completed:final",
      "turn_count": 3,
      "turns": [
        {
          "agent": "coder",
          "finish_reason": null,
          "items": [
            {
              "id": "msg-1",
              "kind": "message",
              "phase": "commentary"
            },
            {
              "id": "c-1",
              "kind": "tool_call",
              "phase": null
            },
            {
              "id": "call-1.output",
              "kind": "tool_call_output",
              "phase": null
            }
          ],
          "next_step": "run_again",
          "turn": 1
        },
        {
          "agent": "coder",
          "finish_reason": null,
          "items": [
            {
              "id": "msg-2",
              "kind": "message",
              "phase": "commentary"
            },
            {
              "id": "c-2",
              "kind": "tool_call",
              "phase": null
            },
            {
              "id": "call-2.output",
              "kind": "tool_call_output",
              "phase": null
            }
          ],
          "next_step": "run_again",
          "turn": 2
        },
        {
          "agent": "coder",
          "finish_reason": "final",
          "items": [
            {
              "id": "msg-3",
              "kind": "message",
              "phase": "final"
            }
          ],
          "next_step": "final_output",
          "turn": 3
        }
      ]
    }
    "###);
}

#[tokio::test]
async fn turn_items_rejects_a_record_from_another_run() {
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
        "first-message",
        "first run",
    )])]);
    let first_cancel = CancelScope::root();
    let first = Runner::run(request(Vec::new(), &first_model, &first_cancel))
        .await
        .unwrap();

    let second_model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
        "second-message",
        "second run",
    )])]);
    let second_cancel = CancelScope::root();
    let second = Runner::run(request(Vec::new(), &second_model, &second_cancel))
        .await
        .unwrap();

    let foreign_record = &first.turn_records()[0];
    assert!(second.turn_items(foreign_record).is_empty());
    assert_eq!(
        second.turn_items(&second.turn_records()[0])[0]
            .id()
            .as_str(),
        "second-message"
    );
}

#[tokio::test]
async fn stop_on_first_tool_executes_the_batch_without_calling_the_model_again() {
    let first = Arc::new(ScriptedTool::new("read_file"));
    let second = Arc::new(ScriptedTool::new("write_file"));
    let first_calls = Arc::clone(&first.calls);
    let second_calls = Arc::clone(&second.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message("msg-1", "我先读再写"),
        tool_call("c-1", "call-1", "read_file"),
        tool_call("c-2", "call-2", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![first, second],
        ToolUseBehavior::StopOnFirstTool,
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
    assert!(result.final_message().is_none());
    assert!(matches!(
        result.new_items()[3].kind(),
        RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-1"
    ));
    assert!(matches!(
        result.new_items()[4].kind(),
        RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-2"
    ));
}

#[tokio::test]
async fn stop_at_tools_stops_only_for_matching_names_and_supports_qualified_names() {
    let non_match = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]),
        ModelResponse::new(vec![message("msg-1", "继续后的最终回答")]),
    ]);
    let cancel = CancelScope::root();
    let continued = Runner::run(request_with_tool_use_behavior(
        vec![non_match],
        ToolUseBehavior::stop_at_tools(["write_file"]),
        &model,
        &cancel,
    ))
    .await
    .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        continued.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));

    let matched = Arc::new(ScriptedTool::namespaced("workspace", "write_file"));
    let matching_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-2",
        "call-2",
        "write_file",
    )])]);
    let matched_cancel = CancelScope::root();
    let stopped = Runner::run(request_with_tool_use_behavior(
        vec![matched],
        ToolUseBehavior::stop_at_tools(["workspace.write_file"]),
        &matching_model,
        &matched_cancel,
    ))
    .await
    .unwrap();
    assert_eq!(matching_model.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        stopped.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
}

#[tokio::test]
async fn custom_tool_use_behavior_receives_complete_ordered_results_and_decides_to_stop() {
    let observed = Arc::new(Mutex::new(Vec::<Vec<(String, String, bool)>>::new()));
    let observed_by_handler = Arc::clone(&observed);
    let handler = Arc::new(move |results: &[ToolUseResult]| -> Result<bool> {
        observed_by_handler.lock().unwrap().push(
            results
                .iter()
                .map(|result| {
                    (
                        result.call_id().as_str().to_owned(),
                        result.tool().qualified_name().to_owned(),
                        result.output().is_error(),
                    )
                })
                .collect(),
        );
        Ok(results
            .iter()
            .any(|result| result.tool().name() == "write_file"))
    });
    let read = Arc::new(ScriptedTool::new("read_file"));
    let write = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        tool_call("c-1", "call-1", "read_file"),
        tool_call("c-2", "call-2", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![read, write],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        [[
            ("call-1".to_owned(), "read_file".to_owned(), false),
            ("call-2".to_owned(), "write_file".to_owned(), false),
        ]]
    );
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
}

#[tokio::test]
async fn custom_tool_use_behavior_can_continue_to_the_model() {
    let handler = Arc::new(|_results: &[ToolUseResult]| -> Result<bool> { Ok(false) });
    let tool = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]),
        ModelResponse::new(vec![message("msg-1", "工具结果已处理")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![tool],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
}

#[tokio::test]
async fn tool_stop_does_not_bypass_a_pending_approval_interruption() {
    let gated = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![gated],
        ToolUseBehavior::StopOnFirstTool,
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
}

/// A tool that always fails, so its observation carries `is_error`.
struct FailingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl FailingTool {
    fn new(name: &str) -> Self {
        let scripted = ScriptedTool::new(name);
        Self {
            origin: scripted.origin,
            schema: scripted.schema,
        }
    }
}

#[async_trait]
impl Tool for FailingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Err(Error::tool(
            ToolErrorKind::ExecutionFailed,
            self.origin.qualified_name(),
            "磁盘满了",
        ))
    }
}

#[tokio::test]
async fn not_every_observation_is_a_result_that_can_end_the_run() {
    // Both rows answer the model with a `tool.*` observation, and neither is a value any policy
    // could promote. Stopping on one would report `FinishReason::ToolStop`, whose `is_complete()`
    // is true — R15 sees no closeout owed and R17-3 takes the success edge — over a call that
    // failed, or one that never ran at all.
    let cases: [(&str, Arc<dyn Tool>); 2] =
        [
            // Caller admission refused it, so from the model's side the tool does not exist. The other
            // half of that same story — a name the turn never advertised — is already excluded.
            (
                "not_admitted",
                Arc::new(ScriptedTool::new("write_file").with_options(
                    ToolOptions::new().with_allowed_callers([ToolCaller::Programmatic]),
                )),
            ),
            // It ran and failed. The model has not read the failure yet, which is exactly why the turn
            // owes it another round.
            ("failed", Arc::new(FailingTool::new("write_file"))),
        ];

    for (label, tool) in cases {
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
            ModelResponse::new(vec![message("msg-1", "第二轮的最终回答")]),
        ]);
        let cancel = CancelScope::root();

        let result = Runner::run(request_with_tool_use_behavior(
            vec![tool],
            ToolUseBehavior::StopOnFirstTool,
            &model,
            &cancel,
        ))
        .await
        .unwrap();

        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "{label} 应当逼出下一轮，而不是当场收尾"
        );
        assert!(
            matches!(
                result.outcome(),
                RunOutcome::Completed {
                    reason: FinishReason::Final
                }
            ),
            "{label} 的结局是：{:?}",
            result.outcome()
        );
        // Still answered: excluding it from the policy's view must not drop the record the next
        // request pairs to this call.
        assert!(
            result.new_items().iter().any(|item| matches!(
                item.kind(),
                RunItemKind::ToolCallOutput(output)
                    if output.call_id().as_str() == "call-1" && output.is_error()
            )),
            "{label} 丢了配对给 call-1 的观察"
        );
    }
}

/// What a `CancellationRacingHandler` does once the test lets it reach a decision.
#[derive(Clone, Copy)]
enum HandlerRace {
    /// Never becomes ready, so only cancellation can end the run.
    NeverReady,
    /// Becomes ready without waking anything, so the poll that observes it is provably the one
    /// cancellation triggered — and `CancelScope::run` looks at the handler first.
    ReadyInTheCancellingPoll,
}

/// Ready from a flag, and **never registers a waker**: arming it schedules nothing.
struct ReadyWithoutWaking(Arc<AtomicBool>);

impl Future for ReadyWithoutWaking {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

struct CancellationRacingHandler {
    entered: Arc<AtomicUsize>,
    ready: Arc<AtomicBool>,
    race: HandlerRace,
}

#[async_trait]
impl ToolUseBehaviorHandler for CancellationRacingHandler {
    async fn should_stop(&self, _tool_results: &[ToolUseResult]) -> Result<bool> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        match self.race {
            HandlerRace::NeverReady => {
                std::future::pending::<()>().await;
                unreachable!("a pending handler can only leave through cancellation")
            }
            HandlerRace::ReadyInTheCancellingPoll => {
                ReadyWithoutWaking(Arc::clone(&self.ready)).await;
                // The most dangerous answer it could give: stop the run and call it finished.
                Ok(true)
            }
        }
    }
}

#[tokio::test]
async fn cancellation_wins_over_custom_policy_whether_or_not_it_answers() {
    let cases: [(&str, HandlerRace); 2] = [
        // A bare `.await` would leave the run alive here with nothing running to blame, and no
        // tool left for the drain protocol to reach.
        ("handler_never_answers", HandlerRace::NeverReady),
        // The harder half. `CancelScope::run` polls the handler before the cancellation, so a
        // handler that is ready in that same wake-up returns normally and the scope is never
        // consulted. Nothing downstream would catch it — settlement does no further awaiting and
        // the runner breaks straight out of the loop on `FinalOutput` — so without an exit
        // checkpoint a stopped run settles as `FinishReason::ToolStop`: `is_complete()`, success
        // edge, no closeout owed.
        (
            "handler_answers_stop_in_the_cancelling_poll",
            HandlerRace::ReadyInTheCancellingPoll,
        ),
    ];

    for (label, race) in cases {
        let entered = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(CancellationRacingHandler {
            entered: Arc::clone(&entered),
            ready: Arc::clone(&ready),
            race,
        });
        let tool = Arc::new(ScriptedTool::new("read_file"));
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
            "c-1",
            "call-1",
            "read_file",
        )])]);
        let cancel = CancelScope::root();
        let scope = cancel.clone();

        let task = tokio::spawn(Runner::run(request_with_tool_use_behavior(
            vec![tool],
            ToolUseBehavior::custom(handler),
            &model,
            &cancel,
        )));

        timeout(Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{label}: 策略应当被问到"));

        // Arming before cancelling is what makes this deterministic rather than timed: it wakes
        // nothing, so the next poll the handler receives is the one `cancel` delivers.
        ready.store(true, Ordering::SeqCst);
        scope.cancel(CancelReason::UserInterrupt);

        let settled = timeout(Duration::from_secs(1), task)
            .await
            .unwrap_or_else(|_| panic!("{label}: 取消后 run 必须返回"))
            .expect("run 任务 join");
        let error = match settled {
            Ok(result) => panic!("{label}: 取消的 run 交付了结果 {:?}", result.outcome()),
            Err(error) => error,
        };
        assert!(
            matches!(error, Error::Cancelled { .. }),
            "{label} 报成了：{error}"
        );
    }
}

#[tokio::test]
async fn custom_tool_use_behavior_errors_propagate_unchanged() {
    // Not folded into stop or continue: a policy that could not decide has not decided, and
    // picking either outcome for it would be the framework inventing an answer.
    let handler = Arc::new(|_results: &[ToolUseResult]| -> Result<bool> {
        Err(Error::caller("这个策略拿不定主意"))
    });
    let tool = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "read_file",
    )])]);
    let cancel = CancelScope::root();

    let error = Runner::run(request_with_tool_use_behavior(
        vec![tool],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Caller { .. }));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn settlement_outcome_decides_message_channels_and_carries_them_to_next_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        // Provider can label this Final, but its tool call means the runner must treat it as
        // progress.
        ModelResponse::new(vec![
            message_with_phase("msg-1", "我先写入文件", OutputPhase::Final),
            tool_call("c-1", "call-1", "write_file"),
        ]),
        // The inverse must also be corrected: a terminal turn is the final delivery.
        ModelResponse::new(vec![message_with_phase(
            "msg-2",
            "文件已写入",
            OutputPhase::Commentary,
        )]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Final]
    );
    assert_eq!(result.final_message().unwrap().text_content(), "文件已写入");

    let inputs = model.input_items.lock().unwrap();
    let Some(ModelInputItem::Message(first_message)) = inputs[1].get(1) else {
        panic!("第二轮必须收到第一轮的 assistant message");
    };
    assert_eq!(first_message.phase(), Some(OutputPhase::Commentary));
}

#[tokio::test]
async fn only_last_message_is_delivery_when_commentary_and_delivery_coexist_in_turn() {
    // One response carrying narration and then the answer is the shape this milestone is modelled
    // on. Stamping the whole turn `Final` costs twice: the UI renders two closing deliveries, and
    // the record goes back to the model as input next turn, teaching it that pre-tool narration is
    // what a final answer looks like.
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message_with_phase("msg-1", "我先说一下思路", OutputPhase::Commentary),
        message_with_phase("msg-2", "结论是这样", OutputPhase::Final),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap();

    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Final]
    );
    assert_eq!(result.final_message().unwrap().text_content(), "结论是这样");
    // The convenience projection reads that one field, so commentary in the same turn stays out.
    assert_eq!(result.final_text(), "结论是这样");
}

#[tokio::test]
async fn run_hitting_turn_limit_has_no_delivery_message() {
    // The cap fires between turns, so the last thing the model said is still work in progress.
    // R3-8's error handler is what turns that into a delivered result; until then this has to be
    // `None` rather than promoting a progress update to the run's conclusion.
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(
        (0..8)
            .map(|turn| {
                ModelResponse::new(vec![
                    message_with_phase(
                        &format!("msg-{turn}"),
                        "我接着写下一个文件",
                        OutputPhase::Final,
                    ),
                    tool_call(&format!("c-{turn}"), &format!("call-{turn}"), "write_file"),
                ])
            })
            .collect(),
    );
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(2)),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Commentary]
    );
    assert!(result.final_message().is_none());
    // Empty rather than the last thing the model happened to say: nothing was delivered, and
    // promoting a progress update to the run's conclusion is what this projection must not do.
    assert!(result.final_text().is_empty());

    // No turn ended this run — the cap did, between two of them. Both records still say the turn
    // asked for another, and the reason lives on the outcome instead.
    let codes = result
        .turn_records()
        .iter()
        .map(TurnRecord::next_step_code)
        .collect::<Vec<_>>();
    assert_eq!(codes, ["run_again", "run_again"]);
}

#[tokio::test]
async fn final_message_does_not_treat_non_assistant_message_as_delivery() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![item(
        "user-1",
        RunItemKind::Message(Message::user("不是模型交付").with_phase(OutputPhase::Final)),
    )])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap();

    assert!(result.final_message().is_none());
}

#[tokio::test]
async fn reaching_turn_limit_ends_softly_instead_of_erroring() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    // The model requests a tool on every turn and never concludes.
    let model = ScriptedModel::new(
        (0..8)
            .map(|turn| {
                ModelResponse::new(vec![tool_call(
                    &format!("c-{turn}"),
                    &format!("call-{turn}"),
                    "write_file",
                )])
            })
            .collect(),
    );
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(3)),
    )
    .await
    .unwrap();

    // The limit is the loop's termination condition, not a failure; the host still receives the
    // items already produced.
    assert_eq!(result.turns(), 3);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(result.model_responses().len(), 3);
}

/// The remaining allowance is told to the model at the **tail of the input**, never in the system
/// instructions: a number that changes every turn would break the provider's prefix cache on every
/// call. It rides there as a user message, the one role input history carries on every provider.
/// The reminder is also not a record — it never reaches session history.
#[tokio::test]
async fn the_token_budget_reminder_rides_the_input_tail_and_leaves_the_prefix_alone() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::from_request(RequestUsage::new(7, 5))),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let result = Runner::run(
        request(vec![tool_for_run], &model, &cancel)
            .with_config(RunConfig::new().with_max_tokens(1_000)),
    )
    .await
    .unwrap();

    let instructions = model.instructions.lock().unwrap().clone();
    assert_eq!(instructions[0], instructions[1]);
    assert!(
        !instructions[0]
            .as_deref()
            .is_some_and(|text| text.contains("Task token budget"))
    );

    let inputs = model.input_items.lock().unwrap().clone();
    assert_eq!(
        [reminder_text(&inputs[0]), reminder_text(&inputs[1])],
        [
            "Task token budget: 1000 tokens remain. Pace the remaining work accordingly.",
            "Task token budget: 988 tokens remain. Pace the remaining work accordingly."
        ]
    );
    // By text, not by role: now that the reminder wears the role every other tail item wears,
    // asserting the absence of a system record would pass without saying anything about it.
    assert!(!result.new_items().iter().any(|item| matches!(
        item.kind(),
        RunItemKind::Message(message) if message.text_content().contains("Task token budget")
    )));
}

/// Compaction preserves user messages verbatim, and the budget reminder wears the user role
/// because that is the only role input history carries on every provider. It is still not a turn
/// the user took: carrying it would freeze one turn's remaining-token count inside a record that
/// outlives the turn, sitting next to the fresh reminder the next request appends anyway.
#[tokio::test]
async fn a_compaction_summary_does_not_keep_the_budget_reminder_as_a_user_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("c-1", "call-1", "write_file"),
            tool_call("c-2", "call-2", "write_file"),
        ])
        .with_usage(Usage::from_request(RequestUsage::new(10, 4))),
        ModelResponse::new(vec![message("summary-1", COMPACTION_SUMMARY_JSON)]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();
    let compaction = CompactionCapability::new(
        ContextWindowConfig::default(),
        AnchorRetention::new(0, 0, 1).expect("one tail record is a valid retention policy"),
        Some(3),
        None,
    )
    .expect("a converging explicit item limit is valid");
    let config = RunConfig::new()
        .with_max_tokens(1_000)
        .with_context_processor(Arc::new(compaction));

    let result = Runner::run(request(vec![tool], &model, &cancel).with_config(config))
        .await
        .expect("the compacted run succeeds");

    let summary = result
        .new_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::Compaction(compaction) => Some(compaction.summary().to_owned()),
            _ => None,
        })
        .expect("the run compacted its history");
    assert!(
        summary.contains("帮我改一下文件"),
        "the summary keeps the turn the user actually took: {summary}"
    );
    assert!(
        !summary.contains("Task token budget"),
        "the summary must not keep the loop's own tail item: {summary}"
    );

    // The reminder itself is unaffected: the turn after the compaction still gets a current one.
    let inputs = model.input_items.lock().unwrap().clone();
    assert_eq!(
        reminder_text(&inputs[2]),
        "Task token budget: 986 tokens remain. Pace the remaining work accordingly."
    );
}

/// The response that crosses the ceiling has already been paid for, so the turn it belongs to is
/// settled in full. The run stops at the next turn boundary instead, with the work intact.
#[tokio::test]
async fn an_exhausting_response_still_gets_its_turn_settled() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::from_request(RequestUsage::new(7, 5))),
    ]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let result = Runner::run(
        request(vec![tool_for_run], &model, &cancel)
            .with_config(RunConfig::new().with_max_tokens(12)),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::BudgetExhausted
        }
    ));
    assert_eq!(result.state().tokens_used(), 12);
    assert_eq!(result.model_responses().len(), 1);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.new_items().len(), 2);
}

/// The model answered and the answer was paid for; that the same response also exhausted the
/// budget must not turn a delivered result into an empty one.
#[tokio::test]
async fn a_final_answer_survives_the_response_that_exhausts_the_budget() {
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![message("msg-1", "here is the answer")])
            .with_usage(Usage::from_request(RequestUsage::new(6, 6))),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(Vec::new(), &model, &cancel).with_config(RunConfig::new().with_max_tokens(12)),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(
        result.final_message().unwrap().text_content(),
        "here is the answer"
    );
    assert_eq!(result.state().tokens_used(), 12);
}

#[tokio::test]
async fn terminal_budget_handler_can_deliver_and_record_a_closeout_message() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let result = Runner::run(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(BudgetCloseoutHandler {
                    write_to_history: true,
                })),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        result.final_message().unwrap().text_content(),
        CLOSEOUT_TEXT
    );
    assert_eq!(phases(&result), [OutputPhase::Final]);
    assert_eq!(result.new_items().len(), 3);
}

/// Declining to record is a real choice: the host shows the closeout, and a continuation resumes
/// from the work itself rather than from an apology the model would then have to answer.
#[tokio::test]
async fn a_closeout_can_be_delivered_without_entering_history() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let result = Runner::run(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(BudgetCloseoutHandler {
                    write_to_history: false,
                })),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        result.final_message().unwrap().text_content(),
        CLOSEOUT_TEXT
    );
    assert_eq!(result.new_items().len(), 2);
    assert!(phases(&result).is_empty());
}

#[tokio::test]
async fn a_non_persisted_closeout_is_emitted_as_a_stream_delivery() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let mut stream = Runner::run_streamed(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(BudgetCloseoutHandler {
                    write_to_history: false,
                })),
        ),
    );
    let mut deliveries = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::FinalMessage(message) = event {
            deliveries.push(message.text_content());
        }
    }

    assert_eq!(deliveries, [CLOSEOUT_TEXT]);
    let result = stream.finish().await.unwrap();
    assert_eq!(
        result.final_message().unwrap().text_content(),
        CLOSEOUT_TEXT
    );
    assert_eq!(result.new_items().len(), 2);
}

/// One handler answers for every terminal condition, so it has to be able to say "not mine".
/// Declining leaves the run exactly as it would have ended with no handler installed.
#[tokio::test]
async fn a_handler_that_declines_leaves_the_run_untouched() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let seen = Arc::new(AtomicUsize::new(0));
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let result = Runner::run(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(DecliningCloseoutHandler {
                    seen: Arc::clone(&seen),
                })),
        ),
    )
    .await
    .unwrap();

    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert!(result.final_message().is_none());
    assert_eq!(result.new_items().len(), 2);
}

#[tokio::test]
async fn a_closeout_that_is_not_a_final_answer_is_rejected() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let error = Runner::run(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(MisbehavingCloseoutHandler)),
        ),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Caller { .. }));
}

#[tokio::test]
async fn wall_clock_budget_cancels_an_in_flight_model_call_and_ends_softly() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root();
    let deadline = Deadline::after(Duration::from_millis(20));

    let run = Runner::run(
        pending_request(model, &cancel).with_config(RunConfig::new().with_deadline(deadline)),
    );
    let result = timeout(Duration::from_secs(1), run).await.unwrap().unwrap();

    assert!(started.await.is_ok());
    assert!(dropped.await.is_ok());
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::BudgetExhausted
        }
    ));
    assert_eq!(result.turns(), 1);
    // The run's own scope absorbed the expiry; the caller's is left for the caller to decide about.
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn an_inherited_deadline_remains_cancellation_not_a_budget_closeout() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root().with_deadline(Deadline::after(Duration::from_millis(20)));
    let seen = Arc::new(AtomicUsize::new(0));

    let run = Runner::run(pending_request(model, &cancel).with_config(
        RunConfig::new().with_error_handler(Arc::new(DecliningCloseoutHandler {
            seen: Arc::clone(&seen),
        })),
    ));
    let error = timeout(Duration::from_secs(1), run)
        .await
        .unwrap()
        .unwrap_err();

    assert!(started.await.is_ok());
    assert!(dropped.await.is_ok());
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(seen.load(Ordering::SeqCst), 0);
}

/// A wall clock that only bounded the model call would be no wall clock at all: one slow tool
/// would carry the run past it by however long the tool takes.
#[tokio::test]
async fn the_wall_clock_stops_a_running_tool_too() {
    let inner = Arc::new(ScriptedTool::new("write_file"));
    let tool: Arc<dyn Tool> = Arc::new(SlowTool {
        inner: Arc::clone(&inner),
    });
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::from_request(RequestUsage::new(3, 4))),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();

    let run =
        Runner::run(request(vec![tool], &model, &cancel).with_config(
            RunConfig::new().with_deadline(Deadline::after(Duration::from_millis(20))),
        ));
    let result = timeout(Duration::from_secs(5), run).await.unwrap().unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::BudgetExhausted
        }
    ));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.model_responses().len(), 1);
    assert_eq!(result.usage().total_tokens(), 7);
}

#[tokio::test]
async fn cancelling_the_caller_stops_a_pending_budget_closeout_handler() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let (started_sender, started) = oneshot::channel();
    let handler = Arc::new(PendingCloseoutHandler {
        started: Mutex::new(Some(started_sender)),
    });
    let tool_for_run: Arc<dyn Tool> = Arc::clone(&tool) as Arc<dyn Tool>;

    let task = tokio::spawn(Runner::run(
        request(vec![tool_for_run], &model, &cancel).with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(handler),
        ),
    ));
    started.await.unwrap();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled { .. }));
}

/// Both conditions hold at once, and the host reacts to each differently: an interrupted run is
/// resumed on the user's word, an exhausted one on a larger allowance.
#[tokio::test]
async fn cancellation_outranks_an_exhausted_turn_budget() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();
    let mut spent = BudgetSnapshot::new();
    spent.record_turn();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = Runner::run(
        request(Vec::new(), &model, &cancel)
            .with_config(RunConfig::new().with_max_turns(1))
            .with_state(RunState::start(RunId::new("run-loop")).with_budget(spent)),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled { .. }));
}

/// Spend crosses a continuation boundary, so the second segment starts against what the first one
/// already used rather than against a fresh allowance.
#[tokio::test]
async fn a_continuation_is_measured_against_what_an_earlier_segment_spent() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "done")])]);
    let cancel = CancelScope::root();
    let mut spent = BudgetSnapshot::new();
    spent.record_turn();
    let mut carried = RunState::start(RunId::new("run-loop")).with_budget(spent);
    carried.record_usage(&Usage::from_request(RequestUsage::new(6, 6)));

    let result = Runner::run(
        request(Vec::new(), &model, &cancel)
            .with_config(RunConfig::new().with_max_tokens(12))
            .with_state(carried),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::BudgetExhausted
        }
    ));
    assert_eq!(result.turns(), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

/// The turn cap is the loop's own termination condition, so the one call that can drop it has to
/// answer for it.
#[tokio::test]
async fn a_budget_without_a_turn_cap_is_rejected() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel)
            .with_config(RunConfig::new().with_budget(BudgetLimit::new().with_max_tokens(100))),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Config { .. }));
    assert!(error.to_string().contains("max_turns"));
}

#[tokio::test]
async fn zero_turn_limit_is_rejected_instead_of_treated_as_unlimited() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel).with_config(RunConfig::new().with_max_turns(0)),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("max_turns"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn zero_tool_concurrency_limit_is_rejected_before_model_call() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel)
            .with_config(RunConfig::new().with_max_function_tool_concurrency(0)),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("max_function_tool_concurrency"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stopping_for_pending_approval_is_an_outcome_not_an_error() {
    let gated = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let tool_calls = Arc::clone(&gated.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message_with_phase("msg-1", "我需要写入文件", OutputPhase::Final),
        tool_call("c-1", "call-1", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![gated], &model, &cancel))
        .await
        .unwrap();

    // Reporting an error would give the host no way to answer it and resume.
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("应当停下来问人，实际是 {:?}", result.outcome());
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].kind().is_interruption());
    assert_eq!(
        result.state().pending_interruptions(),
        [items[0].id().clone()]
    );
    assert_eq!(
        result
            .state()
            .pending_interruption_items()
            .collect::<Vec<_>>(),
        [&items[0]],
        "a checkpointed interruption must resolve to its authoritative generated record"
    );
    assert_eq!(result.outcome().finish_reason(), None);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.turns(), 1);
    // The turn settled, and what it settled on is the pause itself — not a finish reason.
    let [record] = result.turn_records() else {
        panic!("一轮结算应当留下一条记录");
    };
    assert_eq!(record.next_step_code(), "interruption");
    assert_eq!(record.finish_reason(), None);
    let RunItemKind::Message(message) = result.new_items()[0].kind() else {
        panic!("第一项必须是模型消息");
    };
    assert_eq!(message.phase(), Some(OutputPhase::Commentary));
    assert!(result.final_message().is_none());

    let resume_model = ScriptedModel::new(Vec::new());
    let error = Runner::run(resume_request(
        Vec::new(),
        &resume_model,
        &cancel,
        result.state().clone(),
    ))
    .await
    .expect_err("an unanswered checkpoint must not issue another model call");
    assert!(error.to_string().contains("unanswered interruptions"));
    assert_eq!(resume_model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approved_checkpointed_tool_call_runs_once_then_continues() {
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let interrupted = Runner::run(request(vec![tool.clone()], &first_model, &cancel))
        .await
        .expect("first segment must ask for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };
    let mut state = interrupted.state().clone();
    state
        .approve(&items[0], true)
        .expect("host approval must be retained");
    let second_model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "done")])]);
    let resumed = Runner::run(resume_request(
        vec![tool.clone()],
        &second_model,
        &cancel,
        state,
    ))
    .await
    .expect("approved checkpoint must resume");

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert!(resumed.state().pending_interruptions().is_empty());
    assert!(
        resumed
            .state()
            .pending_interruption_resolutions()
            .is_empty()
    );
    assert_eq!(resumed.state().permission_rules().len(), 1);
    let output_item = resumed
        .new_items()
        .iter()
        .find(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
        .expect("the approved call must be answered in history");
    // Attributed like every other stored record. A session read back later has to be able to say
    // who produced this one, and it does not travel through settlement's `attribute` step.
    assert!(
        output_item.provenance().is_some(),
        "a settled approval output must name its producing agent"
    );

    // The outcome reaches the failure tracker, so the loop breakers see the resumed call at all.
    // An entry exists for an identity only once an outcome has been filed under it.
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    assert!(
        resumed
            .state()
            .tool_failure()
            .agent(&AgentId::new("coder"))
            .and_then(|failures| failures.entry(&identity))
            .is_some(),
        "a resumed call's outcome must be filed under its identity"
    );

    // The settled output has to reach the model, or the resumed turn asks about a call whose
    // result the transcript never carried.
    let inputs = second_model.input_items.lock().unwrap();
    assert!(
        inputs[0]
            .iter()
            .any(|item| matches!(item, ModelInputItem::ToolCallOutput(_))),
        "the resumed call's output must be part of the next request"
    );
}

/// The mirror of the approval path: the tool must not run, and the model must be told why.
#[tokio::test]
async fn rejected_checkpointed_tool_call_is_refused_without_running_the_tool() {
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let interrupted = Runner::run(request(vec![tool.clone()], &first_model, &cancel))
        .await
        .expect("first segment must ask for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };
    let mut state = interrupted.state().clone();
    state
        .reject(&items[0], true)
        .expect("host rejection must be retained");
    let second_model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "ok")])]);
    let resumed = Runner::run(resume_request(
        vec![tool.clone()],
        &second_model,
        &cancel,
        state,
    ))
    .await
    .expect("rejected checkpoint must resume");

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        tool.calls.load(Ordering::SeqCst),
        0,
        "a rejected call must never reach the tool"
    );
    assert!(resumed.state().pending_interruptions().is_empty());
    assert!(
        resumed
            .state()
            .pending_interruption_resolutions()
            .is_empty()
    );

    // The refusal is a model-visible error result, not a silent gap in the transcript: without the
    // error flag the model reads its own rejected call as having succeeded.
    let refusal_item = resumed
        .new_items()
        .iter()
        .find(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
        .expect("a rejection must still answer the call");
    assert!(
        refusal_item.provenance().is_some(),
        "a refusal record must name its producing agent like every other stored record"
    );
    let RunItemKind::ToolCallOutput(refusal) = refusal_item.kind() else {
        unreachable!()
    };
    assert!(refusal.is_error());
    assert_eq!(refusal.call_id().as_str(), "call-1");

    // A rejection is filed as a refusal, exactly like a call the permission stage declines: both
    // answered without running the tool. Recording nothing would leave an earlier failure streak
    // standing behind a call that never ran.
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    assert!(
        resumed
            .state()
            .tool_failure()
            .agent(&AgentId::new("coder"))
            .and_then(|failures| failures.entry(&identity))
            .is_some(),
        "a host rejection must be filed under the identity it answered"
    );

    // `always` leaves a deny rule behind, so a later call of the same tool is refused without
    // asking the host a second time.
    assert_eq!(resumed.state().permission_rules().len(), 1);
    assert_eq!(
        resumed.state().permission_rules()[0].decision(),
        PermissionDecision::Deny
    );
}

/// A refusal must clear the streak rather than let it stand behind a call that never ran.
#[tokio::test]
async fn a_rejected_call_clears_the_streak_it_never_contributed_to() {
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let agent_id = AgentId::new("coder");
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let interrupted = Runner::run(request(vec![tool.clone()], &first_model, &cancel))
        .await
        .expect("first segment must ask for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };

    // Two earlier failures of the same identity, as an earlier turn would have left them.
    let mut state = interrupted.state().clone();
    {
        let (_, failure) = state.trackers_mut();
        for call in ["call-old-1", "call-old-2"] {
            failure.record_turn(
                &agent_id,
                [ToolOutcome::failed(
                    identity.clone(),
                    CallId::new(call),
                    &json!({ "path": "a.txt" }),
                    &json!({"error": "boom"}),
                    "tool.failed",
                )],
            );
        }
    }
    assert_eq!(
        state
            .tool_failure()
            .no_progress_streak(&agent_id, &identity),
        2
    );

    state
        .reject(&items[0], false)
        .expect("host rejection must be retained");
    let second_model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "ok")])]);
    let resumed = Runner::run(resume_request(
        vec![tool.clone()],
        &second_model,
        &cancel,
        state,
    ))
    .await
    .expect("rejected checkpoint must resume");

    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        resumed
            .state()
            .tool_failure()
            .no_progress_streak(&agent_id, &identity),
        0,
        "a call the host refused is not evidence that the tool is still failing"
    );
}

/// A host answer settles the question an `Ask` rule poses; it does not overrule a `Deny`.
#[tokio::test]
async fn a_checkpointed_answer_satisfies_an_ask_rule_but_not_a_deny_rule() {
    async fn resume_under(
        rule: PermissionRule,
        tool: &Arc<ScriptedTool>,
        cancel: &CancelScope,
    ) -> Result<RunResult> {
        let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
            "c-1",
            "call-1",
            "write_file",
        )])]);
        let interrupted = Runner::run(request(vec![tool.clone()], &first_model, cancel))
            .await
            .expect("first segment must ask for approval");
        let RunOutcome::Interrupted { items } = interrupted.outcome() else {
            panic!("expected an approval interruption");
        };
        let mut state = interrupted.state().clone();
        state
            .approve(&items[0], false)
            .expect("host approval must be retained");
        let second_model =
            ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "done")])]);
        Runner::run(
            resume_request(vec![tool.clone()], &second_model, cancel, state)
                .with_config(RunConfig::new().with_permission_rules([rule])),
        )
        .await
    }

    let cancel = CancelScope::root();

    // An `Ask` rule is a question, and the host has now answered it. Treating the rule as
    // unsatisfied would ask again on a resume that exists precisely because it was answered,
    // leaving the run permanently unresumable.
    let asked = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let resumed = resume_under(
        PermissionRule::new(PermissionDecision::Ask).with_tool_name("write_file"),
        &asked,
        &cancel,
    )
    .await
    .expect("an answered ask rule must not block the resume");
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(asked.calls.load(Ordering::SeqCst), 1);

    // A `Deny` rule is policy that was never up for a click. Resolving the interruption is not a
    // licence to overrule it, so the call is refused rather than executed.
    let denied = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let resumed = resume_under(
        PermissionRule::new(PermissionDecision::Deny).with_tool_name("write_file"),
        &denied,
        &cancel,
    )
    .await
    .expect("a denied checkpoint still resumes, refusing the call");
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        denied.calls.load(Ordering::SeqCst),
        0,
        "a deny rule must survive a host approval"
    );
    let refusal = resumed
        .new_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .expect("a refused call must still be answered");
    assert!(refusal.is_error());
}

#[tokio::test]
async fn cancellation_is_not_treated_as_normal_completion() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "不该跑到")])]);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap_err();

    // Reporting `FinalOutput { Final }` would tell R15 that no closeout is owed and send R17-3
    // down the success path.
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn two_resume_input_modes_produce_different_histories() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    // Display history, session history, and next-turn input are three distinct things. Resume
    // input is a projection rather than a fourth array, so it cannot tell a different story from
    // `new_items`.
    let preserve = result.continuation_input(ContinuationInput::PreserveAll);
    let normalized = result.continuation_input(ContinuationInput::Normalized);
    assert_eq!(preserve.len(), 1 + result.new_items().len());
    assert_eq!(normalized.len(), preserve.len());
    assert_eq!(result.original_input().len(), 1);

    // Original input comes first, followed by generated items in occurrence order.
    assert!(matches!(preserve[0], ModelInputItem::Message(_)));
    assert!(matches!(preserve[1], ModelInputItem::ToolCall(_)));
    assert!(matches!(preserve[2], ModelInputItem::ToolCallOutput(_)));
}

#[tokio::test]
async fn streaming_path_emits_each_turns_records_immediately_and_returns_terminal_state() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut turns = Vec::new();
    let mut items = Vec::new();
    let mut finished = None;
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::TurnStarted { turn, agent } => turns.push((turn, agent)),
            RunStreamEvent::Item(item) => items.push(item.id().as_str().to_owned()),
            RunStreamEvent::Finished(outcome) => finished = Some(outcome),
            _ => panic!("未知事件"),
        }
    }

    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].0, 1);
    // Commentary uses the public identity, which is the agent configured by the user and shown by
    // the host.
    assert_eq!(turns[0].1.as_str(), "coder");
    assert_eq!(items, ["c-1", "call-1.output", "msg-1"]);
    assert!(matches!(
        finished,
        Some(RunOutcome::Completed {
            reason: FinishReason::Final
        })
    ));

    // Exhausting the events still leaves the terminal result available: they are two views of the
    // same run, not a one-use pipe.
    let result = stream.finish().await.unwrap();
    assert_eq!(result.turns(), 2);
    assert_eq!(result.new_items().len(), 3);
}

#[tokio::test]
async fn streaming_path_returns_pending_approval_without_waiting_for_a_host_reply() {
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let tool_calls = Arc::clone(&tool.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message_with_phase(
            "msg-1",
            "I need approval before writing",
            OutputPhase::Final,
        ),
        tool_call("call-1", "tool-call-1", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut turns = Vec::new();
    let mut items = Vec::new();
    let mut finished = None;
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::TurnStarted { turn, .. } => turns.push(turn),
            RunStreamEvent::Item(item) => items.push(item),
            RunStreamEvent::Finished(outcome) => finished = Some(outcome),
            other => panic!("unexpected event while waiting for approval: {other:?}"),
        }
    }

    assert_eq!(turns, [1]);
    assert_eq!(
        items
            .iter()
            .map(|item| item.id().as_str())
            .collect::<Vec<_>>(),
        ["msg-1", "call-1", "tool-call-1.approval"]
    );
    let Some(RunOutcome::Interrupted { items: pending }) = finished else {
        panic!("the stream must finish with the pending approval outcome");
    };
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id().as_str(), "tool-call-1.approval");
    assert!(matches!(pending[0].kind(), RunItemKind::ToolApproval(_)));
    // The question the host is handed is the settled record, not the copy execution raised: it
    // names its producer. Asserted directly rather than only through the equality below, which
    // would still hold if attribution stopped happening and both copies lost it together.
    assert_eq!(
        pending[0]
            .provenance()
            .map(|provenance| provenance.agent_id().as_str()),
        Some("coder")
    );
    assert!(items.contains(&pending[0]));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);

    let result = stream.finish().await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
    assert_eq!(result.turns(), 1);
}

#[tokio::test]
async fn stream_events_include_settlement_normalized_message_channels() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            message_with_phase("msg-1", "我先写入文件", OutputPhase::Final),
            tool_call("c-1", "call-1", "write_file"),
        ]),
        ModelResponse::new(vec![message_with_phase(
            "msg-2",
            "文件已写入",
            OutputPhase::Commentary,
        )]),
    ]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut phases = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event
            && let RunItemKind::Message(message) = item.kind()
        {
            phases.push(message.phase());
        }
    }

    assert_eq!(
        phases,
        [Some(OutputPhase::Commentary), Some(OutputPhase::Final)]
    );
    assert_eq!(stream.finish().await.unwrap().turns(), 2);
}

/// With partial messages on, provider narration reaches the subscriber and the turn still settles
/// from the terminal response rather than from the deltas.
#[tokio::test]
async fn partial_messages_forward_provider_events_and_settle_from_the_terminal_response() {
    let model = StreamingModel::new(vec![vec![
        raw_event("response.created"),
        raw_event("response.output_text.delta"),
        // The adapter's own view of the item. The run publishes its settled copy instead, so this
        // must not reach the subscriber as a second record.
        ModelStreamEvent::RunItem(ra_core::model::RunItemStreamEvent::new(
            "message_output_created",
            message("msg-1", "改完了"),
        )),
        raw_event("response.completed"),
        ModelStreamEvent::Completed(Box::new(
            ModelResponse::new(vec![message("msg-1", "改完了")])
                .with_usage(Usage::from_request(RequestUsage::new(11, 3))),
        )),
    ]]);
    let streamed_calls = Arc::clone(&model.streamed_calls);
    let blocking_calls = Arc::clone(&model.blocking_calls);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(
        streaming_request(&model, &cancel)
            .with_config(RunConfig::new().with_partial_messages(true)),
    );
    let mut raw = Vec::new();
    let mut items = Vec::new();
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::RawResponse(event) => raw.push(event.event_type().to_owned()),
            RunStreamEvent::Item(item) => items.push(item.id().as_str().to_owned()),
            _ => {}
        }
    }

    assert_eq!(
        raw,
        [
            "response.created",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    assert_eq!(
        items,
        ["msg-1"],
        "the settled record is published once; the adapter's copy of it is not a second record"
    );
    assert_eq!(streamed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(blocking_calls.load(Ordering::SeqCst), 0);

    let result = stream.finish().await.unwrap();
    assert_eq!(result.turns(), 1);
    // Usage exists only on the terminal response; a run that folded the deltas would report none.
    assert_eq!(result.usage().input_tokens(), 11);
    assert_eq!(result.usage().output_tokens(), 3);
}

#[tokio::test]
async fn completed_stream_tool_call_starts_before_the_terminal_response() {
    let tool_started = Arc::new(Notify::new());
    let tool = Arc::new(NotifyingTool::new("write_file", Arc::clone(&tool_started)));
    let response = ModelResponse::new(vec![tool_call("call-1", "tool-call-1", "write_file")]);
    let model = DispatchingStreamingModel::new(response, tool_started);
    let cancel = CancelScope::root();

    let result = timeout(
        Duration::from_millis(250),
        Runner::run_streamed(
            RunRequest::new(
                agent_with_tool_use_behavior(vec![tool], ToolUseBehavior::StopOnFirstTool),
                Arc::new(SingleModelResolver {
                    model: Arc::clone(&model) as Arc<dyn Model>,
                }),
                RunId::new("run-loop"),
                cancel.clone(),
                vec![ModelInputItem::Message(Message::user("stream a tool call"))],
            )
            .with_config(RunConfig::new().with_partial_messages(true)),
        )
        .finish(),
    )
    .await
    .expect("the tool must start while the model stream is still open")
    .expect("the streamed turn must settle");

    assert_eq!(result.turns(), 1);
    assert_eq!(result.new_items().len(), 2);
    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::ToolStop),
        "the terminal response still owns settlement and tool-stop policy"
    );
}

/// A run without narration still streams so it can dispatch completed tool calls early — and the
/// switch still decides what a subscriber sees, which is the only thing it was ever about.
#[tokio::test]
async fn a_run_without_partial_messages_streams_without_forwarding_narration() {
    let model = StreamingModel::new(vec![vec![
        raw_event("response.created"),
        raw_event("response.completed"),
        ModelStreamEvent::Completed(Box::new(ModelResponse::new(vec![message(
            "msg-1",
            "完事了",
        )]))),
    ]]);
    let streamed_calls = Arc::clone(&model.streamed_calls);
    let blocking_calls = Arc::clone(&model.blocking_calls);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(streaming_request(&model, &cancel));
    let mut raw = 0_usize;
    let mut items = Vec::new();
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::RawResponse(_) => raw += 1,
            RunStreamEvent::Item(item) => items.push(item.id().as_str().to_owned()),
            _ => {}
        }
    }
    let result = stream.finish().await.unwrap();

    assert_eq!(result.turns(), 1);
    assert_eq!(
        streamed_calls.load(Ordering::SeqCst),
        1,
        "streaming is the execution shape even when raw narration is not forwarded"
    );
    assert_eq!(blocking_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        raw, 0,
        "a host that did not ask for narration must not be sent the provider's frames"
    );
    assert_eq!(
        items,
        ["msg-1"],
        "settled records are the run's own and are published regardless of the switch"
    );
}

/// The blocking entry point does not expose narration but still uses the streaming execution path.
#[tokio::test]
async fn partial_messages_do_not_change_the_blocking_entry_points_execution_shape() {
    let model = StreamingModel::new(vec![vec![ModelStreamEvent::Completed(Box::new(
        ModelResponse::new(vec![message("msg-1", "完事了")]),
    ))]]);
    let streamed_calls = Arc::clone(&model.streamed_calls);
    let cancel = CancelScope::root();

    let result = Runner::run(
        streaming_request(&model, &cancel)
            .with_config(RunConfig::new().with_partial_messages(true)),
    )
    .await
    .unwrap();

    assert_eq!(result.turns(), 1);
    assert_eq!(streamed_calls.load(Ordering::SeqCst), 1);
}

/// Narration is not a turn: a stream that never states its terminal facts fails the call.
#[tokio::test]
async fn a_stream_that_ends_without_terminal_facts_fails_the_turn() {
    let model = StreamingModel::new(vec![vec![
        raw_event("response.created"),
        raw_event("response.output_text.delta"),
    ]]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(
        streaming_request(&model, &cancel)
            .with_config(RunConfig::new().with_partial_messages(true)),
    );
    let error = stream
        .finish()
        .await
        .expect_err("a stream that never settled has not produced a turn");

    assert!(
        error.to_string().contains("without a terminal response"),
        "unexpected message: {error}"
    );
}

/// A terminal response closes the model channel: accepting a later one would make the run settle
/// to a response the provider had already superseded or contradicted.
#[tokio::test]
async fn a_stream_with_an_event_after_its_terminal_response_fails_the_turn() {
    let model = StreamingModel::new(vec![vec![
        ModelStreamEvent::Completed(Box::new(ModelResponse::new(vec![message(
            "msg-first",
            "first answer",
        )]))),
        ModelStreamEvent::Completed(Box::new(ModelResponse::new(vec![message(
            "msg-second",
            "second answer",
        )]))),
    ]]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(
        streaming_request(&model, &cancel)
            .with_config(RunConfig::new().with_partial_messages(true)),
    );
    let error = stream
        .finish()
        .await
        .expect_err("a terminal response must be the last model-stream event");

    assert!(
        error
            .to_string()
            .contains("event after its terminal response"),
        "unexpected error: {error}"
    );
}

/// A failure arriving after the terminal response is reported as itself, not as the ordering rule
/// it also broke: the contract violation is the consequence, and the frame carried the cause.
#[tokio::test]
async fn a_failure_after_the_terminal_response_still_reports_what_failed() {
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(
        model_request(Arc::new(SettledThenFailingModel), &cancel)
            .with_config(RunConfig::new().with_partial_messages(true)),
    );
    let error = stream
        .finish()
        .await
        .expect_err("a stream that failed has not produced a usable turn");

    assert!(
        error.to_string().contains("connection reset"),
        "unexpected error: {error}"
    );
}

/// Runs one tool call whose tool refuses any repeat, and reports whether the tool executed.
///
/// `narrated` is the only difference between the two runs: whether the adapter announced the call
/// as a completed stream item, which is what decides whether it can be started early.
async fn repeat_limited_call(narrated: bool) -> (usize, bool) {
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_max_repeat_streak(NonZeroU32::new(1).unwrap())),
    );
    let tool_calls = Arc::clone(&tool.calls);
    let call = tool_call("c-1", "call-1", "write_file");
    let terminal = ModelResponse::new(vec![call.clone()]);
    let first_turn = if narrated {
        narrated_tool_call_turn(call, terminal)
    } else {
        vec![ModelStreamEvent::Completed(Box::new(terminal))]
    };
    let model = StreamingModel::new(vec![
        first_turn,
        vec![ModelStreamEvent::Completed(Box::new(ModelResponse::new(
            vec![message("msg-1", "换个思路")],
        )))],
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(RunRequest::new(
        agent(vec![tool as Arc<dyn Tool>]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("改文件"))],
    ))
    .await
    .unwrap();

    let refused = result.new_items().iter().any(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) => {
            output.is_error() && output.output()["error"]["code"] == json!("tool.repeated_call")
        }
        _ => false,
    });
    (tool_calls.load(Ordering::SeqCst), refused)
}

/// The repeat breaker counts the call being admitted, which means it cannot be evaluated before
/// the response that contains it exists. A tool that configures the limit therefore waits for
/// settlement instead of starting early — otherwise the threshold would quietly depend on how
/// talkative the provider's adapter happens to be.
#[tokio::test]
async fn a_repeat_limit_is_enforced_whether_or_not_the_adapter_narrates_the_call() {
    assert_eq!(
        repeat_limited_call(true).await,
        (0, true),
        "a narrated call must not escape the breaker by starting before settlement"
    );
    assert_eq!(
        repeat_limited_call(false).await,
        (0, true),
        "the settled path is the behaviour the narrated one has to match"
    );
}

/// A call the stream announced and the terminal response then described differently cannot be
/// settled: one of the two is not what the provider ran, and the runtime cannot tell which.
///
/// How far the tool itself got is deliberately not asserted. The turn fails on the mismatch
/// whether the early task reached the tool or was cancelled while still queued, and pinning that
/// down would be asserting the scheduler rather than the rule.
#[tokio::test]
async fn a_terminal_response_that_changes_a_streamed_call_fails_the_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = StreamingModel::new(vec![narrated_tool_call_turn(
        tool_call_with_arguments("c-1", "call-1", "write_file", json!({ "path": "a.txt" })),
        ModelResponse::new(vec![tool_call_with_arguments(
            "c-1",
            "call-1",
            "write_file",
            json!({ "path": "b.txt" }),
        )]),
    )]);
    let cancel = CancelScope::root();

    let error = Runner::run(RunRequest::new(
        agent(vec![tool as Arc<dyn Tool>]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("改文件"))],
    ))
    .await
    .expect_err("a response that contradicts its own stream cannot settle a turn");

    assert!(
        error.to_string().contains("changed streamed function call"),
        "unexpected error: {error}"
    );
}

/// The terminal response is the record the session stores, so a call missing from it has no
/// record to answer under, however far its tool got.
#[tokio::test]
async fn a_terminal_response_that_omits_a_streamed_call_fails_the_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = StreamingModel::new(vec![narrated_tool_call_turn(
        tool_call("c-1", "call-1", "write_file"),
        ModelResponse::new(vec![message("msg-1", "算了")]),
    )]);
    let cancel = CancelScope::root();

    let error = Runner::run(RunRequest::new(
        agent(vec![tool as Arc<dyn Tool>]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("改文件"))],
    ))
    .await
    .expect_err("a started call the response never bound cannot be settled");

    assert!(
        error.to_string().contains("omitted a function call"),
        "unexpected error: {error}"
    );
}

/// A malformed terminal response still has to reap a tool that began from one of its stream
/// items. Returning directly from classification would only abort the JoinSet on drop, skipping
/// the turn's explicit cancellation-and-drain protocol.
#[tokio::test]
async fn a_terminal_classification_error_drains_streamed_tool_work() {
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(PendingDropTool::new(
        "write_file",
        Arc::clone(&started),
        Arc::clone(&dropped),
    ));
    let first = tool_call("c-1", "call-1", "write_file");
    let terminal = ModelResponse::new(vec![
        first.clone(),
        tool_call("c-2", "call-1", "write_file"),
    ]);
    let model = DispatchingStreamingModel::new(terminal, started);
    let cancel = CancelScope::root();

    let error = Runner::run(RunRequest::new(
        agent(vec![tool as Arc<dyn Tool>]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("改文件"))],
    ))
    .await
    .expect_err("duplicate terminal call ids must fail classification");

    assert!(
        error
            .to_string()
            .contains("claimed by more than one action"),
        "unexpected error: {error}"
    );
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "settlement must wait for the early tool task to acknowledge cancellation"
    );
}

/// Narrates one completed tool call, then fails — while reporting the failure as replay-safe.
///
/// An adapter answering `Safe` here is not lying: it knows the *request* was never accepted. What
/// it cannot know is that a tool already started on the strength of the item it emitted.
struct DispatchThenFailModel {
    tool_started: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Model for DispatchThenFailModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller(
            "this fixture only answers on the streaming entry point",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let call = tool_call("c-1", "call-1", "write_file");
        let tool_started = Arc::clone(&self.tool_started);
        stream::unfold(0_u8, move |stage| {
            let call = call.clone();
            let tool_started = Arc::clone(&tool_started);
            async move {
                match stage {
                    0 => Some((
                        Ok(ModelStreamEvent::RunItem(
                            ra_core::model::RunItemStreamEvent::new("tool_call", call),
                        )),
                        1,
                    )),
                    // Only fail once the side effect has actually happened, so the test is about
                    // the rule rather than about which task the scheduler polled first.
                    1 => {
                        tool_started.notified().await;
                        Some((
                            Err(NormalizedProviderError::new(
                                ProviderErrorKind::Network,
                                "connection reset",
                            )
                            .into_error()),
                            2,
                        ))
                    }
                    _ => None,
                }
            }
        })
        .boxed()
    }

    fn get_retry_advice(&self, _request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        Some(RetryAdvice::new().with_replay_safety(ReplaySafety::Safe))
    }
}

/// Replay safety is the adapter's answer about the provider, never about this runtime. Once a
/// tool has run, no advice can make sending the same request again safe — the second turn would
/// produce the same call and the same side effect.
#[tokio::test]
async fn a_tool_started_from_the_stream_vetoes_a_retry_the_adapter_calls_safe() {
    let tool_started = Arc::new(Notify::new());
    let tool = Arc::new(NotifyingTool::new("write_file", Arc::clone(&tool_started)));
    let tool_calls = Arc::clone(&tool.inner.calls);
    let model = Arc::new(DispatchThenFailModel {
        tool_started,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let model_calls = Arc::clone(&model.calls);
    let cancel = CancelScope::root();

    let retry = ModelRetrySettings::new()
        .with_max_retries(3)
        .with_backoff(
            RetryBackoffSettings::new()
                .with_initial_delay(Duration::ZERO)
                .with_jitter(false),
        )
        .with_policy(Arc::new(NetworkErrorRetryPolicy));

    let error = timeout(
        Duration::from_secs(2),
        Runner::run(
            RunRequest::new(
                agent(vec![tool as Arc<dyn Tool>]),
                Arc::new(SingleModelResolver {
                    model: Arc::clone(&model) as Arc<dyn Model>,
                }),
                RunId::new("run-loop"),
                cancel.clone(),
                vec![ModelInputItem::Message(Message::user("改文件"))],
            )
            .with_config(
                RunConfig::new().with_model_settings(ModelSettings::new().with_retry(retry)),
            ),
        ),
    )
    .await
    .expect("the run must not retry its way into a loop")
    .expect_err("a call whose tool already ran cannot be replayed");

    assert_eq!(error.code(), "provider.network");
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "the retry budget was three; consumption is what stopped it, not exhaustion"
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
}

/// Two stream items for one `call_id` would start the same call twice, and only the first could
/// ever be matched to a record.
#[tokio::test]
async fn a_function_call_narrated_twice_in_one_stream_fails_the_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let call = tool_call("c-1", "call-1", "write_file");
    let model = StreamingModel::new(vec![vec![
        ModelStreamEvent::RunItem(ra_core::model::RunItemStreamEvent::new(
            "tool_call",
            call.clone(),
        )),
        ModelStreamEvent::RunItem(ra_core::model::RunItemStreamEvent::new(
            "tool_call",
            call.clone(),
        )),
        ModelStreamEvent::Completed(Box::new(ModelResponse::new(vec![call]))),
    ]]);
    let cancel = CancelScope::root();

    let error = Runner::run(RunRequest::new(
        agent(vec![tool as Arc<dyn Tool>]),
        Arc::new(SingleModelResolver {
            model: Arc::clone(&model) as Arc<dyn Model>,
        }),
        RunId::new("run-loop"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("改文件"))],
    ))
    .await
    .expect_err("one call_id names one call");

    assert!(
        error.to_string().contains("more than once"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn terminal_result_does_not_require_reading_all_events_first() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "完事了")])]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(request(Vec::new(), &model, &cancel));
    let result = stream.finish().await.unwrap();

    assert_eq!(result.turns(), 1);
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "完事了"
    );
}

#[tokio::test]
async fn dropping_finish_midstream_still_cancels_background_run() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root();
    let stream = Runner::run_streamed(pending_request(model, &cancel));
    let finish = tokio::spawn(async move { stream.finish().await });

    started.await.unwrap();
    finish.abort();
    let _ = finish.await;

    timeout(Duration::from_millis(250), dropped)
        .await
        .expect("dropping finish must cancel the background run")
        .unwrap();
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn both_paths_produce_the_same_result() {
    let script = || {
        vec![
            ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
            ModelResponse::new(vec![message("msg-1", "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let direct_model = ScriptedModel::new(script());
    let direct = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &direct_model,
        &cancel,
    ))
    .await
    .unwrap();

    let streamed_model = ScriptedModel::new(script());
    let streamed = Runner::run_streamed(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &streamed_model,
        &cancel,
    ))
    .finish()
    .await
    .unwrap();

    // The two entry points share one loop and settlement path. If they diverged, the fixed path
    // would hide the other's bug.
    assert_eq!(direct.turns(), streamed.turns());
    assert_eq!(
        direct
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        streamed
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        direct.outcome().finish_reason(),
        streamed.outcome().finish_reason()
    );
}

#[tokio::test]
async fn run_level_model_override_applies_to_every_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();
    let selectors: Selectors = Arc::new(Mutex::new(Vec::new()));

    Runner::run(
        request_recording(vec![tool], &model, &cancel, Arc::clone(&selectors))
            .with_config(RunConfig::new().with_model("run/override")),
    )
    .await
    .unwrap();

    // It must be included on every turn, not only the first. A silently ignored run-level setting
    // manifests as inconsistent billing and latency rather than an error.
    assert_eq!(
        selectors.lock().unwrap().clone(),
        [
            Some("run/override".to_owned()),
            Some("run/override".to_owned())
        ]
    );
}

#[tokio::test]
async fn forced_tool_choice_resets_after_the_model_uses_a_tool() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(request(vec![tool], &model, &cancel).with_config(
        RunConfig::new().with_model_settings(
            ModelSettings::new().with_tool_choice(ToolChoice::Tool("write_file".to_owned())),
        ),
    ))
    .await
    .unwrap();

    // The second turn carries no selection at all rather than an explicit "auto": the release
    // happens on the resolved value, so there is nothing left to send.
    assert_eq!(
        model.tool_choices.lock().unwrap().clone(),
        [Some(ToolChoice::Tool("write_file".to_owned())), None]
    );
}

#[tokio::test]
async fn a_host_that_disabled_tool_calls_keeps_them_disabled() {
    // The model called a tool it was told not to call. Treating that as consent to lift the
    // restriction would let the model's own misbehaviour rewrite the host's setting for the rest
    // of the run, which is the opposite of what a forced-choice release is for.
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(
        request(vec![tool], &model, &cancel).with_config(
            RunConfig::new()
                .with_model_settings(ModelSettings::new().with_tool_choice(ToolChoice::None)),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        model.tool_choices.lock().unwrap().clone(),
        [Some(ToolChoice::None), Some(ToolChoice::None)]
    );
}

#[tokio::test]
async fn repeated_identical_tool_call_is_refused_without_ending_the_run() {
    // A refusal the model cannot read teaches it nothing, so it arrives as an observation: the
    // run keeps its items, the other calls in the batch are not cancelled, and the next turn can
    // try something else.
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_max_repeat_streak(NonZeroU32::new(2).unwrap())),
    );
    let tool_calls = Arc::clone(&tool.calls);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![tool_call("c-2", "call-2", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "changing approach")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    // The second call carries the same arguments as the first, so it is refused before running.
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    let refusal = result
        .new_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-2" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("the refused call still answers its `call_id`");
    assert!(refusal.is_error());
    assert_eq!(
        refusal.output()["error"]["code"],
        json!("tool.repeated_call")
    );
}

#[tokio::test]
async fn run_state_is_returned_with_result_so_next_segment_can_continue_counting() {
    // Each segment uses its own `call_id`: the provider mints one per call and the tracker's
    // replay criterion keys on it. Reusing one would make the second segment a replay of the first.
    let script = |segment: u32| {
        vec![
            ModelResponse::new(vec![tool_call(
                &format!("c-{segment}"),
                &format!("call-{segment}"),
                "write_file",
            )]),
            ModelResponse::new(vec![message(&format!("msg-{segment}"), "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let first_model = ScriptedModel::new(script(1));
    let first = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &first_model,
        &cancel,
    ))
    .await
    .unwrap();
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    assert_eq!(
        first
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        1
    );
    assert_eq!(first.state().starting_agent(), Some(&AgentId::new("coder")));
    assert_eq!(first.state().current_agent(), Some(&AgentId::new("coder")));
    assert_eq!(first.state().original_input(), first.original_input());
    assert_eq!(first.state().generated_items(), first.new_items());
    assert_eq!(first.state().model_responses(), first.model_responses());

    // Carry the complete run state into the second segment. Replacing it with an empty state would
    // reset consecutive segments, turning pause-and-resume into a way to bypass the R3-6 circuit
    // breaker. This resume entry point takes the complete `RunState`, so later runtime facts cannot
    // be omitted either.
    let second_model = ScriptedModel::new(script(2));
    let second = Runner::run(resume_request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &second_model,
        &cancel,
        first.state().clone(),
    ))
    .await
    .unwrap();

    assert_eq!(
        second
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        2
    );
    assert_eq!(
        second_model.input_items.lock().unwrap()[0],
        first.continuation_input(ContinuationInput::PreserveAll),
        "a restored checkpoint must supply the earlier authoritative history"
    );
    assert_eq!(
        second.state().generated_items().len(),
        first.new_items().len() + second.new_items().len()
    );
    assert_eq!(
        second.state().model_responses().len(),
        first.model_responses().len() + second.model_responses().len()
    );

    // The third segment is what catches a continuation projected from the segment instead of from
    // the run: `second.new_items()` holds only what the second segment produced, so pairing it
    // with the run's opening input would send the model a conversation missing its first segment.
    let third_model = ScriptedModel::new(script(3));
    let third = Runner::run(resume_request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &third_model,
        &cancel,
        second.state().clone(),
    ))
    .await
    .unwrap();

    assert_eq!(
        third_model.input_items.lock().unwrap()[0],
        second.continuation_input(ContinuationInput::PreserveAll),
        "a continuation must project the whole run's history, not the last segment's"
    );
    assert_eq!(
        third.state().generated_items().len(),
        first.new_items().len() + second.new_items().len() + third.new_items().len()
    );
    assert_eq!(
        third.original_input(),
        second.continuation_input(ContinuationInput::PreserveAll),
        "an automatic resume records the checkpoint projection as its continuation base"
    );
}

/// A resumed request retains an explicit continuation base rather than silently replacing it with
/// the checkpoint projection.
#[tokio::test]
async fn resuming_a_checkpoint_uses_the_caller_supplied_continuation_base() {
    let first_model =
        ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "改完了")])]);
    let cancel = CancelScope::root();
    let first = Runner::run(request(Vec::new(), &first_model, &cancel))
        .await
        .unwrap();

    let second_model =
        ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "又改完了")])]);
    let mut continuation = first.continuation_input(ContinuationInput::PreserveAll);
    continuation.push(ModelInputItem::Message(Message::user("继续检查边界条件")));
    let result = Runner::run(
        RunRequest::new(
            agent(Vec::new()),
            Arc::new(FixedResolver {
                model: Arc::clone(&second_model),
                selectors: Arc::new(Mutex::new(Vec::new())),
            }),
            RunId::new("run-loop"),
            cancel.clone(),
            continuation.clone(),
        )
        .with_state(first.state().clone()),
    )
    .await
    .unwrap();

    assert_eq!(
        second_model.calls.load(Ordering::SeqCst),
        1,
        "the supplied continuation reaches the provider"
    );
    assert_eq!(second_model.input_items.lock().unwrap()[0], continuation);
    assert_eq!(result.original_input(), continuation);
    let mut expected_next = continuation;
    expected_next.extend(
        result
            .new_items()
            .iter()
            .filter_map(RunItem::to_model_input),
    );
    assert_eq!(
        result.continuation_input(ContinuationInput::PreserveAll),
        expected_next,
        "the next continuation appends only records from this segment"
    );

    let third_model = ScriptedModel::new(Vec::new());
    let error = Runner::run(resume_request(
        Vec::new(),
        &third_model,
        &cancel,
        result.state().clone(),
    ))
    .await
    .expect_err("automatic projection cannot omit a caller-managed input");
    assert!(error.to_string().contains("cannot project input"));
    assert_eq!(third_model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn work_state_handle_is_propagated_all_the_way_to_tools() {
    // This chain is what R3-13 buys: attached once on the run, it reaches the tool through
    // settlement, batch, and dispatch, so R17 adds channel operations to the handle rather than a
    // parameter to four layers and to every third-party `Tool::call`.
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.work_states);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();
    let task_state: Arc<dyn WorkStateHandle> = Arc::new(TaskState { plan: "第三步" });

    Runner::run(
        request(vec![tool], &model, &cancel)
            .with_services(ToolServices::new().with_work_state(task_state)),
    )
    .await
    .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), [Some("第三步".to_owned())]);
}

#[tokio::test]
async fn run_without_work_state_lets_tools_observe_none_not_empty_shell() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.work_states);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), [None]);
}

#[tokio::test]
async fn dropping_stream_does_not_cancel_callers_own_scope() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(pending_request(model, &cancel));
    started.await.unwrap();
    drop(stream);

    // Dropping the stream cancels this run, preventing the provider from spending on an unobserved
    // result, but it cancels a child scope. Cancelling the caller's scope too would let one run's
    // lifetime determine the whole host's lifetime.
    timeout(Duration::from_millis(250), dropped)
        .await
        .expect("dropping the stream must cancel and reap its run")
        .unwrap();
    assert!(!cancel.is_cancelled());

    // The same caller scope can start another run.
    let again = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "又完事了")])]);
    let result = Runner::run(request(Vec::new(), &again, &cancel))
        .await
        .unwrap();
    assert_eq!(result.turns(), 1);
}

#[tokio::test]
async fn one_run_context_reaches_dynamic_availability_and_every_tool_call() {
    // The acceptance the two context layers exist for: a run has one identity, and every stage that
    // enters third-party code is told the same one. Preparation asks `is_enabled` before the model
    // call; dispatch calls `call` after it; a second turn does both again. All four readings name
    // the same run, the same public agent and the same host object.
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_availability(ToolAvailability::Dynamic)),
    );
    let seen = Arc::clone(&tool.runs);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![tool_call("c-2", "call-2", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(
        request(vec![tool], &model, &cancel)
            .with_app_context(Arc::new(HostState { workspace: "/ws" })),
    )
    .await
    .unwrap();

    let seen = seen.lock().unwrap().clone();
    let expected = "run-loop/coder//ws";
    assert_eq!(
        seen,
        [
            format!("enabled:{expected}"),
            format!("call:{expected}"),
            format!("enabled:{expected}"),
            format!("call:{expected}"),
            format!("enabled:{expected}"),
        ]
    );
}

#[tokio::test]
async fn a_run_without_host_state_reads_none_rather_than_another_hosts_object() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.runs);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["call:run-loop/coder/<none>"]
    );
}

#[tokio::test]
async fn resuming_with_state_carries_the_states_run_id_into_runner_and_context() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.runs);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();

    let previous_state = RunState::start(RunId::new("authoritative-resumed-id"));

    let result = Runner::run(
        RunRequest::new(
            agent(vec![tool]),
            Arc::new(FixedResolver {
                model: Arc::clone(&model),
                selectors: Arc::new(Mutex::new(Vec::new())),
            }),
            RunId::new("placeholder-id"),
            cancel,
            vec![ModelInputItem::Message(Message::user("do something"))],
        )
        .with_state(previous_state),
    )
    .await
    .unwrap();

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["call:authoritative-resumed-id/coder/<none>"]
    );
    assert_eq!(result.state().run_id().as_str(), "authoritative-resumed-id");
}

struct InspectingControlTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    observed_requests: Arc<Mutex<Vec<String>>>,
}

impl InspectingControlTool {
    fn new(name: &str) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            observed_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Tool for InspectingControlTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        for req in context.run().pending_control_requests() {
            self.observed_requests
                .lock()
                .unwrap()
                .push(req.request_id().to_owned());
        }
        Ok(ToolOutput::text("inspected"))
    }
}

#[tokio::test]
async fn runner_projects_pending_control_requests_into_live_context_seen_by_tools() {
    let tool = Arc::new(InspectingControlTool::new("inspect_reqs"));
    let observed = Arc::clone(&tool.observed_requests);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "inspect_reqs")]),
        ModelResponse::new(vec![message("msg-1", "done")]),
    ]);
    let cancel = CancelScope::root();

    let state =
        RunState::start(RunId::new("run-pending-reqs")).with_pending_control_requests(vec![
            PendingControlRequest::new("approval-req-42"),
            PendingControlRequest::new("approval-req-99"),
        ]);

    Runner::run(request(vec![tool], &model, &cancel).with_state(state))
        .await
        .unwrap();

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        ["approval-req-42", "approval-req-99"]
    );
}

#[tokio::test]
async fn runner_automatically_snapshots_event_seq_allocator_advancement_into_run_state() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "done")])]);
    let cancel = CancelScope::root();

    let request = request(Vec::new(), &model, &cancel);
    let allocator = request.event_seq_allocator().clone();

    assert_eq!(allocator.allocate().unwrap(), 0);
    assert_eq!(allocator.allocate().unwrap(), 1);
    assert_eq!(allocator.allocate().unwrap(), 2);

    let result = Runner::run(request).await.unwrap();

    assert_eq!(result.state().next_host_event_seq(), 3);
}

#[tokio::test]
async fn runner_restores_event_seq_allocator_with_persisted_max_seq() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "done")])]);
    let cancel = CancelScope::root();

    let state = RunState::start(RunId::new("run-restore-test")).with_next_host_event_seq(10);
    let request =
        request(Vec::new(), &model, &cancel).with_state_and_persisted_max_seq(state, Some(25));

    let allocator = request.event_seq_allocator().clone();
    assert_eq!(allocator.current_next(), 26);
    assert_eq!(allocator.allocate().unwrap(), 26);
    assert_eq!(allocator.allocate().unwrap(), 27);

    let result = Runner::run(request).await.unwrap();

    assert_eq!(result.state().next_host_event_seq(), 28);
}

#[tokio::test]
async fn test_live_host_event_emission_across_multiple_tools() {
    use ra_core::event::{
        ExecEvent, InMemoryHostEventSink,
        exec::{ExecOutputEvent, ExecSessionId, ExecStartedEvent, ExecStreamKind},
    };

    struct ToolA {
        origin: ToolOrigin,
        schema: ToolSchema,
    }
    impl ToolA {
        fn new() -> Self {
            Self {
                origin: ToolOrigin::new("tool_a").unwrap(),
                schema: ToolSchema::new(
                    "tool_a",
                    json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false
                    }),
                )
                .unwrap(),
            }
        }
    }
    #[async_trait]
    impl Tool for ToolA {
        fn origin(&self) -> &ToolOrigin {
            &self.origin
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
            let emitter = context
                .event_emitter()
                .expect("tool context must have event emitter");
            let s1 = emitter.emit_exec(ExecEvent::Started(ExecStartedEvent::new(
                ExecSessionId::new("session-a"),
                "command-a",
            )))?;
            let s2 = emitter.emit_exec(ExecEvent::Output(ExecOutputEvent::new(
                ExecSessionId::new("session-a"),
                ExecStreamKind::Stdout,
                0,
                10,
                "output-a",
            )))?;
            Ok(ToolOutput::text(format!(
                "tool_a executed with seq {s1},{s2}"
            )))
        }
    }

    struct ToolB {
        origin: ToolOrigin,
        schema: ToolSchema,
    }
    impl ToolB {
        fn new() -> Self {
            Self {
                origin: ToolOrigin::new("tool_b").unwrap(),
                schema: ToolSchema::new(
                    "tool_b",
                    json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false
                    }),
                )
                .unwrap(),
            }
        }
    }
    #[async_trait]
    impl Tool for ToolB {
        fn origin(&self) -> &ToolOrigin {
            &self.origin
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
            let emitter = context
                .event_emitter()
                .expect("tool context must have event emitter");
            let s1 = emitter.emit_exec(ExecEvent::Started(ExecStartedEvent::new(
                ExecSessionId::new("session-b"),
                "command-b",
            )))?;
            let s2 = emitter.emit_exec(ExecEvent::Output(ExecOutputEvent::new(
                ExecSessionId::new("session-b"),
                ExecStreamKind::Stdout,
                0,
                10,
                "output-b",
            )))?;
            Ok(ToolOutput::text(format!(
                "tool_b executed with seq {s1},{s2}"
            )))
        }
    }

    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("item-1", "call-1", "tool_a"),
            tool_call("item-2", "call-2", "tool_b"),
        ]),
        ModelResponse::new(vec![message("msg-final", "all tools done")]),
    ]);

    let cancel = CancelScope::root();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let services = ToolServices::new().with_event_sink(sink.clone());

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(ToolA::new()), Arc::new(ToolB::new())];
    let req = request(tools, &model, &cancel).with_services(services);

    let result = Runner::run(req).await.expect("run must succeed");
    let run_id = result.state().run_id().clone();

    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    let events = sink.events();
    // Invariant 1: sequence numbers are unique within the run
    let seq_set: std::collections::HashSet<u64> = events.iter().map(|e| e.seq()).collect();
    assert_eq!(
        seq_set.len(),
        4,
        "all emitted host events must have unique sequence numbers"
    );

    // Invariant 2: events for each tool execution session are strictly monotonic
    for session_name in ["session-a", "session-b"] {
        let session_seqs: Vec<u64> = events
            .iter()
            .filter_map(|e| match e.body() {
                ra_core::event::HostEventBody::Exec(ExecEvent::Started(s))
                    if s.session_id().as_str() == session_name =>
                {
                    Some(e.seq())
                }
                ra_core::event::HostEventBody::Exec(ExecEvent::Output(o))
                    if o.session_id().as_str() == session_name =>
                {
                    Some(e.seq())
                }
                _ => None,
            })
            .collect();
        assert_eq!(session_seqs.len(), 2);
        assert!(
            session_seqs[0] < session_seqs[1],
            "session events must be strictly monotonic"
        );
    }

    // Invariant 3: all events share the identical run_id and authentic agent_id
    for evt in &events {
        assert_eq!(evt.run_id(), &run_id);
        assert_eq!(evt.agent_id().as_str(), "coder");
    }

    // Invariant 4: run state next seq advanced to at least 4
    assert!(result.state().next_host_event_seq() >= 4);
}
