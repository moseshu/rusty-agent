//! Agent instruction roles, per-call evaluation, and session isolation through real adapters.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::Result,
    item::{Message, ModelInputItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
    },
    prompt::{PromptSource, ResolvedPrompt},
    session::Session,
    state::RunId,
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use ra_session::InMemorySession;
use rstest::rstest;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const STATIC_TEXT: &str = "  System instructions\nkeep whitespace.  ";

struct Resolver {
    model: Arc<dyn Model>,
    protocol: ApiProtocol,
}

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test-model".into()),
                self.protocol,
            ),
            self.model.clone(),
            ModelSettings::new().with_max_tokens(1024),
            ModelSettings::new(),
        ))
    }
}

fn dynamic_prompt(context: &RunContext) -> Result<ResolvedPrompt> {
    let calls = context.app_context::<AtomicUsize>().unwrap();
    let turn = calls.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(context.agent().id(), &AgentId::new("worker"));
    Ok(ResolvedPrompt::new(
        format!(
            "  System instructions turn {turn} for {}\n  ",
            context.agent().name()
        ),
        PromptSource::Dynamic("test".into()),
    ))
}

fn reply(protocol: ApiProtocol) -> ResponseTemplate {
    let response = match protocol {
        ApiProtocol::OpenAiResponses => json!({
            "id":"resp_test", "object":"response", "status":"completed",
            "output":[{"type":"message", "id":"msg_test", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"done"}]}],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }),
        ApiProtocol::OpenAiChatCompletions => json!({
            "id":"chat_test", "object":"chat.completion",
            "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"done"}}]
        }),
        ApiProtocol::AnthropicMessages => json!({
            "id":"msg_test", "type":"message", "role":"assistant",
            "content":[{"type":"text","text":"done"}], "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        _ => unreachable!(),
    };
    let frames = match protocol {
        ApiProtocol::OpenAiResponses => {
            vec![json!({"type":"response.completed", "response":response})]
        }
        ApiProtocol::OpenAiChatCompletions => vec![json!({
            "id":"chat_test", "object":"chat.completion.chunk",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"done"},"finish_reason":"stop"}]
        })],
        ApiProtocol::AnthropicMessages => vec![
            json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ],
        _ => unreachable!(),
    };
    let mut body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    if protocol == ApiProtocol::OpenAiChatCompletions {
        body.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// Mounts `calls` scripted replies for `protocol` and returns a resolver for its real adapter.
async fn resolver(protocol: ApiProtocol, server: &MockServer, calls: u64) -> Arc<Resolver> {
    let endpoint = match protocol {
        ApiProtocol::OpenAiResponses => "/v1/responses",
        ApiProtocol::OpenAiChatCompletions => "/v1/chat/completions",
        ApiProtocol::AnthropicMessages => "/v1/messages",
        _ => unreachable!(),
    };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(reply(protocol))
        .expect(calls)
        .mount(server)
        .await;
    let auth = OpenAiAuth::new("test-key").with_base_url(format!("{}/v1/", server.uri()));
    let model: Arc<dyn Model> = match protocol {
        ApiProtocol::OpenAiResponses => {
            Arc::new(OpenAiResponsesModel::new("test-model", auth).unwrap())
        }
        ApiProtocol::OpenAiChatCompletions => {
            Arc::new(OpenAiChatModel::new("test-model", auth).unwrap())
        }
        ApiProtocol::AnthropicMessages => Arc::new(
            AnthropicMessagesModel::new(
                "test-model",
                AnthropicAuth::new("test-key").with_base_url(format!("{}/v1/", server.uri())),
            )
            .unwrap(),
        ),
        _ => unreachable!(),
    };
    Arc::new(Resolver { model, protocol })
}

#[rstest]
#[case(ApiProtocol::OpenAiResponses)]
#[case(ApiProtocol::OpenAiChatCompletions)]
#[case(ApiProtocol::AnthropicMessages)]
#[tokio::test]
async fn instructions_use_the_adapter_system_position_and_never_enter_history(
    #[case] protocol: ApiProtocol,
    #[values("static", "sync", "async")] callback: &str,
    #[values(false, true)] streamed: bool,
) {
    let server = MockServer::start().await;
    let resolver = resolver(protocol, &server, 2).await;
    let builder = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker");
    let agent = match callback {
        "static" => builder.instructions(STATIC_TEXT),
        "sync" => builder.dynamic_instructions_sync_fn(dynamic_prompt),
        "async" => builder.dynamic_instructions_async_fn(|context| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                dynamic_prompt(context)
            })
        }),
        _ => unreachable!(),
    }
    .build()
    .unwrap();
    let session = Arc::new(InMemorySession::new("instruction-history"));
    let calls = Arc::new(AtomicUsize::new(0));
    for input in ["first", "second"] {
        let request = RunRequest::new(
            AgentBinding::direct(agent.clone()),
            resolver.clone(),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(input))],
        )
        .with_app_context(calls.clone())
        .with_session(session.clone());
        let result = if streamed {
            Runner::run_streamed(request).finish().await
        } else {
            Runner::run(request).await
        }
        .unwrap();
        assert_eq!(result.final_text(), "done");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        if callback == "static" { 0 } else { 2 }
    );
    let requests = server.received_requests().await.unwrap();
    for (index, request) in requests.iter().enumerate() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let expected = if callback == "static" {
            STATIC_TEXT.to_owned()
        } else {
            format!("  System instructions turn {} for Worker\n  ", index + 1)
        };
        let input = match protocol {
            ApiProtocol::OpenAiResponses => {
                assert_eq!(body["instructions"], expected);
                body["input"].as_array().unwrap().clone()
            }
            ApiProtocol::OpenAiChatCompletions => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages[0]["content"], expected);
                messages[1..].to_vec()
            }
            ApiProtocol::AnthropicMessages => {
                assert_eq!(body["system"][0]["text"], expected);
                body["messages"].as_array().unwrap().clone()
            }
            _ => unreachable!(),
        };
        let roles: Vec<_> = input
            .iter()
            .map(|item| item["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            if index == 0 {
                vec!["user"]
            } else {
                vec!["user", "assistant", "user"]
            }
        );
        assert!(
            !serde_json::to_string(&input)
                .unwrap()
                .contains("System instructions")
        );
        assert_eq!(
            input[0]["content"][0]["text"]
                .as_str()
                .or_else(|| input[0]["content"].as_str()),
            Some("first")
        );
        if index == 1 {
            assert_eq!(
                input[2]["content"][0]["text"]
                    .as_str()
                    .or_else(|| input[2]["content"].as_str()),
                Some("second")
            );
        }
    }
    let history = session.get_items(None).await.unwrap();
    let texts: Vec<_> = history
        .iter()
        .map(|item| match item.kind() {
            RunItemKind::Message(message) => message.text_content(),
            other => panic!("unexpected session item: {other:?}"),
        })
        .collect();
    assert_eq!(texts, ["first", "done", "second", "done"]);
}

/// Empty generated instructions follow each upstream wire contract instead of sending an empty
/// system message: Responses forwards the empty string as Python does, Chat Completions omits a
/// falsy value, and Anthropic Messages omits the block its API would reject as empty.
#[rstest]
#[case(ApiProtocol::OpenAiResponses)]
#[case(ApiProtocol::OpenAiChatCompletions)]
#[case(ApiProtocol::AnthropicMessages)]
#[tokio::test]
async fn empty_generated_instructions_send_no_empty_system_message(#[case] protocol: ApiProtocol) {
    let server = MockServer::start().await;
    let resolver = resolver(protocol, &server, 1).await;
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .dynamic_instructions_sync_fn(|_| Ok(ResolvedPrompt::new("", PromptSource::Agent)))
        .build()
        .unwrap();
    let request = RunRequest::new(
        AgentBinding::direct(agent),
        resolver,
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("hello"))],
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    match protocol {
        ApiProtocol::OpenAiResponses => assert_eq!(body["instructions"], ""),
        ApiProtocol::OpenAiChatCompletions => {
            let roles: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect();
            assert_eq!(roles, ["user"]);
        }
        ApiProtocol::AnthropicMessages => assert!(body.get("system").is_none(), "{body}"),
        _ => unreachable!(),
    }
}
