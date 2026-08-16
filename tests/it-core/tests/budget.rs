//! Contracts for protocol-neutral budget values.

use std::time::Duration;

use ra_core::{
    budget::{BudgetLimit, BudgetSnapshot},
    cancel::Deadline,
    error::BudgetKind,
    usage::Usage,
};

#[test]
fn snapshot_tracks_usage_and_reports_remaining_allowance() {
    let limit = BudgetLimit::new().with_max_turns(3).with_max_tokens(100);
    let mut snapshot = BudgetSnapshot::new();

    snapshot.record_turn();
    snapshot.record_usage(&Usage::new(40, 20));

    assert_eq!(snapshot.turns_used(), 1);
    assert_eq!(snapshot.tokens_used(), 60);
    assert_eq!(snapshot.remaining_turns(&limit), Some(2));
    assert_eq!(snapshot.remaining_tokens(&limit), Some(40));
    assert_eq!(snapshot.exhausted_kind(&limit), None);
}

#[test]
fn snapshot_uses_a_stable_priority_when_multiple_limits_are_exhausted() {
    let limit = BudgetLimit::new()
        .with_max_turns(1)
        .with_max_tokens(1)
        .with_deadline(Deadline::after(Duration::ZERO));
    let mut snapshot = BudgetSnapshot::new();

    snapshot.record_turn();
    snapshot.record_usage(&Usage::new(1, 0));

    assert_eq!(snapshot.exhausted_kind(&limit), Some(BudgetKind::MaxTurns));
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
    let snapshot = BudgetSnapshot::new();

    assert_eq!(limit.remaining_wall_clock(), Some(Duration::ZERO));
    assert_eq!(snapshot.exhausted_kind(&limit), Some(BudgetKind::WallClock));
    assert_eq!(
        snapshot.exhausted_kind(&BudgetLimit::new().with_max_turns(1)),
        None
    );
}

/// A checkpoint carries spend and preserves newer fields. The wall clock is not missing from it —
/// it never lived here, because a monotonic instant cannot be restored anyway.
#[test]
fn a_checkpoint_round_trip_preserves_the_whole_snapshot() {
    let mut snapshot = BudgetSnapshot::new();
    snapshot.record_turn();
    snapshot.record_usage(&Usage::new(4, 6));

    let clean = serde_json::to_value(&snapshot).unwrap();
    let restored: BudgetSnapshot = serde_json::from_value(clean).unwrap();
    assert_eq!(restored, snapshot);

    let mut value = serde_json::to_value(&snapshot).unwrap();
    value["future_reservation"] = serde_json::json!({ "tokens": 9 });
    let restored: BudgetSnapshot = serde_json::from_value(value).unwrap();

    assert_eq!(restored.turns_used(), 1);
    assert_eq!(restored.tokens_used(), 10);
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
