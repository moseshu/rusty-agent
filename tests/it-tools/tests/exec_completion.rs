//! Completion observation after delivery, retries and per-invocation opt-out.
#![cfg(unix)]
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    sandbox::token_truncation::{TruncationPolicy, formatted_truncate_text, truncate_text},
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_exec::{
    sandbox::ExecEnvironment,
    session::{ExecCompletion, ExecSessionId, ProcessManager},
};
use ra_tools::{
    exec_command::{ExecCommandTool, ExecCompletionObserver, truncated_exec_output},
    write_stdin::WriteStdinTool,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct Observer {
    disabled: AtomicBool,
    reports: Mutex<Vec<ExecCompletion>>,
}
impl ExecCompletionObserver for Observer {
    fn wants_completion(&self, _: &ToolContext<'_>) -> bool {
        !self.disabled.load(Ordering::SeqCst)
    }
    fn completed(&self, _: &ToolContext<'_>, completion: &ExecCompletion) {
        self.reports.lock().unwrap().push(completion.clone());
    }
}
fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("agent"))
        .name("Agent")
        .build()
        .unwrap();
    RunContext::new(RunId::new("run"), &agent)
}
async fn call(tool: &dyn Tool, id: &str, args: Value) -> ToolOutput {
    tool.call(ToolContext::new(&run(), tool, &CallId::new(id), &args))
        .await
        .unwrap()
}
fn args(command: &str, wait: u64) -> Value {
    json!({"cmd":command,"workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":wait,"timeout_ms":null})
}
fn poll(id: &ExecSessionId, chars: &str, wait: u64) -> Value {
    json!({"session_id":id.as_str(),"chars":chars,"until":"done","match_text":null,"control":null,"yield_time_ms":wait})
}
async fn started(exec: &ExecCommandTool) -> ExecSessionId {
    call(exec, "original", args("read l; printf 'finished\\n'", 10)).await;
    exec.process_manager()
        .active_sessions()
        .await
        .pop()
        .unwrap()
}

#[tokio::test]
async fn current_poll_opt_out_consumes_claim_and_advances_cursors() {
    let manager = Arc::new(ProcessManager::default());
    let observer = Arc::new(Observer::default());
    let exec = ExecCommandTool::new()
        .unwrap()
        .with_manager(manager.clone())
        .with_completion_observer(observer.clone());
    let stdin = WriteStdinTool::new(manager.clone())
        .unwrap()
        .with_completion_observer(observer.clone());
    let id = started(&exec).await;
    assert!(observer.reports.lock().unwrap().is_empty());
    observer.disabled.store(true, Ordering::SeqCst);
    let output = call(&stdin, "poll", poll(&id, "q\n", 1000)).await;
    assert!(output.as_text().unwrap().contains("finished"));
    assert_eq!(
        manager.interactive_output_cursors(&id).await.unwrap(),
        (9, 0)
    );
    observer.disabled.store(false, Ordering::SeqCst);
    call(&stdin, "repeat", poll(&id, "", 1000)).await;
    assert!(observer.reports.lock().unwrap().is_empty());
    assert!(
        manager
            .lock_interaction(&id)
            .await
            .unwrap()
            .deliver(true)
            .await
            .completion()
            .is_none()
    );
}

#[tokio::test]
async fn uncaptured_completion_is_consumed_without_notification() {
    let manager = Arc::new(ProcessManager::default());
    let observer = Arc::new(Observer::default());
    let exec = ExecCommandTool::new()
        .unwrap()
        .with_manager(manager.clone());
    let id = started(&exec).await;
    let stdin = WriteStdinTool::new(manager.clone())
        .unwrap()
        .with_completion_observer(observer.clone());
    call(&stdin, "poll", poll(&id, "q\n", 1000)).await;
    assert!(observer.reports.lock().unwrap().is_empty());
    assert!(
        manager
            .lock_interaction(&id)
            .await
            .unwrap()
            .deliver(true)
            .await
            .completion()
            .is_none()
    );
}

#[tokio::test]
async fn exec_opt_out_leaves_no_capture_and_does_not_change_model_text() {
    let manager = Arc::new(ProcessManager::default());
    let observer = Arc::new(Observer::default());
    observer.disabled.store(true, Ordering::SeqCst);
    let exec = ExecCommandTool::new()
        .unwrap()
        .with_manager(manager.clone())
        .with_completion_observer(observer.clone());
    let id = started(&exec).await;
    manager
        .write_stdin(&id, Some("q\n"), false, None)
        .await
        .unwrap();
    manager.wait_for_close(&id).await;
    let delivery = manager
        .lock_interaction(&id)
        .await
        .unwrap()
        .deliver(true)
        .await;
    assert_eq!(delivery.completion().unwrap().output(), None);
    assert!(observer.reports.lock().unwrap().is_empty());
    let off = call(
        &exec,
        "off",
        args("printf 'alpha\\n'; printf 'beta\\n' >&2", 1000),
    )
    .await;
    observer.disabled.store(false, Ordering::SeqCst);
    let on = call(
        &exec,
        "on",
        args("printf 'alpha\\n'; printf 'beta\\n' >&2", 1000),
    )
    .await;
    assert_eq!(off, on);
    assert_eq!(observer.reports.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropped_wait_can_be_completed_by_a_later_poll() {
    let manager = Arc::new(ProcessManager::default());
    let observer = Arc::new(Observer::default());
    let exec = ExecCommandTool::new()
        .unwrap()
        .with_manager(manager.clone())
        .with_completion_observer(observer.clone());
    let stdin = WriteStdinTool::new(manager.clone())
        .unwrap()
        .with_completion_observer(observer.clone());
    let id = started(&exec).await;
    {
        let pending = call(&stdin, "cancelled", poll(&id, "", 1000));
        tokio::pin!(pending);
        tokio::select! { _ = &mut pending => panic!("poll returned early"), () = tokio::task::yield_now() => {} }
    }
    assert!(observer.reports.lock().unwrap().is_empty());
    call(&stdin, "completion", poll(&id, "q\n", 1000)).await;
    call(&stdin, "repeat", poll(&id, "", 1000)).await;
    let reports = observer.reports.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].output(), Some("finished\n"));
    assert_eq!(reports[0].origin_call_id(), Some("original"));
}

#[test]
fn truncation_ports_all_three_branches_and_both_notice_cases() {
    let policy = TruncationPolicy::bytes(80);
    assert_eq!(
        truncated_exec_output(&"x".repeat(200), 0, 200, policy),
        formatted_truncate_text(&"x".repeat(200), policy)
    );
    let marker = ra_exec::output::omission_marker(100);
    assert_eq!(
        truncated_exec_output("tail", 100, 104, policy),
        format!("{marker}\ntail")
    );
    assert_eq!(truncated_exec_output(&marker, 100, 104, policy), marker);
    for text in [
        format!("{marker}{}", "x".repeat(200)),
        format!("{}{marker}{}", "x".repeat(200), "x".repeat(200)),
    ] {
        let truncated = truncate_text(&text, policy);
        let notice = if truncated.contains(&marker) {
            String::new()
        } else {
            format!("{marker}\n")
        };
        assert_eq!(
            truncated_exec_output(&text, 100, 1001, policy),
            format!("Warning: truncated output (original token count: 251)\n{notice}\n{truncated}")
        );
    }
}

struct RetryPolicy;
impl ra_tools::exec_command::ExecCommandPolicy for RetryPolicy {
    fn needs_approval(&self, _: &ToolContext<'_>) -> ra_core::error::Result<bool> {
        Ok(false)
    }
    fn attempt(
        &self,
        _: &ToolContext<'_>,
    ) -> ra_core::error::Result<ra_tools::exec_command::ExecCommandAttempt> {
        use ra_exec::sandbox::unix_local::EnvPolicy;
        Ok(ra_tools::exec_command::ExecCommandAttempt::new(
            ExecEnvironment::default()
                .with_sandbox(ra_exec::sandbox::SandboxPolicy::new(
                    ra_exec::sandbox::SandboxLevel::WorkspaceWrite,
                ))
                .with_env_policy(EnvPolicy::new().with_set("ATTEMPT", "denied")),
        ))
    }
    fn on_sandbox_denial(
        &self,
        _: &ToolContext<'_>,
        _: &ra_tools::exec_command::ExecCommandAttempt,
        _: &ra_exec::output::ExecOutputSummary,
    ) -> ra_core::error::Result<ra_tools::exec_command::SandboxDenialAction> {
        Ok(ra_tools::exec_command::SandboxDenialAction::Retry(
            Box::new(ExecEnvironment::default()),
        ))
    }
}
#[tokio::test]
async fn sandbox_retry_reports_only_the_final_effective_attempt() {
    let observer = Arc::new(Observer::default());
    let exec = ExecCommandTool::new()
        .unwrap()
        .with_command_policy(Arc::new(RetryPolicy))
        .with_completion_observer(observer.clone());
    // The first attempt emits the same refusal text the sandbox classifier consumes.
    call(&exec, "retry", args("if [ \"$ATTEMPT\" = denied ]; then echo 'Operation not permitted' >&2; exit 1; fi; printf retried", 1000)).await;
    let reports = observer.reports.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].output(), Some("retried"));
}
