//! Provider payload compatibility smoke tests.
//!
//! The matrix exercises the public adapters at their HTTP boundary. Anthropic has no production
//! adapter yet, so its hidden preview is intentionally limited to the common request subset that
//! this test owns.

use ra_core::{
    item::{AgentId, CallId, Message, ModelInputItem, OutputPhase, ToolCall, ToolCallOutput},
    model::{
        Effort, Model, ModelHandoffDefinition, ModelProvider, ModelRequest, ModelSettings,
        ModelToolDefinition, ProviderKey, ThinkingConfig, ToolChoice,
    },
    tool::ToolOutput,
};
use ra_model::{
    anthropic::smoke::preview_request_payload,
    compat::CompatEndpoint,
    openai::{
        auth::OpenAiAuth,
        chat::OpenAiChatModel,
        responses::OpenAiResponsesModel,
    },
    provider::quirks::ProviderQuirks,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "matrix-model";

fn resolved(settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("compat-matrix"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn matrix_request() -> ModelRequest {
    let settings = ModelSettings::new()
        .with_max_tokens(256)
        .with_effort(Effort::High)
        .with_tool_choice(ToolChoice::Tool("lookup".to_owned()))
        .with_parallel_tool_calls(false);
    ModelRequest::new(
        vec![
            ModelInputItem::Message(Message::user("look up account A-42")),
            ModelInputItem::ToolCall(ToolCall::new(
                CallId::new("call_lookup"),
                "lookup",
                json!({"account_id": "A-42"}),
            )),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call_lookup"),
                json!({"status": "found"}),
            )),
        ],
        resolved(settings),
    )
    .with_system_instructions("You are the account assistant.")
    .with_tools(vec![
        ModelToolDefinition::new(
            "lookup",
            json!({
                "type": "object",
                "properties": {"account_id": {"type": "string"}},
                "required": ["account_id"]
            }),
        )
        .with_description("Look up an account")
        .with_strict(true),
    ])
    .with_handoffs(vec![
        ModelHandoffDefinition::new(
            AgentId::new("research-agent"),
            "delegate_research",
            json!({"type": "object"}),
        )
        .with_description("Delegate account research"),
    ])
}

fn responses_success() -> Value {
    json!({
        "id": "resp_smoke",
        "object": "response",
        "status": "completed",
        "output": []
    })
}

fn chat_success() -> Value {
    json!({
        "id": "chat_smoke",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "ok"}
        }]
    })
}

async fn received_body(server: &MockServer) -> Value {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    assert_eq!(requests.len(), 1, "expected one request");
    serde_json::from_slice(&requests[0].body).expect("request should be JSON")
}

async fn capture_responses(request: ModelRequest) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(responses_success()))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("responses smoke model should build");
    model
        .get_response(request)
        .await
        .expect("responses smoke response should convert");
    received_body(&server).await
}

async fn capture_chat(request: ModelRequest) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_success()))
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("chat smoke model should build")
    .with_quirks(ProviderQuirks::new().with_parallel_tool_calls(true));
    model
        .get_response(request)
        .await
        .expect("chat smoke response should convert");
    received_body(&server).await
}

async fn capture_compat(request: ModelRequest) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_success()))
        .mount(&server)
        .await;
    let provider = CompatEndpoint::new(server.uri(), MODEL)
        .with_quirks(ProviderQuirks::new().with_parallel_tool_calls(true))
        .build_provider()
        .expect("compat smoke provider should build");
    provider
        .get_model(None)
        .expect("compat smoke model should resolve")
        .get_response(request)
        .await
        .expect("compat smoke response should convert");
    received_body(&server).await
}

/// Lowers through the real Responses adapter and returns the refusal it must produce.
///
/// The endpoint is mounted so a regression that lowers the setting instead of refusing it shows up
/// as a request on the wire rather than as a passing test.
async fn responses_refusal(request: ModelRequest) -> String {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(responses_success()))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("responses smoke model should build");
    let error = model
        .get_response(request)
        .await
        .expect_err("the Responses adapter must refuse the setting");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("wiremock should retain requests")
            .len(),
        0,
        "a refused setting must not reach the endpoint"
    );
    error.to_string()
}

/// The Chat Completions counterpart of [`responses_refusal`], under the default strict policy.
async fn chat_refusal(request: ModelRequest) -> String {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_success()))
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("chat smoke model should build");
    let error = model
        .get_response(request)
        .await
        .expect_err("the Chat Completions adapter must refuse the setting");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("wiremock should retain requests")
            .len(),
        0,
        "a refused setting must not reach the endpoint"
    );
    error.to_string()
}

fn history_request(input: Vec<ModelInputItem>) -> ModelRequest {
    ModelRequest::new(input, resolved(ModelSettings::new().with_max_tokens(256)))
}

#[tokio::test]
async fn one_neutral_request_has_stable_payloads_for_all_supported_protocols() {
    let request = matrix_request();
    let matrix = json!({
        "openai_responses": capture_responses(request.clone()).await,
        "openai_chat": capture_chat(request.clone()).await,
        "anthropic_messages": preview_request_payload(MODEL, &request)
            .expect("Anthropic matrix request should lower"),
        "openai_compatible": capture_compat(request).await,
    });

    insta::assert_json_snapshot!(matrix, @r###"
{
  "anthropic_messages": {
    "max_tokens": 256,
    "messages": [
      {
        "content": [
          {
            "text": "look up account A-42",
            "type": "text"
          }
        ],
        "role": "user"
      },
      {
        "content": [
          {
            "id": "call_lookup",
            "input": {
              "account_id": "A-42"
            },
            "name": "lookup",
            "type": "tool_use"
          }
        ],
        "role": "assistant"
      },
      {
        "content": [
          {
            "content": "{\"status\":\"found\"}",
            "tool_use_id": "call_lookup",
            "type": "tool_result"
          }
        ],
        "role": "user"
      }
    ],
    "model": "matrix-model",
    "output_config": {
      "effort": "high"
    },
    "system": [
      {
        "text": "You are the account assistant.",
        "type": "text"
      }
    ],
    "tool_choice": {
      "disable_parallel_tool_use": true,
      "name": "lookup",
      "type": "tool"
    },
    "tools": [
      {
        "description": "Look up an account",
        "input_schema": {
          "properties": {
            "account_id": {
              "type": "string"
            }
          },
          "required": [
            "account_id"
          ],
          "type": "object"
        },
        "name": "lookup"
      },
      {
        "description": "Delegate account research",
        "input_schema": {
          "type": "object"
        },
        "name": "delegate_research"
      }
    ]
  },
  "openai_chat": {
    "max_tokens": 256,
    "messages": [
      {
        "content": "You are the account assistant.",
        "role": "system"
      },
      {
        "content": "look up account A-42",
        "role": "user"
      },
      {
        "content": null,
        "role": "assistant",
        "tool_calls": [
          {
            "function": {
              "arguments": "{\"account_id\":\"A-42\"}",
              "name": "lookup"
            },
            "id": "call_lookup",
            "type": "function"
          }
        ]
      },
      {
        "content": "{\"status\":\"found\"}",
        "role": "tool",
        "tool_call_id": "call_lookup"
      }
    ],
    "model": "matrix-model",
    "parallel_tool_calls": false,
    "reasoning_effort": "high",
    "tool_choice": {
      "function": {
        "name": "lookup"
      },
      "type": "function"
    },
    "tools": [
      {
        "function": {
          "description": "Look up an account",
          "name": "lookup",
          "parameters": {
            "properties": {
              "account_id": {
                "type": "string"
              }
            },
            "required": [
              "account_id"
            ],
            "type": "object"
          },
          "strict": true
        },
        "type": "function"
      },
      {
        "function": {
          "description": "Delegate account research",
          "name": "delegate_research",
          "parameters": {
            "type": "object"
          },
          "strict": false
        },
        "type": "function"
      }
    ]
  },
  "openai_compatible": {
    "max_tokens": 256,
    "messages": [
      {
        "content": "You are the account assistant.",
        "role": "system"
      },
      {
        "content": "look up account A-42",
        "role": "user"
      },
      {
        "content": null,
        "role": "assistant",
        "tool_calls": [
          {
            "function": {
              "arguments": "{\"account_id\":\"A-42\"}",
              "name": "lookup"
            },
            "id": "call_lookup",
            "type": "function"
          }
        ]
      },
      {
        "content": "{\"status\":\"found\"}",
        "role": "tool",
        "tool_call_id": "call_lookup"
      }
    ],
    "model": "matrix-model",
    "parallel_tool_calls": false,
    "reasoning_effort": "high",
    "tool_choice": {
      "function": {
        "name": "lookup"
      },
      "type": "function"
    },
    "tools": [
      {
        "function": {
          "description": "Look up an account",
          "name": "lookup",
          "parameters": {
            "properties": {
              "account_id": {
                "type": "string"
              }
            },
            "required": [
              "account_id"
            ],
            "type": "object"
          },
          "strict": true
        },
        "type": "function"
      },
      {
        "function": {
          "description": "Delegate account research",
          "name": "delegate_research",
          "parameters": {
            "type": "object"
          },
          "strict": false
        },
        "type": "function"
      }
    ]
  },
  "openai_responses": {
    "include": [
      "reasoning.encrypted_content"
    ],
    "input": [
      {
        "content": [
          {
            "text": "look up account A-42",
            "type": "input_text"
          }
        ],
        "role": "user",
        "type": "message"
      },
      {
        "arguments": "{\"account_id\":\"A-42\"}",
        "call_id": "call_lookup",
        "name": "lookup",
        "type": "function_call"
      },
      {
        "call_id": "call_lookup",
        "output": "{\"status\":\"found\"}",
        "type": "function_call_output"
      }
    ],
    "instructions": "You are the account assistant.",
    "max_output_tokens": 256,
    "model": "matrix-model",
    "parallel_tool_calls": false,
    "reasoning": {
      "effort": "high"
    },
    "store": false,
    "tool_choice": {
      "name": "lookup",
      "type": "function"
    },
    "tools": [
      {
        "description": "Look up an account",
        "name": "lookup",
        "parameters": {
          "properties": {
            "account_id": {
              "type": "string"
            }
          },
          "required": [
            "account_id"
          ],
          "type": "object"
        },
        "strict": true,
        "type": "function"
      },
      {
        "description": "Delegate account research",
        "name": "delegate_research",
        "parameters": {
          "type": "object"
        },
        "strict": false,
        "type": "function"
      }
    ]
  }
}
"###);
}

#[test]
fn anthropic_preview_groups_tool_turns_and_projects_structured_output() {
    let structured = serde_json::to_value(ToolOutput::text("visible result"))
        .expect("structured tool output should serialize");
    let request = ModelRequest::new(
        vec![
            ModelInputItem::Message(Message::assistant("checking", OutputPhase::Commentary)),
            ModelInputItem::ToolCall(ToolCall::new(
                CallId::new("call_a"),
                "lookup",
                json!({"id": "A"}),
            )),
            ModelInputItem::ToolCall(ToolCall::new(
                CallId::new("call_b"),
                "lookup",
                json!({"id": "B"}),
            )),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(CallId::new("call_a"), structured)),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call_b"),
                json!("plain result"),
            )),
        ],
        resolved(
            ModelSettings::new()
                .with_max_tokens(256)
                .with_parallel_tool_calls(false),
        ),
    );

    let payload = preview_request_payload(MODEL, &request)
        .expect("Anthropic preview should lower grouped tool turns");

    assert_eq!(payload["messages"].as_array().map(Vec::len), Some(2));
    assert_eq!(payload["messages"][0]["role"], "assistant");
    assert_eq!(payload["messages"][0]["content"][0]["text"], "checking");
    assert_eq!(payload["messages"][0]["content"][1]["id"], "call_a");
    assert_eq!(payload["messages"][0]["content"][2]["id"], "call_b");
    assert_eq!(payload["messages"][1]["role"], "user");
    assert_eq!(
        payload["messages"][1]["content"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(
        payload["messages"][1]["content"][0]["content"][0]["text"],
        "visible result"
    );
    assert_eq!(
        payload["messages"][1]["content"][1]["content"],
        "plain result"
    );
    assert_eq!(
        payload["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
    assert!(payload.get("disable_parallel_tool_use").is_none());
}

#[test]
fn anthropic_preview_refuses_an_unrepresentable_no_tools_choice() {
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved(
            ModelSettings::new()
                .with_max_tokens(256)
                .with_tool_choice(ToolChoice::None),
        ),
    )
    .with_tools(vec![ModelToolDefinition::new(
        "lookup",
        json!({"type": "object"}),
    )]);

    let error = preview_request_payload(MODEL, &request)
        .expect_err("the preview must not silently discard ToolChoice::None");
    assert!(error.to_string().contains("ToolChoice::None"));
}

/// Smallest budget Anthropic accepts, and the smallest output ceiling that leaves room for it.
const THINKING_BUDGET: u64 = 2048;
const THINKING_MAX_TOKENS: u64 = 4096;

fn thinking_settings(thinking: ThinkingConfig) -> ModelSettings {
    ModelSettings::new()
        .with_max_tokens(THINKING_MAX_TOKENS)
        .with_thinking(thinking)
}

fn every_thinking_configuration() -> [ThinkingConfig; 3] {
    [
        ThinkingConfig::Adaptive,
        ThinkingConfig::Enabled {
            budget_tokens: THINKING_BUDGET,
        },
        ThinkingConfig::Disabled,
    ]
}

fn hello(settings: ModelSettings) -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved(settings),
    )
}

#[test]
fn anthropic_preview_lowers_every_thinking_configuration() {
    let payload = |thinking| {
        preview_request_payload(MODEL, &hello(thinking_settings(thinking)))
            .expect("Anthropic thinking configuration should lower")
    };

    assert_eq!(
        payload(ThinkingConfig::Adaptive)["thinking"],
        json!({"type": "adaptive"})
    );
    assert_eq!(
        payload(ThinkingConfig::Enabled {
            budget_tokens: THINKING_BUDGET
        })["thinking"],
        json!({"type": "enabled", "budget_tokens": THINKING_BUDGET})
    );
    assert_eq!(
        payload(ThinkingConfig::Disabled)["thinking"],
        json!({"type": "disabled"})
    );
}

#[test]
fn anthropic_preview_lowers_every_effort_level() {
    for (effort, label) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let request = hello(
            ModelSettings::new()
                .with_max_tokens(256)
                .with_effort(effort),
        );

        let payload = preview_request_payload(MODEL, &request)
            .expect("Anthropic effort configuration should lower");
        assert_eq!(payload["output_config"], json!({"effort": label}));
    }
}

/// Thinking travels with the rest of a request rather than only on its own.
#[test]
fn anthropic_preview_keeps_thinking_beside_tools_and_effort() {
    let request = hello(
        thinking_settings(ThinkingConfig::Adaptive)
            .with_effort(Effort::High)
            .with_tool_choice(ToolChoice::Tool("lookup".to_owned())),
    )
    .with_tools(vec![ModelToolDefinition::new(
        "lookup",
        json!({"type": "object"}),
    )]);

    let payload = preview_request_payload(MODEL, &request)
        .expect("thinking should lower alongside tools and effort");
    assert_eq!(payload["thinking"], json!({"type": "adaptive"}));
    assert_eq!(payload["output_config"], json!({"effort": "high"}));
    assert_eq!(
        payload["tool_choice"],
        json!({"type": "tool", "name": "lookup"})
    );
    assert_eq!(payload["tools"][0]["name"], "lookup");
    assert_eq!(payload["max_tokens"], json!(THINKING_MAX_TOKENS));
}

#[test]
fn anthropic_preview_rejects_a_thinking_budget_that_exhausts_max_tokens() {
    for max_tokens in [THINKING_BUDGET - 1, THINKING_BUDGET] {
        let request = hello(
            thinking_settings(ThinkingConfig::Enabled {
                budget_tokens: THINKING_BUDGET,
            })
            .with_max_tokens(max_tokens),
        );

        let error = preview_request_payload(MODEL, &request)
            .expect_err("Anthropic must reject max_tokens at or below the thinking budget");
        assert!(
            error
                .to_string()
                .contains(&format!("max_tokens ({max_tokens})"))
        );
        assert!(
            error
                .to_string()
                .contains(&format!("thinking.budget_tokens ({THINKING_BUDGET})"))
        );
    }
}

/// The floor is Anthropic's own: a budget under it is a 400 however much room `max_tokens` leaves.
#[test]
fn anthropic_preview_rejects_a_thinking_budget_below_the_floor() {
    let request = hello(thinking_settings(ThinkingConfig::Enabled {
        budget_tokens: 1023,
    }));

    let error = preview_request_payload(MODEL, &request)
        .expect_err("Anthropic must reject a thinking budget below its minimum");
    assert!(error.to_string().contains("at least 1024"));

    let accepted = hello(thinking_settings(ThinkingConfig::Enabled {
        budget_tokens: 1024,
    }));
    assert_eq!(
        preview_request_payload(MODEL, &accepted).expect("the floor itself should lower")["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );
}

/// Turning thinking off is only representable below the top two effort levels.
#[test]
fn anthropic_preview_rejects_disabled_thinking_at_the_highest_efforts() {
    for effort in [Effort::XHigh, Effort::Max] {
        let request = hello(thinking_settings(ThinkingConfig::Disabled).with_effort(effort));

        let error = preview_request_payload(MODEL, &request)
            .expect_err("Anthropic must reject disabled thinking at the top effort levels");
        assert!(error.to_string().contains("thinking.type=disabled"));
        assert!(
            error
                .to_string()
                .contains(&format!("output_config.effort={effort}"))
        );
    }

    let request = hello(thinking_settings(ThinkingConfig::Disabled).with_effort(Effort::High));
    assert_eq!(
        preview_request_payload(MODEL, &request).expect("high effort still accepts disabled")["thinking"],
        json!({"type": "disabled"})
    );
}

/// Neither `OpenAI` protocol has a thinking switch, so no shape may be accepted and then dropped.
#[tokio::test]
async fn openai_protocols_refuse_every_thinking_configuration() {
    for thinking in every_thinking_configuration() {
        let error = responses_refusal(hello(thinking_settings(thinking))).await;
        assert!(error.contains("cannot carry a ThinkingConfig"), "{error}");

        let error = chat_refusal(hello(thinking_settings(thinking))).await;
        assert!(error.contains("cannot carry a ThinkingConfig"), "{error}");
    }
}

/// Provider adapters preserve every neutral effort level; model-specific support belongs to a
/// future capability axis rather than a protocol-wide deny-list.
#[tokio::test]
async fn openai_protocols_lower_every_effort_level() {
    for (effort, label) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let settings = || {
            ModelSettings::new()
                .with_max_tokens(256)
                .with_effort(effort)
        };

        let body = capture_responses(hello(settings())).await;
        assert_eq!(body["reasoning"]["effort"], label);

        let body = capture_chat(hello(settings())).await;
        assert_eq!(body["reasoning_effort"], label);
    }
}

#[tokio::test]
async fn responses_and_chat_preserve_reasoning_and_tool_pairs_across_protocol_boundaries() {
    let responses_source = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_history",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "id": "rs_history",
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "need lookup"}]
                },
                {
                    "id": "fc_history",
                    "type": "function_call",
                    "call_id": "call_from_responses",
                    "name": "lookup",
                    "arguments": "{\"account_id\":\"A-42\"}"
                }
            ]
        })))
        .mount(&responses_source)
        .await;
    let responses_model = OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", responses_source.uri())),
    )
    .expect("Responses source model should build");
    let responses_history = responses_model
        .get_response(history_request(vec![ModelInputItem::Message(
            Message::user("start"),
        )]))
        .await
        .expect("Responses source response should convert")
        .to_input_items();

    let chat_target = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_success()))
        .mount(&chat_target)
        .await;
    let chat_model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", chat_target.uri())),
    )
    .expect("Chat target model should build")
    .with_quirks(ProviderQuirks::new().with_reasoning_content(true));
    let mut responses_to_chat = responses_history;
    responses_to_chat.push(ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_from_responses"),
        json!({"status": "found"}),
    )));
    chat_model
        .get_response(history_request(responses_to_chat))
        .await
        .expect("Chat should lower Responses history");
    let chat_body = received_body(&chat_target).await;
    assert_eq!(chat_body["messages"][0]["reasoning_content"], "need lookup");
    assert_eq!(
        chat_body["messages"][0]["tool_calls"][0]["id"],
        "call_from_responses"
    );
    assert_eq!(chat_body["messages"][1]["role"], "tool");
    assert_eq!(
        chat_body["messages"][1]["tool_call_id"],
        "call_from_responses"
    );

    let chat_source = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat_history",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "checking",
                    "reasoning_content": "need lookup",
                    "tool_calls": [{
                        "id": "call_from_chat",
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "arguments": "{\"account_id\":\"A-42\"}"
                        }
                    }]
                }
            }]
        })))
        .mount(&chat_source)
        .await;
    let chat_source_model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", chat_source.uri())),
    )
    .expect("Chat source model should build")
    .with_quirks(ProviderQuirks::new().with_reasoning_content(true));
    let chat_history = chat_source_model
        .get_response(history_request(vec![ModelInputItem::Message(
            Message::user("start"),
        )]))
        .await
        .expect("Chat source response should convert")
        .to_input_items();

    let responses_target = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(responses_success()))
        .mount(&responses_target)
        .await;
    let responses_target_model = OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", responses_target.uri())),
    )
    .expect("Responses target model should build");
    let mut chat_to_responses = chat_history;
    chat_to_responses.push(ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_from_chat"),
        json!({"status": "found"}),
    )));
    responses_target_model
        .get_response(history_request(chat_to_responses))
        .await
        .expect("Responses should lower Chat history");
    let responses_body = received_body(&responses_target).await;
    assert_eq!(responses_body["input"][0]["type"], "reasoning");
    assert_eq!(
        responses_body["input"][0]["summary"][0]["text"],
        "need lookup"
    );
    assert_eq!(responses_body["input"][2]["type"], "function_call");
    assert_eq!(responses_body["input"][2]["call_id"], "call_from_chat");
    assert_eq!(responses_body["input"][3]["type"], "function_call_output");
    assert_eq!(responses_body["input"][3]["call_id"], "call_from_chat");
}
