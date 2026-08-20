//! Behaviour of the `exec_command` tool: its schema, what it runs, and what it says when it cannot.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_tools::exec_command::{ExecCommandLimits, ExecCommandTool};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Every argument the strict schema requires, so a test only writes the ones it cares about.
fn arguments(overrides: Value) -> Value {
    let mut base = json!({
        "cmd": "true",
        "workdir": null,
        "shell": null,
        "tty": null,
        "login": null,
        "yield_time_ms": null,
        "timeout_ms": null
    });
    let (Value::Object(base_map), Value::Object(overrides)) = (&mut base, overrides) else {
        panic!("both the base arguments and the overrides must be objects");
    };
    base_map.extend(overrides);
    base
}

fn workspace() -> TempDir {
    tempfile::tempdir().expect("a temporary workspace")
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("runner"))
        .name("Runner")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-exec-command"), agent.as_ref())
}

async fn execute(tool: &ExecCommandTool, arguments: &Value) -> ra_core::error::Result<ToolOutput> {
    let call_id = CallId::new("call-exec-1");
    let run = run();
    tool.call(ToolContext::new(&run, tool, &call_id, arguments))
        .await
}

/// Runs a call and renders whatever comes back, so a failure is read the way a model reads it.
async fn observe(tool: &ExecCommandTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-exec-1");
    let run = run();
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure handling must not fail")
            .expect("exec_command handles every failure it produces"),
    }
}

fn rooted(dir: &TempDir) -> ExecCommandTool {
    ExecCommandTool::rooted(dir.path()).expect("a rooted exec_command")
}

#[tokio::test]
async fn test_exec_command_schema_and_identity() {
    let tool = ExecCommandTool::new().expect("exec_command builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "exec_command");
    assert!(tool.schema().strict_json_schema());

    let schema = tool.schema().input_schema();
    assert_eq!(schema["additionalProperties"], json!(false));
    // Every advertised argument is one this build reads. The approval-shaped arguments Codex
    // carries arrive with the milestone that consumes them rather than as required nulls.
    assert_eq!(
        schema["required"],
        json!([
            "cmd",
            "login",
            "shell",
            "timeout_ms",
            "tty",
            "workdir",
            "yield_time_ms"
        ])
    );
}

#[tokio::test]
async fn test_exec_command_options() {
    let tool = ExecCommandTool::new().expect("exec_command builds");

    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
    assert!(tool.options().is_advertised());
}

#[tokio::test]
async fn test_exec_command_simple_execution() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = execute(&tool, &arguments(json!({ "cmd": "echo 'hello execution'" })))
        .await
        .expect("execution succeeds");

    assert_eq!(
        output.as_text().expect("text output").trim(),
        "hello execution"
    );
    // A clean run costs the model the output and nothing else.
    assert!(output.metadata().guidance().is_empty());
    assert!(output.metadata().truncations().is_empty());
}

#[tokio::test]
async fn test_exec_command_custom_workdir() {
    let dir = workspace();
    let sub = dir.path().join("subdir");
    std::fs::create_dir_all(&sub).expect("create subdir");
    std::fs::write(sub.join("file.txt"), "found in sub").expect("write file");

    let tool = rooted(&dir);
    let output = execute(
        &tool,
        &arguments(json!({ "cmd": "cat file.txt", "workdir": "subdir" })),
    )
    .await
    .expect("execution succeeds");

    assert_eq!(
        output.as_text().expect("text output").trim(),
        "found in sub"
    );
}

#[tokio::test]
async fn test_exec_command_rejects_escaping_workdir() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = observe(&tool, &arguments(json!({ "cmd": "ls", "workdir": "../outside" }))).await;

    let text = output.as_text().expect("text output");
    assert!(text.contains("uses `..`"), "unexpected sentence: {text}");
    // The diagnosis is stated once. A version that rebuilt the failure by searching its own message
    // produced the sentence twice, nested inside itself.
    assert_eq!(text.matches("uses `..`").count(), 1);
    assert!(!output.metadata().guidance().is_empty());
}

#[tokio::test]
async fn test_exec_command_rejects_workdir_outside_the_root() {
    let dir = workspace();
    let outside = workspace();
    let tool = rooted(&dir);

    let output = observe(
        &tool,
        &arguments(json!({ "cmd": "ls", "workdir": outside.path().to_string_lossy() })),
    )
    .await;

    let text = output.as_text().expect("text output");
    assert!(text.contains("outside the workspace"), "unexpected sentence: {text}");
    assert!(!output.metadata().guidance().is_empty());
}

#[tokio::test]
async fn test_exec_command_reports_a_missing_workdir_once() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = observe(&tool, &arguments(json!({ "cmd": "ls", "workdir": "absent" }))).await;

    let text = output.as_text().expect("text output");
    assert!(text.contains("No such directory"), "unexpected sentence: {text}");
    assert_eq!(text.matches("No such directory").count(), 1);
}

/// A terminal that cannot be given is refused, not silently replaced by a pipe.
#[tokio::test]
async fn test_exec_command_refuses_a_terminal_it_cannot_allocate() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = observe(&tool, &arguments(json!({ "cmd": "ls", "tty": true }))).await;

    let text = output.as_text().expect("text output");
    assert!(text.contains("cannot allocate a terminal"), "unexpected sentence: {text}");
    assert!(!output.metadata().guidance().is_empty());
}

#[tokio::test]
async fn test_exec_command_rejects_unknown_arguments() {
    let dir = workspace();
    let tool = rooted(&dir);

    let mut args = arguments(json!({ "cmd": "ls" }));
    args["justification"] = json!("because");
    let output = observe(&tool, &args).await;

    let text = output.as_text().expect("text output");
    assert!(text.contains("Invalid arguments"), "unexpected sentence: {text}");
}

#[tokio::test]
async fn test_exec_command_background_yield() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = execute(
        &tool,
        &arguments(json!({ "cmd": "sleep 30; echo done", "yield_time_ms": 100 })),
    )
    .await
    .expect("execution starts and yields");

    let text = output.as_text().expect("text output");
    assert!(text.contains("still running"), "unexpected sentence: {text}");
    assert!(!output.metadata().guidance().is_empty());

    for session_id in tool.process_manager().active_sessions().await {
        tool.process_manager().cancel(&session_id).await;
    }
}

/// A hard timeout is an argument that does something: the command is taken away when it expires.
#[tokio::test]
async fn test_exec_command_enforces_its_timeout() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = execute(
        &tool,
        &arguments(json!({
            "cmd": "sleep 30",
            "yield_time_ms": 100,
            "timeout_ms": 300
        })),
    )
    .await
    .expect("execution starts and yields");
    assert!(output.as_text().expect("text output").contains("still running"));

    let manager = tool.process_manager();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if manager.active_sessions().await.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the timeout never took the command away"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn test_exec_command_non_zero_exit_code() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = execute(
        &tool,
        &arguments(json!({ "cmd": "echo 'something failed' >&2; exit 7" })),
    )
    .await
    .expect("execution completes");

    let text = output.as_text().expect("text output");
    assert!(text.contains("something failed"));
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|line| line.contains("status 7"))
    );
}

#[tokio::test]
async fn test_exec_command_truncation_limits() {
    let dir = workspace();
    let limits = ExecCommandLimits::new().with_max_output_bytes(50);
    let tool = rooted(&dir).with_limits(limits);

    let output = execute(&tool, &arguments(json!({ "cmd": "printf '%0.s0' $(seq 1 200)" })))
        .await
        .expect("execution completes");

    let text = output.as_text().expect("text output");
    assert!(text.contains("[omitted"), "unexpected body: {text}");

    let truncations = output.metadata().truncations();
    assert_eq!(truncations.len(), 1);
    // Both numbers count source bytes: 200 produced, the 50 byte ceiling retained. The marker
    // standing in for the omitted middle is not output the command produced.
    assert_eq!(truncations[0].original_bytes(), 200);
    assert_eq!(truncations[0].retained_bytes(), 50);
}
