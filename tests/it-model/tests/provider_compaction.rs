//! Server-side compaction across the three adapters.
//!
//! OpenAI Responses carries it natively: `context_management` goes out through the provider's own
//! `extra_body` bucket, a `compaction` output item comes back as a provider compaction without its
//! `created_by` field — the reference drops that one before storing the item — and the item is
//! replayed verbatim. Chat Completions and Anthropic Messages refuse the item, as the reference's
//! Chat converter does: nothing in their requests can carry it back.

use futures::StreamExt as _;
use ra_core::{
    item::{Message, ModelInputItem, ProviderCompaction, RunItemKind},
    model::{Model, ModelRequest, ModelSettings, ModelStreamEvent, ProviderKey},
};
use ra_model::anthropic::{AnthropicAuth, AnthropicMessagesModel};
use ra_model::openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn settings(provider: &str, settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new(provider),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn compaction_output() -> Value {
    json!({
        "id": "cmp_1",
        "type": "compaction",
        "encrypted_content": "opaque-state",
        "created_by": "server",
    })
}

fn stored_compaction() -> ProviderCompaction {
    ProviderCompaction::new(
        "openai",
        json!({"id": "cmp_1", "type": "compaction", "encrypted_content": "opaque-state"}),
    )
}

fn answer() -> Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": "done", "annotations": []}],
    })
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.expect("requests retained");
    serde_json::from_slice(&requests[0].body).expect("JSON body")
}

fn responses_model(server: &MockServer) -> OpenAiResponsesModel {
    OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model")
}

async fn mount(server: &MockServer, route: &str, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(template)
        .mount(server)
        .await;
}

// ---- OpenAI Responses ----------------------------------------------------------------------

/// The reference's `turn_resolution` compaction branch: the item is kept, `created_by` dropped.
#[tokio::test]
async fn responses_keeps_a_compaction_item_without_its_creator() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/responses",
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1",
            "status": "completed",
            "output": [compaction_output(), answer()],
        })),
    )
    .await;

    let response = responses_model(&server)
        .get_response(ModelRequest::new(
            vec![ModelInputItem::Message(Message::user("hi"))],
            settings("openai", ModelSettings::new()),
        ))
        .await
        .expect("response");

    let RunItemKind::ProviderCompaction(compaction) = response.output()[0].kind() else {
        panic!("not a provider compaction: {:?}", response.output()[0]);
    };
    assert_eq!(compaction, &stored_compaction());
    assert_eq!(response.output()[0].id().as_str(), "cmp_1");
    assert!(matches!(
        response.output()[1].kind(),
        RunItemKind::Message(_)
    ));
}

#[tokio::test]
async fn responses_keeps_a_streamed_compaction_item_too() {
    let server = MockServer::start().await;
    let frames = [
        json!({"type": "response.created", "sequence_number": 0,
               "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "response.output_item.done", "sequence_number": 1, "output_index": 0,
               "item": compaction_output()}),
        json!({"type": "response.completed", "sequence_number": 2,
               "response": {"id": "resp_1", "status": "completed", "output": [compaction_output()]}}),
    ];
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    mount(
        &server,
        "/v1/responses",
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;
    let model = responses_model(&server);

    let events = model
        .stream_response(ModelRequest::new(
            vec![],
            settings("openai", ModelSettings::new()),
        ))
        .collect::<Vec<_>>()
        .await;

    let lifted: Vec<RunItemKind> = events
        .into_iter()
        .filter_map(|event| match event.expect("no frame fails") {
            ModelStreamEvent::RunItem(event) => Some(event.item().kind().clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifted,
        [RunItemKind::ProviderCompaction(stored_compaction())]
    );
}

#[tokio::test]
async fn responses_replays_a_compaction_verbatim_and_sends_context_management() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/responses",
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_2", "status": "completed", "output": [answer()],
        })),
    )
    .await;
    let context_management = json!([{"type": "compaction", "compact_threshold": 123}]);

    responses_model(&server)
        .get_response(ModelRequest::new(
            vec![
                ModelInputItem::ProviderCompaction(stored_compaction()),
                ModelInputItem::Message(Message::user("next")),
            ],
            settings(
                "openai",
                ModelSettings::new().with_extra_body_value(
                    ProviderKey::new("openai"),
                    "context_management",
                    context_management.clone(),
                ),
            ),
        ))
        .await
        .expect("request");

    let body = sent_body(&server).await;
    assert_eq!(body["input"][0], stored_compaction().payload().clone());
    assert_eq!(body["context_management"], context_management);
}

/// Another provider's bucket is not sent: the field reaches only the provider it was written for.
#[tokio::test]
async fn context_management_in_another_providers_bucket_is_not_sent() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/responses",
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_3", "status": "completed", "output": [answer()],
        })),
    )
    .await;

    responses_model(&server)
        .get_response(ModelRequest::new(
            vec![ModelInputItem::Message(Message::user("hi"))],
            settings(
                "openai",
                ModelSettings::new().with_extra_body_value(
                    ProviderKey::new("elsewhere"),
                    "context_management",
                    json!([]),
                ),
            ),
        ))
        .await
        .expect("request");

    assert!(sent_body(&server).await.get("context_management").is_none());
}

// ---- Chat Completions and Anthropic Messages ------------------------------------------------

/// The reference's `chatcmpl_converter` refusal, in its words.
#[tokio::test]
async fn chat_refuses_a_compaction_item() {
    let server = MockServer::start().await;
    let model = OpenAiChatModel::new(
        "chat-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model");

    let error = model
        .get_response(ModelRequest::new(
            vec![ModelInputItem::ProviderCompaction(stored_compaction())],
            settings("openai", ModelSettings::new()),
        ))
        .await
        .expect_err("refused");

    assert!(
        error.to_string().contains(
            "Compaction items are not supported for chat completions. Please use the Responses \
             API to handle compaction."
        ),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn anthropic_refuses_a_compaction_item() {
    let server = MockServer::start().await;
    let model = AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model");

    let error = model
        .get_response(ModelRequest::new(
            vec![ModelInputItem::ProviderCompaction(stored_compaction())],
            settings("anthropic", ModelSettings::new().with_max_tokens(1024)),
        ))
        .await
        .expect_err("refused");

    assert!(
        error
            .to_string()
            .contains("Compaction items are not supported for Anthropic Messages"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
