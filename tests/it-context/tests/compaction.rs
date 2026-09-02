//! Context-compaction decisions, summaries, and retained anchors.

use std::collections::BTreeMap;

use ra_context::compaction::anchor::{AnchorRetention, DEFAULT_TAIL_ITEMS};
use ra_context::compaction::summary::{CompactionSummaryBuilder, SummarySlot};
use ra_context::compaction::{
    CompactedModelInput, CompactionLimits, CompactionPolicy, CompactionReason, ContextUsage,
    project_compacted_model_input,
};
use ra_context::window::{ContextWindowConfig, DEFAULT_COMPACTION_THRESHOLD_RATIO};
use ra_core::item::{
    CallId, Compaction, ItemId, McpApprovalRequest, McpApprovalResponse, Message, ModelInputItem,
    OutputPhase, Reasoning, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
};
use ra_core::prompt::estimate_tokens;

/// A projection policy whose only trigger is a total-token ceiling.
///
/// Every retention converges with such a trigger, so a projection test states only the retention
/// shape it is about. Convergence itself is covered separately.
fn policy(head: usize, anchors: usize, tail: usize) -> CompactionPolicy {
    CompactionPolicy::new(
        CompactionLimits::new(None, None, Some(4_000)).expect("a total-token trigger"),
        AnchorRetention::new(head, anchors, tail).expect("a non-empty retention policy"),
    )
    .expect("a total-token trigger converges with any retention")
}

fn user_message(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}

fn labels(compacted: &CompactedModelInput) -> Vec<&'static str> {
    compacted
        .items()
        .iter()
        .map(ModelInputItem::label)
        .collect()
}

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
fn default_compaction_capability_uses_a_bounded_recent_working_set() {
    let capability = ra_context::compaction::CompactionCapability::default();

    assert_eq!(capability.context_windows(), &ContextWindowConfig::default());
    assert_eq!(capability.retention(), AnchorRetention::default());
    assert_eq!(capability.retention().head_items(), 0);
    assert_eq!(capability.retention().max_anchor_items(), 0);
    assert_eq!(capability.retention().tail_items(), DEFAULT_TAIL_ITEMS);
    assert_eq!(capability.max_items(), None);
    assert_eq!(capability.max_single_item_tokens(), None);
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

#[test]
fn compacted_model_input_is_a_projection_and_never_summarizes_control_records() {
    let history = vec![
        RunItem::new(
            ItemId::new("message-1"),
            RunItemKind::Message(Message::user("first request")),
        ),
        RunItem::new(
            ItemId::new("approval-1"),
            RunItemKind::ToolApproval(ToolApproval::new(
                CallId::new("call-1"),
                "exec_command",
                serde_json::json!({"cmd": "git status"}),
            )),
        ),
        RunItem::new(
            ItemId::new("message-2"),
            RunItemKind::Message(Message::assistant("working", OutputPhase::Commentary)),
        ),
        RunItem::new(
            ItemId::new("message-3"),
            RunItemKind::Message(Message::assistant("latest", OutputPhase::Commentary)),
        ),
    ];

    let compacted = project_compacted_model_input(
        &history,
        policy(1, 0, 1),
        [],
        "A durable summary of the omitted work.",
    )
    .expect("the middle model item is replaceable");

    assert_eq!(
        compacted.compacted_item_ids(),
        &[ItemId::new("message-2")],
        "the approval stays a control-plane record rather than becoming summary content"
    );
    assert_eq!(
        labels(&compacted),
        ["message", "compaction", "message"],
        "the approval is not emitted, and it did not spend the head or tail budget either"
    );
    assert_eq!(
        compacted.items()[1],
        ModelInputItem::Compaction(Compaction::new(
            "A durable summary of the omitted work.",
            vec![ItemId::new("message-2")],
        ))
    );

    // Retention is counted in model-visible records, so the three messages fill a head of three.
    let nothing_to_replace =
        project_compacted_model_input(&history, policy(3, 0, 0), [], "Unused summary.");
    assert!(nothing_to_replace.is_err());
    let blank_summary = project_compacted_model_input(&history, policy(1, 0, 1), [], " \n ");
    assert!(blank_summary.is_err());
}

#[test]
fn compacted_model_input_keeps_a_pending_hosted_approval_request() {
    let approval = McpApprovalRequest::new(
        "approval-1",
        "filesystem",
        "delete_file",
        serde_json::json!({"path": "obsolete.txt"}),
    );
    let history = vec![
        user_message("message-0", "clean up the temporary file"),
        RunItem::new(
            ItemId::new("mcp-approval-1"),
            RunItemKind::McpApprovalRequest(approval.clone()),
        ),
        user_message("message-1", "please keep the approval request"),
        user_message("message-2", "what remains to be done?"),
    ];

    let compacted = project_compacted_model_input(
        &history,
        policy(1, 0, 1),
        [],
        "The unretained message is summarized.",
    )
    .expect("the middle message is replaceable");

    assert_eq!(
        labels(&compacted),
        ["message", "compaction", "mcp_approval_request", "message"],
        "the pending hosted approval survives after the summary"
    );
    assert_eq!(
        compacted.items()[2],
        ModelInputItem::McpApprovalRequest(approval),
        "the provider receives the concrete approval protocol item"
    );
    assert_eq!(
        compacted.compacted_item_ids(),
        &[ItemId::new("message-1")],
        "the hosted approval is not claimed by the summary"
    );
}

#[test]
fn an_answered_hosted_approval_is_replaced_together_with_its_response() {
    let history = vec![
        user_message("message-0", "clean up the temporary file"),
        RunItem::new(
            ItemId::new("mcp-request-1"),
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "approval-1",
                "filesystem",
                "delete_file",
                serde_json::json!({"path": "obsolete.txt"}),
            )),
        ),
        RunItem::new(
            ItemId::new("mcp-response-1"),
            RunItemKind::McpApprovalResponse(McpApprovalResponse::new("approval-1", true)),
        ),
        user_message("message-1", "filler"),
        user_message("message-2", "what remains to be done?"),
    ];

    let compacted = project_compacted_model_input(
        &history,
        policy(1, 0, 1),
        [],
        "The approved deletion and the work after it.",
    )
    .expect("the answered approval and the filler are replaceable");

    // Pinning the request because its kind is an interruption would tell the server an approval is
    // still open that this run granted turns ago, and would leave the history a floor it can never
    // compact below.
    assert_eq!(
        labels(&compacted),
        ["message", "compaction", "message"],
        "an answered request is an ordinary record once its response exists"
    );
    assert_eq!(
        compacted.compacted_item_ids(),
        &[
            ItemId::new("mcp-request-1"),
            ItemId::new("mcp-response-1"),
            ItemId::new("message-1"),
        ],
        "the request and the answer are represented by the same summary"
    );

    // The reverse split is closed too. A tail landing on the response alone does not pull the
    // request back into retention — widening only ever goes the other way, as it does for a tool
    // result whose call was replaced — so the response joins its request in the summary.
    let tail_lands_on_the_response =
        project_compacted_model_input(&history, policy(1, 0, 3), [], "The approved deletion.")
            .expect("the answered approval is replaceable");
    assert_eq!(
        labels(&tail_lands_on_the_response),
        ["message", "compaction", "message", "message"],
        "a response never travels without the request it answers"
    );
    assert_eq!(
        tail_lands_on_the_response.compacted_item_ids(),
        &[ItemId::new("mcp-request-1"), ItemId::new("mcp-response-1")]
    );
}

#[test]
fn compacted_anchors_are_retained_verbatim_after_the_summary() {
    let history: Vec<RunItem> = (0..7)
        .map(|index| user_message(&format!("message-{index}"), &format!("turn {index}")))
        .collect();

    let compacted = project_compacted_model_input(
        &history,
        policy(1, 1, 1),
        [3],
        "Everything between the opening turn and the latest one.",
    )
    .expect("the unanchored middle is replaceable");

    assert_eq!(
        labels(&compacted),
        ["message", "compaction", "message", "message"],
        "head, then the summary, then the anchor and the tail"
    );
    assert_eq!(
        compacted.items()[2],
        ModelInputItem::Message(Message::user("turn 3")),
        "the anchored middle turn survives verbatim rather than being summarized"
    );
    assert_eq!(
        compacted.compacted_item_ids(),
        &[
            ItemId::new("message-1"),
            ItemId::new("message-2"),
            ItemId::new("message-4"),
            ItemId::new("message-5"),
        ],
        "the anchor is excluded from the summary's coverage on both sides of it"
    );
}

#[test]
fn compacted_model_input_never_strands_a_result_from_its_call() {
    let history = vec![
        user_message("message-0", "run the tests"),
        RunItem::new(
            ItemId::new("call-1"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "run_tests",
                serde_json::json!({"attempt": 1}),
            )),
        ),
        RunItem::new(
            ItemId::new("output-1"),
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                serde_json::json!({"ok": false}),
            )),
        ),
        RunItem::new(
            ItemId::new("call-2"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-2"),
                "run_tests",
                serde_json::json!({"attempt": 2}),
            )),
        ),
        RunItem::new(
            ItemId::new("output-2"),
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-2"),
                serde_json::json!({"ok": false}),
            )),
        ),
    ];

    // A tail of one lands mid-pair. Emitting the retained output alone would be a `tool_result`
    // with no preceding `tool_use`, which the Anthropic adapter refuses to build a request from.
    let split = project_compacted_model_input(
        &history,
        policy(0, 0, 1),
        [],
        "Both attempts failed the same way.",
    )
    .expect("a replaced call carries its stranded result into the summary");
    assert_eq!(labels(&split), ["compaction"]);
    assert_eq!(
        split.compacted_item_ids(),
        &[
            ItemId::new("message-0"),
            ItemId::new("call-1"),
            ItemId::new("output-1"),
            ItemId::new("call-2"),
            ItemId::new("output-2"),
        ],
        "the stranded result is represented by the summary rather than dropped silently"
    );

    // A tail that covers the whole pair keeps it, and keeps it adjacent.
    let intact =
        project_compacted_model_input(&history, policy(0, 0, 2), [], "The first attempt failed.")
            .expect("the older pair is replaceable");
    assert_eq!(
        labels(&intact),
        ["compaction", "tool_call", "tool_call_output"]
    );
    assert_eq!(
        intact.compacted_item_ids(),
        &[
            ItemId::new("message-0"),
            ItemId::new("call-1"),
            ItemId::new("output-1"),
        ]
    );
}

#[test]
fn compacted_model_input_replaces_a_reasoning_item_whose_follower_is_gone() {
    let history = vec![
        user_message("message-0", "explain the failure"),
        RunItem::new(
            ItemId::new("reasoning-1"),
            RunItemKind::Reasoning(Reasoning::new().with_id("rs_1")),
        ),
        RunItem::new(
            ItemId::new("message-1"),
            RunItemKind::Message(Message::assistant(
                "because of the lock",
                OutputPhase::Commentary,
            )),
        ),
        user_message("message-2", "and now?"),
        RunItem::new(
            ItemId::new("message-3"),
            RunItemKind::Message(Message::assistant("retrying", OutputPhase::Commentary)),
        ),
    ];

    // The head ends on the reasoning item while the assistant turn it belongs to is replaced.
    // The inserted summary reads as a valid follower to `InputItemNormalizer`, so nothing
    // downstream would catch this; the projection has to resolve it here.
    let compacted = project_compacted_model_input(
        &history,
        policy(2, 0, 1),
        [],
        "The assistant explained the lock contention.",
    )
    .expect("the middle turns are replaceable");

    assert_eq!(
        labels(&compacted),
        ["message", "compaction", "message"],
        "a reasoning item cannot outlive the turn it belongs to"
    );
    assert_eq!(
        compacted.compacted_item_ids(),
        &[
            ItemId::new("reasoning-1"),
            ItemId::new("message-1"),
            ItemId::new("message-2"),
        ]
    );
}

#[test]
fn a_summary_inherits_the_coverage_of_the_summary_it_replaces() {
    let history = vec![
        RunItem::new(
            ItemId::new("summary-1"),
            RunItemKind::Compaction(Compaction::new(
                "The opening investigation.",
                vec![ItemId::new("archived-1"), ItemId::new("archived-2")],
            )),
        ),
        user_message("message-1", "keep going"),
        user_message("message-2", "still going"),
        user_message("message-3", "latest"),
    ];

    let compacted = project_compacted_model_input(
        &history,
        policy(0, 0, 1),
        [],
        "The investigation and everything after it.",
    )
    .expect("the earlier summary is itself replaceable");

    assert_eq!(
        compacted.compacted_item_ids(),
        &[
            ItemId::new("archived-1"),
            ItemId::new("archived-2"),
            ItemId::new("summary-1"),
            ItemId::new("message-1"),
            ItemId::new("message-2"),
        ],
        "records the first summary stood for stay covered once it is replaced in turn"
    );
}

#[test]
fn a_compaction_policy_refuses_a_retention_that_cannot_clear_its_item_trigger() {
    let trigger = CompactionLimits::new(Some(4), None, None).expect("an item trigger");
    assert!(
        CompactionPolicy::new(
            trigger,
            AnchorRetention::new(3, 2, 2).expect("non-empty policy"),
        )
        .is_err(),
        "seven retained items plus a summary never fall below a four-item trigger"
    );
    assert!(
        CompactionPolicy::new(
            trigger,
            AnchorRetention::new(1, 0, 1).expect("non-empty policy"),
        )
        .is_ok()
    );
}
