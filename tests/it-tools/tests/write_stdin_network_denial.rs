//! A network denial that stops a command is reported in place of the result of the call that
//! observes it, as Codex's unified exec reports it: the `exec_command` call when the command has not
//! been left running, otherwise the `write_stdin` call that sees the denial or the end.
#![cfg(unix)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_exec::{
    sandbox::ExecEnvironment,
    session::{ExecSessionId, ProcessManager},
};
use ra_tools::{
    exec_command::{
        ExecCommandAttempt, ExecCommandPolicy, ExecCommandTool, ExecNetworkApproval,
        ExecNetworkApprovals,
    },
    write_stdin::WriteStdinTool,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const DENIED: &str = "Network access was denied by the Codex sandbox network proxy.";

/// A deferred approval the case denies by cancelling `denied`. Finishing a denied approval reports
/// `rejection` when the case gave one, and nothing otherwise, so the default message is the tool's.
#[derive(Debug)]
struct FakeApproval {
    denied: CancellationToken,
    rejection: Option<String>,
    finishes: AtomicUsize,
}

#[async_trait]
impl ExecNetworkApproval for FakeApproval {
    async fn finish_after_exit(&self) -> std::result::Result<(), String> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        match &self.rejection {
            Some(rejection) if self.denied.is_cancelled() => Err(rejection.clone()),
            _ => Ok(()),
        }
    }

    fn is_denied(&self) -> bool {
        self.denied.is_cancelled()
    }
}

impl FakeApproval {
    fn finished(&self) -> bool {
        self.finishes.load(Ordering::SeqCst) > 0
    }
}

fn approval(rejection: Option<&str>) -> Arc<FakeApproval> {
    Arc::new(FakeApproval {
        denied: CancellationToken::new(),
        rejection: rejection.map(str::to_owned),
        finishes: AtomicUsize::new(0),
    })
}

/// Runs every command under `approval`. When `stops` is set the denial also cancels the command,
/// as the host's approval does through the attempt's environment; otherwise the command runs on
/// and only the approval knows it was denied.
struct NetworkPolicy {
    approval: Arc<FakeApproval>,
    stops: bool,
}

impl ExecCommandPolicy for NetworkPolicy {
    fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(false)
    }

    fn attempt(&self, _context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
        let environment = if self.stops {
            ExecEnvironment::default().with_cancellation(self.approval.denied.clone())
        } else {
            ExecEnvironment::default()
        };
        Ok(ExecCommandAttempt::new(environment).with_network_approval(self.approval.clone()))
    }
}

fn tools(
    workspace: &TempDir,
    approval: &Arc<FakeApproval>,
    stops: bool,
) -> (ExecCommandTool, WriteStdinTool) {
    tools_with(
        workspace,
        Arc::new(NetworkPolicy {
            approval: Arc::clone(approval),
            stops,
        }),
    )
}

fn tools_with(
    workspace: &TempDir,
    policy: Arc<dyn ExecCommandPolicy>,
) -> (ExecCommandTool, WriteStdinTool) {
    let manager = Arc::new(ProcessManager::default());
    let approvals = Arc::new(ExecNetworkApprovals::new());
    (
        ExecCommandTool::rooted(workspace.path())
            .expect("exec_command builds")
            .with_manager(Arc::clone(&manager))
            .with_command_policy(policy)
            .with_network_approvals(Arc::clone(&approvals)),
        WriteStdinTool::new(manager)
            .expect("write_stdin builds")
            .with_network_approvals(approvals),
    )
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("runner"))
        .name("Runner")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-network-denial"), agent.as_ref())
}

async fn call<T: Tool>(tool: &T, arguments: serde_json::Value) -> Result<ToolOutput> {
    let run = run();
    let call_id = CallId::new("call-network-denial");
    tool.call(ToolContext::new(&run, tool, &call_id, &arguments))
        .await
}

fn command(command: &str, yield_time_ms: u64) -> serde_json::Value {
    json!({
        "cmd": command,
        "workdir": null,
        "shell": null,
        "tty": null,
        "login": null,
        "yield_time_ms": yield_time_ms,
        "timeout_ms": null,
        "sandbox_permissions": null,
        "justification": null,
        "prefix_rule": null
    })
}

async fn start(exec_command: &ExecCommandTool, command_line: &str) -> String {
    call(exec_command, command(command_line, 100))
        .await
        .expect("the command starts and yields");
    exec_command
        .process_manager()
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the command left a session")
        .to_string()
}

fn poll(session_id: &str) -> serde_json::Value {
    write(session_id, "")
}

fn write(session_id: &str, chars: &str) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "chars": chars,
        "until": "done",
        "match_text": null,
        "control": null,
        "yield_time_ms": 5000
    })
}

/// Waits, boundedly, for `condition` to hold.
async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the condition did not hold within five seconds");
}

#[tokio::test]
async fn a_denial_that_cancelled_a_running_command_is_its_poll_result() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 30").await;

    // The review denies the access: the environment's token ends the command.
    approval.denied.cancel();
    let error = call(&write_stdin, poll(&session_id))
        .await
        .expect_err("the denial replaces the result");

    assert!(error.to_string().contains(DENIED), "{error}");
    assert!(approval.finished());
    // Codex releases the process whose result the denial replaced.
    let manager = Arc::clone(exec_command.process_manager());
    let id = ExecSessionId::new(session_id.clone());
    eventually(async || manager.get_session_state(&id).await.is_none()).await;
    let error = call(&write_stdin, poll(&session_id)).await.unwrap_err();
    assert!(!error.to_string().contains(DENIED), "{error}");
}

#[tokio::test]
async fn the_review_rejection_is_reported_in_place_of_the_default_message() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(Some("rejected by user"));
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 30").await;

    approval.denied.cancel();
    let error = call(&write_stdin, poll(&session_id)).await.unwrap_err();

    assert!(error.to_string().contains("rejected by user"), "{error}");
}

#[tokio::test]
async fn a_command_that_ends_cleanly_reports_its_output() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 1; echo finished").await;

    let output = call(&write_stdin, poll(&session_id))
        .await
        .expect("an approved command is reported normally");

    assert!(output.as_text().unwrap().contains("finished"), "{output:?}");
}

/// Nobody polls the command, and its approval is still finished once it ends.
#[tokio::test]
async fn a_command_left_running_has_its_approval_finished_when_it_ends() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 0.3; echo finished").await;
    assert!(!approval.finished());

    eventually(async || approval.finished()).await;

    let output = call(&write_stdin, poll(&session_id))
        .await
        .expect("the finished approval left nothing to report");
    assert!(output.as_text().unwrap().contains("finished"), "{output:?}");
}

#[tokio::test]
async fn a_command_denied_before_it_starts_fails_its_start() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    approval.denied.cancel();
    let (exec_command, _write_stdin) = tools(&workspace, &approval, true);

    let error = call(&exec_command, command("sleep 30", 100))
        .await
        .expect_err("a denied command is a failure");

    assert!(error.to_string().contains(DENIED), "{error}");
}

/// The command ends within its initial wait, after a denial it outlived: the denial is the result.
#[tokio::test]
async fn a_short_command_denied_while_it_runs_reports_the_denial() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, _write_stdin) = tools(&workspace, &approval, false);
    let denied = approval.denied.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        denied.cancel();
    });

    let error = call(&exec_command, command("sleep 0.3; echo done", 5000))
        .await
        .expect_err("the denial replaces the output");

    assert!(error.to_string().contains(DENIED), "{error}");
}

/// The denial arrives during the initial wait and the command is still running when it yields:
/// the start fails and the session is released rather than handed back.
#[tokio::test]
async fn a_denial_during_the_initial_wait_fails_the_yielded_start() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, _write_stdin) = tools(&workspace, &approval, false);
    let manager = Arc::clone(exec_command.process_manager());
    let denied = approval.denied.clone();
    let observed = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let session = manager.active_sessions().await.into_iter().next();
            denied.cancel();
            session.expect("the command is running")
        })
    };

    let error = call(&exec_command, command("sleep 30", 500))
        .await
        .expect_err("a command denied before it yields fails its start");

    assert!(error.to_string().contains(DENIED), "{error}");
    let session = observed.await.unwrap();
    eventually(async || manager.get_session_state(&session).await.is_none()).await;
}

/// Input to a command the denial already stopped reports the denial, not a session that is no
/// longer active.
#[tokio::test]
async fn input_to_a_command_the_denial_stopped_reports_the_denial() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 30").await;
    approval.denied.cancel();
    let manager = Arc::clone(exec_command.process_manager());
    let id = ExecSessionId::new(session_id.clone());
    eventually(async || {
        manager
            .get_session_state(&id)
            .await
            .is_some_and(|state| state.is_terminal())
    })
    .await;

    let error = call(&write_stdin, write(&session_id, "hello\n"))
        .await
        .expect_err("the denial replaces the result");

    assert!(error.to_string().contains(DENIED), "{error}");
}

/// A failed interrupt must still report the network denial that stopped the command.
#[tokio::test]
async fn interrupt_to_a_command_the_denial_stopped_reports_the_denial() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(Some("rejected by user"));
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 30").await;
    approval.denied.cancel();
    let manager = Arc::clone(exec_command.process_manager());
    let id = ExecSessionId::new(session_id.clone());
    eventually(async || {
        manager
            .get_session_state(&id)
            .await
            .is_some_and(|state| state.is_terminal())
    })
    .await;

    let mut arguments = poll(&session_id);
    arguments["control"] = json!("interrupt");
    let error = call(&write_stdin, arguments).await.unwrap_err();

    assert!(error.to_string().contains("rejected by user"), "{error}");
    eventually(async || manager.get_session_state(&id).await.is_none()).await;
}

/// A failed spawn finishes its approval and gives a reviewed rejection precedence over the spawn
/// error. Without a rejection, the original spawn error is preserved.
#[tokio::test]
async fn a_failed_spawn_finishes_its_network_approval() {
    for denied in [false, true] {
        let workspace = TempDir::new().unwrap();
        let approval = approval(Some("rejected by user"));
        let (exec_command, _) = tools(&workspace, &approval, false);
        if denied {
            approval.denied.cancel();
        }
        let mut arguments = command("echo unused", 100);
        arguments["shell"] = json!(workspace.path().join("missing-shell"));

        let error = call(&exec_command, arguments).await.unwrap_err();

        assert_eq!(approval.finishes.load(Ordering::SeqCst), 1, "{error}");
        if denied {
            assert!(error.to_string().contains("rejected by user"), "{error}");
        } else {
            assert!(
                error.to_string().contains("could not be started"),
                "{error}"
            );
        }
    }
}

/// Input to a command that ended without a denial keeps the failure it always had.
#[tokio::test]
async fn input_to_a_command_that_ended_cleanly_still_fails_as_before() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval, true);
    let session_id = start(&exec_command, "sleep 0.3").await;
    let manager = Arc::clone(exec_command.process_manager());
    let id = ExecSessionId::new(session_id.clone());
    eventually(async || {
        manager
            .get_session_state(&id)
            .await
            .is_some_and(|state| state.is_terminal())
    })
    .await;

    let error = call(&write_stdin, write(&session_id, "hello\n"))
        .await
        .expect_err("a write to an ended command fails");

    assert!(!error.to_string().contains(DENIED), "{error}");
}

/// The retry cases need a sandbox that refuses a write outside the workspace as the command starts.
#[cfg(target_os = "macos")]
mod sandbox_retry {
    use std::sync::atomic::AtomicBool;

    use ra_exec::{
        output::ExecOutputSummary,
        sandbox::{SandboxLevel, SandboxPolicy},
    };
    use ra_tools::exec_command::SandboxDenialAction;

    use super::*;

    struct RetryPolicy {
        first: Arc<FakeApproval>,
        retry: Arc<FakeApproval>,
        ask: bool,
        retried: AtomicBool,
        fail_spawn: bool,
    }

    impl ExecCommandPolicy for RetryPolicy {
        fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
            Ok(false)
        }

        fn attempt(&self, _context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
            Ok(ExecCommandAttempt::new(
                ExecEnvironment::default()
                    .with_sandbox(SandboxPolicy::new(SandboxLevel::WorkspaceWrite)),
            )
            .with_network_approval(self.first.clone()))
        }

        fn on_sandbox_denial(
            &self,
            _context: &ToolContext<'_>,
            _attempt: &ExecCommandAttempt,
            _output: &ExecOutputSummary,
        ) -> Result<SandboxDenialAction> {
            Ok(if self.ask {
                SandboxDenialAction::RequestApproval(json!({}))
            } else {
                SandboxDenialAction::Retry(Box::new(ExecEnvironment::default()))
            })
        }

        fn retry_attempt(
            &self,
            _context: &ToolContext<'_>,
            _attempt: &ExecCommandAttempt,
            environment: ExecEnvironment,
        ) -> Result<ExecCommandAttempt> {
            self.retried.store(true, Ordering::SeqCst);
            let environment = if self.fail_spawn {
                environment.with_env_policy(
                    ra_exec::sandbox::unix_local::EnvPolicy::new().with_set("INVALID_VALUE", "\0"),
                )
            } else {
                environment
            };
            Ok(ExecCommandAttempt::new(environment).with_network_approval(self.retry.clone()))
        }
    }

    fn policy(first: &Arc<FakeApproval>, ask: bool) -> (Arc<RetryPolicy>, Arc<FakeApproval>) {
        let retry = approval(None);
        (
            Arc::new(RetryPolicy {
                first: Arc::clone(first),
                retry: Arc::clone(&retry),
                ask,
                retried: AtomicBool::new(false),
                fail_spawn: false,
            }),
            retry,
        )
    }

    #[tokio::test]
    async fn a_refused_attempt_denied_network_access_is_not_retried() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("written.txt");
        let first = approval(None);
        first.denied.cancel();
        let (policy, _retry) = policy(&first, false);
        let (exec_command, _) = tools_with(&workspace, policy.clone());

        let error = call(
            &exec_command,
            command(&format!("touch '{}'", target.display()), 5000),
        )
        .await
        .expect_err("the denial is the result");

        assert!(error.to_string().contains(DENIED), "{error}");
        assert!(!policy.retried.load(Ordering::SeqCst));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn a_refused_attempt_denied_network_access_asks_nothing() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("written.txt");
        let first = approval(Some("rejected by user"));
        first.denied.cancel();
        let (policy, _retry) = policy(&first, true);
        let (exec_command, _) = tools_with(&workspace, policy);

        let error = call(
            &exec_command,
            command(&format!("touch '{}'", target.display()), 5000),
        )
        .await
        .expect_err("the rejection is the result");

        assert!(error.to_string().contains("rejected by user"), "{error}");
    }

    #[tokio::test]
    async fn a_retry_runs_under_its_own_network_approval() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("written.txt");
        let first = approval(None);
        let (policy, retry) = policy(&first, false);
        let (exec_command, _) = tools_with(&workspace, policy.clone());

        call(
            &exec_command,
            command(&format!("touch '{}'", target.display()), 5000),
        )
        .await
        .expect("the retry runs");

        assert!(policy.retried.load(Ordering::SeqCst));
        assert!(target.exists());
        assert_eq!(first.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(retry.finishes.load(Ordering::SeqCst), 1);
    }

    /// An invalid environment makes only the retry fail to spawn, so its approval must be
    /// finished on the error path rather than on a completed process result.
    #[tokio::test]
    async fn a_retry_that_fails_to_spawn_finishes_its_own_network_approval() {
        for denied in [false, true] {
            let workspace = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let target = outside.path().join("written.txt");
            let first = approval(None);
            let (mut policy, retry) = policy(&first, false);
            Arc::get_mut(&mut policy).unwrap().fail_spawn = true;
            if denied {
                retry.denied.cancel();
            }
            let (exec_command, _) = tools_with(&workspace, policy.clone());

            let error = call(
                &exec_command,
                command(&format!("touch '{}'", target.display()), 5000),
            )
            .await
            .unwrap_err();

            assert!(policy.retried.load(Ordering::SeqCst), "{error}");
            assert_eq!(first.finishes.load(Ordering::SeqCst), 1);
            assert_eq!(retry.finishes.load(Ordering::SeqCst), 1, "{error}");
            assert!(!target.exists());
            if denied {
                assert!(error.to_string().contains(DENIED), "{error}");
            } else {
                assert!(
                    error.to_string().contains("could not be started"),
                    "{error}"
                );
            }
        }
    }

    /// Once the refused attempt's approval is finished clean, a later denial of the retry's own
    /// approval is the call's result.
    #[tokio::test]
    async fn a_denied_retry_reports_its_own_denial() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("written.txt");
        let first = approval(None);
        let (policy, retry) = policy(&first, false);
        retry.denied.cancel();
        let (exec_command, _) = tools_with(&workspace, policy);

        let error = call(
            &exec_command,
            command(&format!("touch '{}'", target.display()), 5000),
        )
        .await
        .expect_err("the retry's denial is the result");

        assert!(error.to_string().contains(DENIED), "{error}");
    }
}
