//! `ra-core`：错误分类学（R0-2）的行为断言。
//!
//! 这里锁住的是**契约**，不是实现细节：
//! - `code()` 全局唯一且格式稳定——它进 trace 标签与 eval 归因，串了就归因错
//! - 可恢复性投影与 `is_retryable()` 永不矛盾——这是「投影而非存储」的收益
//! - 调用方缺陷永不可重试、取消不算失败——两条 R0-2 明确的硬判据

use ra_core::error::{
    BudgetKind, Error, GuardrailStage, ProtocolErrorKind, ProviderErrorKind, Recoverability,
    SandboxErrorKind, SessionErrorKind, ToolErrorKind,
};

/// 每个 (变体 × kind) 组合各一个实例。新增变体或 kind 时必须同步补进来——
/// 下面的唯一性与格式断言会因为漏补而变弱，但 `code()` 的 `match` 是穷尽的，
/// 编译器会先一步拦住忘记处理的新变体。
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
// code()：机器可读标识
// ---------------------------------------------------------------------------

#[test]
fn code_全局唯一() {
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
fn code_格式稳定() {
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
// 可恢复性投影
// ---------------------------------------------------------------------------

#[test]
fn is_retryable_与投影永不矛盾() {
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
fn is_cancelled_与投影永不矛盾() {
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
fn 调用方缺陷永不可重试() {
    let err = Error::caller("参数组合非法");
    assert_eq!(err.recoverability(), Recoverability::Fatal);
    assert!(!err.is_retryable());
    assert!(
        !err.recoverability().is_recoverable(),
        "调用方缺陷换参数也没用"
    );
}

#[test]
fn 取消不算失败() {
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
fn 模型拒答走换模型而非原样重试() {
    // R1-12：拒答触发模型回退，原样重试只会再被拒一次。
    let err = Error::provider(ProviderErrorKind::Refusal, "refused");
    assert_eq!(err.recoverability(), Recoverability::RetryableWithChange);
    assert!(!err.is_retryable(), "拒答原样重试无意义");
    assert!(err.recoverability().is_recoverable(), "换模型后仍值得再试");
}

#[test]
fn 上下文超限先压缩再重试() {
    // R5：不是原样重试，要先把上下文压下去。
    let err = Error::provider(ProviderErrorKind::ContextOverflow, "too long");
    assert_eq!(err.recoverability(), Recoverability::RetryableWithChange);
    assert!(!err.is_retryable());
}

#[test]
fn 瞬时故障可原样重试() {
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
fn 认证失败需要人介入而非重试() {
    let err = Error::provider(ProviderErrorKind::Auth, "invalid api key");
    assert_eq!(err.recoverability(), Recoverability::NeedsIntervention);
    assert!(!err.is_retryable(), "换个 key 之前重试多少次都没用");
}

#[test]
fn 预算耗尽不是可重试故障() {
    // R3-8：预算耗尽走 NextStep::FinalOutput 的软结束，不是重试。
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
fn 护栏触发是刻意拦截不是故障() {
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
// 消息：两个受众
// ---------------------------------------------------------------------------

/// `Display` 与 `user_message` 允许相同的变体。
///
/// 取消不是故障，没有任何内部细节需要对用户隐藏——两个受众看到同一句话是
/// **正确的**，不是偷懒。除此之外的变体，Display 都带着 `{kind:?}` 之类的
/// 技术标注，必须为 UI 另写一版。
const 允许两者相同: &[&str] = &["cancelled"];

#[test]
fn user_message_非空且区别于_display() {
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
fn user_message_不泄露内部枚举名() {
    // Display 里带 `{kind:?}` 是刻意的（给日志看）；user_message 不该出现。
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
fn with_source_在支持的变体上生效() {
    use std::error::Error as _;

    let io = std::io::Error::other("底层 io 失败");
    let err = Error::session(SessionErrorKind::Io, "写 rollout 失败").with_source(io);

    let source = err.source().expect("Session 变体应保留 source");
    assert!(source.to_string().contains("底层 io 失败"));
}

#[test]
fn with_source_在不支持的变体上是无操作() {
    use std::error::Error as _;

    // Budget / Guardrail / Cancelled 不携带 source：附加应被静默忽略，
    // 而不是 panic，也不是悄悄换成别的变体。
    let io = std::io::Error::other("无关错误");
    let err = Error::budget(BudgetKind::MaxTurns, "达到上限").with_source(io);

    assert!(err.source().is_none(), "Budget 变体不应携带 source");
    assert_eq!(err.code(), "budget.max_turns", "变体不应被改写");
}

// ---------------------------------------------------------------------------
// Recoverability 自身
// ---------------------------------------------------------------------------

#[test]
fn recoverability_的三个谓词互相自洽() {
    let all = [
        Recoverability::Retryable,
        Recoverability::RetryableWithChange,
        Recoverability::NeedsIntervention,
        Recoverability::Fatal,
        Recoverability::Cancelled,
    ];

    for r in all {
        // 可原样重试 ⟹ 可恢复
        if r.is_retryable() {
            assert!(r.is_recoverable(), "{r} 可重试却不可恢复，自相矛盾");
        }
        // 取消 ⟺ 非失败
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
fn recoverability_display_是稳定的_snake_case() {
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
