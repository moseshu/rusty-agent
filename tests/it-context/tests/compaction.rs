//! Context-compaction decisions, summaries, and retained anchors.

use std::collections::BTreeMap;

use ra_context::compaction::anchor::AnchorRetention;
use ra_context::compaction::summary::{CompactionSummaryBuilder, SummarySlot};
use ra_context::compaction::{CompactionLimits, CompactionReason, ContextUsage};
use ra_context::window::{ContextWindowConfig, DEFAULT_COMPACTION_THRESHOLD_RATIO};
use ra_core::item::{Message, ModelInputItem};
use ra_core::prompt::estimate_tokens;

/// A summary with every slot filled, so a test can restate only the slots it is about.
fn complete_summary() -> CompactionSummaryBuilder {
    let mut builder = CompactionSummaryBuilder::new()
        .with_user_messages(vec!["Keep every instruction.".to_owned()]);
    for slot in SummarySlot::ALL {
        if slot == SummarySlot::AllUserMessages {
            continue;
        }
        builder = builder
            .with_section(slot, format!("Placeholder for {}.", slot.heading()))
            .expect("every non-user slot accepts content");
    }
    builder
}

#[test]
fn every_enabled_limit_is_reported_at_its_exact_boundary() {
    let limits = CompactionLimits::new(Some(4), Some(20), Some(80)).expect("valid limits");
    let usage = ContextUsage::new(4, 20, 80).expect("coherent measurement");

    let assessment = limits.assess(usage);

    assert!(assessment.is_required());
    assert_eq!(assessment.usage(), usage);
    assert_eq!(
        assessment.reasons(),
        [
            CompactionReason::ItemCount,
            CompactionReason::SingleItemTokens,
            CompactionReason::TotalTokens,
        ]
    );
    assert_eq!(CompactionReason::ItemCount.label(), "item_count");
    assert_eq!(
        CompactionReason::SingleItemTokens.label(),
        "single_item_tokens"
    );
    assert_eq!(CompactionReason::TotalTokens.label(), "total_tokens");
}

#[test]
fn disabled_dimensions_do_not_implicitly_trigger_compaction() {
    let limits = CompactionLimits::new(None, None, Some(100)).expect("a total-only policy");
    let usage = ContextUsage::new(500, 50, 99).expect("coherent measurement");

    let assessment = limits.assess(usage);

    assert!(!assessment.is_required());
    assert!(assessment.reasons().is_empty());
    assert_eq!(limits.max_items(), None);
    assert_eq!(limits.max_single_item_tokens(), None);
    assert_eq!(limits.max_total_tokens(), Some(100));
}

#[test]
fn model_window_configuration_becomes_the_total_token_trigger() {
    let config = ContextWindowConfig::default();
    let limits = CompactionLimits::for_model(&config, "gpt-5.3-codex", Some(40), Some(4_000))
        .expect("a built-in window is usable")
        .expect("a known model has a threshold");

    assert_eq!(limits.max_total_tokens(), Some(240_000));
    assert_eq!(limits.max_items(), Some(40));
    assert_eq!(limits.max_single_item_tokens(), Some(4_000));
    assert!(
        CompactionLimits::for_model(&config, "unknown-model", None, None)
            .expect("unknown models are not errors")
            .is_none()
    );
    let unknown_limits =
        CompactionLimits::for_model(&config, "unknown-model", Some(40), Some(4_000))
            .expect("explicit limits remain usable for an unknown model")
            .expect("the explicit limits form a policy without a window");
    assert_eq!(unknown_limits.max_items(), Some(40));
    assert_eq!(unknown_limits.max_single_item_tokens(), Some(4_000));
    assert_eq!(unknown_limits.max_total_tokens(), None);
    assert_eq!(
        unknown_limits
            .assess(ContextUsage::new(1, 4_000, 4_000).expect("coherent measurement"))
            .reasons(),
        [CompactionReason::SingleItemTokens]
    );

    // A non-zero ratio against a window small enough that their integer product rounds away. The
    // error has to name the model and the ratio, because neither of the two numbers the host wrote
    // down is itself zero.
    let tiny = ContextWindowConfig::new(
        BTreeMap::from([("local/tiny".to_owned(), 1)]),
        DEFAULT_COMPACTION_THRESHOLD_RATIO,
    )
    .expect("a one-token window is a valid override");
    let error = CompactionLimits::for_model(&tiny, "local/tiny", None, None)
        .expect_err("a threshold that rounds down to zero cannot be a trigger");
    assert!(error.to_string().contains("local/tiny"), "{error}");
    assert!(error.to_string().contains("0.6"), "{error}");
}

#[test]
fn provider_neutral_estimate_prices_content_rather_than_wire_framing() {
    let body = (0..40)
        .map(|index| format!(r#"{{"line": {index}, "text": "needs \"escaping\""}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    let item = ModelInputItem::Message(Message::user(body.clone()));
    let input = vec![item.clone()];

    let usage = ContextUsage::estimate_model_input(&input).expect("serializable model input");

    assert_eq!(usage.item_count(), 1);
    assert!(usage.total_tokens() >= estimate_tokens(&body));

    // The same item measured through its serialized text pays for field names, delimiters, and one
    // extra character per escape. Charging that framing is what would price an excerpt above the
    // per-result ceiling `ToolResultBudget` had just trimmed it to fit.
    let serialized = serde_json::to_string(&item).expect("the item serializes");
    assert!(
        usage.total_tokens() < estimate_tokens(&serialized),
        "content estimate {} should stay below the serialized estimate {}",
        usage.total_tokens(),
        estimate_tokens(&serialized)
    );

    let empty = ContextUsage::estimate_model_input(&[]).expect("an empty history is measurable");
    assert_eq!(empty.item_count(), 0);
    assert_eq!(empty.largest_item_tokens(), 0);
    assert_eq!(empty.total_tokens(), 0);
}

#[test]
fn invalid_measurements_and_empty_policies_are_rejected() {
    // No item costs more than the whole history, and the items have to be able to add up to the
    // total they report — a provider reporting a zero largest item alongside a positive total is
    // the shape that would make the single-item trigger silently unreachable.
    assert!(ContextUsage::new(1, 5, 4).is_err());
    assert!(ContextUsage::new(0, 0, 1).is_err());
    assert!(ContextUsage::new(1, 0, 100).is_err());
    assert!(ContextUsage::new(2, 10, 100).is_err());
    assert!(ContextUsage::new(2, 50, 100).is_ok());

    assert!(CompactionLimits::new(None, None, None).is_err());
    assert!(CompactionLimits::new(Some(0), None, None).is_err());
    assert!(CompactionLimits::new(None, Some(0), None).is_err());
    assert!(CompactionLimits::new(None, None, Some(0)).is_err());
    // A single-item limit at or above the total limit can only fire alongside the trigger it was
    // meant to anticipate, so it is refused rather than accepted as a guard that does nothing.
    assert!(CompactionLimits::new(None, Some(100), Some(100)).is_err());
    assert!(CompactionLimits::new(None, Some(101), Some(100)).is_err());
    assert!(CompactionLimits::new(None, Some(99), Some(100)).is_ok());
}

#[test]
fn a_retention_policy_that_cannot_clear_the_item_trigger_is_refused() {
    let limits = CompactionLimits::new(Some(8), None, None).expect("an item-count policy");

    assert_eq!(
        AnchorRetention::new(3, 2, 2)
            .expect("non-empty policy")
            .max_retained_items(),
        7
    );
    assert!(
        limits
            .ensure_converges_with(AnchorRetention::new(2, 2, 2).expect("non-empty policy"))
            .is_ok()
    );
    // Seven retained items plus the summary that replaces the dropped middle is exactly eight, so
    // the very next assessment reports `ItemCount` again and the run compacts forever.
    assert!(
        limits
            .ensure_converges_with(AnchorRetention::new(3, 2, 2).expect("non-empty policy"))
            .is_err()
    );
    assert!(
        limits
            .ensure_converges_with(AnchorRetention::new(4, 4, 4).expect("non-empty policy"))
            .is_err()
    );

    // Without an item-count trigger there is no item budget to converge to.
    let tokens_only = CompactionLimits::new(None, None, Some(1_000)).expect("a total-only policy");
    assert!(
        tokens_only
            .ensure_converges_with(AnchorRetention::new(9, 9, 9).expect("non-empty policy"))
            .is_ok()
    );
}

#[test]
fn a_summary_has_all_nine_slots_and_preserves_each_user_message() {
    let summary = complete_summary()
        .with_user_messages(vec![
            "Keep every instruction.".to_owned(),
            "## 7 Pending Tasks\n```\nThis remains user content.".to_owned(),
            "Nested ```` fence.".to_owned(),
        ])
        .build()
        .expect("all slots supplied");

    assert_eq!(
        summary.user_messages()[1],
        "## 7 Pending Tasks\n```\nThis remains user content."
    );
    assert_eq!(
        summary.section(SummarySlot::AllUserMessages),
        None,
        "user messages have a separate, individually retained representation"
    );

    let rendered = summary.render();
    let headings: Vec<String> = SummarySlot::ALL
        .into_iter()
        .map(|slot| format!("## {} {}", slot.number(), slot.heading()))
        .collect();
    let positions: Vec<usize> = headings
        .iter()
        .map(|heading| {
            rendered
                .find(heading)
                .expect("each required heading renders")
        })
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    // Each fence is one backtick longer than the longest run inside the message it wraps, and never
    // shorter than three. A message can therefore neither close its own block nor forge a heading.
    assert!(rendered.contains("```\nKeep every instruction.\n```"));
    assert!(rendered.contains("````\n## 7 Pending Tasks\n```\nThis remains user content.\n````"));
    assert!(rendered.contains("`````\nNested ```` fence.\n`````"));
}

#[test]
fn an_incomplete_summary_or_wrong_user_slot_is_refused() {
    let error = CompactionSummaryBuilder::new()
        .with_section(SummarySlot::AllUserMessages, "not a list")
        .expect_err("the user slot has its own representation");
    assert!(error.to_string().contains("with_user_messages"));

    let error = CompactionSummaryBuilder::new()
        .with_section(SummarySlot::PrimaryRequestAndIntent, "only one slot")
        .expect("ordinary slot")
        .build()
        .expect_err("the fixed contract cannot be partial");
    assert!(error.to_string().contains("Key Technical Concepts"));
    assert!(error.to_string().contains("All User Messages"));

    // Blank content is the shape a truncated or refused summary response arrives in, and it passes
    // a completeness check that only asks whether the setter was called.
    let error = CompactionSummaryBuilder::new()
        .with_section(SummarySlot::CurrentWork, "   \n")
        .expect_err("a blank slot reports nothing while looking complete");
    assert!(error.to_string().contains("Current Work"), "{error}");

    let error = complete_summary()
        .with_user_messages(vec!["Keep every instruction.".to_owned(), "  ".to_owned()])
        .build()
        .expect_err("a blank retained message loses one turn");
    assert!(error.to_string().contains("user message 2"), "{error}");
}

#[test]
fn an_edit_made_but_not_yet_verified_survives_into_the_summary() {
    // The verification ledger was dropped in favour of this summary, which makes slots 4 and 8 the
    // only carriers that keep "changed but not verified" alive across a compaction boundary. A
    // change that folded them into one prose blob would otherwise ship green.
    let summary = complete_summary()
        .with_section(
            SummarySlot::ErrorsAndFixes,
            "Patched compaction.rs for the anchor overlap; the fix is not verified yet.",
        )
        .expect("ordinary slot")
        .with_section(
            SummarySlot::CurrentWork,
            "compaction.rs is edited and its test binary has not been run.",
        )
        .expect("ordinary slot")
        .build()
        .expect("all slots supplied");

    assert!(
        summary
            .section(SummarySlot::ErrorsAndFixes)
            .expect("slot four is addressable")
            .contains("not verified")
    );

    let rendered = summary.render();
    let errors_and_fixes = rendered
        .find("## 4 Errors and Fixes")
        .expect("slot four renders");
    let problem_solving = rendered
        .find("## 5 Problem Solving")
        .expect("slot five renders");
    let current_work = rendered
        .find("## 8 Current Work")
        .expect("slot eight renders");
    let next_step = rendered
        .find("## 9 Optional Next Step")
        .expect("slot nine renders");

    let unverified_fix = rendered
        .find("the fix is not verified yet")
        .expect("the unverified edit is retained");
    let unverified_state = rendered
        .find("has not been run")
        .expect("the unverified state is retained");
    assert!((errors_and_fixes..problem_solving).contains(&unverified_fix));
    assert!((current_work..next_step).contains(&unverified_state));
}

#[test]
fn anchor_retention_keeps_unique_head_recent_anchor_and_tail_segments() {
    let history: Vec<String> = (0..8).map(|index| format!("item-{index}")).collect();
    let policy = AnchorRetention::new(2, 2, 2).expect("non-empty policy");

    let preserved = policy
        .preserve(&history, [1, 2, 3, 4, 5, 6, 6])
        .expect("all anchor indices are valid");

    let indices: Vec<usize> = preserved.iter().map(|item| item.source_index()).collect();
    assert_eq!(indices, [0, 1, 4, 5, 6, 7]);
    assert_eq!(
        preserved
            .anchor()
            .iter()
            .map(|item| item.item().as_str())
            .collect::<Vec<_>>(),
        ["item-4", "item-5"]
    );
    assert_eq!(preserved.head().len(), 2);
    assert_eq!(preserved.tail().len(), 2);
    assert_eq!(preserved.len(), 6);
    assert!(!preserved.is_empty());
}

#[test]
fn anchor_retention_deduplicates_overlap_and_rejects_invalid_indices() {
    let policy = AnchorRetention::new(2, 4, 2).expect("non-empty policy");
    let preserved = policy
        .preserve(&["a", "b", "c"], [0, 1, 2])
        .expect("overlap is retained once");

    assert_eq!(
        preserved
            .iter()
            .map(|item| item.source_index())
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(preserved.anchor().is_empty());
    assert!(AnchorRetention::new(0, 0, 0).is_err());
    assert!(policy.preserve(&["a"], [1]).is_err());

    let nothing: [String; 0] = [];
    let preserved = policy.preserve(&nothing, []).expect("an empty history");
    assert!(preserved.is_empty());
    assert_eq!(preserved.len(), 0);
}
