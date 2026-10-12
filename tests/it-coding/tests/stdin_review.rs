//! Codex's stdin review as the coding host's `write_stdin` applies it
//! (`core/src/unified_exec/stdin_approval.rs`): input to a running terminal asks first only when
//! the terminal runs with authority a command launched now would not get — it was launched
//! outside its sandbox, or the thread's sandbox changed since — with Codex's reason; where the
//! approval policy rejects sandbox prompts such input is refused. Waits and signals are never
//! reviewed, and an approved input is written.

use std::sync::Arc;

use ra_coding::{host::CodingHost, thread_settings::CodingThreadSettingsAdapter};
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Result,
    item::CallId,
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_protocol::server::{
    ThreadSettingsAdapter,
    protocol::{AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::runner::RunConfig;
use serde_json::{Value, json};

fn workspace() -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    }
}

fn read_only() -> SandboxPolicy {
    SandboxPolicy::ReadOnly {
        network_access: false,
    }
}

const DRIFTED: &str = "Send input to an existing terminal. This terminal retains sandbox or network settings that differ from the current permissions. The cwd is its launch directory; the terminal's current directory and state may have changed.";
const UNSANDBOXED: &str = "Send input to an existing terminal. This terminal runs without a filesystem sandbox. The cwd is its launch directory; the terminal's current directory and state may have changed.";
const ESCALATED: &str = "Send input to an existing terminal. This terminal was launched outside the sandbox, bypassing any managed network proxy. The cwd is its launch directory; the terminal's current directory and state may have changed.";

fn policy_state(state: &Value) -> &Value {
    ra_tools::write_stdin::approval_policy_state(state).expect("the tool keeps its policy payload")
}

struct Fixture {
    _dir: tempfile::TempDir,
    exec: Arc<dyn Tool>,
    stdin: Arc<dyn Tool>,
    agent: Arc<AgentSpec>,
    adapter: CodingThreadSettingsAdapter,
    config: RunConfig,
}

fn fixture(approval: AskForApproval, sandbox: SandboxPolicy) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), sandbox).unwrap();
    let config = adapter
        .prepare(
            &adapter.initialize(host.build_run_config()),
            &ThreadPolicyOverrides::new().with_approval_policy(approval),
        )
        .unwrap();
    let capability = host.shell_capability().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("coder")
        .instructions("code")
        .build()
        .unwrap();
    Fixture {
        _dir: dir,
        exec: capability.exec_command(),
        stdin: capability.write_stdin(),
        agent,
        adapter,
        config,
    }
}

impl Fixture {
    fn run(&self) -> RunContext {
        RunContext::new(RunId::new("run"), &self.agent)
            .with_app_context(self.config.app_context().unwrap().clone())
    }

    /// Changes the thread's policy, as `thread/settings/update` does.
    fn update(&mut self, update: &ThreadPolicyOverrides) {
        self.config = self.adapter.prepare(&self.config, update).unwrap();
    }

    async fn call(
        &self,
        tool: &Arc<dyn Tool>,
        call_id: &str,
        arguments: &Value,
        resume: Option<&Value>,
    ) -> Result<ToolOutput> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, tool.as_ref(), &call_id, arguments);
        let context = match resume {
            Some(state) => context.with_resume_state(state),
            None => context,
        };
        tool.call(context).await
    }

    async fn asks(&self, tool: &Arc<dyn Tool>, arguments: &Value) -> Option<Value> {
        self.asks_as(tool, "write", arguments).await
    }

    async fn asks_as(
        &self,
        tool: &Arc<dyn Tool>,
        call_id: &str,
        arguments: &Value,
    ) -> Option<Value> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, tool.as_ref(), &call_id, arguments);
        if !tool.needs_approval(&context).await.unwrap() {
            return None;
        }
        Some(
            tool.approval_state(&context)
                .await
                .unwrap()
                .expect("an input that asks keeps what it asks about"),
        )
    }

    /// Opens `cat` as a terminal, approving the launch first if it asks, and returns its session.
    async fn open(&self, extra: Value) -> String {
        let mut arguments = json!({"cmd": "cat", "yield_time_ms": 300});
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        let approved = self.asks_as(&self.exec, "open", &arguments).await;
        let output = self
            .call(&self.exec, "open", &arguments, approved.as_ref())
            .await
            .unwrap();
        let text = output.as_text().unwrap().to_owned();
        let start = text.find("Session `").expect("the command keeps running") + "Session `".len();
        text[start..][..text[start..].find('`').unwrap()].to_owned()
    }

    fn input(session: &str, chars: &str) -> Value {
        json!({"session_id": session, "chars": chars, "yield_time_ms": 500})
    }

    async fn write(&self, session: &str, chars: &str, resume: Option<&Value>) -> Result<String> {
        self.call(&self.stdin, "write", &Self::input(session, chars), resume)
            .await
            .map(|output| output.as_text().unwrap_or_default().to_owned())
    }

    /// The failure text the model reads for a refused input.
    async fn refusal(&self, session: &str, chars: &str) -> String {
        let arguments = Self::input(session, chars);
        let error = self
            .call(&self.stdin, "write", &arguments, None)
            .await
            .expect_err("the input is refused");
        let run = self.run();
        let call_id = CallId::new("write");
        let context = ToolContext::new(&run, self.stdin.as_ref(), &call_id, &arguments);
        self.stdin
            .handle_failure(&context, &error)
            .await
            .unwrap()
            .and_then(|output| output.as_text().map(str::to_owned))
            .unwrap_or_else(|| error.to_string())
    }
}

/// Input to a terminal running under the thread's current sandbox is written without asking.
#[tokio::test]
async fn input_under_the_launch_sandbox_does_not_ask() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture.open(json!({})).await;
    assert_eq!(
        fixture
            .asks(&fixture.stdin, &Fixture::input(&session, "hello\n"))
            .await,
        None
    );
    let output = fixture.write(&session, "hello\n", None).await.unwrap();
    assert!(output.contains("hello"), "{output}");
}

/// Once the thread's sandbox changes, input to a terminal launched under the old one asks with
/// Codex's reason: one launched without a sandbox says so, one launched in a different sandbox
/// says its settings differ.
#[tokio::test]
async fn a_changed_sandbox_asks_with_codexs_reason() {
    for (launched, reason) in [
        (workspace(), DRIFTED),
        (SandboxPolicy::DangerFullAccess, UNSANDBOXED),
    ] {
        let mut fixture = fixture(AskForApproval::OnRequest, launched);
        let session = fixture.open(json!({})).await;
        fixture.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
        let state = fixture
            .asks(&fixture.stdin, &Fixture::input(&session, "hello\n"))
            .await
            .expect("the input asks");
        assert_eq!(policy_state(&state)["reason"], reason);
        assert_eq!(policy_state(&state)["launch_call_id"], "open");
        assert_eq!(policy_state(&state)["session_id"], session.as_str());
        assert_eq!(policy_state(&state)["input"], "hello\n");
    }
}

/// A terminal launched outside its sandbox asks for every input, even under the same policy, as
/// Codex reviews a terminal whose launch bypassed its sandbox.
#[tokio::test]
async fn an_escalated_terminal_asks_for_every_input() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture
        .open(json!({"sandbox_permissions": "require_escalated"}))
        .await;
    let state = fixture
        .asks(&fixture.stdin, &Fixture::input(&session, "hello\n"))
        .await
        .expect("the input asks");
    assert_eq!(policy_state(&state)["reason"], ESCALATED);
}

/// An approved input is written as it was approved.
#[tokio::test]
async fn an_approved_input_is_written() {
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture.open(json!({})).await;
    fixture.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
    let state = fixture
        .asks(&fixture.stdin, &Fixture::input(&session, "approved\n"))
        .await
        .expect("the input asks");
    let output = fixture
        .write(&session, "approved\n", Some(&state))
        .await
        .unwrap();
    assert!(output.contains("approved"), "{output}");
}

/// Where the approval policy rejects sandbox prompts, input that would ask is refused with
/// Codex's reason instead; so is input carrying a NUL byte, which cannot be shown for review.
#[tokio::test]
async fn input_that_cannot_be_asked_about_is_refused() {
    let mut never = fixture(AskForApproval::Never, workspace());
    let session = never.open(json!({})).await;
    never.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
    assert_eq!(
        never
            .asks(&never.stdin, &Fixture::input(&session, "hello\n"))
            .await,
        None
    );
    assert_eq!(
        never.refusal(&session, "hello\n").await,
        "approval required by policy, but AskForApproval is set to Never"
    );

    let mut on_request = fixture(AskForApproval::OnRequest, workspace());
    let session = on_request.open(json!({})).await;
    on_request.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
    assert_eq!(
        on_request.refusal(&session, "nul\0byte").await,
        "terminal input contains a NUL byte and cannot be reviewed safely"
    );
}

/// A wait and a signal are not reviewed, however the sandbox changed.
#[tokio::test]
async fn waits_and_signals_are_not_reviewed() {
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture.open(json!({})).await;
    fixture.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
    assert_eq!(
        fixture
            .asks(&fixture.stdin, &Fixture::input(&session, ""))
            .await,
        None
    );
    let interrupt =
        json!({"session_id": session, "chars": "", "control": "interrupt", "yield_time_ms": 500});
    assert_eq!(fixture.asks(&fixture.stdin, &interrupt).await, None);
}

/// A terminal a sandbox retry launched outside its sandbox asks for every input, as Codex reviews
/// a terminal whose retry left the sandbox.
#[tokio::test]
async fn a_terminal_a_sandbox_retry_launched_asks_for_every_input() {
    let outside = tempfile::tempdir().unwrap();
    let fixture = fixture(
        AskForApproval::Granular {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        },
        SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        },
    );
    let target = outside.path().join("written.txt");
    let arguments = json!({
        "cmd": format!("touch '{}' && cat", target.display()),
        "yield_time_ms": 300
    });
    let error = fixture
        .call(&fixture.exec, "open", &arguments, None)
        .await
        .expect_err("the refused attempt asks to run outside the sandbox");
    let retry = ra_core::tool::ToolApprovalRequest::of(&error)
        .expect("an approval request")
        .state()
        .clone();
    let output = fixture
        .call(&fixture.exec, "open", &arguments, Some(&retry))
        .await
        .unwrap();
    assert!(target.exists());
    let text = output.as_text().unwrap().to_owned();
    let start = text.find("Session `").expect("the retry keeps running") + "Session `".len();
    let session = text[start..][..text[start..].find('`').unwrap()].to_owned();

    let state = fixture
        .asks(&fixture.stdin, &Fixture::input(&session, "hello\n"))
        .await
        .expect("the input asks");
    assert_eq!(policy_state(&state)["reason"], ESCALATED);
}

/// A signal sent with input is a call `write_stdin` refuses on its own; it is not asked about.
#[tokio::test]
async fn a_signal_with_input_is_not_asked_about() {
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture.open(json!({})).await;
    fixture.update(&ThreadPolicyOverrides::new().with_sandbox_policy(read_only()));
    let invalid =
        json!({"session_id": session, "chars": "x", "control": "interrupt", "yield_time_ms": 500});
    assert_eq!(fixture.asks(&fixture.stdin, &invalid).await, None);
}

/// Codex refuses an input whose complete JSON action plus reason exceeds 8,000 bytes, before
/// asking or writing any of it. Escaping and UTF-8 bytes both count, not just input characters.
#[tokio::test]
async fn oversized_review_actions_are_refused_without_writing() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture
        .open(json!({"sandbox_permissions": "require_escalated"}))
        .await;
    for chars in [
        format!("#{}\nREJECTED=1\n", "\"".repeat(4_100)),
        format!("{}REJECTED=1\n", "\u{1b}".repeat(1_400)),
        format!("{}REJECTED=1\n", "界".repeat(2_700)),
    ] {
        assert!(
            fixture
                .asks(&fixture.stdin, &Fixture::input(&session, &chars))
                .await
                .is_none()
        );
        assert_eq!(
            fixture.refusal(&session, &chars).await,
            "terminal input and permission details are too large to review safely; use a smaller input or start a new terminal with fewer grants"
        );
    }
    let state = fixture
        .asks(&fixture.stdin, &Fixture::input(&session, "accepted\n"))
        .await
        .unwrap();
    let output = fixture
        .write(&session, "accepted\n", Some(&state))
        .await
        .unwrap();
    assert!(output.contains("accepted"), "{output}");
    assert!(!output.contains("REJECTED"), "{output}");
}

/// The complete reviewed action may fill the upstream limit exactly; one extra byte refuses it.
#[tokio::test]
async fn the_review_size_limit_includes_metadata_and_reason() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture
        .open(json!({"sandbox_permissions": "require_escalated"}))
        .await;
    let state = fixture
        .asks(&fixture.stdin, &Fixture::input(&session, "x"))
        .await
        .unwrap();
    let action = json!({
        "tool": "write_stdin", "session_id": session, "chars": "",
        "cwd": policy_state(&state)["cwd"],
        "sandbox_permissions": "require_escalated", "tty": false
    });
    let overhead = serde_json::to_string_pretty(&action).unwrap().len() + ESCALATED.len();
    let fits = "x".repeat(8_000 - overhead);
    assert!(
        fixture
            .asks(&fixture.stdin, &Fixture::input(&session, &fits))
            .await
            .is_some()
    );
    let exceeds = format!("{fits}x");
    assert!(
        fixture
            .asks(&fixture.stdin, &Fixture::input(&session, &exceeds))
            .await
            .is_none()
    );
}

/// The saved approval authorizes exactly its input and session, even through the public tool API.
#[tokio::test]
async fn resumed_input_is_bound_to_all_original_arguments() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture
        .open(json!({"sandbox_permissions": "require_escalated"}))
        .await;
    let arguments = Fixture::input(&session, "approved\n");
    let state = fixture.asks(&fixture.stdin, &arguments).await.unwrap();
    // The invocation binding survives persistence.
    let state: Value = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    for (field, changed) in [
        ("chars", json!("unreviewed\n")),
        ("session_id", json!("another-session")),
        ("yield_time_ms", json!(100)),
        ("control", json!("cancel")),
    ] {
        let mut other = arguments.clone();
        other[field] = changed;
        let error = fixture
            .call(&fixture.stdin, "write", &other, Some(&state))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("different invocation"),
            "{error}"
        );
    }
    let output = fixture
        .write(&session, "approved\n", Some(&state))
        .await
        .unwrap();
    assert!(
        output.contains("approved") && !output.contains("unreviewed"),
        "{output}"
    );
}

/// A different call or run, and malformed state, cannot inherit another input's approval.
#[tokio::test]
async fn resumed_input_rejects_other_invocations_and_invalid_state() {
    let fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture
        .open(json!({"sandbox_permissions": "require_escalated"}))
        .await;
    let arguments = Fixture::input(&session, "approved\n");
    let state = fixture.asks(&fixture.stdin, &arguments).await.unwrap();
    let error = fixture
        .call(&fixture.stdin, "other-call", &arguments, Some(&state))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("different invocation"),
        "{error}"
    );
    let run = RunContext::new(RunId::new("another-run"), &fixture.agent)
        .with_app_context(fixture.config.app_context().unwrap().clone());
    let call_id = CallId::new("write");
    let context = ToolContext::new(&run, fixture.stdin.as_ref(), &call_id, &arguments)
        .with_resume_state(&state);
    let error = fixture.stdin.call(context).await.unwrap_err();
    assert!(
        error.to_string().contains("different invocation"),
        "{error}"
    );
    for malformed in [json!(null), json!({}), policy_state(&state).clone()] {
        let error = fixture
            .call(&fixture.stdin, "write", &arguments, Some(&malformed))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot restore terminal input approval"),
            "{error}"
        );
    }
    let output = fixture
        .write(&session, "approved\n", Some(&state))
        .await
        .unwrap();
    assert!(output.contains("approved"), "{output}");
}
