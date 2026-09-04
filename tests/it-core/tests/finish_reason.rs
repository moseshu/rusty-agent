//! R3-1b contracts for the structured reason a run stopped.

use ra_core::{error::BudgetKind, finish::FinishReason};
use serde_json::{Value, json};

/// Every variant. `FinishReason` is `#[non_exhaustive]`, so a test crate cannot get this list from
/// an exhaustive match — adding a variant means adding it here too, which is the point at which
/// someone has to decide what it means for `is_complete` and `is_resumable`.
fn all_reasons() -> Vec<FinishReason> {
    vec![
        FinishReason::Final,
        FinishReason::ToolStop,
        FinishReason::MaxTurns,
        FinishReason::BudgetExhausted,
        FinishReason::Cancelled,
        FinishReason::ErrorHandled,
        FinishReason::GuardrailTripped,
    ]
}

#[test]
fn test_finish_reason_01() {
    let reasons = all_reasons();
    let mut codes: Vec<&str> = reasons.iter().copied().map(FinishReason::code).collect();
    codes.sort_unstable();

    let total = codes.len();
    codes.dedup();
    assert_eq!(
        codes.len(),
        total,
        "存在重复的 code，图边路由与指标维度会串"
    );

    for code in codes {
        assert!(!code.is_empty());
        assert!(
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "code `{code}` 只允许小写字母与下划线——它会作为指标维度值与协议字段"
        );
    }
}

#[test]
fn test_finish_reason_02() {
    for reason in all_reasons() {
        assert_eq!(reason.to_string(), reason.code());
    }
}

#[test]
fn test_finish_reason_03() {
    // This value is persisted as a `RunState` field, written into a rollout line, and recorded as a
    // trace field. Three spellings that disagree leave replay and the metrics unable to line up.
    for reason in all_reasons() {
        let wire = serde_json::to_value(reason).expect("finish reason 应可序列化");
        assert_eq!(wire, Value::String(reason.code().to_owned()));

        let parsed: FinishReason =
            serde_json::from_value(wire).expect("finish reason 应可反序列化回来");
        assert_eq!(parsed, reason);
    }
}

#[test]
fn test_finish_reason_04() {
    // An old build reading a value a newer one wrote has to fail explicitly rather than land on some
    // default variant: the latter shows "stopped by a guardrail" as "completed normally".
    let unknown = serde_json::from_value::<FinishReason>(json!("teleported"));
    assert!(unknown.is_err());
}

#[test]
fn test_finish_reason_05() {
    // The distinction is who ended the run, not how good the answer was: a model that answered
    // wrongly still ended the run itself.
    assert!(FinishReason::Final.is_complete());
    assert!(FinishReason::ToolStop.is_complete());

    for reason in [
        FinishReason::MaxTurns,
        FinishReason::BudgetExhausted,
        FinishReason::Cancelled,
        FinishReason::ErrorHandled,
        FinishReason::GuardrailTripped,
    ] {
        assert!(!reason.is_complete(), "{reason} 是被外部打断的，不该算完成");
    }
}

#[test]
fn test_finish_reason_06() {
    // "Did not finish" is not "resuming is worth something": a guardrail refusal and an error
    // handler's closeout both reproduce the same stop on a second run.
    for reason in [
        FinishReason::MaxTurns,
        FinishReason::BudgetExhausted,
        FinishReason::Cancelled,
    ] {
        assert!(reason.is_resumable());
        assert!(!reason.is_complete());
    }

    for reason in [FinishReason::ErrorHandled, FinishReason::GuardrailTripped] {
        assert!(!reason.is_resumable(), "{reason} 续跑会复现同一个停止点");
        assert!(!reason.is_complete());
    }

    for reason in all_reasons() {
        assert!(
            !(reason.is_complete() && reason.is_resumable()),
            "{reason} 不能既是完成态又可续跑"
        );
    }
}

#[test]
fn test_finish_reason_07() {
    // `max_turns` keeps a reason of its own because a host reacts to it as "the agent is going in
    // circles" rather than "the job was too large" — the other three are the ones that ran out.
    assert_eq!(
        FinishReason::from_budget_kind(BudgetKind::MaxTurns),
        FinishReason::MaxTurns
    );
    for kind in [BudgetKind::Tokens, BudgetKind::Cost, BudgetKind::WallClock] {
        assert_eq!(
            FinishReason::from_budget_kind(kind),
            FinishReason::BudgetExhausted
        );
    }

    // Both are soft endings and both can be resumed, which is the property resumption relies on.
    for kind in [
        BudgetKind::MaxTurns,
        BudgetKind::Tokens,
        BudgetKind::Cost,
        BudgetKind::WallClock,
    ] {
        assert!(FinishReason::from_budget_kind(kind).is_resumable());
    }
}
