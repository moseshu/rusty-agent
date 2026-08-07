//! `ra-core`：取消契约（R0-4）的行为断言。
//!
//! 这里锁住的是**契约**，不是实现细节：
//! - 取消只向下传播，且传播不改写子作用域自己的根因——归因不在第一跳就丢
//! - 「已取消」与「有根因」永远同时成立——不存在查不出原因的取消
//! - 时限只能收紧不能放宽，且过期一定会在检查点生效（哪怕没人 arm 定时器）
//! - 取消不是失败：不计失败率、不触发重试

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ra_core::cancel::{CancelReason, CancelScope, DRAIN_GRACE, Deadline, ScopeKind};
use ra_core::error::Recoverability;

/// 每个原因各一个实例。新增变体时必须同步补进来。
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

/// 规范的四层树：run -> turn -> tool -> process。
fn 四层树() -> (CancelScope, CancelScope, CancelScope, CancelScope) {
    let run = CancelScope::root();
    let turn = run.child(ScopeKind::Turn);
    let tool = turn.child(ScopeKind::Tool);
    let process = tool.child(ScopeKind::Process);
    (run, turn, tool, process)
}

// ---------------------------------------------------------------------------
// 传播方向
// ---------------------------------------------------------------------------

#[test]
fn 取消传播到所有后代含孙子层() {
    // 「父图取消必须能杀到孙子 run 的子进程」是最容易漏的一处泄漏，
    // 所以这里断言的是整条链，不只是直接子节点。
    let (run, turn, tool, process) = 四层树();
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
fn 取消不向上传播() {
    // 一个工具超时不该把整个 run 干掉——否则 loop 没法把超时当成一次工具失败
    // 继续走下去。
    let (run, turn, tool, process) = 四层树();

    tool.cancel(CancelReason::Timeout);

    assert!(tool.is_cancelled());
    assert!(process.is_cancelled(), "子进程应随工具一起取消");
    assert!(!turn.is_cancelled(), "turn 不该被子作用域的取消带走");
    assert!(!run.is_cancelled(), "run 不该被子作用域的取消带走");
}

// ---------------------------------------------------------------------------
// 根因归因
// ---------------------------------------------------------------------------

#[test]
fn 后代沿链继承根因() {
    let (run, turn, tool, process) = 四层树();

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
fn 先到的根因不被后到的覆盖() {
    // 工具先超时，随后用户中断整个 run：工具那一层仍然是超时。
    // 这正是不设 `ParentCancelled` 的收益——每一层报的都是自己的真实死因。
    let (run, turn, tool, _process) = 四层树();

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
fn 重复取消不改写根因() {
    let scope = CancelScope::root();

    scope.cancel(CancelReason::Timeout);
    scope.cancel(CancelReason::UserInterrupt);

    assert_eq!(scope.reason(), Some(CancelReason::Timeout));
}

#[test]
fn 已取消与有根因永远同时成立() {
    let (run, _turn, tool, _process) = 四层树();
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
fn 裸_token_取消降级为_unspecified() {
    // 第三方库只认 CancellationToken，绕过 cancel() 是无法避免的。
    // 契约要求这种路径**仍然可查**，只是根因降级——而不是出现「已取消但没有原因」。
    let scope = CancelScope::root();
    let child = scope.child(ScopeKind::Tool);

    scope.token().cancel();

    assert!(child.is_cancelled());
    assert_eq!(scope.reason(), Some(CancelReason::Unspecified));
    assert_eq!(child.reason(), Some(CancelReason::Unspecified));
}

// ---------------------------------------------------------------------------
// 时限
// ---------------------------------------------------------------------------

#[test]
fn 时限只能收紧不能放宽() {
    // 否则一个工具就能给自己批一个比整个 run 更长的时限。
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
fn 子作用域继承时限() {
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
fn 过期时限不等于已取消() {
    // deadline 是纯数据，ra-core 不 arm 定时器：没人看它的时候它不会自己触发。
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::ZERO));

    assert!(run.deadline().expect("应有时限").is_expired());
    assert!(
        !run.is_cancelled(),
        "is_cancelled 只看取消信号，与底层 token 的视图保持一致"
    );
}

#[test]
fn 过期时限在检查点转成真取消并传播() {
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
fn 未到期时检查点放行() {
    let run = CancelScope::root().with_deadline(Deadline::after(Duration::from_secs(600)));
    assert!(run.ensure_not_cancelled().is_ok());
}

#[test]
fn deadline_剩余时间随到期而归零() {
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
// 可取消的 await 点
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_正常完成时透传结果() {
    let scope = CancelScope::root();
    let out = scope.run(async { 42 }).await.expect("未取消不该失败");
    assert_eq!(out, 42);
}

#[tokio::test]
async fn 已取消的作用域不再启动新工作() {
    // 即便 future 早已就绪也不给它机会跑：取消之后不该再产生任何副作用。
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
async fn run_在中途取消时返回带根因的错误() {
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
async fn 子作用域的_run_随父取消而返回() {
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
async fn cancelled_在被取消时唤醒() {
    let run = CancelScope::root();
    let tool = run.child(ScopeKind::Tool);
    let 取消端 = run.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        取消端.cancel(CancelReason::PeerFailure);
    });

    // 没被唤醒就会挂死在这里，由测试超时兜底。
    tool.cancelled().await;
    assert!(tool.is_cancelled());
}

// ---------------------------------------------------------------------------
// 句柄语义
// ---------------------------------------------------------------------------

#[test]
fn clone_是同一个作用域而不是新层级() {
    // 交给 spawn 出去的任务用 clone，派生新层级用 child——两者不能混。
    let scope = CancelScope::root();
    let 句柄 = scope.clone();

    句柄.cancel(CancelReason::UserInterrupt);

    assert!(scope.is_cancelled(), "clone 应共享同一个取消信号");
    assert_eq!(scope.reason(), Some(CancelReason::UserInterrupt));
}

#[test]
fn 作用域被_drop_不会取消它() {
    // 这是 tokio-util 的语义，也是 CancelOnDrop 存在的理由：
    // 忘记取消不会报错，只会让后代永远等下去。
    let run = CancelScope::root();
    let tool = run.child(ScopeKind::Tool);
    let 子进程 = tool.child(ScopeKind::Process);

    drop(tool);

    assert!(!子进程.is_cancelled(), "父作用域被 drop 不等于被取消");
}

#[test]
fn cancel_on_drop_在_drop_时取消() {
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
fn cancel_on_drop_取消的是被守卫的那一层及其后代() {
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
fn disarm_之后不再取消() {
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
// 原因的分类与投影
// ---------------------------------------------------------------------------

#[test]
fn reason_code_全局唯一且格式稳定() {
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
fn display_就是_code() {
    for reason in all_reasons() {
        assert_eq!(reason.to_string(), reason.code());
    }
}

#[test]
fn 超时类与人为中断类互斥() {
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
fn user_message_非空且不是_code() {
    for reason in all_reasons() {
        let msg = reason.user_message();
        assert!(!msg.trim().is_empty(), "`{reason}` 的 user_message 为空");
        if matches!(reason, CancelReason::Custom(_)) {
            continue; // 自定义原因的标签本身就是文案。
        }
        assert_ne!(msg, reason.code(), "`{reason}` 把机器标识当成了用户文案");
    }
}

// ---------------------------------------------------------------------------
// 与错误分类学的对接
// ---------------------------------------------------------------------------

#[test]
fn 任何原因收敛成的错误都是取消而非失败() {
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
fn 检查点错误携带面向人的原因文本() {
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
// 常量
// ---------------------------------------------------------------------------

#[test]
fn drain_宽限期是有限的正值() {
    // 零会让「等到终态」退化成「直接强杀」，无穷会让取消挂死。
    assert!(DRAIN_GRACE > Duration::ZERO);
    assert!(DRAIN_GRACE <= Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// 层级标签
// ---------------------------------------------------------------------------

#[test]
fn scope_kind_标签稳定() {
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
