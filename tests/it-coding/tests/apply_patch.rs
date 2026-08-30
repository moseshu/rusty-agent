//! The coding product's installation of the editing entry.
//!
//! The tool's own behaviour is `it-tools`' subject. What is product content, and therefore tested
//! here, is that this product installs it at all, hands it a workspace-confined capability, and
//! advertises it under an approval policy.

use ra_coding::{CodingHost, build_agent_with_host};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{ToolApprovalPolicy, ToolContext},
};
use serde_json::json;
use tempfile::TempDir;

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

    // Found by name rather than by position: this case is about the editing entry, and an index
    // would make it fail the next time the host installs another tool beside it.
    let editing = agent
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "apply_patch")
        .expect("the host-backed agent advertises the editing entry");

    assert_eq!(editing.options().approval(), ToolApprovalPolicy::Always);
}

/// The capability the host hands the tool is the workspace it was opened on, not the process root.
#[tokio::test]
async fn the_host_confines_the_editing_entry_to_its_workspace() {
    let workspace = TempDir::new().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("host");
    let tool = host.apply_patch_tool().expect("tool");
    tool.validate().expect("valid tool contract");

    let agent = AgentSpec::builder()
        .id(AgentId::new("coding-runner"))
        .name("Coding runner")
        .build()
        .expect("agent");
    let run = RunContext::new(RunId::new("run-apply-patch"), &agent);
    let arguments = json!("*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n");
    let call_id = CallId::new("call-host-patch");
    tool.call(ToolContext::new(&run, tool.as_ref(), &call_id, &arguments))
        .await
        .expect("tool result");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "created\n"
    );
}
