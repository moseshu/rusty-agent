// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The patch tool's verification, approval preparation and begin/end events, as Codex's
//! `ApplyPatchHandler` verifies a patch, announces it with `emitter.begin`, and ends it with
//! `emitter.finish`. The verification cases are Codex's `core/tests/suite/apply_patch_cli.rs`
//! cases, run against the tool rather than through a model.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::{Error, ToolErrorKind},
    event::{
        FileEvent, HostEventBody, HostEventSink, InMemoryHostEventSink,
        file::{FileChangeKind, PatchApplyStatus},
    },
    item::{AgentId, CallId},
    state::{RunId, RunState},
    tool::{Tool, ToolApprovalPolicy, ToolContext, ToolServices},
};
use ra_exec::fs::RootedFileSystem;
use ra_tools::{apply_patch::ApplyPatchTool, exec_command::ApprovalResolver};
use serde_json::{Value, json};
use tempfile::TempDir;

/// One announced change: path, kind, move destination, and diff.
type Announced = (String, FileChangeKind, Option<String>, String);

/// What the host channel saw of one patch: its begin's changes and its ends.
#[derive(Debug, Default, PartialEq, Eq)]
struct Boundaries {
    begins: Vec<Vec<Announced>>,
    ends: Vec<(PatchApplyStatus, String)>,
}

struct Harness {
    workspace: TempDir,
    tool: ApplyPatchTool,
    sink: Arc<InMemoryHostEventSink>,
    services: ToolServices,
    run: RunContext,
    call_id: CallId,
}

impl Harness {
    fn new(resolver: Option<Arc<ApprovalResolver>>) -> Self {
        let workspace = TempDir::new().expect("workspace");
        let filesystem = RootedFileSystem::open(workspace.path()).expect("workspace capability");
        let mut tool = ApplyPatchTool::new(Arc::new(filesystem)).expect("tool");
        if let Some(resolver) = resolver {
            tool = tool.with_approval_resolver(resolver);
        }
        let sink = Arc::new(InMemoryHostEventSink::new());
        let services =
            ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
        let agent = AgentSpec::builder()
            .id(AgentId::new("patch-runner"))
            .name("Patch runner")
            .build()
            .expect("agent");
        let state = RunState::start(RunId::new("run-patch-approval"));
        let run = RunContext::new(RunId::new("run-patch-approval"), &agent)
            .with_event_seq_allocator(state.restore_event_seq_allocator(None));
        Self {
            workspace,
            tool,
            sink,
            services,
            run,
            call_id: CallId::new("call-patch"),
        }
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.workspace.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, content).expect("write");
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.path().join(path)).ok()
    }

    fn context<'a>(&'a self, arguments: &'a Value, state: Option<&'a Value>) -> ToolContext<'a> {
        let context = ToolContext::new(&self.run, &self.tool, &self.call_id, arguments)
            .with_services(&self.services);
        match state {
            Some(state) => context.with_resume_state(state),
            None => context,
        }
    }

    /// Runs the call, rendering a failure as the runtime's custom failure handling does.
    async fn call(&self, patch: &str, state: Option<&Value>) -> Result<String, (Error, String)> {
        let arguments = json!(patch);
        match self.tool.call(self.context(&arguments, state)).await {
            Ok(output) => Ok(output.as_text().expect("text").to_owned()),
            Err(error) => {
                let text = self.render(&arguments, &error).await;
                Err((error, text))
            }
        }
    }

    async fn approval_state(&self, patch: &str) -> Result<Option<Value>, (Error, String)> {
        let arguments = json!(patch);
        match self
            .tool
            .approval_state(&self.context(&arguments, None))
            .await
        {
            Ok(state) => Ok(state),
            Err(error) => {
                let text = self.render(&arguments, &error).await;
                Err((error, text))
            }
        }
    }

    async fn needs_approval(&self, patch: &str) -> bool {
        let arguments = json!(patch);
        self.tool
            .needs_approval(&self.context(&arguments, None))
            .await
            .expect("needs_approval")
    }

    async fn render(&self, arguments: &Value, error: &Error) -> String {
        self.tool
            .handle_failure(&self.context(arguments, None), error)
            .await
            .expect("failure handling")
            .unwrap_or_else(|| panic!("the tool renders its own failure: {error}"))
            .as_text()
            .expect("text")
            .to_owned()
    }

    fn boundaries(&self) -> Boundaries {
        let mut boundaries = Boundaries::default();
        for event in self.sink.events() {
            match event.body() {
                HostEventBody::File(FileEvent::PatchApplyBegin(begin)) => {
                    assert_eq!(begin.call_id(), &self.call_id);
                    boundaries.begins.push(
                        begin
                            .changes()
                            .iter()
                            .map(|change| {
                                (
                                    change.path().to_owned(),
                                    change.change().clone(),
                                    change.moved_to().map(str::to_owned),
                                    change.diff().to_owned(),
                                )
                            })
                            .collect(),
                    );
                }
                HostEventBody::File(FileEvent::PatchApplyEnd(end)) => {
                    assert_eq!(end.call_id(), &self.call_id);
                    boundaries
                        .ends
                        .push((end.status(), end.output().to_owned()));
                }
                _ => {}
            }
        }
        boundaries
    }

    fn changed_files(&self) -> usize {
        self.sink
            .events()
            .iter()
            .filter(|event| matches!(event.body(), HostEventBody::File(FileEvent::Changed(_))))
            .count()
    }
}

fn asks(answer: bool) -> Arc<ApprovalResolver> {
    Arc::new(move |_context: &ToolContext<'_>| Ok(answer))
}

fn refuses(reason: &'static str) -> Arc<ApprovalResolver> {
    Arc::new(move |_context: &ToolContext<'_>| Err(ra_tools::exec_command::rejection(reason)))
}

fn kind(error: &Error) -> Option<ToolErrorKind> {
    match error {
        Error::Tool { kind, .. } => Some(*kind),
        _ => None,
    }
}

fn approved() -> Value {
    json!({ "apply_patch": { "item_started": true } })
}

const ADD: &str = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n";
const UPDATE: &str =
    "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** End Patch\n";
const MISSING: &str =
    "*** Begin Patch\n*** Update File: missing.txt\n@@\n-nope\n+better\n*** End Patch";

// --- Verification (core/tests/suite/apply_patch_cli.rs) -------------------------------------

/// Codex's verification cases: each fails with `apply_patch verification failed`, from the call
/// and from the approval it would ask for alike, writes nothing, and announces nothing.
#[tokio::test]
async fn codex_verification_failures_write_and_announce_nothing() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "rejects_invalid_hunk_header",
            "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch",
            "",
        ),
        (
            "reports_missing_context",
            "*** Begin Patch\n*** Update File: modify.txt\n@@\n-missing\n+changed\n*** End Patch",
            "modify.txt",
        ),
        ("reports_missing_target_file", MISSING, "missing.txt"),
        (
            "delete_missing_file_reports_error",
            "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch",
            "missing.txt",
        ),
        (
            "delete_directory_reports_verification_error",
            "*** Begin Patch\n*** Delete File: dir\n*** End Patch",
            "dir",
        ),
        (
            "rejects_duplicate_resolved_paths",
            "*** Begin Patch\n*** Update File: duplicate.txt\n@@\n-before\n+first after\n*** Update File: ./duplicate.txt\n@@\n-before\n+second after\n*** End Patch",
            "multiple operations target",
        ),
        (
            "missing_second_chunk_context_rejected",
            "*** Begin Patch\n*** Update File: two_chunks.txt\n@@\n-b\n+B\n\n-d\n+D\n*** End Patch",
            "",
        ),
    ];
    for (name, patch, mentions) in cases {
        let harness = Harness::new(Some(asks(true)));
        harness.write("modify.txt", "line1\nline2\n");
        harness.write("duplicate.txt", "before\n");
        harness.write("two_chunks.txt", "a\nb\nc\nd\n");
        std::fs::create_dir(harness.workspace.path().join("dir")).expect("dir");

        let (error, text) = harness.call(patch, None).await.expect_err(name);
        assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput), "{name}");
        assert!(
            text.starts_with("apply_patch verification failed: "),
            "{name}: {text}"
        );
        assert!(text.contains(mentions), "{name}: {text}");

        let (error, asked) = harness.approval_state(patch).await.expect_err(name);
        assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput), "{name}");
        assert_eq!(asked, text, "{name}: the call and the question fail alike");

        assert_eq!(
            harness.read("modify.txt").as_deref(),
            Some("line1\nline2\n")
        );
        assert_eq!(harness.read("duplicate.txt").as_deref(), Some("before\n"));
        assert_eq!(
            harness.read("two_chunks.txt").as_deref(),
            Some("a\nb\nc\nd\n")
        );
        assert_eq!(harness.read("missing.txt"), None, "{name}");
        assert!(harness.workspace.path().join("dir").is_dir(), "{name}");
        assert_eq!(harness.boundaries(), Boundaries::default(), "{name}");
        assert_eq!(harness.changed_files(), 0, "{name}");
    }
}

/// A patch that cannot be parsed fails at decoding, before the permission stage or any hook; one
/// that parses decodes to the parsed JSON, which the tool reads itself.
#[tokio::test]
async fn decoding_parses_the_patch_and_reads_nothing() {
    let harness = Harness::new(None);
    let error = harness
        .tool
        .decode_input(&json!(
            "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch"
        ))
        .expect_err("unparsable");
    assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput));
    assert!(
        harness
            .render(&json!(""), &error)
            .await
            .starts_with("apply_patch verification failed: ")
    );
    // A patch naming a missing file still decodes: decoding does not read the workspace.
    assert!(
        harness
            .tool
            .decode_input(&json!(MISSING))
            .expect("parsable")
            .is_none()
    );
    assert!(
        harness
            .tool
            .decode_input(&json!({ "patch": ADD }))
            .expect("object form")
            .is_none()
    );
}

// --- Without a host policy ------------------------------------------------------------------

/// Without a policy the tool always asks, and the question is still prepared after verification:
/// a patch that does not verify is never asked about.
#[tokio::test]
async fn without_a_policy_the_question_is_prepared_after_verification() {
    let harness = Harness::new(None);
    assert_eq!(
        harness.tool.options().approval(),
        ToolApprovalPolicy::Always
    );

    let (error, _) = harness.approval_state(MISSING).await.expect_err("missing");
    assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput));
    assert_eq!(harness.boundaries(), Boundaries::default());

    assert_eq!(
        harness.approval_state(ADD).await.expect("state"),
        Some(approved())
    );
    assert_eq!(harness.boundaries().begins.len(), 1);
}

// --- Preparing the question -----------------------------------------------------------------

/// The question is prepared by verifying the patch and announcing it, before anything is written.
#[tokio::test]
async fn the_question_announces_the_verified_patch_and_writes_nothing() {
    let harness = Harness::new(Some(asks(true)));
    harness.write("source.txt", "old\n");
    harness.write("before.txt", "kept\n");
    harness.write("gone.txt", "bye\n");
    let patch = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** Update File: source.txt\n@@\n-old\n+new\n*** Update File: before.txt\n*** Move to: after.txt\n@@\n-kept\n+moved\n*** Delete File: gone.txt\n*** End Patch\n";

    assert_eq!(
        harness.approval_state(patch).await.expect("state"),
        Some(approved())
    );

    assert_eq!(
        harness.boundaries(),
        Boundaries {
            begins: vec![vec![
                (
                    "added.txt".to_owned(),
                    FileChangeKind::Added,
                    None,
                    "created\n".to_owned()
                ),
                (
                    "source.txt".to_owned(),
                    FileChangeKind::Updated,
                    None,
                    "@@\n-old\n+new\n".to_owned()
                ),
                (
                    "before.txt".to_owned(),
                    FileChangeKind::Moved,
                    Some("after.txt".to_owned()),
                    "@@\n-kept\n+moved\n".to_owned()
                ),
                (
                    "gone.txt".to_owned(),
                    FileChangeKind::Deleted,
                    None,
                    String::new()
                ),
            ]],
            ends: Vec::new(),
        }
    );
    assert_eq!(harness.read("added.txt"), None);
    assert_eq!(harness.read("source.txt").as_deref(), Some("old\n"));
    assert_eq!(harness.read("before.txt").as_deref(), Some("kept\n"));
    assert_eq!(harness.read("gone.txt").as_deref(), Some("bye\n"));
    assert_eq!(harness.changed_files(), 0);
}

/// A patch the policy refuses is refused when the question is prepared, unasked, with the
/// policy's reason; one that is also broken is reported as broken, as Codex verifies first.
#[tokio::test]
async fn a_refused_patch_is_not_asked_about_and_verification_comes_first() {
    let harness = Harness::new(Some(refuses("patch rejected: not here")));
    harness.write("source.txt", "old\n");

    let (error, text) = harness.approval_state(UPDATE).await.expect_err("refused");
    assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput));
    assert_eq!(text, "patch rejected: not here");
    let (_, called) = harness.call(UPDATE, None).await.expect_err("refused");
    assert_eq!(called, text, "the question and the call refuse alike");

    let (_, broken) = harness.approval_state(MISSING).await.expect_err("broken");
    assert!(
        broken.starts_with("apply_patch verification failed: "),
        "{broken}"
    );
    let (_, broken) = harness.call(MISSING, None).await.expect_err("broken");
    assert!(
        broken.starts_with("apply_patch verification failed: "),
        "{broken}"
    );

    assert_eq!(harness.read("source.txt").as_deref(), Some("old\n"));
    assert_eq!(harness.boundaries(), Boundaries::default());
}

/// Whether to ask is the policy's answer, and a refusal is not a reason to ask: the call refuses
/// it once the patch has been verified.
#[tokio::test]
async fn needs_approval_answers_the_policy_and_leaves_a_refusal_to_the_call() {
    assert!(Harness::new(Some(asks(true))).needs_approval(ADD).await);
    assert!(!Harness::new(Some(asks(false))).needs_approval(ADD).await);

    let harness = Harness::new(Some(refuses("patch rejected: no")));
    assert!(!harness.needs_approval(ADD).await);
    let (error, text) = harness.call(ADD, None).await.expect_err("refused");
    assert_eq!(kind(&error), Some(ToolErrorKind::PermissionDenied));
    assert_eq!(text, "patch rejected: no");
    assert_eq!(harness.read("added.txt"), None);
    assert_eq!(harness.boundaries(), Boundaries::default());
}

/// A policy that fails rather than refuses is not a refusal: it is the host's error.
#[tokio::test]
async fn a_failing_policy_is_not_read_as_a_refusal() {
    let harness = Harness::new(Some(Arc::new(|_context: &ToolContext<'_>| {
        Err(Error::caller("the policy is broken"))
    })));
    let arguments = json!(ADD);
    let error = harness
        .tool
        .needs_approval(&harness.context(&arguments, None))
        .await
        .expect_err("the policy's error");
    assert!(matches!(error, Error::Caller { .. }), "{error}");
    let error = harness
        .tool
        .call(harness.context(&arguments, None))
        .await
        .expect_err("the policy's error");
    assert!(matches!(error, Error::Caller { .. }), "{error}");
    assert_eq!(harness.read("added.txt"), None);
}

// --- The call's two paths -------------------------------------------------------------------

/// A call that was not asked about verifies, announces and applies itself — even when its policy
/// would ask, as under a fixed allow rule — and never asks from inside.
#[tokio::test]
async fn a_call_without_state_announces_itself_and_applies() {
    let harness = Harness::new(Some(asks(true)));
    harness.write("source.txt", "old\n");

    let text = harness.call(UPDATE, None).await.expect("applied");

    assert_eq!(harness.read("source.txt").as_deref(), Some("new\n"));
    let boundaries = harness.boundaries();
    assert_eq!(boundaries.begins.len(), 1);
    assert_eq!(boundaries.ends, vec![(PatchApplyStatus::Completed, text)]);
}

/// The approved call runs as asked: the begin was announced with the question, so it is not
/// announced again.
#[tokio::test]
async fn an_approved_call_applies_without_announcing_again() {
    let harness = Harness::new(Some(asks(true)));
    harness.write("source.txt", "old\n");

    harness.approval_state(UPDATE).await.expect("asked");
    let text = harness
        .call(UPDATE, Some(&approved()))
        .await
        .expect("applied");

    assert_eq!(harness.read("source.txt").as_deref(), Some("new\n"));
    let boundaries = harness.boundaries();
    assert_eq!(
        boundaries.begins.len(),
        1,
        "announced once, with the question"
    );
    assert_eq!(boundaries.ends, vec![(PatchApplyStatus::Completed, text)]);
}

/// The workspace can change while the question waits: the approved call verifies again, and
/// ends the item it announced as failed.
#[tokio::test]
async fn an_approved_call_that_no_longer_verifies_ends_failed() {
    let harness = Harness::new(Some(asks(true)));
    harness.write("source.txt", "old\n");
    harness.approval_state(UPDATE).await.expect("asked");
    harness.write("source.txt", "changed meanwhile\n");

    let (error, text) = harness
        .call(UPDATE, Some(&approved()))
        .await
        .expect_err("no longer verifies");

    assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput));
    assert!(
        text.starts_with("apply_patch verification failed: "),
        "{text}"
    );
    assert_eq!(
        harness.read("source.txt").as_deref(),
        Some("changed meanwhile\n")
    );
    assert_eq!(
        harness.boundaries().ends,
        vec![(PatchApplyStatus::Failed, text)]
    );
}

/// The policy can change while the question waits: the approved call is refused, and its item
/// ends declined, as Codex reports a rejected patch.
#[tokio::test]
async fn an_approved_call_the_policy_now_refuses_ends_declined() {
    let asked = Harness::new(Some(asks(true)));
    asked.write("source.txt", "old\n");
    asked.approval_state(UPDATE).await.expect("asked");

    let harness = Harness::new(Some(refuses("patch rejected: revoked")));
    harness.write("source.txt", "old\n");
    let (error, text) = harness
        .call(UPDATE, Some(&approved()))
        .await
        .expect_err("refused");

    assert_eq!(kind(&error), Some(ToolErrorKind::PermissionDenied));
    assert_eq!(text, "patch rejected: revoked");
    assert_eq!(harness.read("source.txt").as_deref(), Some("old\n"));
    assert_eq!(
        harness.boundaries(),
        Boundaries {
            begins: Vec::new(),
            ends: vec![(PatchApplyStatus::Declined, text)],
        }
    );
}

/// An approval this tool did not ask for is not an approval of this patch.
#[tokio::test]
async fn a_state_the_tool_did_not_keep_is_refused() {
    for state in [
        json!({ "apply_patch": { "item_started": false } }),
        json!({ "apply_patch": "approved" }),
        json!({ "command": "ls" }),
    ] {
        let harness = Harness::new(Some(asks(true)));
        let (error, _) = harness
            .call(ADD, Some(&state))
            .await
            .expect_err("unknown state");
        assert_eq!(kind(&error), Some(ToolErrorKind::InvalidInput), "{state}");
        assert_eq!(harness.read("added.txt"), None, "{state}");
        assert_eq!(harness.boundaries(), Boundaries::default(), "{state}");
    }
}

// --- How an applied patch ends --------------------------------------------------------------

/// A patch that verified and stopped part way ends failed with the text the model reads, once;
/// what it wrote first stays written.
#[tokio::test]
async fn a_patch_that_stops_part_way_ends_failed_once() {
    let harness = Harness::new(None);
    harness.write("blocker", "file\n");
    let patch = "*** Begin Patch\n*** Add File: ok.txt\n+ok\n*** Add File: blocker/new.txt\n+blocked\n*** End Patch\n";

    let (error, text) = harness.call(patch, None).await.expect_err("stopped");

    assert_eq!(kind(&error), Some(ToolErrorKind::ExecutionFailed));
    assert!(
        text.starts_with("Patch stopped after changing 1 path(s)"),
        "{text}"
    );
    assert_eq!(harness.read("ok.txt").as_deref(), Some("ok\n"));
    let boundaries = harness.boundaries();
    assert_eq!(boundaries.begins.len(), 1);
    assert_eq!(boundaries.ends, vec![(PatchApplyStatus::Failed, text)]);
    assert_eq!(harness.changed_files(), 1);
}

/// A patch that applies ends completed once, with the text the model reads.
#[tokio::test]
async fn an_applied_patch_ends_completed_once() {
    let harness = Harness::new(None);
    let text = harness.call(ADD, None).await.expect("applied");
    assert_eq!(harness.read("added.txt").as_deref(), Some("created\n"));
    assert_eq!(
        harness.boundaries().ends,
        vec![(PatchApplyStatus::Completed, text)]
    );
}
