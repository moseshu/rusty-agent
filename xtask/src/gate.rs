//! 门禁结果。
//!
//! 三态而不是两态：**SKIP 不是 PASS**。
//!
//! 九条门禁里有几条的被测对象还不存在（工具 schema 要等 R2、prompt 分段要等
//! R4）。这些条目要么写成占位输出「待实现」，要么假装通过——前者会被当成完成，
//! 后者更糟，它给的是虚假的安全感。所以单列一档 [`Outcome::Skip`]，带上**是哪个
//! 任务在挡着**，并在汇总里单独计数：CI 不会因为它红，但每跑一次都会看见还欠几条。

use core::fmt;

/// 一条门禁的执行结果。
pub(crate) enum Outcome {
    /// 通过，附一句话说明查了什么。
    Pass(String),
    /// 前置条件不具备，本次未执行。
    Skip {
        /// 为什么跳过。
        reason: String,
        /// 挡着它的任务号，如 `R2-9`。
        blocked_by: &'static str,
    },
    /// 未通过，逐条列出违规。
    Fail(Vec<String>),
}

impl Outcome {
    /// 通过。
    pub(crate) fn pass(detail: impl Into<String>) -> Self {
        Self::Pass(detail.into())
    }

    /// 跳过。
    pub(crate) fn skip(blocked_by: &'static str, reason: impl Into<String>) -> Self {
        Self::Skip {
            reason: reason.into(),
            blocked_by,
        }
    }

    /// 没有违规就通过，否则失败。
    pub(crate) fn from_violations(violations: Vec<String>, detail: impl Into<String>) -> Self {
        if violations.is_empty() {
            Self::pass(detail)
        } else {
            Self::Fail(violations)
        }
    }

    /// 是否算失败。**只有它让 CI 变红**。
    pub(crate) const fn is_failure(&self) -> bool {
        matches!(self, Self::Fail(_))
    }

    /// 是否被跳过。
    pub(crate) const fn is_skipped(&self) -> bool {
        matches!(self, Self::Skip { .. })
    }

    /// 固定宽度的状态标签，用于汇总表对齐。
    pub(crate) const fn status(&self) -> &'static str {
        match self {
            Self::Pass(_) => "PASS",
            Self::Skip { .. } => "SKIP",
            Self::Fail(_) => "FAIL",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass(detail) => f.write_str(detail),
            Self::Skip { reason, blocked_by } => write!(f, "{reason}（等 {blocked_by}）"),
            Self::Fail(violations) => write!(f, "{} 处违规", violations.len()),
        }
    }
}
