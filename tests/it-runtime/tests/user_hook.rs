//! Host hook effects are exercised through real runner, streaming, resume and event-sink paths.

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_context::{
    compaction::{CompactionCapability, anchor::AnchorRetention},
    window::ContextWindowConfig,
};
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior},
    cancel::{CancelReason, CancelScope},
    capability::{
        ContextProcessor, ContextProcessorRequest, ContextProcessorResult, ContextSummarizer,
    },
    context::RunContext,
    error::{Error, Result},
    event::{HostEventBody, InMemoryHostEventSink},
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, OutputGuardrail},
    hook::{
        CompactTrigger, HookDecision, HookEvent, HookEventName, HookReport, HookRunStatus,
        UserHook, UserHookContext,
    },
    item::{
        CallId, Compaction, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel, RunItemStreamEvent,
    },
    permission::{PermissionDecision, PermissionMode, PermissionRule},
    session::SessionId,
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        ToolServices,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    hook::{UserHookRegistration, UserHooks},
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct RecordingHook {
    decisions: Mutex<VecDeque<HookDecision>>,
    seen: Mutex<Vec<(HookEventName, Value)>>,
    delay: Option<Duration>,
    fail: bool,
    entered: Notify,
}

impl RecordingHook {
    fn new(decisions: impl IntoIterator<Item = HookDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
            delay: None,
            fail: false,
            entered: Notify::new(),
        }
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn register(self: &Arc<Self>, event: HookEventName) -> UserHookRegistration {
        UserHookRegistration::new(event, self.clone())
    }
}

#[async_trait]
impl UserHook for RecordingHook {
    fn name(&self) -> &str {
        "host callback"
    }
    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        assert!(
            !context.cancel().is_cancelled(),
            "cleanup hooks need a live scope"
        );
        let data = match event {
            HookEvent::PreToolUse(call) | HookEvent::PermissionRequest(call) => {
                json!({"call_id": call.call_id().as_str(), "arguments": call.arguments()})
            }
            HookEvent::PostToolUse { call, output } => {
                json!({"call_id":call.call_id().as_str(), "output":output.as_text()})
            }
            HookEvent::Stop(delivery) | HookEvent::SubagentStop { delivery, .. } => {
                json!({"active":delivery.stop_hook_active(), "message":delivery.message().map(Message::text_content), "tools":delivery.tool_outputs().len()})
            }
            HookEvent::Interrupt { reason } => json!({"reason": reason.code()}),
            HookEvent::SubagentStart { parent_run_id } => json!({"parent":parent_run_id.as_str()}),
            HookEvent::PreCompact { record_id, trigger } => {
                json!({"record":record_id.as_str(), "trigger":trigger.as_str()})
            }
            HookEvent::PostCompact {
                record_id,
                trigger,
                compaction,
            } => {
                json!({"record":record_id.as_str(), "trigger":trigger.as_str(), "covered":compaction.compacted_items().len()})
            }
            _ => Value::Null,
        };
        self.seen.lock().unwrap().push((event.name(), data));
        self.entered.notify_one();
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        if self.fail {
            return Err(Error::caller("host callback unavailable"));
        }
        Ok(self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
}

struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    input: Mutex<Vec<Vec<ModelInputItem>>>,
}
impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            input: Mutex::new(Vec::new()),
        })
    }
    fn next(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.input.lock().unwrap().push(request.input().to_vec());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("model script exhausted"))
    }
    fn calls(&self) -> usize {
        self.input.lock().unwrap().len()
    }
}
#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.next(request)
    }
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        match self.next(request) {
            Ok(response) => {
                let mut events: Vec<_> = response
                    .output()
                    .iter()
                    .cloned()
                    .map(|item| {
                        Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                            "item", item,
                        )))
                    })
                    .collect();
                events.push(Ok(ModelStreamEvent::Completed(Box::new(response))));
                stream::iter(events).boxed()
            }
            Err(error) => stream::iter([Err(error)]).boxed(),
        }
    }
}
struct FixedResolver(Arc<ScriptedModel>);
impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct CountingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: AtomicUsize,
}
impl CountingTool {
    fn new(approval: ToolApprovalPolicy) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("write_file").unwrap(),
            schema: ToolSchema::new(
                "write_file",
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            )
            .unwrap(),
            options: ToolOptions::new().with_approval(approval),
            calls: AtomicUsize::new(0),
        })
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
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("complete tool output"))
    }
}
fn agent(tool: Option<Arc<CountingTool>>, stop_on_tool: bool) -> Arc<AgentSpec> {
    let mut builder = AgentSpec::builder()
        .id(AgentId::new("agent"))
        .name("Agent")
        .instructions("perform the task");
    if let Some(tool) = tool {
        builder = builder.tools(vec![tool as Arc<dyn Tool>]);
    }
    if stop_on_tool {
        builder = builder.tool_use_behavior(ToolUseBehavior::StopOnFirstTool);
    }
    builder.build().unwrap()
}
fn request(
    model: &Arc<ScriptedModel>,
    agent: Arc<AgentSpec>,
    config: RunConfig,
    cancel: CancelScope,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver(model.clone())),
        RunId::new("hook-run"),
        cancel,
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_config(config)
}
fn response(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}
fn tool_response() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("call-item"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "write_file",
            json!({"path":"file.txt"}),
        )),
    )])
}
fn two_tool_responses() -> ModelResponse {
    ModelResponse::new(
        ["call-1", "call-2"]
            .into_iter()
            .map(|call| {
                RunItem::new(
                    ItemId::new(format!("item-{call}")),
                    RunItemKind::ToolCall(ToolCall::new(
                        CallId::new(call),
                        "write_file",
                        json!({"path":"file.txt"}),
                    )),
                )
            })
            .collect(),
    )
}
fn block(prompt: &str) -> HookDecision {
    HookDecision::Block {
        prompt: prompt.into(),
    }
}
fn deny(message: &str) -> HookDecision {
    HookDecision::Deny {
        message: message.into(),
    }
}
fn reports(sink: &InMemoryHostEventSink) -> Vec<HookReport> {
    sink.events()
        .iter()
        .filter_map(|event| match event.body() {
            HostEventBody::Hook(report) => Some(report.clone()),
            _ => None,
        })
        .collect()
}
fn services(sink: &Arc<InMemoryHostEventSink>) -> ToolServices {
    ToolServices::new().with_event_sink(sink.clone())
}
fn has_user_prompt(input: &[ModelInputItem], prompt: &str) -> bool {
    input.iter().any(|item| matches!(item, ModelInputItem::Message(message) if message == &Message::user(prompt)))
}

#[tokio::test]
async fn pre_tool_denial_answers_the_call_without_running_or_asking_approval() {
    let hook = Arc::new(RecordingHook::new([deny("choose another file")]));
    let permission = Arc::new(RecordingHook::new([HookDecision::Allow]));
    let tool = CountingTool::new(ToolApprovalPolicy::Always);
    let model = ScriptedModel::new(vec![tool_response(), response("final", "done")]);
    let sink = Arc::new(InMemoryHostEventSink::new());
    let config = RunConfig::new()
        .with_user_hook(hook.register(HookEventName::PreToolUse))
        .with_user_hook(permission.register(HookEventName::PermissionRequest));
    let result = Runner::run(
        request(
            &model,
            agent(Some(tool.clone()), false),
            config,
            CancelScope::root(),
        )
        .with_services(services(&sink)),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(tool.calls(), 0);
    assert_eq!(permission.count(), 0);
    assert_eq!(
        hook.seen.lock().unwrap()[0].1["arguments"],
        json!({"path":"file.txt"})
    );
    assert!(result.new_items().iter().any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(output) if output.is_error() && output.call_id().as_str() == "call-1")));
    assert_eq!(reports(&sink)[0].call_id().unwrap().as_str(), "call-1");
}

#[tokio::test]
async fn permission_hook_grants_only_pending_approval_and_policy_denial_wins() {
    for denied in [false, true] {
        let hook = Arc::new(RecordingHook::new([HookDecision::Allow]));
        let tool = CountingTool::new(ToolApprovalPolicy::Always);
        let model = ScriptedModel::new(vec![tool_response(), response("final", "done")]);
        let mut config =
            RunConfig::new().with_user_hook(hook.register(HookEventName::PermissionRequest));
        if denied {
            config = config.with_permission_rules([PermissionRule::new(PermissionDecision::Deny)]);
        }
        let result = Runner::run(request(
            &model,
            agent(Some(tool.clone()), false),
            config,
            CancelScope::root(),
        ))
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(tool.calls(), usize::from(!denied));
        assert_eq!(hook.count(), usize::from(!denied));
    }
}

#[tokio::test]
async fn permission_denial_beats_a_peer_grant_and_dont_ask_is_not_overridden() {
    for dont_ask in [false, true] {
        let allow = Arc::new(RecordingHook::new([HookDecision::Allow]));
        let refuse = Arc::new(RecordingHook::new([deny("blocked by host")]));
        let tool = CountingTool::new(ToolApprovalPolicy::Always);
        let model = ScriptedModel::new(vec![tool_response(), response("final", "done")]);
        let mut config = RunConfig::new()
            .with_user_hook(allow.register(HookEventName::PermissionRequest))
            .with_user_hook(refuse.register(HookEventName::PermissionRequest));
        if dont_ask {
            config = config.with_permission_mode(PermissionMode::DontAsk);
        }
        Runner::run(request(
            &model,
            agent(Some(tool.clone()), false),
            config,
            CancelScope::root(),
        ))
        .await
        .unwrap();
        assert_eq!(tool.calls(), 0);
        assert_eq!(refuse.count(), usize::from(!dont_ask));
    }
}

#[tokio::test]
async fn streamed_tools_fire_pre_and_post_once_and_post_has_no_control_effect() {
    let pre = Arc::new(RecordingHook::new([]));
    let post = Arc::new(RecordingHook::new([deny("invalid post-tool block")]));
    let tool = CountingTool::new(ToolApprovalPolicy::Never);
    let model = ScriptedModel::new(vec![tool_response(), response("final", "done")]);
    let sink = Arc::new(InMemoryHostEventSink::new());
    let config = RunConfig::new()
        .with_partial_messages(true)
        .with_user_hook(pre.register(HookEventName::PreToolUse))
        .with_user_hook(post.register(HookEventName::PostToolUse));
    let result = Runner::run_streamed(
        request(
            &model,
            agent(Some(tool.clone()), false),
            config,
            CancelScope::root(),
        )
        .with_services(services(&sink)),
    )
    .finish()
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(tool.calls(), 1);
    assert_eq!(pre.count(), 1);
    assert_eq!(post.count(), 1);
    assert_eq!(
        post.seen.lock().unwrap()[0].1["output"],
        "complete tool output"
    );
    assert!(
        reports(&sink)
            .iter()
            .any(|report| report.status() == HookRunStatus::Ignored)
    );
}

#[tokio::test]
async fn stop_continuation_enters_history_and_can_block_more_than_once() {
    let hook = Arc::new(RecordingHook::new([
        block("check the result"),
        block("check again"),
    ]));
    let model = ScriptedModel::new(vec![
        response("first", "candidate"),
        response("second", "candidate again"),
        response("third", "done"),
    ]);
    let config = RunConfig::new().with_user_hook(hook.register(HookEventName::Stop));
    let result = Runner::run(request(
        &model,
        agent(None, false),
        config,
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(model.calls(), 3);
    let seen = hook.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .map(|(_, data)| data["active"].as_bool().unwrap())
            .collect::<Vec<_>>(),
        [false, true, true]
    );
    assert!(has_user_prompt(
        &model.input.lock().unwrap()[1],
        "check the result"
    ));
    assert!(has_user_prompt(
        &model.input.lock().unwrap()[2],
        "check again"
    ));
    assert!(result.state().stop_hook_active());
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    assert!(restored.stop_hook_active());
    assert_eq!(result.turn_records()[0].next_step_code(), "run_again");
    // Two continuations in one run, so the identity scheme has to separate them without help.
    let continuations: Vec<_> = result
        .new_items()
        .iter()
        .filter(|item| item.id().as_str().starts_with("hook-continuation-"))
        .map(|item| item.id().as_str())
        .collect();
    assert_eq!(continuations.len(), 2);
    assert_ne!(continuations[0], continuations[1]);
}

#[tokio::test]
async fn empty_stop_prompt_warns_and_the_original_delivery_stands() {
    let hook = Arc::new(RecordingHook::new([block(" \n ")]));
    let sink = Arc::new(InMemoryHostEventSink::new());
    let model = ScriptedModel::new(vec![response("final", "done")]);
    let result = Runner::run(
        request(
            &model,
            agent(None, false),
            RunConfig::new().with_user_hook(hook.register(HookEventName::Stop)),
            CancelScope::root(),
        )
        .with_services(services(&sink)),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert!(!result.state().stop_hook_active());
    let report = reports(&sink).pop().unwrap();
    assert_eq!(report.status(), HookRunStatus::Ignored);
    assert!(report.warning().unwrap().contains("without a prompt"));
    let event = sink.events().pop().unwrap();
    let restored: ra_core::event::HostEvent =
        serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    assert_eq!(restored.body(), event.body());
}

#[tokio::test]
async fn repeated_stop_blocks_consume_the_existing_turn_budget() {
    let hook = Arc::new(RecordingHook::new([block("more"), block("more")]));
    let model = ScriptedModel::new(vec![
        response("first", "candidate"),
        response("second", "candidate"),
    ]);
    let config = RunConfig::new()
        .with_max_turns(2)
        .with_user_hook(hook.register(HookEventName::Stop));
    let result = Runner::run(request(
        &model,
        agent(None, false),
        config,
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(model.calls(), 2);
    assert_eq!(hook.count(), 2);
    assert!(
        result.final_message().is_none(),
        "blocked candidates are not budget closeouts"
    );
    assert!(!result.outcome().finish_reason().unwrap().is_complete());
}

struct FinalCheck(Mutex<Vec<String>>);
#[async_trait]
impl OutputGuardrail for FinalCheck {
    fn name(&self) -> &str {
        "final check"
    }
    async fn check(
        &self,
        _: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        self.0.lock().unwrap().push(output.text());
        Ok(GuardrailFunctionOutput::pass())
    }
}

#[tokio::test]
async fn stop_on_tool_can_continue_and_only_actual_delivery_gets_output_guardrails() {
    let hook = Arc::new(RecordingHook::new([block("explain the result")]));
    let guardrail = Arc::new(FinalCheck(Mutex::new(Vec::new())));
    let tool = CountingTool::new(ToolApprovalPolicy::Never);
    let model = ScriptedModel::new(vec![tool_response(), response("final", "done")]);
    let config = RunConfig::new()
        .with_user_hook(hook.register(HookEventName::Stop))
        .with_output_guardrail(guardrail.clone());
    let result = Runner::run(request(
        &model,
        agent(Some(tool), true),
        config,
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(hook.seen.lock().unwrap()[0].1["tools"], 1);
    assert_eq!(*guardrail.0.lock().unwrap(), ["done"]);
}

#[tokio::test]
async fn child_start_and_stop_state_survive_approval_resume() {
    let start = Arc::new(RecordingHook::new([]));
    let stop = Arc::new(RecordingHook::new([block("use the tool")]));
    let root_stop = Arc::new(RecordingHook::new([]));
    let pre = Arc::new(RecordingHook::new([]));
    let config = RunConfig::new()
        .with_user_hook(start.register(HookEventName::SubagentStart))
        .with_user_hook(stop.register(HookEventName::SubagentStop))
        .with_user_hook(root_stop.register(HookEventName::Stop))
        .with_user_hook(pre.register(HookEventName::PreToolUse));
    let tool = CountingTool::new(ToolApprovalPolicy::Always);
    let agent = agent(Some(tool.clone()), false);
    let first = ScriptedModel::new(vec![response("first", "candidate"), tool_response()]);
    let interrupted = Runner::run(
        request(&first, agent.clone(), config.clone(), CancelScope::root())
            .with_parent_run_id(RunId::new("parent"))
            .unwrap(),
    )
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("approval expected")
    };
    assert_eq!(start.count(), 1);
    assert_eq!(stop.count(), 1);
    let mut state: RunState =
        serde_json::from_value(serde_json::to_value(interrupted.state()).unwrap()).unwrap();
    assert!(state.stop_hook_active());
    state.approve(&items[0], false).unwrap();
    let second = ScriptedModel::new(vec![response("final", "done")]);
    let resumed = RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver(second)),
        RunId::new("ignored"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_config(config)
    .with_state(state);
    let result = Runner::run(resumed).await.unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(start.count(), 1);
    assert_eq!(root_stop.count(), 0);
    assert_eq!(
        pre.count(),
        2,
        "pre-tool checks repeat before approved execution"
    );
    assert_eq!(stop.seen.lock().unwrap()[1].1["active"], true);
    assert_eq!(tool.calls(), 1);
    assert_eq!(result.state().parent_run_id().unwrap().as_str(), "parent");
}

#[tokio::test]
async fn hook_failures_and_timeouts_are_reported_and_do_not_grant_permission() {
    for timed_out in [false, true] {
        let mut hook = RecordingHook::new([HookDecision::Allow]);
        hook.fail = !timed_out;
        if timed_out {
            hook.delay = Some(Duration::from_secs(60));
        }
        let hook = Arc::new(hook);
        let sink = Arc::new(InMemoryHostEventSink::new());
        let config = RunConfig::new().with_user_hook(
            hook.register(HookEventName::PermissionRequest)
                .with_timeout(Duration::from_millis(10)),
        );
        let model = ScriptedModel::new(vec![tool_response()]);
        let tool = CountingTool::new(ToolApprovalPolicy::Always);
        let result = Runner::run(
            request(
                &model,
                agent(Some(tool.clone()), false),
                config,
                CancelScope::root(),
            )
            .with_services(services(&sink)),
        )
        .await
        .unwrap();
        assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
        assert_eq!(tool.calls(), 0);
        assert_eq!(
            reports(&sink)[0].status(),
            if timed_out {
                HookRunStatus::TimedOut
            } else {
                HookRunStatus::Failed
            }
        );
    }
}

#[tokio::test]
async fn cancellation_drops_a_waiting_hook_and_notifies_interrupt_without_reviving_the_run() {
    let mut pending = RecordingHook::new([]);
    pending.delay = Some(Duration::from_secs(60));
    let pending = Arc::new(pending);
    let interrupt = Arc::new(RecordingHook::new([block("must be ignored")]));
    let sink = Arc::new(InMemoryHostEventSink::new());
    let config = RunConfig::new()
        .with_user_hook(pending.register(HookEventName::Stop))
        .with_user_hook(interrupt.register(HookEventName::Interrupt));
    let model = ScriptedModel::new(vec![response("first", "candidate")]);
    let cancel = CancelScope::root();
    let task = tokio::spawn(Runner::run(
        request(&model, agent(None, false), config, cancel.clone()).with_services(services(&sink)),
    ));
    pending.entered.notified().await;
    cancel.cancel(CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.is_cancelled());
    assert_eq!(interrupt.count(), 1);
    assert_eq!(
        interrupt.seen.lock().unwrap()[0].1["reason"],
        "user_interrupt"
    );
    assert_eq!(model.calls(), 1);
    assert!(
        reports(&sink)
            .iter()
            .any(|report| report.status() == HookRunStatus::Cancelled)
    );
    assert!(
        reports(&sink)
            .iter()
            .any(|report| report.event() == HookEventName::Interrupt
                && report.status() == HookRunStatus::Ignored)
    );
}

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

/// Both compact events describe an attempt that really happens. The processor checks its own
/// threshold first, so a run that never crosses it announces nothing, and post-compact follows
/// only a summary that was actually produced — under the same record identity pre-compact named.
#[tokio::test]
async fn compact_events_announce_a_real_attempt_under_one_record_identity() {
    for over_threshold in [true, false] {
        let hook = Arc::new(RecordingHook::new([]));
        let tool = CountingTool::new(ToolApprovalPolicy::Never);
        let model = ScriptedModel::new(if over_threshold {
            vec![
                two_tool_responses(),
                response("summary-1", COMPACTION_SUMMARY_JSON),
                response("final", "done"),
            ]
        } else {
            vec![response("final", "done")]
        });
        let compaction = CompactionCapability::new(
            ContextWindowConfig::default(),
            AnchorRetention::new(0, 0, 1).unwrap(),
            Some(3),
            None,
        )
        .unwrap();
        let config = RunConfig::new()
            .with_context_processor(Arc::new(compaction))
            .with_user_hook(hook.register(HookEventName::PreCompact))
            .with_user_hook(hook.register(HookEventName::PostCompact));
        let result = Runner::run(request(
            &model,
            agent(Some(tool), false),
            config,
            CancelScope::root(),
        ))
        .await
        .unwrap();

        assert_eq!(result.final_text(), "done");
        let seen = hook.seen.lock().unwrap();
        if !over_threshold {
            assert!(seen.is_empty(), "no attempt, no announcement");
            continue;
        }
        assert_eq!(
            seen.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [HookEventName::PreCompact, HookEventName::PostCompact]
        );
        assert_eq!(seen[0].1["trigger"], "automatic");
        assert_eq!(seen[0].1["record"], seen[1].1["record"]);
        assert_eq!(seen[1].1["covered"], 4);
        let record = result
            .new_items()
            .iter()
            .find(|item| matches!(item.kind(), RunItemKind::Compaction(_)))
            .expect("the compaction record reaches history");
        assert_eq!(seen[0].1["record"], record.id().as_str());
    }
}

/// A processor answering an explicit user request announces `manual`; nothing in the framework
/// produces that trigger for it. Neither compact event carries a control effect, so a decision
/// returned here is reported and ignored rather than changing the projection.
struct ManualCompactor;
#[async_trait]
impl ContextProcessor for ManualCompactor {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        request.notify_pre_compact(CompactTrigger::Manual).await?;
        let compaction = Compaction::new("a summary the host produced", Vec::new());
        request
            .notify_post_compact(CompactTrigger::Manual, &compaction)
            .await?;
        Ok(ContextProcessorResult::new(request.input().to_vec()))
    }
}

#[tokio::test]
async fn a_host_processor_announces_a_manual_compaction_and_gets_no_control_effect() {
    let hook = Arc::new(RecordingHook::new([block("keep working"), deny("stop")]));
    let sink = Arc::new(InMemoryHostEventSink::new());
    let model = ScriptedModel::new(vec![response("final", "done")]);
    let config = RunConfig::new()
        .with_context_processor(Arc::new(ManualCompactor))
        .with_user_hook(hook.register(HookEventName::PreCompact))
        .with_user_hook(hook.register(HookEventName::PostCompact));
    let result = Runner::run(
        request(&model, agent(None, false), config, CancelScope::root())
            .with_services(services(&sink)),
    )
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    assert_eq!(hook.seen.lock().unwrap()[0].1["trigger"], "manual");
    assert_eq!(model.calls(), 1);
    assert!(
        reports(&sink)
            .iter()
            .all(|report| report.status() == HookRunStatus::Ignored
                && report.decision() == &HookDecision::Continue)
    );
}

#[tokio::test]
async fn session_and_prompt_events_are_explicit_host_boundaries_and_observational() {
    let hook = Arc::new(RecordingHook::new([
        block("invalid"),
        HookDecision::Allow,
        deny("invalid"),
    ]));
    let hooks = UserHooks::default()
        .with_hook(hook.register(HookEventName::SessionStart))
        .with_hook(hook.register(HookEventName::UserPromptSubmit))
        .with_hook(hook.register(HookEventName::SessionEnd));
    let model = ScriptedModel::new(vec![response("final", "done")]);
    let agent = agent(None, false);
    Runner::run(request(
        &model,
        agent.clone(),
        RunConfig::new().with_user_hooks(hooks.clone()),
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(hook.count(), 0, "a run must not masquerade as a session");
    let bound = hooks.bind(
        Arc::new(RunContext::new(RunId::new("host-run"), &agent)),
        CancelScope::root(),
        ToolServices::new(),
    );
    let session_id = SessionId::new("session");
    let input = [ModelInputItem::Message(Message::user("new prompt"))];
    for event in [
        HookEvent::SessionStart {
            session_id: &session_id,
            resumed: false,
        },
        HookEvent::UserPromptSubmit { input: &input },
        HookEvent::SessionEnd {
            session_id: &session_id,
            reason: "closed",
        },
    ] {
        assert_eq!(bound.dispatch(event).await.unwrap(), HookDecision::Continue);
    }
    assert_eq!(hook.count(), 3);
}

#[tokio::test]
async fn blocked_candidate_is_not_delivered_after_tool_stop() {
    let hook = Arc::new(RecordingHook::new([block("use the tool instead")]));
    let guardrail = Arc::new(FinalCheck(Mutex::new(Vec::new())));
    let tool = CountingTool::new(ToolApprovalPolicy::Never);
    let model = ScriptedModel::new(vec![
        response("candidate", "rejected candidate"),
        tool_response(),
    ]);
    let result = Runner::run(request(
        &model,
        agent(Some(tool), true),
        RunConfig::new()
            .with_user_hook(hook.register(HookEventName::Stop))
            .with_output_guardrail(guardrail.clone()),
        CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(hook.seen.lock().unwrap()[1].1["message"], Value::Null);
    assert!(result.final_message().is_none());
    assert_eq!(result.final_text(), "");
    let checked = guardrail.0.lock().unwrap();
    assert_eq!(checked.len(), 1);
    assert!(!checked[0].contains("rejected candidate"));
    assert!(checked[0].contains("complete tool output"));
    assert!(result.state().generated_items().iter().any(|item| matches!(
        item.kind(), RunItemKind::Message(message) if message.text_content() == "rejected candidate"
    )), "the blocked candidate remains in history");
}
