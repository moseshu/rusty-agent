//! What the agent loop puts into its spans, and what it must keep out of them.
//!
//! # Why this is its own test binary
//!
//! These cases assert on captured span output, which means they need a subscriber. A subscriber
//! installed with `set_default` is thread-local, but **`tracing`'s callsite interest cache is
//! process-global and computed lazily**: the first time a span macro is reached anywhere in the
//! process, its interest is decided against whichever dispatchers are alive at that instant, and a
//! callsite first reached while none is alive caches as "never". Sharing a binary with the rest of
//! the runner cases — which drive the same span callsites on other threads with no subscriber —
//! therefore loses a race often enough to matter: the capture comes back empty or missing whole
//! span kinds. Cargo gives each test file its own process, so keeping these here means every
//! callsite in this binary is first reached while one of these two subscribers is installed.
//!
//! **Adding a case that runs the runtime without installing a subscriber reopens that race.**
//!
//! # Two views of the same run
//!
//! Assertions on numbers read [`SpanCollector`], which keeps each span's fields as values. The
//! assertions that nothing sensitive leaked read the formatted text instead, because a leak can
//! also arrive through an event message, which never appears in a span's field set.

use std::{
    collections::HashMap,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result, ToolErrorKind},
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ProviderKey, ResolvedModel,
    },
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
    usage::Usage,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::json;
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    subscriber::DefaultGuard,
};
use tracing_subscriber::{
    Layer,
    fmt::{MakeWriter, format::FmtSpan},
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

/// One closed span: its name, its parent's name, and the fields as they stood at close.
#[derive(Clone, Debug)]
struct SpanRecord {
    name: String,
    parent: Option<String>,
    fields: HashMap<String, String>,
}

impl SpanRecord {
    fn kind(&self) -> Option<&str> {
        self.field("span.kind")
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    /// A numeric field, failing the test rather than defaulting when it is missing: an absent
    /// count and a zero count mean very different things about the instrumentation.
    fn number(&self, name: &str) -> u64 {
        self.field(name)
            .unwrap_or_else(|| panic!("span `{}` has no `{name}`: {:?}", self.name, self.fields))
            .parse()
            .unwrap_or_else(|_| panic!("`{name}` is not a number: {:?}", self.field(name)))
    }
}

/// Field values of every span, recorded when it closes.
#[derive(Clone, Default)]
struct SpanCollector(Arc<Mutex<Vec<SpanRecord>>>);

impl SpanCollector {
    fn all(&self) -> Vec<SpanRecord> {
        self.0.lock().unwrap().clone()
    }

    fn of_kind(&self, kind: &str) -> Vec<SpanRecord> {
        self.all()
            .into_iter()
            .filter(|span| span.kind() == Some(kind))
            .collect()
    }

    /// The single span of a kind, failing when the run produced a different number of them.
    fn only(&self, kind: &str) -> SpanRecord {
        let mut spans = self.of_kind(kind);
        assert_eq!(
            spans.len(),
            1,
            "expected exactly one `{kind}` span, got {spans:?}"
        );
        spans.remove(0)
    }
}

/// Span fields as strings. Types are flattened deliberately: the assertions here care what value
/// was recorded under a name, not which `tracing` primitive carried it.
struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

struct SpanFields(HashMap<String, String>);

impl<S> Layer<S> for SpanCollector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(SpanFields(fields)) = span.extensions_mut().get_mut::<SpanFields>()
        {
            values.record(&mut FieldVisitor(fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let fields = span
            .extensions()
            .get::<SpanFields>()
            .map(|stored| stored.0.clone())
            .unwrap_or_default();
        self.0.lock().unwrap().push(SpanRecord {
            name: span.name().to_owned(),
            parent: span.parent().map(|parent| parent.name().to_owned()),
            fields,
        });
    }
}

/// Formatted output, for asserting on what never appears anywhere in the log.
#[derive(Clone, Default)]
struct TextCapture(Arc<Mutex<Vec<u8>>>);

impl TextCapture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for TextCapture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for TextCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs both captures for the current thread.
///
/// The guard has to outlive the run: dropping it early restores the previous subscriber, and the
/// span closes — which is when every terminal field is read — go nowhere.
fn capture() -> (SpanCollector, TextCapture, DefaultGuard) {
    let spans = SpanCollector::default();
    let text = TextCapture::default();
    let subscriber = tracing_subscriber::registry()
        .with(spans.clone())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(text.clone())
                .with_ansi(false)
                .with_span_events(FmtSpan::CLOSE),
        )
        .with(tracing_subscriber::filter::LevelFilter::TRACE);
    let guard = tracing::subscriber::set_default(subscriber);
    (spans, text, guard)
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Replays a fixed script of responses, one per turn.
struct ScriptedModel {
    script: Mutex<Vec<Result<ModelResponse>>>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().map(Ok).collect()),
        })
    }

    fn failing(error: Error) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(vec![Err(error)]),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            // The loop asked for a turn the script did not plan for. Answering with a final
            // message would hide the mismatch behind a passing test.
            return Err(Error::caller("scripted model ran out of responses"));
        }
        script.remove(0)
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

struct FixedResolver {
    model: Arc<ScriptedModel>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
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

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
    handler_time: Duration,
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
            handler_time: Duration::ZERO,
        }
    }

    /// Makes the handler take measurable time, so a timing assertion is about the split rather
    /// than about two zeroes.
    fn taking(mut self, handler_time: Duration) -> Self {
        self.handler_time = handler_time;
        self
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
        if !self.handler_time.is_zero() {
            tokio::time::sleep(self.handler_time).await;
        }
        Ok(ToolOutput::text("done"))
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

/// A tool whose failure stops settlement after the model response has already incurred usage.
struct PropagatingFailureTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl PropagatingFailureTool {
    fn new(name: &str) -> Self {
        let tool = ScriptedTool::new(name);
        Self {
            origin: tool.origin,
            schema: tool.schema,
        }
    }
}

#[async_trait]
impl Tool for PropagatingFailureTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Propagate)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Err(Error::tool(
            ToolErrorKind::ExecutionFailed,
            self.origin.qualified_name(),
            "test failure",
        ))
    }
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            name,
            json!({ "path": "a.txt" }),
        )),
    )
}

fn request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(
            AgentSpec::builder()
                .id(AgentId::new("coder"))
                .name("Coder")
                .instructions("do the thing")
                .tools(tools)
                .tool_use_behavior(ToolUseBehavior::RunLlmAgain)
                .build()
                .unwrap(),
        ),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
        }),
        RunId::new("run-trace"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

/// One tool call, then a final answer, with usage only on the second response.
fn tool_then_answer() -> Arc<ScriptedModel> {
    ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "final answer")]).with_usage(
            Usage::new(100, 20)
                .with_cached_input_tokens(80)
                .with_cache_write_tokens(10)
                .with_reasoning_tokens(12),
        ),
    ])
}

// ---------------------------------------------------------------------------
// cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn generation_span_records_normalized_usage() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = tool_then_answer();
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();
    drop(guard);

    let generations = spans.of_kind("generation");
    assert_eq!(generations.len(), 2, "{generations:?}");
    assert_eq!(generations[0].field("model.name"), Some("canonical-model"));
    assert_eq!(generations[0].field("model.provider"), Some("test-provider"));
    // A response that carried no usage reports zeroes rather than nothing. `Usage` has no "not
    // reported" state to preserve, so the span cannot invent one either.
    assert_eq!(generations[0].number("usage.input_tokens"), 0);
    assert_eq!(generations[1].number("usage.input_tokens"), 100);
    assert_eq!(generations[1].number("usage.cached_input_tokens"), 80);
    assert_eq!(generations[1].number("usage.cache_write_tokens"), 10);
    assert_eq!(generations[1].number("usage.output_tokens"), 20);
    assert_eq!(generations[1].number("usage.reasoning_tokens"), 12);
    assert_eq!(generations[1].field("outcome"), Some("ok"));

    // The run total is the sum of the calls, on the span one level up.
    let agent = spans.only("agent");
    assert_eq!(agent.number("usage.input_tokens"), 100);
    assert_eq!(agent.number("usage.output_tokens"), 20);
    assert_eq!(agent.field("finish.reason"), Some("final"));
}

#[tokio::test(flavor = "current_thread")]
async fn spans_nest_agent_turn_generation_and_function() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = tool_then_answer();
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();
    drop(guard);

    // Attribution is the whole point of the tree: a tool call that is not under its turn, and a
    // turn that is not under its run, cannot be costed against either.
    assert_eq!(spans.only("agent").parent, None);
    let turns = spans.of_kind("turn");
    assert_eq!(turns.len(), 2, "{turns:?}");
    assert_eq!(turns[0].field("turn.index"), Some("0"));
    assert_eq!(turns[1].field("turn.index"), Some("1"));
    for turn in &turns {
        assert_eq!(turn.parent.as_deref(), Some("agent"), "{turn:?}");
        assert_eq!(turn.field("outcome"), Some("ok"));
    }
    for generation in spans.of_kind("generation") {
        assert_eq!(generation.parent.as_deref(), Some("turn"), "{generation:?}");
    }
    let function = spans.only("function");
    assert_eq!(function.parent.as_deref(), Some("turn"), "{function:?}");
    assert_eq!(function.field("tool.name"), Some("write_file"));
    assert_eq!(function.field("tool.call_id"), Some("call-1"));
}

#[tokio::test(flavor = "current_thread")]
async fn function_span_separates_admission_wait_from_handler_execution() {
    // One permit and two calls, so the second call really does queue: the wait it reports is the
    // gate, not the handler, and a report that only had the total could not tell the two apart.
    const HANDLER: Duration = Duration::from_millis(40);
    let tool = Arc::new(ScriptedTool::new("write_file").taking(HANDLER));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            tool_call("call-item-1", "call-1", "write_file"),
            tool_call("call-item-2", "call-2", "write_file"),
        ]),
        ModelResponse::new(vec![message("msg-1", "final answer")]),
    ]);
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    Runner::run(
        request(vec![tool], &model, &cancel)
            .with_config(RunConfig::new().with_max_function_tool_concurrency(1)),
    )
    .await
    .unwrap();
    drop(guard);

    let functions = spans.of_kind("function");
    assert_eq!(functions.len(), 2, "{functions:?}");
    for function in &functions {
        let wait = function.number("tool.admission_wait_ms");
        let execution = function.number("tool.execution_ms");
        let total = function.number("duration.ms");
        assert!(
            wait + execution <= total,
            "the two segments must fit inside the span they split: \
             wait={wait} execution={execution} total={total}"
        );
        assert!(
            execution + 5 >= u64::try_from(HANDLER.as_millis()).unwrap(),
            "execution {execution}ms should cover the handler's own {HANDLER:?}"
        );
    }
    let queued = functions
        .iter()
        .map(|function| function.number("tool.admission_wait_ms"))
        .max()
        .unwrap();
    assert!(
        queued + 5 >= u64::try_from(HANDLER.as_millis()).unwrap(),
        "the second call waited behind the first, so its admission wait should show it: {queued}ms"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn budget_termination_names_the_exhausted_dimension() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(1)),
    )
    .await
    .unwrap();
    drop(guard);

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    let agent = spans.only("agent");
    assert_eq!(agent.field("outcome"), Some("ok"));
    assert_eq!(agent.field("finish.reason"), Some("max_turns"));
    // `FinishReason` folds tokens, cost and wall clock into one `budget_exhausted`, so which
    // allowance ran out is only recoverable from this field.
    assert_eq!(agent.field("budget.kind"), Some("max_turns"));
    assert!(agent.fields.contains_key("duration.ms"), "{agent:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_model_call_is_classified_as_an_error_with_its_code() {
    let model = ScriptedModel::failing(Error::caller("the provider rejected the request"));
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap_err();
    drop(guard);

    // A failure must not be silently absent from any level that saw it.
    for kind in ["generation", "turn", "agent"] {
        let span = spans.only(kind);
        assert_eq!(span.field("outcome"), Some("error"), "{kind}: {span:?}");
        assert_eq!(span.field("error.code"), Some("caller"), "{kind}: {span:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn paid_usage_survives_a_later_tool_failure_on_turn_and_agent_spans() {
    let tool = Arc::new(PropagatingFailureTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "write_file",
    )])
    .with_usage(
        Usage::new(37, 11)
            .with_cached_input_tokens(23)
            .with_cache_write_tokens(7)
            .with_reasoning_tokens(5),
    )]);
    let cancel = CancelScope::root();
    let (spans, _text, guard) = capture();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap_err();
    drop(guard);

    // The response was paid for before settlement propagated the tool failure, so both enclosing
    // spans must retain it even though no `RunResult` exists to expose the normal projection.
    for kind in ["turn", "agent"] {
        let span = spans.only(kind);
        assert_eq!(span.field("outcome"), Some("error"), "{kind}: {span:?}");
        assert_eq!(span.number("usage.input_tokens"), 37, "{kind}: {span:?}");
        assert_eq!(span.number("usage.cached_input_tokens"), 23, "{kind}: {span:?}");
        assert_eq!(span.number("usage.cache_write_tokens"), 7, "{kind}: {span:?}");
        assert_eq!(span.number("usage.output_tokens"), 11, "{kind}: {span:?}");
        assert_eq!(span.number("usage.reasoning_tokens"), 5, "{kind}: {span:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_run_records_its_root_cause_and_the_level_that_raised_it() {
    let model = tool_then_answer();
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);
    let (spans, _text, guard) = capture();

    Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap_err();
    drop(guard);

    let agent = spans.only("agent");
    // Cancellation is its own tier, not a failure: folding it into `error` makes the failure rate
    // spike every time someone presses stop.
    assert_eq!(agent.field("outcome"), Some("cancelled"));
    assert_eq!(agent.field("cancel.reason"), Some("user_interrupt"));
    // The level that raised it, not the level that noticed it — the run scope here inherits the
    // caller's interrupt rather than raising one of its own.
    assert_eq!(agent.field("cancel.scope"), Some("run"));
}

#[tokio::test(flavor = "current_thread")]
async fn replaying_the_same_script_produces_the_same_counts() {
    async fn counts() -> Vec<(String, u64, u64)> {
        let tool = Arc::new(ScriptedTool::new("write_file"));
        let model = tool_then_answer();
        let cancel = CancelScope::root();
        let (spans, _text, guard) = capture();
        Runner::run(request(vec![tool], &model, &cancel))
            .await
            .unwrap();
        drop(guard);
        spans
            .all()
            .iter()
            .filter_map(|span| {
                Some((
                    span.kind()?.to_owned(),
                    span.field("usage.input_tokens")?.parse().ok()?,
                    span.field("usage.output_tokens")?.parse().ok()?,
                ))
            })
            .collect()
    }

    // Statistics that drift between two identical replays cannot be a baseline for anything.
    assert_eq!(counts().await, counts().await);
}

#[tokio::test(flavor = "current_thread")]
async fn no_span_or_event_carries_model_content_tool_arguments_or_tool_output() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = tool_then_answer();
    let cancel = CancelScope::root();
    let (spans, text, guard) = capture();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();
    drop(guard);

    // Checked against the formatted log rather than the field set alone, because an event message
    // can leak what no span field carries.
    let output = text.text();
    for secret in ["final answer", "a.txt", "done", "帮我改一下文件"] {
        assert!(
            !output.contains(secret),
            "`{secret}` reached the trace: {output}"
        );
    }
    // The instructions are prompt material and have no business in a span either.
    assert!(!output.contains("do the thing"), "{output}");
    assert!(
        !spans.all().is_empty(),
        "the assertion above is worthless if nothing was captured"
    );
}
