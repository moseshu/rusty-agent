//! Codex's command policy as the coding host's `exec_command` applies it: which commands ask
//! before they run, which are refused with Codex's reason, which leave the sandbox because a rule
//! allows every command they run or an approved call asked to, and how an amendment approved for a
//! call applies to later calls but not to that one.

use std::sync::Arc;

use ra_coding::{
    exec_policy::{ExecPolicyManager, ExecPolicySources, default_policy_path},
    host::CodingHost,
    thread_settings::{CodingThreadSettings, CodingThreadSettingsAdapter},
};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId, ItemId, RunItem, RunItemKind, ToolCallOutput},
    state::RunId,
    tool::{Tool, ToolContext},
};
use ra_protocol::server::{
    ReviewDecision, ThreadSettingsAdapter,
    protocol::{AskForApproval, ExecPolicyAmendment, SandboxPolicy, ThreadPolicyOverrides},
};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    rules_home: tempfile::TempDir,
    host: CodingHost,
    tool: Arc<dyn Tool>,
    agent: Arc<AgentSpec>,
    config: ra_runtime::runner::RunConfig,
    adapter: CodingThreadSettingsAdapter,
}

async fn fixture(approval: AskForApproval, sandbox: SandboxPolicy, rules: Option<&str>) -> Fixture {
    fixture_with_rules_home(approval, sandbox, rules, true).await
}

/// A fixture whose rules come from its rules home, which is also where approved amendments go
/// when `writes_amendments` is set.
async fn fixture_with_rules_home(
    approval: AskForApproval,
    sandbox: SandboxPolicy,
    rules: Option<&str>,
    writes_amendments: bool,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let rules_home = tempfile::tempdir().unwrap();
    let mut adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), sandbox).unwrap();
    if let Some(rules) = rules {
        std::fs::create_dir_all(rules_home.path().join("rules")).unwrap();
        std::fs::write(rules_home.path().join("rules").join("test.rules"), rules).unwrap();
    }
    let manager =
        ExecPolicyManager::load(&ExecPolicySources::new().with_config_folder(rules_home.path()))
            .await
            .unwrap();
    adapter = adapter.with_exec_policy(Arc::new(manager));
    if writes_amendments {
        adapter = adapter.with_rules_home(rules_home.path());
    }
    let config = adapter.initialize(host.build_run_config());
    let config = adapter
        .prepare(
            &config,
            &ThreadPolicyOverrides::new().with_approval_policy(approval),
        )
        .unwrap();
    let tool = host.exec_command_tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("coder")
        .instructions("code")
        .build()
        .unwrap();
    Fixture {
        dir,
        rules_home,
        host,
        tool,
        agent,
        config,
        adapter,
    }
}

impl Fixture {
    fn run(&self) -> RunContext {
        RunContext::new(RunId::new("run"), &self.agent)
            .with_app_context(self.config.app_context().unwrap().clone())
    }

    fn settings(&self) -> &CodingThreadSettings {
        self.config
            .app_context()
            .and_then(|context| context.downcast_ref::<CodingThreadSettings>())
            .unwrap()
    }

    async fn needs_approval(&self, arguments: &Value) -> bool {
        self.needs_approval_as("call", arguments).await
    }

    async fn needs_approval_as(&self, call_id: &str, arguments: &Value) -> bool {
        self.approval_result_as(call_id, arguments).await.unwrap()
    }

    async fn approval_result_as(
        &self,
        call_id: &str,
        arguments: &Value,
    ) -> ra_core::error::Result<bool> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, self.tool.as_ref(), &call_id, arguments);
        self.tool.needs_approval(&context).await
    }

    /// The amendment the approval request of a call that is waiting would propose.
    fn proposed_amendment(&self, call_id: &str) -> Option<ExecPolicyAmendment> {
        self.settings()
            .pending_command_requirement(&RunId::new("run"), &CallId::new(call_id))
            .unwrap()
            .proposed_execpolicy_amendment()
            .cloned()
    }

    /// The output, or the failure text the model reads.
    async fn call(&self, arguments: &Value) -> Result<String, String> {
        self.call_as("call", arguments).await
    }

    async fn call_as(&self, call_id: &str, arguments: &Value) -> Result<String, String> {
        let run = self.run();
        let call_id = CallId::new(call_id);
        let context = ToolContext::new(&run, self.tool.as_ref(), &call_id, arguments);
        match self.tool.call(context).await {
            Ok(output) => Ok(format!("{output:?}")),
            Err(error) => {
                let context = ToolContext::new(&run, self.tool.as_ref(), &call_id, arguments);
                let handled = self.tool.handle_failure(&context, &error).await.unwrap();
                Err(handled
                    .and_then(|output| output.as_text().map(str::to_owned))
                    .unwrap_or_else(|| error.to_string()))
            }
        }
    }
}

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

/// Under `OnRequest` with a restricted sandbox, an ordinary command runs in the sandbox without
/// asking, following Codex's default approval requirement.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn on_request_runs_ordinary_commands_without_asking() {
    let fixture = fixture(AskForApproval::OnRequest, workspace(), None).await;
    let arguments = json!({"cmd": "printf hi > made.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval(&arguments).await);
    fixture.call(&arguments).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.path().join("made.txt")).unwrap(),
        "hi"
    );
    let _ = &fixture.host;
}

/// A forced `rm` asks under `OnRequest`, and under `Never` is refused with Codex's reason as the
/// model-visible result, without running.
#[tokio::test]
async fn forced_rm_asks_or_is_refused_with_codex_reason() {
    let on_request = fixture(AskForApproval::OnRequest, workspace(), None).await;
    let arguments = json!({"cmd": "rm -rf victim", "yield_time_ms": 2000});
    assert!(on_request.needs_approval(&arguments).await);

    let never = fixture(AskForApproval::Never, workspace(), None).await;
    std::fs::create_dir(never.dir.path().join("victim")).unwrap();
    assert!(!never.needs_approval(&arguments).await);
    let error = never.call(&arguments).await.unwrap_err();
    assert_eq!(
        error,
        "`/bin/sh -c 'rm -rf victim'` rejected: rm -f style commands are not permitted. Use a safer approach"
    );
    assert!(never.dir.path().join("victim").exists());
}

/// A command every rule allows skips approval even under `UnlessTrusted`, and its first attempt
/// runs outside the sandbox; one no rule matches asks.
#[tokio::test]
async fn allow_rules_skip_approval_and_leave_the_sandbox() {
    let fixture = fixture(
        AskForApproval::UnlessTrusted,
        read_only(),
        Some(r#"prefix_rule(pattern=["touch"], decision="allow")"#),
    )
    .await;
    let allowed = json!({"cmd": "touch outside-sandbox.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval(&allowed).await);
    fixture.call(&allowed).await.unwrap();
    assert!(fixture.dir.path().join("outside-sandbox.txt").exists());

    assert!(
        fixture
            .needs_approval_as("unmatched", &json!({"cmd": "ls", "yield_time_ms": 2000}))
            .await
    );
}

/// A command a rule allows only in part stays in the sandbox: here the redirect keeps the script
/// from being split, so the read-only sandbox still refuses the write.
#[tokio::test]
async fn partly_allowed_commands_stay_in_the_sandbox() {
    let fixture = fixture(
        AskForApproval::OnRequest,
        read_only(),
        Some(r#"prefix_rule(pattern=["printf"], decision="allow")"#),
    )
    .await;
    let arguments = json!({"cmd": "printf changed > blocked.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval(&arguments).await);
    let _ = fixture.call(&arguments).await;
    assert!(!fixture.dir.path().join("blocked.txt").exists());
}

/// A forbidden rule refuses the command with its justification, and a prompt rule asks.
#[tokio::test]
async fn forbidden_and_prompt_rules_apply() {
    let fixture = fixture(
        AskForApproval::OnRequest,
        workspace(),
        Some(
            r#"
prefix_rule(pattern=["git", "push"], decision="forbidden", justification="pushes need review")
prefix_rule(pattern=["git", "commit"], decision="prompt")
"#,
        ),
    )
    .await;
    let push = json!({"cmd": "git push origin main", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval(&push).await);
    assert_eq!(
        fixture.call(&push).await.unwrap_err(),
        "`/bin/sh -c 'git push origin main'` rejected: pushes need review"
    );
    assert!(
        fixture
            .needs_approval_as(
                "commit",
                &json!({"cmd": "git commit -m x", "yield_time_ms": 2000})
            )
            .await
    );
}

fn granular_without_sandbox_approval() -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval: false,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    }
}

/// Codex's `justification` without an explicit `sandbox_permissions` is refused with Codex's text,
/// without asking and without running.
#[tokio::test]
async fn justification_requires_sandbox_permissions() {
    let fixture = fixture(AskForApproval::OnRequest, workspace(), None).await;
    let arguments = json!({
        "cmd": "touch justified.txt",
        "justification": "Do you want to create the file?",
        "yield_time_ms": 2000
    });
    assert!(!fixture.needs_approval(&arguments).await);
    assert_eq!(
        fixture.call(&arguments).await.unwrap_err(),
        "`justification` requires an explicit `sandbox_permissions`; use `sandbox_permissions: \"require_escalated\"` for unsandboxed execution, or omit `justification`."
    );
    assert!(!fixture.dir.path().join("justified.txt").exists());
}

/// Asking to leave the sandbox under a policy that never prompts for it is refused with Codex's
/// text, as Codex's `exec_command` handler refuses it, and the command does not run.
#[tokio::test]
async fn escalation_is_refused_when_sandbox_prompts_are_rejected() {
    let arguments = json!({
        "cmd": "touch escalated.txt",
        "sandbox_permissions": "require_escalated",
        "yield_time_ms": 2000
    });

    let never = fixture(AskForApproval::Never, read_only(), None).await;
    assert!(!never.needs_approval(&arguments).await);
    assert_eq!(
        never.call(&arguments).await.unwrap_err(),
        "approval policy is Never; reject command — you cannot ask for escalated permissions if the approval policy is Never"
    );
    assert!(!never.dir.path().join("escalated.txt").exists());

    let granular = fixture(granular_without_sandbox_approval(), read_only(), None).await;
    assert!(!granular.needs_approval(&arguments).await);
    let error = granular.call(&arguments).await.unwrap_err();
    assert!(
        error.contains("you cannot ask for escalated permissions"),
        "{error}"
    );
    assert!(!granular.dir.path().join("escalated.txt").exists());
}

/// A `require_escalated` call asks, proposes the prefix the model requested, and once approved
/// runs its first attempt outside the read-only sandbox; the same command without the override
/// runs inside it without asking.
#[tokio::test]
async fn approved_escalation_runs_outside_the_sandbox() {
    let fixture = fixture(AskForApproval::OnRequest, read_only(), None).await;
    let escalated = json!({
        "cmd": "touch escalated.txt",
        "sandbox_permissions": "require_escalated",
        "justification": "Do you want to create the file?",
        "prefix_rule": ["touch"],
        "yield_time_ms": 2000
    });
    assert!(fixture.needs_approval_as("escalated", &escalated).await);
    assert_eq!(
        fixture.proposed_amendment("escalated"),
        Some(ExecPolicyAmendment::new(["touch"]))
    );
    fixture.call_as("escalated", &escalated).await.unwrap();
    assert!(fixture.dir.path().join("escalated.txt").exists());

    let sandboxed = json!({"cmd": "touch sandboxed.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval_as("sandboxed", &sandboxed).await);
    let _ = fixture.call_as("sandboxed", &sandboxed).await;
    assert!(!fixture.dir.path().join("sandboxed.txt").exists());
}

/// An amendment approved for a call is written to the rules home and allows later calls, which
/// then skip approval and leave the sandbox; the call it was approved for runs as it was decided,
/// in the sandbox.
#[tokio::test]
async fn an_approved_amendment_applies_to_later_calls_only() {
    let fixture = fixture(AskForApproval::UnlessTrusted, read_only(), None).await;
    let first = json!({"cmd": "touch first.txt", "yield_time_ms": 2000});
    assert!(fixture.needs_approval_as("first", &first).await);
    let amendment = fixture.proposed_amendment("first").unwrap();
    assert_eq!(amendment, ExecPolicyAmendment::new(["touch", "first.txt"]));

    let decision = ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment: ExecPolicyAmendment::new(["touch"]),
    };
    assert_eq!(
        fixture
            .settings()
            .persist_approved_execpolicy_amendment(&decision)
            .await,
        None
    );
    assert_eq!(
        std::fs::read_to_string(default_policy_path(fixture.rules_home.path())).unwrap(),
        "prefix_rule(pattern=[\"touch\"], decision=\"allow\")\n"
    );

    let _ = fixture.call_as("first", &first).await;
    assert!(!fixture.dir.path().join("first.txt").exists());

    let later = json!({"cmd": "touch later.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval_as("later", &later).await);
    fixture.call_as("later", &later).await.unwrap();
    assert!(fixture.dir.path().join("later.txt").exists());
}

/// An amendment that cannot be written yields Codex's warning and changes nothing; the approved
/// call still runs.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn an_amendment_that_cannot_be_written_warns_and_the_call_runs() {
    let decision = ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment: ExecPolicyAmendment::new(["touch"]),
    };
    let arguments = json!({"cmd": "touch approved.txt", "yield_time_ms": 2000});

    let unwritable = fixture(AskForApproval::UnlessTrusted, workspace(), None).await;
    std::fs::write(
        unwritable.rules_home.path().join("rules"),
        "not a directory",
    )
    .unwrap();
    assert!(unwritable.needs_approval(&arguments).await);
    let warning = unwritable
        .settings()
        .persist_approved_execpolicy_amendment(&decision)
        .await
        .unwrap();
    assert!(
        warning.starts_with("Failed to apply execpolicy amendment: failed to update rules file "),
        "{warning}"
    );
    unwritable.call(&arguments).await.unwrap();
    assert!(unwritable.dir.path().join("approved.txt").exists());
    assert!(
        unwritable
            .needs_approval_as(
                "later",
                &json!({"cmd": "touch later.txt", "yield_time_ms": 2000})
            )
            .await
    );

    let homeless =
        fixture_with_rules_home(AskForApproval::UnlessTrusted, workspace(), None, false).await;
    assert_eq!(
        homeless
            .settings()
            .persist_approved_execpolicy_amendment(&decision)
            .await
            .as_deref(),
        Some("Failed to apply execpolicy amendment: no rules home is configured for this thread")
    );
    assert_eq!(
        homeless
            .settings()
            .persist_approved_execpolicy_amendment(&ReviewDecision::Approved)
            .await,
        None
    );
}

/// A failed request consumes its decision, and agents-python's invocation identity rule prevents
/// its call ID from lending that authority to another command.
#[tokio::test]
async fn a_failed_call_cannot_authorize_changed_arguments() {
    let fixture = fixture(
        AskForApproval::OnRequest,
        read_only(),
        Some(
            r#"prefix_rule(pattern=["printf"], decision="allow")
prefix_rule(pattern=["touch"], decision="forbidden")"#,
        ),
    )
    .await;
    let first = json!({"cmd": "printf ok", "workdir": "missing", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval_as("reused", &first).await);
    assert!(fixture.call_as("reused", &first).await.is_err());
    assert!(
        fixture
            .settings()
            .pending_command_requirement(&RunId::new("run"), &CallId::new("reused"))
            .is_none()
    );

    let changed = json!({"cmd": "touch forbidden.txt", "yield_time_ms": 2000});
    let error = fixture
        .approval_result_as("reused", &changed)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("different invocation"),
        "{error}"
    );
    let error = fixture.call_as("reused", &changed).await.unwrap_err();
    assert!(error.contains("different invocation"), "{error}");
    assert!(!fixture.dir.path().join("forbidden.txt").exists());
}

/// Changing a pending call's execution or approval arguments cannot reuse its first decision.
/// Conflicting checks leave the original pending invocation intact.
#[tokio::test]
async fn pending_decisions_are_bound_to_all_arguments() {
    let fixture = fixture(AskForApproval::OnRequest, read_only(), None).await;
    let first = json!({"cmd": "touch original.txt", "sandbox_permissions": "require_escalated"});
    assert!(fixture.needs_approval_as("pending", &first).await);
    for (field, value) in [
        ("cmd", json!("touch changed.txt")),
        ("workdir", json!("other")),
        ("shell", json!("/bin/bash")),
        ("login", json!(true)),
        ("sandbox_permissions", json!("use_default")),
        ("justification", json!("Run this?")),
        ("prefix_rule", json!(["touch"])),
    ] {
        let mut changed = first.clone();
        changed[field] = value;
        assert!(
            fixture
                .approval_result_as("pending", &changed)
                .await
                .is_err(),
            "{field}"
        );
        assert!(
            fixture.call_as("pending", &changed).await.is_err(),
            "{field}"
        );
    }
    assert!(fixture.needs_approval_as("pending", &first).await);
    fixture.call_as("pending", &first).await.unwrap();
    assert!(fixture.dir.path().join("original.txt").exists());
    assert!(!fixture.dir.path().join("changed.txt").exists());
}

/// A reusable amendment changes later calls, while repeated checks of an existing pending call
/// continue to return its frozen approval requirement.
#[tokio::test]
async fn repeated_approval_checks_keep_the_first_decision_after_an_amendment() {
    let fixture = fixture(AskForApproval::UnlessTrusted, read_only(), None).await;
    let first = json!({"cmd": "touch pending.txt", "yield_time_ms": 2000});
    assert!(fixture.needs_approval_as("pending", &first).await);
    let frozen = fixture
        .settings()
        .pending_command_requirement(&RunId::new("run"), &CallId::new("pending"))
        .unwrap();
    let amendment = ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment: ExecPolicyAmendment::new(["touch"]),
    };
    assert_eq!(
        fixture
            .settings()
            .persist_approved_execpolicy_amendment(&amendment)
            .await,
        None
    );
    assert!(fixture.needs_approval_as("pending", &first).await);
    assert_eq!(
        fixture
            .settings()
            .pending_command_requirement(&RunId::new("run"), &CallId::new("pending")),
        Some(frozen)
    );
    let _ = fixture.call_as("pending", &first).await;
    assert!(!fixture.dir.path().join("pending.txt").exists());
    assert!(
        !fixture
            .needs_approval_as("later", &json!({"cmd": "touch later.txt"}))
            .await
    );
}

/// Completed invocations cannot be executed again under the same provider call ID.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn an_executed_call_id_cannot_be_replayed() {
    let fixture = fixture(AskForApproval::OnRequest, workspace(), None).await;
    let arguments = json!({"cmd": "printf x >> once.txt", "yield_time_ms": 2000});
    assert!(!fixture.needs_approval(&arguments).await);
    fixture.call(&arguments).await.unwrap();
    assert!(
        fixture
            .approval_result_as("call", &arguments)
            .await
            .is_err()
    );
    assert!(fixture.call(&arguments).await.is_err());
    assert_eq!(
        std::fs::read_to_string(fixture.dir.path().join("once.txt")).unwrap(),
        "x"
    );
}

/// A refusal output settles the pending decision but keeps its invocation identity until the
/// turn ends, matching agents-python's completed invocation records.
#[tokio::test]
async fn refused_calls_release_their_pending_decisions() {
    let fixture = fixture(AskForApproval::UnlessTrusted, workspace(), None).await;
    let arguments = json!({"cmd": "touch rejected.txt"});
    assert!(fixture.needs_approval(&arguments).await);
    let event = ra_runtime::thread::ThreadEvent::Item {
        turn_id: "run".into(),
        item: RunItem::new(
            ItemId::new("call.output"),
            RunItemKind::ToolCallOutput(
                ToolCallOutput::new(CallId::new("call"), json!("rejected")).with_error(true),
            ),
        ),
    };
    fixture.adapter.on_thread_event(&fixture.config, &event);
    assert!(
        fixture
            .settings()
            .pending_command_requirement(&RunId::new("run"), &CallId::new("call"))
            .is_none()
    );
    assert!(fixture.call(&arguments).await.is_err());
    assert!(!fixture.dir.path().join("rejected.txt").exists());
}

/// Turn completion and cancellation release all records for that turn, including calls that
/// never entered the tool; they leave another run's pending decision intact.
#[tokio::test]
async fn turn_termination_releases_only_its_command_records() {
    use ra_runtime::thread::{ThreadEvent, TurnAbortReason};
    for aborted in [false, true] {
        let fixture = fixture(AskForApproval::UnlessTrusted, workspace(), None).await;
        let arguments = json!({"cmd": "touch pending.txt"});
        assert!(fixture.needs_approval(&arguments).await);
        let other_run = RunContext::new(RunId::new("other"), &fixture.agent)
            .with_app_context(fixture.config.app_context().unwrap().clone());
        let call_id = CallId::new("call");
        let context = ToolContext::new(&other_run, fixture.tool.as_ref(), &call_id, &arguments);
        assert!(fixture.tool.needs_approval(&context).await.unwrap());
        let event = if aborted {
            ThreadEvent::TurnAborted {
                turn_id: "run".into(),
                reason: TurnAbortReason::Interrupted,
                completed_at: 0,
                duration_ms: 0,
            }
        } else {
            ThreadEvent::TurnComplete {
                turn_id: "run".into(),
                last_agent_message: None,
                completed_at: 0,
                duration_ms: 0,
            }
        };
        fixture.adapter.on_thread_event(&fixture.config, &event);
        assert!(
            fixture
                .settings()
                .pending_command_requirement(&RunId::new("run"), &call_id)
                .is_none()
        );
        assert!(
            fixture
                .settings()
                .pending_command_requirement(&RunId::new("other"), &call_id)
                .is_some()
        );
        // The synthetic run can now register a different invocation, proving that termination
        // removed the identity as well as the decision.
        assert!(
            fixture
                .needs_approval(&json!({"cmd": "touch later.txt"}))
                .await
        );
    }
}
