//! Older tool outputs are compacted only in the request projection.

use std::collections::BTreeSet;

use ra_context::eviction::{
    DEFAULT_MAX_OUTPUT_CHARS, DEFAULT_PREVIEW_CHARS, DEFAULT_RECENT_ITEMS, DEFAULT_RECENT_TURNS,
    ToolOutputTrimmer,
};
use ra_core::{
    item::{CallId, ImageBlock, ImageSource, Message, ModelInputItem, ToolCall, ToolCallOutput},
    tool::{
        ArtifactRef, ModelExcerpt, ObservationMetadata, ToolOutput, ToolOutputBlock, Truncation,
        TruncationStage,
    },
};
use serde_json::Value;

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

fn call(call_id: &str, name: &str) -> ModelInputItem {
    ModelInputItem::ToolCall(ToolCall::new(
        CallId::new(call_id),
        name,
        serde_json::json!({}),
    ))
}

fn output(call_id: &str, body: impl Into<String>) -> ModelInputItem {
    ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new(call_id),
        Value::String(body.into()),
    ))
}

fn structured_output(call_id: &str, output: &ToolOutput) -> ModelInputItem {
    ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new(call_id),
        serde_json::to_value(output).expect("structured output serializes"),
    ))
}

fn output_text(item: &ModelInputItem) -> &str {
    match item {
        ModelInputItem::ToolCallOutput(output) => {
            output.output().as_str().expect("the test result is text")
        }
        _ => panic!("expected a tool output"),
    }
}

fn structured(item: &ModelInputItem) -> ToolOutput {
    match item {
        ModelInputItem::ToolCallOutput(output) => ToolOutput::from_stored(output.output())
            .expect("the projected result is readable")
            .expect("the projected result is still a structured tool result"),
        _ => panic!("expected a tool output"),
    }
}

/// The model-visible text of a projected result, as a provider adapter would render it.
fn model_text(item: &ModelInputItem) -> String {
    structured(item)
        .model_blocks()
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn trims_only_oversized_outputs_before_the_recent_user_turn_window() {
    let large_old = "old result ".repeat(40);
    let large_recent = "recent result ".repeat(40);
    let input = vec![
        user("first request"),
        call("call-1", "search"),
        output("call-1", large_old.clone()),
        user("second request"),
        call("call-2", "search"),
        output("call-2", large_recent.clone()),
        user("third request"),
        call("call-3", "search"),
        output("call-3", large_recent.clone()),
    ];
    let trimmer = ToolOutputTrimmer::new(2, 80, 20).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert!(output_text(&projected[2]).starts_with("[Trimmed: search output"));
    assert!(output_text(&projected[2]).contains("old result old"));
    assert_eq!(output_text(&projected[5]), large_recent);
    assert_eq!(output_text(&projected[8]), large_recent);
    assert_eq!(
        output_text(&input[2]),
        large_old,
        "the session input is unchanged"
    );
}

#[test]
fn a_preview_is_separated_from_its_header_by_a_real_newline() {
    let large = "line of evidence ".repeat(40);
    let input = vec![
        user("first request"),
        call("call-1", "search"),
        output("call-1", large),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 100, 20).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let summary = output_text(&projected[2]);

    assert!(
        summary.contains("char preview]\nline of evidence"),
        "the header and preview must be separated by a newline, not the two characters \
         `\\` and `n`: {summary}"
    );
    assert!(
        !summary.contains("\\n"),
        "no escaped newline reaches a model: {summary}"
    );
}

#[test]
fn a_replacement_never_exceeds_the_configured_output_ceiling() {
    let large = "verbose tool chatter ".repeat(200);
    // A preview allowance far larger than the ceiling: the ceiling still wins.
    for (max_output_chars, preview_chars) in [(9, 4000), (40, 4000), (200, 4000), (500, 200)] {
        let input = vec![
            user("first request"),
            call("call-1", "shell_command"),
            output("call-1", large.clone()),
            user("second request"),
        ];
        let trimmer =
            ToolOutputTrimmer::new(1, max_output_chars, preview_chars).expect("valid limits");

        let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
        let summary = output_text(&projected[2]);

        assert!(
            summary.chars().count() <= max_output_chars,
            "a {}-char replacement broke a {max_output_chars}-char ceiling: {summary}",
            summary.chars().count()
        );
        assert!(summary.starts_with("[Trimmed"));
    }
}

#[test]
fn an_allowlist_trims_only_the_paired_tool_name() {
    let large = "output ".repeat(50);
    let input = vec![
        user("first request"),
        call("call-search", "search"),
        output("call-search", large.clone()),
        call("call-shell", "shell_command"),
        output("call-shell", large.clone()),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 80, 20)
        .expect("valid limits")
        .with_trimmable_tools(Some(BTreeSet::from(["search".to_owned()])));

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert!(output_text(&projected[2]).starts_with("[Trimmed: search output"));
    assert_eq!(output_text(&projected[4]), large);
}

#[test]
fn an_empty_allowlist_disables_a_configured_trimmer() {
    let large = "output ".repeat(50);
    let input = vec![
        user("first request"),
        call("call-1", "search"),
        output("call-1", large.clone()),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 80, 20)
        .expect("valid limits")
        .with_trimmable_tools(Some(BTreeSet::new()));

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert_eq!(output_text(&projected[2]), large);
}

#[test]
fn an_unpaired_output_is_not_trimmed_when_an_allowlist_requires_an_identity() {
    let large = "orphan output ".repeat(30);
    let input = vec![
        user("first request"),
        output("missing-call", large.clone()),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 80, 20)
        .expect("valid limits")
        .with_trimmable_tools(Some(BTreeSet::from(["search".to_owned()])));

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert_eq!(output_text(&projected[1]), large);
}

#[test]
fn an_unpaired_output_is_trimmed_under_an_unnamed_tool_when_no_allowlist_is_set() {
    let large = "orphan output ".repeat(30);
    let input = vec![
        user("first request"),
        output("missing-call", large),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 80, 20).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert!(output_text(&projected[1]).starts_with("[Trimmed: unknown_tool output"));
}

#[test]
fn a_single_user_turn_run_still_trims_beyond_the_recent_item_window() {
    let large = "grep hit ".repeat(60);
    let mut input = vec![user("the only request")];
    for index in 0..12 {
        input.push(call(&format!("call-{index}"), "grep"));
        input.push(output(&format!("call-{index}"), large.clone()));
    }
    let trimmer = ToolOutputTrimmer::new(2, 80, 20)
        .expect("valid limits")
        .with_recent_items(6)
        .expect("a non-zero item window");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    // 25 items, so the last six (indices 19..25) are protected and everything before is not.
    assert!(
        output_text(&projected[2]).starts_with("[Trimmed: grep output"),
        "the oldest result of a one-request run must still be trimmable"
    );
    assert!(output_text(&projected[18]).starts_with("[Trimmed: grep output"));
    assert_eq!(output_text(&projected[20]), large);
    assert_eq!(output_text(&projected[24]), large);
}

#[test]
fn a_history_shorter_than_the_item_window_is_left_alone() {
    let large = "grep hit ".repeat(60);
    let input = vec![
        user("the only request"),
        call("call-1", "grep"),
        output("call-1", large.clone()),
    ];
    let trimmer = ToolOutputTrimmer::new(2, 80, 20)
        .expect("valid limits")
        .with_recent_items(20)
        .expect("a non-zero item window");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");

    assert_eq!(output_text(&projected[2]), large);
}

#[test]
fn structured_output_drops_opaque_blocks_without_previewing_them() {
    let structured_result = ToolOutput::new(vec![
        ToolOutputBlock::text("readable evidence ".repeat(30)),
        ToolOutputBlock::Image(ImageBlock::new(ImageSource::base64(
            "image/png",
            "secret-image-data".repeat(100),
        ))),
    ])
    .expect("a structured output");
    let input = vec![
        user("first request"),
        call("call-1", "inspect"),
        structured_output("call-1", &structured_result),
        user("second request"),
    ];
    // The ceiling has to clear the header before it can buy preview characters; naming the opaque
    // block is what makes this header long.
    let trimmer = ToolOutputTrimmer::new(1, 120, 20).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let summary = model_text(&projected[2]);

    assert!(summary.starts_with("[Trimmed"));
    assert!(summary.contains("dropped 1 opaque block"));
    assert!(summary.contains("readable evidence"));
    assert!(!summary.contains("secret-image-data"));
}

#[test]
fn a_trimmed_structured_result_keeps_its_metadata_and_artifact_reference() {
    let artifact = ArtifactRef::new("tool-output/abc/def").expect("a valid reference");
    let metadata = ObservationMetadata::new()
        .with_truncation(Truncation::new(TruncationStage::ContextBudget, 5_000, 200))
        .with_guidance("Re-run with a narrower pattern to see the rest.");
    let budgeted = ToolOutput::text("complete observation")
        .with_metadata(metadata)
        .with_model_excerpt(
            ModelExcerpt::new(
                vec![ToolOutputBlock::text("body of the excerpt ".repeat(30))],
                artifact.clone(),
            )
            .expect("a valid excerpt"),
        );
    let input = vec![
        user("first request"),
        call("call-1", "read_file"),
        structured_output("call-1", &budgeted),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 400, 40).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let trimmed = structured(&projected[2]);

    assert_eq!(
        trimmed.metadata().truncations().len(),
        1,
        "the cut an earlier stage recorded still travels with the result"
    );
    assert_eq!(
        trimmed.metadata().guidance(),
        ["Re-run with a narrower pattern to see the rest."]
    );
    assert_eq!(
        trimmed
            .model_excerpt()
            .expect("the artifact locator survives the second projection")
            .artifact_ref(),
        &artifact
    );
    assert!(
        model_text(&projected[2]).chars().count() <= 400,
        "metadata, locator, and summary must fit the same model-facing ceiling"
    );
}

#[test]
fn oversized_retained_metadata_falls_back_to_a_bare_summary_within_the_ceiling() {
    let budgeted = ToolOutput::text("tool body ".repeat(100)).with_metadata(
        ObservationMetadata::new().with_guidance("metadata that cannot fit ".repeat(40)),
    );
    let input = vec![
        user("first request"),
        call("call-1", "read_file"),
        structured_output("call-1", &budgeted),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 120, 40).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let summary = output_text(&projected[2]);

    assert!(summary.starts_with("[Trimmed"));
    assert!(summary.chars().count() <= 120);
    assert!(
        !summary.contains("metadata that cannot fit"),
        "a projection cannot preserve metadata that would overrun its ceiling"
    );
}

#[test]
fn a_preview_shows_the_tools_own_content_rather_than_the_frameworks_note() {
    let metadata = ObservationMetadata::new()
        .with_truncation(Truncation::new(TruncationStage::ContextBudget, 5_000, 200))
        .with_guidance(
            "Context-budget excerpt limited by size: the complete model-visible result was \
             5000 bytes.",
        );
    let budgeted = ToolOutput::new(vec![ToolOutputBlock::text(
        "the answer the model asked for ".repeat(40),
    )])
    .expect("a structured output")
    .with_metadata(metadata);
    let input = vec![
        user("first request"),
        call("call-1", "read_file"),
        structured_output("call-1", &budgeted),
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 400, 60).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let preview = structured(&projected[2])
        .as_text()
        .expect("the trimmed result is one text block")
        .to_owned();

    assert!(
        preview.contains("the answer the model asked for"),
        "the preview must be the tool's output, not the framework's own prose: {preview}"
    );
    assert!(
        !preview.contains("Context-budget excerpt limited by"),
        "the guidance sentence belongs in the metadata note, not in the preview: {preview}"
    );
}

#[test]
fn a_trimmed_result_keeps_its_call_id_and_error_flag() {
    let large = "failure detail ".repeat(50);
    let failed = ModelInputItem::ToolCallOutput(
        ToolCallOutput::new(CallId::new("call-1"), Value::String(large)).with_error(true),
    );
    let input = vec![
        user("first request"),
        call("call-1", "shell_command"),
        failed,
        user("second request"),
    ];
    let trimmer = ToolOutputTrimmer::new(1, 80, 20).expect("valid limits");

    let projected = trimmer.trim_model_input(&input).expect("trimming succeeds");
    let ModelInputItem::ToolCallOutput(trimmed) = &projected[2] else {
        panic!("expected a tool output");
    };

    assert!(output_text(&projected[2]).starts_with("[Trimmed"));
    assert_eq!(trimmed.call_id(), &CallId::new("call-1"));
    assert!(
        trimmed.is_error(),
        "a replayed tool failure must not become an apparent success"
    );
}

#[test]
fn rejects_a_configuration_that_cannot_carry_a_replacement() {
    assert!(
        ToolOutputTrimmer::new(0, 10, 1).is_err(),
        "no protected turn"
    );
    assert!(
        ToolOutputTrimmer::new(1, 0, 1).is_err(),
        "no output ceiling"
    );
    assert!(
        ToolOutputTrimmer::new(1, 8, 1).is_err(),
        "a ceiling below `[Trimmed]` cannot carry any replacement"
    );
    assert!(ToolOutputTrimmer::new(1, 9, 1).is_ok());
    assert!(
        ToolOutputTrimmer::new(1, 9, 1)
            .expect("valid limits")
            .with_recent_items(0)
            .is_err(),
        "a zero item window would leave the newest result trimmable"
    );
}

#[test]
fn the_default_trimmer_uses_the_published_constants() {
    let trimmer = ToolOutputTrimmer::default();

    assert_eq!(trimmer.recent_turns(), DEFAULT_RECENT_TURNS);
    assert_eq!(trimmer.recent_items(), DEFAULT_RECENT_ITEMS);
    assert_eq!(trimmer.max_output_chars(), DEFAULT_MAX_OUTPUT_CHARS);
    assert_eq!(trimmer.preview_chars(), DEFAULT_PREVIEW_CHARS);
    assert_eq!(trimmer.trimmable_tools(), None);
}
