//! Codex's session approval cache as a coding thread keeps it: a command or patch the host
//! approves for the session is not asked about again in that thread. A command is keyed by its
//! canonical form, executable, working directory, terminal and sandbox override; a patch by every
//! path it touches, a move's destination included, and it skips only when all of them are approved.
//! Only `ApprovedForSession` is cached, and each thread starts with an empty cache.

use std::sync::Arc;

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
        CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolApproval,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    state::RunId,
    tool::{Tool, ToolApprovalRequest, ToolContext, ToolOutput},
};
use ra_protocol::server::{
    ReviewDecision, ThreadSettingsAdapter,
    protocol::{AskForApproval, ExecPolicyAmendment, SandboxPolicy, ThreadPolicyOverrides},
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

fn granular_sandbox_approval() -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval: true,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    outside: tempfile::TempDir,
    _rules_home: tempfile::TempDir,
    host: CodingHost,
    exec: Arc<dyn Tool>,
    patch: Arc<dyn Tool>,
    agent: Arc<AgentSpec>,
    adapter: CodingThreadSettingsAdapter,
    approval: AskForApproval,
    config: RunConfig,
}

async fn fixture(approval: AskForApproval) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let rules_home = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let manager =
        ExecPolicyManager::load(&ExecPolicySources::new().with_config_folder(rules_home.path()))
            .await
            .unwrap();
    let adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), workspace_only())
        .unwrap()
        .with_exec_policy(Arc::new(manager));
    let exec = host.exec_command_tool().unwrap();
    let patch = host.apply_patch_tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("coder")
        .instructions("code")
        .tools(vec![Arc::clone(&exec), Arc::clone(&patch)])
        .build()
        .unwrap();
    let mut fixture = Fixture {
        dir,
        outside,
        _rules_home: rules_home,
        host,
        exec,
        patch,
        agent,
        adapter,
        approval,
        config: RunConfig::default(),
    };
    fixture.new_thread();
    fixture
}

impl Fixture {
    /// Starts another thread with the same adapter and policy.
    fn new_thread(&mut self) {
        let config = self.adapter.initialize(self.host.build_run_config());
        self.config = self
            .adapter
            .prepare(
                &config,
                &ThreadPolicyOverrides::new().with_approval_policy(self.approval),
            )
            .unwrap();
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

    /// Answers the approval a call of `tool` with `arguments` asks for with `decision`.
    fn answer(&self, tool: &str, arguments: &Value, decision: &ReviewDecision) {
        let approval = ToolApproval::new(CallId::new("asked"), tool, arguments.clone());
        self.settings().cache_review_decision(&approval, decision);
    }

    async fn asks(&self, tool: &Arc<dyn Tool>, call_id: &str, arguments: &Value) -> bool {
        let run = self.run();
        let call_id = CallId::new(call_id);
        tool.needs_approval(&ToolContext::new(&run, tool.as_ref(), &call_id, arguments))
            .await
            .unwrap()
    }

    async fn command_asks(&self, call_id: &str, arguments: &Value) -> bool {
        self.asks(&self.exec, call_id, arguments).await
    }

    async fn patch_asks(&self, call_id: &str, arguments: &Value) -> bool {
        self.asks(&self.patch, call_id, arguments).await
    }

    async fn call(
        &self,
        call_id: &str,
        arguments: &Value,
        resume: Option<&Value>,
    ) -> Result<ToolOutput> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, self.exec.as_ref(), &call_id, arguments);
        let context = match resume {
            Some(state) => context.with_resume_state(state),
            None => context,
        };
        self.exec.call(context).await
    }

    /// A command that logs one attempt in the workspace, then writes outside it, which the
    /// sandbox refuses.
    fn refused_command(&self) -> Value {
        let target = self.outside.path().join("written.txt");
        json!({
            "cmd": format!("echo attempt >> attempts.log; touch '{}'", target.display()),
            "yield_time_ms": 5000
        })
    }

    fn attempts(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("attempts.log"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }
}

fn command(cmd: &str) -> Value {
    json!({"cmd": cmd, "yield_time_ms": 5000})
}

fn patch(body: &str) -> Value {
    json!({"patch": format!("*** Begin Patch\n{body}*** End Patch\n")})
}

/// Approved for the session, a command runs again without asking, also spelled with other
/// whitespace, which Codex's canonical form ignores.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_command_approved_for_the_session_does_not_ask_again() {
    let fixture = fixture(AskForApproval::UnlessTrusted).await;
    let first = command("touch made.txt");
    assert!(fixture.command_asks("first", &first).await);
    fixture.answer("exec_command", &first, &ReviewDecision::ApprovedForSession);

    assert!(!fixture.command_asks("second", &first).await);
    let spaced = command("touch   made.txt");
    assert!(!fixture.command_asks("spaced", &spaced).await);
    fixture.call("spaced", &spaced, None).await.unwrap();
    assert!(fixture.dir.path().join("made.txt").exists());
}

/// Only a session approval is cached: approving once, approving with an amendment, or refusing
/// leaves the next call asking.
#[tokio::test]
async fn other_decisions_are_not_cached() {
    let fixture = fixture(AskForApproval::UnlessTrusted).await;
    let arguments = command("touch made.txt");
    for decision in [
        ReviewDecision::Approved,
        ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment: ExecPolicyAmendment::new(["touch"]),
        },
        ReviewDecision::denied("no"),
        ReviewDecision::Abort,
    ] {
        fixture.answer("exec_command", &arguments, &decision);
        assert!(
            fixture.command_asks("next", &arguments).await,
            "{decision:?}"
        );
    }
}

/// The key holds where and how the command runs: another working directory, executable or
/// sandbox override asks again; the same directory spelled differently does not. A plain command
/// is keyed by its words, so `-lc` and `-c` share a key; a script keeps its text and shell mode.
#[tokio::test]
async fn the_key_holds_where_and_how_the_command_runs() {
    let fixture = fixture(AskForApproval::UnlessTrusted).await;
    std::fs::create_dir(fixture.dir.path().join("sub")).unwrap();
    let approved = command("touch made.txt");
    fixture.answer(
        "exec_command",
        &approved,
        &ReviewDecision::ApprovedForSession,
    );

    let root = fixture.host.workspace_root().display().to_string();
    for (case, arguments) in [
        ("dot", json!({"cmd": "touch made.txt", "workdir": "."})),
        (
            "absolute root",
            json!({"cmd": "touch made.txt", "workdir": root}),
        ),
        (
            "login shell",
            json!({"cmd": "touch made.txt", "login": true}),
        ),
    ] {
        assert!(!fixture.command_asks(case, &arguments).await, "{case}");
    }
    for (case, arguments) in [
        (
            "other directory",
            json!({"cmd": "touch made.txt", "workdir": "sub"}),
        ),
        (
            "other executable",
            json!({"cmd": "touch made.txt", "shell": "/bin/bash"}),
        ),
        (
            "escalated",
            json!({"cmd": "touch made.txt", "sandbox_permissions": "require_escalated"}),
        ),
        ("other command", command("touch other.txt")),
    ] {
        assert!(fixture.command_asks(case, &arguments).await, "{case}");
    }

    let script = command("printf hi > made.txt");
    fixture.answer("exec_command", &script, &ReviewDecision::ApprovedForSession);
    assert!(!fixture.command_asks("script", &script).await);
    let login_script = json!({"cmd": "printf hi > made.txt", "login": true});
    assert!(fixture.command_asks("login script", &login_script).await);
}

/// Each thread starts with an empty cache, as each Codex session does.
#[tokio::test]
async fn each_thread_has_its_own_cache() {
    let mut fixture = fixture(AskForApproval::UnlessTrusted).await;
    let arguments = command("touch made.txt");
    fixture.answer(
        "exec_command",
        &arguments,
        &ReviewDecision::ApprovedForSession,
    );
    assert!(!fixture.command_asks("same thread", &arguments).await);

    fixture.new_thread();
    assert!(fixture.command_asks("other thread", &arguments).await);
}

/// A patch approved for the session approves each path it touches, a move's destination
/// included. A later patch skips only when every path it touches is approved.
#[tokio::test]
async fn a_patch_approved_for_the_session_covers_each_of_its_paths() {
    let fixture = fixture(AskForApproval::OnRequest).await;
    let add = patch("*** Add File: a.txt\n+one\n");
    assert!(fixture.patch_asks("add", &add).await);
    fixture.answer("apply_patch", &add, &ReviewDecision::ApprovedForSession);

    let update = patch("*** Update File: a.txt\n@@\n-one\n+two\n");
    assert!(!fixture.patch_asks("update", &update).await);
    let both = patch("*** Update File: a.txt\n@@\n-one\n+two\n*** Add File: b.txt\n+new\n");
    assert!(fixture.patch_asks("both", &both).await);

    let moved = patch("*** Update File: a.txt\n*** Move to: c.txt\n@@\n-one\n+two\n");
    assert!(
        fixture.patch_asks("move", &moved).await,
        "the source is approved but not the destination"
    );
    fixture.answer("apply_patch", &moved, &ReviewDecision::ApprovedForSession);
    let destination = patch("*** Delete File: c.txt\n");
    assert!(!fixture.patch_asks("destination", &destination).await);

    // The bare-string form names the same paths.
    let bare = json!("*** Begin Patch\n*** Delete File: a.txt\n*** End Patch\n");
    assert!(!fixture.patch_asks("bare", &bare).await);
    assert!(
        fixture
            .patch_asks("unparsed", &json!({"patch": "not a patch"}))
            .await
    );
}

/// A command approved for the session whose first attempt the sandbox refuses runs again outside
/// the sandbox without asking, whether or not its policy asked before it ran.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_refused_command_approved_for_the_session_retries_without_asking() {
    for approval in [granular_sandbox_approval(), AskForApproval::UnlessTrusted] {
        let fixture = fixture(approval).await;
        let arguments = fixture.refused_command();
        fixture.answer(
            "exec_command",
            &arguments,
            &ReviewDecision::ApprovedForSession,
        );

        assert!(
            !fixture.command_asks("call", &arguments).await,
            "{approval:?}"
        );
        let output = fixture.call("call", &arguments, None).await;
        assert!(output.is_ok(), "{approval:?}: {output:?}");
        assert_eq!(fixture.attempts(), 2, "{approval:?}");
        assert!(
            fixture.outside.path().join("written.txt").exists(),
            "{approval:?}"
        );
    }
}

/// The retry approval Codex asks after a sandbox refusal is cached too: approved for the session,
/// the same command later retries without asking.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_retry_approved_for_the_session_is_not_asked_again() {
    let fixture = fixture(granular_sandbox_approval()).await;
    let arguments = fixture.refused_command();
    let error = fixture.call("first", &arguments, None).await.unwrap_err();
    let state = ToolApprovalRequest::of(&error)
        .expect("the refused attempt asks to run outside the sandbox")
        .state()
        .clone();
    fixture.answer(
        "exec_command",
        &arguments,
        &ReviewDecision::ApprovedForSession,
    );
    fixture
        .call("first", &arguments, Some(&state))
        .await
        .unwrap();
    assert_eq!(fixture.attempts(), 2);

    std::fs::remove_file(fixture.outside.path().join("written.txt")).unwrap();
    fixture.call("second", &arguments, None).await.unwrap();
    assert_eq!(fixture.attempts(), 4, "a sandboxed attempt, then the retry");
    assert!(fixture.outside.path().join("written.txt").exists());
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

fn run_request(fixture: &Fixture, run_id: &str, responses: Vec<ModelResponse>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(Arc::clone(&fixture.agent)),
        Arc::new(Resolver(Arc::new(ScriptedModel(std::sync::Mutex::new(
            responses,
        ))))),
        RunId::new(run_id),
        CancelScope::root(),
        Vec::new(),
    )
    .with_config(fixture.config.clone())
    .with_services(fixture.host.build_tool_services())
}

fn tool_call(call_id: &str, arguments: &Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "exec_command",
            arguments.clone(),
        )),
    )])
}

fn done() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("final"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}

/// Through the runner: the approval a run stops on, answered for the session, is cached from the
/// approval item the host receives, and a later turn's call of the same command runs without
/// stopping. The host still approves the paused call itself, once.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_session_approval_answered_through_the_runner_spans_turns() {
    let fixture = fixture(AskForApproval::UnlessTrusted).await;
    // The runner checks arguments against the strict schema, which lists every field.
    let mut arguments = command("echo ran >> ran.log");
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

    let interrupted = Runner::run(run_request(
        &fixture,
        "turn-1",
        vec![tool_call("c-1", &arguments)],
    ))
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
    fixture
        .settings()
        .cache_review_decision(approval, &ReviewDecision::ApprovedForSession);
    let mut state = interrupted.state().clone();
    state.approve(item, false).unwrap();
    let resumed = Runner::run(run_request(&fixture, "turn-1", vec![done()]).with_state(state))
        .await
        .unwrap();
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));

    let second = Runner::run(run_request(
        &fixture,
        "turn-2",
        vec![tool_call("c-2", &arguments), done()],
    ))
    .await
    .unwrap();
    assert!(
        matches!(second.outcome(), RunOutcome::Completed { .. }),
        "{:?}",
        second.outcome()
    );
    let log = std::fs::read_to_string(fixture.dir.path().join("ran.log")).unwrap();
    assert_eq!(log.lines().count(), 2);
}
