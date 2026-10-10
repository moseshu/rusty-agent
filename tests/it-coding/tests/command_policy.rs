//! Codex's command policy as the coding host's `exec_command` applies it: which commands ask
//! before they run, which are refused with Codex's reason, and which leave the sandbox because a
//! rule allows every command they run.

use std::sync::Arc;

use ra_coding::{
    exec_policy::{ExecPolicyManager, ExecPolicySources},
    host::CodingHost,
    thread_settings::CodingThreadSettingsAdapter,
};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolContext},
};
use ra_protocol::server::{
    ThreadSettingsAdapter,
    protocol::{AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    host: CodingHost,
    tool: Arc<dyn Tool>,
    agent: Arc<AgentSpec>,
    config: ra_runtime::runner::RunConfig,
}

async fn fixture(approval: AskForApproval, sandbox: SandboxPolicy, rules: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let rules_home = tempfile::tempdir().unwrap();
    let mut adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), sandbox).unwrap();
    if let Some(rules) = rules {
        std::fs::create_dir_all(rules_home.path().join("rules")).unwrap();
        std::fs::write(rules_home.path().join("rules").join("test.rules"), rules).unwrap();
        let manager = ExecPolicyManager::load(
            &ExecPolicySources::new().with_config_folder(rules_home.path()),
        )
        .await
        .unwrap();
        adapter = adapter.with_exec_policy(Arc::new(manager));
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
        host,
        tool,
        agent,
        config,
    }
}

impl Fixture {
    fn run(&self) -> RunContext {
        RunContext::new(RunId::new("run"), &self.agent)
            .with_app_context(self.config.app_context().unwrap().clone())
    }

    async fn needs_approval(&self, arguments: &Value) -> bool {
        let run = self.run();
        let call_id = CallId::new("call");
        let context = ToolContext::new(&run, self.tool.as_ref(), &call_id, arguments);
        self.tool.needs_approval(&context).await.unwrap()
    }

    /// The output, or the failure text the model reads.
    async fn call(&self, arguments: &Value) -> Result<String, String> {
        let run = self.run();
        let call_id = CallId::new("call");
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
/// asking, which R13-6's default requirement used to ask about.
#[tokio::test]
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
            .needs_approval(&json!({"cmd": "ls", "yield_time_ms": 2000}))
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
            .needs_approval(&json!({"cmd": "git commit -m x", "yield_time_ms": 2000}))
            .await
    );
}
