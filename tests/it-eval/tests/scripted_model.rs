//! Scripted-model contracts adapted from the upstream testing model suite (MIT).

use std::{
    error::Error as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt, TryStreamExt, stream};
use ra_core::{
    error::{Error, ProviderErrorKind},
    item::{
        AgentId, CallId, HandoffCall, ItemId, McpListTools, Message, ModelInputItem, ModelResponse,
        Reasoning, RunItem, RunItemKind, ToolCall, ToolCallKind,
    },
    model::{
        ConversationContinuation, Model, ModelProvider, ModelRequest, ModelRetryAdviceRequest,
        ModelSettings, ModelStreamDelta, ModelStreamDeltaKind, ModelStreamEvent,
        ModelToolDefinition, ModelTracing, NormalizedProviderError, ProviderKey, ReplaySafety,
        RetryAdvice, stamp_replay_safety,
    },
    usage::{RequestUsage, Usage},
};
use ra_eval::fixture::{
    ModelStep, ScriptedModel, ScriptedProvider, UnconsumedModelSteps, UnexpectedModelCall,
    assistant_message, function_call,
};
use serde_json::json;

fn request() -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("fixture input"))],
        ModelSettings::new().resolve(
            &ProviderKey::new("fixture"),
            &ModelSettings::new(),
            &ModelSettings::new(),
            &ModelSettings::new(),
        ),
    )
    .with_system_instructions("system fixture")
    .with_tools(vec![ModelToolDefinition::new(
        "echo",
        json!({"type": "object"}),
    )])
    .with_previous_response_id("previous")
    .with_tracing(ModelTracing::EnabledWithoutData)
}

fn output(text: &str) -> Vec<RunItem> {
    vec![assistant_message(text, None)]
}

#[tokio::test]
async fn queue_exhaustion_and_completion_preserve_structured_evidence() {
    let model = ScriptedModel::default();
    assert!(model.first_call().is_none());
    model.enqueue(output("first"));
    model.extend([ModelStep::new(output("second")), ModelStep::default()]);
    let error = model.assert_complete().unwrap_err();
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<UnconsumedModelSteps>()
            .unwrap()
            .remaining_steps(),
        3
    );
    assert_eq!(
        model.get_response(request()).await.unwrap().output(),
        output("first")
    );
    assert_eq!(
        model.get_response(request()).await.unwrap().output(),
        output("second")
    );
    model.get_response(request()).await.unwrap();
    model.assert_complete().unwrap();

    for streamed in [false, true] {
        let error = if streamed {
            model
                .stream_response(request())
                .try_collect::<Vec<_>>()
                .await
                .unwrap_err()
        } else {
            model.get_response(request()).await.unwrap_err()
        };
        let unexpected = error
            .source()
            .unwrap()
            .downcast_ref::<UnexpectedModelCall>()
            .unwrap();
        assert_eq!(unexpected.call_index(), if streamed { 4 } else { 3 });
        assert_eq!(
            unexpected.to_string(),
            if streamed {
                "Unexpected streaming model call #5: no scripted steps remain."
            } else {
                "Unexpected non-streaming model call #4: no scripted steps remain."
            }
        );
        assert_eq!(unexpected.call().streamed(), streamed);
        assert_eq!(unexpected.call().request(), &request());
    }
    assert_eq!(model.calls().len(), 5);
    assert_eq!(model.remaining_steps(), 0);
}

#[tokio::test]
async fn request_and_history_snapshots_are_detached() {
    let model = ScriptedModel::new([ModelStep::default(), ModelStep::default()]);
    let original = request();
    model.get_response(original.clone()).await.unwrap();
    let changed = original.with_input(Vec::new());
    assert_ne!(changed, request());
    let mut calls = model.calls();
    calls.clear();
    assert_eq!(model.first_call().unwrap().request(), &request());
    assert!(!model.first_call().unwrap().streamed());
    model
        .stream_response(changed.clone())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(model.last_call().unwrap().request(), &changed);
    assert!(model.last_call().unwrap().streamed());
    assert_eq!(model.calls().len(), 2);
}

#[tokio::test]
async fn usage_defaults_preserve_explicit_entries_and_transport_ids() {
    let detailed = Usage::from_request(
        RequestUsage::new(10, 7)
            .with_cached_input_tokens(3)
            .with_cache_write_tokens(2)
            .with_reasoning_tokens(4),
    );
    let mut zero_requests = serde_json::to_value(&detailed).unwrap();
    zero_requests["requests"] = json!(0);
    let zero_requests: Usage = serde_json::from_value(zero_requests).unwrap();
    let response = ModelResponse::new(output("explicit"))
        .with_usage(zero_requests.clone())
        .with_request_id("request-fixed")
        .with_response_id("");
    let model = ScriptedModel::new([
        ModelStep::default(),
        ModelStep::default(),
        response.clone().into(),
        ModelStep::from(ModelResponse::new(Vec::new()).with_usage(Usage::default())),
    ]);
    assert_eq!(
        model
            .get_response(request())
            .await
            .unwrap()
            .usage()
            .requests(),
        1
    );
    model.set_default_usage(Some(detailed.clone()));
    assert_eq!(
        model.get_response(request()).await.unwrap().usage(),
        &detailed
    );
    let configured =
        ScriptedModel::new([ModelStep::default()]).with_default_usage(detailed.clone());
    assert_eq!(
        configured.get_response(request()).await.unwrap().usage(),
        &detailed
    );
    let streamed = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        streamed.last(),
        Some(&ModelStreamEvent::Completed(Box::new(response)))
    );
    model.set_default_usage(None);
    let absent_id = model.get_response(request()).await.unwrap();
    assert_eq!(absent_id.response_id(), None);
    assert_eq!(absent_id.usage().requests(), 1);
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn recursive_sync_and_async_responders_use_one_call_and_do_not_hold_the_queue_lock() {
    let model = Arc::new(ScriptedModel::default());
    let weak = Arc::downgrade(&model);
    model.enqueue(ModelStep::respond_async(move |call| {
        let model = weak.upgrade().unwrap();
        async move {
            assert_eq!(model.calls(), vec![call.clone()]);
            model.enqueue(output("queued"));
            ModelStep::respond(move |nested| {
                assert_eq!(nested, call);
                output("dynamic").into()
            })
        }
    }));
    let events = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert!(
        matches!(events.last(), Some(ModelStreamEvent::Completed(response)) if response.output() == output("dynamic"))
    );
    assert_eq!(model.calls().len(), 1);
    assert_eq!(model.remaining_steps(), 1);
    assert_eq!(
        model.get_response(request()).await.unwrap().output(),
        output("queued")
    );
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn retry_advice_is_scoped_to_the_exact_error_and_model_and_preserves_sources() {
    let advice = RetryAdvice::new()
        .with_suggested(true)
        .with_replay_safety(ReplaySafety::Safe);
    let first = Error::provider(ProviderErrorKind::Network, "first")
        .with_source(std::io::Error::other("original source"));
    let model = ScriptedModel::new([
        ModelStep::raise_error(first, Some(advice.clone())),
        ModelStep::respond(|_| Error::provider(ProviderErrorKind::Timeout, "second").into()),
    ]);
    let first = model.get_response(request()).await.unwrap_err();
    let second = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    let continuation = ConversationContinuation::None;
    let context = |error| ModelRetryAdviceRequest::new(error, 1, false, &continuation);
    assert_eq!(
        model.get_retry_advice(&context(&first)),
        Some(advice.clone())
    );
    assert_eq!(model.get_retry_advice(&context(&second)), None);
    assert_eq!(
        ScriptedModel::default().get_retry_advice(&context(&first)),
        None
    );
    assert_eq!(
        first.source().unwrap().source().unwrap().to_string(),
        "original source"
    );
    assert!(matches!(
        first,
        Error::Provider {
            kind: ProviderErrorKind::Network,
            ..
        }
    ));
    let wrapped = stamp_replay_safety(first, ReplaySafety::Safe);
    assert_eq!(
        model.get_retry_advice(&ModelRetryAdviceRequest::new(
            &wrapped,
            1,
            false,
            &continuation,
        )),
        Some(advice)
    );
}

#[tokio::test]
async fn advice_node_keeps_normalized_provider_facts_reachable_through_the_chain() {
    let advice = RetryAdvice::new().with_suggested(true);
    let normalized = NormalizedProviderError::new(ProviderErrorKind::RateLimit, "throttled")
        .with_status_code(429)
        .with_retry_after(Duration::from_millis(250))
        .into_error();
    let model = ScriptedModel::new([ModelStep::raise_error(normalized, Some(advice.clone()))]);
    let error = model.get_response(request()).await.unwrap_err();
    let facts = NormalizedProviderError::from_error(&error).unwrap();
    assert_eq!(facts.status_code(), Some(429));
    assert_eq!(facts.retry_after(), Some(Duration::from_millis(250)));
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<NormalizedProviderError>()
            .is_none()
    );
    assert!(
        error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<NormalizedProviderError>()
            .is_some()
    );
    let continuation = ConversationContinuation::None;
    assert_eq!(
        model.get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            1,
            false,
            &continuation,
        )),
        Some(advice)
    );
}

#[tokio::test]
async fn reusing_an_owned_error_clears_stale_advice_including_through_runtime_wrappers() {
    let model = ScriptedModel::new([ModelStep::raise_error(
        Error::provider(ProviderErrorKind::Network, "reused error"),
        Some(RetryAdvice::new().with_suggested(true)),
    )]);
    let error = model.get_response(request()).await.unwrap_err();
    let continuation = ConversationContinuation::None;
    assert!(
        model
            .get_retry_advice(&ModelRetryAdviceRequest::new(
                &error,
                1,
                false,
                &continuation,
            ))
            .is_some()
    );
    let error = Mutex::new(Some(stamp_replay_safety(error, ReplaySafety::Safe)));
    model.enqueue(ModelStep::respond(move |_| {
        error.lock().unwrap().take().unwrap().into()
    }));
    let error = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    assert_eq!(
        model.get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            2,
            true,
            &continuation,
        )),
        None
    );
    assert!(matches!(
        error,
        Error::Provider {
            kind: ProviderErrorKind::Network,
            ..
        }
    ));
    model.enqueue(ModelStep::raise_error(
        error,
        Some(RetryAdvice::new().with_suggested(true)),
    ));
    let error = model.get_response(request()).await.unwrap_err();
    let error = Mutex::new(Some(error));
    model.enqueue(ModelStep::stream_factory(move |_| {
        let error = error.lock().unwrap().take().unwrap();
        Box::pin(stream::once(async move { Err(error) }))
    }));
    let error = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    assert_eq!(
        model.get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            3,
            true,
            &continuation,
        )),
        None
    );
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn exact_streams_are_not_repaired_and_have_a_separate_nonstreaming_fallback() {
    let event = ModelStreamEvent::Delta(ModelStreamDelta::new(
        ItemId::new("exact"),
        ModelStreamDeltaKind::OutputText {
            delta: "partial".into(),
        },
    ));
    let events = vec![event];
    let model = ScriptedModel::new([
        ModelStep::stream(events.clone()),
        ModelStep::stream(Vec::new()).with_response(ModelResponse::new(output("fallback"))),
    ]);
    assert_eq!(
        model
            .stream_response(request())
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        events
    );
    assert_eq!(
        model.get_response(request()).await.unwrap().output(),
        output("fallback")
    );
    model.assert_complete().unwrap();
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn dropping_stream_releases_factory_resources_and_unpolled_streams_do_not_consume() {
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let model = ScriptedModel::new([ModelStep::stream_factory(move |call| {
        assert!(call.streamed());
        let guard = DropFlag(Arc::clone(&flag));
        Box::pin(stream::unfold(Some(guard), |state| async {
            Some((
                Ok(ModelStreamEvent::Completed(Box::new(ModelResponse::new(
                    Vec::new(),
                )))),
                state,
            ))
        }))
    })]);
    drop(model.stream_response(request()));
    assert_eq!(model.remaining_steps(), 1);
    assert!(model.calls().is_empty());
    let mut stream = model.stream_response(request());
    stream.next().await.unwrap().unwrap();
    assert!(!dropped.load(Ordering::SeqCst));
    drop(stream);
    assert!(dropped.load(Ordering::SeqCst));
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn automatic_streams_emit_empty_added_items_deltas_completed_items_and_terminal_facts() {
    let reasoning = RunItem::new(
        ItemId::new("reasoning"),
        RunItemKind::Reasoning(
            Reasoning::new()
                .with_encrypted_content("encrypted fixture")
                .with_provider_data(json!({"private_thought": "fixture"}))
                .with_summary(vec!["summary".into()])
                .with_content(vec!["body".into()]),
        ),
    );
    let items = vec![
        reasoning.clone(),
        assistant_message("answer", Some("message")),
        function_call("echo", json!({"value": 7}), "call", None, Some("tools")),
        RunItem::new(
            ItemId::new("handoff"),
            RunItemKind::HandoffCall(
                HandoffCall::new(CallId::new("transfer"), AgentId::new("reviewer"), json!({}))
                    .with_tool_name("transfer_to_reviewer"),
            ),
        ),
        RunItem::new(
            ItemId::new("patch"),
            RunItemKind::ToolCall(
                ToolCall::custom(
                    CallId::new("patch-call"),
                    "apply_patch",
                    "*** Begin Patch\n*** End Patch",
                )
                .with_namespace("tools"),
            ),
        ),
        RunItem::new(
            ItemId::new("search"),
            RunItemKind::ToolCall(
                ToolCall::new(
                    CallId::new("search-call"),
                    "tool_search",
                    json!({"query": "echo"}),
                )
                .with_kind(ToolCallKind::ToolSearch),
            ),
        ),
    ];
    let response = ModelResponse::new(items.clone())
        .with_response_id("stable-response")
        .with_request_id("stable-request");
    let model = ScriptedModel::new([response.clone()]);
    let events = model
        .stream_response(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let names: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::OutputItemAdded(event) | ModelStreamEvent::RunItem(event) => {
                Some(event.name())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        [
            "reasoning",
            "reasoning",
            "message",
            "message",
            "tool_call",
            "handoff_call",
            "tool_call",
            "tool_call",
        ]
    );
    let ModelStreamEvent::OutputItemAdded(added) = &events[0] else {
        panic!("missing reasoning opening")
    };
    let RunItemKind::Reasoning(empty) = added.item().kind() else {
        panic!("wrong item kind")
    };
    assert!(empty.summary().is_empty() && empty.content().is_empty());
    assert!(empty.encrypted_content().is_none() && empty.provider_data().is_none());
    assert!(
        matches!(&events[1], ModelStreamEvent::Delta(delta) if matches!(delta.kind(), ModelStreamDeltaKind::ReasoningSummaryPartAdded { summary_index: 0 }))
    );
    assert!(
        matches!(&events[2], ModelStreamEvent::Delta(delta) if matches!(delta.kind(), ModelStreamDeltaKind::ReasoningSummaryText { delta, summary_index: 0 } if delta == "summary"))
    );
    assert!(
        matches!(&events[3], ModelStreamEvent::Delta(delta) if matches!(delta.kind(), ModelStreamDeltaKind::ReasoningText { delta, content_index: 0 } if delta == "body"))
    );
    let ModelStreamEvent::OutputItemAdded(added) = &events[5] else {
        panic!("missing message opening")
    };
    let RunItemKind::Message(empty) = added.item().kind() else {
        panic!("wrong item kind")
    };
    assert!(empty.content().is_empty());
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::RunItem(event) => Some(event.item().clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, items);
    assert_eq!(
        events.last(),
        Some(&ModelStreamEvent::Completed(Box::new(response)))
    );
    let RunItemKind::ToolCall(call) = items[2].kind() else {
        panic!("missing call")
    };
    assert_eq!(call.namespace(), Some("tools"));
    assert_eq!(items[2].id().as_str(), "call");
}

#[tokio::test]
async fn unsupported_automatic_items_fail_only_on_the_streaming_entry_point() {
    let hosted = RunItem::new(
        ItemId::new("hosted"),
        RunItemKind::McpListTools(McpListTools::new("docs", Vec::new())),
    );
    let items = vec![assistant_message("before unsupported item", None), hosted];
    let model = ScriptedModel::new([items.clone(), items.clone()]);
    assert_eq!(model.get_response(request()).await.unwrap().output(), items);
    let mut stream = model.stream_response(request());
    let error = stream.next().await.unwrap().unwrap_err();
    assert!(stream.next().await.is_none());
    assert!(matches!(
        error,
        Error::Provider {
            kind: ProviderErrorKind::Behavior,
            ..
        }
    ));
    assert!(
        error
            .source()
            .unwrap()
            .to_string()
            .contains("ModelStep::stream")
    );
    assert!(error.to_string().contains("mcp_list_tools"));
    model.assert_complete().unwrap();
}

#[tokio::test]
async fn provider_returns_the_same_model_for_default_and_named_requests() {
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::default());
    let provider = ScriptedProvider::new(Arc::clone(&model));
    for name in [None, Some("fixture"), Some("anything")] {
        assert!(Arc::ptr_eq(&provider.get_model(name).unwrap(), &model));
    }
    provider.close().await.unwrap();
}
