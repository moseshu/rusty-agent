//! Integration tests for CodingHost, dual-channel isolation, and coding profiles.

use std::{path::Path, sync::Arc};

use ra_coding::{CodingHost, CodingProfile};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    event::{
        AgentEvent, ExecEvent, HostEventBody, HostEventEmitter, HostEventSink,
        InMemoryHostEventSink,
        agent::AgentSpawnedEvent,
        exec::{ExecOutputEvent, ExecSessionId, ExecStartedEvent, ExecStreamKind},
    },
    item::{AgentId, CallId},
    state::{RunId, RunState},
    tool::{
        ObservationMetadata, ResourceKind, ToolContext, ToolOutput, Truncation, TruncationStage,
    },
};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn test_coding_host_lifecycle_and_emitter() {
    let temp = tempdir().expect("must create tempdir");
    let sink = Arc::new(InMemoryHostEventSink::new());

    let host = CodingHost::open(temp.path())
        .expect("must open coding host")
        .with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);

    // Workspace capability check
    let test_file = Path::new("hello.txt");
    host.workspace_fs()
        .write_file(test_file, b"Hello Coding Host!")
        .expect("must write file via host workspace capability");
    assert!(host.workspace_fs().exists(test_file));

    // ToolServices generation check
    let services = host.build_tool_services();
    assert!(services.event_sink().is_some());
    assert!(services.output_projector().is_some());
    assert!(host.build_run_config().model_input_projector().is_some());

    // HostEventEmitter bound emission check
    let run_id = RunId::new("run-coding-1");
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("coder");

    let emitter = host.emitter(agent_id.clone(), allocator);
    assert_eq!(emitter.run_id(), &run_id);
    assert_eq!(emitter.agent_id(), &agent_id);

    let seq1 = emitter
        .emit_exec(ExecEvent::Started(ExecStartedEvent::new(
            ExecSessionId::new("exec-1"),
            "cargo check",
        )))
        .expect("must emit exec started");

    let seq2 = emitter
        .emit_exec(ExecEvent::Output(ExecOutputEvent::new(
            ExecSessionId::new("exec-1"),
            ExecStreamKind::Stdout,
            0,
            24,
            "Finished dev target(s)\n",
        )))
        .expect("must emit exec output");

    let seq3 = emitter
        .emit_agent(AgentEvent::Spawned(AgentSpawnedEvent::new(AgentId::new(
            "sub-tester",
        ))))
        .expect("must emit agent spawned");

    assert_eq!(seq1, 0);
    assert_eq!(seq2, 1);
    assert_eq!(seq3, 2);

    assert_eq!(sink.len(), 3);
    let events = sink.events();
    assert_eq!(events[0].seq(), 0);
    assert_eq!(events[0].run_id(), &run_id);
    assert_eq!(events[1].seq(), 1);
    assert_eq!(events[1].run_id(), &run_id);
    assert_eq!(events[2].seq(), 2);
    assert_eq!(events[2].run_id(), &run_id);

    if let HostEventBody::Exec(ExecEvent::Started(e)) = events[0].body() {
        assert_eq!(e.command(), "cargo check");
    } else {
        panic!("expected exec started event");
    }
}

#[test]
fn test_workspace_tools_share_one_resource_boundary() {
    let workspace = tempdir().expect("must create tempdir");
    let host = CodingHost::open(workspace.path()).expect("must open coding host");
    let read_file = host.read_file_tool().expect("read_file builds");
    let grep = host.grep_tool().expect("grep builds");
    let glob = host.glob_tool().expect("glob builds");
    let apply_patch = host.apply_patch_tool().expect("apply_patch builds");
    let exec_command = host.exec_command_tool().expect("exec_command builds");

    let read_options = read_file.options();
    let grep_options = grep.options();
    let glob_options = glob.options();
    let patch_options = apply_patch.options();
    let exec_options = exec_command.options();
    let read_claims = read_options.resource_claims();
    let grep_claims = grep_options.resource_claims();
    let glob_claims = glob_options.resource_claims();
    let patch_claims = patch_options.resource_claims();
    let exec_claims = exec_options.resource_claims();
    assert_eq!(read_claims.len(), 1);
    assert_eq!(grep_claims.len(), 1);
    assert_eq!(glob_claims.len(), 1);
    assert_eq!(patch_claims.len(), 1);
    assert_eq!(exec_claims.len(), 1);

    let workspace_resource = read_claims[0].resource();
    assert_eq!(grep_claims[0].resource(), workspace_resource);
    assert_eq!(glob_claims[0].resource(), workspace_resource);
    assert_eq!(patch_claims[0].resource(), workspace_resource);
    assert_eq!(exec_claims[0].resource(), workspace_resource);
    assert!(!read_claims[0].is_exclusive());
    assert!(!grep_claims[0].is_exclusive());
    assert!(!glob_claims[0].is_exclusive());
    assert!(patch_claims[0].is_exclusive());
    assert!(exec_claims[0].is_exclusive());

    // Agreeing with each other is not enough: three tools that each derived an identity from a
    // *different* root would also agree, and would then hold three unrelated locks. The identity
    // has to name this host's canonical root.
    assert_eq!(workspace_resource.kind(), &ResourceKind::Workspace);
    assert_eq!(
        workspace_resource.value(),
        host.workspace_root().to_string_lossy()
    );
}

/// The three tools resolve against one capability, not three handles that happen to agree.
///
/// A shared `ResourceId` proves only that they name the same lock. This proves the descriptor
/// behind it is also shared: the patch entry writes, and the read entry sees the write.
#[tokio::test]
async fn test_workspace_tools_resolve_against_one_capability() {
    let workspace = tempdir().expect("must create tempdir");
    let host = CodingHost::open(workspace.path()).expect("must open coding host");
    let apply_patch = host.apply_patch_tool().expect("apply_patch builds");
    let read_file = host.read_file_tool().expect("read_file builds");

    let agent = AgentSpec::builder()
        .id(AgentId::new("coding-runner"))
        .name("Coding runner")
        .build()
        .expect("agent");
    let run = RunContext::new(RunId::new("run-workspace-boundary"), &agent);

    let patch = json!("*** Begin Patch\n*** Add File: note.txt\n+shared\n*** End Patch\n");
    let patch_call = CallId::new("call-patch");
    apply_patch
        .call(ToolContext::new(
            &run,
            apply_patch.as_ref(),
            &patch_call,
            &patch,
        ))
        .await
        .expect("patch applies");

    let read = json!({ "path": "note.txt" });
    let read_call = CallId::new("call-read");
    let output = read_file
        .call(ToolContext::new(
            &run,
            read_file.as_ref(),
            &read_call,
            &read,
        ))
        .await
        .expect("read succeeds");

    assert!(
        output
            .as_text()
            .expect("a single text block")
            .contains("shared"),
        "the read entry must see what the patch entry wrote"
    );
}

#[test]
fn test_dual_channel_strict_isolation() {
    // Channel 1: Model-visible output via ToolOutput and ObservationMetadata
    let metadata = ObservationMetadata::new()
        .with_truncation(Truncation::new(TruncationStage::Tool, 10000, 2000))
        .with_guidance("Tip: Narrow search query to reduce output size.");

    let tool_output = ToolOutput::text("fn main() {}\n").with_metadata(metadata);
    let blocks = tool_output.model_blocks();

    // Verify model channel contents: contains prose guidance, NOT raw event JSON
    assert_eq!(blocks.len(), 2);
    let guidance_block = blocks[0].as_text().expect("metadata block must be text");
    assert!(guidance_block.contains("truncated by tool"));
    assert!(guidance_block.contains("Tip: Narrow search query"));
    assert!(!guidance_block.contains("\"family\":"));
    assert!(!guidance_block.contains("\"seq\":"));

    let code_block = blocks[1].as_text().expect("output block must be text");
    assert_eq!(code_block, "fn main() {}\n");

    // Channel 2: Host-visible event via HostEventEmitter & HostEvent
    let sink = Arc::new(InMemoryHostEventSink::new());
    let run_id = RunId::new("run-channel-2");
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let emitter = HostEventEmitter::new(AgentId::new("coder"), allocator, sink.clone());

    emitter
        .emit_exec(ExecEvent::Started(
            ExecStartedEvent::new(ExecSessionId::new("sess-1"), "cargo test").with_pid(4_242),
        ))
        .expect("must emit");

    assert_eq!(sink.len(), 1);
    let host_event = &sink.events()[0];
    assert_eq!(host_event.seq(), 0);
    assert_eq!(host_event.run_id(), &run_id);

    // Assert that the serialized HostEvent is never present in the tool output model blocks
    let host_json = serde_json::to_string(host_event).expect("must serialize");
    assert!(!guidance_block.contains(&host_json));
    assert!(!code_block.contains(&host_json));
}

#[test]
fn test_coding_profiles() {
    assert_eq!(CodingProfile::default(), CodingProfile::CodexLike);

    let profiles = [
        CodingProfile::Core,
        CodingProfile::CodexLike,
        CodingProfile::Full,
    ];
    for p in profiles {
        let json_str = serde_json::to_string(&p).expect("must serialize profile");
        let restored: CodingProfile =
            serde_json::from_str(&json_str).expect("must deserialize profile");
        assert_eq!(p, restored);
    }
}
