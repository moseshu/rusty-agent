//! Role tools inherit host authority; optional restrictions subtract editing and collaboration.

#[path = "../../support/configured_coding_host.rs"]
#[allow(dead_code)]
mod configured_host;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_control,
    skills::{PluginSkillRoot, SkillDiscoveryConfig},
    thread_settings::{CodingThreadSettings, CodingThreadSettingsAdapter},
};
use ra_core::{
    agent::AgentId,
    context::RunContext,
    item::CallId,
    prompt::PromptRole,
    state::RunId,
    tool::{ToolContext, ToolServices},
    user_input::{RequestUserInputArgs, RequestUserInputResponse, UserInputPort},
};
use ra_protocol::server::{
    ThreadSettingsAdapter,
    protocol::{AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use rstest::rstest;
use serde_json::json;
use tokio::{sync::Notify, time::timeout};

#[derive(Default)]
struct Questions {
    asked: Mutex<Vec<RequestUserInputArgs>>,
    answer: Notify,
}

#[async_trait]
impl UserInputPort for Questions {
    async fn request_user_input(
        &self,
        _: &RunContext,
        _: &CallId,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        self.asked.lock().unwrap().push(args);
        self.answer.notified().await;
        Some(RequestUserInputResponse::new())
    }
}

#[rstest]
#[case(PromptRole::ReadOnlySpecialist, false)]
#[case(PromptRole::ReadOnlySpecialist, true)]
#[case(PromptRole::Planner, false)]
#[case(PromptRole::Planner, true)]
#[tokio::test]
async fn assembled_roles_bind_mode_behavior_without_removing_control_tools(
    #[case] role: PromptRole,
    #[case] restricted: bool,
) {
    let workspace = tempfile::tempdir().unwrap();
    let questions = Arc::new(Questions::default());
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_user_input(questions.clone())
        .with_read_only_role_restrictions(restricted);
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &role,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let run = RunContext::new(RunId::generate(), built.agent());
    let plan = built
        .agent()
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "update_plan")
        .unwrap();
    let call = CallId::new("plan");
    let arguments = json!({"plan": []});
    let result = plan
        .call(ToolContext::new(&run, plan.as_ref(), &call, &arguments))
        .await;
    if matches!(role, PromptRole::Planner) {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("update_plan is a TODO/checklist tool and is not allowed in Plan mode")
        );
    } else {
        assert_eq!(result.unwrap().as_text(), Some("Plan updated"));
    }
    let ask = built
        .agent()
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "request_user_input")
        .unwrap();
    let arguments = json!({"questions": [{"id":"scope", "header":"Scope", "question":"Which scope?", "options": [
        {"label":"Focused", "description":"Review the current change."},
        {"label":"Broad", "description":"Review the full project."}
    ]}]});
    let call = CallId::new("ask");
    let services = ToolServices::new().with_user_input(questions.clone());
    let child =
        RunContext::new(RunId::generate(), built.agent()).with_parent_run_id(RunId::new("parent"));
    let error = ask
        .call(ToolContext::new(&child, ask.as_ref(), &call, &arguments).with_services(&services))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("can only be used by the root thread")
    );
    assert!(questions.asked.lock().unwrap().is_empty());
    let pending =
        ask.call(ToolContext::new(&run, ask.as_ref(), &call, &arguments).with_services(&services));
    tokio::pin!(pending);
    assert!(
        timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err()
    );
    {
        let requests = questions.asked.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].is_blocking(),
            matches!(role, PromptRole::Planner)
        );
        assert_eq!(requests[0].auto_resolution_ms(), None);
    }
    questions.answer.notify_one();
    assert!(
        timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .is_ok()
    );
    if let Some(control) = built.agent_control() {
        control.shutdown().await;
    }
}

#[rstest]
#[case(PromptRole::ReadOnlySpecialist, false)]
#[case(PromptRole::ReadOnlySpecialist, true)]
#[case(PromptRole::Planner, false)]
#[case(PromptRole::Planner, true)]
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn external_plugin_instructions_are_readable_through_the_inherited_shell(
    #[case] role: PromptRole,
    #[case] restricted: bool,
) {
    let workspace = tempfile::tempdir().unwrap();
    let plugin = tempfile::tempdir().unwrap();
    let skill = plugin.path().join("skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: Review changes.\n---\nEXTERNAL PLUGIN BODY",
    )
    .unwrap();
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_discovery(
            SkillDiscoveryConfig::new()
                .without_project_skills()
                .with_plugin_root(
                    PluginSkillRoot::new(plugin.path().join("skills"), plugin.path(), "acme")
                        .unwrap(),
                ),
        )
        .with_read_only_role_restrictions(restricted);
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &role,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
    let sandbox = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };
    let adapter = CodingThreadSettingsAdapter::new(host.workspace_root(), sandbox.clone()).unwrap();
    let config = adapter
        .prepare(
            &adapter.initialize(host.build_run_config()),
            &ThreadPolicyOverrides::new().with_approval_policy(AskForApproval::Never),
        )
        .unwrap();
    let app = config.app_context().unwrap().clone();
    let run = RunContext::new(RunId::generate(), built.agent()).with_app_context(app.clone());
    let capability = built
        .configure_run(config)
        .capabilities()
        .iter()
        .find(|capability| capability.kind() == ra_core::capability::CapabilityFamily::SKILLS)
        .unwrap()
        .clone();
    let instructions = capability
        .bind(&run)
        .unwrap()
        .unwrap()
        .instructions()
        .await
        .unwrap()
        .unwrap();
    assert!(instructions.content().contains("- acme:review:"));
    assert!(!instructions.content().contains("EXTERNAL PLUGIN BODY"));
    let reader = built
        .agent()
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "read_file")
        .unwrap();
    let call = CallId::new("read");
    let arguments = json!({"path":skill});
    let error = reader
        .call(ToolContext::new(&run, reader.as_ref(), &call, &arguments))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside the workspace"));
    let shell = built
        .agent()
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "exec_command")
        .unwrap();
    let arguments = json!({"cmd": format!("cat '{}'", skill.display().to_string().replace('\'', "'\\''")), "yield_time_ms":2000});
    assert!(
        !shell
            .needs_approval(&ToolContext::new(&run, shell.as_ref(), &call, &arguments))
            .await
            .unwrap()
    );
    let output = shell
        .call(ToolContext::new(&run, shell.as_ref(), &call, &arguments))
        .await
        .unwrap();
    assert!(
        output.as_text().unwrap().contains("EXTERNAL PLUGIN BODY"),
        "{output:?}"
    );
    let settings = app.downcast_ref::<CodingThreadSettings>().unwrap();
    assert_eq!(settings.sandbox_policy(), &sandbox);
    assert_eq!(settings.cwd(), host.workspace_root());
    assert_eq!(settings.approval_policy(), AskForApproval::Never);
    let denied_path = workspace.path().join("must-not-exist");
    let arguments = json!({"cmd":"printf forbidden > must-not-exist",
        "sandbox_permissions":"require_escalated", "justification":"Attempt a role override."});
    let call = CallId::new("escalation");
    let error = shell
        .call(ToolContext::new(&run, shell.as_ref(), &call, &arguments))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("approval policy is Never"),
        "{error}"
    );
    assert!(!denied_path.exists());
    host.process_manager()
        .terminate_background_terminals()
        .await;
}
