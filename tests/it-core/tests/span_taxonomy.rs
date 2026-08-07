//! `ra-core`：span 分类与字段词表（R0-3）的行为断言。
//!
//! 这里锁住的是**词表**，不是实现细节：
//! - 名字唯一且格式稳定——散一个 `tool_name` 出去，聚合出来的成本数字就是错的
//! - 级别是从可恢复性投影出来的，不由 callsite 自己拍——否则重试前的失败全打
//!   ERROR，真要人看的那条被淹掉
//! - 取消记成 `cancelled` 而不是 `error`——与 R0-2 / R0-4 同一条判据
//! - `record` 的字段必须先占位，否则静默无效：这条 `tracing` 语义值得钉死

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

/// 每个分类各一个实例。新增变体时必须同步补进来。
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

/// 内置分类（不含 `Custom`）。
fn builtin_kinds() -> Vec<SpanKind> {
    let mut kinds = all_kinds();
    kinds.retain(|k| !matches!(k, SpanKind::Custom(_)));
    kinds
}

// ---------------------------------------------------------------------------
// span 名与分类标签
// ---------------------------------------------------------------------------

#[test]
fn span_name_全局唯一且格式稳定() {
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
fn 自定义分类的_span_名收敛到_custom_但标签保留() {
    // tracing 的 span 名必须是 &'static str，自定义标签给不了。
    // 所以名字收敛、标签另走字段——归因按标签，不按名字。
    let kind = SpanKind::custom("flow_node");
    assert_eq!(kind.span_name(), "custom");
    assert_eq!(kind.label(), "flow_node");
    assert!(
        kind.required_fields().contains(&field::SPAN_LABEL),
        "自定义分类必须带上 span.label，否则名字收敛之后就分不出来了"
    );
}

#[test]
fn 内置分类的名字与标签一致() {
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
// 级别分档
// ---------------------------------------------------------------------------

#[test]
fn run_骨架进_info_高频检查进_debug() {
    // 判据：一次正常的 run 在 INFO 下应当读得完。
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
fn 没有任何_span_默认进_error_或_trace() {
    // span 是结构，不是告警：出事该发事件，不该把整个 span 提到 ERROR。
    for kind in all_kinds() {
        assert!(
            matches!(kind.level(), Level::INFO | Level::DEBUG),
            "`{kind}` 的 span 级别越界了：{:?}",
            kind.level()
        );
    }
}

// ---------------------------------------------------------------------------
// 字段词表
// ---------------------------------------------------------------------------

#[test]
fn 字段名全局唯一() {
    let mut names = field::ALL.to_vec();
    names.sort_unstable();

    let total = names.len();
    names.dedup();
    assert_eq!(names.len(), total, "字段名重复，词表失去意义");
}

#[test]
fn 字段名格式稳定() {
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
fn 每个分类的必填字段都在词表内且含分类标识() {
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
fn 必填字段只要创建时就知道的标识() {
    // 终态字段要用 Empty 占位后再 record，不能出现在必填清单里——
    // 否则 callsite 只能先造一个假值填进去。
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
fn 缓存_token_单列() {
    // 缓存命中率是成本主因；折进 input_tokens 就再也算不出命中率。
    assert!(field::ALL.contains(&field::USAGE_CACHED_INPUT_TOKENS));
    assert_ne!(field::USAGE_CACHED_INPUT_TOKENS, field::USAGE_INPUT_TOKENS);
}

// ---------------------------------------------------------------------------
// 终态投影
// ---------------------------------------------------------------------------

#[test]
fn 取消收敛成_cancelled_而不是_error() {
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
fn 非取消的错误收敛成_error() {
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
fn 终态投影与错误自身的判据永不矛盾() {
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
fn outcome_取值稳定() {
    assert_eq!(SpanOutcome::Ok.as_str(), "ok");
    assert_eq!(SpanOutcome::Error.as_str(), "error");
    assert_eq!(SpanOutcome::Cancelled.as_str(), "cancelled");

    for outcome in [SpanOutcome::Ok, SpanOutcome::Error, SpanOutcome::Cancelled] {
        assert_eq!(outcome.to_string(), outcome.as_str());
    }
}

// ---------------------------------------------------------------------------
// 级别投影
// ---------------------------------------------------------------------------

#[test]
fn 可自愈的失败进_warn_需要人介入的进_error() {
    assert_eq!(level_for(Recoverability::Retryable), Level::WARN);
    assert_eq!(level_for(Recoverability::RetryableWithChange), Level::WARN);
    assert_eq!(level_for(Recoverability::NeedsIntervention), Level::ERROR);
    assert_eq!(level_for(Recoverability::Fatal), Level::ERROR);
}

#[test]
fn 取消不进_error_级() {
    // 用户按停止键不该在日志里刷红，否则真错误会被淹掉。
    assert_eq!(level_for(Recoverability::Cancelled), Level::INFO);
}

#[test]
fn 需要人介入的错误才配拿到_error_级() {
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
// record helper：字段真的落进去了
// ---------------------------------------------------------------------------

/// 把 fmt 层的输出捕获到内存里。
#[derive(Clone, Default)]
struct 捕获(Arc<Mutex<Vec<u8>>>);

impl 捕获 {
    fn 文本(&self) -> String {
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

/// 在一个只输出 span 关闭事件的 subscriber 下跑 `f`，返回捕获到的文本。
fn 捕获_span_关闭(f: impl FnOnce()) -> String {
    let sink = 捕获::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::CLOSE)
        .finish();

    tracing::subscriber::with_default(subscriber, f);
    sink.文本()
}

#[test]
fn record_outcome_写进_span_字段() {
    let out = 捕获_span_关闭(|| {
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
fn 未占位的字段_record_无效() {
    // 这是 tracing 的语义，不是 helper 的缺陷。钉住它，免得将来有人以为
    // record 失败会报错——它只会静默丢掉，日志里少一个字段没人会注意到。
    let out = 捕获_span_关闭(|| {
        let span = tracing::info_span!("turn", span.kind = SpanKind::Turn.label());
        record_outcome(&span, SpanOutcome::Ok);
    });

    assert!(
        !out.contains("outcome="),
        "未声明 Empty 的字段竟然被记录了，说明 tracing 语义变了，文档要跟着改：{out}"
    );
}

#[test]
fn record_error_只落_code_不落文本() {
    let out = 捕获_span_关闭(|| {
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
fn record_error_对取消记成_cancelled() {
    let out = 捕获_span_关闭(|| {
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
fn record_cancel_落根因与发起层级() {
    // 取消契约（R0-4）承诺的两个字段，唯一写入口。
    let out = 捕获_span_关闭(|| {
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
fn 字段常量与宏_callsite_的字面量一致() {
    // 宏的字段名只能写字面量，没法把常量插进去，两边只能靠这条断言对齐。
    // 上面几个 callsite 用到的字面量都列在这里。
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
