use ra_coding::{CodingHost, build_agent_with_host};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::ToolContext,
};
use serde_json::json;
use tempfile::TempDir;

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coding-runner"))
        .name("Coding runner")
        .build()
        .expect("agent");
    RunContext::new(RunId::new("run-apply-patch"), &agent)
}

#[tokio::test]
async fn applies_multiple_actions_and_reports_the_committed_delta() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let host = CodingHost::open(workspace.path()).expect("host");
    let tool = host.apply_patch_tool().expect("tool");
    tool.validate().expect("valid tool contract");

    let arguments = json!({ "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Add File: added.txt\n+created\n*** End Patch\n" });
    let call_id = CallId::new("call-patch");
    let run = run();
    let output = tool
        .call(ToolContext::new(&run, tool.as_ref(), &call_id, &arguments))
        .await
        .expect("tool result");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("source.txt")).expect("updated"),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "created\n"
    );
    assert!(output.as_text().expect("text").contains("2 path(s)"));
}

#[tokio::test]
async fn keeps_already_committed_changes_when_a_later_action_fails() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let host = CodingHost::open(workspace.path()).expect("host");
    let tool = host.apply_patch_tool().expect("tool");
    let arguments = json!({ "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Delete File: missing.txt\n*** End Patch\n" });
    let call_id = CallId::new("call-patch-partial");
    let run = run();
    let output = tool
        .call(ToolContext::new(&run, tool.as_ref(), &call_id, &arguments))
        .await
        .expect("tool result");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("source.txt")).expect("updated"),
        "new\n"
    );
    assert!(
        output
            .as_text()
            .expect("text")
            .contains("stopped after changing 1 path(s)")
    );
}

#[tokio::test]
async fn accepts_a_bare_patch_string_for_the_custom_tool_wire() {
    let workspace = TempDir::new().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("host");
    let tool = host.apply_patch_tool().expect("tool");
    let arguments = json!("*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n");
    let call_id = CallId::new("call-custom-patch");
    let run = run();
    tool.call(ToolContext::new(&run, tool.as_ref(), &call_id, &arguments))
        .await
        .expect("tool result");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "created\n"
    );
}

#[test]
fn host_backed_agent_advertises_the_editing_entry() {
    let workspace = TempDir::new().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("host");
    let agent = build_agent_with_host(
        AgentId::new("coding-agent"),
        "Coding agent",
        &ra_core::prompt::PromptRole::Main,
        &host,
    )
    .expect("agent");
    assert_eq!(agent.tools().len(), 1);
    assert_eq!(agent.tools()[0].origin().name(), "apply_patch");
}

#[test]
fn description_discloses_that_a_failed_patch_can_be_partially_applied() {
    let workspace = TempDir::new().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("host");
    let tool = host.apply_patch_tool().expect("tool");

    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Applies a V4A patch to workspace files. A later failure can leave earlier actions applied.\nSupply the patch verbatim, from `*** Begin Patch` through `*** End Patch`."
        )
    );
}
