//! Cache-key routing on both OpenAI protocols, independent of prefix length and transport mode.
#[path = "prompt_cache_key/options.rs"]
mod options;

use futures::StreamExt;
use ra_core::{
    item::{Message, ModelInputItem},
    model::{Model, ModelRequest, ModelSettings, ProviderKey, SettingsLayers},
    prompt::CachePlan,
};
use ra_model::{
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
    provider::quirks::ProviderQuirks,
};
use rstest::rstest;
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn endpoint(chat: bool, streaming: bool, supported: bool) -> (MockServer, Arc<dyn Model>) {
    let server = MockServer::start().await;
    let template = if streaming {
        let body = if chat {
            "data: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned()
        } else {
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[]}})
            )
        };
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    } else if chat {
        ResponseTemplate::new(200).set_body_json(json!({"id":"chat-1","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}))
    } else {
        ResponseTemplate::new(200)
            .set_body_json(json!({"id":"resp-1","status":"completed","output":[]}))
    };
    Mock::given(method("POST"))
        .and(path(if chat {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        }))
        .respond_with(template)
        .mount(&server)
        .await;
    let auth = OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri()));
    let quirks = ProviderQuirks::new().with_prompt_cache_key(supported);
    let model: Arc<dyn Model> = if chat {
        Arc::new(
            OpenAiChatModel::new("test", auth)
                .unwrap()
                .with_quirks(quirks),
        )
    } else {
        Arc::new(
            OpenAiResponsesModel::new("test", auth)
                .unwrap()
                .with_quirks(quirks),
        )
    };
    (server, model)
}

#[rstest]
#[tokio::test]
async fn runtime_key_and_legacy_fallback_follow_target_endpoint_capabilities(
    #[values(false, true)] chat: bool,
    #[values(false, true)] streaming: bool,
    #[values(false, true)] supported: bool,
) {
    let (server, model) = endpoint(chat, streaming, supported).await;
    let settings = SettingsLayers::new().resolve(&ProviderKey::new("custom"));
    assert_eq!(
        model.supports_default_prompt_cache_key(&settings),
        supported
    );
    for instructions in [None, Some("short")] {
        let mut request = ModelRequest::new(
            vec![ModelInputItem::Message(Message::user("hello"))],
            settings.clone(),
        )
        .with_prompt_cache_key("root-thread");
        if let Some(text) = instructions {
            request = request
                .with_system_instructions(text)
                .with_cache_plan(CachePlan::for_prefix(text, Some("legacy")));
        }
        if streaming {
            for event in model.stream_response(request).collect::<Vec<_>>().await {
                event.unwrap();
            }
        } else {
            model.get_response(request).await.unwrap();
        }
    }
    let legacy = ModelRequest::new(vec![], settings)
        .with_system_instructions("short")
        .with_cache_plan(CachePlan::for_prefix("short", Some("legacy")));
    if streaming {
        for event in model.stream_response(legacy).collect::<Vec<_>>().await {
            event.unwrap();
        }
    } else {
        model.get_response(legacy).await.unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    for (index, request) in requests.iter().enumerate() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if supported {
            assert_eq!(
                body["prompt_cache_key"],
                if index == 2 { "legacy" } else { "root-thread" }
            );
        } else {
            assert!(body.get("prompt_cache_key").is_none());
        }
    }
}

#[rstest]
#[tokio::test]
async fn explicit_extras_including_null_override_every_automatic_key(
    #[values(false, true)] chat: bool,
    #[values(false, true)] streaming: bool,
    #[values(false, true)] supported: bool,
) {
    let (server, model) = endpoint(chat, streaming, supported).await;
    for value in [json!("explicit"), Value::Null] {
        let layer = ModelSettings::new().with_extra_body_value(
            ProviderKey::new("custom"),
            "prompt_cache_key",
            value.clone(),
        );
        let settings = SettingsLayers::new()
            .with_run(&layer)
            .resolve(&ProviderKey::new("custom"));
        assert!(!model.supports_default_prompt_cache_key(&settings));
        let request = ModelRequest::new(
            vec![ModelInputItem::Message(Message::user("hello"))],
            settings,
        )
        .with_prompt_cache_key("runtime");
        if streaming {
            for event in model.stream_response(request).collect::<Vec<_>>().await {
                event.unwrap();
            }
        } else {
            model.get_response(request).await.unwrap();
        }
    }
    let requests = server.received_requests().await.unwrap();
    for (request, expected) in requests.iter().zip([json!("explicit"), Value::Null]) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.get("prompt_cache_key"), Some(&expected));
    }
    assert!(
        std::str::from_utf8(&requests[1].body)
            .unwrap()
            .contains("\"prompt_cache_key\":null")
    );
}
