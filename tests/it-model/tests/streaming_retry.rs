//! Real adapter failures must respect the runtime's public and thread consumption boundaries.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures::StreamExt;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::Result,
    item::{ContentBlock, Message, MessageRole, ModelInputItem},
    model::{
        ApiProtocol, ConversationContinuation, Model, ModelRequest, ModelResolver,
        ModelRetryAdviceRequest, ModelRetrySettings, ModelSelector, ModelSettings,
        NetworkErrorRetryPolicy, ProviderKey, ReplaySafety, ResolvedModel, RetryBackoffSettings,
        SettingsLayers,
    },
    session::SessionId,
    state::RunId,
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunRequest, RunStreamEvent, Runner},
    thread::{AgentThread, ThreadEvent, TurnInputRequest},
};
use rstest::rstest;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Clone, Copy, Debug)]
enum Protocol {
    Responses,
    Chat,
    Anthropic,
}

impl Protocol {
    fn api_protocol(self) -> ApiProtocol {
        match self {
            Self::Responses => ApiProtocol::OpenAiResponses,
            Self::Chat => ApiProtocol::OpenAiChatCompletions,
            Self::Anthropic => ApiProtocol::AnthropicMessages,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Responses => "/v1/responses",
            Self::Chat => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }

    fn body(self, attempt: usize) -> String {
        let id = format!("answer-{attempt}");
        let mut frames = match self {
            Self::Responses => vec![
                json!({"type":"response.created","response":{"id":format!("resp-{attempt}")}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":id,"type":"message","role":"assistant","content":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"item_id":id,"content_index":0,"delta":"Hel"}),
            ],
            Self::Chat => vec![
                json!({"id":id,"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}),
            ],
            Self::Anthropic => vec![
                json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            ],
        };
        if attempt > 0 {
            frames.extend(match self {
                Self::Responses => vec![json!({"type":"response.completed","response":{"id":format!("resp-{attempt}"),"status":"completed","output":[{"id":id,"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}]}})],
                Self::Chat => vec![json!({"id":id,"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]})],
                Self::Anthropic => vec![
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                    json!({"type":"message_stop"}),
                ],
            });
        }
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect()
    }

    fn model(self, endpoint: &MockServer) -> Arc<dyn Model> {
        let base = format!("{}/v1/", endpoint.uri());
        match self {
            Self::Responses => Arc::new(
                OpenAiResponsesModel::new(
                    "mock-model",
                    OpenAiAuth::new("test-key").with_base_url(base),
                )
                .unwrap(),
            ),
            Self::Chat => Arc::new(
                OpenAiChatModel::new(
                    "mock-model",
                    OpenAiAuth::new("test-key").with_base_url(base),
                )
                .unwrap(),
            ),
            Self::Anthropic => Arc::new(
                AnthropicMessagesModel::new(
                    "mock-model",
                    AnthropicAuth::new("test-key").with_base_url(base),
                )
                .unwrap(),
            ),
        }
    }
}

struct Resolver(Arc<dyn Model>, Protocol);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("mock"),
                Some("mock-model".into()),
                self.1.api_protocol(),
            ),
            Arc::clone(&self.0),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn agent() -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("retry-agent"))
            .name("Retry agent")
            .instructions("answer")
            .build()
            .unwrap(),
    )
}

fn config(partial_messages: bool) -> RunConfig {
    RunConfig::new()
        .with_partial_messages(partial_messages)
        .with_model_settings(
            ModelSettings::new().with_max_tokens(128).with_retry(
                ModelRetrySettings::new()
                    .with_max_retries(1)
                    .with_backoff(
                        RetryBackoffSettings::new()
                            .with_initial_delay(Duration::ZERO)
                            .with_jitter(false),
                    )
                    .with_policy(Arc::new(NetworkErrorRetryPolicy)),
            ),
        )
}

async fn endpoint(protocol: Protocol) -> (MockServer, Arc<AtomicUsize>) {
    let endpoint = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path(protocol.path()))
        .respond_with(move |_: &wiremock::Request| {
            let attempt = requests.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(protocol.body(attempt), "text/event-stream")
        })
        .mount(&endpoint)
        .await;
    (endpoint, calls)
}

/// Codex retries interrupted narration and announces the retry before starting the next request.
#[rstest]
#[case::responses(Protocol::Responses)]
#[case::chat(Protocol::Chat)]
#[case::anthropic(Protocol::Anthropic)]
#[tokio::test]
async fn a_thread_retries_real_adapter_narration(
    #[case] protocol: Protocol,
    #[values(false, true)] partial_messages: bool,
) {
    let (endpoint, calls) = endpoint(protocol).await;
    let thread = AgentThread::builder(
        SessionId::new("retry-thread"),
        agent(),
        Arc::new(Resolver(protocol.model(&endpoint), protocol)),
    )
    .with_config(config(partial_messages))
    .build();
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text("hello")]))
        .await;
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let event = thread.next_event().await.unwrap();
            let finished = matches!(event, ThreadEvent::TurnComplete { .. });
            events.push(event);
            if finished {
                break;
            }
        }
        events
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error { .. })),
        "{events:?}"
    );
    let retry = events
        .iter()
        .position(|event| matches!(event, ThreadEvent::StreamError { .. }))
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ThreadEvent::StreamError { .. }))
            .count(),
        1
    );
    assert!(
        events[..retry]
            .iter()
            .any(|event| matches!(event, ThreadEvent::ItemStarted { .. }))
    );
    assert!(
        events[..retry]
            .iter()
            .any(|event| matches!(event, ThreadEvent::Delta { .. }))
    );
    assert!(
        events[retry + 1..]
            .iter()
            .any(|event| matches!(event, ThreadEvent::Delta { .. }))
    );
    assert!(
        matches!(&events[retry], ThreadEvent::StreamError { message, additional_details, .. }
        if message == "Reconnecting... 1/1" && additional_details.contains("stream"))
    );
    assert!(
        matches!(events.last().unwrap(), ThreadEvent::TurnComplete { last_agent_message: Some(text), .. } if text == "Hello")
    );
    let answers: Vec<_> = thread
        .history()
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) if message.role() == MessageRole::Assistant => {
                Some(message.text_content())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        answers,
        ["Hello"],
        "the failed attempt must not become history"
    );
    thread.shutdown().await;
}

/// The framework consumer owns the replay veto, regardless of which adapter supplied the events.
#[rstest]
#[case::responses(Protocol::Responses)]
#[case::chat(Protocol::Chat)]
#[case::anthropic(Protocol::Anthropic)]
#[tokio::test]
async fn public_partial_output_still_vetoes_adapter_retries(
    #[case] protocol: Protocol,
    #[values(false, true)] partial_messages: bool,
) {
    let (endpoint, calls) = endpoint(protocol).await;
    let request = RunRequest::new(
        agent(),
        Arc::new(Resolver(protocol.model(&endpoint), protocol)),
        RunId::new("retry-run"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("hello"))],
    )
    .with_config(config(partial_messages));
    let mut stream = Runner::run_streamed(request);
    let mut deltas = 0;
    while let Some(event) = stream.next_event().await {
        if matches!(event, RunStreamEvent::Delta(_)) {
            deltas += 1;
        }
    }
    let result = stream.finish().await;
    if partial_messages {
        assert_eq!(
            result.expect_err("published output prevents replay").code(),
            "provider.network"
        );
        assert_eq!(deltas, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    } else {
        result.expect("suppressed narration permits a stateless retry");
        assert_eq!(deltas, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

/// Receiving partial output does not prove that a server-managed continuation is replay safe.
#[rstest]
#[case::previous_response(ConversationContinuation::PreviousResponseId("resp_previous".into()))]
#[case::conversation(ConversationContinuation::ConversationId("conv_existing".into()))]
#[tokio::test]
async fn interrupted_stateful_responses_keep_replay_safety_unknown(
    #[case] continuation: ConversationContinuation,
) {
    let (endpoint, calls) = endpoint(Protocol::Responses).await;
    let model = Protocol::Responses.model(&endpoint);
    let settings = SettingsLayers::new().resolve(&ProviderKey::new("openai"));
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        settings,
    )
    .with_continuation(continuation.clone());
    let error = model
        .stream_response(request)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("the stream is truncated");
    let advice = model
        .get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            0,
            true,
            &continuation,
        ))
        .expect("the adapter supplies retry evidence");
    assert_eq!(advice.replay_safety(), ReplaySafety::Unknown);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
