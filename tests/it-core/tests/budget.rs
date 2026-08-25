//! Contracts for protocol-neutral budget values.

use std::time::Duration;

use ra_core::{
    budget::{BudgetLimit, BudgetSnapshot},
    cancel::Deadline,
    error::BudgetKind,
    state::{RunId, RunState},
    usage::{RequestUsage, Usage},
};

fn state_with(turns: u32, usage: &Usage) -> RunState {
    let mut state = RunState::start(RunId::new("run-budget"));
    for _ in 0..turns {
        state.budget_mut().record_turn();
    }
    state.record_usage(usage);
    state
}

#[test]
fn snapshot_tracks_turns_and_reports_remaining_allowance() {
    let limit = BudgetLimit::new().with_max_turns(3).with_max_tokens(100);
    let state = state_with(1, &Usage::from_request(RequestUsage::new(40, 20)));

    assert_eq!(state.budget().turns_used(), 1);
    assert_eq!(state.tokens_used(), 60);
    assert_eq!(state.budget().remaining_turns(&limit), Some(2));
    assert_eq!(state.remaining_tokens(&limit), Some(40));
    assert_eq!(state.exhausted_budget_kind(&limit), None);
}

/// The token dimension is answered from the usage ledger, not from a counter of the budget's own.
/// There is deliberately no way to advance one without the other, because the day they disagree is
/// the day a run either overspends or stops with allowance left.
#[test]
fn the_token_ceiling_is_measured_against_the_usage_ledger() {
    let limit = BudgetLimit::new().with_max_turns(9).with_max_tokens(50);
    let mut state = state_with(1, &Usage::from_request(RequestUsage::new(30, 10)));

    assert_eq!(state.exhausted_budget_kind(&limit), None);

    state.record_usage(&Usage::from_request(RequestUsage::new(8, 4)));

    assert_eq!(state.tokens_used(), state.usage_totals().total_tokens());
    assert_eq!(
        state.exhausted_budget_kind(&limit),
        Some(BudgetKind::Tokens)
    );
    assert_eq!(state.remaining_tokens(&limit), Some(0));
}

#[test]
fn a_stable_priority_decides_which_limit_is_reported_when_several_are_exhausted() {
    let limit = BudgetLimit::new()
        .with_max_turns(1)
        .with_max_tokens(1)
        .with_deadline(Deadline::after(Duration::ZERO));
    let state = state_with(1, &Usage::from_request(RequestUsage::new(1, 0)));

    assert_eq!(
        state.exhausted_budget_kind(&limit),
        Some(BudgetKind::MaxTurns)
    );
}

#[test]
fn zero_ceilings_are_rejected_before_a_run_starts() {
    let error = BudgetLimit::new()
        .with_max_tokens(0)
        .validate()
        .unwrap_err();

    assert!(error.to_string().contains("max_tokens"));
}

#[test]
fn an_expired_wall_clock_is_read_from_the_limit_that_set_it() {
    let limit = BudgetLimit::new().with_deadline(Deadline::after(Duration::ZERO));
    let state = state_with(0, &Usage::default());

    assert_eq!(limit.remaining_wall_clock(), Some(Duration::ZERO));
    assert_eq!(
        state.exhausted_budget_kind(&limit),
        Some(BudgetKind::WallClock)
    );
    assert_eq!(
        state.exhausted_budget_kind(&BudgetLimit::new().with_max_turns(1)),
        None
    );
}

/// A checkpoint carries spend and preserves newer fields. The wall clock is not missing from it —
/// it never lived here, because a monotonic instant cannot be restored anyway.
#[test]
fn a_checkpoint_round_trip_preserves_the_whole_snapshot() {
    let mut snapshot = BudgetSnapshot::new();
    snapshot.record_turn();

    let clean = serde_json::to_value(&snapshot).unwrap();
    let restored: BudgetSnapshot = serde_json::from_value(clean).unwrap();
    assert_eq!(restored, snapshot);

    let mut value = serde_json::to_value(&snapshot).unwrap();
    value["future_reservation"] = serde_json::json!({ "tokens": 9 });
    let restored: BudgetSnapshot = serde_json::from_value(value).unwrap();

    assert_eq!(restored.turns_used(), 1);
    assert_eq!(restored.schema_version(), snapshot.schema_version());
    assert_eq!(
        restored.unknown().get("future_reservation"),
        Some(&serde_json::json!({ "tokens": 9 }))
    );
    assert_eq!(
        serde_json::to_value(restored).unwrap()["future_reservation"],
        serde_json::json!({ "tokens": 9 })
    );
}

/// Token spend recorded by a build that counted it on the snapshot is **still spend**. It moves
/// into the ledger when the snapshot is attached, so a continuation is measured against what the
/// earlier segment actually used rather than starting its allowance over.
#[test]
fn a_snapshot_written_when_the_budget_counted_tokens_carries_that_spend_into_the_ledger() {
    let legacy = serde_json::json!({
        "schema_version": 1,
        "turns_used": 2,
        "tokens_used": 4_000
    });

    let restored: BudgetSnapshot = serde_json::from_value(legacy).unwrap();
    // A host may checkpoint the snapshot on its own before attaching it to a `RunState`. Its spend
    // must survive that round trip; otherwise the later migration has nothing left to move.
    let standalone = serde_json::to_value(&restored).unwrap();
    assert_eq!(standalone["tokens_used"], serde_json::json!(4_000));
    let restored: BudgetSnapshot = serde_json::from_value(standalone).unwrap();

    let state = RunState::start(RunId::new("run-legacy")).with_budget(restored);

    assert_eq!(state.budget().turns_used(), 2);
    assert_eq!(state.tokens_used(), 4_000);
    assert_eq!(state.usage_totals().carried_total_tokens(), 4_000);

    // The ceiling sees it, which is the point: without the migration this run would be handed a
    // second full allowance.
    let limit = BudgetLimit::new().with_max_turns(9).with_max_tokens(4_000);
    assert_eq!(
        state.exhausted_budget_kind(&limit),
        Some(BudgetKind::Tokens)
    );

    // Carried spend has no known split, so it stays out of the counters a cache hit rate divides
    // by rather than being attributed to input tokens nobody measured.
    assert_eq!(state.usage_totals().input_tokens(), 0);
    assert_eq!(state.usage_totals().output_tokens(), 0);
    assert_eq!(state.usage_totals().requests(), 0);

    // Taking the spend into the ledger consumes the legacy carrier, so a current snapshot no
    // longer emits the obsolete field.
    assert!(
        serde_json::to_value(state.budget())
            .unwrap()
            .get("tokens_used")
            .is_none()
    );
}
