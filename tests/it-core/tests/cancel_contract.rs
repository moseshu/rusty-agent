//! `ra-core`: behavioral assertions for the cancellation contract (R0-4).
//!
//! What is locked down here is the **contract**, not implementation detail:
//! - cancellation propagates downward only, and propagation never rewrites a child scope's own
//!   root cause — attribution does not die on the first hop
//! - "cancelled" and "has a root cause" always hold together — there is no cancellation whose
//!   reason cannot be recovered
//! - a deadline can only tighten, and an expired one always takes effect at a checkpoint even when
//!   nobody armed a timer
//! - cancellation is not failure: no failure rate, no retry

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ra_core::cancel::{CancelReason, CancelScope, DRAIN_GRACE, Deadline, ScopeKind};
use ra_core::error::Recoverability;

/// One instance of every reason. Adding a variant means adding it here too.
fn all_reasons() -> Vec<CancelReason> {
    vec![
        CancelReason::UserInterrupt,
        CancelReason::Shutdown,
        CancelReason::Deadline,
        CancelReason::Timeout,
        CancelReason::Superseded,
        CancelReason::PeerFailure,
        CancelReason::Unspecified,
        CancelReason::custom("myapp_quota"),
    ]
}

/// The canonical four-level tree: run -> turn -> tool -> process.
fn four_level_tree() -> (CancelScope, CancelScope, CancelScope, CancelScope) {
    let run = CancelScope::root();
    let turn = run.child(ScopeKind::Turn);
    let tool = turn.child(ScopeKind::Tool);
    let process = tool.child(ScopeKind::Process);
    (run, turn, tool, process)
}

// ---------------------------------------------------------------------------
// propagation direction
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_01() {
    // "cancelling the parent has to reach the child process of a grandchild run" is the easiest
    // leak to miss, so this asserts the whole chain rather than the direct child alone.
    let (run, turn, tool, process) = four_level_tree();
    assert!(!process.is_cancelled(), "初始状态不该是已取消");

    run.cancel(CancelReason::UserInterrupt);

    for scope in [&run, &turn, &tool, &process] {
        assert!(
            scope.is_cancelled(),
            "`{}` 层没有收到传播下来的取消",
            scope.kind()
        );
    }
}

#[test]
fn test_cancel_contract_02() {
    // One tool timing out must not take the whole run with it, or the loop could not treat the
    // timeout as a single tool failure and carry on.
    let (run, turn, tool, process) = four_level_tree();

    tool.cancel(CancelReason::Timeout);

    assert!(tool.is_cancelled());
    assert!(process.is_cancelled(), "子进程应随工具一起取消");
    assert!(!turn.is_cancelled(), "turn 不该被子作用域的取消带走");
    assert!(!run.is_cancelled(), "run 不该被子作用域的取消带走");
}

// ---------------------------------------------------------------------------
// root-cause attribution
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_03() {
    let (run, turn, tool, process) = four_level_tree();

    run.cancel(CancelReason::UserInterrupt);

    for scope in [&run, &turn, &tool, &process] {
        assert_eq!(
            scope.reason(),
            Some(CancelReason::UserInterrupt),
            "`{}` 层丢失了根因",
            scope.kind()
        );
    }
}

#[test]
fn test_cancel_contract_04() {
    // The tool times out first, then the user interrupts the whole run: that tool still reports a
    // timeout. This is exactly what having no `ParentCancelled` buys — every level reports how it
    // actually died.
    let (run, turn, tool, _process) = four_level_tree();

    tool.cancel(CancelReason::Timeout);
    run.cancel(CancelReason::UserInterrupt);

    assert_eq!(tool.reason(), Some(CancelReason::Timeout));
    assert_eq!(run.reason(), Some(CancelReason::UserInterrupt));
    assert_eq!(
        turn.reason(),
        Some(CancelReason::UserInterrupt),
        "自己没被单独取消过的一层，应报上游根因"
    );
}

#[test]
fn test_cancel_contract_05() {
    let scope = CancelScope::root();

    scope.cancel(CancelReason::Timeout);
    scope.cancel(CancelReason::UserInterrupt);

    assert_eq!(scope.reason(), Some(CancelReason::Timeout));
}

#[test]
fn test_cancel_contract_06() {
    let (run, _turn, tool, _process) = four_level_tree();
    assert!(run.reason().is_none(), "未取消时不该有根因");
    assert!(tool.reason().is_none());

    run.cancel(CancelReason::Shutdown);

    for scope in [&run, &tool] {
        assert_eq!(
            scope.is_cancelled(),
            scope.reason().is_some(),
            "`{}` 层的取消状态与根因不同步",
            scope.kind()
        );
    }
}

#[test]
fn test_cancel_contract_07() {
    // A third-party library only speaks CancellationToken, so bypassing cancel() is unavoidable.
    // The contract requires such a path to **stay inspectable** with a degraded root cause, rather
    // than producing "cancelled but reasonless".
    let scope = CancelScope::root();
    let child = scope.child(ScopeKind::Tool);

    scope.token().cancel();

    assert!(child.is_cancelled());
    assert_eq!(scope.reason(), Some(CancelReason::Unspecified));
    assert_eq!(child.reason(), Some(CancelReason::Unspecified));
}

// ---------------------------------------------------------------------------
// deadlines
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_08() {
    // Otherwise a single tool could grant itself more time than the whole run.
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::from_secs(600)));
    let 原始 = run.deadline().expect("run 应带时限");

    let 收紧 = run
        .clone()
        .with_deadline(Deadline::after(Duration::from_secs(30)));
    let 放宽 = run
        .clone()
        .with_deadline(Deadline::after(Duration::from_secs(86_400)));

    assert!(
        收紧.deadline().expect("应有时限") < 原始,
        "更短的时限应生效"
    );
    assert_eq!(放宽.deadline(), Some(原始), "更长的时限应被忽略");
}

#[test]
fn test_cancel_contract_09() {
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::from_secs(600)));
    let tool = run.child(ScopeKind::Tool);

    assert_eq!(
        tool.deadline(),
        run.deadline(),
        "子作用域应继承父的时限终点"
    );

    let 更紧的工具 = run
        .child(ScopeKind::Tool)
        .with_deadline(Deadline::after(Duration::from_secs(5)));
    assert!(更紧的工具.deadline() < run.deadline());
}

#[test]
fn test_cancel_contract_10() {
    // A deadline is pure data and ra-core arms no timer: with nobody looking at it, it never
    // fires on its own.
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::ZERO));

    assert!(run.deadline().expect("应有时限").is_expired());
    assert!(
        !run.is_cancelled(),
        "is_cancelled 只看取消信号，与底层 token 的视图保持一致"
    );
}

#[test]
fn test_cancel_contract_11() {
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::ZERO));
    let tool = run.child(ScopeKind::Tool);

    let err = run
        .ensure_not_cancelled()
        .expect_err("过期后检查点应返回错误");

    assert!(err.is_cancelled());
    assert_eq!(run.reason(), Some(CancelReason::Deadline));
    assert!(tool.is_cancelled(), "兜底转出来的取消同样要传播到后代");
    assert_eq!(tool.reason(), Some(CancelReason::Deadline));
}

#[test]
fn test_cancel_contract_12() {
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::from_secs(600)));
    assert!(run.ensure_not_cancelled().is_ok());
}

#[test]
fn test_cancel_contract_13() {
    let 未到期 = Deadline::after(Duration::from_secs(600));
    assert!(!未到期.is_expired());
    assert!(未到期.remaining() > Duration::from_secs(500));

    let 已到期 = Deadline::after(Duration::ZERO);
    assert!(已到期.is_expired());
    assert_eq!(
        已到期.remaining(),
        Duration::ZERO,
        "过期后应饱和到零而不是回绕"
    );
}

// ---------------------------------------------------------------------------
// cancellable await points
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cancel_contract_14() {
    let scope = CancelScope::root();
    let out = scope.run(async { 42 }).await.expect("未取消不该失败");
    assert_eq!(out, 42);
}

#[tokio::test]
async fn test_cancel_contract_15() {
    // Even a future that is long since ready gets no chance to run: nothing should produce a side
    // effect after cancellation.
    let scope = CancelScope::root();
    scope.cancel(CancelReason::Superseded);

    let 跑过了 = Arc::new(AtomicBool::new(false));
    let 标记 = Arc::clone(&跑过了);

    let out = scope
        .run(async move { 标记.store(true, Ordering::SeqCst) })
        .await;

    assert!(out.is_err(), "已取消的作用域应直接返回错误");
    assert!(!跑过了.load(Ordering::SeqCst), "future 根本不该被 poll");
}

#[tokio::test]
async fn test_cancel_contract_16() {
    let scope = CancelScope::root();
    let 取消端 = scope.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        取消端.cancel(CancelReason::UserInterrupt);
    });

    let err = scope
        .run(tokio::time::sleep(Duration::from_secs(3600)))
        .await
        .expect_err("取消后应立刻返回，而不是等 future 跑完");

    assert!(err.is_cancelled());
    assert_eq!(scope.reason(), Some(CancelReason::UserInterrupt));
}

#[tokio::test]
async fn test_cancel_contract_17() {
    let run = CancelScope::root();
    let tool = run.child(ScopeKind::Tool);
    let 取消端 = run.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        取消端.cancel(CancelReason::Shutdown);
    });

    let err = tool
        .run(tokio::time::sleep(Duration::from_secs(3600)))
        .await
        .expect_err("父作用域取消后子作用域的 await 点应立刻返回");

    assert!(err.is_cancelled());
    assert_eq!(tool.reason(), Some(CancelReason::Shutdown));
}

#[tokio::test]
async fn test_cancel_contract_18() {
    let run = CancelScope::root();
    let tool = run.child(ScopeKind::Tool);
    let 取消端 = run.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        取消端.cancel(CancelReason::PeerFailure);
    });

    // Without a wake-up this hangs here, and the test timeout is the backstop.
    tool.cancelled().await;
    assert!(tool.is_cancelled());
}

// ---------------------------------------------------------------------------
// handle semantics
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_19() {
    // Hand a spawned task a clone and derive a new level with child; the two must not be mixed.
    let scope = CancelScope::root();
    let 句柄 = scope.clone();

    句柄.cancel(CancelReason::UserInterrupt);

    assert!(scope.is_cancelled(), "clone 应共享同一个取消信号");
    assert_eq!(scope.reason(), Some(CancelReason::UserInterrupt));
}

#[test]
fn test_cancel_contract_20() {
    // This is tokio-util semantics and the very reason CancelOnDrop exists: forgetting to cancel
    // raises no error, it just leaves descendants waiting forever.
    let run = CancelScope::root();
    let tool = run.child(ScopeKind::Tool);
    let 子进程 = tool.child(ScopeKind::Process);

    drop(tool);

    assert!(!子进程.is_cancelled(), "父作用域被 drop 不等于被取消");
}

#[test]
fn test_cancel_contract_21() {
    let run = CancelScope::root();
    let 兄弟 = run.child(ScopeKind::Tool);
    let 观察 = run.child(ScopeKind::Tool);

    {
        let 守卫 = 观察.clone().cancel_on_drop(CancelReason::Superseded);
        assert!(!守卫.scope().is_cancelled(), "守卫在作用域内不该取消");
    }

    assert!(观察.is_cancelled(), "守卫 drop 后应取消被守卫的作用域");
    assert_eq!(观察.reason(), Some(CancelReason::Superseded));
    assert!(!run.is_cancelled(), "守卫只该取消它自己那一层");
    assert!(!兄弟.is_cancelled(), "兄弟作用域不受影响");
}

#[test]
fn test_cancel_contract_22() {
    let run = CancelScope::root();
    let 子进程 = {
        let 守卫 = run
            .child(ScopeKind::Tool)
            .cancel_on_drop(CancelReason::Superseded);
        let 子进程 = 守卫.scope().child(ScopeKind::Process);
        assert!(!子进程.is_cancelled());
        子进程
    };

    assert!(子进程.is_cancelled(), "守卫 drop 后后代应被取消");
    assert_eq!(子进程.reason(), Some(CancelReason::Superseded));
    assert!(!run.is_cancelled());
}

#[test]
fn test_cancel_contract_23() {
    let run = CancelScope::root();
    let tool = {
        let 守卫 = run
            .child(ScopeKind::Tool)
            .cancel_on_drop(CancelReason::Superseded);
        守卫.disarm()
    };

    assert!(!tool.is_cancelled(), "正常收尾后不该再触发取消");
    assert!(tool.reason().is_none());
}

// ---------------------------------------------------------------------------
// reason classification and projections
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_24() {
    let reasons = all_reasons();
    let mut codes: Vec<&str> = reasons.iter().map(CancelReason::code).collect();
    codes.sort_unstable();

    let total = codes.len();
    codes.dedup();
    assert_eq!(
        codes.len(),
        total,
        "存在重复的取消原因 code，trace 归因会串"
    );

    for code in codes {
        assert!(!code.is_empty(), "code 不能为空");
        assert!(
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "code `{code}` 只允许小写字母与下划线——它会作为指标维度值"
        );
    }
}

#[test]
fn test_cancel_contract_25() {
    for reason in all_reasons() {
        assert_eq!(reason.to_string(), reason.code());
    }
}

#[test]
fn test_cancel_contract_26() {
    for reason in all_reasons() {
        assert!(
            !(reason.is_expiry() && reason.is_user_initiated()),
            "`{reason}` 同时算超时与人为中断，两个统计口径会重复计数"
        );
    }

    assert!(CancelReason::Deadline.is_expiry());
    assert!(CancelReason::Timeout.is_expiry());
    assert!(CancelReason::UserInterrupt.is_user_initiated());
    assert!(CancelReason::Shutdown.is_user_initiated());
    assert!(!CancelReason::Superseded.is_expiry());
    assert!(!CancelReason::Superseded.is_user_initiated());
}

#[test]
fn test_cancel_contract_27() {
    for reason in all_reasons() {
        let msg = reason.user_message();
        assert!(!msg.trim().is_empty(), "`{reason}` 的 user_message 为空");
        if matches!(reason, CancelReason::Custom(_)) {
            continue; // a custom reason's label is the prose.
        }
        assert_ne!(msg, reason.code(), "`{reason}` 把机器标识当成了用户文案");
    }
}

// ---------------------------------------------------------------------------
// interface with the error taxonomy
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_28() {
    for reason in all_reasons() {
        let err = ra_core::error::Error::from(reason.clone());
        assert!(err.is_cancelled(), "`{reason}` 应收敛成取消");
        assert_eq!(err.recoverability(), Recoverability::Cancelled);
        assert!(
            !err.recoverability().is_failure(),
            "`{reason}` 被计入失败率了"
        );
        assert!(!err.is_retryable(), "`{reason}` 不该触发重试");
    }
}

#[test]
fn test_cancel_contract_29() {
    let scope = CancelScope::root();
    scope.cancel(CancelReason::UserInterrupt);

    let err = scope.ensure_not_cancelled().expect_err("已取消应返回错误");
    assert!(
        err.user_message()
            .contains(&CancelReason::UserInterrupt.user_message()),
        "错误文案里应能看到取消原因：{}",
        err.user_message()
    );
}

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_30() {
    // Zero would degrade "wait for a terminal state" into "kill immediately", and infinity would
    // make cancellation hang.
    assert!(DRAIN_GRACE > Duration::ZERO);
    assert!(DRAIN_GRACE <= Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// level labels
// ---------------------------------------------------------------------------

#[test]
fn test_cancel_contract_31() {
    assert_eq!(ScopeKind::Run.label(), "run");
    assert_eq!(ScopeKind::Turn.label(), "turn");
    assert_eq!(ScopeKind::Tool.label(), "tool");
    assert_eq!(ScopeKind::Process.label(), "process");
    assert_eq!(ScopeKind::custom("flow_node").label(), "flow_node");

    assert_eq!(
        CancelScope::root().kind(),
        &ScopeKind::Run,
        "根作用域是 run 级"
    );
}
