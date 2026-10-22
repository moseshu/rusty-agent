// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
// Also adapted from openai/openai-agents-python (MIT) @ f53c62451c7f34c15f0c87fa745e05404025420f, modified.
//! Native hosted web search declaration and Responses wire contracts.

use std::collections::BTreeMap;

use futures::StreamExt;
use ra_core::model::{
    Model, ModelRequest, ModelSettings, ModelToolDefinition, ProviderKey, SettingsLayers,
    ToolChoice,
};
use ra_model::openai::{
    auth::OpenAiAuth,
    responses::{
        OpenAiResponsesModel,
        web_search::{
            ResponsesWebSearchTool, WebSearchContentType, WebSearchContextSize, WebSearchFilters,
            WebSearchTool, WebSearchToolImageSettings, WebSearchUserLocation,
        },
    },
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn request(tool: ModelToolDefinition, choice: ToolChoice) -> ModelRequest {
    ModelRequest::new(
        vec![],
        SettingsLayers::new()
            .with_run(&ModelSettings::new().with_tool_choice(choice))
            .resolve(&ProviderKey::new("openai")),
    )
    .with_tools(vec![tool])
}

async fn wire(request: ModelRequest, streaming: bool) -> Value {
    let server = MockServer::start().await;
    let response = json!({"id":"response-web", "output":[], "status":"completed"});
    let template = if streaming {
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {}\n\n",
                json!({"type":"response.completed","response":response})
            ),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(response)
    };
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        "web-model",
        OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri())),
    )
    .unwrap();
    if streaming {
        let mut stream = model.stream_response(request);
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    } else {
        model.get_response(request).await.unwrap();
    }
    serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap()
}

#[tokio::test]
async fn framework_defaults_and_native_choice_reach_both_http_paths() {
    for streaming in [false, true] {
        for choice in [
            ToolChoice::Auto,
            ToolChoice::Required,
            ToolChoice::None,
            ToolChoice::Tool("web_search".into()),
        ] {
            let tool = WebSearchTool::default();
            assert_eq!(tool.name(), "web_search");
            let body = wire(
                request(tool.to_definition().unwrap(), choice.clone()),
                streaming,
            )
            .await;
            assert_eq!(
                body["tools"],
                json!([{"type":"web_search","filters":null,"user_location":null,"search_context_size":"medium"}])
            );
            let expected = match choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::Required => json!("required"),
                ToolChoice::None => json!("none"),
                _ => json!({"type":"web_search"}),
            };
            assert_eq!(body["tool_choice"], expected);
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
    }
}

#[tokio::test]
async fn context_filters_location_and_explicit_false_are_preserved() {
    for streaming in [false, true] {
        for size in [
            WebSearchContextSize::Low,
            WebSearchContextSize::Medium,
            WebSearchContextSize::High,
        ] {
            let tool = WebSearchTool {
                search_context_size: size,
                external_web_access: Some(false),
                filters: Some(
                    serde_json::from_value(
                        json!({"allowed_domains":["example.com"],"future_filter":["keep"]}),
                    )
                    .unwrap(),
                ),
                user_location: Some(WebSearchUserLocation {
                    country: Some("US".into()),
                    region: Some("California".into()),
                    city: Some("San Francisco".into()),
                    timezone: Some("America/Los_Angeles".into()),
                    ..WebSearchUserLocation::default()
                }),
                ..WebSearchTool::default()
            };
            let body = wire(
                request(tool.to_definition().unwrap(), ToolChoice::Auto),
                streaming,
            )
            .await;
            let native = &body["tools"][0];
            assert_eq!(
                native["search_context_size"],
                serde_json::to_value(size).unwrap()
            );
            assert_eq!(native["external_web_access"], false);
            assert_eq!(
                native["filters"],
                json!({"allowed_domains":["example.com"],"future_filter":["keep"]})
            );
            assert_eq!(
                native["user_location"],
                json!({"type":"approximate","country":"US","region":"California","city":"San Francisco","timezone":"America/Los_Angeles"})
            );
        }
    }
}

#[tokio::test]
async fn image_results_add_required_include_and_preserve_caption_false() {
    for streaming in [false, true] {
        for types in [
            vec![WebSearchContentType::Text],
            vec![WebSearchContentType::Image],
            vec![WebSearchContentType::Text, WebSearchContentType::Image],
        ] {
            let tool = WebSearchTool {
                search_content_types: Some(types.clone()),
                image_settings: Some(WebSearchToolImageSettings {
                    max_results: Some(3),
                    caption: Some(false),
                    ..WebSearchToolImageSettings::default()
                }),
                ..WebSearchTool::default()
            };
            let settings = ModelSettings::new().with_extra_body(
                ProviderKey::new("openai"),
                BTreeMap::from([(
                    "include".into(),
                    json!(["web_search_call.results", "other.result"]),
                )]),
            );
            let call = ModelRequest::new(
                vec![],
                SettingsLayers::new()
                    .with_run(&settings)
                    .resolve(&ProviderKey::new("openai")),
            )
            .with_tools(vec![tool.to_definition().unwrap()]);
            let body = wire(call, streaming).await;
            assert_eq!(
                body["tools"][0]["search_content_types"],
                serde_json::to_value(&types).unwrap()
            );
            assert_eq!(
                body["tools"][0]["image_settings"],
                json!({"max_results":3,"caption":false})
            );
            assert_eq!(
                body["include"],
                json!([
                    "other.result",
                    "reasoning.encrypted_content",
                    "web_search_call.results"
                ])
            );
            let body = wire(
                request(
                    WebSearchTool {
                        search_content_types: Some(types.clone()),
                        ..WebSearchTool::default()
                    }
                    .to_definition()
                    .unwrap(),
                    ToolChoice::Auto,
                ),
                streaming,
            )
            .await;
            assert_eq!(
                body["include"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("web_search_call.results")),
                types.contains(&WebSearchContentType::Image)
            );
            assert!(body["tools"][0].get("image_settings").is_none());
        }
    }
}

#[tokio::test]
async fn codex_adapter_options_include_indexed_access_without_framework_defaults() {
    for streaming in [false, true] {
        let tool = ResponsesWebSearchTool {
            external_web_access: Some(true),
            indexed_web_access: Some(true),
            ..ResponsesWebSearchTool::default()
        };
        let body = wire(
            request(tool.to_definition().unwrap(), ToolChoice::Required),
            streaming,
        )
        .await;
        assert_eq!(
            body["tools"],
            json!([{"type":"web_search","external_web_access":true,"indexed_web_access":true}])
        );
    }
}

#[tokio::test]
async fn native_declarations_do_not_replace_legacy_hosted_definitions_or_functions() {
    let settings = ModelSettings::new().with_extra_body(
        ProviderKey::new("openai"),
        BTreeMap::from([(
            "tools".into(),
            json!([{"type":"file_search","vector_store_ids":["store"]}]),
        )]),
    );
    let call = ModelRequest::new(
        vec![],
        SettingsLayers::new()
            .with_run(&settings)
            .resolve(&ProviderKey::new("openai")),
    )
    .with_tools(vec![
        ModelToolDefinition::new("lookup", json!({"type":"object"})),
        WebSearchTool::default().to_definition().unwrap(),
        ModelToolDefinition::new("last", json!({"type":"object"})),
    ]);
    let body = wire(call, false).await;
    assert_eq!(body["tools"][0]["type"], "file_search");
    assert_eq!(body["tools"][1]["name"], "lookup");
    assert_eq!(body["tools"][2]["type"], "web_search");
    assert_eq!(body["tools"][3]["name"], "last");
}

#[test]
fn filters_keep_unknown_fields_but_reject_an_invalid_known_field_type() {
    let filters: WebSearchFilters =
        serde_json::from_value(json!({"future_filter":["example.com"]})).unwrap();
    assert_eq!(filters.extra["future_filter"], json!(["example.com"]));
    assert!(serde_json::from_value::<WebSearchFilters>(json!({"allowed_domains":42})).is_err());
}

#[tokio::test]
async fn other_protocols_refuse_native_declarations_before_http() {
    let auth = OpenAiAuth::new("test").with_base_url("http://127.0.0.1:1/v1");
    let chat = ra_model::openai::chat::OpenAiChatModel::new("chat", auth).unwrap();
    let call = request(
        WebSearchTool::default().to_definition().unwrap(),
        ToolChoice::Auto,
    );
    let error = chat.get_response(call.clone()).await.unwrap_err();
    assert!(error.to_string().contains("hosted tools"), "{error}");
    let anthropic = ra_model::anthropic::AnthropicMessagesModel::new(
        "claude",
        ra_model::anthropic::AnthropicAuth::new("test").with_base_url("http://127.0.0.1:1"),
    )
    .unwrap();
    let error = anthropic.get_response(call).await.unwrap_err();
    assert!(error.to_string().contains("hosted tools"), "{error}");
}

#[tokio::test]
async fn native_search_provider_errors_remain_provider_errors() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{"type":"invalid_request_error","message":"web search is unavailable for this model","param":"tools"}}))).mount(&server).await;
        let model = OpenAiResponsesModel::new(
            "unsupported-web-model",
            OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri())),
        )
        .unwrap();
        let call = request(
            WebSearchTool::default().to_definition().unwrap(),
            ToolChoice::Required,
        );
        let error = if streaming {
            let mut stream = model.stream_response(call);
            stream.next().await.unwrap().unwrap_err()
        } else {
            model.get_response(call).await.unwrap_err()
        };
        assert!(
            error.to_string().contains("web search is unavailable"),
            "{error}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[test]
fn framework_filter_defaults_and_owned_definitions_survive_serialization() {
    let tool = WebSearchTool {
        filters: Some(WebSearchFilters {
            extra: BTreeMap::from([("future_filter".into(), json!(["keep"]))]),
            ..WebSearchFilters::default()
        }),
        user_location: Some(
            serde_json::from_value(json!({"country":"US", "future_location":"keep"})).unwrap(),
        ),
        image_settings: Some(
            serde_json::from_value(
                json!({"max_results":-1, "caption":false, "future_image_setting":"keep"}),
            )
            .unwrap(),
        ),
        ..WebSearchTool::default()
    };
    let definition = tool.to_definition().unwrap();
    let restored: ModelToolDefinition =
        serde_json::from_value(serde_json::to_value(&definition).unwrap()).unwrap();
    assert_eq!(restored, definition);
    let ra_core::model::ModelToolKind::Hosted { declaration, .. } = restored.kind() else {
        panic!("hosted definition")
    };
    assert_eq!(
        declaration["filters"],
        json!({"allowed_domains":null,"future_filter":["keep"]})
    );
    assert!(definition.advertised_bytes().unwrap() > 50);
    assert_eq!(declaration["user_location"]["future_location"], "keep");
    assert_eq!(
        declaration["image_settings"],
        json!({"max_results":-1,"caption":false,"future_image_setting":"keep"})
    );
}

#[tokio::test]
async fn framework_builtin_selection_is_distinct_from_a_same_named_function() {
    let call = request(
        WebSearchTool::default().to_definition().unwrap(),
        ToolChoice::Tool("web_search".into()),
    )
    .with_tools(vec![
        ModelToolDefinition::new("web_search", json!({"type":"object"})),
        WebSearchTool::default().to_definition().unwrap(),
    ]);
    let body = wire(call, false).await;
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][1]["type"], "web_search");
    assert_eq!(body["tool_choice"], json!({"type":"web_search"}));
}
