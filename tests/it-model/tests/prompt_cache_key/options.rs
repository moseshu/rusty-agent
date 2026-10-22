//! Explicit settings and content marks ported from the Agents SDK; see NOTICE for sources.

use super::*;
use ra_core::{
    item::{
        CallId, ContentBlock, FileBlock, FileSource, ImageBlock, ImageSource, MessageRole,
        TextBlock, ToolCall, ToolCallOutput,
    },
    tool::{ToolOutput, ToolOutputBlock},
};
use ra_model::openai::{
    chat::ChatLoweringOptions,
    prompt_cache::{
        PromptCacheBreakpoint, PromptCacheMode, PromptCacheOptions, PromptCacheRetention,
        PromptCacheTtl, with_prompt_cache_options, with_prompt_cache_retention,
    },
};

fn settings(layer: &ModelSettings) -> ra_core::model::ResolvedModelSettings {
    SettingsLayers::new()
        .with_run(layer)
        .resolve(&ProviderKey::new("custom"))
}

fn options() -> PromptCacheOptions {
    PromptCacheOptions::new()
        .with_mode(PromptCacheMode::Explicit)
        .with_ttl(PromptCacheTtl::ThirtyMinutes)
}

async fn dispatch(model: &dyn Model, request: ModelRequest, streaming: bool) {
    if streaming {
        for event in model.stream_response(request).collect::<Vec<_>>().await {
            event.unwrap();
        }
    } else {
        model.get_response(request).await.unwrap();
    }
}

async fn sent_body(server: &MockServer) -> Value {
    serde_json::from_slice(
        &server
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .body,
    )
    .unwrap()
}

fn content(body: &Value, chat: bool) -> &Value {
    &body[if chat { "messages" } else { "input" }][0]["content"]
}

#[rstest]
#[tokio::test]
async fn explicit_options_and_retention_ignore_automatic_key_quirks(
    #[values(false, true)] chat: bool,
    #[values(false, true)] streaming: bool,
) {
    let (server, model) = endpoint(chat, streaming, false).await;
    for retention in [
        PromptCacheRetention::InMemory,
        PromptCacheRetention::TwentyFourHours,
    ] {
        let layer = with_prompt_cache_retention(
            with_prompt_cache_options(ModelSettings::new(), ProviderKey::new("custom"), options()),
            ProviderKey::new("custom"),
            retention,
        );
        dispatch(
            &*model,
            ModelRequest::new(vec![], settings(&layer)),
            streaming,
        )
        .await;
        let body = sent_body(&server).await;
        assert_eq!(
            body["prompt_cache_options"],
            json!({"mode":"explicit", "ttl":"30m"})
        );
        assert_eq!(body["prompt_cache_retention"], json!(retention));
        assert!(body.get("prompt_cache_key").is_none());
    }
    dispatch(
        &*model,
        ModelRequest::new(vec![], settings(&ModelSettings::new())),
        streaming,
    )
    .await;
    let body = sent_body(&server).await;
    assert!(body.get("prompt_cache_options").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn options_replace_as_a_whole_and_defaults_remain_omitted() {
    let key = ProviderKey::new("custom");
    let base = with_prompt_cache_options(ModelSettings::new(), key.clone(), options());
    for override_options in [
        PromptCacheOptions::new().with_mode(PromptCacheMode::Implicit),
        PromptCacheOptions::new(),
    ] {
        let run = with_prompt_cache_options(ModelSettings::new(), key.clone(), override_options);
        let resolved = SettingsLayers::new()
            .with_provider(&base)
            .with_run(&run)
            .resolve(&key);
        assert_eq!(
            resolved.extra_body()["prompt_cache_options"],
            json!(override_options)
        );
        assert!(
            resolved.extra_body()["prompt_cache_options"]
                .get("ttl")
                .is_none()
        );
    }
    assert_eq!(
        settings(&base).extra_body()["prompt_cache_options"],
        json!(options())
    );
    assert_eq!(options().mode(), Some(PromptCacheMode::Explicit));
    assert_eq!(options().ttl(), Some(PromptCacheTtl::ThirtyMinutes));
}

fn blocks() -> Vec<ContentBlock> {
    let metadata = PromptCacheBreakpoint::Explicit.metadata();
    vec![
        ContentBlock::Text(
            TextBlock::new("stable").with_provider_metadata("openai", metadata.clone()),
        ),
        ContentBlock::Image(
            ImageBlock::new(ImageSource::url("https://example.com/image.png"))
                .with_provider_metadata("openai", metadata.clone()),
        ),
        ContentBlock::File(
            FileBlock::new(FileSource::provider_file("file-1"))
                .with_provider_metadata("openai", metadata),
        ),
    ]
}

#[rstest]
#[tokio::test]
async fn text_image_and_file_marks_survive_serialized_history_and_both_transports(
    #[values(false, true)] chat: bool,
    #[values(false, true)] streaming: bool,
) {
    let (server, model) = endpoint(chat, streaming, false).await;
    let history = vec![ModelInputItem::Message(Message::new(
        MessageRole::User,
        blocks(),
    ))];
    let stored = serde_json::to_vec(&history).unwrap();
    let restored = serde_json::from_slice(&stored).unwrap();
    dispatch(
        &*model,
        ModelRequest::new(restored, settings(&ModelSettings::new())),
        streaming,
    )
    .await;
    let body = sent_body(&server).await;
    let parts = content(&body, chat).as_array().unwrap();
    assert_eq!(parts.len(), 3);
    for part in parts {
        assert_eq!(part["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
        assert!(part.get("provider_metadata").is_none());
    }
    assert_eq!(parts[0]["type"], if chat { "text" } else { "input_text" });
    assert_eq!(
        parts[1]["type"],
        if chat { "image_url" } else { "input_image" }
    );
    assert_eq!(parts[2]["type"], if chat { "file" } else { "input_file" });
}

#[tokio::test]
async fn chat_keeps_a_single_marked_text_as_an_array_but_ignores_null_marks() {
    let (server, model) = endpoint(true, false, false).await;
    for metadata in [
        PromptCacheBreakpoint::Explicit.metadata(),
        json!({"prompt_cache_breakpoint":null}),
    ] {
        let text = TextBlock::new("stable").with_provider_metadata("openai", metadata.clone());
        let input = vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::Text(text)],
        ))];
        dispatch(
            &*model,
            ModelRequest::new(input, settings(&ModelSettings::new())),
            false,
        )
        .await;
        let body = sent_body(&server).await;
        if metadata["prompt_cache_breakpoint"].is_null() {
            assert_eq!(content(&body, true), "stable");
        } else {
            assert_eq!(
                content(&body, true),
                &json!([
                    {"type":"text", "text":"stable", "prompt_cache_breakpoint":{"mode":"explicit"}}
                ])
            );
        }
    }
}

#[rstest]
#[tokio::test]
async fn assistant_marks_pass_through_responses_and_follow_chat_degradation_policy(
    #[values(false, true)] streaming: bool,
) {
    let marked = || {
        vec![ModelInputItem::Message(Message::new(
            MessageRole::Assistant,
            vec![blocks().remove(0)],
        ))]
    };
    let request = || ModelRequest::new(marked(), settings(&ModelSettings::new()));
    let (server, responses) = endpoint(false, streaming, false).await;
    dispatch(&*responses, request(), streaming).await;
    assert_eq!(
        content(&sent_body(&server).await, false)[0]["prompt_cache_breakpoint"],
        json!({"mode":"explicit"})
    );

    let (server, strict) = endpoint(true, streaming, false).await;
    if streaming {
        assert!(
            strict
                .stream_response(request())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .any(|r| r.is_err())
        );
    } else {
        assert!(strict.get_response(request()).await.is_err());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    let relaxed = OpenAiChatModel::new(
        "test",
        OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri())),
    )
    .unwrap()
    .with_lowering_options(ChatLoweringOptions::new().with_strict_feature_validation(false));
    dispatch(&relaxed, request(), streaming).await;
    assert_eq!(content(&sent_body(&server).await, true), "stable");
}

#[rstest]
#[tokio::test]
async fn tool_output_images_and_files_preserve_marks(
    #[values(false, true)] chat: bool,
    #[values(false, true)] streaming: bool,
) {
    let (server, model) = endpoint(chat, streaming, false).await;
    let model: Arc<dyn Model> = if chat {
        Arc::new(
            OpenAiChatModel::new(
                "test",
                OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri())),
            )
            .unwrap()
            .with_quirks(ProviderQuirks::new().with_multimodal_tool_output(true)),
        )
    } else {
        model
    };
    let mut marked = blocks().into_iter().skip(1);
    let ContentBlock::Image(image) = marked.next().unwrap() else {
        unreachable!()
    };
    let ContentBlock::File(file) = marked.next().unwrap() else {
        unreachable!()
    };
    let output = ToolOutput::new(vec![
        ToolOutputBlock::Image(image),
        ToolOutputBlock::File(file),
    ])
    .unwrap();
    let input = vec![
        ModelInputItem::ToolCall(ToolCall::new(CallId::new("call-1"), "read", json!({}))),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(CallId::new("call-1"), json!(output))),
    ];
    dispatch(
        &*model,
        ModelRequest::new(input, settings(&ModelSettings::new())),
        streaming,
    )
    .await;
    let body = sent_body(&server).await;
    let output =
        &body[if chat { "messages" } else { "input" }][1][if chat { "content" } else { "output" }];
    let parts = output.as_array().unwrap();
    assert_eq!(parts.len(), 2);
    for part in parts {
        assert_eq!(part["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
    }
}

#[rstest]
#[tokio::test]
async fn explicit_mark_count_is_left_to_the_endpoint(#[values(false, true)] chat: bool) {
    let (server, model) = endpoint(chat, false, false).await;
    let input = vec![ModelInputItem::Message(Message::new(
        MessageRole::User,
        vec![blocks().remove(0); 5],
    ))];
    dispatch(
        &*model,
        ModelRequest::new(input, settings(&ModelSettings::new())),
        false,
    )
    .await;
    assert_eq!(
        content(&sent_body(&server).await, chat)
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[rstest]
#[tokio::test]
async fn only_the_family_namespace_is_read_and_non_null_marks_are_opaque(
    #[values(false, true)] chat: bool,
) {
    let (server, model) = endpoint(chat, false, false).await;
    let raw_mark = json!({"mode":"future-mode", "future_option":42});
    let text = TextBlock::new("stable")
        .with_provider_metadata("custom", PromptCacheBreakpoint::Explicit.metadata())
        .with_provider_metadata("openai", json!({"prompt_cache_breakpoint":raw_mark}));
    let input = vec![ModelInputItem::Message(Message::new(
        MessageRole::User,
        vec![ContentBlock::Text(text)],
    ))];
    dispatch(
        &*model,
        ModelRequest::new(input, settings(&ModelSettings::new())),
        false,
    )
    .await;
    let body = sent_body(&server).await;
    assert_eq!(content(&body, chat)[0]["prompt_cache_breakpoint"], raw_mark);
}
