//! Complete tool observations stay in the session while only their model view is budgeted.

use ra_context::budget::ToolResultBudget;
use ra_core::{
    item::{CallId, ImageBlock, ImageSource},
    prompt::estimate_tokens,
    state::RunId,
    tool::{ObservationMetadata, ToolOutput, ToolOutputBlock, Truncation, TruncationStage},
};

/// The excerpt text a model would receive, joined the way the projector builds it.
fn excerpt_text(output: &ToolOutput) -> String {
    output
        .model_excerpt()
        .expect("an excerpted result")
        .blocks()
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_payload_cost(output: &ToolOutput) -> (usize, usize) {
    let blocks = output.model_blocks();
    let text = blocks
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    let opaque_bytes = blocks
        .iter()
        .filter(|block| block.as_text().is_none())
        .map(|block| {
            serde_json::to_string(block)
                .expect("opaque blocks serialize")
                .len()
        })
        .sum::<usize>();
    (text.len() + opaque_bytes, estimate_tokens(&text))
}

fn project(budget: &ToolResultBudget, call: &str, output: ToolOutput) -> ToolOutput {
    budget
        .project_output(&RunId::new("run-1"), &CallId::new(call), output)
        .expect("projection succeeds")
}

#[test]
fn a_result_inside_both_limits_stays_unprojected() {
    let budget = ToolResultBudget::new(512, 128).expect("a valid budget");
    let output = ToolOutput::text("small result");

    let projected = project(&budget, "call-small", output.clone());

    assert_eq!(projected, output);
    assert!(projected.model_excerpt().is_none());
    assert_eq!(projected.model_blocks(), projected.blocks());
    assert!(projected.metadata().truncations().is_empty());
}

#[test]
fn an_oversized_text_result_keeps_complete_history_and_a_head_tail_excerpt() {
    let budget = ToolResultBudget::new(512, 128).expect("a valid budget");
    let complete = "HEAD: useful first fact\n".to_owned()
        + &"middle evidence that is deliberately too large\n".repeat(12)
        + "TAIL: useful final fact";
    let output = ToolOutput::text(complete.clone());

    let projected = project(&budget, "call-1", output);

    assert_eq!(projected.as_text(), Some(complete.as_str()));
    let excerpt = projected
        .model_excerpt()
        .expect("an oversized result is excerpted");
    assert_eq!(
        excerpt.artifact_ref().as_str(),
        "tool-output/72756e2d31/63616c6c2d31"
    );
    let body = excerpt_text(&projected);
    assert!(body.contains("HEAD: useful"));
    assert!(body.contains("TAIL: useful"));
    assert!(body.contains("middle omitted by context budget"));

    let truncation = projected
        .metadata()
        .truncations()
        .last()
        .expect("the context cut is recorded");
    assert_eq!(truncation.stage(), TruncationStage::ContextBudget);
    assert!(truncation.original_bytes() > truncation.retained_bytes());
    assert!(projected.metadata().guidance()[0].contains("byte and token limits"));

    let model_blocks = projected.model_blocks();
    assert!(
        model_blocks[0]
            .as_text()
            .expect("metadata note")
            .contains("truncated by context_budget")
    );
    assert!(
        model_blocks
            .last()
            .and_then(ToolOutputBlock::as_text)
            .is_some_and(|text| text.contains("tool-output/72756e2d31/63616c6c2d31"))
    );
}

/// The whole point of the excerpt: the complete body must not reach the provider.
#[test]
fn the_complete_body_never_reaches_the_model_once_an_excerpt_exists() {
    let budget = ToolResultBudget::new(512, 128).expect("a valid budget");
    let complete = "HEAD\n".to_owned() + &"UNIQUE-MIDDLE-EVIDENCE\n".repeat(40) + "TAIL";
    let output = ToolOutput::text(complete.clone());

    let projected = project(&budget, "call-1", output);

    let rendered = projected
        .model_blocks()
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains(&complete));
    assert!(rendered.len() < complete.len());
    // The complete blocks are still the stored record.
    assert_eq!(projected.as_text(), Some(complete.as_str()));
}

/// A projector whose own output fails its admission check has no invariant left to test.
#[test]
fn the_excerpt_it_produces_fits_the_budget_that_produced_it() {
    for (max_bytes, max_tokens) in [(512_usize, 128_usize), (1_024, 128), (4_096, 128)] {
        let budget = ToolResultBudget::new(max_bytes, max_tokens).expect("a valid budget");
        let complete = "line of evidence\n".repeat(600);
        let projected = project(&budget, "call-fit", ToolOutput::text(complete));

        let (model_bytes, model_tokens) = model_payload_cost(&projected);
        assert!(
            model_bytes <= max_bytes,
            "model payload of {model_bytes} bytes overruns the {max_bytes}-byte ceiling"
        );
        assert!(
            model_tokens <= max_tokens,
            "model payload of {model_tokens} estimated tokens overruns the {max_tokens}-token ceiling"
        );
    }
}

#[test]
fn the_token_limit_is_reported_even_when_the_byte_limit_has_room() {
    let budget = ToolResultBudget::new(16_384, 128).expect("a valid budget");

    let projected = project(&budget, "call-token", ToolOutput::text("a".repeat(1_000)));

    assert!(projected.model_excerpt().is_some());
    assert!(projected.metadata().guidance()[0].contains("the token limit"));
}

#[test]
fn existing_metadata_is_priced_before_the_excerpt_body() {
    let budget = ToolResultBudget::new(1_024, 128).expect("a valid budget");
    let output = ToolOutput::text("HEAD\n".to_owned() + &"middle\n".repeat(200) + "TAIL")
        .with_metadata(
            ObservationMetadata::new()
                .with_truncation(Truncation::new(TruncationStage::Tool, 4_000, 800))
                .with_guidance("The tool omitted earlier lines before context budgeting."),
        );

    let projected = project(&budget, "call-metadata", output);

    assert!(projected.model_excerpt().is_some());
    let (model_bytes, model_tokens) = model_payload_cost(&projected);
    assert!(model_bytes <= budget.max_bytes());
    assert!(model_tokens <= budget.max_tokens());
}

/// An image is often the answer, so one the ceiling can afford survives the projection.
#[test]
fn an_opaque_block_the_ceiling_can_afford_survives_the_excerpt() {
    let budget = ToolResultBudget::new(4_096, 128).expect("a valid budget");
    let output = ToolOutput::new(vec![
        ToolOutputBlock::text("x".repeat(600)),
        ToolOutputBlock::Image(ImageBlock::new(ImageSource::base64(
            "image/png",
            "A".repeat(512),
        ))),
    ])
    .expect("a two-block result");

    let projected = project(&budget, "call-image", output);

    let excerpt = projected.model_excerpt().expect("an excerpted result");
    assert!(
        excerpt
            .blocks()
            .iter()
            .any(|block| matches!(block, ToolOutputBlock::Image(_))),
        "an affordable image must not be replaced by prose"
    );
    assert!(!excerpt_text(&projected).contains("non-text tool-output block"));
}

#[test]
fn opaque_blocks_are_not_misrepresented_as_text_when_they_are_omitted() {
    let budget = ToolResultBudget::new(1_024, 128).expect("a valid budget");
    let output = ToolOutput::block(ToolOutputBlock::Image(ImageBlock::new(
        ImageSource::base64("image/png", "A".repeat(2_048)),
    )));

    let projected = project(&budget, "call-image", output);

    assert!(matches!(projected.blocks()[0], ToolOutputBlock::Image(_)));
    let excerpt = projected
        .model_excerpt()
        .expect("the image must be excerpted");
    assert!(
        excerpt
            .blocks()
            .iter()
            .all(|block| block.as_text().is_some()),
        "an unaffordable image is named, not shipped"
    );
    assert!(excerpt_text(&projected).contains("non-text tool-output block"));
    let truncation = projected
        .metadata()
        .truncations()
        .last()
        .expect("the context cut is recorded");
    assert_eq!(truncation.retained_bytes(), 0);
}

/// A ceiling smaller than the excerpt's own framing produces prose, not output.
#[test]
fn a_ceiling_too_small_to_carry_an_excerpt_is_rejected_at_configuration_time() {
    for ceilings in [
        (0, 0),
        (0, 64),
        (1_024, 0),
        (16, 64),
        (1_024, 4),
        (256, 128),
    ] {
        let error = ToolResultBudget::new(ceilings.0, ceilings.1)
            .expect_err("a floor, not a non-zero check");
        assert!(error.to_string().contains("at least"));
    }
    ToolResultBudget::new(
        ToolResultBudget::default().max_bytes(),
        ToolResultBudget::default().max_tokens(),
    )
    .expect("the shipped defaults clear their own floor");
}

/// The floor prices the framework's fixed prose; the identifiers a reference carries it cannot.
///
/// A run and call identifier are chosen by the host and the provider long after a budget was
/// configured, so the shortfall they can cause has to be reported by the projection rather than by
/// the constructor — and reported with the numbers, so it names which ceiling to raise.
#[test]
fn a_projection_that_cannot_afford_its_artifact_reference_reports_the_shortfall() {
    let budget = ToolResultBudget::new(512, 128).expect("a valid budget");

    let error = budget
        .project_output(
            &RunId::new("r".repeat(64)),
            &CallId::new("c".repeat(64)),
            ToolOutput::text("line of evidence\n".repeat(600)),
        )
        .expect_err("a reference this long does not fit a 512-byte ceiling");

    let message = error.to_string();
    assert!(message.contains("artifact reference"), "{message}");
    assert!(
        message.contains("512 bytes and 128 estimated tokens"),
        "{message}"
    );
    assert!(message.contains("projected"), "{message}");

    // The same identifiers project cleanly once the ceiling can afford them.
    ToolResultBudget::new(4_096, 256)
        .expect("a valid budget")
        .project_output(
            &RunId::new("r".repeat(64)),
            &CallId::new("c".repeat(64)),
            ToolOutput::text("line of evidence\n".repeat(600)),
        )
        .expect("a ceiling with room for the reference projects");
}
