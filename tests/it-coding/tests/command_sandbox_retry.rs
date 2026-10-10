//! Codex's orchestrator after its sandbox refuses a command's first attempt, as the coding host's
//! `exec_command` applies it: under `Never` and `OnRequest` the refused attempt is the result;
//! otherwise the command runs again outside the sandbox, without asking when the call was already
//! approved, and after the host approves it when it was not. The approval is asked from inside the
//! invocation and keeps what the first attempt was decided with, so the approved call runs only
//! the retry, however the policy changed meanwhile and in whichever process it resumes.
//!
//! Every command appends a line to `attempts.log` in the workspace, which the sandbox allows, and
//! then writes to a directory outside every writable root, which it refuses. The log counts the
//! attempts that ran.

use std::{path::Path, sync::Arc};

use futures::{StreamExt, stream};
use ra_coding::{
    exec_policy::{ExecPolicyManager, ExecPolicySources},
    host::CodingHost,
    thread_settings::{CodingThreadSettings, CodingThreadSettingsAdapter},
};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall,
        ToolResumePoint,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{Tool, ToolApprovalRequest, ToolContext, ToolOutput},
};
use ra_protocol::server::{
    ReviewDecision, ThreadSettingsAdapter,
    protocol::{AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::{Value, json};

/// Writes only the workspace: the temporary directories are excluded, so another temporary
/// directory is outside every writable root.
fn workspace_only() -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    }
}

fn granular(sandbox_approval: bool) -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    outside: tempfile::TempDir,
    rules_home: tempfile::TempDir,
    host: CodingHost,
    tool: Arc<dyn Tool>,
    agent: Arc<AgentSpec>,
    approval: AskForApproval,
    config: RunConfig,
}

async fn thread_config(
    host: &CodingHost,
    rules_home: &Path,
    approval: AskForApproval,
) -> RunConfig {
    let manager = ExecPolicyManager::load(&ExecPolicySources::new().with_config_folder(rules_home))
        .await
        .unwrap();
    let adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), workspace_only())
        .unwrap()
        .with_exec_policy(Arc::new(manager))
        .with_rules_home(rules_home);
    let config = adapter.initialize(host.build_run_config());
    adapter
        .prepare(
            &config,
            &ThreadPolicyOverrides::new().with_approval_policy(approval),
        )
        .unwrap()
}

async fn fixture(approval: AskForApproval) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let rules_home = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let config = thread_config(&host, rules_home.path(), approval).await;
    let tool = host.exec_command_tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("coder")
        .instructions("code")
        .tools(vec![Arc::clone(&tool)])
        .build()
        .unwrap();
    Fixture {
        dir,
        outside,
        rules_home,
        host,
        tool,
        agent,
        approval,
        config,
    }
}

impl Fixture {
    /// The same thread loaded again, as another process would load it: the same workspace, rules
    /// and policy, and nothing held in memory.
    async fn restart(&mut self) {
        self.config = thread_config(&self.host, self.rules_home.path(), self.approval).await;
    }

    fn settings(&self) -> &CodingThreadSettings {
        self.config
            .app_context()
            .and_then(|context| context.downcast_ref::<CodingThreadSettings>())
            .unwrap()
    }

    fn run(&self) -> RunContext {
        RunContext::new(RunId::new("run"), &self.agent)
            .with_app_context(self.config.app_context().unwrap().clone())
    }

    /// The command: one logged attempt, then a write the sandbox refuses.
    fn arguments(&self, cmd_suffix: &str) -> Value {
        let target = self.outside.path().join("written.txt");
        json!({
            "cmd": format!("echo attempt >> attempts.log; {cmd_suffix}touch '{}'", target.display()),
            "yield_time_ms": 5000
        })
    }

    fn attempts(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("attempts.log"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    fn written(&self) -> bool {
        self.outside.path().join("written.txt").exists()
    }

    async fn needs_approval(&self, arguments: &Value) -> bool {
        self.needs_approval_as("call", arguments).await
    }

    async fn needs_approval_as(&self, call_id: &str, arguments: &Value) -> bool {
        let run = self.run();
        let call_id = CallId::new(call_id);
        self.tool
            .needs_approval(&ToolContext::new(
                &run,
                self.tool.as_ref(),
                &call_id,
                arguments,
            ))
            .await
            .unwrap()
    }

    async fn approval_state(&self, arguments: &Value) -> Value {
        let run = self.run();
        let call_id = CallId::new("call");
        self.tool
            .approval_state(&ToolContext::new(
                &run,
                self.tool.as_ref(),
                &call_id,
                arguments,
            ))
            .await
            .unwrap()
            .expect("the command keeps its decision with the approval")
    }

    async fn call(&self, arguments: &Value, resume: Option<&Value>) -> Result<ToolOutput> {
        self.call_as("call", arguments, resume).await
    }

    async fn call_as(
        &self,
        call_id: &str,
        arguments: &Value,
        resume: Option<&Value>,
    ) -> Result<ToolOutput> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, self.tool.as_ref(), &call_id, arguments);
        let context = match resume {
            Some(state) => context.with_resume_state(state),
            None => context,
        };
        self.tool.call(context).await
    }
}

/// The state of the approval a call asked for from inside its invocation.
fn retry_state(outcome: Result<ToolOutput>) -> Value {
    let error = outcome.expect_err("the refused attempt asks to run outside the sandbox");
    ToolApprovalRequest::of(&error)
        .expect("an approval request, not a failure")
        .state()
        .clone()
}

/// A call no one approved, whose first attempt the sandbox refused, asks to run again outside the
/// sandbox with Codex's retry reason; approved, it runs only the retry.
#[tokio::test]
async fn a_refused_attempt_asks_before_running_outside_the_sandbox() {
    let fixture = fixture(granular(true)).await;
    let arguments = fixture.arguments("");
    assert!(!fixture.needs_approval(&arguments).await);

    let state = retry_state(fixture.call(&arguments, None).await);
    assert_eq!(state["policy_state"]["stage"], "sandbox_retry");
    assert_eq!(
        state["policy_state"]["retry_reason"],
        "command failed; retry without sandbox?"
    );
    assert_eq!(fixture.attempts(), 1);
    assert!(!fixture.written());

    fixture.call(&arguments, Some(&state)).await.unwrap();
    assert_eq!(fixture.attempts(), 2, "the first attempt is not run again");
    assert!(fixture.written());
}

/// A call the host approved before it ran is not asked about again: its refused first attempt is
/// retried outside the sandbox at once, as Codex's `should_bypass_approval` decides.
#[tokio::test]
async fn an_approved_call_retries_without_asking_again() {
    let fixture = fixture(AskForApproval::UnlessTrusted).await;
    let arguments = fixture.arguments("");
    assert!(fixture.needs_approval(&arguments).await);
    let approved = fixture.approval_state(&arguments).await;
    assert_eq!(approved["policy_state"]["stage"], "approval");

    fixture.call(&arguments, Some(&approved)).await.unwrap();
    assert_eq!(fixture.attempts(), 2);
    assert!(fixture.written());
}

/// Under `OnRequest` and `Never`, and under `Granular` without sandbox approval, the refused
/// attempt's output is the result: nothing runs outside the sandbox and nothing is asked.
#[tokio::test]
async fn policies_that_do_not_ask_return_the_refused_attempt() {
    for approval in [
        AskForApproval::OnRequest,
        AskForApproval::Never,
        granular(false),
    ] {
        let fixture = fixture(approval).await;
        let arguments = fixture.arguments("");
        assert!(!fixture.needs_approval(&arguments).await);
        let output = fixture.call(&arguments, None).await.unwrap();
        let text = format!("{output:?}");
        assert!(
            text.to_lowercase().contains("operation not permitted")
                || text.to_lowercase().contains("read-only file system"),
            "{approval:?}: {text}"
        );
        assert_eq!(fixture.attempts(), 1, "{approval:?}");
        assert!(!fixture.written(), "{approval:?}");
    }
}

/// Ordinary failures and commands still running when they yield do not request a retry.
#[tokio::test]
async fn ordinary_failure_and_yield_are_not_retried() {
    let fixture = fixture(granular(true)).await;

    let failing = json!({"cmd": "echo failing; exit 3", "yield_time_ms": 5000});
    let output = fixture.call_as("failing", &failing, None).await.unwrap();
    assert!(format!("{output:?}").contains("failing"));

    let running = json!({"cmd": "sleep 2; touch /nonexistent/x", "yield_time_ms": 200});
    let output = fixture.call_as("running", &running, None).await.unwrap();
    assert!(
        format!("{output:?}").contains("still running"),
        "{output:?}"
    );
}

/// The retry resumes in another process from the approval's state alone: the first attempt is
/// not run again.
#[tokio::test]
async fn a_retry_resumes_in_another_process() {
    let mut fixture = fixture(granular(true)).await;
    let arguments = fixture.arguments("");
    let state = retry_state(fixture.call(&arguments, None).await);
    let state: Value = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();

    fixture.restart().await;
    fixture.call(&arguments, Some(&state)).await.unwrap();
    assert_eq!(fixture.attempts(), 2);
    assert!(fixture.written());
}

/// The amendment approved with the call allows its command, which would send a fresh decision
/// straight outside the sandbox; the approved call keeps the path it was decided
/// on, in the sandbox first, even resumed in another process.
#[tokio::test]
async fn an_amendment_does_not_change_the_approved_calls_path() {
    let mut fixture = fixture(AskForApproval::UnlessTrusted).await;
    let arguments = fixture.arguments("");
    assert!(fixture.needs_approval(&arguments).await);
    let approved = fixture.approval_state(&arguments).await;

    // The amendment the approval request proposes: the redirect keeps the script whole, so it is
    // the whole command.
    let amendment = fixture
        .settings()
        .pending_command_requirement(&RunId::new("run"), &CallId::new("call"))
        .and_then(|requirement| requirement.proposed_execpolicy_amendment().cloned())
        .expect("the approval proposes an amendment");
    let decision = ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment: amendment,
    };
    assert_eq!(
        fixture
            .settings()
            .persist_approved_execpolicy_amendment(&decision)
            .await,
        None
    );
    fixture.restart().await;
    assert!(
        !fixture.needs_approval_as("fresh", &arguments).await,
        "the amendments allow the command now"
    );

    fixture.call(&arguments, Some(&approved)).await.unwrap();
    assert_eq!(
        fixture.attempts(),
        2,
        "a sandboxed first attempt, then the retry"
    );
    assert!(fixture.written());
}

/// The retry an approval admits belongs to the call that asked: the same call ID with other
/// arguments cannot use it.
#[tokio::test]
async fn a_retry_approval_cannot_run_other_arguments() {
    let fixture = fixture(granular(true)).await;
    let arguments = fixture.arguments("");
    let state = retry_state(fixture.call(&arguments, None).await);

    let other = json!({"cmd": "touch other.txt", "yield_time_ms": 5000});
    let error = fixture.call(&other, Some(&state)).await.unwrap_err();
    assert!(error.to_string().contains("reused tool call ID"), "{error}");
    assert!(!fixture.dir.path().join("other.txt").exists());
}

struct ScriptedModel(std::sync::Mutex<Vec<ModelResponse>>);

impl ScriptedModel {
    fn next(&self) -> Result<ModelResponse> {
        let mut script = self.0.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next()
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next()
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn run_request(fixture: &Fixture, responses: Vec<ModelResponse>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(Arc::clone(&fixture.agent)),
        Arc::new(Resolver(Arc::new(ScriptedModel(std::sync::Mutex::new(
            responses,
        ))))),
        RunId::new("run"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_config(fixture.config.clone())
    .with_services(fixture.host.build_tool_services())
}

/// Through the runner: the refused call pauses the run with an approval the host answers from a
/// serialized checkpoint in another process, and the resumed run runs only the retry.
#[tokio::test]
async fn a_checkpointed_retry_approval_runs_only_the_retry() {
    let mut fixture = fixture(granular(true)).await;
    let mut arguments = fixture.arguments("");
    for field in [
        "workdir",
        "shell",
        "tty",
        "login",
        "timeout_ms",
        "sandbox_permissions",
        "justification",
        "prefix_rule",
    ] {
        arguments[field] = Value::Null;
    }
    let call = ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call"),
            "exec_command",
            arguments,
        )),
    )]);
    let interrupted = Runner::run(run_request(&fixture, vec![call]))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval, got {:?}", interrupted.outcome());
    };
    let [item] = items.as_slice() else {
        panic!("expected one approval");
    };
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected a tool approval");
    };
    assert_eq!(
        approval.resume().map(|resume| resume.point()),
        Some(ToolResumePoint::DuringCall)
    );
    assert_eq!(fixture.attempts(), 1);

    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(interrupted.state()).unwrap()).unwrap();
    state.approve(item, false).unwrap();
    fixture.restart().await;
    let done = ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )]);
    let resumed = Runner::run(run_request(&fixture, vec![done]).with_state(state))
        .await
        .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(fixture.attempts(), 2);
    assert!(fixture.written());
}

#[tokio::test]
async fn retry_approval_is_consumed_once() {
    let fixture = fixture(granular(true)).await;
    let arguments = fixture.arguments("");
    let state = retry_state(fixture.call(&arguments, None).await);
    fixture.call(&arguments, Some(&state)).await.unwrap();
    assert_eq!(fixture.attempts(), 2);
    assert!(
        fixture.call(&arguments, Some(&state)).await.is_err(),
        "a spent retry approval ran the command again"
    );
    assert_eq!(fixture.attempts(), 2);
}

#[tokio::test]
async fn a_late_sandbox_refusal_is_output_even_when_retry_approval_is_allowed() {
    for approval in [granular(true), AskForApproval::UnlessTrusted] {
        let fixture = fixture(approval).await;
        let arguments = fixture.arguments("sleep 0.4; ");
        let state = if fixture.needs_approval(&arguments).await {
            Some(fixture.approval_state(&arguments).await)
        } else {
            None
        };
        let output = fixture.call(&arguments, state.as_ref()).await.unwrap();
        let text = format!("{output:?}").to_lowercase();
        assert!(
            text.contains("operation not permitted") || text.contains("read-only file system"),
            "{text}"
        );
        assert_eq!(
            fixture.attempts(),
            1,
            "a later refusal must not replay earlier side effects"
        );
        assert!(!fixture.written());
    }
}

#[tokio::test]
async fn approval_retains_the_selected_sandbox() {
    let mut fixture = fixture(AskForApproval::UnlessTrusted).await;
    let adapter = CodingThreadSettingsAdapter::new(
        fixture.host.workspace_root(),
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap();
    fixture.config = adapter
        .prepare(
            &adapter.initialize(fixture.host.build_run_config()),
            &ThreadPolicyOverrides::new().with_approval_policy(AskForApproval::UnlessTrusted),
        )
        .unwrap();
    let arguments = json!({"cmd": "if touch probe.txt 2>/dev/null; then printf writable; else printf readonly; fi", "yield_time_ms": 5000});
    assert!(fixture.needs_approval(&arguments).await);
    let state = fixture.approval_state(&arguments).await;
    fixture.config = adapter
        .prepare(
            &fixture.config,
            &ThreadPolicyOverrides::new().with_sandbox_policy(workspace_only()),
        )
        .unwrap();
    let output = fixture.call(&arguments, Some(&state)).await.unwrap();
    assert!(
        format!("{output:?}").contains("readonly"),
        "the approved call adopted a new sandbox: {output:?}"
    );
    assert!(!fixture.dir.path().join("probe.txt").exists());
}

#[tokio::test]
async fn resumed_checkpoint_cannot_execute_again() {
    let mut fixture = fixture(granular(true)).await;
    let mut arguments = fixture.arguments("");
    for field in [
        "workdir",
        "shell",
        "tty",
        "login",
        "timeout_ms",
        "sandbox_permissions",
        "justification",
        "prefix_rule",
    ] {
        arguments[field] = Value::Null;
    }
    let call = ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call"),
            "exec_command",
            arguments,
        )),
    )]);
    let interrupted = Runner::run(run_request(&fixture, vec![call]))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval, got {:?}", interrupted.outcome());
    };
    let [item] = items.as_slice() else {
        panic!("expected one approval");
    };
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected a tool approval");
    };
    assert_eq!(
        approval.resume().map(|resume| resume.point()),
        Some(ToolResumePoint::DuringCall)
    );
    assert_eq!(fixture.attempts(), 1);

    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(interrupted.state()).unwrap()).unwrap();
    state.approve(item, false).unwrap();
    fixture.restart().await;
    let done = ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )]);
    let resumed = Runner::run(run_request(&fixture, vec![done]).with_state(state.clone()))
        .await
        .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(fixture.attempts(), 2);
    assert!(fixture.written());
    let done = ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-2"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )]);
    let replay = Runner::run(run_request(&fixture, vec![done]).with_state(state)).await;
    assert!(
        replay.is_err(),
        "the same approved checkpoint executed its retry a second time"
    );
    assert_eq!(fixture.attempts(), 2);
}

#[tokio::test]
async fn retry_does_not_bypass_configured_deny() {
    let mut fixture = fixture(granular(true)).await;
    let mut arguments = fixture.arguments("");
    for field in [
        "workdir",
        "shell",
        "tty",
        "login",
        "timeout_ms",
        "sandbox_permissions",
        "justification",
        "prefix_rule",
    ] {
        arguments[field] = Value::Null;
    }
    let call = ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call"),
            "exec_command",
            arguments,
        )),
    )]);
    let interrupted = Runner::run(run_request(&fixture, vec![call]))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval, got {:?}", interrupted.outcome());
    };
    let [item] = items.as_slice() else {
        panic!("expected one approval");
    };
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected a tool approval");
    };
    assert_eq!(
        approval.resume().map(|resume| resume.point()),
        Some(ToolResumePoint::DuringCall)
    );
    assert_eq!(fixture.attempts(), 1);

    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(interrupted.state()).unwrap()).unwrap();
    state.approve(item, false).unwrap();
    fixture.restart().await;
    fixture.config =
        fixture
            .config
            .clone()
            .with_permission_rules([ra_core::permission::PermissionRule::new(
                ra_core::permission::PermissionDecision::Deny,
            )
            .with_tool_name("exec_command")]);
    let done = ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )]);
    let resumed = Runner::run(run_request(&fixture, vec![done]).with_state(state))
        .await
        .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        fixture.attempts(),
        1,
        "an explicit deny must stop the retry"
    );
    assert!(!fixture.written());
}

#[tokio::test]
async fn a_retry_approval_is_consumed_atomically() {
    let fixture = fixture(granular(true)).await;
    let arguments = fixture.arguments("");
    let state = retry_state(fixture.call(&arguments, None).await);
    let (left, right) = tokio::join!(
        fixture.call(&arguments, Some(&state)),
        fixture.call(&arguments, Some(&state))
    );
    assert_ne!(left.is_ok(), right.is_ok());
    assert_eq!(fixture.attempts(), 2);
}

#[tokio::test]
async fn a_cancelled_retry_cannot_execute_again() {
    let fixture = fixture(granular(true)).await;
    let target = fixture.outside.path().join("written.txt");
    let arguments = json!({"cmd": format!("echo attempt >> attempts.log; touch '{}' || exit 1; sleep 2", target.display()), "yield_time_ms": 5000});
    let state = retry_state(fixture.call(&arguments, None).await);
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        fixture.call(&arguments, Some(&state)),
    )
    .await;
    fixture
        .host
        .process_manager()
        .terminate_background_terminals()
        .await;
    assert!(cancelled.is_err());
    assert_eq!(fixture.attempts(), 2);
    assert!(fixture.call(&arguments, Some(&state)).await.is_err());
    assert_eq!(fixture.attempts(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn a_retry_that_fails_to_spawn_cannot_execute_again() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture(granular(true)).await;
    let shell = fixture.dir.path().join("shell");
    let install_shell = || {
        std::fs::write(&shell, "#!/bin/sh\nexec /bin/sh \"$@\"\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The first run of a newly written executable can be held for a system scan longer than
        // the launch grace period; run it once so the sandboxed attempt measures the command.
        assert!(
            std::process::Command::new(&shell)
                .args(["-c", "true"])
                .status()
                .unwrap()
                .success()
        );
    };
    install_shell();
    let mut arguments = fixture.arguments("");
    arguments["shell"] = json!(shell);
    let state = retry_state(fixture.call(&arguments, None).await);
    std::fs::remove_file(&shell).unwrap();
    assert!(fixture.call(&arguments, Some(&state)).await.is_err());
    install_shell();
    assert!(fixture.call(&arguments, Some(&state)).await.is_err());
    assert_eq!(fixture.attempts(), 1);
    assert!(!fixture.written());
}

#[tokio::test]
async fn an_approved_request_keeps_its_prepared_working_directory() {
    let mut fixture = fixture(AskForApproval::UnlessTrusted).await;
    let arguments = json!({"cmd": "printf retained > prepared.txt", "yield_time_ms": 5000});
    assert!(fixture.needs_approval(&arguments).await);
    let state = fixture.approval_state(&arguments).await;
    let restored: Value = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    let other = tempfile::tempdir().unwrap();
    let replacement = CodingHost::open(other.path()).unwrap();
    fixture.tool = replacement.exec_command_tool().unwrap();
    fixture.call(&arguments, Some(&restored)).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.path().join("prepared.txt")).unwrap(),
        "retained"
    );
    assert!(!other.path().join("prepared.txt").exists());
}

#[tokio::test]
async fn a_timed_out_command_that_printed_a_refusal_is_not_retried() {
    let fixture = fixture(granular(true)).await;
    let arguments = json!({"cmd": "echo attempt >> attempts.log; printf 'permission denied' >&2; sleep 2", "yield_time_ms": 5000, "timeout_ms": 100});
    let output = fixture.call(&arguments, None).await.unwrap();
    assert!(format!("{output:?}").contains("permission denied"));
    assert_eq!(fixture.attempts(), 1);
}

/// Invalid prepared requests remain observations, including when a fixed rule asks for approval.
#[tokio::test]
async fn invalid_requests_return_to_the_model_without_asking_for_approval() {
    use ra_core::permission::{PermissionDecision, PermissionRule};
    for fixed_ask in [false, true] {
        for (field, value, message) in [
            ("workdir", json!("../escape"), "uses `..`"),
            ("workdir", json!("missing-directory"), "No such directory"),
            ("tty", json!(true), "terminal"),
        ] {
            let mut fixture = fixture(AskForApproval::UnlessTrusted).await;
            let mut arguments = json!({"cmd": "printf should-not-run > invalid-request.txt", "yield_time_ms": 5000});
            for name in [
                "workdir",
                "shell",
                "tty",
                "login",
                "timeout_ms",
                "sandbox_permissions",
                "justification",
                "prefix_rule",
            ] {
                arguments[name] = Value::Null;
            }
            arguments[field] = value;
            assert!(!fixture.needs_approval(&arguments).await);
            if fixed_ask {
                fixture.config =
                    fixture
                        .config
                        .clone()
                        .with_permission_rules([PermissionRule::new(PermissionDecision::Ask)
                            .with_tool_name("exec_command")]);
            }
            let call = ModelResponse::new(vec![RunItem::new(
                ItemId::new("c-1"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("call"),
                    "exec_command",
                    arguments,
                )),
            )]);
            let done = ModelResponse::new(vec![RunItem::new(
                ItemId::new("m-1"),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )]);
            let result = Runner::run(run_request(&fixture, vec![call, done]))
                .await
                .unwrap();
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            let output = result
                .state()
                .generated_items()
                .iter()
                .find_map(|item| match item.kind() {
                    RunItemKind::ToolCallOutput(output) => Some(output),
                    _ => None,
                })
                .expect("invalid input is returned as tool output");
            assert!(output.output().to_string().contains(message), "{output:?}");
            assert!(
                !result
                    .state()
                    .generated_items()
                    .iter()
                    .any(|item| matches!(item.kind(), RunItemKind::ToolApproval(_)))
            );
            assert!(!fixture.dir.path().join("invalid-request.txt").exists());
        }
    }
}
