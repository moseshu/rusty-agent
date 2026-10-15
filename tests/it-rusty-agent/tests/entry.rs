//! Ports of input preservation, sync-loop reuse, streaming symmetry, and approval recovery.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agents::{
    item::{CallId, ItemId, ModelResponse, OutputPhase, ToolCall},
    model::{Model, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent, RawResponseEvent},
    prelude::*,
    provider::quirks::ProviderQuirks,
    runner::{RunErrorHandler, RunErrorHandlerInput, RunErrorHandlerResult},
    usage::RequestUsage,
};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize, Serialize, JsonSchema, ToolInput)]
struct GreetingInput {
    name: String,
}

/// Greets a person.
#[tool]
fn greeting(name: String) -> String {
    format!("Hello, {name}!")
}

static APPROVED_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Performs an operation after approval.
#[tool(needs_approval = true)]
fn approved_operation() -> String {
    APPROVED_CALLS.fetch_add(1, Ordering::SeqCst);
    "approved".to_owned()
}

struct ScriptedModel {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    runtimes: Mutex<Vec<String>>,
    raw: bool,
}

impl ScriptedModel {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            runtimes: Mutex::new(Vec::new()),
            raw: true,
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.runtimes
            .lock()
            .unwrap()
            .push(tokio::runtime::Handle::current().id().to_string());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        stream::once(async move {
            let response = self.get_response(request).await?;
            let mut events = Vec::new();
            if self.raw {
                events.push(Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                    ProviderKey::new("test"),
                    "response.output_text.delta",
                    json!({"delta":"answer"}),
                ))));
            }
            events.push(Ok(ModelStreamEvent::Completed(Box::new(response))));
            Ok::<_, Error>(stream::iter(events))
        })
        .try_flatten()
        .boxed()
    }
}

use futures::TryStreamExt;

struct Provider(Arc<dyn Model>);
#[async_trait]
impl ModelProvider for Provider {
    fn get_model(&self, _: Option<&str>) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
}

fn registry(model: Arc<dyn Model>) -> Arc<ProviderRegistry> {
    Arc::new(
        ProviderRegistry::builder(ProviderKey::new("test"))
            .register(
                ProviderRegistration::new(
                    ProviderKey::new("test"),
                    ApiProtocol::OpenAiResponses,
                    move |_: ProviderQuirks| {
                        Ok(Arc::new(Provider(model.clone())) as Arc<dyn ModelProvider>)
                    },
                )
                .with_defaults(ModelSettings::new().with_temperature(0.2)),
            )
            .build()
            .unwrap(),
    )
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("test-agent"))
        .name("Test agent")
        .instructions("Answer briefly.")
        .model_settings(ModelSettings::new().with_temperature(0.5))
        .build()
        .unwrap()
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("answer"),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
    .with_usage(RequestUsage::new(11, 7))
}

#[tokio::test]
async fn convenient_and_explicit_requests_match_input_settings_usage_and_sessions() {
    let model = ScriptedModel::new(vec![answer("answer"), answer("answer")]);
    let models = registry(model.clone());
    let runner = AgentRunner::new(models.clone());
    let agent = agent();
    let config = RunConfig::new()
        .with_model("test/specific")
        .with_model_settings(ModelSettings::new().with_temperature(0.8));
    let first_session = Arc::new(InMemorySession::new("first"));
    let second_session = Arc::new(InMemorySession::new("second"));
    let convenient = runner
        .request(agent.clone(), "hello")
        .with_config(config.clone())
        .with_session(first_session.clone())
        .run()
        .await
        .unwrap();
    let explicit = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            models,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("hello"))],
        )
        .with_config(config)
        .with_session(second_session.clone()),
    )
    .await
    .unwrap();
    assert_eq!(convenient.final_text(), explicit.final_text());
    assert_eq!(
        convenient.outcome().finish_reason(),
        explicit.outcome().finish_reason()
    );
    assert_eq!(convenient.usage(), explicit.usage());
    assert_eq!(convenient.usage().input_tokens(), 11);
    let requests = model.requests.lock().unwrap();
    // Independent runs intentionally have distinct cache scopes. Compare every other field.
    let normalized_cache = requests[0].cache_plan().unwrap().clone();
    assert_eq!(
        requests[0],
        requests[1].clone().with_cache_plan(normalized_cache)
    );
    assert_eq!(requests[0].model_settings().temperature(), Some(0.8));
    drop(requests);
    let text = |items: Vec<RunItem>| {
        items
            .into_iter()
            .map(|item| match item.kind() {
                RunItemKind::Message(message) => message.text_content(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text(first_session.get_items(None).await.unwrap()),
        text(second_session.get_items(None).await.unwrap())
    );
    assert_ne!(convenient.state().run_id(), explicit.state().run_id());
}

#[tokio::test]
async fn typed_input_preserves_duplicates_order_and_empty_text() {
    let model = ScriptedModel::new(vec![answer("one"), answer("two"), answer("three")]);
    let runner = AgentRunner::new(registry(model.clone()));
    let item = ModelInputItem::Message(Message::user("repeat"));
    runner
        .run(agent(), [item.clone(), item.clone()])
        .await
        .unwrap();
    runner.run(agent(), "").await.unwrap();
    runner.run(agent(), item.clone()).await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests[0].input(), &[item.clone(), item.clone()]);
    assert_eq!(
        requests[1].input(),
        &[ModelInputItem::Message(Message::user(""))]
    );
    assert_eq!(requests[2].input(), &[item]);
}

// Advanced kernel contracts resolve through the renamed facade alone, and the module paths name
// the same types as the root and prelude exports rather than separate copies.
#[test]
fn kernel_contract_modules_resolve_through_the_facade() {
    use std::marker::PhantomData;
    fn same_type<T>(_: PhantomData<T>, _: PhantomData<T>) {}
    same_type(
        PhantomData::<agents::session::SessionId>,
        PhantomData::<SessionId>,
    );
    same_type(
        PhantomData::<agents::finish::FinishReason>,
        PhantomData::<FinishReason>,
    );
    let _ = (
        PhantomData::<agents::hook::HookEventName>,
        PhantomData::<agents::trace::SpanKind>,
        PhantomData::<agents::event::HostEvent>,
        PhantomData::<agents::session::rollout::PersistContext>,
    );
}

#[test]
fn sync_reuses_runtime_and_provider_across_calls_and_errors() {
    let model = ScriptedModel::new(vec![answer("first"), answer("second")]);
    let runner = AgentRunner::new(registry(model.clone()));
    let first = runner.run_sync(agent(), String::from("hello")).unwrap();
    let second = runner.request(agent(), "again").run_sync().unwrap();
    assert_eq!(first.final_text(), "first");
    assert_eq!(second.final_text(), "second");
    assert_ne!(first.state().run_id(), second.state().run_id());
    let runtimes = model.runtimes.lock().unwrap();
    assert_eq!(runtimes[0], runtimes[1]);
    drop(runtimes);
    assert!(runner.run_sync(agent(), "exhausted").is_err());
    model
        .responses
        .lock()
        .unwrap()
        .push_back(answer("recovered"));
    assert_eq!(
        runner.run_sync(agent(), "retry").unwrap().final_text(),
        "recovered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_rejects_an_active_runtime_before_calling_the_model() {
    let model = ScriptedModel::new(vec![answer("unused")]);
    let runner = AgentRunner::new(registry(model.clone()));
    let error = runner.run_sync(agent(), "hello").unwrap_err();
    assert!(matches!(error, Error::Caller { .. }));
    assert!(model.requests.lock().unwrap().is_empty());
    assert_eq!(
        runner.run(agent(), "async").await.unwrap().final_text(),
        "unused"
    );
}

#[tokio::test]
async fn borrowed_context_and_async_merge_use_existing_semantics() {
    let model = ScriptedModel::new(vec![answer("answer"), answer("answer")]);
    let runner = AgentRunner::new(registry(model.clone()));
    let agent = agent()
        .to_builder()
        .dynamic_instructions_async_fn(|context| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                assert!(context.app_context::<bool>().is_none());
                let name = context
                    .app_context::<String>()
                    .ok_or_else(|| Error::caller("String context required"))?;
                Ok(ResolvedPrompt::new(
                    format!("Hello {name}"),
                    PromptSource::Dynamic("test".to_owned()),
                ))
            })
        })
        .build()
        .unwrap();
    let session = Arc::new(InMemorySession::new("context"));
    let calls = Arc::new(AtomicUsize::new(0));
    let config = RunConfig::new().with_session_input_callback_async_fn({
        let calls = calls.clone();
        move |history, new_input| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(history.iter().chain(new_input.iter()).cloned().collect())
            })
        }
    });
    for input in ["first", "second"] {
        runner
            .request(agent.clone(), input)
            .with_context("Rust".to_owned())
            .with_session(session.clone())
            .with_config(config.clone())
            .run()
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(session.get_items(None).await.unwrap().len(), 4);
    assert_eq!(model.requests.lock().unwrap()[1].input().len(), 4);
    for request in [
        runner.request(agent.clone(), "missing"),
        runner.request(agent.clone(), "wrong").with_context(4_u32),
        // An `Arc<String>` is stored as that type, so a `String` lookup misses it.
        runner
            .request(agent, "nested")
            .with_context(Arc::new("Rust".to_owned())),
    ] {
        let error = request.run().await.unwrap_err();
        assert!(matches!(error, Error::Caller { .. }));
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn streamed_text_skips_raw_deltas_and_keeps_the_result() {
    let model = ScriptedModel::new(vec![answer("answer")]);
    let runner = AgentRunner::new(registry(model));
    let mut stream = runner
        .request(agent(), "hello")
        .with_config(RunConfig::new().with_partial_messages(true))
        .run_streamed();
    assert_eq!(stream.next_text().await.as_deref(), Some("answer"));
    assert!(stream.next_text().await.is_none());
    let result = stream.finish().await.unwrap();
    assert_eq!(result.final_text(), "answer");
    assert_eq!(result.usage().input_tokens(), 11);
}

#[tokio::test]
async fn streamed_item_reader_and_slow_consumers_keep_all_records() {
    let model = ScriptedModel::new(vec![answer("answer"), answer("without reading")]);
    let runner = AgentRunner::new(registry(model));
    let mut stream = runner.run_streamed(agent(), "hello");
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut items = Vec::new();
    while let Some(item) = stream.next_item().await {
        items.push(item);
    }
    let result = stream.finish().await.unwrap();
    assert!(!items.is_empty());
    assert_eq!(items, result.new_items());
    assert_eq!(
        runner
            .run_streamed(agent(), "again")
            .finish()
            .await
            .unwrap()
            .final_text(),
        "without reading"
    );
}

#[tokio::test]
async fn approval_resume_preserves_checkpoint_identity_usage_and_session() {
    APPROVED_CALLS.store(0, Ordering::SeqCst);
    let mut requests_by_entry = Vec::new();
    for (streamed, convenient) in [(false, false), (false, true), (true, false), (true, true)] {
        let response = ModelResponse::new(vec![RunItem::new(
            ItemId::new("call-item"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call"),
                "approved_operation",
                json!({}),
            )),
        )]);
        let model = ScriptedModel::new(vec![response, answer("approved answer")]);
        let models = registry(model.clone());
        let runner = AgentRunner::new(models.clone());
        let agent = agent()
            .to_builder()
            .tool(Arc::new(approved_operation::tool().unwrap()))
            .build()
            .unwrap();
        let session = Arc::new(InMemorySession::new("approval"));
        let request = if convenient {
            runner.request(agent.clone(), "perform operation")
        } else {
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                models.clone(),
                RunId::generate(),
                CancelScope::root(),
                vec![ModelInputItem::Message(Message::user("perform operation"))],
            )
        }
        .with_session(session.clone());
        let result = if streamed {
            request.run_streamed().finish().await
        } else {
            request.run().await
        }
        .unwrap();
        assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        let mut state: RunState =
            serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
        state
            .approve(&result.outcome().interruptions()[0], false)
            .unwrap();
        let run_id = state.run_id().clone();
        let request = if convenient {
            runner.request(agent, state)
        } else {
            RunRequest::new(
                AgentBinding::direct(agent),
                models,
                RunId::generate(),
                CancelScope::root(),
                vec![],
            )
            .with_state(state)
        }
        .with_session(session.clone());
        let resumed = if streamed {
            request.run_streamed().finish().await
        } else {
            request.run().await
        }
        .unwrap();
        assert_eq!(resumed.state().run_id(), &run_id);
        assert_eq!(resumed.final_text(), "approved answer");
        assert_eq!(resumed.state().usage_totals().requests(), 2);
        assert_eq!(resumed.usage().requests(), 1);
        let input_count = session.get_items(None).await.unwrap().iter().filter(|item| matches!(item.kind(), RunItemKind::Message(message) if message.text_content() == "perform operation")).count();
        assert_eq!(input_count, 1);
        requests_by_entry.push(model.requests.lock().unwrap().clone());
    }
    assert_eq!(APPROVED_CALLS.load(Ordering::SeqCst), 4);
    for requests in &requests_by_entry[1..] {
        for (expected, actual) in requests_by_entry[0].iter().zip(requests) {
            assert_eq!(
                *expected,
                actual
                    .clone()
                    .with_cache_plan(expected.cache_plan().unwrap().clone())
            );
        }
    }
}

#[tokio::test]
async fn async_session_callback_mutations_errors_and_cancellation_are_preserved() {
    let model = ScriptedModel::new(vec![answer("answer")]);
    let runner = AgentRunner::new(registry(model.clone()));
    let session = Arc::new(InMemorySession::new("merge"));
    let config = RunConfig::new().with_session_input_callback_async_fn(|history, input| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            input.push(RunItem::new(
                ItemId::new("injected"),
                RunItemKind::Message(Message::user("injected")),
            ));
            Ok(history.iter().chain(input.iter()).cloned().collect())
        })
    });
    runner
        .request(agent(), "hello")
        .with_config(config)
        .with_session(session.clone())
        .run()
        .await
        .unwrap();
    assert_eq!(session.get_items(None).await.unwrap().len(), 3);
    let error = runner
        .request(agent(), "bad")
        .with_session(session.clone())
        .with_config(
            RunConfig::new().with_session_input_callback_async_fn(|_, _| {
                Box::pin(async { Err(Error::caller("merge failed")) })
            }),
        )
        .run()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("merge failed"));
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancel = CancelScope::root();
    let request = runner
        .request(agent(), "wait")
        .with_session(session)
        .with_cancel_scope(cancel.clone())
        .with_config(RunConfig::new().with_session_input_callback_async_fn({
            let entered = entered.clone();
            move |_, _| {
                entered.notify_one();
                Box::pin(std::future::pending())
            }
        }));
    let task = tokio::spawn(request.run());
    entered.notified().await;
    cancel.cancel(CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn text_reader_leaves_failure_available_from_finish() {
    let runner = AgentRunner::new(registry(ScriptedModel::new(vec![])));
    let mut stream = runner.run_streamed(agent(), "hello");
    assert!(stream.next_text().await.is_none());
    assert!(
        stream
            .finish()
            .await
            .unwrap_err()
            .to_string()
            .contains("script exhausted")
    );
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct TypedAnswer {
    approved: bool,
}

#[tokio::test]
async fn typed_outputs_and_decode_type_errors_work_through_the_facade() {
    let model = ScriptedModel::new(vec![answer(r#"{"approved":true}"#)]);
    let runner = AgentRunner::new(registry(model.clone()));
    let agent = agent()
        .to_builder()
        .output_type::<TypedAnswer>()
        .unwrap()
        .build()
        .unwrap();
    let result = runner.run(agent, "review").await.unwrap();
    assert!(result.final_output_as::<TypedAnswer>().unwrap().approved);
    assert!(result.final_output_as::<String>().is_err());
    assert!(model.requests.lock().unwrap()[0].output_schema().is_some());
    let _schema = GreetingInput::func_schema("greeting_input").unwrap();
    assert_eq!(greeting::tool().unwrap().schema().name(), "greeting");
}

struct Closeout;
#[async_trait]
impl RunErrorHandler for Closeout {
    async fn handle(&self, _: RunErrorHandlerInput<'_>) -> Result<Option<RunErrorHandlerResult>> {
        Ok(Some(
            RunErrorHandlerResult::new(Message::assistant("handled", OutputPhase::Final))
                .with_write_to_history(false),
        ))
    }
}

#[tokio::test]
async fn text_reader_includes_unrecorded_final_deliveries() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![RunItem::new(
        ItemId::new("greeting-call"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("greeting"),
            "greeting",
            json!({"name":"Rust"}),
        )),
    )])]);
    let runner = AgentRunner::new(registry(model));
    let agent = agent()
        .to_builder()
        .tool(Arc::new(greeting::tool().unwrap()))
        .build()
        .unwrap();
    let mut stream = runner
        .request(agent, "hello")
        .with_config(
            RunConfig::new()
                .with_max_turns(1)
                .with_error_handler(Arc::new(Closeout)),
        )
        .run_streamed();
    assert_eq!(stream.next_text().await.as_deref(), Some("handled"));
    assert!(stream.next_text().await.is_none());
    assert_eq!(stream.finish().await.unwrap().final_text(), "handled");
}

struct HangingModel(Arc<tokio::sync::Notify>);
#[async_trait]
impl Model for HangingModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_reaches_a_convenient_async_request() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let runner = AgentRunner::new(registry(Arc::new(HangingModel(entered.clone()))));
    let cancel = CancelScope::root();
    let request = runner
        .request(agent(), "wait")
        .with_cancel_scope(cancel.clone());
    let task = tokio::spawn(request.run());
    entered.notified().await;
    cancel.cancel(CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled { .. }));
}

/// Signals a waiting thread when called and records when its pending call is dropped.
struct SyncHangingModel {
    entered: std::sync::mpsc::Sender<()>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
struct MarkDropped(Arc<std::sync::atomic::AtomicBool>);
impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl Model for SyncHangingModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        let _dropped = MarkDropped(self.dropped.clone());
        let _ = self.entered.send(());
        std::future::pending().await
    }
}

// Rust counterpart of upstream's interrupted `run_sync` case: the host cancels through the
// request's explicit scope from another thread. The pending model call must be dropped before
// the sync call returns, and the thread's reused runtime must then serve the next call normally.
#[test]
fn sync_call_cancelled_from_another_thread_leaves_the_runtime_reusable() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hanging = AgentRunner::new(registry(Arc::new(SyncHangingModel {
        entered: entered_tx,
        dropped: dropped.clone(),
    })));
    let model = ScriptedModel::new(vec![answer("after cancel")]);
    let ready = AgentRunner::new(registry(model.clone()));
    let cancel = CancelScope::root();
    std::thread::spawn({
        let cancel = cancel.clone();
        move || {
            let cancelled = hanging
                .request(agent(), "wait")
                .with_cancel_scope(cancel)
                .run_sync();
            let cancelled_call_dropped = dropped.load(Ordering::SeqCst);
            let next = ready.run_sync(agent(), "next");
            let _ = result_tx.send((cancelled, cancelled_call_dropped, next));
        }
    });

    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    cancel.cancel(CancelReason::UserInterrupt);
    let (cancelled, cancelled_call_dropped, next) =
        result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(cancelled.unwrap_err(), Error::Cancelled { .. }));
    assert!(cancelled_call_dropped);
    assert_eq!(next.unwrap().final_text(), "after cancel");
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelling_borrowed_async_callbacks_stops_without_calling_the_model() {
    let model = ScriptedModel::new(vec![answer("unused")]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let agent = agent()
        .to_builder()
        .dynamic_instructions_async_fn({
            let entered = entered.clone();
            move |_| {
                entered.notify_one();
                Box::pin(std::future::pending())
            }
        })
        .build()
        .unwrap();
    let runner = AgentRunner::new(registry(model.clone()));
    let cancel = CancelScope::root();
    let task = tokio::spawn(
        runner
            .request(agent, "wait")
            .with_cancel_scope(cancel.clone())
            .run(),
    );
    entered.notified().await;
    cancel.cancel(CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled { .. }));
    assert!(model.requests.lock().unwrap().is_empty());
}

struct DropModel {
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<tokio::sync::Notify>,
}
struct ReportDrop(Arc<tokio::sync::Notify>);
impl Drop for ReportDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
#[async_trait]
impl Model for DropModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        let _report = ReportDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropping_a_convenient_stream_cancels_the_model_without_cancelling_its_parent() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(tokio::sync::Notify::new());
    let runner = AgentRunner::new(registry(Arc::new(DropModel {
        entered: entered.clone(),
        dropped: dropped.clone(),
    })));
    let parent = CancelScope::root();
    let stream = runner
        .request(agent(), "wait")
        .with_cancel_scope(parent.clone())
        .run_streamed();
    entered.notified().await;
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .unwrap();
    assert!(!parent.is_cancelled());
}

#[tokio::test]
async fn a_shipped_provider_runs_text_and_streamed_inputs_without_a_custom_resolver() {
    use agents::openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    let server = MockServer::start().await;
    let frames = [
        json!({"type":"response.created", "response":{"id":"resp", "status":"in_progress"}}),
        json!({"type":"response.output_text.delta", "delta":"answer"}),
        json!({"type":"response.completed", "response":{
            "id":"resp", "object":"response", "status":"completed",
            "output":[{"id":"message", "type":"message", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"answer"}]}],
            "usage":{"input_tokens":11, "output_tokens":7, "total_tokens":18}
        }}),
    ];
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(2)
        .mount(&server)
        .await;
    let auth = OpenAiAuth::new("fixture-key").with_base_url(format!("{}/v1/", server.uri()));
    let models = ProviderRegistry::builder(ProviderKey::new("openai"))
        .register(ProviderRegistration::new(
            ProviderKey::new("openai"),
            ApiProtocol::OpenAiResponses,
            move |quirks: ProviderQuirks| {
                Ok(Arc::new(
                    OpenAiResponsesProvider::new(auth.clone(), "fixture-model")?
                        .with_quirks(quirks),
                ) as Arc<dyn ModelProvider>)
            },
        ))
        .build()
        .unwrap();
    let runner = AgentRunner::new(Arc::new(models));
    let first = runner.run(agent(), "hello").await.unwrap();
    let mut stream = runner
        .request(agent(), "hello")
        .with_config(RunConfig::new().with_partial_messages(true))
        .run_streamed();
    assert_eq!(stream.next_text().await.as_deref(), Some("answer"));
    assert!(stream.next_text().await.is_none());
    let second = stream.finish().await.unwrap();
    assert_eq!(first.final_text(), second.final_text());
    assert_eq!(first.usage(), second.usage());
    let requests = server.received_requests().await.unwrap();
    let first_body: serde_json::Value = requests[0].body_json().unwrap();
    let second_body: serde_json::Value = requests[1].body_json().unwrap();
    assert_eq!(first_body, second_body);
    assert_eq!(first_body["model"], "fixture-model");
    assert_eq!(first_body["input"][0]["content"][0]["text"], "hello");
}
