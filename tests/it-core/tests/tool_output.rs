//! R2-3 contracts for a structured tool result.

use ra_core::{
    compat::SchemaVersion,
    item::{
        Base64FileSource, FileBlock, FileSource, ImageBlock, ImageDetail, ImageSource,
        ProviderFileSource, UrlSource,
    },
    tool::{
        OBSERVATION_METADATA_SCHEMA_VERSION, ObservationMetadata, TOOL_OUTPUT_SCHEMA_VERSION,
        ToolOutput, ToolOutputBlock, Truncation, TruncationStage,
    },
};
use serde_json::json;

#[test]
fn test_tool_output_01() {
    // `openai-agents` 从另一头撞过这一格：`all([])` 是 True，空的结构化列表通过了转换
    // 检查、整条工具结果被静默丢掉，直到下一次请求被 provider 拒了才现形。
    let error = ToolOutput::new(Vec::new()).unwrap_err();

    assert!(error.to_string().contains("at least one block"));
    assert_eq!(
        ToolOutput::new(vec![ToolOutputBlock::text("done")])
            .unwrap()
            .blocks()
            .len(),
        1
    );
}

#[test]
fn test_tool_output_02() {
    let error = serde_json::from_value::<ToolOutput>(json!({
        "schema_version": 1,
        "blocks": []
    }))
    .unwrap_err();

    assert!(error.to_string().contains("at least one block"));
}

#[test]
fn test_tool_output_03() {
    let output: ToolOutput = serde_json::from_value(json!({
        "type": "text",
        "text": "written before R2-3"
    }))
    .expect("R2-1 session records must remain readable");

    assert_eq!(output.as_text(), Some("written before R2-3"));
    assert_eq!(output.schema_version(), TOOL_OUTPUT_SCHEMA_VERSION);
}

#[test]
fn test_tool_output_04() {
    // 这些错误出现在 resume 与 rollout replay 里，问的是「几千条里哪一条读不了、怎么坏的」。
    // `#[serde(untagged)]` 对每一种失败都只会答「data did not match any variant」——真正的
    // 原因连同产生它的那次尝试一起被丢掉了。
    let cases = [
        (
            json!({"schema_version": 1, "blocks": [{"type": "bogus"}]}),
            "unknown variant `bogus`",
        ),
        (
            json!({"schema_version": 1, "blocks": [{"type": "text"}]}),
            "missing field `text`",
        ),
        (json!({"schema_version": 1, "blocks": 7}), "invalid type"),
        (
            json!({"schema_version": 1, "blocks": []}),
            "at least one block",
        ),
    ];
    for (payload, expected) in cases {
        let error = serde_json::from_value::<ToolOutput>(payload.clone()).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "{payload} 的报错应当点名 `{expected}`，实际是：{error}"
        );
        assert!(!error.to_string().contains("did not match any variant"));
    }
}

#[test]
fn test_tool_output_05() {
    // 整条判别规则压在这个不变量上：认领的凭据是「带我们自己的版本标记」。哪天有人给
    // `schema_version` 加了 `skip_serializing_if`，每一条记录都会静默变成「不是我们的」、
    // 全部退回字符串回放——模型于是读到一坨 JSON 字面量，而没有任何断言会挂。
    let output = ToolOutput::text("done").with_metadata(
        ObservationMetadata::new().with_truncation(Truncation::new(TruncationStage::Tool, 90, 4)),
    );
    let stored = serde_json::to_value(&output).expect("tool output must serialize");

    assert!(
        stored.get("schema_version").is_some(),
        "版本标记就是认领凭据"
    );
    let restored = ToolOutput::from_stored(&stored)
        .expect("自己写的记录不该读不了")
        .expect("自己写的记录必须被认成工具结果");
    assert_eq!(restored, output);
}

#[test]
fn test_tool_output_06() {
    // 中间那一档是这个签名存在的理由：把「读不了」当成「不是我们的」，会让新版本写下的
    // 记录被静默字符串化成 JSON 塞进模型上下文，而不是在有人看得见的地方失败。
    let readable = json!({"schema_version": 1, "blocks": [{"type": "text", "text": "done"}]});
    assert_eq!(
        ToolOutput::from_stored(&readable)
            .expect("可读的记录不该报错")
            .expect("它确实是一条工具结果")
            .as_text(),
        Some("done")
    );

    // 宿主在 R2-3 之前存的裸值：不是工具结果，调用方字符串化即可。
    for foreign in [
        json!({"rows": 2}),
        json!("plain"),
        json!({"type": "image"}),
        json!({"blocks": 7}),
    ] {
        assert!(
            ToolOutput::from_stored(&foreign)
                .expect("裸值不是错误")
                .is_none(),
            "{foreign} 不该被当成工具结果"
        );
    }

    // 声称是工具结果却读不了：报错，不退化。
    let error = ToolOutput::from_stored(&json!({
        "schema_version": 1,
        "blocks": [{"type": "bogus"}]
    }))
    .unwrap_err();
    assert!(error.to_string().contains("unreadable"));
}

#[test]
fn test_tool_output_07() {
    assert_eq!(ToolOutput::text("done").as_text(), Some("done"));

    // 拼接会让调用方以为自己拿到了全部，而图片块已经悄悄没了。
    let multimodal = ToolOutput::new(vec![
        ToolOutputBlock::text("这是截图"),
        ToolOutputBlock::Image(ImageBlock::new(ImageSource::provider_file("file-1"))),
    ])
    .unwrap();
    assert_eq!(multimodal.as_text(), None);
    assert_eq!(multimodal.blocks().len(), 2);
}

#[test]
fn test_tool_output_08() {
    // 绝大多数结果没有截断也没有建议。这种情况下渲染出一个空块，等于每一轮为每个工具
    // 结果各付一次没有内容的钱。
    let quiet = ToolOutput::text("done");

    assert!(quiet.metadata().render().is_none());
    assert_eq!(quiet.model_blocks(), quiet.blocks());
}

#[test]
fn test_tool_output_09() {
    // 工具先按自己的上限截了一刀，R5-1 的预算又截了一刀。只留一格的话，模型被告知的
    // 损失会比实际的小。
    let metadata = ObservationMetadata::new()
        .with_truncation(Truncation::new(TruncationStage::Tool, 12_000, 4_000))
        .with_truncation(Truncation::new(TruncationStage::ContextBudget, 4_000, 500));
    let output = ToolOutput::text("部分内容").with_metadata(metadata);

    let truncations = output.metadata().truncations();
    assert_eq!(truncations.len(), 2);
    assert_eq!(truncations[0].stage(), TruncationStage::Tool);
    assert_eq!(truncations[0].original_bytes(), 12_000);
    assert_eq!(truncations[1].stage(), TruncationStage::ContextBudget);
    assert_eq!(truncations[1].retained_bytes(), 500);
    assert!(output.metadata().is_truncated());
}

#[test]
fn test_tool_output_10() {
    // R5-1 的裁剪发生在工具早就返回之后，它必须能追加而不是重建一份。
    let mut output = ToolOutput::text("部分内容").with_metadata(
        ObservationMetadata::new().with_truncation(Truncation::new(
            TruncationStage::Tool,
            900,
            300,
        )),
    );
    output.metadata_mut().push_truncation(Truncation::new(
        TruncationStage::ContextBudget,
        300,
        100,
    ));

    assert_eq!(output.metadata().truncations().len(), 2);
}

#[test]
fn test_tool_output_11() {
    // 结构化的那份留给宿主（预算、UI、日志），模型读到的是一句话。两者分开，
    // 「哪些事实值这些 token」就成了渲染策略而不是 wire 格式。
    let output = ToolOutput::text("hit-1\nhit-2").with_metadata(
        ObservationMetadata::new()
            .with_truncation(Truncation::new(TruncationStage::Tool, 9_000, 200))
            .with_guidance("narrow the search with a path prefix"),
    );

    let blocks = output.model_blocks();
    assert_eq!(blocks.len(), 2);
    let note = blocks[0].as_text().expect("元数据块必须是文本");
    assert!(note.contains("truncated by tool"));
    assert!(note.contains("200"));
    assert!(note.contains("9000"));
    assert!(note.contains("narrow the search with a path prefix"));
    assert_eq!(blocks[1].as_text(), Some("hit-1\nhit-2"));

    // 渲染是投影不是字段：存下来的那份仍然只有正文。
    assert_eq!(output.blocks().len(), 1);
}

#[test]
fn test_tool_output_12() {
    for (stage, label) in [
        (TruncationStage::Tool, "tool"),
        (TruncationStage::ContextBudget, "context_budget"),
    ] {
        assert_eq!(stage.label(), label);
        assert_eq!(stage.to_string(), label);
        assert_eq!(serde_json::to_value(stage).unwrap(), json!(label));
    }
}

#[test]
fn test_tool_output_13() {
    let sources = vec![
        ImageSource::base64("image/png", "AAAA"),
        ImageSource::local_path("/tmp/a.png"),
        ImageSource::url("https://example.com/a.png"),
        ImageSource::provider_file("file-1"),
    ];
    for source in sources {
        let block = ImageBlock::new(source).with_detail(ImageDetail::High);
        let restored: ImageBlock =
            serde_json::from_value(serde_json::to_value(&block).unwrap()).unwrap();
        assert_eq!(restored, block);
        assert_eq!(restored.detail(), Some(ImageDetail::High));
    }

    let files = vec![
        FileSource::Base64(Base64FileSource::new("AAAA").with_filename("report.pdf")),
        FileSource::Url(UrlSource::new("https://example.com/a.pdf")),
        FileSource::ProviderFile(ProviderFileSource::new("file-2")),
    ];
    for source in files {
        let block = FileBlock::new(source);
        let restored: FileBlock =
            serde_json::from_value(serde_json::to_value(&block).unwrap()).unwrap();
        assert_eq!(restored, block);
    }
}

#[test]
fn test_tool_output_14() {
    let block = ImageBlock::new(ImageSource::provider_file("file-1"));
    let wire = serde_json::to_value(&block).unwrap();

    assert!(block.detail().is_none());
    assert!(wire.get("detail").is_none());
}

#[test]
fn test_tool_output_15() {
    let stored = json!({
        "schema_version": 7,
        "blocks": [{ "type": "text", "text": "done", "future_span": "s-1" }],
        "metadata": {
            "schema_version": 9,
            "truncations": [{
                "schema_version": 4,
                "stage": "tool",
                "original_bytes": 100,
                "retained_bytes": 10,
                "future_unit": "tokens"
            }],
            "future_scan": { "skipped": 3 }
        },
        "future_top": true
    });

    let output: ToolOutput =
        serde_json::from_value(stored).expect("newer output must stay readable");
    assert_eq!(output.schema_version(), SchemaVersion::new(7));
    assert_eq!(output.metadata().schema_version(), SchemaVersion::new(9));

    let written = serde_json::to_value(&output).unwrap();
    assert_eq!(written["future_top"], json!(true));
    assert_eq!(written["metadata"]["future_scan"]["skipped"], json!(3));
    assert_eq!(
        written["metadata"]["truncations"][0]["future_unit"],
        json!("tokens")
    );
}

#[test]
fn test_tool_output_16() {
    let output = ToolOutput::text("done");

    assert_eq!(output.schema_version(), TOOL_OUTPUT_SCHEMA_VERSION);
    assert_eq!(
        output.metadata().schema_version(),
        OBSERVATION_METADATA_SCHEMA_VERSION
    );
    assert!(output.unknown().is_empty());
}
