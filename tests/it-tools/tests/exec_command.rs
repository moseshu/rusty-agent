//! Behaviour of the `exec_command` tool: its schema, what it runs, and what it says when it cannot.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_tools::exec_command::{ExecCommandLimits, ExecCommandTool};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The one argument Codex's `exec_command` requires; a test adds only the ones it cares about.
fn arguments(overrides: Value) -> Value {
    let mut base = json!({"cmd": "true"});
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
async fn test_exec_command_schema_decode_failure_is_model_visible() {
    let tool = ExecCommandTool::new().expect("exec_command builds");
    let arguments = json!({"cmd": 3});
    let error = tool
        .decode_input(&arguments)
        .expect_err("invalid arguments must fail before the tool body runs");
    let run = run();
    let output = tool
        .handle_failure(
            &ToolContext::new(&run, &tool, &CallId::new("call-invalid"), &arguments),
            &error,
        )
        .await
        .expect("failure shaping must not fail")
        .expect("schema decode failures keep exec_command's model-visible guidance");

    assert_eq!(
        output.as_text().expect("a single text block"),
        "Invalid arguments: argument `$.cmd` has type integer, expected string."
    );
}

#[tokio::test]
async fn test_exec_command_schema_and_identity() {
    let tool = ExecCommandTool::new().expect("exec_command builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "exec_command");
    // Codex declares `exec_command` with `strict: false`.
    assert!(!tool.schema().strict_json_schema());
    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Runs a command in a shell. Returns its output, or a `session_id` when the command is still\nrunning after the yield timeout."
        )
    );

    let schema = tool.schema().input_schema();
    // Codex declares a closed object that requires only `cmd`; its decoder still ignores
    // arguments it does not read.
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["cmd"]));
}

#[tokio::test]
async fn test_exec_command_options() {
    let tool = ExecCommandTool::new().expect("exec_command builds");

    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Always);
    assert!(tool.options().is_advertised());
}

/// What keeps two commands out of each other's way is the workspace claim, not the schedule.
///
/// Commands are schedulable in parallel so that a batch of them is one batch. The serialization
/// that lost is not gone: the rooted form claims its workspace exclusively, so two commands rooted
/// in the same tree still take turns. An assertion on the concurrency mode alone would keep passing
/// with that claim deleted, and every command in a batch would then run over the same worktree.
#[tokio::test]
async fn test_a_rooted_command_claims_its_workspace_exclusively() {
    let dir = workspace();
    let tool = rooted(&dir);
    let claims = tool.options().resource_claims().to_vec();

    assert_eq!(claims.len(), 1, "{claims:?}");
    assert!(claims[0].is_exclusive(), "{claims:?}");
    assert!(
        claims[0].resource().to_string().contains(
            &dir.path()
                .canonicalize()
                .expect("a real path")
                .display()
                .to_string()
        ),
        "the claim must name this workspace: {claims:?}"
    );
}

#[tokio::test]
async fn test_exec_command_simple_execution() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = execute(
        &tool,
        &arguments(json!({ "cmd": "echo 'hello execution'" })),
    )
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

    let output = observe(
        &tool,
        &arguments(json!({ "cmd": "ls", "workdir": "../outside" })),
    )
    .await;

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
    assert!(
        text.contains("outside the workspace"),
        "unexpected sentence: {text}"
    );
    assert!(!output.metadata().guidance().is_empty());
}

#[tokio::test]
async fn test_exec_command_reports_a_missing_workdir_once() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = observe(
        &tool,
        &arguments(json!({ "cmd": "ls", "workdir": "absent" })),
    )
    .await;

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("No such directory"),
        "unexpected sentence: {text}"
    );
    assert_eq!(text.matches("No such directory").count(), 1);
}

/// A terminal that cannot be given is refused, not silently replaced by a pipe.
#[tokio::test]
async fn test_exec_command_refuses_a_terminal_it_cannot_allocate() {
    let dir = workspace();
    let tool = rooted(&dir);

    let output = observe(&tool, &arguments(json!({ "cmd": "ls", "tty": true }))).await;

    let text = output.as_text().expect("text output");
    assert!(
        text.contains("cannot allocate a terminal"),
        "unexpected sentence: {text}"
    );
    assert!(!output.metadata().guidance().is_empty());
}

/// Codex's `ExecCommandArgs` ignores arguments it does not read rather than refusing the call.
/// Without a host policy the approval arguments are among them: nothing here can honor them.
#[tokio::test]
async fn test_exec_command_ignores_unknown_arguments() {
    let dir = workspace();
    let tool = rooted(&dir);

    let mut args = arguments(json!({ "cmd": "printf ran" }));
    args["justification"] = json!("because");
    args["max_output_tokens"] = json!(5);
    args["unrelated"] = json!({"nested": true});
    let output = execute(&tool, &args).await.expect("the call runs");

    let text = output.as_text().expect("text output");
    assert!(text.contains("ran"), "unexpected sentence: {text}");
}

/// Codex keeps its declaration and its decoder apart: the model is shown a closed object
/// (`additionalProperties: false`), and a call that carries a field anyway still runs. Every
/// form keeps both, so closing the advertised object does not close the decoder.
#[tokio::test]
async fn test_exec_command_advertises_a_closed_object_yet_ignores_unknown_arguments_in_every_form()
{
    let dir = workspace();
    for tool in every_form(&dir) {
        tool.validate().expect("identity and schema must agree");
        let schema = tool.schema().input_schema();
        assert!(!tool.schema().strict_json_schema());
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["cmd"]));
        assert_eq!(
            tool.model_definition().input_schema()["additionalProperties"],
            json!(false)
        );

        let args =
            json!({"cmd": "printf ran", "max_output_tokens": 5, "unrelated": {"nested": true}});
        tool.decode_input(&args).expect("unknown arguments decode");
        assert_eq!(observe(&tool, &args).await.as_text(), Some("ran"));
    }
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
    assert!(
        text.contains("still running"),
        "unexpected sentence: {text}"
    );
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
    assert!(
        output
            .as_text()
            .expect("text output")
            .contains("still running")
    );

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

    let output = execute(
        &tool,
        &arguments(json!({ "cmd": "printf '%0.s0' $(seq 1 200)" })),
    )
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

fn permissive_policy(tool: ExecCommandTool) -> ExecCommandTool {
    tool.with_policy_resolvers(
        std::sync::Arc::new(|_context: &ToolContext<'_>| {
            Ok(ra_exec::sandbox::ExecEnvironment::default())
        }),
        std::sync::Arc::new(|_context: &ToolContext<'_>| Ok(false)),
    )
}

/// Codex's `exec_permission_approvals` schema: once turned on, a tool with a policy offers
/// `with_additional_permissions` and the permissions it asks for, with Codex's descriptions, and
/// accepts them, whichever is installed first. Without a policy nothing is offered; what is not
/// offered still decodes, as Codex's arguments do, and is the policy's to refuse.
#[tokio::test]
async fn test_exec_command_offers_additional_permissions_when_turned_on() {
    let dir = workspace();
    let tools = [
        permissive_policy(rooted(&dir)).with_additional_permissions(),
        permissive_policy(rooted(&dir).with_additional_permissions()),
    ];
    for tool in &tools {
        tool.validate().expect("identity and schema must agree");
        let schema = tool.schema().input_schema();
        assert_eq!(schema["required"], json!(["cmd"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let properties = &schema["properties"];
        assert_eq!(
            properties["sandbox_permissions"]["description"],
            "Per-command sandbox override. Defaults to `use_default`; use `with_additional_permissions` with `additional_permissions`, or `require_escalated` for unsandboxed execution."
        );
        assert_eq!(
            schema["definitions"]["SandboxPermissionsArgument"]["enum"],
            json!([
                "use_default",
                "with_additional_permissions",
                "require_escalated"
            ])
        );
        assert_eq!(
            properties["additional_permissions"]["description"],
            "Sandboxed filesystem or network access for this command; only with `sandbox_permissions: \"with_additional_permissions\"`."
        );
        let definitions = &schema["definitions"];
        assert_eq!(
            definitions["PermissionProfileArgument"]["description"],
            "Filesystem or network access request."
        );
        assert_eq!(
            definitions["NetworkPermissionsArgument"]["properties"]["enabled"]["description"],
            "True requests network access; false or omitted requests none."
        );
        let file_system = &definitions["FileSystemPermissionsArgument"]["properties"];
        assert_eq!(
            file_system["read"]["description"],
            "Absolute paths to grant read access; omit when none are needed."
        );
        assert_eq!(
            file_system["write"]["description"],
            "Absolute paths to grant write access; omit when none are needed."
        );

        let args = arguments(json!({
            "cmd": "printf ok",
            "sandbox_permissions": "with_additional_permissions",
            "justification": null,
            "prefix_rule": null,
            "additional_permissions": {
                "network": null,
                "file_system": {"read": null, "write": ["out"]}
            }
        }));
        tool.decode_input(&args)
            .expect("the additional permissions decode");
        assert_eq!(observe(tool, &args).await.as_text(), Some("ok"));
    }

    let without_policy = rooted(&dir).with_additional_permissions();
    assert!(
        without_policy.schema().input_schema()["properties"]
            .get("additional_permissions")
            .is_none()
    );
    // Not offered is not refused at decoding: Codex's `ExecCommandArgs` reads
    // `additional_permissions` whatever is advertised, and its handler refuses them while the
    // feature is off. Here that refusal is the host policy's, as the coding product's does.
    let off = permissive_policy(rooted(&dir));
    assert!(
        off.decode_input(&arguments(json!({
            "sandbox_permissions": "use_default",
            "justification": null,
            "prefix_rule": null,
            "additional_permissions": {"network": {"enabled": true}, "file_system": null}
        })))
        .is_ok(),
        "arguments the tool does not offer are left to the policy"
    );
}

/// With policy resolvers installed the tool advertises Codex's approval arguments, with Codex's
/// descriptions, and accepts them; the resolvers read them from the call's arguments.
#[tokio::test]
async fn test_exec_command_policy_resolvers_add_the_approval_arguments() {
    let dir = workspace();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let recorded = std::sync::Arc::clone(&seen);
    let tool = rooted(&dir).with_policy_resolvers(
        std::sync::Arc::new(|_context: &ToolContext<'_>| {
            Ok(ra_exec::sandbox::ExecEnvironment::default())
        }),
        std::sync::Arc::new(move |context: &ToolContext<'_>| {
            *recorded.lock().unwrap() = Some(context.arguments()["prefix_rule"].clone());
            Ok(false)
        }),
    );
    tool.validate().expect("identity and schema must agree");

    let schema = tool.schema().input_schema();
    assert_eq!(schema["required"], json!(["cmd"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    let properties = &schema["properties"];
    assert_eq!(
        properties["justification"]["description"],
        "User-facing approval question for `require_escalated`; omit otherwise."
    );
    assert_eq!(
        properties["prefix_rule"]["description"],
        r#"Reusable approval prefix for `cmd`, only with `sandbox_permissions: "require_escalated"`; for example ["git", "pull"]."#
    );
    assert_eq!(
        properties["sandbox_permissions"]["description"],
        "Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for unsandboxed execution."
    );
    // Codex's `with_additional_permissions` is offered only once turned on.
    assert_eq!(
        schema["definitions"]["SandboxPermissionsArgument"]["enum"],
        json!(["use_default", "require_escalated"])
    );

    let args = arguments(json!({
        "cmd": "printf ok",
        "sandbox_permissions": "require_escalated",
        "justification": "Do you want to run it?",
        "prefix_rule": ["printf"],
        "yield_time_ms": 2000
    }));
    tool.decode_input(&args)
        .expect("the approval arguments decode");
    let run = run();
    let call_id = CallId::new("call-exec-1");
    assert!(
        !tool
            .needs_approval(&ToolContext::new(&run, &tool, &call_id, &args))
            .await
            .unwrap()
    );
    assert_eq!(*seen.lock().unwrap(), Some(json!(["printf"])));
    let output = observe(&tool, &args).await;
    assert_eq!(output.as_text(), Some("ok"));
}

/// A sandbox that cannot be prepared is a process-creation failure, as Codex's unified exec
/// reports it: not a policy rejection, and with no instruction to start the command again.
#[tokio::test]
async fn test_exec_command_reports_sandbox_preparation_as_process_creation() {
    use ra_exec::sandbox::{ExecEnvironment, NetworkAccess, SandboxLevel, SandboxPolicy};

    let dir = workspace();
    let marker = dir.path().join("must-not-exist");
    let tool = rooted(&dir).with_policy_resolvers(
        std::sync::Arc::new(|_context: &ToolContext<'_>| {
            // Every platform refuses this contradiction before choosing a backend.
            Ok(ExecEnvironment::default().with_sandbox(
                SandboxPolicy::new(SandboxLevel::Unconfined).with_network(NetworkAccess::Denied),
            ))
        }),
        std::sync::Arc::new(|_context: &ToolContext<'_>| Ok(false)),
    );
    let arguments = arguments(json!({ "cmd": format!("touch '{}'", marker.display()) }));

    let error = execute(&tool, &arguments)
        .await
        .expect_err("an unpreparable sandbox must not run the command");
    assert!(matches!(
        error,
        ra_core::error::Error::Tool {
            kind: ra_core::error::ToolErrorKind::ExecutionFailed,
            ..
        }
    ));
    let output = observe(&tool, &arguments).await;
    let text = output.as_text().expect("text output");
    assert!(
        text.starts_with("Failed to create unified exec process: "),
        "{text}"
    );
    assert!(text.contains("cannot deny the network"), "{text}");
    assert!(output.metadata().guidance().is_empty(), "{text}");
    assert!(!marker.exists());
}

/// The three argument forms this tool can advertise: without a policy, with one, and with Codex's
/// additional permissions on top. Codex decodes all of them from one `ExecCommandArgs`.
fn every_form(dir: &TempDir) -> [ExecCommandTool; 3] {
    [
        rooted(dir),
        permissive_policy(rooted(dir)),
        permissive_policy(rooted(dir)).with_additional_permissions(),
    ]
}

/// Codex requires only `cmd`: every other argument may be left out, and each form runs the call.
#[tokio::test]
async fn test_exec_command_runs_with_only_cmd_in_every_form() {
    let dir = workspace();
    for tool in every_form(&dir) {
        let args = json!({"cmd": "printf ran"});
        tool.decode_input(&args).expect("only `cmd` decodes");
        assert_eq!(observe(&tool, &args).await.as_text(), Some("ran"));
    }
}

/// An omitted `yield_time_ms` is the host's default, not a fixed value: the host's configured
/// default still applies, as the omitted argument does in Codex.
#[tokio::test]
async fn test_exec_command_omitted_yield_uses_the_host_default() {
    let dir = workspace();
    let tool = rooted(&dir).with_limits(ExecCommandLimits::new().with_default_yield_time_ms(50));
    let output = execute(&tool, &json!({"cmd": "sleep 5; echo late"}))
        .await
        .expect("the command starts and yields");
    let text = output.as_text().expect("text output");
    assert!(
        text.contains("still running"),
        "unexpected sentence: {text}"
    );
}

/// What Codex refuses at decoding is still refused in every form: a missing `cmd`, a value of the
/// wrong type, and an explicit `null` for `tty` or `yield_time_ms`, which have defaults rather
/// than a null. Other optional arguments accept `null` as their absent value.
#[tokio::test]
async fn test_exec_command_refuses_what_codex_cannot_decode_in_every_form() {
    let dir = workspace();
    for tool in every_form(&dir) {
        for refused in [
            json!({}),
            json!({"cmd": 3}),
            json!({"cmd": "true", "timeout_ms": "soon"}),
            json!({"cmd": "true", "tty": null}),
            json!({"cmd": "true", "yield_time_ms": null}),
        ] {
            assert!(
                tool.decode_input(&refused).is_err(),
                "{refused} must not decode"
            );
        }
        for accepted in [
            json!({"cmd": "true", "workdir": null, "shell": null, "login": null, "timeout_ms": null}),
            json!({"cmd": "true", "tty": false, "yield_time_ms": 10}),
        ] {
            tool.decode_input(&accepted)
                .unwrap_or_else(|error| panic!("{accepted} must decode: {error}"));
        }
    }
}

/// Approval preparation renders a policy refusal but preserves host failures as run errors.
#[tokio::test]
async fn approval_preparation_distinguishes_refusals_from_host_failures() {
    use std::sync::Arc;

    use ra_core::error::{Error, Result, ToolErrorKind};
    use ra_tools::exec_command::{ExecCommandAttempt, ExecCommandPolicy, rejection};

    struct Policy {
        refuses: bool,
    }

    impl ExecCommandPolicy for Policy {
        fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
            Ok(true)
        }

        fn approval_state(&self, _context: &ToolContext<'_>) -> Result<Option<Value>> {
            Err(if self.refuses {
                rejection("policy refused")
            } else {
                Error::caller("host failed")
            })
        }

        fn attempt(&self, _context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
            panic!("approval preparation must not execute a command")
        }
    }

    for refuses in [false, true] {
        let tool = ExecCommandTool::new()
            .unwrap()
            .with_command_policy(Arc::new(Policy { refuses }));
        let run = run();
        let call_id = CallId::new("prepare");
        let arguments = json!({"cmd": "true"});
        let context = ToolContext::new(&run, &tool, &call_id, &arguments);
        let error = tool.approval_state(&context).await.unwrap_err();
        if refuses {
            assert!(matches!(
                error,
                Error::Tool {
                    kind: ToolErrorKind::InvalidInput,
                    ..
                }
            ));
            let output = tool
                .handle_failure(&context, &error)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(output.as_text(), Some("policy refused"));
        } else {
            assert_eq!(error.to_string(), Error::caller("host failed").to_string());
            assert!(
                tool.handle_failure(&context, &error)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
}
