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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
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

async fn install_native_profile(
    fixture: &mut Fixture,
    profile: ra_filesystem::models::PermissionProfile,
) {
    use ra_coding::{
        config::{ConfigBuilder, ConfigOverrides},
        thread_settings::ProductPermissionSource,
    };
    let home = tempfile::tempdir().unwrap();
    let cwd = fixture._dir.path().canonicalize().unwrap();
    let builder = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd),
            permission_profile: Some(profile),
            approval_policy: Some(AskForApproval::OnRequest),
            ..ConfigOverrides::default()
        });
    let config = builder.clone().build().await.unwrap();
    fixture.adapter = fixture
        .adapter
        .clone()
        .with_product_config(&config, ProductPermissionSource::new(builder));
    fixture.config = fixture.adapter.initialize(fixture.config.clone());
}

/// Approval cannot add new denied reads to a terminal whose process has already started.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn denied_read_drift_refuses_stdin_even_when_sandbox_prompts_are_allowed() {
    use ra_filesystem::{
        models::PermissionProfile,
        permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy,
            NetworkSandboxPolicy,
        },
    };
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let session = fixture.open(json!({})).await;
    let root = fixture._dir.path().canonicalize().unwrap();
    let mut policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &ra_filesystem::legacy::SandboxPolicy::new_workspace_write_policy(),
        &root,
    );
    policy.entries.push(FileSystemSandboxEntry::new(
        ra_paths::AbsolutePathBuf::from_absolute_path(root.join("private"))
            .unwrap()
            .into(),
        FileSystemAccessMode::Deny,
    ));
    install_native_profile(
        &mut fixture,
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
    )
    .await;
    let text = fixture.refusal(&session, "hello\n").await;
    assert!(text.contains("this terminal cannot enforce the current denied-read restrictions; start a new terminal"), "{text}");
}

/// The coding adapter must carry its canonical profile into exec_command, beyond its wire view.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn coding_shell_enforces_canonical_deny_read_and_metadata_protection() {
    use ra_filesystem::{
        models::PermissionProfile,
        permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy,
            NetworkSandboxPolicy,
        },
    };
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let root = fixture._dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".rusty-agent/rules")).unwrap();
    std::fs::write(root.join(".rusty-agent/rules/default.rules"), "original").unwrap();
    std::fs::write(root.join("secret"), "private-token").unwrap();
    let mut policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &ra_filesystem::legacy::SandboxPolicy::new_workspace_write_policy(),
        &root,
    );
    policy.entries.push(FileSystemSandboxEntry::new(
        ra_paths::AbsolutePathBuf::from_absolute_path(root.join("secret"))
            .unwrap()
            .into(),
        FileSystemAccessMode::Deny,
    ));
    install_native_profile(
        &mut fixture,
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
    )
    .await;
    let output = fixture.call(&fixture.exec, "denied-command", &json!({"cmd": "cat secret; printf changed > .rusty-agent/rules/default.rules", "yield_time_ms":1000}), None).await.unwrap();
    let text = output.as_text().unwrap();
    assert!(!text.contains("private-token"), "{text}");
    assert!(text.contains("Operation not permitted"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join(".rusty-agent/rules/default.rules")).unwrap(),
        "original"
    );
}

/// Explicit approval may grant broad writes, but denied reads still require Seatbelt.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn approved_escalation_keeps_denied_reads_enforced() {
    use ra_filesystem::{
        models::PermissionProfile,
        permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy,
            NetworkSandboxPolicy,
        },
    };
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let root = fixture._dir.path().canonicalize().unwrap();
    std::fs::write(root.join("secret"), "private-token").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().canonicalize().unwrap().join("written");
    let mut policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &ra_filesystem::legacy::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        },
        &root,
    );
    policy.entries.push(FileSystemSandboxEntry::new(
        ra_paths::AbsolutePathBuf::from_absolute_path(root.join("secret"))
            .unwrap()
            .into(),
        FileSystemAccessMode::Deny,
    ));
    policy.entries.push(FileSystemSandboxEntry::new(
        ra_filesystem::permissions::FileSystemPath::GlobPattern {
            pattern: "~/.rusty-agent-denied-test/**".to_owned(),
        },
        FileSystemAccessMode::Deny,
    ));
    install_native_profile(
        &mut fixture,
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
    )
    .await;
    fixture
        .call(
            &fixture.exec,
            "before-escalation",
            &json!({
                "cmd": format!("printf unapproved > '{}'", target.display()), "yield_time_ms":1000,
            }),
            None,
        )
        .await
        .unwrap();
    assert!(
        !target.exists(),
        "the baseline must not already allow the approved target"
    );
    let arguments = json!({"cmd":format!("printf approved > '{}'; cat secret",target.display()), "sandbox_permissions":"require_escalated", "justification":"write outside the workspace", "yield_time_ms":1000});
    let approval = fixture
        .asks_as(&fixture.exec, "approved-deny-read", &arguments)
        .await
        .expect("explicit escalation asks");
    let output = fixture
        .call(
            &fixture.exec,
            "approved-deny-read",
            &arguments,
            Some(&approval),
        )
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
    assert!(!text.contains("private-token"), "{text}");
    assert!(text.contains("Operation not permitted"), "{text}");
}

/// Explicit additional entries cross the product/native type boundary without flattening them.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn additional_permissions_preserve_denies_and_nested_read_only_paths() {
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let root = fixture._dir.path().canonicalize().unwrap();
    std::fs::write(root.join("secret"), "private-token").unwrap();
    std::fs::write(root.join("read-only.txt"), "original").unwrap();
    let host = CodingHost::open(&root)
        .unwrap()
        .with_exec_permission_approvals();
    fixture.exec = host.shell_capability().unwrap().exec_command();
    let arguments = json!({"cmd":"cat secret; printf changed > read-only.txt", "yield_time_ms":1000,
        "sandbox_permissions":"with_additional_permissions",
        "additional_permissions":{"file_system":{"entries":[
            {"path":{"type":"path","path":root.join("secret")},"access":"deny"},
            {"path":{"type":"path","path":root.join("read-only.txt")},"access":"read"}
        ]}}
    });
    let approval = fixture
        .asks_as(&fixture.exec, "additional-denies", &arguments)
        .await;
    let output = fixture
        .call(
            &fixture.exec,
            "additional-denies",
            &arguments,
            approval.as_ref(),
        )
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    assert!(!text.contains("private-token"), "{text}");
    assert!(text.contains("Operation not permitted"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("read-only.txt")).unwrap(),
        "original"
    );
}

/// A retry after sandbox denial cannot remove the only enforcement of denied reads.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn approved_retry_after_denied_read_keeps_the_sandbox() {
    use ra_filesystem::{
        models::PermissionProfile,
        permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy,
            NetworkSandboxPolicy,
        },
    };
    let mut fixture = fixture(AskForApproval::OnRequest, workspace());
    let root = fixture._dir.path().canonicalize().unwrap();
    std::fs::write(root.join("secret"), "private-token").unwrap();
    let mut policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &ra_filesystem::legacy::SandboxPolicy::new_workspace_write_policy(),
        &root,
    );
    policy.entries.push(FileSystemSandboxEntry::new(
        ra_paths::AbsolutePathBuf::from_absolute_path(root.join("secret"))
            .unwrap()
            .into(),
        FileSystemAccessMode::Deny,
    ));
    install_native_profile(
        &mut fixture,
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
    )
    .await;
    fixture
        .update(&ThreadPolicyOverrides::new().with_approval_policy(AskForApproval::UnlessTrusted));
    let arguments = json!({"cmd":"printf attempt >> attempts; cat secret", "yield_time_ms":1000});
    let approval = fixture
        .asks_as(&fixture.exec, "retry-denied-read", &arguments)
        .await
        .expect("initial command asks");
    let output = fixture
        .call(
            &fixture.exec,
            "retry-denied-read",
            &arguments,
            Some(&approval),
        )
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    assert!(!text.contains("private-token"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("attempts")).unwrap(),
        "attemptattempt"
    );
}

/// Profile restrictions survive the product-to-native boundary on legacy platforms.
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn legacy_platform_commands_refuse_unenforceable_profiles_before_launch() {
    use ra_filesystem::{
        models::PermissionProfile,
        permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy,
            NetworkSandboxPolicy,
        },
    };
    for case in ["metadata", "deny", "readonly", "approved-deny"] {
        let mut fixture = fixture(AskForApproval::OnRequest, workspace());
        let root = fixture._dir.path().canonicalize().unwrap();
        let mut fs = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
            &ra_filesystem::legacy::SandboxPolicy::new_workspace_write_policy(),
            &root,
        );
        if case != "metadata" {
            fs.entries.push(FileSystemSandboxEntry::new(
                ra_paths::AbsolutePathBuf::from_absolute_path(root.join("restricted"))
                    .unwrap()
                    .into(),
                if case.contains("deny") {
                    FileSystemAccessMode::Deny
                } else {
                    FileSystemAccessMode::Read
                },
            ));
        }
        install_native_profile(
            &mut fixture,
            PermissionProfile::from_runtime_permissions(&fs, NetworkSandboxPolicy::Restricted),
        )
        .await;
        let mut arguments = json!({"cmd": "echo launched > must-not-exist", "yield_time_ms":1000});
        let approval = if case == "approved-deny" {
            arguments["sandbox_permissions"] = json!("require_escalated");
            arguments["justification"] = json!("approved test command");
            Some(
                fixture
                    .asks_as(&fixture.exec, "legacy-rejected", &arguments)
                    .await
                    .expect("explicit escalation asks"),
            )
        } else {
            None
        };
        let error = fixture
            .call(
                &fixture.exec,
                "legacy-rejected",
                &arguments,
                approval.as_ref(),
            )
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("Failed to create unified exec process: "),
            "{case}: {text}"
        );
        assert!(
            text.contains("legacy command sandbox cannot enforce this profile"),
            "{case}: {text}"
        );
        assert!(!root.join("must-not-exist").exists());
    }
}
