//! R2-3 contracts for a structured tool result.

use ra_core::{
    compat::SchemaVersion,
    item::{
        Base64FileSource, FileBlock, FileSource, ImageBlock, ImageDetail, ImageSource,
        ProviderFileSource, UrlSource,
    },
    tool::{
        ArtifactRef, MODEL_EXCERPT_SCHEMA_VERSION, ModelExcerpt,
        OBSERVATION_METADATA_SCHEMA_VERSION, ObservationMetadata, TOOL_OUTPUT_SCHEMA_VERSION,
        ToolOutput, ToolOutputBlock, Truncation, TruncationStage,
    },
};
use serde_json::json;

#[test]
fn test_tool_output_01() {
    // `openai-agents` hit this square from the other side: `all([])` is True, so an empty structured
    // list passed the conversion check, the whole tool result was silently dropped, and it only
    // surfaced when the provider rejected the next request.
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
    // These errors show up during resume and rollout replay, where the question is which of several
    // thousand records is unreadable and how it broke. `#[serde(untagged)]` answers every failure
    // with "data did not match any variant" — the real cause is discarded along with the attempt
    // that produced it.
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
    // The whole discrimination rule rests on this invariant: what claims a record is our own version
    // marker. The day someone puts `skip_serializing_if` on `schema_version`, every record silently
    // becomes "not ours" and falls back to string replay — the model then reads a lump of JSON
    // literal, and no assertion fails.
    let output = ToolOutput::text("done").with_metadata(
        ObservationMetadata::new().with_truncation(Truncation::new(TruncationStage::Tool, 90, 4)),
    );
    let stored = serde_json::to_value(&output).expect("tool output must serialize");

    assert!(
        stored.get("schema_version").is_some(),
        "the version marker is what claims the record"
    );
    let restored = ToolOutput::from_stored(&stored)
        .expect("a record we wrote ourselves must be readable")
        .expect("a record we wrote ourselves must be recognized as a tool result");
    assert_eq!(restored, output);
}

#[test]
fn test_tool_output_06() {
    // The middle case is why this signature exists: treating "unreadable" as "not ours" would push a
    // record a newer build wrote into the model's context as stringified JSON instead of failing
    // where somebody can see it.
    let readable = json!({"schema_version": 1, "blocks": [{"type": "text", "text": "done"}]});
    assert_eq!(
        ToolOutput::from_stored(&readable)
            .expect("a readable record must not error")
            .expect("it really is a tool result")
            .as_text(),
        Some("done")
    );

    // A bare value a host stored before this type existed: not a tool result, and a caller may
    // stringify it.
    for foreign in [
        json!({"rows": 2}),
        json!("plain"),
        json!({"type": "image"}),
        json!({"blocks": 7}),
    ] {
        assert!(
            ToolOutput::from_stored(&foreign)
                .expect("a bare value is not an error")
                .is_none(),
            "{foreign} must not be taken for a tool result"
        );
    }

    // Claims to be a tool result and cannot be read: an error, not a fallback.
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

    // Concatenating would let a caller believe it received everything while the image block had
    // quietly gone.
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
    // The vast majority of results carry neither a truncation nor guidance. Rendering an empty block
    // for those pays, every turn and for every tool result, for content that is not there.
    let quiet = ToolOutput::text("done");

    assert!(quiet.metadata().render().is_none());
    assert_eq!(quiet.model_blocks(), quiet.blocks());
}

#[test]
fn test_tool_output_09() {
    // The tool cut once against its own ceiling and the context budget cut again. With room for only
    // one, the loss the model is told about is smaller than the loss it took.
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
    // Budget trimming happens long after the tool returned, so it has to append rather than rebuild.
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
    // The structured half is for the host — budget, UI, logs — and the model reads one sentence.
    // Keeping them apart makes "which facts are worth these tokens" a rendering policy rather than a
    // wire format.
    let output = ToolOutput::text("hit-1\nhit-2").with_metadata(
        ObservationMetadata::new()
            .with_truncation(Truncation::new(TruncationStage::Tool, 9_000, 200))
            .with_guidance("narrow the search with a path prefix"),
    );

    let blocks = output.model_blocks();
    assert_eq!(blocks.len(), 2);
    let note = blocks[0]
        .as_text()
        .expect("the metadata block must be text");
    assert!(note.contains("truncated by tool"));
    assert!(note.contains("200"));
    assert!(note.contains("9000"));
    assert!(note.contains("narrow the search with a path prefix"));
    assert_eq!(blocks[1].as_text(), Some("hit-1\nhit-2"));

    // Rendering is a projection rather than a field: what is stored still holds only the body.
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

#[test]
fn test_tool_output_17() {
    let artifact = ArtifactRef::new("tool-output/72756e/63616c6c").expect("a stable ref");
    let excerpt = ModelExcerpt::new(vec![ToolOutputBlock::text("head\n...\ntail")], artifact)
        .expect("a non-empty excerpt");
    let output = ToolOutput::text("complete result").with_model_excerpt(excerpt);

    assert_eq!(output.as_text(), Some("complete result"));
    assert_eq!(
        output
            .model_excerpt()
            .expect("the excerpt is installed")
            .schema_version(),
        MODEL_EXCERPT_SCHEMA_VERSION
    );
    assert!(
        output.model_blocks()[0]
            .as_text()
            .is_some_and(|text| text.contains("head"))
    );
    // The model is told where the record is, not offered a fetch no tool answers.
    let note = output
        .model_blocks()
        .last()
        .and_then(ToolOutputBlock::as_text)
        .expect("the artifact note")
        .to_owned();
    assert!(note.contains("artifact `tool-output/72756e/63616c6c`"));
    assert!(note.contains("retained in the session record"));
    // The complete block is not what the provider receives.
    assert!(
        !output
            .model_blocks()
            .iter()
            .filter_map(ToolOutputBlock::as_text)
            .any(|text| text == "complete result")
    );
}

/// Validation written only in a constructor is validation not written: rollout and checkpoint reach
/// these values by deserializing them.
#[test]
fn test_tool_output_18() {
    let empty_blocks = json!({
        "schema_version": 2,
        "blocks": [{ "type": "text", "text": "complete result" }],
        "model_excerpt": {
            "schema_version": 1,
            "blocks": [],
            "artifact_ref": "tool-output/empty"
        }
    });
    let error = ToolOutput::from_stored(&empty_blocks).expect_err("an empty excerpt is malformed");
    assert!(
        error
            .to_string()
            .contains("model excerpt must carry at least one block")
    );

    for reference in ["", "   ", " padded ", "tool-output/\u{7}bell"] {
        let payload = json!({
            "schema_version": 2,
            "blocks": [{ "type": "text", "text": "complete result" }],
            "model_excerpt": {
                "schema_version": 1,
                "blocks": [{ "type": "text", "text": "head" }],
                "artifact_ref": reference
            }
        });
        let error = ToolOutput::from_stored(&payload)
            .expect_err("a stored artifact reference is held to the constructor's rule");
        assert!(
            error.to_string().contains("artifact reference"),
            "unexpected rejection for {reference:?}: {error}"
        );
    }

    // A reference that satisfies the rule still round-trips, excerpt and all.
    let readable = json!({
        "schema_version": 2,
        "blocks": [{ "type": "text", "text": "complete result" }],
        "model_excerpt": {
            "schema_version": 1,
            "blocks": [{ "type": "text", "text": "head" }],
            "artifact_ref": "tool-output/72756e/63616c6c"
        }
    });
    let output = ToolOutput::from_stored(&readable)
        .expect("a well-formed excerpt reads")
        .expect("it is a tool result");
    assert_eq!(
        output
            .model_excerpt()
            .expect("the excerpt survived")
            .artifact_ref()
            .as_str(),
        "tool-output/72756e/63616c6c"
    );
    let rewritten = serde_json::to_value(&output).expect("it serializes");
    assert_eq!(
        ToolOutput::from_stored(&rewritten)
            .expect("what this build wrote, it reads back")
            .expect("it is a tool result"),
        output
    );
}
