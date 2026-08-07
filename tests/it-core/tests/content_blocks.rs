//! R1-2 contracts for provider-neutral multimodal message content.

use std::path::Path;

use ra_core::item::{
    CallId, ContentBlock, ImageSource, ItemId, Message, MessageRole, OutputPhase, RunItem,
    RunItemKind, ThinkingBlock, ToolCall,
};
use serde_json::json;

fn 所有内容块() -> Vec<ContentBlock> {
    vec![
        ContentBlock::text("结果"),
        ContentBlock::thinking("先读取文件", "sig-1"),
        ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
        ContentBlock::image_path("assets/diagram.png"),
        ContentBlock::refusal("无法协助该请求"),
    ]
}

#[test]
fn 四种内容块都能稳定往返() {
    let blocks = 所有内容块();
    assert_eq!(
        blocks.iter().map(ContentBlock::label).collect::<Vec<_>>(),
        vec!["text", "thinking", "image", "image", "refusal"]
    );

    for block in blocks {
        let encoded = serde_json::to_string(&block).expect("内容块应可序列化");
        let decoded: ContentBlock = serde_json::from_str(&encoded).expect("内容块应可反序列化");
        assert_eq!(decoded, block, "往返改变了 {} 块", block.label());
    }
}

#[test]
fn 内容块在_run_item_信封里同样无损() {
    // 信封是 flatten + 相邻标记枚举的嵌套，块自身往返通过不代表套进 RunItem 也通过。
    let message = Message::new(MessageRole::Assistant, 所有内容块());
    let item = RunItem::new(ItemId::new("item-1"), RunItemKind::Message(message));

    let encoded = serde_json::to_string(&item).expect("RunItem 应可序列化");
    let decoded: RunItem = serde_json::from_str(&encoded).expect("RunItem 应可反序列化");
    assert_eq!(decoded, item);
}

#[test]
fn 图片明确区分_base64_与本地路径() {
    let inline = ContentBlock::image_base64("image/webp", "UklGRg==");
    let path = ContentBlock::image_path("assets/screenshot.png");

    let inline_source = inline
        .as_image()
        .and_then(|block| block.source().as_base64())
        .expect("应是 base64 图片");
    assert_eq!(inline_source.media_type(), "image/webp");
    assert_eq!(inline_source.data(), "UklGRg==");

    let path_source = path
        .as_image()
        .and_then(|block| block.source().as_local_path())
        .expect("应是本地路径图片");
    assert_eq!(path_source.path(), Path::new("assets/screenshot.png"));

    let encoded = serde_json::to_value([inline, path]).expect("图片应可序列化");
    assert_eq!(encoded[0]["data"]["source"]["type"], "base64");
    assert_eq!(encoded[1]["data"]["source"]["type"], "local_path");
}

#[test]
fn 本地路径必须是_utf8_否则整条记录写不出去() {
    // PathBuf 的 Serialize 对非 UTF-8 直接报错，炸的是整条 RunItem 而不是单个块。
    // try_new 把这个失败提前到构造期。
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let bad = std::path::PathBuf::from(OsString::from_vec(vec![0x66, 0xff, 0x6f]));
        assert!(ra_core::item::LocalImageSource::try_new(bad.clone()).is_err());
        assert!(serde_json::to_string(&ra_core::item::LocalImageSource::new(bad)).is_err());
    }
    assert!(ra_core::item::LocalImageSource::try_new("assets/ok.png").is_ok());
}

#[test]
fn thinking_签名原样保留() {
    let thinking = ThinkingBlock::new("检查边界", "opaque-signature");
    assert_eq!(thinking.thinking(), "检查边界");
    assert_eq!(thinking.signature(), "opaque-signature");

    let block = ContentBlock::Thinking(thinking);
    let encoded = serde_json::to_string(&block).expect("应可序列化");
    let decoded: ContentBlock = serde_json::from_str(&encoded).expect("应可反序列化");
    assert_eq!(
        decoded.as_thinking().map(ThinkingBlock::signature),
        Some("opaque-signature")
    );
}

#[test]
fn 工具调用只住在顶层项而不是内容块里() {
    // R1-17 的配对与孤儿裁剪只看项级 CallId。工具调用一旦能藏进 message content，
    // 它们就会静默漏掉，所以内容块层根本不提供这种表示。
    let labels: Vec<&str> = 所有内容块()
        .iter()
        .map(ContentBlock::label)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(labels, vec!["image", "refusal", "text", "thinking"]);

    let call = RunItem::new(
        ItemId::new("item-call"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "read_file",
            json!({"path": "Cargo.toml"}),
        )),
    );
    let message = RunItem::new(
        ItemId::new("item-message"),
        RunItemKind::Message(Message::assistant("读完了", OutputPhase::Commentary)),
    );

    assert_eq!(call.call_id().map(CallId::as_str), Some("call-1"));
    assert_eq!(message.call_id(), None, "消息本身不参与 call 配对");
}

#[test]
fn 拒答是独立信号而不是文本() {
    // R1-12 靠这个信号升级模型；混进 text_content 就只能去猜厂商措辞。
    let refused = Message::new(
        MessageRole::Assistant,
        vec![
            ContentBlock::text("先说明："),
            ContentBlock::refusal("无法协助该请求"),
        ],
    );
    let plain = Message::new(
        MessageRole::Assistant,
        vec![ContentBlock::text("这是普通回答")],
    );

    assert_eq!(refused.text_content(), "先说明：");
    assert_eq!(refused.refusal_content().as_deref(), Some("无法协助该请求"));
    assert_eq!(plain.refusal_content(), None);
    assert_eq!(
        serde_json::to_value(ContentBlock::refusal("x")).expect("应可序列化")["type"],
        "refusal"
    );
}

#[test]
fn message_文本投影跳过所有非文本块() {
    let mut content = 所有内容块();
    content.insert(3, ContentBlock::text("完成"));
    let message = Message::new(MessageRole::Assistant, content);

    assert_eq!(message.text_content(), "结果完成");
}

#[test]
fn 未知字段在块与图片来源两层都原样回写() {
    let original = ContentBlock::image_base64("image/png", "AAAA");
    let mut value = serde_json::to_value(original).expect("应可转 JSON");
    value["data"]
        .as_object_mut()
        .expect("image data 应是对象")
        .insert("future_image".into(), json!({"keep": true}));
    value["data"]["source"]["data"]
        .as_object_mut()
        .expect("source data 应是对象")
        .insert("future_source".into(), json!([1, 2, 3]));

    let old_reader: ContentBlock = serde_json::from_value(value).expect("旧代码应能读取");
    let image = old_reader.as_image().expect("应仍是 image");
    let source = image.source().as_base64().expect("应仍是 base64 source");
    assert!(image.unknown().get("future_image").is_some());
    assert!(source.unknown().get("future_source").is_some());

    let rewritten = serde_json::to_value(old_reader).expect("旧代码应能回写");
    assert_eq!(rewritten["data"]["future_image"]["keep"], true);
    assert_eq!(rewritten["data"]["source"]["data"]["future_source"][2], 3);
}

#[test]
fn 本地路径图片也能往返而不只是序列化() {
    let block = ContentBlock::image_path("assets/a.png");
    let encoded = serde_json::to_string(&block).expect("应可序列化");
    let decoded: ContentBlock = serde_json::from_str(&encoded).expect("应可反序列化");

    assert_eq!(decoded, block);
    assert!(matches!(
        decoded.as_image().expect("应是 image").source(),
        ImageSource::LocalPath(_)
    ));
}
