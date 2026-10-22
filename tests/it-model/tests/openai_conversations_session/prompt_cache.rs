//! Content cache marks survive remote history and pending-write reconciliation.

use super::*;
use ra_core::item::{
    Base64FileSource, ContentBlock, FileBlock, FileSource, ImageBlock, ImageDetail, ImageSource,
    TextBlock,
};
use ra_model::openai::prompt_cache::OPENAI_METADATA_NAMESPACE;

fn marked_items(mark: &Value) -> Vec<RunItem> {
    let metadata = json!({"prompt_cache_breakpoint":mark});
    let image = ImageBlock::new(ImageSource::base64("image/png", "aGVsbG8="))
        .with_detail(ImageDetail::High)
        .with_provider_metadata(OPENAI_METADATA_NAMESPACE, metadata.clone());
    let mut files = vec![
        FileBlock::new(FileSource::provider_file("file_123")),
        FileBlock::new(FileSource::url("https://example.test/report.pdf")),
        FileBlock::new(FileSource::Base64(
            Base64FileSource::new("aGVsbG8=").with_filename("report.pdf"),
        )),
    ]
    .into_iter()
    .map(|file| file.with_provider_metadata(OPENAI_METADATA_NAMESPACE, metadata.clone()))
    .collect::<Vec<_>>();
    // Exercise the existing replay envelope together with the new content metadata.
    let mut encoded = serde_json::to_value(&files[0]).unwrap();
    encoded["openai"] = json!({"filename":"report.pdf", "detail":"high"});
    files[0] = serde_json::from_value(encoded).unwrap();
    let mut content = vec![
        ContentBlock::Text(
            TextBlock::new("stable reference")
                .with_provider_metadata(OPENAI_METADATA_NAMESPACE, metadata.clone()),
        ),
        ContentBlock::Image(image.clone()),
    ];
    content.extend(files.iter().cloned().map(ContentBlock::File));
    let mut output = vec![ToolOutputBlock::Image(image)];
    output.extend(files.into_iter().map(ToolOutputBlock::File));
    vec![
        RunItem::new(
            ItemId::new("user"),
            RunItemKind::Message(Message::new(MessageRole::User, content)),
        ),
        RunItem::new(
            ItemId::new("assistant"),
            RunItemKind::Message(Message::new(
                MessageRole::Assistant,
                vec![ContentBlock::Text(
                    TextBlock::new("stable answer")
                        .with_provider_metadata(OPENAI_METADATA_NAMESPACE, metadata),
                )],
            )),
        ),
        RunItem::new(
            ItemId::new("call"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call_1"), "lookup", json!({}))),
        ),
        RunItem::new(
            ItemId::new("output"),
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call_1"),
                json!(ToolOutput::new(output).unwrap()),
            )),
        ),
    ]
}

fn assert_content_marks(item: &RunItem, mark: &Value) {
    let RunItemKind::Message(message) = item.kind() else {
        panic!("expected a message")
    };
    assert!(!message.content().is_empty());
    for block in message.content() {
        let metadata = match block {
            ContentBlock::Text(text) => text.provider_metadata(),
            ContentBlock::Image(image) => image.provider_metadata(),
            ContentBlock::File(file) => file.provider_metadata(),
            _ => panic!("unexpected content block"),
        };
        assert_eq!(
            &metadata[OPENAI_METADATA_NAMESPACE]["prompt_cache_breakpoint"],
            mark
        );
    }
}

#[tokio::test]
async fn cache_breakpoints_survive_conversation_reads_and_replay() {
    let fake = Fake::start().await;
    let session = fake.session().with_provider(ProviderKey::new("gateway"));
    let mark = json!({"mode":"explicit", "future_option":42});
    let original = marked_items(&mark);
    session.add_items(original.clone()).await.unwrap();
    let read = session.get_items(None).await.unwrap();
    assert_eq!(read.len(), original.len());
    assert_content_marks(&read[0], &mark);
    assert_content_marks(&read[1], &mark);
    let RunItemKind::ToolCallOutput(output) = read[3].kind() else {
        panic!("expected tool output")
    };
    let output = ToolOutput::from_stored(output.output()).unwrap().unwrap();
    let blocks = output.model_blocks();
    assert_eq!(blocks.len(), 4);
    for block in blocks {
        let metadata = match block {
            ToolOutputBlock::Image(image) => image.provider_metadata().clone(),
            ToolOutputBlock::File(file) => file.provider_metadata().clone(),
            _ => panic!("unexpected tool output block"),
        };
        assert_eq!(
            metadata[OPENAI_METADATA_NAMESPACE]["prompt_cache_breakpoint"],
            mark
        );
    }
    session.add_items(read).await.unwrap();
    let writes = fake.created_items();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].1, writes[1].1);
}

#[tokio::test]
async fn cache_breakpoints_keep_conversation_persistence_digests_stable() {
    let fake = Fake::start().await;
    let session = fake.session();
    let original = marked_items(&json!({"mode":"explicit"}));
    session.add_items(original.clone()).await.unwrap();
    let read = session.get_items(None).await.unwrap();
    assert_eq!(read.len(), original.len());
    for (before, after) in original.iter().zip(&read) {
        assert_eq!(
            session.item_digest_for_persistence(before).await.unwrap(),
            session.item_digest_for_persistence(after).await.unwrap(),
        );
    }
    let changed = marked_items(&json!({"mode":"explicit", "different":true}));
    assert_ne!(
        session
            .item_digest_for_persistence(&original[0])
            .await
            .unwrap(),
        session
            .item_digest_for_persistence(&changed[0])
            .await
            .unwrap(),
        "the digest must still account for the actual cache mark",
    );
}

#[tokio::test]
async fn pending_conversation_writes_with_cache_breakpoints_resume_without_duplicates() {
    let original = marked_items(&json!({"mode":"explicit"}));
    let input = original
        .iter()
        .map(|item| item.to_model_input().unwrap())
        .collect();
    assert_projected_input_resumes(input, original.len()).await;
}

#[tokio::test]
async fn null_cache_breakpoints_do_not_create_content_metadata() {
    let fake = Fake::start().await;
    fake.seed(
        "conv_x",
        vec![json!({
            "type":"message", "role":"user", "content":[
                {"type":"input_text", "text":"plain", "prompt_cache_breakpoint":null},
                {
                    "type":"input_image", "image_url":"https://example.test/i",
                    "prompt_cache_breakpoint":null
                },
                {"type":"input_file", "file_id":"file_123", "prompt_cache_breakpoint":null}
            ]
        })],
    );
    let session = fake.session().with_conversation_id("conv_x");
    let read = session.get_items(None).await.unwrap();
    let RunItemKind::Message(message) = read[0].kind() else {
        panic!("expected a message")
    };
    assert_eq!(message.content().len(), 3);
    for block in message.content() {
        let metadata = match block {
            ContentBlock::Text(text) => text.provider_metadata(),
            ContentBlock::Image(image) => image.provider_metadata(),
            ContentBlock::File(file) => file.provider_metadata(),
            _ => panic!("unexpected content block"),
        };
        assert!(metadata.is_empty());
    }
}
