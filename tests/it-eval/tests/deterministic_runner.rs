//! A fixed tool workflow exercised repeatedly through the real runner and provider registry.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, ProviderErrorKind, Result},
    item::{
        CallId, HandoffCall, ItemId, Message, ModelInputItem, ModelResponse, RunItem, RunItemKind,
    },
    model::{
        ApiProtocol, Model, ModelProvider, ModelRetrySettings, ModelSettings, ProviderKey,
        ProviderSuggestedRetryPolicy, ReplaySafety, RetryAdvice, RetryBackoffSettings,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_eval::fixture::{ModelCall, ModelStep, ScriptedModel, ScriptedProvider, assistant_message};
use ra_model::provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::{RunConfig, RunOutcome, RunRequest, RunStreamEvent, Runner},
};
use serde_json::{Value, json};

struct EchoTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for EchoTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(context.arguments()["value"].to_string()))
    }
}

async fn run_fixture(streamed: bool) -> (Value, Vec<ModelCall>) {
    let responses: Vec<ModelResponse> =
        serde_json::from_str(include_str!("../fixtures/tool_then_answer.json")).unwrap();
    let model = Arc::new(ScriptedModel::new(responses.clone()));
    let provider: Arc<dyn ModelProvider> =
        Arc::new(ScriptedProvider::new(Arc::clone(&model) as Arc<dyn Model>));
    let key = ProviderKey::new("fixture");
    let registry = Arc::new(
        ProviderRegistry::builder(key.clone())
            .register(ProviderRegistration::new(
                key,
                ApiProtocol::OpenAiResponses,
                move |_: ProviderQuirks| Ok(Arc::clone(&provider)),
            ))
            .build()
            .unwrap(),
    );
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tool = EchoTool {
        origin: ToolOrigin::new("echo").unwrap(),
        schema: ToolSchema::new(
            "echo",
            json!({
                "type": "object", "properties": {"value": {"type": "integer"}},
                "required": ["value"], "additionalProperties": false,
            }),
        )
        .unwrap(),
        calls: Arc::clone(&tool_calls),
    };
    let agent = AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("fixture-agent"))
            .name("Fixture agent")
            .instructions("Echo the supplied value, then report it.")
            .model("fixture/scripted")
            .tools(vec![Arc::new(tool) as Arc<dyn Tool>])
            .build()
            .unwrap(),
    );
    let request = RunRequest::new(
        agent,
        registry.clone(),
        RunId::new("deterministic-fixture"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("Echo 7"))],
    );
    let result = if streamed {
        let mut stream = Runner::run_streamed(request);
        while stream.next_event().await.is_some() {}
        stream.finish().await.unwrap()
    } else {
        Runner::run(request).await.unwrap()
    };
    model.assert_complete().unwrap();
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.turns(), 2);
    assert_eq!(result.final_text(), "Echo result: 7");
    assert_eq!(
        result
            .turn_records()
            .iter()
            .map(|record| record.next_step_code())
            .collect::<Vec<_>>(),
        ["run_again", "final_output"]
    );
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(result.model_responses(), responses);
    assert_eq!(result.usage().requests(), 2);
    assert_eq!(result.usage().input_tokens(), 30);
    assert_eq!(result.usage().output_tokens(), 10);
    let kinds: Vec<_> = result
        .new_items()
        .iter()
        .map(|item| item.kind().label())
        .collect();
    assert_eq!(kinds, ["tool_call", "tool_call_output", "message"]);
    let RunItemKind::ToolCall(call) = result.new_items()[0].kind() else {
        panic!("missing call")
    };
    let RunItemKind::ToolCallOutput(output) = result.new_items()[1].kind() else {
        panic!("missing output")
    };
    assert_eq!(call.call_id(), output.call_id());
    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(ModelCall::streamed));
    assert!(calls[1].request().input().len() > calls[0].request().input().len());
    let snapshot = json!({
        "items": result.new_items(),
        "responses": result.model_responses(),
        "usage": result.usage(),
        "turns": result.turns(),
        "final_text": result.final_text(),
        "next_steps": result.turn_records().iter().map(|record| record.next_step_code()).collect::<Vec<_>>(),
        "finish_reason": result.outcome().finish_reason(),
    });
    registry.close().await.unwrap();
    (snapshot, calls)
}

#[tokio::test]
async fn same_fixture_produces_identical_workflow_five_times_in_both_run_modes() {
    let baseline = run_fixture(false).await;
    for run in 0..5 {
        if run > 0 {
            assert_eq!(run_fixture(false).await, baseline);
        }
        assert_eq!(run_fixture(true).await, baseline);
    }
}

#[tokio::test]
async fn scripted_handoff_stream_transfers_control_and_preserves_paired_history() {
    let target = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .build()
        .unwrap();
    let handoff = target.as_handoff().unwrap();
    let call = HandoffCall::new(CallId::new("transfer"), target.id().clone(), json!({}))
        .with_tool_name(handoff.schema().name());
    let source = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .handoff(handoff)
        .build()
        .unwrap();
    let agents = AgentRegistry::builder()
        .register(source.clone())
        .register(target.clone())
        .build()
        .unwrap();
    let responses = vec![
        ModelResponse::new(vec![RunItem::new(
            ItemId::new("handoff"),
            RunItemKind::HandoffCall(call.clone()),
        )]),
        ModelResponse::new(vec![assistant_message("Review complete", Some("answer"))]),
    ];
    let model = Arc::new(ScriptedModel::new(responses.clone()));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(model.clone()));
    let registry = Arc::new(
        ProviderRegistry::builder(ProviderKey::new("fixture"))
            .register(ProviderRegistration::new(
                ProviderKey::new("fixture"),
                ApiProtocol::OpenAiResponses,
                move |_: ProviderQuirks| Ok(Arc::clone(&provider)),
            ))
            .build()
            .unwrap(),
    );
    let request = RunRequest::new(
        AgentBinding::direct(source.clone()),
        registry.clone(),
        RunId::new("handoff-fixture"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("Review this plan"))],
    )
    .with_config(RunConfig::new().with_agent_registry(agents));
    let mut stream = Runner::run_streamed(request);
    let mut speakers = Vec::new();
    let mut streamed_items = Vec::new();
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::TurnStarted { agent, .. } => speakers.push(agent),
            RunStreamEvent::Item(item) => streamed_items.push(item),
            _ => {}
        }
    }
    let result = stream.finish().await.unwrap();
    assert_eq!(speakers, [source.id().clone(), target.id().clone()]);
    assert_eq!(result.last_agent().id(), target.id());
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(result.final_text(), "Review complete");
    assert_eq!(result.turns(), 2);
    assert_eq!(result.turn_records()[0].next_step_code(), "handoff");
    assert_eq!(result.turn_records()[1].next_step_code(), "final_output");
    assert_eq!(result.model_responses(), responses);
    assert_eq!(streamed_items, result.new_items());
    assert_eq!(
        result
            .new_items()
            .iter()
            .map(|item| item.kind().label())
            .collect::<Vec<_>>(),
        ["handoff_call", "handoff_output", "message"]
    );
    let RunItemKind::HandoffOutput(output) = result.new_items()[1].kind() else {
        panic!("missing handoff output")
    };
    assert_eq!(output.call_id(), call.call_id());
    assert_eq!(output.source_agent(), source.id());
    assert_eq!(output.target_agent(), target.id());
    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(ModelCall::streamed));
    assert!(
        calls[1]
            .request()
            .input()
            .contains(&ModelInputItem::HandoffCall(call))
    );
    assert!(
        calls[1]
            .request()
            .input()
            .contains(&ModelInputItem::HandoffOutput(output.clone()))
    );
    model.assert_complete().unwrap();
    registry.close().await.unwrap();
}

#[tokio::test]
async fn scripted_provider_advice_drives_runtime_retry_and_captures_both_attempts() {
    let model = Arc::new(ScriptedModel::new([
        ModelStep::raise_error(
            Error::provider(ProviderErrorKind::Network, "scripted transient failure"),
            Some(
                RetryAdvice::new()
                    .with_suggested(true)
                    .with_replay_safety(ReplaySafety::Safe),
            ),
        ),
        ModelStep::new(vec![assistant_message("recovered", None)]),
    ]));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(model.clone()));
    let registry = Arc::new(
        ProviderRegistry::builder(ProviderKey::new("fixture"))
            .register(ProviderRegistration::new(
                ProviderKey::new("fixture"),
                ApiProtocol::OpenAiResponses,
                move |_: ProviderQuirks| Ok(Arc::clone(&provider)),
            ))
            .build()
            .unwrap(),
    );
    let retry = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_policy(Arc::new(ProviderSuggestedRetryPolicy))
        .with_backoff(
            RetryBackoffSettings::new()
                .with_initial_delay(Duration::ZERO)
                .with_jitter(false),
        );
    let agent = AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("retry-fixture"))
            .name("Retry fixture")
            .build()
            .unwrap(),
    );
    let result = Runner::run(
        RunRequest::new(
            agent,
            registry.clone(),
            RunId::new("retry-fixture"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_config(RunConfig::new().with_model_settings(ModelSettings::new().with_retry(retry))),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "recovered");
    assert_eq!(result.turns(), 1);
    assert_eq!(result.usage().requests(), 2);
    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    model.assert_complete().unwrap();
    registry.close().await.unwrap();
}
