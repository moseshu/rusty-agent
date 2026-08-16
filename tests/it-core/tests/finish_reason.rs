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
    // R6-6a 把它作为 `RunState` 字段落盘，R9-0 写进 rollout 行，R0-3 写进 trace 字段。
    // 三处记法不一致的话，replay 与指标就对不上。
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
    // 旧版本读到新版本写的值，必须是显式失败而不是落到某个默认变体上——
    // 后者会让「被 guard 拦下」在旧版本里显示成「正常完成」。
    let unknown = serde_json::from_value::<FinishReason>(json!("teleported"));
    assert!(unknown.is_err());
}

#[test]
fn test_finish_reason_05() {
    // 区分的是「谁结束了这个 run」，不是「答得好不好」：模型答错了也仍然是它自己收的尾。
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
    // 「没跑完」不等于「接着跑有意义」：guard 拦下与 error handler 收尾再跑一遍是同样的结果。
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
    // max_turns 单独留一个原因：宿主对它的反应通常是「agent 在打转」，
    // 而不是「活儿太大」——后三种才是额度不够。
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

    // 两条都是软结束，都能续跑——这正是 R3-8 依赖的性质。
    for kind in [
        BudgetKind::MaxTurns,
        BudgetKind::Tokens,
        BudgetKind::Cost,
        BudgetKind::WallClock,
    ] {
        assert!(FinishReason::from_budget_kind(kind).is_resumable());
    }
}
