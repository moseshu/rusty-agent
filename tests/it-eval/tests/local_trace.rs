// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Real-run capture, persisted trace replay, topology and content-selection contracts.

use async_trait::async_trait;
use futures::poll;
use ra_core::{
    agent::{
        AgentId, AgentSpec,
        control::{AgentPath, AgentStatus, SpawnAgentRequest},
    },
    cancel::CancelScope,
    context::RunContext,
    error::{Error, ProviderErrorKind, Result},
    guardrail::{GuardrailFunctionOutput, InputGuardrail},
    item::{CallId, ContentBlock, Message, ModelInputItem, ModelResponse},
    model::{
        ApiProtocol, Model, ModelProvider, ModelRetrySettings, ModelSettings, ProviderKey,
        ProviderSuggestedRetryPolicy, ReplaySafety, RetryAdvice, RetryBackoffSettings,
    },
    session::{
        SessionId,
        rollout::{
            PersistContext, RolloutItem, RolloutRecorder, RolloutRunStarted, RolloutTraceEvent,
        },
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_eval::{
    fixture::{ModelStep, ScriptedModel, ScriptedProvider, assistant_message, function_call},
    replay::replay,
    trace::{LocalTrace, read_jsonl, read_rollout, write_jsonl},
};
use ra_model::provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry, control::AgentControl, tool::AgentAsTool},
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
    thread::{AgentThread, ThreadEvent, TurnInputRequest},
};
use ra_session::rollout::{RolloutFileRecorder, RolloutReader, reconstruct_history};
use serde_json::json;
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

struct Echo(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Echo {
    fn origin(&self) -> &ToolOrigin {
        static ORIGIN: std::sync::OnceLock<ToolOrigin> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(|| ToolOrigin::new("echo").unwrap())
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema::new("echo", json!({"type":"object", "properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false})).unwrap())
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(context.arguments()["value"].to_string()))
    }
}

struct GatedEcho(Echo);
#[async_trait]
impl Tool for GatedEcho {
    fn origin(&self) -> &ToolOrigin {
        self.0.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.0.schema()
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.0.call(context).await
    }
}

/// Collects the `group_id` each agent span declares, as subscribers other than local capture see it.
#[derive(Clone, Default)]
struct AgentSpanGroups(Arc<std::sync::Mutex<Vec<Option<String>>>>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AgentSpanGroups {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Group(Option<String>);
        impl tracing::field::Visit for Group {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "group_id" {
                    self.0 = Some(value.to_owned());
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        if attrs.metadata().name() == "agent" {
            let mut group = Group(None);
            attrs.record(&mut group);
            self.0.lock().unwrap().push(group.0);
        }
    }
}

struct Guard(bool);
#[async_trait]
impl InputGuardrail for Guard {
    fn name(&self) -> &str {
        "fixture guard"
    }
    fn run_in_parallel(&self) -> bool {
        false
    }
    async fn check(&self, _: &RunContext, _: &[ModelInputItem]) -> Result<GuardrailFunctionOutput> {
        Ok(if self.0 {
            GuardrailFunctionOutput::tripwire()
        } else {
            GuardrailFunctionOutput::pass()
        }
        .with_output_info(json!({"evidence":"guard private payload"})))
    }
}

fn registry(model: Arc<ScriptedModel>) -> Arc<ProviderRegistry> {
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(model as Arc<dyn Model>));
    Arc::new(
        ProviderRegistry::builder(ProviderKey::new("fixture"))
            .register(ProviderRegistration::new(
                ProviderKey::new("fixture"),
                ApiProtocol::OpenAiResponses,
                move |_: ProviderQuirks| Ok(provider.clone()),
            ))
            .build()
            .unwrap(),
    )
}

fn trace_agent(tools: Vec<Arc<dyn Tool>>, tripwire: bool) -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("trace-agent"))
            .name("Trace agent")
            .instructions("private instructions")
            .model("fixture/scripted")
            .tools(tools)
            .input_guardrail(Arc::new(Guard(tripwire)))
            .build()
            .unwrap(),
    )
}

fn request(
    model: Arc<ScriptedModel>,
    trace: &LocalTrace,
    tools: Vec<Arc<dyn Tool>>,
    tripwire: bool,
) -> RunRequest {
    RunRequest::new(
        trace_agent(tools, tripwire),
        registry(model),
        RunId::new("run-local"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("private user input"))],
    )
    .with_rollout_recorder(Arc::new(trace.clone()))
    .with_config(RunConfig::new().with_model_settings(
        ModelSettings::new().with_extra_header("authorization", "private transport credential"),
    ))
}

/// Continues a checkpoint with the projection of its own history.
fn resume_request(
    model: Arc<ScriptedModel>,
    trace: &LocalTrace,
    tools: Vec<Arc<dyn Tool>>,
    state: RunState,
) -> RunRequest {
    RunRequest::new(
        trace_agent(tools, false),
        registry(model),
        RunId::new("run-local"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_rollout_recorder(Arc::new(trace.clone()))
    .with_state(state)
}

fn group_config(group: Option<&str>) -> RunConfig {
    group.map_or_else(RunConfig::new, |group| {
        RunConfig::new().with_group_id(group)
    })
}

async fn run_to_end(request: RunRequest, streamed: bool) -> RunResult {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        while stream.next_event().await.is_some() {}
        stream.finish().await.unwrap()
    } else {
        Runner::run(request).await.unwrap()
    }
}

#[tokio::test]
async fn tool_workflow_roundtrips_with_payloads_in_both_runner_modes() {
    for streamed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let recorder = Arc::new(RolloutFileRecorder::create(
            &path,
            SessionId::new("trace-session"),
        ));
        let trace = LocalTrace::new()
            .with_payloads()
            .with_recorder(recorder.clone());
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
        let responses: Vec<ModelResponse> =
            serde_json::from_str(include_str!("../fixtures/tool_then_answer.json")).unwrap();
        let model = Arc::new(ScriptedModel::new(responses.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let request = request(
            model.clone(),
            &trace,
            vec![Arc::new(Echo(calls.clone()))],
            false,
        )
        .with_config(
            RunConfig::new()
                .with_group_id("  workflow-group  ")
                .with_model_settings(
                    ModelSettings::new()
                        .with_extra_header("authorization", "private transport credential"),
                ),
        );
        let result = if streamed {
            let mut stream = Runner::run_streamed(request);
            while stream.next_event().await.is_some() {}
            stream.finish().await.unwrap()
        } else {
            Runner::run(request).await.unwrap()
        };
        trace.shutdown().await.unwrap();
        let records = trace.snapshot();
        assert!(
            records
                .iter()
                .all(|record| record.group_id() == Some("  workflow-group  "))
        );
        let mut bytes = Vec::new();
        write_jsonl(&records, &mut bytes).unwrap();
        let encoded = String::from_utf8(bytes.clone()).unwrap();
        assert!(encoded.contains("private user input"));
        assert!(encoded.contains("private instructions"));
        assert!(!encoded.contains("private transport credential"));
        let imported = read_jsonl(bytes.as_slice()).unwrap();
        assert_eq!(imported, records);
        assert_eq!(read_rollout(&path).await.unwrap(), records);
        let replayed = replay(&imported).unwrap();
        assert_eq!(replay(&imported).unwrap(), replayed);
        assert_eq!(replayed.events_of_kind("model_request").count(), 2);
        let outputs = replayed
            .events_of_kind("model_response")
            .map(|event| {
                serde_json::from_value::<ModelResponse>(event.event().payload().cloned().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs, responses);
        let steps = replayed
            .events_of_kind("next_step")
            .map(|record| record.event().fields()["next_step"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(steps, ["run_again", "final_output"]);
        assert_eq!(
            steps,
            result
                .turn_records()
                .iter()
                .map(|record| record.next_step_code())
                .collect::<Vec<_>>()
        );
        let spans = replayed.spans().values().collect::<Vec<_>>();
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.fields()["span.kind"] == "generation")
                .count(),
            2
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.fields()["span.kind"] == "function")
                .count(),
            1
        );
        let guard = spans
            .iter()
            .find(|span| span.fields()["span.kind"] == "guardrail")
            .unwrap();
        assert_eq!(guard.fields()["guardrail.triggered"], false);
        assert!(spans.iter().all(|span| span.ended_seq().is_some()));
        let generation = spans
            .iter()
            .find(|span| span.fields()["span.kind"] == "generation")
            .unwrap();
        let turn = &replayed.spans()[generation.parent_id().unwrap()];
        assert_eq!(turn.fields()["span.kind"], "turn");
        let root = &replayed.spans()[turn.parent_id().unwrap()];
        assert_eq!(root.fields()["span.kind"], "agent");
        assert_eq!(root.fields()["run.id"], "run-local");
        assert_eq!(root.trace_id(), generation.trace_id());
        let tool_ids = replayed
            .events_of_kind("item")
            .filter_map(|record| record.event().fields().get("tool.call_id"))
            .collect::<Vec<_>>();
        assert!(tool_ids.len() >= 2 && tool_ids.iter().all(|id| *id == tool_ids[0]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        model.assert_complete().unwrap();
        let persisted = RolloutReader::open(&path).read_all().await.unwrap();
        let history = reconstruct_history(&persisted).unwrap();
        assert_eq!(history.history().len(), result.new_items().len() + 1);
    }
}

#[tokio::test]
async fn trace_group_preserves_config_values_without_session_or_generated_fallbacks() {
    for streamed in [false, true] {
        for payloads in [false, true] {
            let trace = if payloads {
                LocalTrace::new().with_payloads()
            } else {
                LocalTrace::new()
            };
            let _subscriber = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(trace.clone()),
            );
            let mut trace_ids = std::collections::BTreeSet::new();
            for with_session in [false, true] {
                for group in [Some("  shared-group  "), Some(""), Some(" \t "), None] {
                    let start = trace.snapshot().len();
                    let model = Arc::new(ScriptedModel::new([ModelStep::new(vec![
                        assistant_message("answer", None),
                    ])]));
                    let config = group.map_or_else(RunConfig::new, |group| {
                        RunConfig::new().with_group_id(group)
                    });
                    let mut request =
                        request(model.clone(), &trace, Vec::new(), false).with_config(config);
                    if with_session {
                        request = request.with_session(Arc::new(ra_session::InMemorySession::new(
                            "unrelated-session",
                        )));
                    }
                    if streamed {
                        let mut stream = Runner::run_streamed(request);
                        while stream.next_event().await.is_some() {}
                        stream.finish().await.unwrap();
                    } else {
                        Runner::run(request).await.unwrap();
                    }
                    let records = trace.snapshot();
                    let run = &records[start..];
                    assert!(!run.is_empty());
                    assert!(run.iter().all(|record| record.group_id() == group));
                    let id = run[0].trace_id();
                    assert!(run.iter().all(|record| record.trace_id() == id));
                    assert!(trace_ids.insert(id.to_owned()));
                    assert_ne!(Some(id), group);
                    model.assert_complete().unwrap();
                }
            }
            trace.shutdown().await.unwrap();
            let records = trace.snapshot();
            let mut bytes = Vec::new();
            write_jsonl(&records, &mut bytes).unwrap();
            let restored = read_jsonl(bytes.as_slice()).unwrap();
            assert_eq!(restored, records);
            let replayed = replay(&restored).unwrap();
            for span in replayed.spans().values() {
                let start = restored
                    .iter()
                    .find(|record| record.seq() == span.started_seq())
                    .unwrap();
                assert_eq!(span.group_id(), start.group_id());
                assert!(!span.fields().contains_key("group_id"));
            }
        }
    }
}

#[tokio::test]
async fn concurrent_runs_keep_distinct_trace_ids_with_a_shared_group() {
    let trace = LocalTrace::new();
    let model = Arc::new(ScriptedModel::new([
        ModelStep::new(vec![assistant_message("first", None)]),
        ModelStep::new(vec![assistant_message("second", None)]),
    ]));
    let first = request(model.clone(), &trace, Vec::new(), false)
        .with_config(RunConfig::new().with_group_id("shared"));
    let second = request(model.clone(), &trace, Vec::new(), false)
        .with_config(RunConfig::new().with_group_id("shared"));
    async {
        let first = tokio::spawn(Runner::run(first).with_current_subscriber());
        let mut second = Runner::run_streamed(second);
        while second.next_event().await.is_some() {}
        second.finish().await.unwrap();
        first.await.unwrap().unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(trace.clone()))
    .await;
    trace.shutdown().await.unwrap();
    let records = trace.snapshot();
    let replayed = replay(&records).unwrap();
    let roots = replayed
        .spans()
        .values()
        .filter(|span| span.parent_id().is_none())
        .collect::<Vec<_>>();
    assert_eq!(roots.len(), 2);
    assert_ne!(roots[0].trace_id(), roots[1].trace_id());
    assert!(
        records
            .iter()
            .all(|record| record.group_id() == Some("shared"))
    );
    model.assert_complete().unwrap();
}

/// Ported from the reference's `resolve_trace_settings`: a resumed run whose configuration sets no
/// group uses the one its checkpoint captured, an explicit group overrides it, and the resumed
/// checkpoint records the group its segment used.
#[tokio::test]
async fn resumed_runs_fall_back_to_the_checkpoint_trace_group() {
    for (first, resumed, expected) in [
        (Some("group-1"), None, Some("group-1")),
        (Some("group-1"), Some("override"), Some("override")),
        (Some(""), None, Some("")),
        (None, Some("late"), Some("late")),
        (None, None, None),
    ] {
        for streamed in [false, true] {
            let trace = LocalTrace::new();
            let _subscriber = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(trace.clone()),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(GatedEcho(Echo(calls.clone())))];
            let model = Arc::new(ScriptedModel::new([
                ModelStep::new(vec![function_call(
                    "echo",
                    json!({"value": 1}),
                    "gated-call",
                    None,
                    None,
                )]),
                ModelStep::new(vec![assistant_message("done", None)]),
            ]));
            let interrupted = run_to_end(
                request(model.clone(), &trace, tools.clone(), false)
                    .with_config(group_config(first)),
                streamed,
            )
            .await;
            let RunOutcome::Interrupted { items } = interrupted.outcome() else {
                panic!("the gated call must pause the run");
            };
            assert_eq!(interrupted.state().trace_group_id(), first);
            // Through the wire format, as a host restoring a paused run reads it.
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(interrupted.state()).unwrap()).unwrap();
            state.approve(&items[0], false).unwrap();
            let start = trace.snapshot().len();
            let result = run_to_end(
                resume_request(model.clone(), &trace, tools, state)
                    .with_config(group_config(resumed)),
                streamed,
            )
            .await;
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.state().trace_group_id(), expected);
            trace.shutdown().await.unwrap();
            let records = trace.snapshot();
            let (paused, continued) = records.split_at(start);
            assert!(!paused.is_empty() && !continued.is_empty());
            assert!(paused.iter().all(|record| record.group_id() == first));
            assert!(continued.iter().all(|record| record.group_id() == expected));
            assert_ne!(paused[0].trace_id(), continued[0].trace_id());
            replay(&records).unwrap();
            model.assert_complete().unwrap();
        }
    }
}

#[test]
fn trace_group_is_fixed_when_its_root_span_is_created() {
    let trace = LocalTrace::new();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(trace.clone()), || {
        let grouped = tracing::info_span!(
            "custom",
            span.kind = "custom",
            group_id = "original",
            outcome = tracing::field::Empty,
        );
        grouped.in_scope(|| {
            drop(tracing::info_span!(
                "custom",
                span.kind = "custom",
                group_id = "child"
            ));
        });
        grouped.record("group_id", "changed");
        grouped.record("outcome", "ok");
        drop(grouped);
        let late = tracing::info_span!(
            "custom",
            span.kind = "custom",
            group_id = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        late.record("group_id", "late");
        late.record("outcome", "ok");
        drop(late);
    });
    let records = trace.snapshot();
    assert_eq!(
        records
            .iter()
            .map(|record| (record.event().kind(), record.group_id()))
            .collect::<Vec<_>>(),
        [
            ("span_started", Some("original")),
            ("span_started", Some("original")),
            ("span_ended", Some("original")),
            ("span_updated", Some("original")),
            ("span_ended", Some("original")),
            ("span_started", None),
            ("span_updated", None),
            ("span_ended", None),
        ]
    );
    assert!(
        records
            .iter()
            .all(|record| record.event().fields().get("group_id").is_none())
    );
    replay(&records).unwrap();
}

#[test]
fn trace_group_replay_accepts_legacy_records_and_rejects_inconsistent_groups() {
    use ra_eval::trace::RolloutTraceRecord;
    let records = [
        RolloutTraceRecord::new(
            "capture".into(),
            1,
            0,
            "trace".into(),
            Some("root".into()),
            None,
            RolloutTraceEvent::new("span_started", json!({"span.kind":"agent"}), None),
        ),
        RolloutTraceRecord::new(
            "capture".into(),
            2,
            0,
            "trace".into(),
            Some("root".into()),
            None,
            RolloutTraceEvent::new("span_ended", json!({}), None),
        ),
    ];
    let mut bytes = Vec::new();
    write_jsonl(&records, &mut bytes).unwrap();
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("group_id")
    );
    let legacy = read_jsonl(bytes.as_slice()).unwrap();
    assert_eq!(replay(&legacy).unwrap().spans()["root"].group_id(), None);
    for group in ["", " \t ", "group"] {
        let grouped = records.clone().map(|record| record.with_group_id(group));
        assert_eq!(
            replay(&grouped).unwrap().spans()["root"].group_id(),
            Some(group)
        );
        for corrupt in [
            [grouped[0].clone(), records[1].clone()],
            [records[0].clone(), grouped[1].clone()],
            [
                grouped[0].clone(),
                records[1].clone().with_group_id("different"),
            ],
        ] {
            assert!(
                replay(&corrupt)
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent trace group ID")
            );
        }
    }
}

#[tokio::test]
async fn metadata_capture_preserves_topology_and_decisions_without_content() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    let usage = serde_json::from_value(json!({
        "requests": 1, "input_tokens": 9, "output_tokens": 3,
        "provider_extension": "private usage extension",
    }))
    .unwrap();
    let model = Arc::new(ScriptedModel::new([ModelStep::new(vec![
        assistant_message("private response", None),
    ])
    .with_usage(usage)]));
    Runner::run(request(model, &trace, Vec::new(), false))
        .await
        .unwrap();
    let records = trace.snapshot();
    assert!(
        records
            .iter()
            .all(|record| record.event().payload().is_none())
    );
    let encoded = serde_json::to_string(&records).unwrap();
    for secret in [
        "private instructions",
        "private user input",
        "private response",
        "private transport credential",
        "guard private payload",
        "private usage extension",
    ] {
        assert!(!encoded.contains(secret));
    }
    let replayed = replay(&records).unwrap();
    assert!(!replayed.spans().is_empty());
    let usage = replayed.events_of_kind("model_usage").next().unwrap();
    assert_eq!(usage.event().fields()["usage"]["input_tokens"], 9);
    assert_eq!(
        replayed
            .events_of_kind("next_step")
            .next()
            .unwrap()
            .event()
            .fields()["next_step"],
        "final_output"
    );
}

#[tokio::test]
async fn blocking_guard_refusal_is_replayable_without_a_model_call() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    let model = Arc::new(ScriptedModel::default());
    assert!(
        Runner::run(request(model.clone(), &trace, Vec::new(), true))
            .await
            .is_err()
    );
    assert!(model.calls().is_empty());
    let replayed = replay(&trace.snapshot()).unwrap();
    let guard = replayed
        .spans()
        .values()
        .find(|span| span.fields()["span.kind"] == "guardrail")
        .unwrap();
    assert_eq!(guard.fields()["guardrail.triggered"], true);
    assert_eq!(
        replayed
            .events_of_kind("run_ended")
            .next()
            .unwrap()
            .event()
            .fields()["status"],
        "failed"
    );
    assert_eq!(replayed.events_of_kind("model_request").count(), 0);
}

#[tokio::test]
async fn retry_attempts_and_error_codes_are_retained_without_error_text() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    let model = Arc::new(ScriptedModel::new([
        ModelStep::raise_error(
            Error::provider(ProviderErrorKind::Network, "private upstream failure"),
            Some(
                RetryAdvice::new()
                    .with_suggested(true)
                    .with_replay_safety(ReplaySafety::Safe),
            ),
        ),
        ModelStep::new(vec![assistant_message("recovered", None)]),
    ]));
    let retry = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_policy(Arc::new(ProviderSuggestedRetryPolicy))
        .with_backoff(
            RetryBackoffSettings::new()
                .with_initial_delay(Duration::ZERO)
                .with_jitter(false),
        );
    Runner::run(
        request(model, &trace, Vec::new(), false).with_config(
            RunConfig::new().with_model_settings(ModelSettings::new().with_retry(retry)),
        ),
    )
    .await
    .unwrap();
    let records = trace.snapshot();
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private upstream failure")
    );
    let replayed = replay(&records).unwrap();
    assert_eq!(replayed.events_of_kind("model_request").count(), 2);
    assert_eq!(
        replayed
            .events_of_kind("model_error")
            .next()
            .unwrap()
            .event()
            .fields()["error.code"],
        "provider.network"
    );
    assert_eq!(replayed.events_of_kind("model_response").count(), 1);
}

#[tokio::test]
async fn dropping_pending_run_records_cancelled_attempt_and_closes_generation() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    let model = Arc::new(ScriptedModel::new([ModelStep::respond_async(|_| async {
        futures::future::pending::<ModelStep>().await
    })]));
    let mut run = Box::pin(Runner::run(request(model, &trace, Vec::new(), false)));
    assert!(poll!(run.as_mut()).is_pending());
    drop(run);
    let replayed = replay(&trace.snapshot()).unwrap();
    let generation = replayed
        .spans()
        .values()
        .find(|span| span.fields()["span.kind"] == "generation")
        .unwrap();
    assert_eq!(generation.fields()["outcome"], "cancelled");
    assert!(generation.ended_seq().is_some());
    assert_eq!(
        replayed
            .events_of_kind("model_error")
            .next()
            .unwrap()
            .event()
            .fields()["outcome"],
        "cancelled"
    );
}

#[test]
fn artifact_integrity_unknown_events_and_writer_failures() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    {
        let root = tracing::info_span!(
            "safe root",
            span.kind = "agent",
            input = "private ignored field"
        );
        let _root = root.enter();
        let child = tracing::info_span!(
            "safe child",
            span.kind = "generation",
            outcome = tracing::field::Empty
        );
        child.record("outcome", "ok");
        child.in_scope(|| {
            let _wrapper = tracing::info_span!("host wrapper", input = "private wrapper").entered();
            trace.record_trace(ra_core::session::rollout::RolloutTraceEvent::new(
                "wrapped_observation",
                json!({}),
                None,
            ));
        });
    }
    let records = trace.snapshot();
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private ignored field")
    );
    let replayed = replay(&records).unwrap();
    let wrapped = replayed
        .events_of_kind("wrapped_observation")
        .next()
        .unwrap();
    assert_eq!(
        replayed.spans()[wrapped.span_id().unwrap()].fields()["span.kind"],
        "generation"
    );
    assert_eq!(replayed.spans().len(), 2);
    for (index, field, value) in [
        (0, "schema_version", json!(2)),
        (0, "seq", json!(5)),
        (1, "parent_id", json!("absent")),
        (1, "trace_id", json!("wrong trace")),
    ] {
        let mut corrupt = serde_json::to_value(&records).unwrap();
        corrupt[index][field] = value;
        let corrupt =
            serde_json::from_value::<Vec<ra_eval::trace::RolloutTraceRecord>>(corrupt).unwrap();
        assert!(replay(&corrupt).is_err());
    }
    let mut unknown = serde_json::to_value(&records).unwrap();
    let mut event = unknown.as_array().unwrap().last().unwrap().clone();
    event["seq"] = json!(records.len() + 1);
    event["span_id"] = json!(null);
    event["parent_id"] = json!(null);
    event["event"]["kind"] = json!("future_event");
    unknown.as_array_mut().unwrap().push(event);
    let unknown =
        serde_json::from_value::<Vec<ra_eval::trace::RolloutTraceRecord>>(unknown).unwrap();
    assert_eq!(replay(&unknown).unwrap().events().len(), unknown.len());
    assert!(read_jsonl(&b"{invalid}\n"[..]).is_err());
    assert!(read_jsonl(&b"{\"schema_version\":1"[..]).is_err());
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk unavailable"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(write_jsonl(&records, FailingWriter).is_err());
}

struct ContinueOnce(AtomicUsize);
#[async_trait]
impl ra_core::hook::UserHook for ContinueOnce {
    fn name(&self) -> &str {
        "continue once"
    }
    async fn call(
        &self,
        _: &ra_core::hook::UserHookContext<'_>,
        _: &ra_core::hook::HookEvent<'_>,
    ) -> Result<ra_core::hook::HookDecision> {
        Ok(if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            ra_core::hook::HookDecision::Block {
                prompt: "check again".into(),
            }
        } else {
            ra_core::hook::HookDecision::Continue
        })
    }
}

#[tokio::test]
async fn next_step_observes_stop_hook_override_before_delivery() {
    let trace = LocalTrace::new();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    let model = Arc::new(ScriptedModel::new([
        ModelStep::new(vec![assistant_message("candidate", Some("candidate"))]),
        ModelStep::new(vec![assistant_message("verified", Some("verified"))]),
    ]));
    let config = RunConfig::new().with_user_hook(ra_runtime::hook::UserHookRegistration::new(
        ra_core::hook::HookEventName::Stop,
        Arc::new(ContinueOnce(AtomicUsize::new(0))),
    ));
    let result = Runner::run(request(model, &trace, Vec::new(), false).with_config(config))
        .await
        .unwrap();
    let replayed = replay(&trace.snapshot()).unwrap();
    let steps = replayed
        .events_of_kind("next_step")
        .map(|record| record.event().fields()["next_step"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(steps, ["run_again", "final_output"]);
    assert_eq!(result.final_text(), "verified");
}

#[tokio::test]
async fn session_preserves_multiple_capture_streams_and_legacy_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    let recorder = Arc::new(RolloutFileRecorder::create(
        &path,
        SessionId::new("multi-capture"),
    ));
    let mut combined = Vec::new();
    for _ in 0..2 {
        let trace = LocalTrace::new().with_recorder(recorder.clone());
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
        let model = Arc::new(ScriptedModel::new([ModelStep::new(vec![
            assistant_message("answer", None),
        ])]));
        Runner::run(request(model, &trace, Vec::new(), false))
            .await
            .unwrap();
        trace.flush().await.unwrap();
        combined.extend(trace.snapshot());
    }
    recorder.shutdown().await.unwrap();
    let persisted = read_rollout(&path).await.unwrap();
    assert_eq!(persisted, combined);
    assert_eq!(
        replay(&persisted)
            .unwrap()
            .events_of_kind("run_started")
            .count(),
        2
    );
    assert_ne!(
        persisted[0].capture_id(),
        persisted.last().unwrap().capture_id()
    );
    let path = directory.path().join("legacy.jsonl");
    let legacy = RolloutFileRecorder::create(&path, SessionId::new("legacy"));
    legacy.record(ra_core::session::rollout::RolloutItem::RunStarted(
        ra_core::session::rollout::RolloutRunStarted::new(
            RunId::new("legacy"),
            AgentId::new("legacy"),
        ),
    ));
    legacy.shutdown().await.unwrap();
    assert!(read_rollout(&path).await.unwrap().is_empty());
}

#[derive(Default)]
struct FailingRecorder {
    fail: std::sync::atomic::AtomicBool,
    records: std::sync::Mutex<Vec<ra_core::session::rollout::RolloutItem>>,
}
#[async_trait]
impl RolloutRecorder for FailingRecorder {
    fn record(&self, item: ra_core::session::rollout::RolloutItem) {
        self.records.lock().unwrap().push(item);
    }
    async fn flush(&self) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(Error::caller("storage unavailable"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn persistence_failure_is_reported_and_shutdown_can_be_retried() {
    let recorder = Arc::new(FailingRecorder::default());
    let trace = LocalTrace::new().with_recorder(recorder.clone());
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(trace.clone()));
    recorder.fail.store(true, Ordering::SeqCst);
    {
        let _span = tracing::info_span!("first", span.kind = "agent").entered();
    }
    assert!(trace.flush().await.is_err());
    assert!(trace.shutdown().await.is_err());
    assert!(trace.capture_complete());
    {
        let _span = tracing::info_span!("retry", span.kind = "agent").entered();
    }
    recorder.fail.store(false, Ordering::SeqCst);
    trace.shutdown().await.unwrap();
    let records = trace.snapshot();
    assert_eq!(replay(&records).unwrap().spans().len(), 2);
    let stored = recorder.records.lock().unwrap();
    let count = stored
        .iter()
        .filter(|item| matches!(item, ra_core::session::rollout::RolloutItem::Trace(_)))
        .count();
    assert_eq!(count, records.len());
}

#[tokio::test]
async fn subscriber_wrapped_run_closes_all_spans_when_dropped_under_another_dispatcher() {
    for host_active in [false, true] {
        let trace = LocalTrace::new();
        let model = Arc::new(ScriptedModel::new([ModelStep::respond_async(|_| async {
            futures::future::pending::<ModelStep>().await
        })]));
        let mut run = Box::pin(
            Runner::run(request(model, &trace, Vec::new(), false))
                .with_subscriber(tracing_subscriber::registry().with(trace.clone())),
        );
        assert!(poll!(run.as_mut()).is_pending());
        if host_active {
            let _host = tracing::subscriber::set_default(tracing_subscriber::registry());
            let _span = tracing::info_span!("unrelated host span").entered();
            drop(run);
        } else {
            drop(run);
        }
        let replayed = replay(&trace.snapshot()).unwrap();
        assert!(
            replayed
                .spans()
                .values()
                .all(|span| span.ended_seq().is_some())
        );
        let cancelled = replayed.events_of_kind("model_error").next().unwrap();
        let generation = &replayed.spans()[cancelled.span_id().unwrap()];
        assert_eq!(generation.fields()["span.kind"], "generation");
        assert_eq!(generation.fields()["outcome"], "cancelled");
    }
}

#[tokio::test]
async fn aborted_run_task_drops_under_its_captured_subscriber() {
    let trace = LocalTrace::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let waiting = started.clone();
    let model = Arc::new(ScriptedModel::new([ModelStep::respond_async(move |_| {
        let started = started.clone();
        async move {
            started.notify_one();
            futures::future::pending::<ModelStep>().await
        }
    })]));
    let task = tokio::spawn(
        Runner::run(request(model, &trace, Vec::new(), false))
            .with_subscriber(tracing_subscriber::registry().with(trace.clone())),
    );
    waiting.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let replayed = replay(&trace.snapshot()).unwrap();
    assert!(
        replayed
            .spans()
            .values()
            .all(|span| span.ended_seq().is_some())
    );
    assert!(
        replayed
            .events_of_kind("model_error")
            .next()
            .unwrap()
            .span_id()
            .is_some()
    );
}

#[test]
fn observations_reject_another_registry_and_use_the_nearest_enabled_ancestor() {
    let trace = LocalTrace::new();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::layer::Identity::new())
        .with(
            trace
                .clone()
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        );
    tracing::subscriber::with_default(subscriber, || {
        let _root = tracing::info_span!("captured", span.kind = "agent").entered();
        let _debug = tracing::debug_span!("filtered", span.kind = "custom").entered();
        trace.record_trace(RolloutTraceEvent::new(
            "filtered_observation",
            json!({}),
            None,
        ));
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            let _other = tracing::info_span!("other registry").entered();
            trace.record_trace(RolloutTraceEvent::new(
                "foreign_observation",
                json!({}),
                None,
            ));
        });
    });
    let replayed = replay(&trace.snapshot()).unwrap();
    assert_eq!(replayed.spans().len(), 1);
    let root = replayed.spans().values().next().unwrap();
    let captured = replayed
        .events_of_kind("filtered_observation")
        .next()
        .unwrap();
    assert_eq!(captured.span_id(), Some(root.span_id()));
    assert!(
        replayed
            .events_of_kind("foreign_observation")
            .next()
            .unwrap()
            .span_id()
            .is_none()
    );
    let span_id = root.span_id().strip_prefix("span_").unwrap();
    assert_eq!(span_id.len(), 24);
    assert!(span_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[derive(Default)]
struct LifecycleRecorder {
    flushes: AtomicUsize,
    persists: AtomicUsize,
    shutdowns: AtomicUsize,
    records: std::sync::Mutex<Vec<RolloutItem>>,
}

#[async_trait]
impl RolloutRecorder for LifecycleRecorder {
    fn record(&self, item: RolloutItem) {
        self.records.lock().unwrap().push(item);
    }
    async fn flush(&self) -> Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn persist(&self, _: PersistContext) -> Result<()> {
        self.persists.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn serialization_failure(recorder: &dyn RolloutRecorder) {
    recorder.record_trace(RolloutTraceEvent::new(
        "model_request",
        json!({"capture_error":"serialization"}),
        None,
    ));
}

struct TraceFailureTool {
    echo: Echo,
    trace: LocalTrace,
}

#[async_trait]
impl Tool for TraceFailureTool {
    fn origin(&self) -> &ToolOrigin {
        self.echo.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.echo.schema()
    }
    fn options(&self) -> ToolOptions {
        self.echo.options()
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        serialization_failure(&self.trace);
        self.echo.call(context).await
    }
}

#[tokio::test]
async fn host_sees_incomplete_capture_after_runner_flushes_in_both_modes() {
    for streamed in [false, true] {
        let storage = Arc::new(LifecycleRecorder::default());
        let trace = LocalTrace::new().with_recorder(storage.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = TraceFailureTool {
            echo: Echo(calls.clone()),
            trace: trace.clone(),
        };
        let model = Arc::new(ScriptedModel::new([
            ModelStep::new(vec![function_call(
                "echo",
                json!({"value": 7}),
                "echo-call",
                None,
                None,
            )]),
            ModelStep::new(vec![assistant_message("done", None)]),
        ]));
        let run = request(model.clone(), &trace, vec![Arc::new(tool)], false);
        let result = async {
            if streamed {
                Runner::run_streamed(run).finish().await
            } else {
                Runner::run(run).await
            }
        }
        .with_subscriber(tracing_subscriber::registry().with(trace.clone()))
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(storage.flushes.load(Ordering::SeqCst) > 0);
        assert!(!trace.capture_complete());
        trace.flush().await.unwrap();
        trace.shutdown().await.unwrap();
        trace.shutdown().await.unwrap();
        assert!(!trace.capture_complete());
        assert_eq!(storage.shutdowns.load(Ordering::SeqCst), 1);
        let replayed = replay(&trace.snapshot()).unwrap();
        assert_eq!(
            replayed
                .events()
                .iter()
                .filter(|record| { record.event().fields()["capture_error"] == "serialization" })
                .count(),
            1
        );
        model.assert_complete().unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn failed_item_payload_leaves_exportable_error_metadata() {
    use ra_core::item::{ItemId, MessageRole, RunItem, RunItemKind};
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

    let storage = Arc::new(LifecycleRecorder::default());
    let trace = LocalTrace::new()
        .with_payloads()
        .with_recorder(storage.clone());
    let path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
    let item = RunItem::new(
        ItemId::new("invalid-image"),
        RunItemKind::Message(Message::new(
            MessageRole::Assistant,
            vec![ContentBlock::image_path(path)],
        )),
    );
    assert!(serde_json::to_value(&item).is_err());
    trace.record(RolloutItem::Item(item));
    assert!(!trace.capture_complete());
    trace.flush().await.unwrap();
    trace.shutdown().await.unwrap();
    assert!(!trace.capture_complete());
    let records = trace.snapshot();
    let event = records[0].event();
    assert_eq!(event.kind(), "item");
    assert_eq!(event.fields()["item.id"], "invalid-image");
    assert_eq!(event.fields()["capture_error"], "serialization");
    assert!(event.payload().is_none());
    assert!(matches!(
        storage.records.lock().unwrap()[1],
        RolloutItem::Item(_)
    ));
    let mut exported = Vec::new();
    write_jsonl(&records, &mut exported).unwrap();
    let imported = read_jsonl(exported.as_slice()).unwrap();
    assert_eq!(imported, records);
    assert_eq!(replay(&imported).unwrap().events(), records);
}

#[tokio::test]
async fn diagnostic_failure_stays_visible_without_skipping_storage_lifecycle() {
    let recorder = Arc::new(LifecycleRecorder::default());
    let trace = LocalTrace::new().with_recorder(recorder.clone());
    let host = trace.clone();
    assert!(host.capture_complete());
    serialization_failure(&trace);
    assert!(!host.capture_complete());
    trace.flush().await.unwrap();
    trace.flush().await.unwrap();
    assert_eq!(recorder.flushes.load(Ordering::SeqCst), 2);
    trace.persist(PersistContext::Standard).await.unwrap();
    trace.persist(PersistContext::Standard).await.unwrap();
    assert_eq!(recorder.persists.load(Ordering::SeqCst), 2);
    trace.shutdown().await.unwrap();
    trace.shutdown().await.unwrap();
    assert!(!host.capture_complete());
    assert_eq!(recorder.shutdowns.load(Ordering::SeqCst), 1);
    let count = trace.snapshot().len();
    trace.record_trace(RolloutTraceEvent::new("after_shutdown", json!({}), None));
    assert_eq!(trace.snapshot().len(), count);

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deferred.jsonl");
    let trace = LocalTrace::new().with_recorder(Arc::new(RolloutFileRecorder::create(
        &path,
        SessionId::new("deferred"),
    )));
    trace.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("durable"), AgentId::new("agent")).with_input(vec![
            ModelInputItem::Message(Message::user("authoritative")),
        ]),
    ));
    serialization_failure(&trace);
    trace.shutdown().await.unwrap();
    trace.shutdown().await.unwrap();
    assert!(!trace.capture_complete());
    let stored = RolloutReader::open(&path).read_all().await.unwrap();
    assert_eq!(reconstruct_history(&stored).unwrap().history().len(), 1);
}

#[tokio::test]
async fn child_capture_keeps_history_and_shutdown_independent() {
    let parent_storage = Arc::new(LifecycleRecorder::default());
    let child_storage = Arc::new(LifecycleRecorder::default());
    let trace = LocalTrace::new().with_recorder(parent_storage.clone());
    let child = trace.child_trace(Some(child_storage.clone())).unwrap();
    child.record(RolloutItem::RunStarted(RolloutRunStarted::new(
        RunId::new("child"),
        AgentId::new("child"),
    )));
    serialization_failure(&trace);
    child.flush().await.unwrap();
    child.persist(PersistContext::Standard).await.unwrap();
    child.shutdown().await.unwrap();
    child.shutdown().await.unwrap();
    assert_eq!(child_storage.flushes.load(Ordering::SeqCst), 1);
    assert_eq!(child_storage.persists.load(Ordering::SeqCst), 1);
    assert_eq!(child_storage.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(parent_storage.flushes.load(Ordering::SeqCst), 0);
    assert_eq!(parent_storage.persists.load(Ordering::SeqCst), 0);
    assert_eq!(parent_storage.shutdowns.load(Ordering::SeqCst), 0);
    assert!(
        parent_storage
            .records
            .lock()
            .unwrap()
            .iter()
            .all(|item| matches!(item, RolloutItem::Trace(_)))
    );
    assert!(matches!(
        child_storage.records.lock().unwrap()[0],
        RolloutItem::RunStarted(_)
    ));
    trace.record_trace(RolloutTraceEvent::new("parent_continues", json!({}), None));
    assert_eq!(
        replay(&trace.snapshot())
            .unwrap()
            .events_of_kind("parent_continues")
            .count(),
        1
    );
    trace.shutdown().await.unwrap();
    assert!(!trace.capture_complete());
    assert_eq!(parent_storage.shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_lifecycle_returns_only_its_own_storage_errors() {
    let parent_storage = Arc::new(FailingRecorder::default());
    parent_storage.fail.store(true, Ordering::SeqCst);
    let child_storage = Arc::new(FailingRecorder::default());
    child_storage.fail.store(true, Ordering::SeqCst);
    let trace = LocalTrace::new().with_recorder(parent_storage.clone());
    let child = trace.child_trace(Some(child_storage.clone())).unwrap();
    serialization_failure(child.as_ref());
    assert_eq!(
        child.flush().await.unwrap_err().to_string(),
        Error::caller("storage unavailable").to_string()
    );
    assert_eq!(
        child
            .persist(PersistContext::Standard)
            .await
            .unwrap_err()
            .to_string(),
        Error::caller("storage unavailable").to_string()
    );
    assert_eq!(
        child.shutdown().await.unwrap_err().to_string(),
        Error::caller("storage unavailable").to_string()
    );
    child_storage.fail.store(false, Ordering::SeqCst);
    child.flush().await.unwrap();
    child.persist(PersistContext::Standard).await.unwrap();
    child.shutdown().await.unwrap();
    let no_storage = trace.child_trace(None).unwrap();
    no_storage.flush().await.unwrap();
    no_storage.persist(PersistContext::Standard).await.unwrap();
    no_storage.shutdown().await.unwrap();
    assert_eq!(
        trace.flush().await.unwrap_err().to_string(),
        Error::caller("storage unavailable").to_string()
    );
    parent_storage.fail.store(false, Ordering::SeqCst);
    trace.shutdown().await.unwrap();
    trace.shutdown().await.unwrap();
    assert!(!trace.capture_complete());
}

#[test]
fn span_exit_after_capture_thread_local_teardown_does_not_panic() {
    thread_local! {
        // Initialized before the layer's entered stack, so this guard drops after that stack.
        static LATE_SPAN: std::cell::RefCell<Option<tracing::span::EnteredSpan>> = const {
            std::cell::RefCell::new(None)
        };
    }
    let trace = LocalTrace::new();
    let layer = trace.clone();
    std::thread::spawn(move || {
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            LATE_SPAN.with(|late| {
                *late.borrow_mut() =
                    Some(tracing::info_span!("late", span.kind = "agent").entered());
            });
        });
    })
    .join()
    .unwrap();
    let replayed = replay(&trace.snapshot()).unwrap();
    assert_eq!(replayed.spans().len(), 1);
    assert!(
        replayed
            .spans()
            .values()
            .next()
            .unwrap()
            .ended_seq()
            .is_some()
    );
}

#[tokio::test]
async fn trace_host_events_follow_session_retention_in_both_content_modes() {
    use ra_core::{
        event::{
            ExecEvent, FileEvent, HostEvent, HostEventBody,
            exec::{ExecOutputEvent, ExecStreamKind},
            file::FileReadEvent,
        },
        hook::{HookDecision, HookEventName, HookReport, HookRunStatus},
    };
    for payloads in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filtered.jsonl");
        let trace = LocalTrace::new();
        let trace = if payloads {
            trace.with_payloads()
        } else {
            trace
        };
        let trace = trace.with_recorder(Arc::new(RolloutFileRecorder::create(
            &path,
            SessionId::new("filtered"),
        )));
        let run = request(
            Arc::new(ScriptedModel::default()),
            &trace,
            Vec::new(),
            false,
        );
        for body in [
            HostEventBody::Exec(ExecEvent::Output(ExecOutputEvent::new(
                "process",
                ExecStreamKind::Stdout,
                0,
                15,
                "private delta",
            ))),
            HostEventBody::File(FileEvent::Read(FileReadEvent::new(
                CallId::new("read"),
                "private/path",
                15,
            ))),
            HostEventBody::Hook(HookReport::new(
                "private hook",
                HookEventName::Stop,
                HookRunStatus::Completed,
                HookDecision::Continue,
            )),
            HostEventBody::Unknown {
                family: "future_event".into(),
                data: json!({"keep":true}),
            },
        ] {
            trace.record(RolloutItem::Event(
                HostEvent::allocate(run.event_seq_allocator(), AgentId::new("agent"), body)
                    .unwrap(),
            ));
        }
        trace.shutdown().await.unwrap();
        let records = trace.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event().kind(), "host_event");
        assert_eq!(records[0].event().payload().is_some(), payloads);
        assert_eq!(read_rollout(&path).await.unwrap(), records);
        let stored = std::fs::read_to_string(path).unwrap();
        for secret in ["private delta", "private/path", "private hook"] {
            assert!(!stored.contains(secret));
        }
        assert!(stored.contains("future_event"));
    }
}

#[tokio::test]
async fn nested_agent_tools_share_trace_evidence_without_copying_child_history() {
    for (streamed, group, child_group) in [
        (false, None, None),
        (true, None, None),
        (false, None, Some("child")),
        (true, None, Some("child")),
        (false, Some("parent"), Some("child")),
        (true, Some("parent"), Some("child")),
    ] {
        let storage = Arc::new(LifecycleRecorder::default());
        let trace = LocalTrace::new()
            .with_payloads()
            .with_recorder(storage.clone());
        let nested = AgentSpec::builder()
            .id(AgentId::new("nested"))
            .name("Nested")
            .model("fixture/scripted")
            .build()
            .unwrap();
        let mut builder = nested.as_tool();
        if let Some(child_group) = child_group {
            builder = builder.run_config(RunConfig::new().with_group_id(child_group));
        }
        if streamed {
            builder = builder.on_stream(Arc::new(
                |_: ra_runtime::agent::tool::AgentToolStreamEvent| Ok(()),
            ));
        }
        let model = Arc::new(ScriptedModel::new([
            ModelStep::new(vec![function_call(
                "nested",
                json!({"input":"child task"}),
                "nested-call",
                None,
                None,
            )]),
            ModelStep::new(vec![assistant_message(
                "child answer",
                Some("child-message"),
            )]),
            ModelStep::new(vec![assistant_message(
                "parent answer",
                Some("parent-message"),
            )]),
        ]));
        let config = group.map_or_else(RunConfig::new, |group| {
            RunConfig::new().with_group_id(group)
        });
        let raw = AgentSpanGroups::default();
        Runner::run(
            request(
                model.clone(),
                &trace,
                vec![Arc::new(builder.build().unwrap())],
                false,
            )
            .with_config(config),
        )
        .with_subscriber(
            tracing_subscriber::registry()
                .with(trace.clone())
                .with(raw.clone()),
        )
        .await
        .unwrap();
        trace.shutdown().await.unwrap();
        // The nested run executes in its parent's trace, so its span names no group of its own.
        assert_eq!(*raw.0.lock().unwrap(), [group.map(str::to_owned), None]);
        let records = trace.snapshot();
        assert!(records.iter().all(|record| record.group_id() == group));
        assert!(
            records
                .iter()
                .all(|record| record.trace_id() == records[0].trace_id())
        );
        let replayed = replay(&records).unwrap();
        assert_eq!(replayed.events_of_kind("model_request").count(), 3);
        assert_eq!(replayed.events_of_kind("model_response").count(), 3);
        assert_eq!(replayed.events_of_kind("next_step").count(), 3);
        assert_eq!(replayed.events_of_kind("run_started").count(), 2);
        let starts: Vec<_> = replayed.events_of_kind("run_started").collect();
        assert_eq!(
            starts[1].event().fields()["parent_run_id"],
            starts[0].event().fields()["run.id"]
        );
        assert!(
            replayed
                .events_of_kind("model_response")
                .all(|event| event.span_id().is_some())
        );
        assert!(
            replayed
                .spans()
                .values()
                .all(|span| span.ended_seq().is_some())
        );
        let stored = storage.records.lock().unwrap();
        assert_eq!(
            stored
                .iter()
                .filter(|item| matches!(item, RolloutItem::RunStarted(_)))
                .count(),
            1
        );
        assert!(!stored.iter().any(
            |item| matches!(item, RolloutItem::Item(item) if item.id().as_str() == "child-message")
        ));
        model.assert_complete().unwrap();
    }
}

/// A nested pause captures its enclosing trace, even when the child's configuration differs.
#[tokio::test]
async fn nested_approval_checkpoint_captures_parent_trace_group_and_resumes() {
    for group in [Some("parent"), None] {
        for (streamed, child_streamed) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let trace = LocalTrace::new();
            let _subscriber = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(trace.clone()),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let nested = AgentSpec::builder()
                .id(AgentId::new("nested"))
                .name("Nested")
                .model("fixture/scripted")
                .tool(Arc::new(GatedEcho(Echo(calls.clone()))))
                .build()
                .unwrap();
            let mut builder = nested
                .as_tool()
                .run_config(RunConfig::new().with_group_id("child"));
            if child_streamed {
                builder = builder.on_stream(Arc::new(
                    |_: ra_runtime::agent::tool::AgentToolStreamEvent| Ok(()),
                ));
            }
            let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(builder.build().unwrap())];
            let model = Arc::new(ScriptedModel::new([
                ModelStep::new(vec![function_call(
                    "nested",
                    json!({"input": "child task"}),
                    "nested-call",
                    None,
                    None,
                )]),
                ModelStep::new(vec![function_call(
                    "echo",
                    json!({"value": 1}),
                    "gated-call",
                    None,
                    None,
                )]),
                ModelStep::new(vec![assistant_message("child answer", None)]),
                ModelStep::new(vec![assistant_message("parent answer", None)]),
            ]));
            let interrupted = run_to_end(
                request(model.clone(), &trace, tools.clone(), false)
                    .with_config(group_config(group)),
                streamed,
            )
            .await;
            let RunOutcome::Interrupted { items } = interrupted.outcome() else {
                panic!("the nested gated call must pause both runs");
            };
            assert_eq!(items.len(), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(interrupted.state().trace_group_id(), group);
            assert_eq!(interrupted.state().nested_runs().len(), 1);
            assert_eq!(
                interrupted.state().nested_runs()[0]
                    .state()
                    .unwrap()
                    .trace_group_id(),
                group
            );
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(interrupted.state()).unwrap()).unwrap();
            assert_eq!(
                state.nested_runs()[0].state().unwrap().trace_group_id(),
                group
            );
            state.approve(&items[0], false).unwrap();
            let start = trace.snapshot().len();
            // No parent config is supplied: the resumed segment uses its checkpoint's group.
            let result = run_to_end(
                resume_request(model.clone(), &trace, tools, state),
                streamed,
            )
            .await;
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(result.final_text(), "parent answer");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.state().trace_group_id(), group);
            assert!(result.state().nested_runs().is_empty());
            trace.shutdown().await.unwrap();
            let records = trace.snapshot();
            let (paused, continued) = records.split_at(start);
            assert!(!paused.is_empty() && !continued.is_empty());
            assert!(records.iter().all(|record| record.group_id() == group));
            assert_ne!(paused[0].trace_id(), continued[0].trace_id());
            assert_eq!(
                continued
                    .iter()
                    .filter(|record| record.event().kind() == "run_started")
                    .count(),
                2
            );
            replay(&records).unwrap();
            model.assert_complete().unwrap();
        }
    }
}

struct SpawnWorker;
#[async_trait]
impl Tool for SpawnWorker {
    fn origin(&self) -> &ToolOrigin {
        static ORIGIN: std::sync::OnceLock<ToolOrigin> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(|| ToolOrigin::new("spawn_worker").unwrap())
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            ToolSchema::new(
                "spawn_worker",
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            )
            .unwrap()
        })
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        context
            .services()
            .agent_control()
            .unwrap()
            .spawn(
                SpawnAgentRequest::new("worker", "child task")
                    .with_agent_type(AgentId::new("worker")),
            )
            .await
            .map_err(|error| Error::caller(error.to_string()))?;
        Ok(ToolOutput::text("spawned"))
    }
}

#[tokio::test]
async fn closing_spawned_agent_ignores_parent_trace_failures_with_or_without_store() {
    for with_store in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let control = if with_store {
            AgentControl::new()
                .with_rollout_store(
                    Arc::new(ra_session::RolloutThreadDirectory::new(directory.path())),
                    SessionId::new("root"),
                )
                .unwrap()
        } else {
            AgentControl::new()
        };
        let storage = Arc::new(FailingRecorder::default());
        storage.fail.store(true, Ordering::SeqCst);
        let trace = LocalTrace::new().with_recorder(storage.clone());
        serialization_failure(&trace);
        let worker = AgentSpec::builder()
            .id(AgentId::new("worker"))
            .name("Worker")
            .model("fixture/scripted")
            .build()
            .unwrap();
        let model = Arc::new(ScriptedModel::new([
            ModelStep::new(vec![function_call(
                "spawn_worker",
                json!({}),
                "spawn",
                None,
                None,
            )]),
            ModelStep::new(vec![assistant_message("done", None)]),
            ModelStep::new(vec![assistant_message("done", None)]),
        ]));
        let agents = AgentRegistry::builder().register(worker).build().unwrap();
        let run = request(model.clone(), &trace, vec![Arc::new(SpawnWorker)], false)
            .with_config(RunConfig::new().with_agent_registry(agents))
            .with_agent_control(control.root());
        let result = Runner::run(run)
            .with_subscriber(tracing_subscriber::registry().with(trace.clone()))
            .await
            .unwrap();
        assert_eq!(result.final_text(), "done");
        let worker_path = AgentPath::root().join("worker").unwrap();
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            control.wait_for_status(&worker_path, AgentStatus::is_idle),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(status, AgentStatus::Completed(_)), "{status:?}");
        control.close(&worker_path).await.unwrap();
        control.shutdown().await;
        assert_eq!(
            trace.flush().await.unwrap_err().to_string(),
            Error::caller("storage unavailable").to_string()
        );
        storage.fail.store(false, Ordering::SeqCst);
        trace.shutdown().await.unwrap();
        assert!(!trace.capture_complete());
        model.assert_complete().unwrap();
    }
}

#[tokio::test]
async fn spawned_agents_share_evidence_and_preserve_their_own_rollout() {
    use ra_session::{LoadThreadHistoryParams, ThreadStore};
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), SessionId::new("root"))
        .unwrap();
    let storage = Arc::new(LifecycleRecorder::default());
    let trace = LocalTrace::new()
        .with_payloads()
        .with_recorder(storage.clone());
    let worker = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .model("fixture/scripted")
        .build()
        .unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let release_worker = gate.clone();
    let gate_model = gate.clone();
    // Parent and child may race for the model. The input, rather than call order, selects the step.
    let model = Arc::new(ScriptedModel::new([
        ModelStep::new(vec![function_call(
            "spawn_worker",
            json!({}),
            "spawn",
            None,
            None,
        )]),
        ModelStep::respond_async(move |call| {
            let gate = gate_model.clone();
            async move {
                if call.request().input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content().contains("child task"))) {
                    gate.notified().await;
                    ModelStep::new(vec![assistant_message("worker answer", Some("worker-message"))])
                } else {
                    ModelStep::new(vec![assistant_message("parent answer", Some("parent-message"))])
                }
            }
        }),
        ModelStep::respond_async(move |call| {
            let gate = gate.clone();
            async move {
                if call.request().input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content().contains("child task"))) {
                    gate.notified().await;
                    ModelStep::new(vec![assistant_message("worker answer", Some("worker-message"))])
                } else {
                    ModelStep::new(vec![assistant_message("parent answer", Some("parent-message"))])
                }
            }
        }),
    ]));
    let registry_agents = AgentRegistry::builder().register(worker).build().unwrap();
    let run = request(model.clone(), &trace, vec![Arc::new(SpawnWorker)], false)
        .with_config(
            RunConfig::new()
                .with_group_id("agent-tree")
                .with_agent_registry(registry_agents),
        )
        .with_agent_control(control.root());
    Runner::run(run)
        .with_subscriber(tracing_subscriber::registry().with(trace.clone()))
        .await
        .unwrap();
    // The worker continues after the parent's subscriber scope has been left.
    release_worker.notify_one();
    let worker_path = AgentPath::root().join("worker").unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        control.wait_for_status(&worker_path, AgentStatus::is_idle),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(status, AgentStatus::Completed(_)), "{status:?}");
    let session_id = control
        .agents()
        .into_iter()
        .find(|agent| agent.agent_path() == &worker_path)
        .unwrap()
        .session_id()
        .unwrap()
        .clone();
    control.shutdown().await;
    trace.shutdown().await.unwrap();
    let records = trace.snapshot();
    assert!(
        records
            .iter()
            .filter(|record| record.span_id().is_some())
            .all(|record| record.group_id() == Some("agent-tree")),
    );
    assert!(
        records
            .iter()
            .filter(|record| record.span_id().is_none())
            .all(|record| record.group_id().is_none())
    );
    let replayed = replay(&records).unwrap();
    assert_eq!(replayed.events_of_kind("model_request").count(), 3);
    assert_eq!(replayed.events_of_kind("model_response").count(), 3);
    assert_eq!(replayed.events_of_kind("next_step").count(), 3);
    let starts: Vec<_> = replayed.events_of_kind("run_started").collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1].event().fields()["parent_run_id"],
        starts[0].event().fields()["run.id"]
    );
    assert!(
        replayed
            .spans()
            .values()
            .all(|span| span.ended_seq().is_some())
    );
    assert!(
        replayed
            .events_of_kind("model_response")
            .all(|event| event.span_id().is_some())
    );
    assert!(
        records
            .iter()
            .all(|record| record.capture_id() == records[0].capture_id())
    );
    let child_history = store
        .load_history(&LoadThreadHistoryParams::new(session_id))
        .await
        .unwrap();
    assert!(child_history.records().iter().any(|record| matches!(record.payload().unwrap(), ra_session::RolloutPayload::Item(item) if item.id().as_str() == "worker-message")));
    assert!(storage.records.lock().unwrap().iter().all(
        |item| !matches!(item, RolloutItem::Item(item) if item.id().as_str() == "worker-message")
    ));
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn thread_storage_lifecycle_ignores_diagnostic_failures() {
    use ra_core::event::HostEventBody;
    use ra_session::RolloutPayload;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("thread.jsonl");
    let trace = LocalTrace::new().with_recorder(Arc::new(RolloutFileRecorder::create(
        &path,
        SessionId::new("thread"),
    )));
    let agent = AgentSpec::builder()
        .id(AgentId::new("thread"))
        .name("Thread")
        .model("fixture/scripted")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("thread"),
        AgentBinding::direct(agent),
        registry(Arc::new(
            ScriptedModel::new(std::iter::empty::<ModelStep>()),
        )),
    )
    .with_rollout_recorder(Arc::new(trace.clone()))
    .build();
    serialization_failure(&trace);
    assert!(!trace.capture_complete());
    assert!(!path.exists());
    thread.ensure_rollout_materialized().await.unwrap();
    assert!(path.exists());
    thread.flush_rollout().await.unwrap();
    let settings = json!({"model": "fixture/scripted"});
    thread
        .persist_settings_snapshot(settings.clone())
        .await
        .unwrap();
    thread.shutdown().await;
    thread.close_rollout().await.unwrap();
    thread.close_rollout().await.unwrap();
    assert!(!trace.capture_complete());
    let stored = RolloutReader::open(&path).read_all().await.unwrap();
    assert!(stored.iter().any(|record| {
        matches!(record.payload().unwrap(), RolloutPayload::Event(event)
            if matches!(event.body(), HostEventBody::Unknown { family, data }
                if family == "threadSettingsApplied" && data == &settings))
    }));
    let restored = read_rollout(&path).await.unwrap();
    assert_eq!(restored, trace.snapshot());
    assert_eq!(
        restored
            .iter()
            .filter(|record| record.event().fields()["capture_error"] == "serialization")
            .count(),
        1
    );
}

#[tokio::test]
async fn thread_background_turn_forwards_trace_observations_and_subscriber() {
    let trace = LocalTrace::new();
    let model = Arc::new(ScriptedModel::new([ModelStep::new(vec![
        assistant_message("thread answer", None),
    ])]));
    let agent = AgentSpec::builder()
        .id(AgentId::new("thread"))
        .name("Thread")
        .model("fixture/scripted")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("thread"),
        AgentBinding::direct(agent),
        registry(model.clone()),
    )
    .with_config(RunConfig::new().with_group_id("thread-group"))
    .with_rollout_recorder(Arc::new(trace.clone()))
    .build();
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text(
            "thread input",
        )]))
        .with_subscriber(tracing_subscriber::registry().with(trace.clone()))
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = thread.next_event().await {
            if matches!(event, ThreadEvent::TurnComplete { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    thread.shutdown().await;
    trace.shutdown().await.unwrap();
    let records = trace.snapshot();
    assert!(
        records
            .iter()
            .filter(|record| record.span_id().is_some())
            .all(|record| record.group_id() == Some("thread-group")),
    );
    assert!(
        records
            .iter()
            .filter(|record| record.span_id().is_none())
            .all(|record| record.group_id().is_none())
    );
    let replayed = replay(&records).unwrap();
    assert_eq!(replayed.events_of_kind("model_request").count(), 1);
    assert_eq!(replayed.events_of_kind("model_response").count(), 1);
    assert_eq!(replayed.events_of_kind("next_step").count(), 1);
    assert!(
        replayed
            .spans()
            .values()
            .all(|span| span.ended_seq().is_some())
    );
    assert!(
        replayed
            .events_of_kind("model_response")
            .next()
            .unwrap()
            .span_id()
            .is_some()
    );
    model.assert_complete().unwrap();
}

struct HangingEcho {
    echo: Echo,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for HangingEcho {
    fn origin(&self) -> &ToolOrigin {
        self.echo.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.echo.schema()
    }
    fn options(&self) -> ToolOptions {
        self.echo.options()
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        self.started.notify_one();
        futures::future::pending().await
    }
}

#[tokio::test]
async fn dropping_stream_closes_model_and_aborted_tool_tasks_under_their_subscriber() {
    for tool_waits in [false, true] {
        let trace = LocalTrace::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let model = if tool_waits {
            Arc::new(ScriptedModel::new([ModelStep::new(vec![function_call(
                "echo",
                json!({"value":7}),
                "hanging-call",
                None,
                None,
            )])]))
        } else {
            let started = started.clone();
            Arc::new(ScriptedModel::new([ModelStep::respond_async(move |_| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    futures::future::pending::<ModelStep>().await
                }
            })]))
        };
        let tools: Vec<Arc<dyn Tool>> = if tool_waits {
            vec![Arc::new(HangingEcho {
                echo: Echo(Arc::new(AtomicUsize::new(0))),
                started: started.clone(),
            })]
        } else {
            Vec::new()
        };
        let stream = tracing::subscriber::with_default(
            tracing_subscriber::registry().with(trace.clone()),
            || Runner::run_streamed(request(model, &trace, tools, false)),
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        {
            let _host = tracing::subscriber::set_default(tracing_subscriber::registry());
            let _span = tracing::info_span!("unrelated host during stream drop").entered();
            drop(stream);
        }
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                let replayed = replay(&trace.snapshot()).unwrap();
                if !replayed.spans().is_empty()
                    && replayed
                        .spans()
                        .values()
                        .all(|span| span.ended_seq().is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let replayed = replay(&trace.snapshot()).unwrap();
        if tool_waits {
            assert!(
                replayed
                    .spans()
                    .values()
                    .any(|span| span.fields()["span.kind"] == "function")
            );
        } else {
            assert!(
                replayed
                    .events_of_kind("model_error")
                    .next()
                    .unwrap()
                    .span_id()
                    .is_some()
            );
        }
    }
}

#[tokio::test]
async fn aborting_run_closes_its_spawned_tool_spans() {
    let trace = LocalTrace::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new([ModelStep::new(vec![function_call(
        "echo",
        json!({"value":7}),
        "aborted-tool",
        None,
        None,
    )])]));
    let tool = HangingEcho {
        echo: Echo(Arc::new(AtomicUsize::new(0))),
        started: started.clone(),
    };
    let task = tokio::spawn(
        Runner::run(request(model, &trace, vec![Arc::new(tool)], false))
            .with_subscriber(tracing_subscriber::registry().with(trace.clone())),
    );
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replayed = replay(&trace.snapshot()).unwrap();
            if replayed
                .spans()
                .values()
                .all(|span| span.ended_seq().is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replayed = replay(&trace.snapshot()).unwrap();
    assert!(
        replayed
            .spans()
            .values()
            .any(|span| span.fields()["span.kind"] == "function")
    );
}
