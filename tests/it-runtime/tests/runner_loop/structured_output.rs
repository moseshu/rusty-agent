//! Final-output settlement, recovery, and persisted continuation.

use super::*;
use ra_core::{
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, OutputGuardrail},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    lifecycle::{AgentEndInput, LifecycleHook, LifecycleScope},
    output::{OutputSchema, OutputValue},
};
use ra_runtime::hook::UserHookRegistration;
use ra_session::{InMemorySession, Session};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
struct FinalAnswer {
    summary: String,
}

fn typed_request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    policy: ToolUseBehavior,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .tools(tools)
        .tool_use_behavior(policy)
        .output_type::<FinalAnswer>()
        .unwrap()
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver {
            model: model.clone(),
            selectors: Arc::new(Mutex::new(Vec::new())),
        }),
        RunId::new("typed-run"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("answer"))],
    )
}

/// A plain agent declaring `schema` itself, which the runner validates.
fn declared_request(
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
    schema: OutputSchema,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .output_schema(schema)
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver {
            model: model.clone(),
            selectors: Arc::new(Mutex::new(Vec::new())),
        }),
        RunId::new("declared-run"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("answer"))],
    )
}

async fn drive(request: RunRequest, streamed: bool) -> Result<(RunResult, Vec<RunStreamEvent>)> {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        let mut events = Vec::new();
        while let Some(event) = stream.next_event().await {
            events.push(event);
        }
        Ok((stream.finish().await?, events))
    } else {
        Ok((Runner::run(request).await?, Vec::new()))
    }
}

struct Recovery {
    text: Option<String>,
    history: bool,
    codes: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl RunErrorHandler for Recovery {
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        assert_eq!(input.context().agent().name(), "Coder");
        self.codes
            .lock()
            .unwrap()
            .push(input.error().code().to_owned());
        assert!(!input.data().model_responses().is_empty());
        if input.error().code() == "output.invalid_final_output" {
            assert!(
                input
                    .data()
                    .new_items()
                    .iter()
                    .any(|item| matches!(item.kind(), RunItemKind::Message(_)))
                    || input
                        .data()
                        .model_responses()
                        .last()
                        .unwrap()
                        .output()
                        .is_empty()
            );
        }
        Ok(self.text.as_ref().map(|text| {
            RunErrorHandlerResult::new(Message::assistant(text, OutputPhase::Final))
                .with_write_to_history(self.history)
        }))
    }
}
fn recovery(text: Option<&str>, history: bool, codes: &Arc<Mutex<Vec<String>>>) -> Arc<Recovery> {
    Arc::new(Recovery {
        text: text.map(str::to_owned),
        history,
        codes: codes.clone(),
    })
}

#[tokio::test]
async fn validates_before_delivery_in_ordinary_and_streamed_runs() {
    for streamed in [false, true] {
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
            "answer",
            r#"{"summary":"done"}"#,
        )])]);
        let (result, events) = drive(
            typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(
            result.final_output_as::<FinalAnswer>().unwrap().summary,
            "done"
        );
        assert_eq!(
            result.final_output().unwrap().value(),
            &json!({"summary":"done"})
        );
        assert!(result.final_output_as::<Vec<String>>().is_err());
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(model.request_surfaces.lock().unwrap()[0].2);
        if streamed {
            assert!(matches!(events.last(), Some(RunStreamEvent::Finished(_))));
        }
    }
}

#[tokio::test]
async fn invalid_json_type_and_wrapper_fail_without_retry_and_preserve_paid_response() {
    for streamed in [false, true] {
        for invalid in [
            "not JSON",
            r#"{"summary":123}"#,
            r#"{"unexpected":"value"}"#,
        ] {
            let model =
                ScriptedModel::new(vec![ModelResponse::new(vec![message("invalid", invalid)])]);
            let error = drive(
                typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain),
                streamed,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code(), "output.invalid_final_output");
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(error.run_state().unwrap().model_responses().len(), 1);
            assert_eq!(error.run_state().unwrap().generated_items().len(), 1);
        }
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
            "invalid", r#"["a"]"#,
        )])]);
        let request = declared_request(
            &model,
            &CancelScope::root(),
            OutputSchema::for_type::<Vec<String>>().unwrap(),
        );
        assert_eq!(
            drive(request, streamed).await.unwrap_err().code(),
            "output.invalid_final_output"
        );
    }
}

#[tokio::test]
async fn empty_structured_output_continues_or_recovers_without_extra_turn() {
    for streamed in [false, true] {
        for items in [Vec::new(), vec![message("empty", "")]] {
            let model = ScriptedModel::new(vec![
                ModelResponse::new(items.clone()),
                ModelResponse::new(vec![message("answer", r#"{"summary":"done"}"#)]),
            ]);
            let (result, _) = drive(
                typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), 2);
            assert_eq!(result.turn_records()[0].next_step_code(), "run_again");
            assert_eq!(
                result.final_output_as::<FinalAnswer>().unwrap().summary,
                "done"
            );
            if !items.is_empty() {
                assert_eq!(
                    phases(&result),
                    [OutputPhase::Commentary, OutputPhase::Final]
                );
            }
            let model = ScriptedModel::new(vec![ModelResponse::new(items)]);
            let codes = Arc::new(Mutex::new(Vec::new()));
            let (result, _) = drive(
                typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
                    RunConfig::new().with_error_handler(recovery(
                        Some(r#"{"summary":"fallback"}"#),
                        true,
                        &codes,
                    )),
                ),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                result.final_output_as::<FinalAnswer>().unwrap().summary,
                "fallback"
            );
            assert_eq!(*codes.lock().unwrap(), ["output.invalid_final_output"]);
        }
    }
}

#[tokio::test]
async fn recovered_output_is_validated_and_history_policy_controls_exact_stream_event() {
    for streamed in [false, true] {
        for history in [false, true] {
            let model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
                "invalid", "not JSON",
            )])]);
            let session = Arc::new(InMemorySession::new("typed-session"));
            let codes = Arc::new(Mutex::new(Vec::new()));
            let (result, events) = drive(
                typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain)
                    .with_session(session.clone())
                    .with_config(RunConfig::new().with_error_handler(recovery(
                        Some(r#"{"summary":"safe fallback"}"#),
                        history,
                        &codes,
                    ))),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(
                result.final_output_as::<FinalAnswer>().unwrap().summary,
                "safe fallback"
            );
            assert_eq!(result.final_text(), r#"{"summary":"safe fallback"}"#);
            assert_eq!(result.new_items().len(), if history { 2 } else { 1 });
            if streamed {
                let fallbacks = events.iter().filter(|event| match event {
                    RunStreamEvent::Item(item) => matches!(item.kind(), RunItemKind::Message(message) if message.text_content().contains("safe fallback")),
                    RunStreamEvent::FinalMessage(message) => message.text_content().contains("safe fallback"),
                    _ => false,
                }).count();
                assert_eq!(fallbacks, 1);
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, RunStreamEvent::FinalMessage(_)))
                        .count(),
                    usize::from(!history)
                );
            }
            let stored = session.get_items(None).await.unwrap();
            assert_eq!(stored.len(), if history { 3 } else { 2 });
        }
        for fallback in [None, Some("still not JSON"), Some(r#"{"summary":4}"#)] {
            let model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
                "invalid", "not JSON",
            )])]);
            let codes = Arc::new(Mutex::new(Vec::new()));
            let error = drive(
                typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
                    RunConfig::new().with_error_handler(recovery(fallback, true, &codes)),
                ),
                streamed,
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.code(),
                if fallback.is_none() {
                    "output.invalid_final_output"
                } else {
                    "caller"
                }
            );
            assert_eq!(codes.lock().unwrap().len(), 1);
        }
    }
}

struct Observers {
    seen: Arc<Mutex<Vec<&'static str>>>,
    expected: &'static str,
    trip: bool,
}
#[async_trait]
impl LifecycleHook for Observers {
    fn name(&self) -> &str {
        "typed-observer"
    }
    async fn on_agent_end(&self, _: LifecycleScope, input: &AgentEndInput<'_>) -> Result<()> {
        assert_eq!(
            input
                .output()
                .unwrap()
                .downcast_ref::<FinalAnswer>()
                .unwrap()
                .summary,
            self.expected
        );
        self.seen.lock().unwrap().push("end");
        Ok(())
    }
}
#[async_trait]
impl OutputGuardrail for Observers {
    fn name(&self) -> &str {
        "typed-output"
    }
    async fn check(
        &self,
        _: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        assert_eq!(
            output
                .output()
                .unwrap()
                .downcast_ref::<FinalAnswer>()
                .unwrap()
                .summary,
            self.expected
        );
        self.seen.lock().unwrap().push("guardrail");
        Ok(if self.trip {
            GuardrailFunctionOutput::tripwire()
        } else {
            GuardrailFunctionOutput::pass()
        })
    }
}

#[tokio::test]
async fn recovery_reaches_end_hooks_and_guardrails_with_the_same_decoded_value() {
    for trip in [false, true] {
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("invalid", "bad")])]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::new(Observers {
            seen: seen.clone(),
            expected: "fallback",
            trip,
        });
        let codes = Arc::new(Mutex::new(Vec::new()));
        let result = Runner::run(
            typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
                RunConfig::new()
                    .with_error_handler(recovery(Some(r#"{"summary":"fallback"}"#), false, &codes))
                    .with_lifecycle_hook(observer.clone())
                    .with_output_guardrail(observer),
            ),
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), ["end", "guardrail"]);
        if trip {
            assert_eq!(result.unwrap_err().code(), "guardrail.output");
        } else {
            assert!(result.is_ok());
        }
    }
}

#[tokio::test]
async fn a_guardrail_refusal_does_not_stream_an_unrecorded_recovery() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("invalid", "bad")])]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(Observers {
        seen: seen.clone(),
        expected: "fallback",
        trip: true,
    });
    let codes = Arc::new(Mutex::new(Vec::new()));
    let mut stream = Runner::run_streamed(
        typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
            RunConfig::new()
                .with_error_handler(recovery(Some(r#"{"summary":"fallback"}"#), false, &codes))
                .with_lifecycle_hook(observer.clone())
                .with_output_guardrail(observer),
        ),
    );
    while let Some(event) = stream.next_event().await {
        assert!(!matches!(event, RunStreamEvent::FinalMessage(_)));
    }
    assert_eq!(
        stream.finish().await.unwrap_err().code(),
        "guardrail.output"
    );
    assert_eq!(*seen.lock().unwrap(), ["end", "guardrail"]);
    assert_eq!(*codes.lock().unwrap(), ["output.invalid_final_output"]);
}

#[tokio::test]
async fn tool_stop_promotes_the_matching_value_and_bypasses_model_schema_validation() {
    for streamed in [false, true] {
        let first = Arc::new(ScriptedTool::new("first"));
        let second = Arc::new(ScriptedTool::new("second"));
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![
            message("narration", "not JSON"),
            tool_call("one", "c1", "first"),
            tool_call("two", "c2", "second"),
        ])]);
        let (result, _) = drive(
            typed_request(
                vec![first.clone(), second.clone()],
                &model,
                ToolUseBehavior::stop_at_tools(["second"]),
            ),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(result.final_output_as::<String>().unwrap(), "done");
        assert_eq!(
            result.outcome().finish_reason(),
            Some(FinishReason::ToolStop)
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(result.final_message().is_none());
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    }
}

struct SelectCustom;
#[async_trait]
impl ToolUseBehaviorHandler for SelectCustom {
    async fn final_output(
        &self,
        context: &RunContext,
        results: &[ToolUseResult],
    ) -> Result<ra_core::agent::ToolsToFinalOutputResult> {
        assert_eq!(results.len(), 1);
        assert_eq!(context.agent().name(), "Coder");
        Ok(ra_core::agent::ToolsToFinalOutputResult::final_output(
            OutputValue::typed(FinalAnswer {
                summary: "custom".to_owned(),
            })?,
        ))
    }
}

#[tokio::test]
async fn custom_tool_policy_can_supply_a_typed_final_output() {
    let tool = Arc::new(ScriptedTool::new("read"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "one", "c1", "read",
    )])]);
    let result = Runner::run(typed_request(
        vec![tool],
        &model,
        ToolUseBehavior::custom(Arc::new(SelectCustom)),
    ))
    .await
    .unwrap();
    assert_eq!(
        result.final_output_as::<FinalAnswer>().unwrap().summary,
        "custom"
    );
}

#[tokio::test]
async fn approval_resume_and_session_continuation_keep_the_output_contract() {
    for streamed in [false, true] {
        for policy in [
            ToolUseBehavior::RunLlmAgain,
            ToolUseBehavior::StopOnFirstTool,
        ] {
            let tool = Arc::new(
                ScriptedTool::new("write")
                    .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
            );
            let model = ScriptedModel::new(vec![
                ModelResponse::new(vec![tool_call("one", "c1", "write")]),
                ModelResponse::new(vec![message("answer", r#"{"summary":"after approval"}"#)]),
            ]);
            let session = Arc::new(InMemorySession::new("resume-typed"));
            let (paused, _) = drive(
                typed_request(vec![tool.clone()], &model, policy.clone())
                    .with_session(session.clone()),
                streamed,
            )
            .await
            .unwrap();
            assert!(paused.final_output().is_none());
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("approval expected");
            };
            let mut checkpoint: RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            checkpoint.approve(&items[0], false).unwrap();
            let (resumed, _) = drive(
                RunRequest::new(
                    AgentBinding::direct(paused.last_agent().clone()),
                    Arc::new(FixedResolver {
                        model: model.clone(),
                        selectors: Arc::new(Mutex::new(Vec::new())),
                    }),
                    RunId::new("typed-run"),
                    CancelScope::root(),
                    Vec::new(),
                )
                .with_session(session)
                .with_state(checkpoint),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
            if matches!(policy, ToolUseBehavior::StopOnFirstTool) {
                assert_eq!(resumed.final_output_as::<String>().unwrap(), "done");
                assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(
                    resumed.final_output_as::<FinalAnswer>().unwrap().summary,
                    "after approval"
                );
                assert_eq!(model.calls.load(Ordering::SeqCst), 2);
            }
        }
    }
}

#[tokio::test]
async fn invalid_final_output_recovery_never_replays_completed_tools() {
    let tool = Arc::new(ScriptedTool::new("read"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("one", "c1", "read")]),
        ModelResponse::new(vec![message("invalid", "bad")]),
    ]);
    let codes = Arc::new(Mutex::new(Vec::new()));
    let result = Runner::run(
        typed_request(vec![tool.clone()], &model, ToolUseBehavior::RunLlmAgain).with_config(
            RunConfig::new().with_error_handler(recovery(
                Some(r#"{"summary":"fallback"}"#),
                true,
                &codes,
            )),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        result.final_output_as::<FinalAnswer>().unwrap().summary,
        "fallback"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}

struct WrappedRecovery;
#[async_trait]
impl RunErrorHandler for WrappedRecovery {
    async fn handle(&self, _: RunErrorHandlerInput<'_>) -> Result<Option<RunErrorHandlerResult>> {
        Ok(Some(RunErrorHandlerResult::from_output(
            &OutputSchema::for_type::<Vec<i64>>()?,
            &vec![1, 2],
        )?))
    }
}

#[tokio::test]
async fn typed_handler_wraps_container_values_and_records_history_by_default() {
    for streamed in [false, true] {
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("invalid", "bad")])]);
        let (result, _) = drive(
            declared_request(
                &model,
                &CancelScope::root(),
                OutputSchema::for_type::<Vec<i64>>().unwrap(),
            )
            .with_config(RunConfig::new().with_error_handler(Arc::new(WrappedRecovery))),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(result.final_output_as::<Vec<i64>>().unwrap(), &[1, 2]);
        assert_eq!(result.final_text(), r#"{"response":[1,2]}"#);
        assert_eq!(result.new_items().len(), 2);
        assert_eq!(
            phases(&result),
            [OutputPhase::Commentary, OutputPhase::Final]
        );
    }
}

#[tokio::test]
async fn refusal_precedes_json_validation_and_uses_its_own_recovery_error() {
    for streamed in [false, true] {
        for recovering in [false, true] {
            let refusal = RunItem::new(
                ItemId::new("refusal"),
                RunItemKind::Message(Message::new(
                    MessageRole::Assistant,
                    vec![ra_core::item::ContentBlock::refusal("declined")],
                )),
            );
            let model = ScriptedModel::new(vec![ModelResponse::new(vec![refusal])]);
            let mut request = typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain);
            let codes = Arc::new(Mutex::new(Vec::new()));
            if recovering {
                request = request.with_config(RunConfig::new().with_error_handler(recovery(
                    Some(r#"{"summary":"fallback"}"#),
                    true,
                    &codes,
                )));
            }
            let result = drive(request, streamed).await;
            if recovering {
                assert_eq!(
                    result
                        .unwrap()
                        .0
                        .final_output_as::<FinalAnswer>()
                        .unwrap()
                        .summary,
                    "fallback"
                );
                assert_eq!(*codes.lock().unwrap(), ["provider.refusal"]);
            } else {
                assert_eq!(result.unwrap_err().code(), "provider.refusal");
            }
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn pending_invalid_output_handler_is_cancelled_by_the_caller() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("bad", "invalid")])]);
    let cancel = CancelScope::root();
    let (send, entered) = oneshot::channel();
    let request = declared_request(
        &model,
        &cancel,
        OutputSchema::for_type::<FinalAnswer>().unwrap(),
    )
    .with_config(
        RunConfig::new().with_error_handler(Arc::new(PendingCloseoutHandler {
            started: Mutex::new(Some(send)),
        })),
    );
    let task = tokio::spawn(Runner::run(request));
    timeout(Duration::from_secs(2), entered)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel(ra_core::cancel::CancelReason::UserInterrupt);
    assert!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
}

struct ValueTool {
    inner: ScriptedTool,
    text: &'static str,
}
#[async_trait]
impl Tool for ValueTool {
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
        self.inner.call(context).await?;
        Ok(ToolOutput::text(self.text))
    }
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.inner.needs_approval(context).await
    }
}

#[tokio::test]
async fn approval_resume_selects_completed_siblings_in_original_model_order_without_replay() {
    for streamed in [false, true] {
        for (policy, expected) in [
            (ToolUseBehavior::StopOnFirstTool, "first value"),
            (ToolUseBehavior::stop_at_tools(["first"]), "first value"),
            (ToolUseBehavior::stop_at_tools(["second"]), "second value"),
        ] {
            let first = Arc::new(ValueTool {
                inner: ScriptedTool::new("first"),
                text: "first value",
            });
            let second = Arc::new(ValueTool {
                inner: ScriptedTool::new("second")
                    .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
                text: "second value",
            });
            let model = ScriptedModel::new(vec![ModelResponse::new(vec![
                tool_call("first-call", "c1", "first"),
                tool_call("second-call", "c2", "second"),
            ])]);
            let (paused, _) = drive(
                typed_request(vec![first.clone(), second.clone()], &model, policy),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("approval expected");
            };
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            state.approve(&items[0], false).unwrap();
            let (result, _) = drive(
                RunRequest::new(
                    AgentBinding::direct(paused.last_agent().clone()),
                    Arc::new(FixedResolver {
                        model: model.clone(),
                        selectors: Arc::new(Mutex::new(Vec::new())),
                    }),
                    RunId::new("typed-run"),
                    CancelScope::root(),
                    Vec::new(),
                )
                .with_state(state),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(result.final_output_as::<String>().unwrap(), expected);
            assert_eq!(first.inner.calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.inner.calls.load(Ordering::SeqCst), 1);
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn new_session_runs_decode_their_own_output_and_keep_prior_wire_history() {
    let session = Arc::new(InMemorySession::new("typed-history"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![message("first", r#"{"summary":"first answer"}"#)]),
        ModelResponse::new(vec![message("second", r#"{"summary":"second answer"}"#)]),
    ]);
    for expected in ["first answer", "second answer"] {
        let result = Runner::run(
            typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain)
                .with_session(session.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            result.final_output_as::<FinalAnswer>().unwrap().summary,
            expected
        );
    }
    assert!(model.input_items.lock().unwrap()[1].iter().any(|item| {
        matches!(item, ModelInputItem::Message(message) if message.text_content().contains("first answer"))
    }));
}

#[tokio::test]
async fn budget_closeout_must_satisfy_the_effective_output_contract() {
    for text in ["not JSON", r#"{"summary":"closed"}"#] {
        let tool = Arc::new(ScriptedTool::new("read"));
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
            "one", "c1", "read",
        )])]);
        let codes = Arc::new(Mutex::new(Vec::new()));
        let result = Runner::run(
            typed_request(vec![tool], &model, ToolUseBehavior::RunLlmAgain).with_config(
                RunConfig::new()
                    .with_max_turns(1)
                    .with_error_handler(recovery(Some(text), false, &codes)),
            ),
        )
        .await;
        assert_eq!(*codes.lock().unwrap(), ["budget.max_turns"]);
        if text == "not JSON" {
            assert_eq!(result.unwrap_err().code(), "caller");
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .final_output_as::<FinalAnswer>()
                    .unwrap()
                    .summary,
                "closed"
            );
        }
    }
}

/// Hangs on final-output recovery and declines the budget closeout that follows it.
struct HangingRecovery {
    codes: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl RunErrorHandler for HangingRecovery {
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        let code = input.error().code().to_owned();
        self.codes.lock().unwrap().push(code.clone());
        if code == "output.invalid_final_output" {
            futures::future::pending::<()>().await;
        }
        Ok(None)
    }
}

#[tokio::test]
async fn wall_clock_stops_invalid_output_recovery_and_ends_as_a_budget_stop() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("bad", "invalid")])]);
    let codes = Arc::new(Mutex::new(Vec::new()));
    let request = typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
        RunConfig::new()
            .with_deadline(Deadline::after(Duration::from_millis(50)))
            .with_error_handler(Arc::new(HangingRecovery {
                codes: codes.clone(),
            })),
    );
    let result = timeout(Duration::from_secs(2), Runner::run(request))
        .await
        .expect("the run deadline must stop a pending recovery handler")
        .unwrap();
    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    assert!(result.final_output().is_none());
    assert_eq!(
        *codes.lock().unwrap(),
        ["output.invalid_final_output", "budget.wall_clock"]
    );
}

struct CandidateAndCloseout {
    history: bool,
    closeout: bool,
    codes: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl RunErrorHandler for CandidateAndCloseout {
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        let code = input.error().code();
        self.codes.lock().unwrap().push(code.to_owned());
        let text = match code {
            "output.invalid_final_output" => r#"{"summary":"candidate"}"#,
            "budget.wall_clock" if self.closeout => r#"{"summary":"closeout"}"#,
            _ => return Ok(None),
        };
        Ok(Some(
            RunErrorHandlerResult::new(Message::assistant(text, OutputPhase::Final))
                .with_write_to_history(self.history),
        ))
    }
}

struct CandidateStopHook {
    wait_for_deadline: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UserHook for CandidateStopHook {
    fn name(&self) -> &str {
        "candidate-stop"
    }

    async fn call(&self, _: &UserHookContext<'_>, event: &HookEvent<'_>) -> Result<HookDecision> {
        let HookEvent::Stop(delivery) = event else {
            panic!("only the stop hook is registered");
        };
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            assert_eq!(
                delivery.message().unwrap().text_content(),
                r#"{"summary":"candidate"}"#
            );
            if self.wait_for_deadline {
                futures::future::pending::<()>().await;
            }
            Ok(HookDecision::Block {
                prompt: "continue working".to_owned(),
            })
        } else {
            Ok(HookDecision::Continue)
        }
    }
}

#[tokio::test]
async fn deadline_after_recovery_discards_the_candidate_and_only_delivers_budget_closeout() {
    for streamed in [false, true] {
        for history in [false, true] {
            for closeout in [false, true] {
                let model =
                    ScriptedModel::new(vec![ModelResponse::new(vec![message("bad", "invalid")])]);
                let session = Arc::new(InMemorySession::new("deadline-after-recovery"));
                let codes = Arc::new(Mutex::new(Vec::new()));
                let hook_calls = Arc::new(AtomicUsize::new(0));
                let seen = Arc::new(Mutex::new(Vec::new()));
                let observer = Arc::new(Observers {
                    seen: seen.clone(),
                    expected: if closeout { "closeout" } else { "candidate" },
                    trip: false,
                });
                let request = typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain)
                    .with_session(session.clone())
                    .with_config(
                        RunConfig::new()
                            .with_deadline(Deadline::after(Duration::from_millis(100)))
                            .with_error_handler(Arc::new(CandidateAndCloseout {
                                history,
                                closeout,
                                codes: codes.clone(),
                            }))
                            .with_user_hook(UserHookRegistration::new(
                                HookEventName::Stop,
                                Arc::new(CandidateStopHook {
                                    wait_for_deadline: true,
                                    calls: hook_calls.clone(),
                                }),
                            ))
                            .with_lifecycle_hook(observer.clone())
                            .with_output_guardrail(observer),
                    );
                let (result, events) = timeout(Duration::from_secs(2), drive(request, streamed))
                    .await
                    .expect("the deadline must stop the pending stop hook")
                    .unwrap();
                assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
                assert_eq!(model.calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    result.outcome().finish_reason(),
                    Some(FinishReason::BudgetExhausted)
                );
                assert_eq!(
                    *codes.lock().unwrap(),
                    ["output.invalid_final_output", "budget.wall_clock"]
                );
                if closeout {
                    assert_eq!(
                        result.final_output_as::<FinalAnswer>().unwrap().summary,
                        "closeout"
                    );
                    assert_eq!(result.final_text(), r#"{"summary":"closeout"}"#);
                    assert_eq!(*seen.lock().unwrap(), ["end", "guardrail"]);
                } else {
                    assert!(result.final_message().is_none());
                    assert!(result.final_output().is_none());
                    assert!(seen.lock().unwrap().is_empty());
                }
                let delivered: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        RunStreamEvent::FinalMessage(message) => Some(message.text_content()),
                        _ => None,
                    })
                    .collect();
                let expected = if streamed && closeout && !history {
                    vec![r#"{"summary":"closeout"}"#.to_owned()]
                } else {
                    Vec::new()
                };
                assert_eq!(delivered, expected);
                let stored = session.get_items(None).await.unwrap();
                for items in [result.new_items(), stored.as_slice()] {
                    for (text, recorded) in
                        [("candidate", history), ("closeout", history && closeout)]
                    {
                        assert_eq!(items.iter().any(|item| matches!(item.kind(),
                            RunItemKind::Message(message) if message.text_content().contains(text)
                        )), recorded);
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn a_stop_hook_continuation_does_not_stream_the_discarded_recovery() {
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![message("bad", "invalid")]),
        ModelResponse::new(vec![message("done", r#"{"summary":"done"}"#)]),
    ]);
    let codes = Arc::new(Mutex::new(Vec::new()));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let (result, events) = drive(
        typed_request(Vec::new(), &model, ToolUseBehavior::RunLlmAgain).with_config(
            RunConfig::new()
                .with_error_handler(Arc::new(CandidateAndCloseout {
                    history: false,
                    closeout: false,
                    codes: codes.clone(),
                }))
                .with_user_hook(UserHookRegistration::new(
                    HookEventName::Stop,
                    Arc::new(CandidateStopHook {
                        wait_for_deadline: false,
                        calls: hook_calls.clone(),
                    }),
                )),
        ),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        result.final_output_as::<FinalAnswer>().unwrap().summary,
        "done"
    );
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(*codes.lock().unwrap(), ["output.invalid_final_output"]);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RunStreamEvent::FinalMessage(_)))
    );
}

/// Codex only forwards a turn's schema, so a run-level declaration never fails, retries, or
/// continues because of the final answer's shape.
#[tokio::test]
async fn a_run_level_schema_is_forwarded_but_never_validated() {
    for streamed in [false, true] {
        for (answer, expected) in [
            ("not JSON", OutputValue::text("not JSON")),
            (
                r#"{"unexpected":1}"#,
                OutputValue::json(json!({"unexpected":1})),
            ),
            ("", OutputValue::text("")),
        ] {
            let model =
                ScriptedModel::new(vec![ModelResponse::new(vec![message("answer", answer)])]);
            let codes = Arc::new(Mutex::new(Vec::new()));
            let (result, _) = drive(
                request(Vec::new(), &model, &CancelScope::root())
                    .with_output_schema(OutputSchema::for_type::<FinalAnswer>().unwrap())
                    .with_config(RunConfig::new().with_error_handler(recovery(
                        Some("unused"),
                        true,
                        &codes,
                    ))),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert!(model.request_surfaces.lock().unwrap()[0].2);
            assert!(codes.lock().unwrap().is_empty());
            let output = result.final_output().unwrap();
            assert_eq!(output.value(), expected.value());
            assert_eq!(output.is_plain_text(), expected.is_plain_text());
            assert!(result.final_output_as::<FinalAnswer>().is_err());
        }
    }
}
