//! Reference-based eviction keeps useful old results regardless of their position in history.

use ra_context::eviction::{
    DEFAULT_MAX_OUTPUT_CHARS, DEFAULT_MAX_UNREFERENCED_TURNS, DEFAULT_PREVIEW_CHARS,
    ToolOutputReferenceTrimmer,
};
use ra_core::{
    item::{CallId, ModelInputItem, ToolCall, ToolCallOutput},
    state::{RunId, TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION, ToolOutputReferenceTracker},
    tool::{ToolOutput, ToolOutputBlock},
};

/// A generated run ID is a UUID and provider call IDs run near thirty characters. Both are
/// hex-encoded into an artifact reference, so these are what the replacement ceiling really pays
/// for — short fixture IDs understate it by a factor of three.
const REALISTIC_RUN: &str = "550e8400-e29b-41d4-a716-446655440000";
const REALISTIC_CALL: &str = "call_JQ8x2mZ0aBcDeFgHiJkLmNoP";

/// Fixed cost of the locator sentence plus its block separator for the identifiers above.
const REALISTIC_LOCATOR_CHARS: usize = 213;

fn call(call_id: &str, name: &str) -> ModelInputItem {
    ModelInputItem::ToolCall(ToolCall::new(
        CallId::new(call_id),
        name,
        serde_json::json!({}),
    ))
}

fn structured_output(call_id: &str, text: impl Into<String>) -> ModelInputItem {
    let output = ToolOutput::text(text);
    ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new(call_id),
        serde_json::to_value(output).expect("structured output serializes"),
    ))
}

/// A result stored the way a build predating the structured `ToolOutput` schema wrote it.
fn legacy_output(call_id: &str, text: impl Into<String>) -> ModelInputItem {
    ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new(call_id),
        serde_json::Value::String(text.into()),
    ))
}

fn structured(item: &ModelInputItem) -> ToolOutput {
    let ModelInputItem::ToolCallOutput(output) = item else {
        panic!("expected a tool output");
    };
    ToolOutput::from_stored(output.output())
        .expect("the projected result is readable")
        .expect("the projected result remains structured")
}

fn raw_output(item: &ModelInputItem) -> &serde_json::Value {
    let ModelInputItem::ToolCallOutput(output) = item else {
        panic!("expected a tool output");
    };
    output.output()
}

/// Everything the provider would receive for one result, joined the way the renderer joins it.
fn model_text(output: &ToolOutput) -> String {
    output
        .model_blocks()
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn referenced_old_output_stays_complete_while_an_equally_old_unreferenced_output_is_trimmed() {
    let run_id = RunId::new("run-1");
    let stale = CallId::new("call-stale");
    let live = CallId::new("call-live");
    let large = "grep hit ".repeat(80);
    let input = vec![
        call(stale.as_str(), "grep"),
        structured_output(stale.as_str(), large.clone()),
        call(live.as_str(), "grep"),
        structured_output(live.as_str(), large.clone()),
    ];
    let mut references = ToolOutputReferenceTracker::new(run_id.clone());
    references
        .record_turn(1, [stale.clone(), live.clone()], [])
        .expect("the first two results are registered");
    references
        .record_turn(5, [], [live.clone()])
        .expect("a later turn explicitly uses the live result");

    let trimmer = ToolOutputReferenceTrimmer::new(4, 240, 40).expect("valid limits");
    let projected = trimmer
        .trim_model_input(&run_id, 9, &references, &input)
        .expect("projection succeeds");

    let stale_output = structured(&projected[1]);
    let stale_excerpt = stale_output
        .model_excerpt()
        .expect("a stale output gains a bounded artifact-backed excerpt");
    assert_eq!(
        stale_excerpt.artifact_ref().as_str(),
        "tool-output/72756e2d31/63616c6c2d7374616c65"
    );
    assert!(
        stale_excerpt
            .blocks()
            .iter()
            .filter_map(ToolOutputBlock::as_text)
            .collect::<String>()
            .starts_with("[Trimmed: grep output"),
    );
    assert!(
        model_text(&stale_output).chars().count() <= 240,
        "metadata, excerpt, and artifact note must fit the configured ceiling"
    );
    assert!(
        structured(&projected[3]).model_excerpt().is_none(),
        "a result referenced on turn 5 stays complete on turn 9 despite its position"
    );
    assert_eq!(input[1], structured_output(stale.as_str(), large));
}

#[test]
fn an_untracked_output_is_retained_conservatively() {
    let run_id = RunId::new("run-1");
    let known = CallId::new("call-known");
    let untracked = CallId::new("call-untracked");
    let large = "search match ".repeat(80);
    let input = vec![
        call(known.as_str(), "search"),
        structured_output(known.as_str(), large.clone()),
        call(untracked.as_str(), "search"),
        structured_output(untracked.as_str(), large.clone()),
    ];
    let mut references = ToolOutputReferenceTracker::new(run_id.clone());
    references
        .record_turn(1, [known.clone()], [])
        .expect("known output is registered");
    let trimmer = ToolOutputReferenceTrimmer::new(2, 180, 30).expect("valid limits");

    let projected = trimmer
        .trim_model_input(&run_id, 5, &references, &input)
        .expect("projection succeeds");

    assert!(structured(&projected[1]).model_excerpt().is_some());
    assert_eq!(
        structured(&projected[3]).as_text(),
        Some(large.as_str()),
        "absence of a retention record cannot be used as evidence that a result is stale"
    );
}

#[test]
fn the_staleness_window_counts_only_turns_that_have_completed() {
    let call_id = CallId::new("call-1");
    let mut tracker = ToolOutputReferenceTracker::new(RunId::new("run-1"));
    tracker
        .record_turn(4, [call_id.clone()], [])
        .expect("output is registered");

    // Turn 7 is being prepared and its response has not arrived, so turns 5 and 6 are the only
    // ones that completed without a reference: two, not three.
    assert!(
        !tracker.is_unreferenced_for(&call_id, 7, 3),
        "two completed unreferenced turns do not fill a window of three"
    );
    assert!(
        tracker.is_unreferenced_for(&call_id, 7, 2),
        "two completed unreferenced turns fill a window of two"
    );
    assert!(
        !tracker.is_unreferenced_for(&call_id, 4, 1),
        "the turn that produced the result is not a turn it went unreferenced"
    );
    assert!(
        !tracker.is_unreferenced_for(&call_id, 5, 1),
        "the turn being prepared has not completed and cannot count against the result"
    );
    assert!(
        !tracker.is_unreferenced_for(&call_id, 2, 1),
        "a current turn behind the last reference never makes a result stale"
    );
}

#[test]
fn empty_completed_turns_advance_the_persisted_high_water_mark() {
    let call_id = CallId::new("call-1");
    let mut tracker = ToolOutputReferenceTracker::new(RunId::new("run-1"));
    tracker
        .record_turn(1, [call_id], [])
        .expect("output is registered");
    tracker
        .record_turn(2, [], [])
        .expect("an empty settled turn is recorded");
    tracker
        .record_turn(3, [], [])
        .expect("another empty settled turn is recorded");

    assert_eq!(tracker.last_completed_turn(), Some(3));
    let restored: ToolOutputReferenceTracker =
        serde_json::from_value(serde_json::to_value(&tracker).expect("tracker serializes"))
            .expect("tracker restores");
    assert_eq!(restored.last_completed_turn(), Some(3));
}

#[test]
fn a_realistic_run_and_call_id_still_leaves_room_for_a_preview() {
    let run_id = RunId::new(REALISTIC_RUN);
    let call_id = CallId::new(REALISTIC_CALL);
    let large = "ripgrep match line ".repeat(60);
    let input = vec![
        call(call_id.as_str(), "grep"),
        structured_output(call_id.as_str(), large),
    ];
    let mut references = ToolOutputReferenceTracker::new(run_id.clone());
    references
        .record_turn(1, [call_id.clone()], [])
        .expect("output is registered");
    let trimmer =
        ToolOutputReferenceTrimmer::new(2, DEFAULT_MAX_OUTPUT_CHARS, DEFAULT_PREVIEW_CHARS)
            .expect("valid limits");

    let projected = trimmer
        .trim_model_input(&run_id, 9, &references, &input)
        .expect("projection succeeds");

    let output = structured(&projected[1]);
    let excerpt = output
        .model_excerpt()
        .expect("the default ceiling has room for the locator");
    let rendered = model_text(&output);
    assert!(
        rendered.contains(excerpt.artifact_ref().as_str()),
        "the locator naming the retained original must survive at the default ceiling"
    );
    assert!(
        rendered.contains("char preview]"),
        "readable preview text must survive alongside the locator"
    );
    assert!(rendered.chars().count() <= DEFAULT_MAX_OUTPUT_CHARS);
    assert!(
        rendered.chars().count() > REALISTIC_LOCATOR_CHARS,
        "a projection that is only the locator would mean the preview was squeezed out"
    );
}

#[test]
fn a_ceiling_too_small_for_the_locator_falls_back_to_a_bare_summary() {
    let run_id = RunId::new(REALISTIC_RUN);
    let call_id = CallId::new(REALISTIC_CALL);
    let large = "ripgrep match line ".repeat(60);
    let input = vec![
        call(call_id.as_str(), "grep"),
        structured_output(call_id.as_str(), large),
    ];
    let mut references = ToolOutputReferenceTracker::new(run_id.clone());
    references
        .record_turn(1, [call_id.clone()], [])
        .expect("output is registered");

    // One character below the locator plus the shortest marker it can accompany.
    let too_small =
        ToolOutputReferenceTrimmer::new(2, REALISTIC_LOCATOR_CHARS + 8, 40).expect("valid limits");
    let projected = too_small
        .trim_model_input(&run_id, 9, &references, &input)
        .expect("projection succeeds");
    assert!(
        raw_output(&projected[1]).is_string(),
        "a ceiling that cannot carry the locator degrades to a bare summary rather than \
         overrunning the ceiling"
    );

    // Exactly enough for the locator and the marker, and nothing more.
    let exact =
        ToolOutputReferenceTrimmer::new(2, REALISTIC_LOCATOR_CHARS + 9, 40).expect("valid limits");
    let projected = exact
        .trim_model_input(&run_id, 9, &references, &input)
        .expect("projection succeeds");
    let output = structured(&projected[1]);
    assert!(
        output.model_excerpt().is_some(),
        "one more character is the whole difference between keeping and losing the locator"
    );
    assert_eq!(
        model_text(&output).chars().count(),
        REALISTIC_LOCATOR_CHARS + 9
    );
}

#[test]
fn a_legacy_string_result_is_replaced_by_a_structured_one_carrying_its_locator() {
    let run_id = RunId::new("run-1");
    let call_id = CallId::new("call-legacy");
    let large = "legacy output ".repeat(60);
    let input = vec![
        call(call_id.as_str(), "grep"),
        legacy_output(call_id.as_str(), large.clone()),
    ];
    let mut references = ToolOutputReferenceTracker::new(run_id.clone());
    references
        .record_turn(1, [call_id.clone()], [])
        .expect("output is registered");
    let trimmer = ToolOutputReferenceTrimmer::new(2, 400, 60).expect("valid limits");

    let projected = trimmer
        .trim_model_input(&run_id, 9, &references, &input)
        .expect("projection succeeds");

    let output = structured(&projected[1]);
    assert_eq!(
        output
            .model_excerpt()
            .expect("a legacy result gains an excerpt so it can carry a locator")
            .artifact_ref()
            .as_str(),
        "tool-output/72756e2d31/63616c6c2d6c6567616379"
    );
    assert!(model_text(&output).starts_with("[Trimmed: grep output"));
    assert!(model_text(&output).chars().count() <= 400);
    assert_eq!(
        raw_output(&input[1]),
        &serde_json::Value::String(large),
        "the authoritative record keeps both the original payload and its original shape"
    );
}

#[test]
fn the_tracker_is_replay_safe_serializable_and_scoped_to_its_run() {
    let run_id = RunId::new("run-1");
    let call_id = CallId::new("call-1");
    let mut tracker = ToolOutputReferenceTracker::new(run_id.clone());
    tracker
        .record_turn(2, [call_id.clone()], [])
        .expect("output is registered");
    tracker
        .record_turn(7, [], [call_id.clone()])
        .expect("newer reference is recorded");
    tracker
        .record_turn(2, [call_id.clone()], [call_id.clone()])
        .expect("replay of the producing turn is idempotent");
    assert_eq!(tracker.last_referenced_turn(&call_id), Some(7));
    assert_eq!(
        tracker.schema_version(),
        TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION
    );
    assert!(
        tracker.record_turn(3, [call_id.clone()], []).is_err(),
        "a reused call ID cannot overwrite a different result"
    );

    let restored: ToolOutputReferenceTracker =
        serde_json::from_value(serde_json::to_value(&tracker).expect("tracker serializes"))
            .expect("tracker restores");
    assert_eq!(restored, tracker);
    assert!(
        serde_json::from_value::<ToolOutputReferenceTracker>(serde_json::json!({
            "run_id": "run-1",
            "records": {
                "call-corrupt": {
                    "created_turn": 8,
                    "last_referenced_turn": 7
                }
            }
        }))
        .is_err(),
        "a persisted reference cannot predate the output it claims to retain"
    );
    assert!(
        serde_json::from_value::<ToolOutputReferenceTracker>(serde_json::json!({
            "schema_version": 2,
            "run_id": "run-1",
            "last_completed_turn": 3,
            "records": {
                "call-1": {
                    "created_turn": 2,
                    "last_referenced_turn": 5
                }
            }
        }))
        .is_err(),
        "a completed-turn mark behind a retained reference would restart the axis onto turns the \
         ledger has already used"
    );

    let input = vec![
        call(call_id.as_str(), "search"),
        structured_output(call_id.as_str(), "x".repeat(500)),
    ];
    let trimmer = ToolOutputReferenceTrimmer::new(3, 180, 30).expect("valid limits");
    assert!(
        trimmer
            .trim_model_input(&RunId::new("other-run"), 11, &restored, &input)
            .is_err(),
        "a tracker cannot select results from another run"
    );
}

#[test]
fn a_persisted_ledger_carries_fields_a_newer_build_added_back_out_again() {
    let restored: ToolOutputReferenceTracker = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "run_id": "run-1",
        "records": {
            "call-1": {
                "created_turn": 2,
                "last_referenced_turn": 5,
                "evicted_at_turn": 9
            }
        },
        "archive_generation": 3
    }))
    .expect("an unknown field is not a reason to reject a checkpoint");

    assert_eq!(
        restored.last_completed_turn(),
        Some(5),
        "a checkpoint written before the completed-turn field recovers its best known high-water mark"
    );

    let round_tripped = serde_json::to_value(&restored).expect("tracker serializes");
    assert_eq!(
        round_tripped.pointer("/archive_generation"),
        Some(&serde_json::json!(3)),
        "a field this build cannot name must not be dropped on the way back out"
    );
    assert_eq!(
        round_tripped.pointer("/records/call-1/evicted_at_turn"),
        Some(&serde_json::json!(9)),
        "the same holds for a field added to an individual retention record"
    );
}

#[test]
fn record_turn_rejects_references_it_has_no_evidence_for() {
    let mut tracker = ToolOutputReferenceTracker::new(RunId::new("run-1"));
    let known = CallId::new("call-known");
    tracker
        .record_turn(4, [known.clone()], [])
        .expect("output is registered");

    assert!(
        tracker
            .record_turn(5, [], [CallId::new("call-never-seen")])
            .is_err(),
        "a reference naming no registered output is not evidence about a future result"
    );
    assert!(
        tracker.record_turn(2, [], [known.clone()]).is_err(),
        "a reference cannot predate the output it names"
    );
    assert!(
        tracker
            .record_turn(2, [CallId::new("call-late")], [])
            .is_err(),
        "a turn the ledger has moved past cannot introduce a result, which would be born stale"
    );
    assert_eq!(
        tracker.last_referenced_turn(&known),
        Some(4),
        "a rejected turn leaves the ledger untouched"
    );
    assert_eq!(
        tracker.last_referenced_turn(&CallId::new("call-late")),
        None
    );
}

#[test]
fn reference_trimmer_defaults_and_validation_are_explicit() {
    let trimmer = ToolOutputReferenceTrimmer::default();

    assert_eq!(
        trimmer.max_unreferenced_turns(),
        DEFAULT_MAX_UNREFERENCED_TURNS
    );
    assert_eq!(trimmer.max_output_chars(), DEFAULT_MAX_OUTPUT_CHARS);
    assert_eq!(trimmer.preview_chars(), DEFAULT_PREVIEW_CHARS);
    assert_eq!(
        trimmer,
        ToolOutputReferenceTrimmer::new(
            DEFAULT_MAX_UNREFERENCED_TURNS,
            DEFAULT_MAX_OUTPUT_CHARS,
            DEFAULT_PREVIEW_CHARS
        )
        .expect("the module defaults are a valid configuration"),
        "a field this type neither reads nor exposes must not make two equal configurations differ"
    );
    assert!(ToolOutputReferenceTrimmer::new(0, 100, 20).is_err());
}
