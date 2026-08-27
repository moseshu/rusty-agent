//! The V4A patch tool's own contract, independent of any product that installs it.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext},
};
use ra_exec::fs::RootedFileSystem;
use ra_tools::apply_patch::ApplyPatchTool;
use serde_json::json;
use tempfile::TempDir;

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("patch-runner"))
        .name("Patch runner")
        .build()
        .expect("agent");
    RunContext::new(RunId::new("run-apply-patch"), &agent)
}

/// The tool takes a capability, not a path: whoever confines the filesystem decides the reach.
fn tool(workspace: &TempDir) -> ApplyPatchTool {
    let filesystem = RootedFileSystem::open(workspace.path()).expect("workspace capability");
    ApplyPatchTool::new(Arc::new(filesystem)).expect("tool")
}

async fn call(tool: &ApplyPatchTool, call_id: &str, arguments: &serde_json::Value) -> String {
    let run = run();
    let call_id = CallId::new(call_id);
    tool.call(ToolContext::new(&run, tool, &call_id, arguments))
        .await
        .expect("tool result")
        .as_text()
        .expect("text")
        .to_owned()
}

#[tokio::test]
async fn applies_multiple_actions_and_reports_the_committed_delta() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let tool = tool(&workspace);
    tool.validate().expect("valid tool contract");

    let arguments = json!({ "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Add File: added.txt\n+created\n*** End Patch\n" });
    let output = call(&tool, "call-patch", &arguments).await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("source.txt")).expect("updated"),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "created\n"
    );
    assert!(output.contains("2 path(s)"));
}

#[tokio::test]
async fn keeps_already_committed_changes_when_a_later_action_fails() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let tool = tool(&workspace);

    let arguments = json!({ "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Delete File: missing.txt\n*** End Patch\n" });
    let output = call(&tool, "call-patch-partial", &arguments).await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("source.txt")).expect("updated"),
        "new\n"
    );
    assert!(output.contains("stopped after changing 1 path(s)"));
}

#[tokio::test]
async fn accepts_a_bare_patch_string_for_the_custom_tool_wire() {
    let workspace = TempDir::new().expect("workspace");
    let tool = tool(&workspace);

    let arguments = json!("*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n");
    call(&tool, "call-custom-patch", &arguments).await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "created\n"
    );
}

/// The capability bounds the tool; a patch cannot name its way out of the root it was handed.
#[tokio::test]
async fn a_patch_cannot_write_outside_the_capability_root() {
    let workspace = TempDir::new().expect("workspace");
    let tool = tool(&workspace);

    let arguments =
        json!("*** Begin Patch\n*** Add File: ../escaped.txt\n+created\n*** End Patch\n");
    let output = call(&tool, "call-escape", &arguments).await;

    assert!(
        !workspace
            .path()
            .parent()
            .expect("temporary parent")
            .join("escaped.txt")
            .exists(),
        "{output}"
    );
}

#[tokio::test]
async fn the_default_options_require_approval_and_exclude_concurrency() {
    let workspace = TempDir::new().expect("workspace");
    let tool = tool(&workspace);

    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Always);
    // Writing files is not something to run alongside another write of the same tree.
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(tool.origin().name(), "apply_patch");
}

#[tokio::test]
async fn description_discloses_that_a_failed_patch_can_be_partially_applied() {
    let workspace = TempDir::new().expect("workspace");
    let tool = tool(&workspace);

    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Applies a V4A patch to workspace files. A later failure can leave earlier actions applied.\nSupply the patch verbatim, from `*** Begin Patch` through `*** End Patch`."
        )
    );
}
