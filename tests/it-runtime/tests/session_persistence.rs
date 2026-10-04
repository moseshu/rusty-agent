//! A run that reads its history from a session and appends to it what the run adds.
//!
//! Ported from `openai-agents-python`'s session tests: `tests/memory/test_session.py`,
//! `tests/memory/test_session_limit.py`, the `prepare_input_with_session` and guardrail cases in
//! `tests/test_agent_runner.py`, and the resume case in `tests/test_run_impl_resume_paths.py`.
//! The reference compares items by object identity where this port compares them by `ItemId`, so
//! a case that hands the callback "a copy" builds one under a fresh id.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, InputGuardrail, OutputGuardrail},
    item::{
        CallId, ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase,
        Reasoning, RunItem, RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{Session, SessionId, SessionInputCallback, SessionSettings},
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{
        RunConfig, RunOutcome, RunRequest, RunResult, Runner,
        session_persistence::prepare_input_with_session,
    },
};
use ra_session::InMemorySession;
use serde_json::json;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// Replays one response per model call and keeps the input of every request.
#[derive(Default)]
struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<Vec<ModelInputItem>>>,
}

impl ScriptedModel {
    fn enqueue(&self, response: ModelResponse) {
        self.script.lock().unwrap().push_back(response);
    }

    fn last_input(&self) -> Vec<ModelInputItem> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.input().to_vec());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("the scripted model ran out of responses"))
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: AtomicUsize,
}

impl ScriptedTool {
    fn new(name: &str, options: ToolOptions) -> Arc<Self> {
        Arc::new(Self {
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
            calls: AtomicUsize::new(0),
        })
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

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("tool_result"))
    }
}

struct InputCheck {
    tripwire: bool,
    parallel: bool,
}

#[async_trait]
impl InputGuardrail for InputCheck {
    fn name(&self) -> &str {
        "input_check"
    }

    fn run_in_parallel(&self) -> bool {
        self.parallel
    }

    async fn check(
        &self,
        _context: &RunContext,
        _input: &[ModelInputItem],
    ) -> Result<GuardrailFunctionOutput> {
        Ok(if self.tripwire {
            GuardrailFunctionOutput::tripwire()
        } else {
            GuardrailFunctionOutput::pass()
        })
    }
}

enum OutputVerdict {
    Pass,
    Trip,
    Fail,
}

struct OutputCheck(OutputVerdict);

#[async_trait]
impl OutputGuardrail for OutputCheck {
    fn name(&self) -> &str {
        "output_check"
    }

    async fn check(
        &self,
        _context: &RunContext,
        _output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        match self.0 {
            OutputVerdict::Pass => Ok(GuardrailFunctionOutput::pass()),
            OutputVerdict::Trip => Ok(GuardrailFunctionOutput::tripwire()),
            OutputVerdict::Fail => Err(Error::caller("guardrail failed")),
        }
    }
}

/// An in-memory session that counts appends and can be told to refuse them.
struct CountingSession {
    inner: InMemorySession,
    appends: Mutex<Vec<usize>>,
    fail_appends: bool,
    ignore_ids: bool,
}

impl CountingSession {
    fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemorySession::new(id),
            appends: Mutex::new(Vec::new()),
            fail_appends: false,
            ignore_ids: false,
        })
    }

    fn failing(id: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemorySession::new(id),
            appends: Mutex::new(Vec::new()),
            fail_appends: true,
            ignore_ids: false,
        })
    }

    fn append_sizes(&self) -> Vec<usize> {
        self.appends.lock().unwrap().clone()
    }
}

#[async_trait]
impl Session for CountingSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    fn ignore_ids_for_matching(&self) -> bool {
        self.ignore_ids
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.inner.get_items(limit).await
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        if self.fail_appends {
            return Err(Error::caller("the session store is unavailable"));
        }
        self.appends.lock().unwrap().push(items.len());
        self.inner.add_items(items).await
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.inner.pop_item().await
    }

    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

#[derive(Default)]
struct Setup {
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: Option<ToolUseBehavior>,
    input_guardrail: Option<Arc<dyn InputGuardrail>>,
    output_guardrail: Option<Arc<dyn OutputGuardrail>>,
    callback: Option<Arc<dyn SessionInputCallback>>,
    settings: Option<SessionSettings>,
}

impl Setup {
    fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    fn tool_use_behavior(mut self, behavior: ToolUseBehavior) -> Self {
        self.tool_use_behavior = Some(behavior);
        self
    }

    fn input_guardrail(mut self, guardrail: InputCheck) -> Self {
        self.input_guardrail = Some(Arc::new(guardrail));
        self
    }

    fn output_guardrail(mut self, verdict: OutputVerdict) -> Self {
        self.output_guardrail = Some(Arc::new(OutputCheck(verdict)));
        self
    }

    fn callback(mut self, callback: impl SessionInputCallback + 'static) -> Self {
        self.callback = Some(Arc::new(callback));
        self
    }

    fn settings(mut self, settings: SessionSettings) -> Self {
        self.settings = Some(settings);
        self
    }

    fn request(
        &self,
        model: &Arc<ScriptedModel>,
        run_id: &str,
        input: Vec<ModelInputItem>,
        session: Arc<dyn Session>,
    ) -> RunRequest {
        let mut agent = AgentSpec::builder()
            .id(AgentId::new("assistant"))
            .name("Assistant")
            .instructions("help")
            .tools(self.tools.clone())
            .tool_use_behavior(self.tool_use_behavior.clone().unwrap_or_default());
        if let Some(guardrail) = &self.input_guardrail {
            agent = agent.input_guardrails(vec![Arc::clone(guardrail)]);
        }
        if let Some(guardrail) = &self.output_guardrail {
            agent = agent.output_guardrails(vec![Arc::clone(guardrail)]);
        }
        let mut config = RunConfig::new();
        if let Some(callback) = &self.callback {
            config = config.with_session_input_callback(Arc::clone(callback));
        }
        if let Some(settings) = self.settings {
            config = config.with_session_settings(settings);
        }
        RunRequest::new(
            AgentBinding::direct(agent.build().expect("the agent declaration must be valid")),
            Arc::new(Resolver(Arc::clone(model))),
            RunId::new(run_id),
            CancelScope::root(),
            input,
        )
        .with_config(config)
        .with_session(session)
    }
}

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

fn assistant(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn answer(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![assistant(id, text)])
}

fn call(id: &str, call_id: &str, name: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, json!({}))),
    )
}

fn output(id: &str, call_id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new(call_id), json!(text))),
    )
}

fn stored_user(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}

/// A compact view of a model input: `role:text` for a message, `kind:call_id` for the rest.
fn describe_input(items: &[ModelInputItem]) -> Vec<String> {
    items.iter().map(describe).collect()
}

fn describe(item: &ModelInputItem) -> String {
    match item {
        ModelInputItem::Message(message) => {
            let role = match message.role() {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => "other",
            };
            format!("{role}:{}", message.text_content())
        }
        other => format!(
            "{}:{}",
            other.label(),
            other.call_id().map(CallId::as_str).unwrap_or_default()
        ),
    }
}

fn describe_items(items: &[RunItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            item.to_model_input()
                .map_or_else(|| item.kind().label().to_owned(), |input| describe(&input))
        })
        .collect()
}

async fn stored(session: &dyn Session) -> Vec<String> {
    describe_items(&session.get_items(None).await.unwrap())
}

async fn prepare(
    input: &[ModelInputItem],
    session: &dyn Session,
    callback: Option<&dyn SessionInputCallback>,
    settings: Option<&SessionSettings>,
) -> (Vec<String>, Vec<String>) {
    let plan = prepare_input_with_session(
        &RunId::new("run-prepare"),
        input,
        session,
        callback,
        settings,
    )
    .await
    .expect("the session input must prepare");
    (
        describe_input(plan.prepared_for_model()),
        describe_items(plan.append_for_turn()),
    )
}

async fn run_streamed(request: RunRequest) -> Result<RunResult> {
    let mut stream = Runner::run_streamed(request);
    while stream.next_event().await.is_some() {}
    stream.finish().await
}

// ---------------------------------------------------------------------------------------------
// Preparing the input
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unbounded_read_keeps_a_history_output_whose_call_is_gone() {
    let session = InMemorySession::new_with_items("s", vec![output("o1", "call_prepare", "ok")]);
    let (prepared, appended) = prepare(&[user("hello")], &session, None, None).await;
    assert_eq!(prepared, ["tool_call_output:call_prepare", "user:hello"]);
    assert_eq!(appended, ["user:hello"]);
}

#[tokio::test]
async fn a_bounded_read_drops_a_history_output_its_bound_cut_the_call_from() {
    let session = InMemorySession::new_with_items("s", vec![output("o1", "call_prepare", "ok")]);
    let limit = SessionSettings::new().with_limit(1);
    let (prepared, appended) = prepare(&[user("hello")], &session, None, Some(&limit)).await;
    assert_eq!(prepared, ["user:hello"]);
    assert_eq!(appended, ["user:hello"]);
}

#[tokio::test]
async fn a_bounded_read_keeps_a_new_output_without_its_call() {
    let session = InMemorySession::new("s");
    let new_output = output("unused", "call_prepare", "ok")
        .to_model_input()
        .unwrap();
    let limit = SessionSettings::new().with_limit(1);
    let (prepared, appended) = prepare(&[new_output], &session, None, Some(&limit)).await;
    assert_eq!(prepared, ["tool_call_output:call_prepare"]);
    assert_eq!(appended, ["tool_call_output:call_prepare"]);
}

#[tokio::test]
async fn a_callback_output_is_not_pruned_for_a_bounded_read() {
    let session = InMemorySession::new_with_items("s", vec![output("o1", "call_callback", "ok")]);
    let limit = SessionSettings::new().with_limit(1);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(history.iter().chain(new_input.iter()).cloned().collect())
    };
    let (prepared, appended) =
        prepare(&[user("hello")], &session, Some(&callback), Some(&limit)).await;
    assert_eq!(prepared, ["tool_call_output:call_callback", "user:hello"]);
    assert_eq!(appended, ["user:hello"]);
}

#[tokio::test]
async fn a_bounded_read_keeps_history_calls_with_their_outputs() {
    let session = InMemorySession::new_with_items(
        "s",
        vec![
            call("c1", "call_prepare", "lookup"),
            output("o1", "call_prepare", "ok"),
        ],
    );
    let limit = SessionSettings::new().with_limit(2);
    let (prepared, appended) = prepare(&[user("hello")], &session, None, Some(&limit)).await;
    assert_eq!(
        prepared,
        [
            "tool_call:call_prepare",
            "tool_call_output:call_prepare",
            "user:hello"
        ]
    );
    assert_eq!(appended, ["user:hello"]);
}

#[tokio::test]
async fn a_new_output_replaces_the_history_output_for_the_same_call() {
    let session =
        InMemorySession::new_with_items("s", vec![output("o1", "call_latest", "history-output")]);
    let latest = output("unused", "call_latest", "new-output")
        .to_model_input()
        .unwrap();
    let plan =
        prepare_input_with_session(&RunId::new("run-prepare"), &[latest], &session, None, None)
            .await
            .unwrap();
    let outputs: Vec<&ToolCallOutput> = plan
        .prepared_for_model()
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].output(), &json!("new-output"));
    assert_eq!(plan.append_for_turn().len(), 1);
}

#[tokio::test]
async fn a_history_call_without_an_output_is_dropped_with_its_reasoning() {
    let session = InMemorySession::new_with_items(
        "s",
        vec![
            RunItem::new(
                ItemId::new("r1"),
                RunItemKind::Reasoning(Reasoning::new().with_id("rs_1")),
            ),
            call("c1", "orphan_call", "tool_orphan"),
        ],
    );
    let (prepared, appended) = prepare(&[user("hello")], &session, None, None).await;
    assert_eq!(prepared, ["user:hello"]);
    assert_eq!(appended, ["user:hello"]);
}

#[tokio::test]
async fn a_new_call_without_an_output_is_kept() {
    let session =
        InMemorySession::new_with_items("s", vec![call("c1", "orphan_call", "tool_orphan")]);
    let pending = call("unused", "manual_call", "shell")
        .to_model_input()
        .unwrap();
    let (prepared, appended) = prepare(&[pending], &session, None, None).await;
    assert_eq!(prepared, ["tool_call:manual_call"]);
    assert_eq!(appended, ["tool_call:manual_call"]);
}

#[tokio::test]
async fn a_callback_that_drops_the_new_input_appends_nothing() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, _new_input: &mut Vec<RunItem>| Ok(history.clone());
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:history"]);
    assert!(appended.is_empty());
}

#[tokio::test]
async fn a_callback_that_reorders_the_new_input_appends_it_in_its_order() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(vec![
            new_input[1].clone(),
            history[0].clone(),
            new_input[0].clone(),
        ])
    };
    let (prepared, appended) = prepare(
        &[user("first"), user("second")],
        &session,
        Some(&callback),
        None,
    )
    .await;
    assert_eq!(prepared, ["user:second", "user:history", "user:first"]);
    assert_eq!(appended, ["user:second", "user:first"]);
}

#[tokio::test]
async fn an_item_a_callback_adds_is_appended() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(vec![
            assistant("extra", "extra"),
            history[0].clone(),
            new_input[0].clone(),
        ])
    };
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["assistant:extra", "user:history", "user:new"]);
    assert_eq!(appended, ["assistant:extra", "user:new"]);
}

#[tokio::test]
async fn an_item_rebuilt_equal_to_history_is_matched_to_it_by_content() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(vec![
            RunItem::new(ItemId::new("rebuilt-history"), history[0].kind().clone()),
            RunItem::new(ItemId::new("rebuilt-new"), new_input[0].kind().clone()),
        ])
    };
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:history", "user:new"]);
    assert_eq!(appended, ["user:new"]);
}

#[tokio::test]
async fn repeating_a_history_item_does_not_take_an_equal_new_item_for_history() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "same")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(vec![
            history[0].clone(),
            history[0].clone(),
            new_input[0].clone(),
        ])
    };
    let (prepared, appended) = prepare(&[user("same")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:same", "user:same", "user:same"]);
    assert_eq!(appended, ["user:same"]);
}

#[tokio::test]
async fn a_history_item_taken_out_of_the_history_list_is_still_history() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        let moved = history.remove(0);
        Ok(vec![moved.clone(), new_input[0].clone(), moved])
    };
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:history", "user:new", "user:history"]);
    assert_eq!(appended, ["user:new"]);
}

#[tokio::test]
async fn a_history_item_moved_into_the_new_input_list_stays_history() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        let moved = history.remove(0);
        new_input.insert(0, moved.clone());
        let mut combined = new_input.clone();
        combined.push(moved);
        Ok(combined)
    };
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:history", "user:new", "user:history"]);
    assert_eq!(appended, ["user:new"]);
}

#[tokio::test]
async fn an_item_a_callback_writes_into_the_history_list_is_history() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        history[0] = stored_user("summary", "summary");
        Ok(history.iter().chain(new_input.iter()).cloned().collect())
    };
    let (prepared, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(prepared, ["user:summary", "user:new"]);
    assert_eq!(appended, ["user:new"]);
}

#[tokio::test]
async fn a_second_rebuilt_copy_of_a_history_item_is_new() {
    let session = InMemorySession::new_with_items("s", vec![stored_user("h1", "history")]);
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        let rebuilt = RunItem::new(ItemId::new("rebuilt"), history[0].kind().clone());
        Ok(vec![history[0].clone(), rebuilt, new_input[0].clone()])
    };
    let (_, appended) = prepare(&[user("new")], &session, Some(&callback), None).await;
    assert_eq!(appended, ["user:history", "user:new"]);
}

#[tokio::test]
async fn new_input_never_shares_an_identity_with_history() {
    // A host that reuses one run id across runs of a session would otherwise mint the same
    // identity twice, and the callback reconciliation would take the new item for history.
    let earlier = InMemorySession::new("s");
    let first = prepare_input_with_session(
        &RunId::new("run-reused"),
        &[user("same")],
        &earlier,
        None,
        None,
    )
    .await
    .unwrap();
    earlier
        .add_items(first.append_for_turn().to_vec())
        .await
        .unwrap();

    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(history.iter().chain(new_input.iter()).cloned().collect())
    };
    let second = prepare_input_with_session(
        &RunId::new("run-reused"),
        &[user("same")],
        &earlier,
        Some(&callback),
        None,
    )
    .await
    .unwrap();
    assert_eq!(describe_items(second.append_for_turn()), ["user:same"]);
    assert_ne!(
        second.append_for_turn()[0].id(),
        first.append_for_turn()[0].id()
    );
}

#[tokio::test]
async fn the_run_override_wins_and_unset_values_keep_the_sessions_settings() {
    let items = (0..4)
        .map(|index| stored_user(&format!("h{index}"), &format!("m{index}")))
        .collect();
    let session = InMemorySession::new_with_items("s", items)
        .with_session_settings(SessionSettings::new().with_limit(3));

    let (session_default, _) = prepare(&[user("new")], &session, None, None).await;
    assert_eq!(
        session_default,
        ["user:m1", "user:m2", "user:m3", "user:new"]
    );

    let (unset_override, _) = prepare(
        &[user("new")],
        &session,
        None,
        Some(&SessionSettings::new()),
    )
    .await;
    assert_eq!(unset_override, session_default);

    let one = SessionSettings::new().with_limit(1);
    let (overridden, _) = prepare(&[user("new")], &session, None, Some(&one)).await;
    assert_eq!(overridden, ["user:m3", "user:new"]);

    let zero = SessionSettings::new().with_limit(0);
    let (none, _) = prepare(&[user("new")], &session, None, Some(&zero)).await;
    assert_eq!(none, ["user:new"]);
}

// ---------------------------------------------------------------------------------------------
// Running with a session
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_second_run_starts_from_the_first_runs_history() {
    for streamed in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        let session: Arc<dyn Session> = Arc::new(InMemorySession::new("golden-gate"));
        let setup = Setup::default();

        model.enqueue(answer("m1", "San Francisco"));
        let request = setup.request(
            &model,
            "run-1",
            vec![user("What city is the Golden Gate Bridge in?")],
            Arc::clone(&session),
        );
        let first = if streamed {
            run_streamed(request).await
        } else {
            Runner::run(request).await
        }
        .unwrap();
        assert_eq!(first.final_text(), "San Francisco");

        model.enqueue(answer("m2", "California"));
        let request = setup.request(
            &model,
            "run-2",
            vec![user("What state is it in?")],
            Arc::clone(&session),
        );
        let second = if streamed {
            run_streamed(request).await
        } else {
            Runner::run(request).await
        }
        .unwrap();
        assert_eq!(second.final_text(), "California");

        assert_eq!(
            describe_input(&model.last_input()),
            [
                "user:What city is the Golden Gate Bridge in?",
                "assistant:San Francisco",
                "user:What state is it in?"
            ],
            "streamed: {streamed}"
        );
        assert_eq!(
            stored(session.as_ref()).await,
            [
                "user:What city is the Golden Gate Bridge in?",
                "assistant:San Francisco",
                "user:What state is it in?",
                "assistant:California"
            ]
        );
    }
}

#[tokio::test]
async fn separate_sessions_keep_separate_histories() {
    let model = Arc::new(ScriptedModel::default());
    let cats: Arc<dyn Session> = Arc::new(InMemorySession::new("session-1"));
    let dogs: Arc<dyn Session> = Arc::new(InMemorySession::new("session-2"));
    let setup = Setup::default();

    model.enqueue(answer("m1", "I like cats"));
    Runner::run(setup.request(
        &model,
        "run-1",
        vec![user("I like cats")],
        Arc::clone(&cats),
    ))
    .await
    .unwrap();
    model.enqueue(answer("m2", "I like dogs"));
    Runner::run(setup.request(
        &model,
        "run-2",
        vec![user("I like dogs")],
        Arc::clone(&dogs),
    ))
    .await
    .unwrap();
    model.enqueue(answer("m3", "Yes, you mentioned cats"));
    Runner::run(setup.request(
        &model,
        "run-3",
        vec![user("What did I say I like?")],
        Arc::clone(&cats),
    ))
    .await
    .unwrap();

    assert_eq!(
        describe_input(&model.last_input()),
        [
            "user:I like cats",
            "assistant:I like cats",
            "user:What did I say I like?"
        ]
    );
}

#[tokio::test]
async fn list_input_is_appended_after_the_stored_history() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new_with_items(
        "s",
        vec![
            stored_user("h1", "Earlier message"),
            assistant("h2", "Saved reply"),
        ],
    ));
    model.enqueue(answer("m1", "This should run"));
    Runner::run(Setup::default().request(
        &model,
        "run-1",
        vec![user("Test message")],
        Arc::clone(&session),
    ))
    .await
    .unwrap();

    assert_eq!(
        describe_input(&model.last_input()),
        [
            "user:Earlier message",
            "assistant:Saved reply",
            "user:Test message"
        ]
    );
}

#[tokio::test]
async fn a_callback_shapes_the_model_input_and_only_the_new_turn_is_stored() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new_with_items(
        "s",
        vec![
            stored_user("h1", "Hello there."),
            assistant("h2", "Hi, I'm here to assist you."),
        ],
    ));
    let only_user_history = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(history
            .iter()
            .filter(|item| {
                matches!(item.kind(), RunItemKind::Message(message) if message.role() == MessageRole::User)
            })
            .chain(new_input.iter())
            .cloned()
            .collect())
    };
    model.enqueue(answer("m1", "I'm a model"));
    Runner::run(Setup::default().callback(only_user_history).request(
        &model,
        "run-1",
        vec![user("What your name?")],
        Arc::clone(&session),
    ))
    .await
    .unwrap();

    assert_eq!(
        describe_input(&model.last_input()),
        ["user:Hello there.", "user:What your name?"]
    );
    assert_eq!(
        stored(session.as_ref()).await,
        [
            "user:Hello there.",
            "assistant:Hi, I'm here to assist you.",
            "user:What your name?",
            "assistant:I'm a model"
        ]
    );
}

#[tokio::test]
async fn a_callback_repeating_history_does_not_grow_the_session() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new("session-repeat"));
    let repeat_first = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        if history.is_empty() {
            return Ok(new_input.clone());
        }
        let mut combined = history.clone();
        combined.push(history[0].clone());
        combined.extend(new_input.iter().cloned());
        Ok(combined)
    };
    let setup = Setup::default().callback(repeat_first);
    for turn in 0..3 {
        model.enqueue(answer(&format!("m{turn}"), &format!("assistant {turn}")));
        Runner::run(setup.request(
            &model,
            &format!("run-{turn}"),
            vec![user(&format!("user {turn}"))],
            Arc::clone(&session),
        ))
        .await
        .unwrap();
    }

    assert_eq!(
        stored(session.as_ref()).await,
        [
            "user:user 0",
            "assistant:assistant 0",
            "user:user 1",
            "assistant:assistant 1",
            "user:user 2",
            "assistant:assistant 2"
        ]
    );
}

#[tokio::test]
async fn a_session_limit_bounds_the_history_a_run_reads() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new_with_items(
        "s",
        vec![
            stored_user("h1", "one"),
            assistant("h2", "two"),
            stored_user("h3", "three"),
            assistant("h4", "four"),
        ],
    ));
    model.enqueue(answer("m1", "five"));
    Runner::run(
        Setup::default()
            .settings(SessionSettings::new().with_limit(2))
            .request(&model, "run-1", vec![user("new")], Arc::clone(&session)),
    )
    .await
    .unwrap();

    assert_eq!(
        describe_input(&model.last_input()),
        ["user:three", "assistant:four", "user:new"]
    );
    assert_eq!(
        session.get_items(None).await.unwrap().len(),
        6,
        "the limit bounds the read, not the store"
    );
}

/// The reference's `test_session_add_items_called_multiple_times_for_multi_turn_completion`: the
/// input before the first call, each turn that continues, and the final answer, in that order.
#[tokio::test]
async fn each_settled_turn_is_appended_once_in_order() {
    for streamed in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        let session = CountingSession::new("multi-turn");
        let tool = ScriptedTool::new("lookup", ToolOptions::new());
        model.enqueue(ModelResponse::new(vec![call("c1", "call-1", "lookup")]));
        model.enqueue(answer("m1", "done"));

        let request = Setup::default().tool(tool).request(
            &model,
            "run-1",
            vec![user("look it up")],
            Arc::clone(&session) as Arc<dyn Session>,
        );
        let result = if streamed {
            run_streamed(request).await
        } else {
            Runner::run(request).await
        }
        .unwrap();

        assert_eq!(result.final_text(), "done");
        assert_eq!(session.append_sizes(), [1, 2, 1], "streamed: {streamed}");
        assert_eq!(
            stored(session.as_ref()).await,
            [
                "user:look it up",
                "tool_call:call-1",
                "tool_call_output:call-1",
                "assistant:done"
            ]
        );
        assert_eq!(result.state().session_persisted_item_count(), Some(3));
    }
}

#[tokio::test]
async fn a_tripped_input_guardrail_leaves_only_the_input_in_the_session() {
    for parallel in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        let session: Arc<dyn Session> = Arc::new(InMemorySession::new("guarded"));
        model.enqueue(answer("m1", "should_not_be_saved"));
        let error = Runner::run(
            Setup::default()
                .input_guardrail(InputCheck {
                    tripwire: true,
                    parallel,
                })
                .request(
                    &model,
                    "run-1",
                    vec![user("user_message")],
                    Arc::clone(&session),
                ),
        )
        .await
        .expect_err("a tripped input guardrail stops the run");
        assert!(error.guardrail_evidence().is_some());

        assert_eq!(
            stored(session.as_ref()).await,
            ["user:user_message"],
            "parallel: {parallel}"
        );
    }
}

#[tokio::test]
async fn a_tripped_output_guardrail_keeps_the_refused_answer_out_of_the_session() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new("guarded"));
    model.enqueue(answer("m1", "should_not_be_saved"));
    Runner::run(
        Setup::default()
            .output_guardrail(OutputVerdict::Trip)
            .request(
                &model,
                "run-1",
                vec![user("user_message")],
                Arc::clone(&session),
            ),
    )
    .await
    .expect_err("a tripped output guardrail stops the run");

    assert_eq!(stored(session.as_ref()).await, ["user:user_message"]);
}

#[tokio::test]
async fn an_output_guardrail_that_fails_keeps_the_answer_in_the_session() {
    for streamed in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        let session: Arc<dyn Session> = Arc::new(InMemorySession::new("guarded"));
        model.enqueue(answer("m1", "preserved_on_guardrail_error"));
        let request = Setup::default()
            .output_guardrail(OutputVerdict::Fail)
            .request(
                &model,
                "run-1",
                vec![user("user_message")],
                Arc::clone(&session),
            );
        let error = if streamed {
            run_streamed(request).await
        } else {
            Runner::run(request).await
        }
        .expect_err("a failing output guardrail fails the run");
        assert!(error.to_string().contains("guardrail failed"));

        assert_eq!(
            stored(session.as_ref()).await,
            [
                "user:user_message",
                "assistant:preserved_on_guardrail_error"
            ],
            "streamed: {streamed}"
        );
    }
}

/// The reference's `test_resumed_approval_does_not_duplicate_session_items`.
#[tokio::test]
async fn a_resumed_approval_appends_each_record_once() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new("approval"));
    let tool = ScriptedTool::new(
        "test_tool",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default().tool(tool.clone());
    model.enqueue(ModelResponse::new(vec![call(
        "c1",
        "call-resume",
        "test_tool",
    )]));
    model.enqueue(answer("m1", "done"));

    let first = Runner::run(setup.request(
        &model,
        "run-1",
        vec![user("Use test_tool")],
        Arc::clone(&session),
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(
        stored(session.as_ref()).await,
        ["user:Use test_tool", "tool_call:call-resume"],
        "the paused turn is in the session, without its approval record"
    );

    let mut state = first.state().clone();
    state.approve(&items[0], false).unwrap();
    let resumed = Runner::run(
        setup
            .request(&model, "run-1", Vec::new(), Arc::clone(&session))
            .with_state(state),
    )
    .await
    .unwrap();
    assert_eq!(resumed.final_text(), "done");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        stored(session.as_ref()).await,
        [
            "user:Use test_tool",
            "tool_call:call-resume",
            "tool_call_output:call-resume",
            "assistant:done"
        ]
    );
}

/// The reference's `_should_defer_interrupted_session_items`: with an output guardrail and a tool
/// result that can end the run, the paused turn waits for the verdict on what it produces.
#[tokio::test]
async fn a_paused_turn_whose_tool_may_end_the_run_waits_for_the_output_guardrail() {
    for verdict in [OutputVerdict::Pass, OutputVerdict::Trip] {
        let tripped = matches!(verdict, OutputVerdict::Trip);
        let model = Arc::new(ScriptedModel::default());
        let session: Arc<dyn Session> = Arc::new(InMemorySession::new("deferred"));
        let tool = ScriptedTool::new(
            "commit_tool",
            ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
        );
        let setup = Setup::default()
            .tool(tool.clone())
            .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
            .output_guardrail(verdict);
        model.enqueue(ModelResponse::new(vec![call(
            "c1",
            "call-first",
            "commit_tool",
        )]));

        let first = Runner::run(setup.request(
            &model,
            "run-1",
            vec![user("user_message")],
            Arc::clone(&session),
        ))
        .await
        .unwrap();
        let RunOutcome::Interrupted { items } = first.outcome() else {
            panic!("expected an approval interruption");
        };
        assert_eq!(stored(session.as_ref()).await, ["user:user_message"]);

        let mut state = first.state().clone();
        state.approve(&items[0], false).unwrap();
        let resumed = Runner::run(
            setup
                .request(&model, "run-1", Vec::new(), Arc::clone(&session))
                .with_state(state),
        )
        .await;
        assert_eq!(resumed.is_err(), tripped);

        let expected: &[&str] = if tripped {
            &["user:user_message"]
        } else {
            &[
                "user:user_message",
                "tool_call:call-first",
                "tool_call_output:call-first",
            ]
        };
        assert_eq!(
            stored(session.as_ref()).await,
            expected,
            "tripped: {tripped}"
        );
    }
}

#[tokio::test]
async fn a_continuation_with_a_session_refuses_input_of_its_own() {
    let model = Arc::new(ScriptedModel::default());
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new("refuse"));
    let tool = ScriptedTool::new(
        "test_tool",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default().tool(tool);
    model.enqueue(ModelResponse::new(vec![call("c1", "call-1", "test_tool")]));
    let first = Runner::run(setup.request(&model, "run-1", vec![user("go")], Arc::clone(&session)))
        .await
        .unwrap();

    let error = Runner::run(
        setup
            .request(
                &model,
                "run-1",
                vec![user("and more")],
                Arc::clone(&session),
            )
            .with_state(first.state().clone()),
    )
    .await
    .expect_err("the session already holds the conversation that input would project");
    assert!(error.to_string().contains("pass no input"));
}

/// A run bound to a session only when it resumes owes the session nothing it generated before.
#[tokio::test]
async fn a_run_first_resumed_with_a_session_appends_only_what_follows() {
    let model = Arc::new(ScriptedModel::default());
    let tool = ScriptedTool::new(
        "test_tool",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default().tool(tool);
    model.enqueue(ModelResponse::new(vec![call("c1", "call-1", "test_tool")]));
    model.enqueue(answer("m1", "done"));

    let agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .instructions("help")
        .tools(setup.tools.clone())
        .build()
        .unwrap();
    let first = Runner::run(RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(&model))),
        RunId::new("run-1"),
        CancelScope::root(),
        vec![user("go")],
    ))
    .await
    .unwrap();
    assert_eq!(first.state().session_persisted_item_count(), None);
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval interruption");
    };

    let session: Arc<dyn Session> = Arc::new(InMemorySession::new("late"));
    let mut state: RunState = first.state().clone();
    state.approve(&items[0], false).unwrap();
    Runner::run(
        setup
            .request(&model, "run-1", Vec::new(), Arc::clone(&session))
            .with_state(state),
    )
    .await
    .unwrap();

    assert_eq!(
        stored(session.as_ref()).await,
        ["tool_call_output:call-1", "assistant:done"]
    );
}

#[tokio::test]
async fn a_failing_session_append_fails_the_run() {
    for streamed in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        model.enqueue(answer("m1", "unreached"));
        let request = Setup::default().request(
            &model,
            "run-1",
            vec![user("hello")],
            CountingSession::failing("broken") as Arc<dyn Session>,
        );
        let error = if streamed {
            run_streamed(request).await
        } else {
            Runner::run(request).await
        }
        .expect_err("the session's error is the run's");
        assert!(error.to_string().contains("session store is unavailable"));
        assert!(
            model.requests.lock().unwrap().is_empty(),
            "the input is appended before the first model call"
        );
    }
}

/// Controls failures at the Session boundary, including successful writes with lost replies.
#[derive(Clone, Copy)]
enum AppendFailure {
    Before,
    After,
    Partial,
}

struct RecoverableSession {
    inner: InMemorySession,
    failure: Mutex<Option<AppendFailure>>,
    fail_only_outputs: std::sync::atomic::AtomicBool,
    block: std::sync::atomic::AtomicBool,
    commit_before_block: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
}

impl RecoverableSession {
    fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemorySession::new(id),
            failure: Mutex::new(None),
            fail_only_outputs: std::sync::atomic::AtomicBool::new(false),
            block: std::sync::atomic::AtomicBool::new(false),
            commit_before_block: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
        })
    }

    fn fail_next(&self, failure: AppendFailure) {
        *self.failure.lock().unwrap() = Some(failure);
    }

    fn block_next(&self, committed: bool) {
        self.commit_before_block.store(committed, Ordering::SeqCst);
        self.block.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Session for RecoverableSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.inner.get_items(limit).await
    }
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        let failure = if self.fail_only_outputs.load(Ordering::SeqCst)
            && !items
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
        {
            None
        } else {
            self.failure.lock().unwrap().take()
        };
        if matches!(failure, Some(AppendFailure::Before)) {
            return Err(Error::session(
                ra_core::error::SessionErrorKind::Io,
                "session append failed",
            ));
        }
        if self.block.swap(false, Ordering::SeqCst) {
            if self.commit_before_block.load(Ordering::SeqCst) {
                self.inner.add_items(items).await?;
            }
            self.entered.notify_one();
            return std::future::pending().await;
        }
        if matches!(failure, Some(AppendFailure::Partial)) {
            self.inner.add_items(items[..1].to_vec()).await?;
        } else {
            self.inner.add_items(items).await?;
        }
        if failure.is_some() {
            return Err(Error::session(
                ra_core::error::SessionErrorKind::Io,
                "session append failed",
            ));
        }
        Ok(())
    }
    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.inner.pop_item().await
    }
    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

fn round_trip(state: &RunState) -> RunState {
    serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap()
}

async fn run_mode(request: RunRequest, streamed: bool) -> Result<RunResult> {
    if streamed {
        run_streamed(request).await
    } else {
        Runner::run(request).await
    }
}

async fn approved_run(
    setup: &Setup,
    model: &Arc<ScriptedModel>,
    session: Arc<dyn Session>,
    streamed: bool,
) -> RunState {
    model.enqueue(ModelResponse::new(vec![call("c1", "charge-1", "charge")]));
    let paused = run_mode(
        setup.request(model, "recover", vec![user("charge")], session),
        streamed,
    )
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = paused.outcome() else {
        panic!("expected approval");
    };
    let mut state = round_trip(paused.state());
    state.approve(&items[0], false).unwrap();
    state
}

/// Ported from `test_resumed_session_append_is_recovered_before_next_model`.
#[tokio::test]
async fn failed_resumed_appends_reconcile_before_model_without_repeating_tools() {
    for failure in [AppendFailure::Before, AppendFailure::After] {
        for streamed in [false, true] {
            let model = Arc::new(ScriptedModel::default());
            let session = RecoverableSession::new("resumed-write");
            let tool = ScriptedTool::new(
                "charge",
                ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
            );
            let setup = Setup::default().tool(tool.clone());
            let state = approved_run(&setup, &model, session.clone(), streamed).await;
            model.enqueue(answer("m1", "done"));
            session.fail_next(failure);
            let error = run_mode(
                setup
                    .request(&model, "recover", Vec::new(), session.clone())
                    .with_state(state),
                streamed,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code(), "session.io");
            assert!(error.is_retryable());
            assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                model.requests.lock().unwrap().len(),
                1,
                "append must settle before the next model call"
            );
            let checkpoint = round_trip(
                error
                    .run_state()
                    .expect("the owned state must leave with the error"),
            );
            assert!(checkpoint.pending_session_write().is_some());
            let result = run_mode(
                setup
                    .request(&model, "recover", Vec::new(), session.clone())
                    .with_state(checkpoint),
                !streamed,
            )
            .await
            .unwrap();
            assert_eq!(result.final_text(), "done");
            assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
            assert!(result.state().pending_session_write().is_none());
            assert_eq!(
                stored(session.as_ref()).await,
                [
                    "user:charge",
                    "tool_call:charge-1",
                    "tool_call_output:charge-1",
                    "assistant:done"
                ]
            );
        }
    }
}

/// Ported from `test_resumed_session_append_rejects_ambiguous_recovery`.
#[tokio::test]
async fn a_pending_write_rejects_missing_or_changed_sessions_before_work() {
    let model = Arc::new(ScriptedModel::default());
    let session = RecoverableSession::new("original");
    let tool = ScriptedTool::new(
        "charge",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default().tool(tool.clone());
    let state = approved_run(&setup, &model, session.clone(), false).await;
    session.fail_next(AppendFailure::Before);
    let error = Runner::run(
        setup
            .request(&model, "recover", Vec::new(), session.clone())
            .with_state(state),
    )
    .await
    .unwrap_err();
    let checkpoint = round_trip(error.run_state().unwrap());
    let other = Arc::new(InMemorySession::new("other"));
    Runner::run(
        setup
            .request(&model, "recover", Vec::new(), other)
            .with_state(checkpoint.clone()),
    )
    .await
    .unwrap_err();
    let agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .tool(tool.clone())
        .build()
        .unwrap();
    Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(model.clone())),
            RunId::new("recover"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_state(checkpoint.clone()),
    )
    .await
    .unwrap_err();
    session
        .inner
        .add_items(vec![stored_user("foreign", "another writer")])
        .await
        .unwrap();
    let before = session.get_items(None).await.unwrap();
    let error = Runner::run(
        setup
            .request(&model, "recover", Vec::new(), session.clone())
            .with_state(checkpoint),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("ambiguous"));
    assert_eq!(session.get_items(None).await.unwrap(), before);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_partial_append_is_not_replayed_or_repaired() {
    let model = Arc::new(ScriptedModel::default());
    let session = RecoverableSession::new("partial");
    let tool = ScriptedTool::new("charge", ToolOptions::new());
    let setup = Setup::default().tool(tool.clone());
    model.enqueue(ModelResponse::new(vec![call("c1", "charge-1", "charge")]));
    // An empty opening input makes the call/output pair the first appended batch.
    session.fail_next(AppendFailure::Partial);
    let error = Runner::run(setup.request(&model, "recover", Vec::new(), session.clone()))
        .await
        .unwrap_err();
    let checkpoint = round_trip(error.run_state().unwrap());
    let before = session.get_items(None).await.unwrap();
    let error = Runner::run(
        setup
            .request(&model, "recover", Vec::new(), session.clone())
            .with_state(checkpoint),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("ambiguous"));
    assert_eq!(session.get_items(None).await.unwrap(), before);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

/// Ported from `test_terminal_session_append_failure_rejects_every_later_resume`.
#[tokio::test]
async fn a_failed_terminal_append_cannot_replay_completed_work() {
    for failure in [AppendFailure::Before, AppendFailure::After] {
        let model = Arc::new(ScriptedModel::default());
        let session = RecoverableSession::new("terminal");
        let tool = ScriptedTool::new(
            "charge",
            ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
        );
        let setup = Setup::default()
            .tool(tool.clone())
            .tool_use_behavior(ToolUseBehavior::StopOnFirstTool);
        let state = approved_run(&setup, &model, session.clone(), false).await;
        session.fail_next(failure);
        let error = Runner::run(
            setup
                .request(&model, "recover", Vec::new(), session.clone())
                .with_state(state),
        )
        .await
        .unwrap_err();
        let checkpoint = round_trip(error.run_state().unwrap());
        assert!(checkpoint.terminal_unrecoverable());
        let before = session.get_items(None).await.unwrap();
        let error = run_streamed(
            setup
                .request(&model, "recover", Vec::new(), session.clone())
                .with_state(checkpoint),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cannot be resumed"));
        assert_eq!(session.get_items(None).await.unwrap(), before);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancellation_during_input_append_retains_an_uncertain_write() {
    for committed in [false, true] {
        let model = Arc::new(ScriptedModel::default());
        model.enqueue(answer("m1", "done"));
        let session = RecoverableSession::new("cancelled-input");
        session.block_next(committed);
        let cancel = CancelScope::root();
        let agent = AgentSpec::builder()
            .id(AgentId::new("assistant"))
            .name("Assistant")
            .build()
            .unwrap();
        let request = RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(model.clone())),
            RunId::new("recover"),
            cancel.clone(),
            vec![user("hello")],
        )
        .with_session(session.clone());
        let task = tokio::spawn(Runner::run(request));
        session.entered.notified().await;
        cancel.cancel(ra_core::cancel::CancelReason::UserInterrupt);
        let error = tokio::time::timeout(std::time::Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.is_cancelled());
        let checkpoint = round_trip(error.run_state().unwrap());
        assert!(
            checkpoint
                .pending_session_write()
                .unwrap()
                .before()
                .is_some()
        );
        assert!(model.requests.lock().unwrap().is_empty());
        let result = Runner::run(
            Setup::default()
                .request(&model, "recover", Vec::new(), session.clone())
                .with_state(checkpoint),
        )
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(
            stored(session.as_ref()).await,
            ["user:hello", "assistant:done"]
        );
    }
}

#[tokio::test]
async fn a_deadline_interrupts_a_pending_session_append() {
    let model = Arc::new(ScriptedModel::default());
    let session = RecoverableSession::new("deadline");
    session.block_next(false);
    let request = Setup::default()
        .request(&model, "recover", vec![user("hello")], session)
        .with_config(
            RunConfig::new().with_deadline(ra_core::cancel::Deadline::after(
                std::time::Duration::from_millis(20),
            )),
        );
    let error = tokio::time::timeout(std::time::Duration::from_millis(500), Runner::run(request))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is_cancelled());
    assert!(error.run_state().unwrap().pending_session_write().is_some());
}

#[tokio::test]
async fn repeated_append_failures_keep_the_completed_tool_checkpoint() {
    let model = Arc::new(ScriptedModel::default());
    let session = RecoverableSession::new("repeated");
    let tool = ScriptedTool::new(
        "charge",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default().tool(tool.clone());
    let mut state = approved_run(&setup, &model, session.clone(), false).await;
    model.enqueue(answer("m1", "done"));
    for failure in [
        AppendFailure::Before,
        AppendFailure::Before,
        AppendFailure::After,
    ] {
        session.fail_next(failure);
        let error = Runner::run(
            setup
                .request(&model, "recover", Vec::new(), session.clone())
                .with_state(state),
        )
        .await
        .unwrap_err();
        state = round_trip(error.run_state().unwrap());
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
    let result = Runner::run(
        setup
            .request(&model, "recover", Vec::new(), session.clone())
            .with_state(state),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_deadline_during_terminal_write_prevents_replaying_terminal_work() {
    let model = Arc::new(ScriptedModel::default());
    let session = RecoverableSession::new("bounded-closeout");
    let tool = ScriptedTool::new(
        "charge",
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
    );
    let setup = Setup::default()
        .tool(tool.clone())
        .tool_use_behavior(ToolUseBehavior::StopOnFirstTool);
    let state = approved_run(&setup, &model, session.clone(), false).await;
    session.block_next(false);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        Runner::run(
            setup
                .request(&model, "recover", Vec::new(), session)
                .with_state(state)
                .with_config(
                    RunConfig::new().with_deadline(ra_core::cancel::Deadline::after(
                        std::time::Duration::from_millis(20),
                    )),
                ),
        ),
    )
    .await
    .expect("the closeout must be bounded")
    .unwrap_err();
    assert!(error.is_cancelled());
    assert!(error.run_state().unwrap().terminal_unrecoverable());
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_childs_failed_session_append_resumes_through_its_parent_checkpoint() {
    use ra_runtime::agent::tool::{AgentAsTool, AgentToolStreamEvent};
    for approval in [false, true] {
        for failure in [AppendFailure::Before, AppendFailure::After] {
            let model = Arc::new(ScriptedModel::default());
            let child_session = RecoverableSession::new("child-recovery");
            let parent_session: Arc<dyn Session> =
                Arc::new(InMemorySession::new("parent-recovery"));
            let options = if approval {
                ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
            } else {
                ToolOptions::new()
            };
            let tool = ScriptedTool::new("charge", options);
            let child = AgentSpec::builder()
                .id(AgentId::new("child"))
                .name("Child")
                .tool(tool.clone())
                .build()
                .unwrap();
            let child_tool = child
                .as_tool()
                .session(child_session.clone())
                .on_stream(Arc::new(|_: AgentToolStreamEvent| Ok(())))
                .build()
                .unwrap();
            let setup = Setup::default().tool(Arc::new(child_tool));
            model.enqueue(ModelResponse::new(vec![RunItem::new(
                ItemId::new("outer"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("outer-1"),
                    "child",
                    json!({"input": "charge"}),
                )),
            )]));
            model.enqueue(ModelResponse::new(vec![call(
                "inner", "charge-1", "charge",
            )]));
            model.enqueue(answer("child-answer", "child done"));
            model.enqueue(answer("parent-answer", "parent done"));
            let state = if approval {
                let first = Runner::run(setup.request(
                    &model,
                    "parent",
                    vec![user("delegate")],
                    parent_session.clone(),
                ))
                .await
                .unwrap();
                let RunOutcome::Interrupted { items } = first.outcome() else {
                    panic!("expected child approval");
                };
                let mut state = round_trip(first.state());
                state.approve(&items[0], false).unwrap();
                state
            } else {
                RunState::start(RunId::new("parent"))
            };
            if approval {
                child_session.fail_next(failure);
                let error = Runner::run(
                    setup
                        .request(&model, "parent", Vec::new(), parent_session.clone())
                        .with_state(state),
                )
                .await
                .unwrap_err();
                let checkpoint = round_trip(error.run_state().unwrap());
                assert_eq!(checkpoint.run_id().as_str(), "parent");
                assert!(
                    checkpoint.nested_runs()[0]
                        .state()
                        .unwrap()
                        .pending_session_write()
                        .is_some()
                );
                assert_eq!(model.requests.lock().unwrap().len(), 2);
                let result = run_streamed(
                    setup
                        .request(&model, "parent", Vec::new(), parent_session.clone())
                        .with_state(checkpoint),
                )
                .await
                .unwrap();
                assert_eq!(result.final_text(), "parent done");
                assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    stored(child_session.as_ref()).await,
                    [
                        "user:charge",
                        "tool_call:charge-1",
                        "tool_call_output:charge-1",
                        "assistant:child done"
                    ]
                );
            } else {
                // Let the input write succeed and fail after the child tool has executed.
                child_session
                    .fail_only_outputs
                    .store(true, Ordering::SeqCst);
                child_session.fail_next(failure);
                let error = Runner::run(
                    setup
                        .request(
                            &model,
                            "parent",
                            vec![user("delegate")],
                            parent_session.clone(),
                        )
                        .with_state(state),
                )
                .await
                .unwrap_err();
                let checkpoint = round_trip(error.run_state().unwrap());
                assert_eq!(checkpoint.run_id().as_str(), "parent");
                let result = Runner::run(
                    setup
                        .request(&model, "parent", Vec::new(), parent_session.clone())
                        .with_state(checkpoint),
                )
                .await
                .unwrap();
                assert_eq!(result.final_text(), "parent done");
                assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
            }
        }
    }
}

#[tokio::test]
async fn a_child_session_failure_retains_completed_sibling_outputs() {
    use ra_runtime::agent::tool::AgentAsTool;

    let model = Arc::new(ScriptedModel::default());
    let child_session = RecoverableSession::new("child-sibling-recovery");
    child_session
        .fail_only_outputs
        .store(true, Ordering::SeqCst);
    child_session.fail_next(AppendFailure::Before);
    let parent_session: Arc<dyn Session> = Arc::new(InMemorySession::new("parent-siblings"));
    let sibling = ScriptedTool::new("lookup", ToolOptions::new());
    let charge = ScriptedTool::new("charge", ToolOptions::new());
    let child = AgentSpec::builder()
        .id(AgentId::new("child"))
        .name("Child")
        .tool(charge.clone())
        .build()
        .unwrap();
    let child_tool = child.as_tool().session(child_session).build().unwrap();
    let setup = Setup::default()
        .tool(sibling.clone())
        .tool(Arc::new(child_tool));
    model.enqueue(ModelResponse::new(vec![
        call("lookup-call", "lookup-1", "lookup"),
        RunItem::new(
            ItemId::new("outer"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("outer-1"),
                "child",
                json!({"input": "charge"}),
            )),
        ),
    ]));
    model.enqueue(ModelResponse::new(vec![call(
        "inner", "charge-1", "charge",
    )]));
    model.enqueue(answer("child-answer", "child done"));
    model.enqueue(answer("parent-answer", "parent done"));
    let error = Runner::run(setup.request(
        &model,
        "parent",
        vec![user("delegate")],
        parent_session.clone(),
    ))
    .await
    .unwrap_err();
    let checkpoint = round_trip(error.run_state().unwrap());
    assert!(checkpoint.generated_items().iter().any(|item| matches!(
        item.kind(), RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "lookup-1"
    )));
    let result = Runner::run(
        setup
            .request(&model, "parent", Vec::new(), parent_session.clone())
            .with_state(checkpoint),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "parent done");
    assert_eq!(sibling.calls.load(Ordering::SeqCst), 1);
    assert_eq!(charge.calls.load(Ordering::SeqCst), 1);
    assert!(model.last_input().iter().any(|item| matches!(
        item, ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == "lookup-1"
    )));
    assert_eq!(
        stored(parent_session.as_ref()).await,
        [
            "user:delegate",
            "tool_call:lookup-1",
            "tool_call:outer-1",
            "tool_call_output:lookup-1",
            "tool_call_output:outer-1",
            "assistant:parent done"
        ]
    );
}

#[tokio::test]
async fn ignoring_append_identities_does_not_change_generic_callback_matching() {
    let session = CountingSession {
        inner: InMemorySession::new_with_items(
            "remote",
            vec![RunItem::new(
                ItemId::new("old-id"),
                RunItemKind::Message(Message::user("history")),
            )],
        ),
        appends: Mutex::new(Vec::new()),
        fail_appends: false,
        ignore_ids: true,
    };
    let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
        Ok(vec![
            RunItem::new(ItemId::new("rebuilt"), history[0].kind().clone()),
            new_input[0].clone(),
        ])
    };
    let plan = prepare_input_with_session(
        &RunId::new("run"),
        &[user("new")],
        &session,
        Some(&callback),
        None,
    )
    .await
    .unwrap();
    assert_eq!(plan.prepared_for_model(), &[user("history"), user("new")]);
    assert_eq!(plan.append_for_turn().len(), 1);
    assert_eq!(
        plan.append_for_turn()[0].to_model_input(),
        Some(user("new"))
    );
}
