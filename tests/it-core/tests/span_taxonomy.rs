//! `ra-core`: behavioral assertions for the span taxonomy and field vocabulary (R0-3).
//!
//! What is locked down here is the **vocabulary**, not implementation detail:
//! - names are unique and stably formatted — let one stray `tool_name` out and the aggregated cost
//!   numbers are simply wrong
//! - the level is projected from recoverability rather than chosen by the callsite — otherwise
//!   every pre-retry failure logs as ERROR and the one line that matters is buried
//! - a cancellation records as `cancelled` rather than `error` — the same criterion as R0-2 / R0-4
//! - a field must be reserved before `record`, or it silently does nothing: a `tracing` semantic
//!   worth nailing down

use std::io::Write;
use std::sync::{Arc, Mutex};

use ra_core::cancel::{CancelReason, ScopeKind};
use ra_core::error::{
    BudgetKind, Error, GuardrailStage, ProviderErrorKind, Recoverability, SandboxErrorKind,
    ToolErrorKind,
};
use ra_core::trace::{
    SpanKind, SpanOutcome, field, level_for, record_cancel, record_error, record_outcome,
};
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

/// One instance of every kind. Adding a variant means adding it here too.
fn all_kinds() -> Vec<SpanKind> {
    vec![
        SpanKind::Agent,
        SpanKind::Turn,
        SpanKind::Generation,
        SpanKind::Function,
        SpanKind::Handoff,
        SpanKind::Guardrail,
        SpanKind::McpListTools,
        SpanKind::custom("flow_node"),
    ]
}

/// The built-in kinds, excluding `Custom`.
fn builtin_kinds() -> Vec<SpanKind> {
    let mut kinds = all_kinds();
    kinds.retain(|k| !matches!(k, SpanKind::Custom(_)));
    kinds
}

// ---------------------------------------------------------------------------
// span names and kind labels
// ---------------------------------------------------------------------------

#[test]
fn test_span_taxonomy_01() {
    let kinds = builtin_kinds();
    let mut names: Vec<&str> = kinds.iter().map(SpanKind::span_name).collect();
    names.sort_unstable();

    let total = names.len();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "存在重复的 span 名，trace 归因会把不同 span 聚到一起"
    );

    for name in names {
        assert!(!name.is_empty());
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "span 名 `{name}` 只允许小写字母与下划线"
        );
    }
}

#[test]
fn test_span_taxonomy_02() {
    // A tracing span name has to be a &'static str, which a custom label cannot supply. So names
    // collapse and the label travels in a field instead — attribution goes by label, not by name.
    let kind = SpanKind::custom("flow_node");
    assert_eq!(kind.span_name(), "custom");
    assert_eq!(kind.label(), "flow_node");
    assert!(
        kind.required_fields().contains(&field::SPAN_LABEL),
        "自定义分类必须带上 span.label，否则名字收敛之后就分不出来了"
    );
}

#[test]
fn test_span_taxonomy_03() {
    for kind in builtin_kinds() {
        assert_eq!(
            kind.span_name(),
            kind.label(),
            "内置分类没有理由让名字与标签不同"
        );
        assert_eq!(kind.to_string(), kind.label(), "Display 应就是标签");
    }
}

// ---------------------------------------------------------------------------
// level tiers
// ---------------------------------------------------------------------------

#[test]
fn test_span_taxonomy_04() {
    // The criterion: a normal run should be readable end to end at INFO.
    for kind in [
        SpanKind::Agent,
        SpanKind::Turn,
        SpanKind::Generation,
        SpanKind::Function,
        SpanKind::Handoff,
    ] {
        assert_eq!(kind.level(), Level::INFO, "`{kind}` 是 run 骨架的一部分");
    }

    for kind in [SpanKind::Guardrail, SpanKind::McpListTools] {
        assert_eq!(
            kind.level(),
            Level::DEBUG,
            "`{kind}` 高频且多数时候无事发生，进 INFO 会淹掉骨架"
        );
    }
}

#[test]
fn test_span_taxonomy_05() {
    // A span is structure, not an alert: something going wrong should emit an event rather than
    // promote the whole span to ERROR.
    for kind in all_kinds() {
        assert!(
            matches!(kind.level(), Level::INFO | Level::DEBUG),
            "`{kind}` 的 span 级别越界了：{:?}",
            kind.level()
        );
    }
}

// ---------------------------------------------------------------------------
// field vocabulary
// ---------------------------------------------------------------------------

#[test]
fn test_span_taxonomy_06() {
    let mut names = field::ALL.to_vec();
    names.sort_unstable();

    let total = names.len();
    names.dedup();
    assert_eq!(names.len(), total, "字段名重复，词表失去意义");
}

#[test]
fn test_span_taxonomy_07() {
    for name in field::ALL {
        assert!(!name.is_empty(), "字段名不能为空");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.'),
            "字段名 `{name}` 只允许小写字母、下划线与点"
        );
        assert!(
            !name.starts_with('.') && !name.ends_with('.'),
            "字段名 `{name}` 不能以点开头或结尾"
        );
        assert!(
            name.matches('.').count() <= 1,
            "字段名 `{name}` 最多一级点分：<域>.<项>"
        );
    }
}

#[test]
fn test_span_taxonomy_08() {
    for kind in all_kinds() {
        let required = kind.required_fields();

        assert!(
            required.contains(&field::SPAN_KIND),
            "`{kind}` 的必填字段漏了 span.kind——没有它就没法按分类归因"
        );

        let mut seen = required.to_vec();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total, "`{kind}` 的必填字段有重复");

        for name in required {
            assert!(
                field::ALL.contains(name),
                "`{kind}` 要求了词表外的字段 `{name}`"
            );
        }
    }
}

#[test]
fn test_span_taxonomy_09() {
    // A terminal field is reserved with Empty and recorded later, so it must not appear in the
    // required list — otherwise the callsite would have to invent a fake value to fill it.
    let 终态字段 = [
        field::OUTCOME,
        field::ERROR_CODE,
        field::CANCEL_REASON,
        field::CANCEL_SCOPE,
        field::USAGE_INPUT_TOKENS,
        field::USAGE_CACHED_INPUT_TOKENS,
        field::USAGE_OUTPUT_TOKENS,
        field::USAGE_REASONING_TOKENS,
        field::GUARDRAIL_TRIGGERED,
    ];

    for kind in all_kinds() {
        for name in 终态字段 {
            assert!(
                !kind.required_fields().contains(&name),
                "`{kind}` 把终态字段 `{name}` 列成了创建时必填"
            );
        }
    }
}

#[test]
fn test_span_taxonomy_10() {
    // Cache hit rate is the dominant cost driver; folding it into input_tokens makes the hit rate
    // impossible to compute.
    assert!(field::ALL.contains(&field::USAGE_CACHED_INPUT_TOKENS));
    assert_ne!(field::USAGE_CACHED_INPUT_TOKENS, field::USAGE_INPUT_TOKENS);
}

// ---------------------------------------------------------------------------
// terminal-state projection
// ---------------------------------------------------------------------------

#[test]
fn test_span_taxonomy_11() {
    for err in [
        Error::cancelled("用户中断"),
        Error::tool(ToolErrorKind::Cancelled, "exec_command", "已取消"),
    ] {
        let outcome = SpanOutcome::from(&err);
        assert_eq!(
            outcome,
            SpanOutcome::Cancelled,
            "`{}` 记成了失败",
            err.code()
        );
        assert!(!outcome.is_failure(), "取消不该计入失败率");
    }
}

#[test]
fn test_span_taxonomy_12() {
    for err in [
        Error::config("缺少 model"),
        Error::caller("状态机被违规驱动"),
        Error::provider(ProviderErrorKind::RateLimit, "429"),
        Error::sandbox(SandboxErrorKind::Denied, "越界"),
        Error::budget(BudgetKind::MaxTurns, "达到上限"),
        Error::guardrail(GuardrailStage::ToolInput, "read_before_edit", "未读先写"),
    ] {
        let outcome = SpanOutcome::from(&err);
        assert_eq!(outcome, SpanOutcome::Error, "`{}` 应记成失败", err.code());
        assert!(outcome.is_failure());
    }
}

#[test]
fn test_span_taxonomy_13() {
    for err in [
        Error::cancelled("用户中断"),
        Error::tool(ToolErrorKind::Cancelled, "exec_command", "已取消"),
        Error::provider(ProviderErrorKind::Auth, "invalid key"),
        Error::session(ra_core::error::SessionErrorKind::Io, "写失败"),
    ] {
        assert_eq!(
            SpanOutcome::from(&err).is_failure(),
            err.recoverability().is_failure(),
            "`{}` 的 span 终态与可恢复性投影对失败的判定不一致",
            err.code()
        );
    }
}

#[test]
fn test_span_taxonomy_14() {
    assert_eq!(SpanOutcome::Ok.as_str(), "ok");
    assert_eq!(SpanOutcome::Error.as_str(), "error");
    assert_eq!(SpanOutcome::Cancelled.as_str(), "cancelled");

    for outcome in [SpanOutcome::Ok, SpanOutcome::Error, SpanOutcome::Cancelled] {
        assert_eq!(outcome.to_string(), outcome.as_str());
    }
}

// ---------------------------------------------------------------------------
// level projection
// ---------------------------------------------------------------------------

#[test]
fn test_span_taxonomy_15() {
    assert_eq!(level_for(Recoverability::Retryable), Level::WARN);
    assert_eq!(level_for(Recoverability::RetryableWithChange), Level::WARN);
    assert_eq!(level_for(Recoverability::NeedsIntervention), Level::ERROR);
    assert_eq!(level_for(Recoverability::Fatal), Level::ERROR);
}

#[test]
fn test_span_taxonomy_16() {
    // A user pressing stop should not paint the log red, or real errors get buried.
    assert_eq!(level_for(Recoverability::Cancelled), Level::INFO);
}

#[test]
fn test_span_taxonomy_17() {
    for recoverability in [
        Recoverability::Retryable,
        Recoverability::RetryableWithChange,
        Recoverability::NeedsIntervention,
        Recoverability::Fatal,
        Recoverability::Cancelled,
    ] {
        let 是错误级 = level_for(recoverability) == Level::ERROR;
        assert_eq!(
            是错误级,
            !recoverability.is_recoverable() && recoverability.is_failure(),
            "`{recoverability}` 的日志级别与可恢复性对不上"
        );
    }
}

// ---------------------------------------------------------------------------
// record helpers: the field really lands
// ---------------------------------------------------------------------------

/// Captures the fmt layer's output in memory.
#[derive(Clone, Default)]
struct 捕获(Arc<Mutex<Vec<u8>>>);

impl 捕获 {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for 捕获 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for 捕获 {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `f` under a subscriber that emits span-close events only, and returns the captured text.
fn capture_span_close(f: impl FnOnce()) -> String {
    let sink = 捕获::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::CLOSE)
        .finish();

    tracing::subscriber::with_default(subscriber, f);
    sink.text()
}

#[test]
fn test_span_taxonomy_18() {
    let out = capture_span_close(|| {
        let span = tracing::info_span!(
            "turn",
            span.kind = SpanKind::Turn.label(),
            turn.index = 0,
            outcome = tracing::field::Empty,
        );
        record_outcome(&span, SpanOutcome::Ok);
    });

    assert!(out.contains("outcome=\"ok\""), "没看到终态字段：{out}");
    assert!(out.contains("span.kind=\"turn\""), "没看到分类字段：{out}");
}

#[test]
fn test_span_taxonomy_19() {
    // This is tracing semantics, not a flaw in the helper. Pinning it down keeps someone from
    // later assuming a failed record reports an error — it just drops silently, and nobody
    // notices one missing field in a log.
    let out = capture_span_close(|| {
        let span = tracing::info_span!("turn", span.kind = SpanKind::Turn.label());
        record_outcome(&span, SpanOutcome::Ok);
    });

    assert!(
        !out.contains("outcome="),
        "未声明 Empty 的字段竟然被记录了，说明 tracing 语义变了，文档要跟着改：{out}"
    );
}

#[test]
fn test_span_taxonomy_20() {
    let out = capture_span_close(|| {
        let span = tracing::info_span!(
            "generation",
            span.kind = SpanKind::Generation.label(),
            model.name = "gpt-5",
            model.provider = "openai",
            error.code = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let err = Error::provider(ProviderErrorKind::RateLimit, "密钥 sk-abc123 触发限流");
        record_error(&span, &err);
    });

    assert!(out.contains("error.code=\"provider.rate_limit\""), "{out}");
    assert!(out.contains("outcome=\"error\""), "{out}");
    assert!(
        !out.contains("sk-abc123"),
        "错误文本进了日志——它会变、会含密钥，且按文本聚合就是词表式判断：{out}"
    );
}

#[test]
fn test_span_taxonomy_21() {
    let out = capture_span_close(|| {
        let span = tracing::info_span!(
            "function",
            span.kind = SpanKind::Function.label(),
            tool.name = "exec_command",
            tool.call_id = "call_1",
            error.code = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        record_error(&span, &Error::cancelled("用户中断"));
    });

    assert!(
        out.contains("outcome=\"cancelled\""),
        "取消被记成了失败：{out}"
    );
}

#[test]
fn test_span_taxonomy_22() {
    // The two fields promised by the cancellation contract (R0-4), and their only write path.
    let out = capture_span_close(|| {
        let span = tracing::info_span!(
            "function",
            span.kind = SpanKind::Function.label(),
            tool.name = "exec_command",
            tool.call_id = "call_1",
            cancel.reason = tracing::field::Empty,
            cancel.scope = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        record_cancel(&span, &CancelReason::Timeout, &ScopeKind::Tool);
    });

    assert!(out.contains("cancel.reason=\"timeout\""), "{out}");
    assert!(out.contains("cancel.scope=\"tool\""), "{out}");
    assert!(out.contains("outcome=\"cancelled\""), "{out}");
}

#[test]
fn test_span_taxonomy_23() {
    // A macro field name can only be a literal, so no constant can be interpolated and this
    // assertion is the only thing keeping the two sides aligned. Every literal used by the
    // callsites above is listed here.
    assert_eq!(field::SPAN_KIND, "span.kind");
    assert_eq!(field::OUTCOME, "outcome");
    assert_eq!(field::ERROR_CODE, "error.code");
    assert_eq!(field::CANCEL_REASON, "cancel.reason");
    assert_eq!(field::CANCEL_SCOPE, "cancel.scope");
    assert_eq!(field::TURN_INDEX, "turn.index");
    assert_eq!(field::TOOL_NAME, "tool.name");
    assert_eq!(field::TOOL_CALL_ID, "tool.call_id");
    assert_eq!(field::MODEL_NAME, "model.name");
    assert_eq!(field::MODEL_PROVIDER, "model.provider");
}
