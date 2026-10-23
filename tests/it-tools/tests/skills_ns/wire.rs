//! Schema and protocol verification against executed upstream spec declarations.
use super::*;
use ra_core::{
    item::RunItemKind,
    model::{Model, ModelRequest, ModelSettings, ProviderKey, SettingsLayers},
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn settings() -> ra_core::model::ResolvedModelSettings {
    SettingsLayers::new()
        .with_run(&ModelSettings::new().with_max_tokens(64))
        .resolve(&ProviderKey::new("openai"))
}

#[tokio::test]
async fn schemas_and_responses_wire_match_the_executed_reference() {
    let reference: Value =
        serde_json::from_str(include_str!("../fixtures/skills_ns/upstream-spec.json")).unwrap();
    let pair = tools(cloud(SkillCatalog::default(), ""), 1000);
    for (index, tool) in pair.iter().enumerate() {
        tool.validate().unwrap();
        assert_eq!(
            tool.schema().input_schema(),
            &reference["specs"][index]["tools"][0]["parameters"]
        );
        assert_eq!(tool.model_definition().output_json_schema(), None);
        assert_eq!(
            tool.schema().description(),
            reference["specs"][index]["tools"][0]["description"].as_str()
        );
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"resp","output":[]})))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    model
        .get_response(
            ModelRequest::new(vec![], settings())
                .with_tools(pair.iter().map(|t| t.model_definition()).collect()),
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let mut expected = reference["specs"][0].clone();
    expected["tools"]
        .as_array_mut()
        .unwrap()
        .push(reference["specs"][1]["tools"][0].clone());
    assert_eq!(body["tools"], json!([expected]));
}

#[tokio::test]
async fn chat_and_anthropic_reject_both_skills_tools_before_network_access() {
    let server = MockServer::start().await;
    let chat = OpenAiChatModel::new(
        "gpt-test",
        OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    let anthropic = AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    for tool in tools(cloud(SkillCatalog::default(), ""), 1000) {
        let request =
            || ModelRequest::new(vec![], settings()).with_tools(vec![tool.model_definition()]);
        for error in [
            chat.get_response(request()).await.unwrap_err(),
            anthropic.get_response(request()).await.unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(
                message.contains("only supported with OpenAI Responses models"),
                "{message}"
            );
            assert!(message.contains("models: tool namespace."), "{message}");
            assert!(
                message.contains(tool.origin().qualified_name()),
                "{message}"
            );
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn empty_responses_arguments_reach_the_tool_as_an_empty_object() {
    for arguments in ["", " \n\t"] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"resp","output":[{"type":"function_call","name":"read","namespace":"skills","call_id":"empty","arguments":arguments}]}))).mount(&server).await;
        let model = OpenAiResponsesModel::new(
            "gpt-test",
            OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
        )
        .unwrap();
        let result = model
            .get_response(ModelRequest::new(vec![], settings()))
            .await
            .unwrap();
        let RunItemKind::ToolCall(call_item) = result.output()[0].kind() else {
            panic!("expected tool call")
        };
        assert_eq!(call_item.arguments(), &json!({}));
        let pair = tools(cloud(SkillCatalog::default(), ""), 1000);
        assert_eq!(
            error_message(
                call(pair[1].as_ref(), call_item.arguments().clone())
                    .await
                    .unwrap_err()
            ),
            "missing field `package`"
        );
    }
}
