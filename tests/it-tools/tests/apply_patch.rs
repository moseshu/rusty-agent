//! The V4A patch tool's own contract, independent of any product that installs it.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    event::{FileEvent, HostEventBody, HostEventSink, InMemoryHostEventSink, file::FileChangeKind},
    item::{AgentId, CallId},
    state::{RunId, RunState},
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolServices},
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

/// The text the model reads for a call: its result, or the failure the tool renders.
async fn call(tool: &ApplyPatchTool, call_id: &str, arguments: &serde_json::Value) -> String {
    let run = run();
    let call_id = CallId::new(call_id);
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    let output = match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure handling")
            .unwrap_or_else(|| panic!("the tool renders its own failure: {error}")),
    };
    output.as_text().expect("text").to_owned()
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

/// A later operation that cannot apply to the workspace as it is fails the whole patch at
/// verification, before the earlier ones are written, as Codex's handler verifies every operation
/// first.
#[tokio::test]
async fn a_later_delete_of_a_missing_file_fails_verification_before_any_write() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let tool = tool(&workspace);

    let arguments = json!({ "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Delete File: missing.txt\n*** End Patch\n" });
    let output = call(&tool, "call-patch-partial", &arguments).await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("source.txt")).expect("unchanged"),
        "old\n"
    );
    assert!(
        output.starts_with("apply_patch verification failed: Failed to read missing.txt"),
        "{output}"
    );
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

/// Every committed change leaves a fact on the host channel, including the ones before a failure.
///
/// Application is best effort, so a record written once at the end would describe a run that wrote
/// two files and then stopped as a run that changed nothing — while those two files sit changed on
/// disk. The partial half of this test is the point of it.
#[tokio::test]
async fn records_each_committed_change_including_a_partial_application() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source");
    let tool = tool(&workspace);
    let sink = Arc::new(InMemoryHostEventSink::new());
    let services = ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
    let agent = AgentSpec::builder()
        .id(AgentId::new("patch-runner"))
        .name("Patch runner")
        .build()
        .expect("agent");
    let state = RunState::start(RunId::new("run-apply-patch"));
    let run = RunContext::new(RunId::new("run-apply-patch"), &agent)
        .with_event_seq_allocator(state.restore_event_seq_allocator(None));
    let call_id = CallId::new("call-patch-1");

    // The third action verifies but cannot be written: its parent is a regular file. The first two
    // already have been.
    std::fs::write(workspace.path().join("blocker"), "file\n").expect("blocker");
    let arguments = json!({
        "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** Add File: added.txt\n+created\n*** Add File: blocker/new.txt\n+blocked\n*** End Patch\n"
    });
    let context = || ToolContext::new(&run, &tool, &call_id, &arguments).with_services(&services);
    let error = tool
        .call(context())
        .await
        .expect_err("a patch that stopped part way is a failure");
    let text = tool
        .handle_failure(&context(), &error)
        .await
        .expect("failure handling")
        .expect("rendered")
        .as_text()
        .expect("text")
        .to_owned();
    assert!(
        text.contains("Patch stopped after changing 2 path"),
        "the model is told what was committed before the stop: {text}"
    );

    let events = sink.events();
    let changes: Vec<_> = events
        .iter()
        .filter_map(|event| match event.body() {
            HostEventBody::File(FileEvent::Changed(change)) => Some(change),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes.len(),
        2,
        "the two committed actions must be on the record, and the refused one must not: {events:?}"
    );

    assert_eq!(changes[0].call_id(), &call_id);
    assert_eq!(changes[0].path(), "source.txt");
    assert_eq!(changes[0].change(), &FileChangeKind::Updated);
    assert_eq!(changes[0].lines_added(), 1);
    assert_eq!(changes[0].lines_removed(), 1);

    assert_eq!(changes[1].path(), "added.txt");
    assert_eq!(changes[1].change(), &FileChangeKind::Added);
    assert_eq!(changes[1].lines_added(), 1);
    assert_eq!(changes[1].lines_removed(), 0);
}

/// A move names both ends, because a record with only one of them cannot be followed.
#[tokio::test]
async fn records_a_move_with_both_of_its_paths() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("before.txt"), "kept\n").expect("source");
    let tool = tool(&workspace);
    let sink = Arc::new(InMemoryHostEventSink::new());
    let services = ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
    let agent = AgentSpec::builder()
        .id(AgentId::new("patch-runner"))
        .name("Patch runner")
        .build()
        .expect("agent");
    let state = RunState::start(RunId::new("run-apply-patch"));
    let run = RunContext::new(RunId::new("run-apply-patch"), &agent)
        .with_event_seq_allocator(state.restore_event_seq_allocator(None));
    let call_id = CallId::new("call-patch-move");

    let arguments = json!({
        "patch": "*** Begin Patch\n*** Update File: before.txt\n*** Move to: after.txt\n@@\n-kept\n+moved\n*** End Patch\n"
    });
    tool.call(ToolContext::new(&run, &tool, &call_id, &arguments).with_services(&services))
        .await
        .expect("tool result");

    let events = sink.events();
    let moved = events
        .iter()
        .find_map(|event| match event.body() {
            HostEventBody::File(FileEvent::Changed(change))
                if change.change() == &FileChangeKind::Moved =>
            {
                Some(change)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("a move must be recorded as one: {events:?}"));
    assert_eq!(moved.path(), "before.txt");
    assert_eq!(moved.moved_to(), Some("after.txt"));
}
