//! A network denial that stops a command left running is reported by the `write_stdin` call that
//! observes it, as Codex's unified exec reports it from the process entry.
#![cfg(unix)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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
use ra_exec::{sandbox::ExecEnvironment, session::ProcessManager};
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

/// A deferred approval whose denial is whatever the case flips.
#[derive(Debug)]
struct FakeApproval {
    token: CancellationToken,
    rejection: Option<String>,
    finished: AtomicBool,
}

#[async_trait]
impl ExecNetworkApproval for FakeApproval {
    async fn finish_after_exit(&self) -> std::result::Result<(), String> {
        self.finished.store(true, Ordering::SeqCst);
        match (&self.rejection, self.token.is_cancelled()) {
            (Some(rejection), _) => Err(rejection.clone()),
            (None, true) => {
                Err("Network access was denied by the Codex sandbox network proxy.".into())
            }
            (None, false) => Ok(()),
        }
    }

    fn is_denied(&self) -> bool {
        self.token.is_cancelled()
    }
}

struct NetworkPolicy {
    approval: Arc<FakeApproval>,
}

impl ExecCommandPolicy for NetworkPolicy {
    fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(false)
    }

    fn attempt(&self, _context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
        Ok(ExecCommandAttempt::new(
            ExecEnvironment::default().with_cancellation(self.approval.token.clone()),
        )
        .with_network_approval(self.approval.clone()))
    }
}

fn approval(rejection: Option<&str>) -> Arc<FakeApproval> {
    Arc::new(FakeApproval {
        token: CancellationToken::new(),
        rejection: rejection.map(str::to_owned),
        finished: AtomicBool::new(false),
    })
}

fn tools(workspace: &TempDir, approval: &Arc<FakeApproval>) -> (ExecCommandTool, WriteStdinTool) {
    let manager = Arc::new(ProcessManager::default());
    let approvals = Arc::new(ExecNetworkApprovals::new());
    (
        ExecCommandTool::rooted(workspace.path())
            .expect("exec_command builds")
            .with_manager(Arc::clone(&manager))
            .with_command_policy(Arc::new(NetworkPolicy {
                approval: Arc::clone(approval),
            }))
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

async fn start(exec_command: &ExecCommandTool, command: &str) -> String {
    call(
        exec_command,
        json!({
            "cmd": command,
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null,
            "sandbox_permissions": null,
            "justification": null,
            "prefix_rule": null
        }),
    )
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
    json!({
        "session_id": session_id,
        "chars": "",
        "until": "done",
        "match_text": null,
        "control": null,
        "yield_time_ms": 5000
    })
}

#[tokio::test]
async fn a_denial_that_cancelled_a_running_command_is_its_poll_result() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval);
    let session_id = start(&exec_command, "sleep 30").await;

    // The review denies the access: the environment's token ends the command.
    approval.token.cancel();
    let error = call(&write_stdin, poll(&session_id))
        .await
        .expect_err("the denial replaces the result");

    assert!(
        error
            .to_string()
            .contains("Network access was denied by the Codex sandbox network proxy."),
        "{error}"
    );
    assert!(approval.finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_review_rejection_is_reported_in_place_of_the_default_message() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(Some("rejected by user"));
    let (exec_command, write_stdin) = tools(&workspace, &approval);
    let session_id = start(&exec_command, "sleep 30").await;

    approval.token.cancel();
    let error = call(&write_stdin, poll(&session_id)).await.unwrap_err();

    assert!(error.to_string().contains("rejected by user"), "{error}");
}

#[tokio::test]
async fn a_command_that_ends_cleanly_reports_its_output() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    let (exec_command, write_stdin) = tools(&workspace, &approval);
    let session_id = start(&exec_command, "sleep 1; echo finished").await;

    let output = call(&write_stdin, poll(&session_id))
        .await
        .expect("an approved command is reported normally");

    assert!(output.as_text().unwrap().contains("finished"), "{output:?}");
}

#[tokio::test]
async fn a_command_denied_before_it_yields_fails_its_start() {
    let workspace = TempDir::new().unwrap();
    let approval = approval(None);
    approval.token.cancel();
    let (exec_command, _write_stdin) = tools(&workspace, &approval);

    let error = call(
        &exec_command,
        json!({
            "cmd": "sleep 30",
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null,
            "sandbox_permissions": null,
            "justification": null,
            "prefix_rule": null
        }),
    )
    .await
    .expect_err("a denied command is a failure");

    assert!(error.to_string().contains("denied"), "{error}");
}
