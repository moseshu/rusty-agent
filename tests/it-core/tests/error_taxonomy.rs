//! `ra-core`: behavioral assertions for the error taxonomy (R0-2).
//!
//! What is locked down here is the **contract**, not implementation detail:
//! - `code()` is globally unique and stably formatted — it becomes a trace label and an eval
//!   attribution, so a collision misattributes
//! - the recoverability projection and `is_retryable()` never disagree — that is what "project,
//!   do not store" buys
//! - a caller defect is never retryable and a cancellation is never a failure — two hard criteria
//!   R0-2 states outright

use ra_core::error::{
    BudgetKind, Error, GuardrailStage, ProtocolErrorKind, ProviderErrorKind, Recoverability,
    SandboxErrorKind, SessionErrorKind, ToolErrorKind,
};

/// One instance of every (variant x kind) combination. Adding a variant or a kind means adding it
/// here too: the uniqueness and format assertions below weaken if one is missed, though the
/// `match` in `code()` is exhaustive, so the compiler stops a forgotten variant first.
fn all_errors() -> Vec<Error> {
    let mut v = vec![
        Error::config("缺少 model"),
        Error::caller("状态机被违规驱动"),
    ];

    for kind in [
        ProviderErrorKind::Network,
        ProviderErrorKind::RateLimit,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::ServerError,
        ProviderErrorKind::Auth,
        ProviderErrorKind::BadRequest,
        ProviderErrorKind::Refusal,
        ProviderErrorKind::Behavior,
        ProviderErrorKind::ContextOverflow,
    ] {
        v.push(Error::provider(kind, "provider 失败"));
    }

    for kind in [
        ToolErrorKind::NotFound,
        ToolErrorKind::InvalidInput,
        ToolErrorKind::Timeout,
        ToolErrorKind::ExecutionFailed,
        ToolErrorKind::RepeatedCall,
        ToolErrorKind::NoProgress,
        ToolErrorKind::Cancelled,
    ] {
        v.push(Error::tool(kind, "exec_command", "工具失败"));
    }

    for kind in [
        SandboxErrorKind::Denied,
        SandboxErrorKind::Unavailable,
        SandboxErrorKind::ResourceLimit,
        SandboxErrorKind::Setup,
    ] {
        v.push(Error::sandbox(kind, "沙箱失败"));
    }

    for kind in [
        SessionErrorKind::NotFound,
        SessionErrorKind::Corrupted,
        SessionErrorKind::Io,
        SessionErrorKind::VersionMismatch,
    ] {
        v.push(Error::session(kind, "会话失败"));
    }

    for kind in [
        ProtocolErrorKind::Frame,
        ProtocolErrorKind::Transport,
        ProtocolErrorKind::Handshake,
        ProtocolErrorKind::Timeout,
    ] {
        v.push(Error::protocol(kind, "协议失败"));
    }

    for kind in [
        BudgetKind::MaxTurns,
        BudgetKind::Tokens,
        BudgetKind::Cost,
        BudgetKind::WallClock,
    ] {
        v.push(Error::budget(kind, "预算耗尽"));
    }

    for stage in [
        GuardrailStage::Input,
        GuardrailStage::Output,
        GuardrailStage::ToolInput,
        GuardrailStage::ToolOutput,
    ] {
        v.push(Error::guardrail(stage, "read_before_edit", "护栏触发"));
    }

    v.push(Error::cancelled("用户中断"));
    v
}

// ---------------------------------------------------------------------------
// code(): the machine-readable identity
// ---------------------------------------------------------------------------

#[test]
fn test_error_taxonomy_01() {
    let errors = all_errors();
    let mut codes: Vec<&str> = errors.iter().map(Error::code).collect();
    codes.sort_unstable();

    let total = codes.len();
    codes.dedup();
    assert_eq!(
        codes.len(),
        total,
        "存在重复的 error code，trace 归因会把不同错误聚到一起"
    );
}

#[test]
fn test_error_taxonomy_02() {
    for err in all_errors() {
        let code = err.code();
        assert!(!code.is_empty(), "code 不能为空");
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.'),
            "code `{code}` 只允许小写字母、下划线与点——它会作为指标维度值"
        );
        assert!(
            code.matches('.').count() <= 1,
            "code `{code}` 最多一级点分：<子系统>.<kind>"
        );
        assert!(
            !code.starts_with('.') && !code.ends_with('.'),
            "code `{code}` 不能以点开头或结尾"
        );
    }
}

// ---------------------------------------------------------------------------
// recoverability projection
// ---------------------------------------------------------------------------

#[test]
fn test_error_taxonomy_03() {
    for err in all_errors() {
        assert_eq!(
            err.is_retryable(),
            err.recoverability().is_retryable(),
            "`{}` 的 is_retryable 与 recoverability 投影不一致",
            err.code()
        );
    }
}

#[test]
fn test_error_taxonomy_04() {
    for err in all_errors() {
        assert_eq!(
            err.is_cancelled(),
            err.recoverability() == Recoverability::Cancelled,
            "`{}` 的 is_cancelled 与 recoverability 投影不一致",
            err.code()
        );
    }
}

#[test]
fn test_error_taxonomy_05() {
    let err = Error::caller("参数组合非法");
    assert_eq!(err.recoverability(), Recoverability::Fatal);
    assert!(!err.is_retryable());
    assert!(
        !err.recoverability().is_recoverable(),
        "调用方缺陷换参数也没用"
    );
}

#[test]
fn test_error_taxonomy_06() {
    for err in [
        Error::cancelled("用户中断"),
        Error::tool(ToolErrorKind::Cancelled, "exec_command", "已取消"),
    ] {
        assert!(err.is_cancelled(), "`{}` 应判定为取消", err.code());
        assert!(
            !err.recoverability().is_failure(),
            "`{}` 是取消，不应计入失败率",
            err.code()
        );
        assert!(!err.is_retryable(), "取消不该触发重试");
    }
}

#[test]
fn test_error_taxonomy_07() {
    // R1-12: a refusal triggers the model fallback; retrying unchanged only gets refused again.
    let err = Error::provider(ProviderErrorKind::Refusal, "refused");
    assert_eq!(err.recoverability(), Recoverability::RetryableWithChange);
    assert!(!err.is_retryable(), "拒答原样重试无意义");
    assert!(err.recoverability().is_recoverable(), "换模型后仍值得再试");
}

#[test]
fn test_error_taxonomy_08() {
    // R5: not a plain retry — the context has to be compacted first.
    let err = Error::provider(ProviderErrorKind::ContextOverflow, "too long");
    assert_eq!(err.recoverability(), Recoverability::RetryableWithChange);
    assert!(!err.is_retryable());
}

#[test]
fn test_error_taxonomy_09() {
    for kind in [
        ProviderErrorKind::Network,
        ProviderErrorKind::RateLimit,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::ServerError,
    ] {
        let err = Error::provider(kind, "transient");
        assert!(err.is_retryable(), "`{}` 应可原样重试", err.code());
    }
}

#[test]
fn test_error_taxonomy_10() {
    let err = Error::provider(ProviderErrorKind::Auth, "invalid api key");
    assert_eq!(err.recoverability(), Recoverability::NeedsIntervention);
    assert!(!err.is_retryable(), "换个 key 之前重试多少次都没用");
}

#[test]
fn test_error_taxonomy_11() {
    // R3-8: an exhausted budget takes the soft ending of NextStep::FinalOutput, not a retry.
    for kind in [
        BudgetKind::MaxTurns,
        BudgetKind::Tokens,
        BudgetKind::Cost,
        BudgetKind::WallClock,
    ] {
        let err = Error::budget(kind, "exhausted");
        assert_eq!(err.recoverability(), Recoverability::NeedsIntervention);
        assert!(!err.is_retryable());
    }
}

#[test]
fn test_error_taxonomy_12() {
    for stage in [
        GuardrailStage::Input,
        GuardrailStage::Output,
        GuardrailStage::ToolInput,
        GuardrailStage::ToolOutput,
    ] {
        let err = Error::guardrail(stage, "read_before_edit", "未读先写");
        assert_eq!(err.recoverability(), Recoverability::Fatal);
        assert!(!err.is_retryable(), "绕过护栏重试会让护栏形同虚设");
    }
}

// ---------------------------------------------------------------------------
// messages: two audiences
// ---------------------------------------------------------------------------

/// Variants whose `Display` and `user_message` are allowed to match.
///
/// A cancellation is not a fault and has no internal detail to hide from the user, so both
/// audiences seeing the same sentence is **correct** rather than lazy. Every other variant carries
/// technical annotation such as `{kind:?}` in `Display` and needs a separate wording for the UI.
const 允许两者相同: &[&str] = &["cancelled"];

#[test]
fn test_error_taxonomy_13() {
    for err in all_errors() {
        let user = err.user_message();
        let dev = err.to_string();

        assert!(
            !user.trim().is_empty(),
            "`{}` 的 user_message 为空",
            err.code()
        );
        assert!(!dev.trim().is_empty(), "`{}` 的 Display 为空", err.code());

        if 允许两者相同.contains(&err.code()) {
            continue;
        }
        assert_ne!(
            user,
            dev,
            "`{}` 的 user_message 与 Display 相同——两者面向不同受众，\
             Display 给开发者与日志，user_message 给 UI。若该变体确实无内部\
             细节可隐藏，请显式加进 `允许两者相同`",
            err.code()
        );
    }
}

#[test]
fn test_error_taxonomy_14() {
    // `{kind:?}` in Display is deliberate (it is for logs); it must not reach user_message.
    let internal_tokens = [
        "ProviderErrorKind",
        "ToolErrorKind",
        "SandboxErrorKind",
        "SessionErrorKind",
        "ProtocolErrorKind",
        "BudgetKind",
        "GuardrailStage",
        "Recoverability",
    ];
    for err in all_errors() {
        let user = err.user_message();
        for token in internal_tokens {
            assert!(
                !user.contains(token),
                "`{}` 的 user_message 泄露了内部类型名 `{token}`：{user}",
                err.code()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// with_source
// ---------------------------------------------------------------------------

#[test]
fn test_error_taxonomy_15() {
    use std::error::Error as _;

    let io = std::io::Error::other("底层 io 失败");
    let err = Error::session(SessionErrorKind::Io, "写 rollout 失败").with_source(io);

    let source = err.source().expect("Session 变体应保留 source");
    assert!(source.to_string().contains("底层 io 失败"));
}

#[test]
fn test_error_taxonomy_16() {
    use std::error::Error as _;

    // Budget / Guardrail / Cancelled carry no source: attaching one should be ignored silently,
    // not panic, and not quietly turn into a different variant.
    let io = std::io::Error::other("无关错误");
    let err = Error::budget(BudgetKind::MaxTurns, "达到上限").with_source(io);

    assert!(err.source().is_none(), "Budget 变体不应携带 source");
    assert_eq!(err.code(), "budget.max_turns", "变体不应被改写");
}

// ---------------------------------------------------------------------------
// Recoverability itself
// ---------------------------------------------------------------------------

#[test]
fn test_error_taxonomy_17() {
    let all = [
        Recoverability::Retryable,
        Recoverability::RetryableWithChange,
        Recoverability::NeedsIntervention,
        Recoverability::Fatal,
        Recoverability::Cancelled,
    ];

    for r in all {
        // retryable unchanged implies recoverable
        if r.is_retryable() {
            assert!(r.is_recoverable(), "{r} 可重试却不可恢复，自相矛盾");
        }
        // cancelled if and only if not a failure
        assert_eq!(
            r == Recoverability::Cancelled,
            !r.is_failure(),
            "{r} 的 is_failure 与取消语义不一致"
        );
    }

    assert!(!Recoverability::Fatal.is_recoverable());
    assert!(!Recoverability::NeedsIntervention.is_recoverable());
}

#[test]
fn test_error_taxonomy_18() {
    assert_eq!(Recoverability::Retryable.to_string(), "retryable");
    assert_eq!(
        Recoverability::RetryableWithChange.to_string(),
        "retryable_with_change"
    );
    assert_eq!(
        Recoverability::NeedsIntervention.to_string(),
        "needs_intervention"
    );
    assert_eq!(Recoverability::Fatal.to_string(), "fatal");
    assert_eq!(Recoverability::Cancelled.to_string(), "cancelled");
}
