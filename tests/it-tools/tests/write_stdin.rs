//! Behaviour of the `write_stdin` tool and its shared session manager.

use std::{sync::Arc, time::Duration};

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_exec::{command::ExecLimits, session::ProcessManager};
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
    tools_with(workspace, ProcessManager::default())
}

fn tools_with(workspace: &TempDir, manager: ProcessManager) -> (ExecCommandTool, WriteStdinTool) {
    let manager = Arc::new(manager);
    (
        ExecCommandTool::rooted(workspace.path())
            .expect("exec_command builds")
            .with_manager(Arc::clone(&manager)),
        WriteStdinTool::new(manager).expect("write_stdin builds"),
    )
}

/// Starts `command` in the background and returns the session the model would be handed.
async fn background(exec_command: &ExecCommandTool, command: &str) -> String {
    call(
        exec_command,
        json!({
            "cmd": command,
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
    exec_command
        .process_manager()
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the background command has a session")
        .to_string()
}

/// The arguments a wait-only call sends.
fn wait_args(session_id: &str, until: &str, yield_time_ms: u64) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "chars": "",
        "until": until,
        "match_text": null,
        "control": null,
        "yield_time_ms": yield_time_ms
    })
}

#[tokio::test]
async fn test_write_stdin_schema_and_identity() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");

    tool.validate().expect("identity and schema agree");
    assert_eq!(tool.origin().qualified_name(), "write_stdin");
    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Always);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Writes characters to a running command session, then waits and returns output not yet delivered."
        )
    );
    assert_eq!(
        tool.schema().input_schema()["required"],
        json!([
            "chars",
            "control",
            "match_text",
            "session_id",
            "until",
            "yield_time_ms"
        ])
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

/// `until: done` returns when the command is over *and* its output has been collected.
#[tokio::test]
async fn test_waiting_for_done_returns_a_complete_result() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(
        &exec_command,
        "printf 'starting\\n'; sleep 0.4; printf 'finished\\n'",
    )
    .await;

    let output = call(&write_stdin, wait_args(&session_id, "done", 5_000))
        .await
        .expect("waiting for a command to finish succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("finished"),
        "the tail printed during the wait must be delivered: {text}"
    );
    assert!(
        text.contains("exited with status 0") && text.contains("its output is complete"),
        "a done result must say how it ended and that nothing is left: {text}"
    );
    assert!(
        output.metadata().guidance().is_empty(),
        "a finished session has no next step to suggest"
    );
}

/// Reaching the limit ends the wait and nothing else: the command keeps running.
#[tokio::test]
async fn test_waiting_for_done_reports_a_command_that_is_still_running() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(&exec_command, "sleep 30").await;

    let output = call(&write_stdin, wait_args(&session_id, "done", 150))
        .await
        .expect("a bounded wait succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("the command has not finished"),
        "a wait that ran out must say what it was waiting for: {text}"
    );
    assert!(text.contains("is still running"), "{text}");
    assert!(
        exec_command
            .process_manager()
            .active_sessions()
            .await
            .iter()
            .any(|id| id.to_string() == session_id),
        "reaching the wait limit must not stop the command"
    );

    exec_command
        .process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;
}

/// A match searches everything the session has produced, including output already delivered.
///
/// The alternative — searching only what arrives after the call — loses every handshake where the
/// line being waited for was printed before anyone thought to wait for it, which is most of them.
/// The delivered bytes are a separate question, and this result answers both: matched on text from
/// before, with no new output to show for it.
#[tokio::test]
async fn test_a_match_sees_output_printed_before_the_call() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(&exec_command, "printf 'server READY on 8080\\n'; sleep 30").await;

    let output = call(
        &write_stdin,
        json!({
            "session_id": session_id,
            "chars": "",
            "until": "match",
            "match_text": "READY",
            "control": null,
            "yield_time_ms": 1_000
        }),
    )
    .await
    .expect("matching succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("Matched on stdout"),
        "a match must name the stream it matched on: {text}"
    );
    assert!(
        text.contains("server READY on 8080"),
        "a match must show what it matched: {text}"
    );
    assert!(
        text.contains("No new output"),
        "matching already-delivered output must not redeliver it: {text}"
    );

    exec_command
        .process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;
}

/// A match that never arrives runs out of time and leaves the command alone.
#[tokio::test]
async fn test_a_match_that_never_arrives_times_out() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(&exec_command, "sleep 30").await;

    let output = call(
        &write_stdin,
        json!({
            "session_id": session_id,
            "chars": "",
            "until": "match",
            "match_text": "never printed",
            "control": null,
            "yield_time_ms": 150
        }),
    )
    .await
    .expect("a bounded match succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("without `never printed` appearing"),
        "a match that ran out must name what it was looking for: {text}"
    );
    assert!(text.contains("is still running"), "{text}");

    exec_command
        .process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;
}

/// The wait condition and the text to match have to agree, in both directions.
#[tokio::test]
async fn test_a_match_condition_and_its_text_must_agree() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");

    let missing = observe(
        &tool,
        json!({
            "session_id": "exec-1",
            "chars": "",
            "until": "match",
            "match_text": null,
            "control": null,
            "yield_time_ms": 10
        }),
    )
    .await;
    assert!(
        missing
            .as_text()
            .expect("text output")
            .contains("needs `match_text`"),
        "{:?}",
        missing.as_text()
    );

    let unused = observe(
        &tool,
        json!({
            "session_id": "exec-1",
            "chars": "",
            "until": "output",
            "match_text": "ready",
            "control": null,
            "yield_time_ms": 10
        }),
    )
    .await;
    assert!(
        unused
            .as_text()
            .expect("text output")
            .contains("only applies to `until: match`"),
        "{:?}",
        unused.as_text()
    );
}

/// A signal is not input, so asking for both at once is refused rather than guessed at.
#[tokio::test]
async fn test_control_refuses_to_travel_with_input() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");

    let output = observe(
        &tool,
        json!({
            "session_id": "exec-1",
            "chars": "yes\n",
            "until": "output",
            "match_text": null,
            "control": "cancel",
            "yield_time_ms": 10
        }),
    )
    .await;

    assert!(
        output
            .as_text()
            .expect("text output")
            .contains("`chars` must be empty"),
        "{:?}",
        output.as_text()
    );
}

/// Cancelling records the death immediately, and the result says the output is not all in yet.
///
/// The two facts are days apart in importance and up to a grace period apart in time. A command
/// that ignores `SIGTERM` keeps running — and keeps printing — until the escalation reaches it, so
/// a result that reported "cancelled" as if it meant "finished" would be dropping whatever the
/// command said on its way out.
#[tokio::test]
async fn test_cancelling_separates_the_request_from_the_collection() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(&exec_command, "trap '' TERM; while :; do sleep 0.2; done").await;

    let requested = call(
        &write_stdin,
        json!({
            "session_id": session_id,
            "chars": "",
            "until": "done",
            "match_text": null,
            "control": "cancel",
            "yield_time_ms": 200
        }),
    )
    .await
    .expect("cancelling succeeds");
    let text = requested.as_text().expect("text output");
    assert!(
        text.contains("was cancelled") && text.contains("not complete yet"),
        "a draining session must not be reported as collected: {text}"
    );

    let collected = call(&write_stdin, wait_args(&session_id, "done", 5_000))
        .await
        .expect("waiting out the drain succeeds");
    let text = collected.as_text().expect("text output");
    assert!(
        text.contains("was cancelled") && text.contains("its output is complete"),
        "the escalation must produce a complete result: {text}"
    );
}

/// An interrupt reaches the process group, and the program gets to stop on its own terms.
#[tokio::test]
async fn test_an_interrupt_lets_a_command_stop_on_its_own_terms() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools(&workspace);
    let session_id = background(
        &exec_command,
        "trap 'printf caught; exit 3' INT; while :; do sleep 0.2; done",
    )
    .await;

    let output = call(
        &write_stdin,
        json!({
            "session_id": session_id,
            "chars": "",
            "until": "done",
            "match_text": null,
            "control": "interrupt",
            "yield_time_ms": 5_000
        }),
    )
    .await
    .expect("interrupting succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("caught"),
        "the handler's own output must be delivered: {text}"
    );
    assert!(
        text.contains("exited with status 3"),
        "the command's own answer must survive the interrupt: {text}"
    );
}

/// A session being waited on is not an idle session.
#[tokio::test]
async fn test_waiting_keeps_a_silent_session_from_the_idle_sweep() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (exec_command, write_stdin) = tools_with(
        &workspace,
        ProcessManager::new(ExecLimits::new().with_idle_timeout(Some(Duration::from_millis(150)))),
    );
    let session_id = background(&exec_command, "sleep 30").await;

    let output = call(&write_stdin, wait_args(&session_id, "done", 900))
        .await
        .expect("a bounded wait succeeds");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("is still running"),
        "a session was swept out from under the wait watching it: {text}"
    );

    exec_command
        .process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;
}

/// Session queueing must occur inside the tool's deadline, not in runtime resource admission.
#[tokio::test]
async fn test_write_stdin_leaves_session_queueing_to_its_bounded_guard() {
    let manager = Arc::new(ProcessManager::default());
    let tool = WriteStdinTool::new(manager).expect("write_stdin builds");
    let run = run();
    let call_id = CallId::new("call-write-stdin");
    let arguments = wait_args("exec-42", "output", 10);

    let claims = tool
        .resource_claims(&ToolContext::new(&run, &tool, &call_id, &arguments))
        .await
        .expect("claims resolve");

    assert!(
        claims.is_empty(),
        "session admission must not precede the deadline: {claims:?}"
    );
}

#[tokio::test]
async fn test_poll_preserves_stream_whitespace() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let id = background(&exec, "read line; printf '  indented  '; sleep 2").await;
    let result = call(
        &stdin,
        json!({"session_id":id,"chars":"go\n","yield_time_ms":1000}),
    )
    .await
    .expect("input and output");
    exec.process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(id))
        .await;
    assert!(
        result.as_text().expect("text").contains("  indented  "),
        "{result:?}"
    );
}

#[tokio::test]
async fn test_cancel_bypasses_a_blocked_input_and_delivery_lock() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let id = background(&exec, "sleep 30").await;
    let stdin = Arc::new(stdin);
    let writer_tool = Arc::clone(&stdin);
    let writer_id = id.clone();
    let writer = tokio::spawn(async move {
        call(
            writer_tool.as_ref(),
            json!({
                "session_id":writer_id,"chars":"x".repeat(8 * 1024 * 1024),"yield_time_ms":1000
            }),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !writer.is_finished(),
        "the pipe write should still be blocked"
    );
    let args =
        json!({"session_id":id,"chars":"","control":"cancel","until":"done","yield_time_ms":100});
    tokio::time::timeout(Duration::from_secs(2), call(stdin.as_ref(), args))
        .await
        .expect("cancel must not wait for input")
        .expect("cancel succeeds");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("write unblocks")
            .expect("writer task")
            .is_err()
    );
    assert!(matches!(
        exec.process_manager()
            .get_session_state(&ra_core::event::exec::ExecSessionId::new(id))
            .await,
        Some(ra_exec::session::ExecSessionState::Cancelled)
    ));
}

#[tokio::test]
async fn test_blocked_input_has_a_deadline_and_warns_about_partial_delivery() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let id = background(&exec, "sleep 30").await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        observe(
            &stdin,
            json!({
                "session_id":id,"chars":"x".repeat(8 * 1024 * 1024),"yield_time_ms":50
            }),
        ),
    )
    .await;
    exec.process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(id))
        .await;
    let output = result.expect("input is bounded");
    assert!(
        output
            .as_text()
            .expect("text")
            .contains("Some bytes may already have been written")
    );
}

/// A call that never got to look says so, rather than reporting a wait that saw nothing.
///
/// Contention reports only what this call did, without promising that output remains available.
#[tokio::test]
async fn test_a_call_that_cannot_take_delivery_says_nothing_was_collected() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let session_id = background(&exec, "sleep 30").await;
    let stdin = Arc::new(stdin);

    let id = ra_core::event::exec::ExecSessionId::new(session_id.clone());
    let holder = exec
        .process_manager()
        .lock_interaction(&id)
        .await
        .expect("delivery lock");

    let output = call(stdin.as_ref(), wait_args(&session_id, "output", 100))
        .await
        .expect("a contended call still answers");

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("Another call still owned"),
        "a call that never looked must not read as a wait that saw nothing: {text}"
    );
    assert!(
        text.contains("did not inspect or collect output") && text.contains("No input was sent"),
        "{text}"
    );
    assert!(
        text.contains("is still running"),
        "where the session stands is still part of the answer: {text}"
    );
    assert!(
        !text.contains("No new output"),
        "the headline already said nothing was collected: {text}"
    );
    assert!(
        !output.metadata().guidance().is_empty(),
        "a live session still has a next step"
    );

    drop(holder);
    exec.process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;
}

/// The budget the model named bounds the call, not each phase of it.
#[tokio::test]
async fn test_the_wait_budget_covers_the_whole_call() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let session_id = background(&exec, "read line; sleep 30").await;
    let id = ra_core::event::exec::ExecSessionId::new(session_id.clone());
    let holder = exec
        .process_manager()
        .lock_interaction(&id)
        .await
        .expect("delivery lock");
    // Spend most of the budget in a real lock wait, then enter a wait that cannot finish early.
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(650)).await;
        drop(holder);
    });

    let started = std::time::Instant::now();
    call(
        &stdin,
        json!({
            "session_id": session_id,
            "chars": "go\n",
            "until": "done",
            "match_text": null,
            "control": null,
            "yield_time_ms": 1000
        }),
    )
    .await
    .expect("the write and the wait both happen");
    let elapsed = started.elapsed();
    release.await.expect("release task");

    exec.process_manager()
        .cancel(&ra_core::event::exec::ExecSessionId::new(session_id))
        .await;

    assert!(
        elapsed < Duration::from_millis(1_400),
        "lock queueing plus a fresh wait budget would take at least 1650 ms: {elapsed:?}"
    );
}

#[tokio::test]
async fn test_closed_contended_session_still_guides_output_collection() {
    let dir = tempfile::tempdir().expect("workspace");
    let (exec, stdin) = tools(&dir);
    let id = background(&exec, "read line; printf final_tail").await;
    let session_id = ra_core::event::exec::ExecSessionId::new(id.clone());
    let guard = exec
        .process_manager()
        .lock_interaction(&session_id)
        .await
        .expect("lock");
    exec.process_manager()
        .write_stdin(&session_id, Some("go\n"), false, None)
        .await
        .expect("release process");
    let job = ra_exec::job::BackgroundJob::new(Arc::clone(exec.process_manager()), session_id);
    let done = job
        .wait(ra_exec::job::JobWaitUntil::Timeout(Duration::from_secs(2)))
        .await
        .expect("closeout");
    assert!(done.snapshot().is_closed());
    let output = call(&stdin, wait_args(&id, "output", 50))
        .await
        .expect("contention result");
    drop(guard);
    let collected = call(&stdin, wait_args(&id, "output", 0))
        .await
        .expect("final output");
    assert!(collected.as_text().expect("text").contains("final_tail"));
    let text = output.as_text().expect("text");
    assert!(!text.contains("its output is complete"), "{text}");
    assert!(!text.contains("nothing was lost"), "{text}");
    assert!(text.contains("output capture has closed"), "{text}");
    assert!(!output.metadata().guidance().is_empty(), "{output:?}");
}
