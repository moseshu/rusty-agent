//! Behaviour of the `write_stdin` tool and its shared session manager.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_exec::session::ProcessManager;
use ra_tools::{exec_command::ExecCommandTool, write_stdin::WriteStdinTool};
use serde_json::json;
use tempfile::TempDir;

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("runner"))
        .name("Runner")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-write-stdin"), agent.as_ref())
}

async fn call<T: Tool>(
    tool: &T,
    arguments: serde_json::Value,
) -> ra_core::error::Result<ToolOutput> {
    let run = run();
    tool.call(ToolContext::new(
        &run,
        tool,
        &CallId::new("call-write-stdin"),
        &arguments,
    ))
    .await
}

async fn observe<T: Tool>(tool: &T, arguments: serde_json::Value) -> ToolOutput {
    let run = run();
    let call_id = CallId::new("call-write-stdin");
    let context = || ToolContext::new(&run, tool, &call_id, &arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping succeeds")
            .expect("write_stdin handles its failures"),
    }
}

fn tools(workspace: &TempDir) -> (ExecCommandTool, WriteStdinTool) {
    let manager = Arc::new(ProcessManager::default());
    (
        ExecCommandTool::rooted(workspace.path())
            .expect("exec_command builds")
            .with_manager(Arc::clone(&manager)),
        WriteStdinTool::new(manager).expect("write_stdin builds"),
    )
}

#[tokio::test]
async fn test_write_stdin_schema_and_identity() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");

    tool.validate().expect("identity and schema agree");
    assert_eq!(tool.origin().qualified_name(), "write_stdin");
    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Always);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Writes characters to a running command session and returns output produced afterward."
        )
    );
    assert_eq!(
        tool.schema().input_schema()["required"],
        json!(["chars", "session_id", "yield_time_ms"])
    );
}

#[tokio::test]
async fn test_write_stdin_delivers_bytes_to_a_pipe_backed_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let started = call(
        &exec_command,
        json!({
            "cmd": "printf 'before\\n'; read line; printf 'reply:%s\\n' \"$line\"; sleep 30",
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null
        }),
    )
    .await
    .expect("command starts and yields");
    assert!(started.as_text().expect("text output").contains("before"));

    let session_id = exec_command
        .process_manager()
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the background command has a session");
    let output = call(
        &write_stdin,
        json!({
            "session_id": session_id.to_string(),
            "chars": "hello from stdin\n",
            "yield_time_ms": 1_000
        }),
    )
    .await
    .expect("interactive input succeeds");

    let text = output.as_text().expect("text output");
    let summary = exec_command
        .process_manager()
        .get_output_summary(&session_id)
        .await
        .expect("the session remains retained");
    assert!(
        text.contains("reply:hello from stdin"),
        "unexpected output: {text}; session stdout: {:?}",
        summary.stdout()
    );
    assert!(!text.contains("before"), "historical output leaked: {text}");

    exec_command.process_manager().cancel(&session_id).await;
}

/// A poll outlives the session it polls and returns output not yet delivered to the model.
///
/// This is the ordinary shape of a background command: it yields at the timeout and finishes while
/// nobody is watching. An empty `chars` is not a write, so the process being gone is not a reason
/// to refuse — the model learns the command exited and with what status, rather than being told its
/// identifier is dead.
///
/// The second poll is the other half of the guarantee: what the first one delivered is not
/// delivered again. Without the delivery mark surviving between calls, either the tail is lost or
/// every poll repeats it, and the pair below is what tells those two apart.
#[tokio::test]
async fn test_an_empty_poll_outlives_the_session_it_polls() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let started = call(
        &exec_command,
        json!({
            "cmd": "printf 'before\\n'; sleep 1; printf 'after\\n'",
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null
        }),
    )
    .await
    .expect("command starts and yields");
    assert!(started.as_text().expect("text output").contains("before"));

    // Captured while the command still runs: a finished session is no longer an active one.
    let session_id = exec_command
        .process_manager()
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the background command has a session");
    // Polled after the command is gone, not while it is still running: a poll that arrives during
    // the wait would be answered by a live session and would prove nothing about a finished one.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let output = call(
        &write_stdin,
        json!({
            "session_id": session_id.to_string(),
            "chars": "",
            "yield_time_ms": 100
        }),
    )
    .await
    .expect("a poll outlives the session it polls");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("after"),
        "a finished session must return output that arrived after yield: {text}"
    );

    let repeated = call(
        &write_stdin,
        json!({
            "session_id": session_id.to_string(),
            "chars": "",
            "yield_time_ms": 100
        }),
    )
    .await
    .expect("a repeated poll succeeds");
    let repeated_text = repeated.as_text().expect("text output");
    assert!(
        repeated_text.contains("exited with status 0"),
        "{repeated_text}"
    );
    assert!(
        !repeated_text.contains("after"),
        "output repeated: {repeated_text}"
    );
}

/// Characters sent to a finished session are still a mistake, and still reported as one.
#[tokio::test]
async fn test_writing_to_a_finished_session_is_reported_to_the_model() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    call(
        &exec_command,
        json!({
            "cmd": "printf 'before\\n'; sleep 1",
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null
        }),
    )
    .await
    .expect("command starts and yields");
    let session_id = exec_command
        .process_manager()
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the background command has a session");
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let output = observe(
        &write_stdin,
        json!({
            "session_id": session_id.to_string(),
            "chars": "too late\n",
            "yield_time_ms": 100
        }),
    )
    .await;

    assert!(
        output
            .as_text()
            .expect("text output")
            .contains("is no longer running"),
        "unexpected output: {:?}",
        output.as_text()
    );
}

#[tokio::test]
async fn test_write_stdin_reports_an_unknown_session_to_the_model() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");
    let output = observe(
        &tool,
        json!({
            "session_id": "exec-missing",
            "chars": "hello\n",
            "yield_time_ms": 10
        }),
    )
    .await;

    assert!(
        output
            .as_text()
            .expect("text output")
            .contains("No running session is known"),
    );
}
