//! Contracts for a transfer of control inside the run loop.

#[path = "agent_handoff/structured_output.rs"]
mod structured_output;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::{
        AgentId, AgentSpec, HandoffInputData, HandoffInputFilter, HandoffSpec, HistoryProjection,
    },
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, OutputGuardrail},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::rollout::{RolloutItem, RolloutRecorder},
    state::{RunId, RunState},
    tool::ToolSchema,
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::result::ContinuationInput,
    runner::{RunConfig, RunRequest, Runner},
};
use serde_json::json;

async fn drive(request: RunRequest, streamed: bool) -> Result<ra_runtime::runner::RunResult> {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        let mut items = std::collections::HashSet::new();
        while let Some(event) = stream.next_event().await {
            if let ra_runtime::runner::RunStreamEvent::Item(item) = event {
                assert!(
                    items.insert(item.id().clone()),
                    "a stream item was repeated"
                );
            }
        }
        stream.finish().await
    } else {
        Runner::run(request).await
    }
}

/// The reference persists a filter's `new_items` for the transferring turn, rewritten and
/// synthesized records included, and the receiving agent sees the same list.
#[tokio::test]
async fn a_filters_new_items_are_what_the_transferring_turn_stores() {
    for streamed in [false, true] {
        let target = reviewer(Vec::new());
        let handoff = target.as_handoff().unwrap().with_input_filter_fn(
            |_: &RunContext, data: HandoffInputData| {
                let rewritten = message("msg-1", "sanitized narration");
                let synthesized = message("synthetic", "model-only context");
                std::future::ready(Ok(data.with_new_items(vec![rewritten, synthesized])))
            },
        );
        let model = script();
        let result = drive(
            run_request(
                planner(handoff, Vec::new()),
                target,
                &model,
                RunConfig::new().with_partial_messages(true),
            ),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(model.input_labels(1), ["message", "message", "message"]);
        let input = model.inputs.lock().unwrap();
        assert!(
            serde_json::to_string(&input[1])
                .unwrap()
                .contains("sanitized narration")
        );
        let stored = serde_json::to_string(result.state().generated_items()).unwrap();
        assert!(!stored.contains("这件事该交给审阅者"));
        assert!(stored.contains("sanitized narration"));
        assert!(stored.contains("model-only context"));
        // The filter returned neither half of the transfer, so neither is stored.
        assert!(
            result
                .state()
                .generated_items()
                .iter()
                .all(|item| item.id().as_str() != "call-item-1"
                    && !matches!(item.kind(), RunItemKind::HandoffOutput(_)))
        );
    }
}

#[tokio::test]
async fn filter_failure_preserves_the_turn_and_resumes_without_repeating_the_callback() {
    for streamed in [false, true] {
        let callbacks = Arc::new(AtomicUsize::new(0));
        let filters = Arc::new(AtomicUsize::new(0));
        let invoked = Arc::clone(&callbacks);
        let filtered = Arc::clone(&filters);
        let target = reviewer(Vec::new());
        let handoff = target
            .as_handoff()
            .unwrap()
            .with_on_handoff_fn(move |_| {
                invoked.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            })
            .with_input_filter_fn(move |_: &RunContext, data: HandoffInputData| {
                std::future::ready(if filtered.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::caller("filter failed"))
                } else {
                    Ok(data)
                })
            });
        let source = planner(handoff, Vec::new());
        let config = RunConfig::new()
            .with_partial_messages(true)
            .with_agent_registry(
                AgentRegistry::builder()
                    .register(Arc::clone(&source))
                    .register(Arc::clone(&target))
                    .build()
                    .unwrap(),
            );
        let model = script();
        let error = drive(
            run_request(Arc::clone(&source), target, &model, config.clone()),
            streamed,
        )
        .await
        .unwrap_err();
        let checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap())
                .unwrap();
        assert_eq!(checkpoint.generated_items().len(), 3);
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        let result = drive(
            RunRequest::new(
                AgentBinding::direct(source),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("run-handoff"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_state(checkpoint)
            .with_config(config),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(result.final_text(), "审阅完成");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(filters.load(Ordering::SeqCst), 2);
        assert_eq!(result.state().generated_items().len(), 4);
        assert_eq!(
            model.input_labels(1),
            ["message", "message", "tool_call", "handoff_output"]
        );
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct TransferReason {
    reason: String,
}

#[tokio::test]
async fn enabled_handoff_collisions_use_the_upstream_warn_error_and_disabled_behavior() {
    use ra_runtime::turn::prepare::ToolNameCollisionPolicy;
    for (policy, disabled) in [
        (ToolNameCollisionPolicy::Warn, false),
        (ToolNameCollisionPolicy::Error, false),
        (ToolNameCollisionPolicy::Error, true),
    ] {
        let first = reviewer(Vec::new());
        let second = first
            .to_builder()
            .id(AgentId::new("writer"))
            .build()
            .unwrap();
        let source = AgentSpec::builder()
            .id(AgentId::new("planner"))
            .name("Planner")
            .handoffs([
                first.as_handoff().unwrap(),
                second.as_handoff().unwrap().with_is_enabled(!disabled),
            ])
            .build()
            .unwrap();
        let registry = AgentRegistry::builder()
            .register(Arc::clone(&source))
            .register(first)
            .register(second)
            .build()
            .unwrap();
        let model = script();
        let result = Runner::run(
            RunRequest::new(
                AgentBinding::direct(source),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("collisions"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_config(
                RunConfig::new()
                    .with_agent_registry(registry)
                    .with_tool_name_collision_policy(policy),
            ),
        )
        .await;
        if policy == ToolNameCollisionPolicy::Error && !disabled {
            assert!(result.unwrap_err().to_string().contains("more than once"));
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                result.unwrap().last_agent().id().as_str(),
                if disabled { "reviewer" } else { "writer" }
            );
            assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test]
async fn a_self_handoff_restarts_the_same_agent_in_the_same_run() {
    let source = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .handoff(HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()))
        .build()
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&source))
        .build()
        .unwrap();
    let model = script();
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(source),
            Arc::new(FixedResolver {
                model: Arc::clone(&model),
            }),
            RunId::new("self-handoff"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("review"))],
        )
        .with_config(RunConfig::new().with_agent_registry(registry)),
    )
    .await
    .unwrap();
    assert_eq!(result.turns(), 2);
    assert_eq!(result.last_agent().id().as_str(), "reviewer");
    assert_eq!(
        model.input_labels(1),
        ["message", "message", "tool_call", "handoff_output"]
    );
}

#[tokio::test]
async fn default_history_and_typed_callbacks_survive_ordinary_streaming_and_checkpoint_runs() {
    use ra_core::session::Session;
    for streamed in [false, true] {
        let model = script();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&callbacks);
        let target = reviewer(Vec::new());
        let source = planner(
            target
                .as_handoff()
                .unwrap()
                .with_input_type(move |run: &RunContext, input: TransferReason| {
                    assert_eq!(run.agent().id().as_str(), "planner");
                    assert_eq!(input.reason, "需要审阅");
                    recorded.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(()))
                })
                .unwrap(),
            Vec::new(),
        );
        let session = Arc::new(ra_session::InMemorySession::new("handoff-session"));
        let paused = drive(
            run_request(
                Arc::clone(&source),
                Arc::clone(&target),
                &model,
                RunConfig::new()
                    .with_max_turns(1)
                    .with_error_handler(Arc::new(CheckpointCloseout))
                    .with_partial_messages(true),
            )
            .with_session(session.clone()),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(paused.last_agent().id(), target.id());
        let mut checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
        checkpoint.set_max_turns(Some(10));
        let resumed = drive(
            RunRequest::new(
                AgentBinding::direct(target),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("run-handoff"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_state(checkpoint)
            .with_session(session.clone())
            .with_config(RunConfig::new().with_partial_messages(true)),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(resumed.final_text(), "审阅完成");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(
            model.input_labels(1),
            ["message", "message", "tool_call", "handoff_output"]
        );
        let stored = session.get_items(None).await.unwrap();
        assert_eq!(stored.len(), 5);
        assert_eq!(
            stored
                .iter()
                .filter(|item| item.id().as_str() == "call-item-1")
                .count(),
            1
        );
        assert_eq!(
            stored
                .iter()
                .filter(|item| item.id().as_str() == "call-1.output")
                .count(),
            1
        );
    }
}

struct TransferOrder {
    name: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ra_core::lifecycle::LifecycleHook for TransferOrder {
    fn name(&self) -> &str {
        self.name
    }
    async fn on_handoff(
        &self,
        _: ra_core::lifecycle::LifecycleScope,
        input: &ra_core::lifecycle::HandoffInput<'_>,
    ) -> Result<()> {
        assert_eq!(input.from().id().as_str(), "planner");
        assert_eq!(input.to().id().as_str(), "reviewer");
        self.log.lock().unwrap().push(self.name);
        Ok(())
    }
    async fn on_agent_start(
        &self,
        _: ra_core::lifecycle::LifecycleScope,
        input: &ra_core::lifecycle::AgentStartInput<'_>,
    ) -> Result<()> {
        if input.run().agent().id().as_str() == "reviewer" {
            self.log.lock().unwrap().push("target_start");
        }
        Ok(())
    }
}

#[tokio::test]
async fn invocation_source_hooks_filter_and_target_start_follow_upstream_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let callback_log = Arc::clone(&log);
    let filter_log = Arc::clone(&log);
    let target = reviewer(Vec::new())
        .to_builder()
        .lifecycle_hook(Arc::new(TransferOrder {
            name: "target_handoff",
            log: Arc::clone(&log),
        }))
        .build()
        .unwrap();
    let source = planner(
        target
            .as_handoff()
            .unwrap()
            .with_input_type(move |_: &RunContext, _: TransferReason| {
                callback_log.lock().unwrap().push("callback");
                std::future::ready(Ok(()))
            })
            .unwrap()
            .with_input_filter_fn(move |_: &RunContext, data: HandoffInputData| {
                filter_log.lock().unwrap().push("filter");
                assert_eq!(data.new_items().len(), 3);
                assert!(matches!(
                    data.new_items().last().unwrap().kind(),
                    RunItemKind::HandoffOutput(_)
                ));
                std::future::ready(Ok(data
                    .with_pre_handoff_items(Vec::new())
                    .with_input_items(Vec::new())))
            }),
        Vec::new(),
    )
    .to_builder()
    .lifecycle_hook(Arc::new(TransferOrder {
        name: "source_handoff",
        log: Arc::clone(&log),
    }))
    .build()
    .unwrap();
    let model = script();
    let result = Runner::run(run_request(
        source,
        target,
        &model,
        RunConfig::new().with_handoff_input_filter(Arc::new(FailingFilter)),
    ))
    .await
    .unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["callback", "source_handoff", "filter", "target_start"]
    );
    assert_eq!(model.input_labels(1), ["message"]);
    assert_eq!(result.state().generated_items().len(), 4);
}

struct FailingFilter;
#[async_trait]
impl HandoffInputFilter for FailingFilter {
    async fn filter(&self, _: &RunContext, _: HandoffInputData) -> Result<HandoffInputData> {
        Err(Error::caller("the fallback filter ran"))
    }
}

#[tokio::test]
async fn malformed_arguments_and_callback_failures_stop_before_filtering_or_target_calls() {
    for streamed in [false, true] {
        for invalid in [false, true] {
            let callbacks = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&callbacks);
            let target = reviewer(Vec::new());
            let handoff = target
                .as_handoff()
                .unwrap()
                .with_input_type(move |_: &RunContext, _: TransferReason| {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(Error::caller("callback failed")))
                })
                .unwrap()
                .with_input_filter(Arc::new(FailingFilter));
            let arguments = if invalid {
                json!({"reason": 42})
            } else {
                json!({"reason": "review"})
            };
            let model = ScriptedModel::new(vec![ModelResponse::new(vec![item(
                "transfer",
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("handoff"),
                    "transfer_to_reviewer",
                    arguments,
                )),
            )])]);
            let error = drive(
                run_request(
                    planner(handoff, Vec::new()),
                    target,
                    &model,
                    RunConfig::new().with_partial_messages(true),
                ),
                streamed,
            )
            .await
            .unwrap_err();
            assert_eq!(callbacks.load(Ordering::SeqCst), usize::from(!invalid));
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            if invalid {
                assert_eq!(error.code(), "provider.behavior");
            } else {
                assert!(error.to_string().contains("callback failed"));
            }
        }
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_handoff_callback() {
    use ra_core::cancel::CancelReason;
    let entered = Arc::new(tokio::sync::Notify::new());
    let notified = Arc::clone(&entered);
    let callbacks = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&callbacks);
    let target = reviewer(Vec::new());
    let handoff = target.as_handoff().unwrap().with_on_handoff_fn(move |_| {
        let entered = Arc::clone(&notified);
        let recorded = Arc::clone(&recorded);
        async move {
            entered.notify_one();
            std::future::pending::<()>().await;
            recorded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let source = planner(handoff, Vec::new());
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&source))
        .register(target)
        .build()
        .unwrap();
    let model = script();
    let cancel = CancelScope::root();
    let request = RunRequest::new(
        AgentBinding::direct(source),
        Arc::new(FixedResolver {
            model: Arc::clone(&model),
        }),
        RunId::new("run-cancel"),
        cancel.clone(),
        Vec::new(),
    )
    .with_config(RunConfig::new().with_agent_registry(registry));
    let task = tokio::spawn(Runner::run(request));
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    cancel.cancel(CancelReason::UserInterrupt);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().is_cancelled());
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

struct ApprovalProbe {
    origin: ra_core::tool::ToolOrigin,
    schema: ToolSchema,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ra_core::tool::Tool for ApprovalProbe {
    fn origin(&self) -> &ra_core::tool::ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ra_core::tool::ToolOptions {
        ra_core::tool::ToolOptions::default()
            .with_approval(ra_core::tool::ToolApprovalPolicy::Always)
    }
    async fn call(&self, _: ra_core::tool::ToolContext<'_>) -> Result<ra_core::tool::ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ra_core::tool::ToolOutput::text("approved work"))
    }
}

#[tokio::test]
async fn approvals_defer_the_transfer_and_resume_it_once_without_another_source_model_turn() {
    use ra_core::session::Session;
    for (streamed, repeated, approved) in [
        (false, false, true),
        (true, false, true),
        (false, true, true),
        (true, true, true),
        (false, false, false),
        (true, false, false),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let callbacks = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&callbacks);
        let target = reviewer(Vec::new());
        let source = planner(target.as_handoff().unwrap().with_input_type(move |_: &RunContext, _: TransferReason| {
            recorded.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        }).unwrap(), Vec::new()).to_builder().tool(Arc::new(ApprovalProbe {
            origin: ra_core::tool::ToolOrigin::new("probe").unwrap(),
            schema: ToolSchema::new("probe", json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})).unwrap(),
            calls: Arc::clone(&calls),
        })).build().unwrap();
        let mut response_items = vec![
            item(
                "probe-call",
                RunItemKind::ToolCall(ToolCall::new(CallId::new("probe"), "probe", json!({}))),
            ),
            transfer_call("transfer-call", "transfer"),
        ];
        if repeated {
            response_items.push(transfer_call("extra-transfer", "extra"));
        }
        let model = ScriptedModel::new(vec![
            ModelResponse::new(response_items),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = RunConfig::new()
            .with_partial_messages(true)
            .with_agent_registry(
                AgentRegistry::builder()
                    .register(Arc::clone(&source))
                    .register(Arc::clone(&target))
                    .build()
                    .unwrap(),
            );
        let session = Arc::new(ra_session::InMemorySession::new("approval-handoff"));
        let first = drive(
            run_request(
                Arc::clone(&source),
                Arc::clone(&target),
                &model,
                config.clone(),
            )
            .with_session(session.clone()),
            streamed,
        )
        .await
        .unwrap();
        let ra_runtime::runner::RunOutcome::Interrupted { items } = first.outcome() else {
            panic!("expected approval");
        };
        assert!(first.state().pending_handoff().is_some());
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut checkpoint = first.state().clone();
        if approved {
            checkpoint.approve(&items[0], false).unwrap();
        } else {
            checkpoint.reject(&items[0], false).unwrap();
        }
        let checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
        let resumed = drive(
            RunRequest::new(
                AgentBinding::direct(source),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("run-handoff"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_state(checkpoint)
            .with_config(config)
            .with_session(session.clone()),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(resumed.final_text(), "reviewed");
        assert_eq!(resumed.last_agent().id(), target.id());
        assert!(resumed.state().pending_handoff().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(approved));
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        let mut expected = vec!["message", "tool_call", "tool_call"];
        if repeated {
            expected.push("tool_call");
        }
        expected.push("tool_call_output");
        if repeated {
            expected.push("tool_call_output");
        }
        expected.push("handoff_output");
        assert_eq!(model.input_labels(1), expected);
        let stored = session.get_items(None).await.unwrap();
        assert_eq!(
            stored
                .iter()
                .filter(|item| item.id().as_str() == "transfer-call")
                .count(),
            1
        );
        assert_eq!(
            stored
                .iter()
                .filter(|item| item.id().as_str() == "transfer.output")
                .count(),
            1
        );
    }
}

/// A model that replays a fixed script and records what each call was handed.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
    instructions: Mutex<Vec<Option<String>>>,
    handoff_names: Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            inputs: Mutex::new(Vec::new()),
            instructions: Mutex::new(Vec::new()),
            handoff_names: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn next_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(request.input().to_vec());
        self.instructions
            .lock()
            .unwrap()
            .push(request.system_instructions().map(str::to_owned));
        self.handoff_names.lock().unwrap().push(
            request
                .handoffs()
                .iter()
                .map(|handoff| handoff.name().to_owned())
                .collect(),
        );
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }

    /// The kinds one model call was handed, in order.
    fn input_labels(&self, call: usize) -> Vec<&'static str> {
        self.inputs.lock().unwrap()[call]
            .iter()
            .map(ModelInputItem::label)
            .collect()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.next_response(request)
    }

    fn stream_response(&self, request: ModelRequest) -> ra_core::model::ModelStream<'_> {
        use ra_core::model::{ModelStreamEvent, RunItemStreamEvent};
        let events = match self.next_response(request) {
            Ok(response) => {
                let mut events = response
                    .output()
                    .iter()
                    .cloned()
                    .map(|item| {
                        Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                            "item_done",
                            item,
                        )))
                    })
                    .collect::<Vec<_>>();
                events.push(Ok(ModelStreamEvent::Completed(Box::new(response))));
                events
            }
            Err(error) => vec![Err(error)],
        };
        Box::pin(futures::stream::iter(events))
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

/// Records that it ran, so a run can say whose promise was checked at delivery.
struct RecordingGuardrail {
    name: &'static str,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl OutputGuardrail for RecordingGuardrail {
    fn name(&self) -> &str {
        self.name
    }

    async fn check(
        &self,
        _context: &RunContext,
        _output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        self.seen.lock().unwrap().push(self.name);
        Ok(GuardrailFunctionOutput::pass())
    }
}

/// Keeps only the caller's opening input, dropping everything the projection let through.
struct KeepOnlyOpeningInput;

#[async_trait]
impl HandoffInputFilter for KeepOnlyOpeningInput {
    async fn filter(
        &self,
        _context: &RunContext,
        data: HandoffInputData,
    ) -> Result<HandoffInputData> {
        Ok(data
            .with_pre_handoff_items(Vec::new())
            .with_new_items(Vec::new()))
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

fn commentary(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Commentary)),
    )
}

fn transfer_call(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "transfer_to_reviewer",
            json!({ "reason": "需要审阅" }),
        )),
    )
}

fn handoff_schema() -> ToolSchema {
    ToolSchema::new(
        "transfer_to_reviewer",
        json!({
            "type": "object",
            "properties": { "reason": { "type": "string" } },
            "required": ["reason"],
            "additionalProperties": false
        }),
    )
    .unwrap()
}

fn reviewer(output_guardrails: Vec<Arc<dyn OutputGuardrail>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("review the work")
        .output_guardrails(output_guardrails)
        .build()
        .unwrap()
}

fn planner(
    handoff: HandoffSpec,
    output_guardrails: Vec<Arc<dyn OutputGuardrail>>,
) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .instructions("plan the work")
        .handoff(handoff)
        .output_guardrails(output_guardrails)
        .build()
        .unwrap()
}

/// A planner that transfers to a reviewer, which then answers.
fn script() -> Arc<ScriptedModel> {
    ScriptedModel::new(vec![
        ModelResponse::new(vec![
            commentary("msg-1", "这件事该交给审阅者"),
            transfer_call("call-item-1", "call-1"),
        ]),
        ModelResponse::new(vec![message("msg-2", "审阅完成")]),
    ])
}

fn run_request(
    planner: Arc<AgentSpec>,
    reviewer: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    config: RunConfig,
) -> RunRequest {
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&planner))
        .register(reviewer)
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(planner),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
        }),
        RunId::new("run-handoff"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("帮我处理这件事"))],
    )
    .with_config(config.with_agent_registry(registry))
}

#[tokio::test]
async fn control_moves_to_the_target_and_the_session_keeps_what_the_projection_withheld() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::None),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The run is delivered by whoever ended up holding the answer.
    assert_eq!(result.turns(), 2);
    assert_eq!(result.last_agent().id().as_str(), "reviewer");
    assert_eq!(result.final_text(), "审阅完成");
    assert_eq!(result.state().current_agent().unwrap().as_str(), "reviewer");

    // The second turn runs as the receiving agent: its instructions, and no handoff of its own.
    let instructions = model.instructions.lock().unwrap().clone();
    assert_eq!(
        instructions,
        [
            Some("plan the work".to_owned()),
            Some("review the work".to_owned())
        ]
    );
    let advertised = model.handoff_names.lock().unwrap().clone();
    assert_eq!(advertised[0], ["transfer_to_reviewer"]);
    assert!(advertised[1].is_empty());

    // `HistoryProjection::None` is explicitly selected: the receiving agent is handed the brief that moved
    // control and nothing else — not the caller's request, and not the planner's narration.
    assert_eq!(model.input_labels(0), ["message"]);
    assert_eq!(model.input_labels(1), ["tool_call", "handoff_output"]);

    // The session is authoritative and keeps every record, including the ones the projection
    // withheld from the receiving agent.
    let stored = result
        .state()
        .generated_items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(stored, ["msg-1", "call-item-1", "call-1.output", "msg-2"]);

    // Records stay attributed to whoever produced them, so a transcript read back still says which
    // agent said what.
    let authors = result
        .state()
        .generated_items()
        .iter()
        .filter_map(|item| item.provenance().map(|p| p.agent_id().as_str().to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(authors, ["planner", "planner", "planner", "reviewer"]);

    // The turn records say the same thing, one turn at a time.
    let agents = result
        .turn_records()
        .iter()
        .map(|record| record.agent().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(agents, ["planner", "reviewer"]);
    assert_eq!(result.turn_records()[0].next_step_code(), "handoff");
}

#[tokio::test]
async fn a_full_projection_hands_the_whole_conversation_over() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Full),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    assert_eq!(result.last_agent().id().as_str(), "reviewer");
    assert_eq!(
        model.input_labels(1),
        ["message", "message", "tool_call", "handoff_output"]
    );
}

#[tokio::test]
async fn an_input_filter_narrows_what_the_declaration_allowed_through() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Full)
                .with_input_filter(Arc::new(KeepOnlyOpeningInput)),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The filter dropped both record lists, so only the caller's opening input survives, and the
    // transferring turn stores nothing; the receiving agent's answer is the only stored record.
    assert_eq!(model.input_labels(1), ["message"]);
    assert_eq!(result.state().generated_items().len(), 1);
    assert!(result.state().handoff_projection().unwrap().is_from_start());
}

#[tokio::test]
async fn the_projection_survives_a_checkpoint_and_governs_the_next_segment() {
    let model = script();
    let paused = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::None),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new()
            .with_max_turns(1)
            .with_error_handler(Arc::new(CheckpointCloseout)),
    ))
    .await
    .unwrap();

    // A host that continues the run itself is handed the transfer's view, not the segment's. The
    // convenience projection would otherwise be the one place the withheld history leaks out, with
    // nothing about the call saying so.
    let continuation = paused.continuation_input(ContinuationInput::PreserveAll);
    assert_eq!(
        continuation
            .iter()
            .map(ModelInputItem::label)
            .collect::<Vec<_>>(),
        ["tool_call", "handoff_output"]
    );

    // The first segment stopped on its turn cap, with control already transferred.
    assert_eq!(paused.state().current_agent().unwrap().as_str(), "reviewer");
    let mut restored: RunState =
        serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
    let (base, carried) = restored.model_input_base().unwrap();
    assert_eq!(base.len(), 2);
    assert!(carried.is_empty());

    // A continuation from that checkpoint builds its request from the projection rather than from
    // the history the session kept.
    restored.set_max_turns(Some(10));
    let resumed = Runner::run(
        RunRequest::new(
            AgentBinding::direct(reviewer(Vec::new())),
            Arc::new(FixedResolver {
                model: Arc::clone(&model),
            }),
            RunId::new("run-handoff"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_state(restored)
        .with_config(RunConfig::new()),
    )
    .await
    .unwrap();

    assert_eq!(resumed.final_text(), "审阅完成");
    assert_eq!(model.input_labels(1), ["tool_call", "handoff_output"]);
}

#[tokio::test]
async fn delivery_is_checked_by_the_agent_that_answered_not_the_one_that_started() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::None),
            vec![Arc::new(RecordingGuardrail {
                name: "planner_guard",
                seen: Arc::clone(&seen),
            })],
        ),
        reviewer(vec![Arc::new(RecordingGuardrail {
            name: "reviewer_guard",
            seen: Arc::clone(&seen),
        })]),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The answer belongs to the reviewer, and so does the promise about what may be in it. The
    // planner's check examines an answer it did not write, so it does not run.
    assert_eq!(*seen.lock().unwrap(), ["reviewer_guard"]);
    assert_eq!(result.output_guardrail_results().len(), 1);
}

#[tokio::test]
async fn a_target_the_registry_cannot_resolve_is_refused_before_the_model_is_called() {
    // The registry builder already refuses a declaration set with a missing target, so the gap a
    // run actually hits is the one this covers: an agent that declares a transfer while the run was
    // given no registry to resolve it against.
    let model = script();
    let error = Runner::run(RunRequest::new(
        AgentBinding::direct(planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::None),
            Vec::new(),
        )),
        Arc::new(FixedResolver {
            model: Arc::clone(&model),
        }),
        RunId::new("run-handoff"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("帮我处理这件事"))],
    ))
    .await
    .unwrap_err();

    // Advertising a transfer nothing can execute would spend a model call to reach a dead end.
    assert!(error.to_string().contains("reviewer"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_summary_projection_is_refused_before_it_is_advertised() {
    let model = script();
    let error = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Summary),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("summary history projection"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn typed_handoff_uses_the_named_history_projection() {
    let narrow = HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
        .with_history_projection(HistoryProjection::None);
    let wide = HandoffSpec::new(
        AgentId::new("reviewer"),
        ToolSchema::new(
            "transfer_full",
            json!({
                "type": "object", "properties": {}, "required": [], "additionalProperties": false
            }),
        )
        .unwrap(),
    )
    .with_history_projection(HistoryProjection::Full);
    let source = planner(narrow, Vec::new())
        .to_builder()
        .handoff(wide)
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![item(
            "transfer",
            RunItemKind::HandoffCall(
                ra_core::item::HandoffCall::new(
                    CallId::new("call"),
                    AgentId::new("reviewer"),
                    json!({"reason": "review"}),
                )
                .with_tool_name("transfer_to_reviewer"),
            ),
        )]),
        ModelResponse::new(vec![message("done", "done")]),
    ]);
    Runner::run(run_request(
        source,
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();
    assert_eq!(model.input_labels(1), ["handoff_call", "handoff_output"]);
}

struct ProjectedCloseout {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ra_runtime::runner::result::RunErrorHandler for ProjectedCloseout {
    async fn handle(
        &self,
        input: ra_runtime::runner::result::RunErrorHandlerInput<'_>,
    ) -> Result<Option<ra_runtime::runner::result::RunErrorHandlerResult>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.data().last_agent().id().as_str(), "reviewer");
        assert_eq!(
            input
                .data()
                .original_input()
                .iter()
                .map(ModelInputItem::label)
                .collect::<Vec<_>>(),
            ["tool_call", "handoff_output"]
        );
        assert!(input.data().new_items().is_empty());
        Ok(None)
    }
}

#[tokio::test]
async fn budget_closeout_respects_the_receiving_agents_projection() {
    let calls = Arc::new(AtomicUsize::new(0));
    let error = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::None),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &script(),
        RunConfig::new()
            .with_max_turns(1)
            .with_error_handler(Arc::new(ProjectedCloseout {
                calls: Arc::clone(&calls),
            })),
    ))
    .await
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.code(), "budget.max_turns");
}

/// The reference's `remove_all_tools` shape: drops every call and output it is handed.
struct RemoveAllTools;

fn is_tool_record(item: &RunItem) -> bool {
    matches!(
        item.kind(),
        RunItemKind::ToolCall(_)
            | RunItemKind::ToolCallOutput(_)
            | RunItemKind::HandoffCall(_)
            | RunItemKind::HandoffOutput(_)
    )
}

#[async_trait]
impl HandoffInputFilter for RemoveAllTools {
    async fn filter(
        &self,
        _context: &RunContext,
        data: HandoffInputData,
    ) -> Result<HandoffInputData> {
        let pre = data
            .pre_handoff_items()
            .iter()
            .filter(|item| !is_tool_record(item))
            .cloned()
            .collect();
        let new = data
            .new_items()
            .iter()
            .filter(|item| !is_tool_record(item))
            .cloned()
            .collect();
        Ok(data.with_pre_handoff_items(pre).with_new_items(new))
    }
}

/// A tool whose output a filter has to be able to keep out of the session.
struct SecretTool {
    approval: bool,
}

#[async_trait]
impl ra_core::tool::Tool for SecretTool {
    fn origin(&self) -> &ra_core::tool::ToolOrigin {
        static ORIGIN: std::sync::OnceLock<ra_core::tool::ToolOrigin> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(|| ra_core::tool::ToolOrigin::new("read_secret").unwrap())
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            ToolSchema::new(
                "read_secret",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap()
        })
    }
    fn options(&self) -> ra_core::tool::ToolOptions {
        let options = ra_core::tool::ToolOptions::default();
        if self.approval {
            options.with_approval(ra_core::tool::ToolApprovalPolicy::Always)
        } else {
            options
        }
    }
    async fn call(&self, _: ra_core::tool::ToolContext<'_>) -> Result<ra_core::tool::ToolOutput> {
        Ok(ra_core::tool::ToolOutput::text("SECRET-VALUE-42"))
    }
}

fn secret_call() -> RunItem {
    item(
        "secret-call",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("secret"),
            "read_secret",
            json!({}),
        )),
    )
}

/// A planner holding the secret tool, transferring through `handoff`.
fn secret_planner(handoff: HandoffSpec, approval: bool, deferring: bool) -> Arc<AgentSpec> {
    let builder = planner(handoff, Vec::new())
        .to_builder()
        .tool(Arc::new(SecretTool { approval }));
    let builder = if deferring {
        // Output guardrails with a stop policy that the turn's tools can satisfy: the shape in
        // which an interrupted turn's records stay out of the session until it is decided.
        builder
            .output_guardrails(vec![Arc::new(RecordingGuardrail {
                name: "fine",
                seen: Arc::new(Mutex::new(Vec::new())),
            }) as Arc<dyn OutputGuardrail>])
            .tool_use_behavior(ra_core::agent::ToolUseBehavior::stop_at_tools(["finish"]))
    } else {
        builder
    };
    builder.build().unwrap()
}

/// Runs to completion, returning the IDs and payloads the stream emitted.
async fn drive_recording(
    request: RunRequest,
    streamed: bool,
) -> (ra_runtime::runner::RunResult, Vec<RunItem>) {
    let (result, items) = drive_with_items(request, streamed).await;
    (result.unwrap(), items)
}

async fn drive_with_items(
    request: RunRequest,
    streamed: bool,
) -> (Result<ra_runtime::runner::RunResult>, Vec<RunItem>) {
    if !streamed {
        return (Runner::run(request).await, Vec::new());
    }
    let mut stream = Runner::run_streamed(request);
    let mut items = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let ra_runtime::runner::RunStreamEvent::Item(item) = event {
            items.push(item);
        }
    }
    (stream.finish().await, items)
}

#[derive(Default)]
struct RecordingRollout(Mutex<Vec<RolloutItem>>);

impl RecordingRollout {
    fn items(&self) -> Vec<RunItem> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|item| match item {
                RolloutItem::Item(item) => Some(item.clone()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl RolloutRecorder for RecordingRollout {
    fn record(&self, item: RolloutItem) {
        self.0.lock().unwrap().push(item);
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

fn mentions_secret<T: serde::Serialize + ?Sized>(value: &T) -> bool {
    serde_json::to_string(value)
        .unwrap()
        .contains("SECRET-VALUE-42")
}

/// The reference's `remove_all_tools` keeps tool data out of the session as well as out of the
/// receiving agent's input, while the stream has already carried the model's own records.
#[tokio::test]
async fn a_filter_that_removes_tool_records_keeps_them_out_of_the_session() {
    use ra_core::session::Session;
    for streamed in [false, true] {
        let target = reviewer(Vec::new());
        let source = secret_planner(
            target
                .as_handoff()
                .unwrap()
                .with_input_filter(Arc::new(RemoveAllTools)),
            false,
            false,
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let session = Arc::new(ra_session::InMemorySession::new("filtered-handoff"));
        let (result, emitted) = drive_recording(
            run_request(Arc::clone(&source), target, &model, RunConfig::new())
                .with_session(session.clone()),
            streamed,
        )
        .await;

        assert_eq!(result.final_text(), "reviewed");
        assert_eq!(model.input_labels(1), ["message"]);
        let stored = session.get_items(None).await.unwrap();
        assert!(!mentions_secret(&stored));
        assert!(!mentions_secret(result.state().generated_items()));
        assert!(stored.iter().all(|item| !is_tool_record(item)));
        assert!(result.state().handoff_projection().unwrap().is_from_start());
        if streamed {
            let ids: Vec<&str> = emitted.iter().map(|item| item.id().as_str()).collect();
            assert!(ids.contains(&"secret-call") && ids.contains(&"transfer-call"));
            assert!(!emitted.iter().any(mentions_secret));
        }
    }
}

/// `input_items` narrows the model input alone; the turn still stores the filter's `new_items`.
#[tokio::test]
async fn input_items_narrow_the_model_view_while_the_turn_stores_new_items() {
    use ra_core::session::Session;
    for streamed in [false, true] {
        let target = reviewer(Vec::new());
        let source = secret_planner(
            target.as_handoff().unwrap().with_input_filter_fn(
                |_: &RunContext, data: HandoffInputData| {
                    std::future::ready(Ok(data.with_input_items(Vec::new())))
                },
            ),
            false,
            false,
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let session = Arc::new(ra_session::InMemorySession::new("input-items-handoff"));
        let (result, _) = drive_recording(
            run_request(Arc::clone(&source), target, &model, RunConfig::new())
                .with_session(session.clone()),
            streamed,
        )
        .await;

        assert_eq!(result.final_text(), "reviewed");
        assert_eq!(model.input_labels(1), ["message"]);
        let stored = session.get_items(None).await.unwrap();
        assert!(mentions_secret(&stored));
        assert_eq!(stored.iter().filter(|item| is_tool_record(item)).count(), 4);
    }
}

/// A transfer resumed after approval filters the interrupted turn as the reference does: records
/// the session already received stay, and nothing the filter removed is written afterwards. With
/// output guardrails the turn was held back, so none of its tool records reach the session.
#[tokio::test]
async fn a_resumed_transfer_filters_only_what_the_session_has_not_received() {
    use ra_core::session::Session;
    for (deferring, streamed) in [(false, false), (false, true), (true, false), (true, true)] {
        let target = reviewer(Vec::new());
        let source = secret_planner(
            target
                .as_handoff()
                .unwrap()
                .with_input_filter(Arc::new(RemoveAllTools)),
            true,
            deferring,
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = RunConfig::new().with_agent_registry(
            AgentRegistry::builder()
                .register(Arc::clone(&source))
                .register(Arc::clone(&target))
                .build()
                .unwrap(),
        );
        let session = Arc::new(ra_session::InMemorySession::new("resumed-filtered-handoff"));
        let rollout = Arc::new(RecordingRollout::default());
        let (first, _) = drive_recording(
            run_request(
                Arc::clone(&source),
                Arc::clone(&target),
                &model,
                config.clone(),
            )
            .with_session(session.clone())
            .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        let ra_runtime::runner::RunOutcome::Interrupted { items } = first.outcome() else {
            panic!("expected approval");
        };
        let held = session.get_items(None).await.unwrap();
        assert_eq!(
            held.iter().filter(|item| is_tool_record(item)).count(),
            if deferring { 0 } else { 2 }
        );
        let mut checkpoint = first.state().clone();
        checkpoint.approve(&items[0], false).unwrap();
        let checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
        let (resumed, emitted) = drive_recording(
            RunRequest::new(
                AgentBinding::direct(source),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("run-handoff"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_state(checkpoint)
            .with_config(config)
            .with_session(session.clone())
            .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;

        assert_eq!(resumed.final_text(), "reviewed");
        assert_eq!(model.input_labels(1), ["message"]);
        let stored = session.get_items(None).await.unwrap();
        assert!(!mentions_secret(&stored), "deferring: {deferring}");
        // The eager path already stored both calls when the turn stopped; they stay, unanswered,
        // as the reference leaves them. The held path stored none of the turn's tool records.
        let ids: Vec<&str> = stored
            .iter()
            .filter(|item| is_tool_record(item))
            .map(|item| item.id().as_str())
            .collect();
        if deferring {
            assert!(ids.is_empty());
        } else {
            assert_eq!(ids, ["secret-call", "transfer-call"]);
        }
        assert!(!mentions_secret(resumed.state().generated_items()));
        assert!(!mentions_secret(&emitted));
        assert!(!emitted.iter().any(is_tool_record));
        let recorded = rollout.items();
        assert!(!mentions_secret(&recorded));
        assert!(!recorded.iter().any(|item| matches!(
            item.kind(),
            RunItemKind::ToolCallOutput(_) | RunItemKind::HandoffOutput(_)
        )));
        assert_eq!(resumed.new_items().last().unwrap().id().as_str(), "done");
    }
}

/// Two declarations may share a name and a target. Resume rebinds the one the interrupted turn
/// chose, not the first one with the same name and target.
#[tokio::test]
async fn a_resumed_transfer_runs_the_declaration_the_turn_bound() {
    for (first_enabled, streamed) in [(true, false), (true, true), (false, false)] {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let first_seen = Arc::clone(&first_calls);
        let second_seen = Arc::clone(&second_calls);
        let target = reviewer(Vec::new());
        let first = target
            .as_handoff()
            .unwrap()
            .with_on_handoff_fn(move |_| {
                first_seen.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            })
            .with_is_enabled(first_enabled);
        let second = target.as_handoff().unwrap().with_on_handoff_fn(move |_| {
            second_seen.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        });
        let source = planner(first, Vec::new())
            .to_builder()
            .handoff(second)
            .tool(Arc::new(SecretTool { approval: true }))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = RunConfig::new().with_agent_registry(
            AgentRegistry::builder()
                .register(Arc::clone(&source))
                .register(Arc::clone(&target))
                .build()
                .unwrap(),
        );
        let (paused, _) = drive_recording(
            run_request(
                Arc::clone(&source),
                Arc::clone(&target),
                &model,
                config.clone(),
            ),
            streamed,
        )
        .await;
        let ra_runtime::runner::RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval");
        };
        assert_eq!(
            paused.state().pending_handoff().unwrap().declarations(),
            [1]
        );
        let mut checkpoint = paused.state().clone();
        checkpoint.approve(&items[0], false).unwrap();
        let checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
        let (resumed, _) = drive_recording(
            RunRequest::new(
                AgentBinding::direct(source),
                Arc::new(FixedResolver {
                    model: Arc::clone(&model),
                }),
                RunId::new("run-handoff"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_state(checkpoint)
            .with_config(config),
            streamed,
        )
        .await;
        assert_eq!(resumed.final_text(), "reviewed");
        assert_eq!(first_calls.load(Ordering::SeqCst), 0);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn pairing_cleanup_preserves_records_withheld_from_the_filter() {
    use ra_core::session::Session;
    for streamed in [false, true] {
        let target = reviewer(Vec::new());
        let source = secret_planner(
            target
                .as_handoff()
                .unwrap()
                .with_history_projection(HistoryProjection::LastItems(1))
                .with_input_filter(Arc::new(RemoveAllTools)),
            false,
            false,
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let session = Arc::new(ra_session::InMemorySession::new("projected-filter"));
        let (result, emitted) = drive_recording(
            run_request(source, target, &model, RunConfig::new()).with_session(session.clone()),
            streamed,
        )
        .await;
        let ids: Vec<_> = result
            .state()
            .generated_items()
            .iter()
            .map(|item| item.id().as_str())
            .collect();
        // The projection handed only the tool output to the filter, so the call is protected.
        assert_eq!(ids, ["secret-call", "done"]);
        let stored = session.get_items(None).await.unwrap();
        assert!(
            stored
                .iter()
                .any(|item| item.id().as_str() == "secret-call")
        );
        assert!(!mentions_secret(&stored));
        assert!(!mentions_secret(&emitted));
        assert!(model.input_labels(1).is_empty());
    }
}

fn resume_request(
    source: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    config: RunConfig,
    checkpoint: RunState,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(source),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
        }),
        RunId::new("run-handoff"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_state(checkpoint)
    .with_config(config)
}

#[tokio::test]
async fn failed_resumed_filters_checkpoint_outputs_and_publish_only_the_retry_result() {
    for (redacting, streamed) in [(false, false), (false, true), (true, false), (true, true)] {
        let callbacks = Arc::new(AtomicUsize::new(0));
        let filters = Arc::new(AtomicUsize::new(0));
        let invoked = Arc::clone(&callbacks);
        let filtered = Arc::clone(&filters);
        let target = reviewer(Vec::new());
        let source = secret_planner(
            target
                .as_handoff()
                .unwrap()
                .with_on_handoff_fn(move |_| {
                    invoked.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(()))
                })
                .with_input_filter_fn(move |_: &RunContext, data: HandoffInputData| {
                    std::future::ready(if filtered.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(Error::caller("filter failed"))
                    } else if redacting {
                        Ok(data.with_new_items(vec![message("sanitized", "safe context")]))
                    } else {
                        Ok(data)
                    })
                }),
            true,
            false,
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = RunConfig::new().with_agent_registry(
            AgentRegistry::builder()
                .register(Arc::clone(&source))
                .register(Arc::clone(&target))
                .build()
                .unwrap(),
        );
        let rollout = Arc::new(RecordingRollout::default());
        let (paused, _) = drive_recording(
            run_request(Arc::clone(&source), target, &model, config.clone())
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        let mut checkpoint = paused.state().clone();
        let approval = checkpoint
            .pending_interruption_items()
            .next()
            .unwrap()
            .clone();
        checkpoint.approve(&approval, false).unwrap();
        let (failed, emitted) = drive_with_items(
            resume_request(Arc::clone(&source), &model, config.clone(), checkpoint)
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        let error = failed.unwrap_err();
        let checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap())
                .unwrap();
        assert!(mentions_secret(checkpoint.generated_items()));
        assert_eq!(
            checkpoint
                .pending_handoff()
                .unwrap()
                .unpublished_items()
                .len(),
            2
        );
        assert!(!mentions_secret(&emitted));
        assert!(
            !emitted
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::HandoffOutput(_)))
        );
        assert!(!mentions_secret(&rollout.items()));

        let (resumed, emitted) = drive_recording(
            resume_request(source, &model, config, checkpoint)
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        assert_eq!(resumed.final_text(), "reviewed");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(filters.load(Ordering::SeqCst), 2);
        assert_eq!(mentions_secret(&rollout.items()), !redacting);
        if streamed {
            assert_eq!(mentions_secret(&emitted), !redacting);
            assert_eq!(
                emitted
                    .iter()
                    .filter(|item| matches!(item.kind(), RunItemKind::HandoffOutput(_)))
                    .count(),
                usize::from(!redacting),
            );
            let unique: std::collections::HashSet<_> = emitted.iter().map(RunItem::id).collect();
            assert_eq!(unique.len(), emitted.len());
        }
        if redacting {
            assert!(
                rollout
                    .items()
                    .iter()
                    .any(|item| item.id().as_str() == "sanitized")
            );
        }
    }
}

#[tokio::test]
async fn staged_approval_outputs_are_published_once_before_the_transfer() {
    for streamed in [false, true] {
        let target = reviewer(Vec::new());
        let source = secret_planner(target.as_handoff().unwrap(), true, false);
        let second = item(
            "second-call",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("second"),
                "read_secret",
                json!({}),
            )),
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                secret_call(),
                second,
                transfer_call("transfer-call", "transfer"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = RunConfig::new().with_agent_registry(
            AgentRegistry::builder()
                .register(Arc::clone(&source))
                .register(Arc::clone(&target))
                .build()
                .unwrap(),
        );
        let rollout = Arc::new(RecordingRollout::default());
        let (paused, _) = drive_recording(
            run_request(Arc::clone(&source), target, &model, config.clone())
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        let mut checkpoint = paused.state().clone();
        let first = checkpoint.pending_interruption_items().find(|item| {
            matches!(item.kind(), RunItemKind::ToolApproval(approval) if approval.call_id().as_str() == "secret")
        }).unwrap().clone();
        checkpoint.approve(&first, false).unwrap();
        let (staged, first_emitted) = drive_recording(
            resume_request(Arc::clone(&source), &model, config.clone(), checkpoint)
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        assert!(
            staged
                .state()
                .pending_handoff()
                .unwrap()
                .unpublished_items()
                .is_empty()
        );
        assert_eq!(
            rollout
                .items()
                .iter()
                .filter(|item| item.id().as_str() == "secret.output")
                .count(),
            1
        );
        let mut checkpoint: RunState =
            serde_json::from_value(serde_json::to_value(staged.state()).unwrap()).unwrap();
        let second = checkpoint
            .pending_interruption_items()
            .next()
            .unwrap()
            .clone();
        checkpoint.approve(&second, false).unwrap();
        let (resumed, second_emitted) = drive_recording(
            resume_request(source, &model, config, checkpoint)
                .with_rollout_recorder(rollout.clone()),
            streamed,
        )
        .await;
        assert_eq!(resumed.final_text(), "reviewed");
        for id in ["secret.output", "second.output", "transfer.output"] {
            assert_eq!(
                rollout
                    .items()
                    .iter()
                    .filter(|item| item.id().as_str() == id)
                    .count(),
                1,
                "{id}"
            );
        }
        if streamed {
            assert!(
                first_emitted
                    .iter()
                    .any(|item| item.id().as_str() == "secret.output")
            );
            assert!(
                second_emitted
                    .iter()
                    .all(|item| item.id().as_str() != "secret.output")
            );
            assert!(
                second_emitted
                    .iter()
                    .any(|item| item.id().as_str() == "second.output")
            );
        }
    }
}

struct CheckpointCloseout;
#[async_trait]
impl ra_runtime::runner::result::RunErrorHandler for CheckpointCloseout {
    async fn handle(
        &self,
        _: ra_runtime::runner::result::RunErrorHandlerInput<'_>,
    ) -> Result<Option<ra_runtime::runner::result::RunErrorHandlerResult>> {
        Ok(Some(
            ra_runtime::runner::result::RunErrorHandlerResult::new(Message::assistant(
                "checkpoint",
                OutputPhase::Final,
            ))
            .with_write_to_history(false),
        ))
    }
}
