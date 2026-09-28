//! A sandbox agent run end to end against the real local backend: the manifest is materialized,
//! the agent's capability works in the workspace through the session it was bound to, the run
//! deletes what it created, and a continued run gets the workspace back from its snapshot.
//!
//! The reference's `test_runner_restores_sandbox_from_run_state` and
//! `test_runner_persists_workspace_and_tool_choice_state_across_sandbox_resume`, on this backend.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::{Capability, CapabilityFamily, SandboxBinding},
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    sandbox::{Entry, Manifest, SandboxAgentConfig, SandboxClient, SnapshotSpec},
    state::{RunId, RunState},
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunRequest, Runner},
    sandbox::SandboxRunConfig,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use serde_json::json;

struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    tool_outputs: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            tool_outputs: Mutex::default(),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        for item in request.input() {
            if let ModelInputItem::ToolCallOutput(output) = item {
                self.tool_outputs
                    .lock()
                    .unwrap()
                    .push(output.output().to_string());
            }
        }
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

struct FixedResolver(Arc<ScriptedModel>);

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn call(id: &str, name: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, json!({}))),
    )])
}

fn answer(id: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}

/// Gives the agent two tools that work through the session it is bound to.
struct Workspace {
    binding: Option<SandboxBinding>,
    roots: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Capability for Workspace {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("workspace").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let Some(binding) = &self.binding else {
            return Vec::new();
        };
        vec![
            Arc::new(SessionTool::new("write_note", binding.clone())),
            Arc::new(SessionTool::new("read_note", binding.clone())),
        ]
    }

    fn bind_sandbox(&self, binding: &SandboxBinding) -> Result<Option<Arc<dyn Capability>>> {
        self.roots
            .lock()
            .unwrap()
            .push(binding.manifest().root.clone());
        Ok(Some(Arc::new(Self {
            binding: Some(binding.clone()),
            roots: Arc::clone(&self.roots),
        })))
    }
}

struct SessionTool {
    binding: SandboxBinding,
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl SessionTool {
    fn new(name: &str, binding: SandboxBinding) -> Self {
        Self {
            binding,
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
        }
    }
}

#[async_trait]
impl Tool for SessionTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        let session = self.binding.session();
        let outcome = if self.origin.name() == "write_note" {
            session
                .write(
                    "notes/note.txt".into(),
                    b"written in the first run".to_vec(),
                    None,
                )
                .await
                .map(|()| "written".to_owned())
        } else {
            session
                .read("notes/note.txt".into(), None)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        Ok(ToolOutput::text(
            outcome.unwrap_or_else(|error| format!("error: {error}")),
        ))
    }
}

fn agent(roots: &Arc<Mutex<Vec<String>>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("work in the workspace")
        .sandbox(
            SandboxAgentConfig::empty()
                .with_default_manifest(
                    Manifest::new().with_entry("README.md", Entry::file(b"hello".to_vec())),
                )
                .with_capability(Arc::new(Workspace {
                    binding: None,
                    roots: Arc::clone(roots),
                })),
        )
        .build()
        .unwrap()
}

fn sandbox(snapshots: &tempfile::TempDir) -> SandboxRunConfig {
    SandboxRunConfig::new()
        .with_client(Arc::new(UnixLocalSandboxClient::new()) as Arc<dyn SandboxClient>)
        .with_snapshot_spec(SnapshotSpec::Local {
            base_path: snapshots.path().to_path_buf(),
        })
}

/// Set in the child the default-snapshot scenario runs in, so that running it any other way says
/// so instead of writing into the home directory of the account running the tests.
const CHILD_MARKER: &str = "IT_RUNTIME_DEFAULT_SNAPSHOT_CHILD";

/// The reference's default with nothing configured: a run that names no snapshot still gets its
/// workspace back when it is continued, because the local client defaults to a directory in the
/// account's per-user state directory.
///
/// Runs the scenario in a child copy of this binary whose home is a temporary directory, so the
/// default directory is made there; then checks the snapshot landed in it.
#[test]
fn a_continued_run_finds_its_workspace_without_naming_a_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "default_snapshot_scenario",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // A filter that matched nothing is reported as success.
    assert!(stdout.contains("1 passed"), "{stdout}");

    let base = ra_sandbox::snapshot::defaults::default_local_snapshot_base_dir(
        &ra_sandbox::snapshot::defaults::SnapshotHost::new(
            home.path(),
            std::collections::BTreeMap::new(),
            ra_sandbox::snapshot::defaults::HostPlatform::current(),
        ),
    );
    let snapshots = std::fs::read_dir(&base)
        .unwrap_or_else(|error| panic!("{}: {error}", base.display()))
        .count();
    assert_eq!(snapshots, 1, "{}", base.display());
}

#[tokio::test]
#[ignore = "run by `a_continued_run_finds_its_workspace_without_naming_a_snapshot` in a child"]
async fn default_snapshot_scenario() {
    assert!(
        std::env::var_os(CHILD_MARKER).is_some(),
        "run through its parent test, which gives it a home of its own"
    );
    let roots = Arc::new(Mutex::new(Vec::new()));
    let config = || {
        RunConfig::new().with_sandbox(
            SandboxRunConfig::new()
                .with_client(Arc::new(UnixLocalSandboxClient::new()) as Arc<dyn SandboxClient>),
        )
    };

    let first = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(ScriptedModel::new(vec![
                call("call-1", "write_note"),
                answer("m1"),
            ]))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("write a note"))],
        )
        .with_config(config()),
    )
    .await
    .unwrap();

    let second_model = ScriptedModel::new(vec![call("call-2", "read_note"), answer("m2")]);
    Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(Arc::clone(&second_model))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("read it back"))],
        )
        .with_state(first.state().clone())
        .with_config(config()),
    )
    .await
    .unwrap();

    let outputs = second_model.tool_outputs.lock().unwrap().clone();
    assert!(
        outputs.len() == 1 && outputs[0].contains("written in the first run"),
        "{outputs:?}"
    );
}

#[tokio::test]
async fn a_continued_run_gets_its_workspace_back_from_the_snapshot() {
    let snapshots = tempfile::tempdir().unwrap();
    let roots = Arc::new(Mutex::new(Vec::new()));

    let first_model = ScriptedModel::new(vec![call("call-1", "write_note"), answer("m1")]);
    let first = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(Arc::clone(&first_model))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("write a note"))],
        )
        .with_config(RunConfig::new().with_sandbox(sandbox(&snapshots))),
    )
    .await
    .unwrap();

    assert_eq!(first.final_text(), "done");
    let outputs = first_model.tool_outputs.lock().unwrap().clone();
    assert!(
        outputs.len() == 1 && outputs[0].contains("written"),
        "{outputs:?}"
    );
    // The client replaced the default root with a directory of its own, materialized the manifest
    // there, and deleted it when the run ended.
    let first_root = PathBuf::from(roots.lock().unwrap()[0].clone());
    assert_ne!(first_root, PathBuf::from("/workspace"));
    assert!(!first_root.exists(), "{}", first_root.display());
    let payload = first.state().sandbox_resume_state().unwrap().clone();
    assert_eq!(payload["backend_id"], "unix_local");

    // Continued from the checkpoint through JSON: the workspace comes back from the snapshot.
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(first.state()).unwrap()).unwrap();
    let second_model = ScriptedModel::new(vec![call("call-2", "read_note"), answer("m2")]);
    let second = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(Arc::clone(&second_model))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("read it back"))],
        )
        .with_state(restored)
        .with_config(RunConfig::new().with_sandbox(sandbox(&snapshots))),
    )
    .await
    .unwrap();

    assert_eq!(second.final_text(), "done");
    let outputs = second_model.tool_outputs.lock().unwrap().clone();
    assert!(
        outputs.len() == 1 && outputs[0].contains("written in the first run"),
        "{outputs:?}"
    );
    let second_root = PathBuf::from(roots.lock().unwrap()[1].clone());
    assert!(!second_root.exists(), "{}", second_root.display());
}

/// `test_runner_rejects_unix_local_manifest_user_and_group_provisioning`: an agent that runs as a
/// named user asks for an account this backend cannot create, and the run is refused rather than
/// materializing content for a user that does not exist.
#[tokio::test]
async fn a_run_as_user_is_refused_by_the_local_backend() {
    let snapshots = tempfile::tempdir().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .sandbox(SandboxAgentConfig::empty().with_run_as(ra_core::sandbox::User::new("builder")))
        .build()
        .unwrap();
    let error = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(FixedResolver(ScriptedModel::new(vec![answer("m")]))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_config(RunConfig::new().with_sandbox(sandbox(&snapshots))),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support manifest users or groups"),
        "{error}"
    );
}

/// `test_unix_local_runner_cleanup_preserves_resumed_caller_owned_workspace_root`: a root the host
/// chose is the host's, and neither the run that created the session nor the one that resumed it
/// deletes it.
#[tokio::test]
async fn a_workspace_root_the_host_chose_survives_both_runs() {
    let snapshots = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().join("repo");
    let roots = Arc::new(Mutex::new(Vec::new()));
    let config = || {
        RunConfig::new().with_sandbox(
            sandbox(&snapshots).with_manifest(
                Manifest::new()
                    .with_root(root.to_string_lossy().into_owned())
                    .with_entry("README.md", Entry::file(b"hello".to_vec())),
            ),
        )
    };

    let first = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(ScriptedModel::new(vec![answer("m1")]))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_config(config()),
    )
    .await
    .unwrap();
    assert!(root.join("README.md").exists());

    Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent(&roots)),
            Arc::new(FixedResolver(ScriptedModel::new(vec![answer("m2")]))),
            RunId::new("run-local"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("again"))],
        )
        .with_state(first.state().clone())
        .with_config(config()),
    )
    .await
    .unwrap();
    assert!(root.join("README.md").exists());
}

// --- pause and resume ------------------------------------------------------------------------

/// What the manifest writes and a snapshot keeps.
const DURABLE_TEXTS: [(&str, &str); 6] = [
    ("inline.txt", "inline file v1\n"),
    ("delete_me.txt", "delete me v1\n"),
    ("tree/nested.txt", "nested file v1\n"),
    ("copied_file.txt", "local file source v1\n"),
    ("copied_dir/child.txt", "local dir child v1\n"),
    (
        "copied_dir/nested/grandchild.txt",
        "local dir grandchild v1\n",
    ),
];

/// What the manifest writes and a snapshot leaves out, to be written again on resume.
const EPHEMERAL_TEXT: (&str, &str) = ("tree/ephemeral.txt", "ephemeral file v1\n");

/// The workspace after the first run's tools, as the reference's lifecycle helpers describe it.
const PATCHED_TEXTS: [(&str, &str); 8] = [
    ("inline.txt", "inline file v2\n"),
    ("created_by_patch.txt", "created by patch"),
    ("tree/nested.txt", "nested file v1\n"),
    ("copied_file.txt", "local file source v1\n"),
    ("copied_dir/child.txt", "local dir child v1\n"),
    (
        "copied_dir/nested/grandchild.txt",
        "local dir grandchild v1\n",
    ),
    ("runtime_note.txt", "runtime note v1\n"),
    ("archive_dir/hello.txt", "hello from tar archive\n"),
];

/// Every directory and file a resumed workspace holds, and nothing else.
const RESTORED_DIRS: [&str; 4] = ["archive_dir", "copied_dir", "copied_dir/nested", "tree"];
const RESTORED_FILES: [&str; 10] = [
    "archive_dir/hello.txt",
    "bundle.tar",
    "copied_dir/child.txt",
    "copied_dir/nested/grandchild.txt",
    "copied_file.txt",
    "created_by_patch.txt",
    "inline.txt",
    "runtime_note.txt",
    "tree/ephemeral.txt",
    "tree/nested.txt",
];

/// The host files the manifest copies from, and the manifest that names them.
fn lifecycle_sources(directory: &std::path::Path) -> Manifest {
    let source_root = directory.join("manifest-sources");
    std::fs::create_dir_all(source_root.join("local-dir/nested")).unwrap();
    std::fs::write(source_root.join("local-file.txt"), "local file source v1\n").unwrap();
    std::fs::write(
        source_root.join("local-dir/child.txt"),
        "local dir child v1\n",
    )
    .unwrap();
    std::fs::write(
        source_root.join("local-dir/nested/grandchild.txt"),
        "local dir grandchild v1\n",
    )
    .unwrap();
    let source = |name: &str| source_root.join(name).to_string_lossy().into_owned();
    Manifest::new()
        .with_path_grant(
            ra_core::sandbox::SandboxPathGrant::new(&source_root.to_string_lossy()).unwrap(),
        )
        .with_entry("inline.txt", Entry::file(b"inline file v1\n".to_vec()))
        .with_entry("delete_me.txt", Entry::file(b"delete me v1\n".to_vec()))
        .with_entry(
            "tree",
            Entry::dir()
                .with_child("nested.txt", Entry::file(b"nested file v1\n".to_vec()))
                .with_child(
                    "ephemeral.txt",
                    Entry::file(b"ephemeral file v1\n".to_vec()).ephemeral(true),
                ),
        )
        .with_entry(
            "copied_file.txt",
            Entry::local_file(source("local-file.txt")),
        )
        .with_entry("copied_dir", Entry::local_dir(Some(source("local-dir"))))
}

/// The reference's `SandboxFileCapability` and `SandboxLifecycleProbeCapability` in one, with its
/// `approval_tool` beside them.
struct LifecycleProbe {
    binding: Option<SandboxBinding>,
}

#[async_trait]
impl Capability for LifecycleProbe {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("sandbox_lifecycle_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let Some(binding) = &self.binding else {
            return Vec::new();
        };
        [
            "assert_manifest_materialized",
            "write_runtime_note",
            "apply_lifecycle_patch",
            "assert_workspace_escape_blocked",
            "extract_lifecycle_archive",
            "start_lifecycle_pty",
            "assert_restored_lifecycle_state",
            "read_runtime_note",
            "approval_tool",
        ]
        .into_iter()
        .map(|name| Arc::new(ProbeTool::new(name, binding.clone())) as Arc<dyn Tool>)
        .collect()
    }

    fn bind_sandbox(&self, binding: &SandboxBinding) -> Result<Option<Arc<dyn Capability>>> {
        Ok(Some(Arc::new(Self {
            binding: Some(binding.clone()),
        })))
    }
}

struct ProbeTool {
    inner: SessionTool,
}

impl ProbeTool {
    fn new(name: &str, binding: SandboxBinding) -> Self {
        Self {
            inner: SessionTool::new(name, binding),
        }
    }

    async fn run(&self) -> std::result::Result<String, String> {
        let session = self.inner.binding.session().as_ref();
        match self.inner.origin.name() {
            "assert_manifest_materialized" => {
                for (path, text) in DURABLE_TEXTS.into_iter().chain([EPHEMERAL_TEXT]) {
                    expect_text(session, path, text).await?;
                }
                Ok("manifest materialized".to_owned())
            }
            "write_runtime_note" => {
                session
                    .write(
                        "runtime_note.txt".into(),
                        b"runtime note v1\n".to_vec(),
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok("wrote runtime_note.txt".to_owned())
            }
            "apply_lifecycle_patch" => {
                use ra_patch::ApplyPatchOperation;
                ra_tools::sandbox::apply_patch::WorkspaceEditor::new(session)
                    .apply_patch(&[
                        ApplyPatchOperation::update_file(
                            "inline.txt",
                            "@@\n-inline file v1\n+inline file v2\n",
                        ),
                        ApplyPatchOperation::create_file(
                            "created_by_patch.txt",
                            "+created by patch\n",
                        ),
                        ApplyPatchOperation::delete_file("delete_me.txt"),
                    ])
                    .await
                    .map_err(|error| error.to_string())?;
                for (path, text) in &PATCHED_TEXTS[..7] {
                    expect_text(session, path, text).await?;
                }
                expect_missing(session, "delete_me.txt").await?;
                Ok("lifecycle patch applied".to_owned())
            }
            "assert_workspace_escape_blocked" => {
                for path in ["../outside.txt", "/tmp/sandbox-outside.txt"] {
                    expect_blocked(session, path).await?;
                }
                // A link planted inside the workspace that points out of it.
                let root = std::path::PathBuf::from(session.state().manifest().root.clone());
                let outside = root.parent().unwrap().join("symlink-outside.txt");
                std::fs::write(&outside, "outside symlink target\n").unwrap();
                std::os::unix::fs::symlink(&outside, root.join("symlink_escape.txt")).unwrap();
                let blocked = expect_blocked(session, "symlink_escape.txt").await;
                std::fs::remove_file(root.join("symlink_escape.txt")).unwrap();
                let untouched = std::fs::read_to_string(&outside).unwrap();
                std::fs::remove_file(&outside).unwrap();
                blocked?;
                if untouched != "outside symlink target\n" {
                    return Err(format!("the link's target was written: {untouched:?}"));
                }
                Ok("workspace escape blocked".to_owned())
            }
            "extract_lifecycle_archive" => {
                let mut builder = tar::Builder::new(Vec::new());
                let payload = b"hello from tar archive\n";
                let mut header = tar::Header::new_ustar();
                header.set_size(payload.len() as u64);
                header.set_mode(0o644);
                builder
                    .append_data(&mut header, "archive_dir/hello.txt", &payload[..])
                    .unwrap();
                session
                    .extract(
                        "bundle.tar".into(),
                        builder.into_inner().unwrap(),
                        None,
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                expect_text(session, "archive_dir/hello.txt", "hello from tar archive\n").await?;
                Ok("archive extracted".to_owned())
            }
            "start_lifecycle_pty" => {
                use ra_core::sandbox::{PtyStartRequest, PtyWriteRequest, ShellInvocation};
                let started = session
                    .pty_start(
                        PtyStartRequest::new(
                            [
                                "sh",
                                "-c",
                                "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
                            ]
                            .map(str::to_owned),
                        )
                        .with_shell(ShellInvocation::None)
                        .with_tty(true)
                        .with_yield_time_s(0.5),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let process = started.process_id.ok_or("the process exited")?;
                let ready = String::from_utf8_lossy(&started.output).replace("\r\n", "\n");
                if ready != "ready\n" {
                    return Err(format!("started with {ready:?}"));
                }
                let echoed = session
                    .pty_write(PtyWriteRequest::new(process, "hello pty\n").with_yield_time_s(0.5))
                    .await
                    .map_err(|error| error.to_string())?;
                let echoed_text = String::from_utf8_lossy(&echoed.output).replace("\r\n", "\n");
                if echoed_text != "hello pty\ngot:hello pty\n"
                    || echoed.process_id != Some(process)
                    || echoed.exit_code.is_some()
                {
                    return Err(format!("wrote and got {echoed:?}"));
                }
                Ok("pty started and echoed stdin".to_owned())
            }
            "assert_restored_lifecycle_state" => {
                for (path, text) in PATCHED_TEXTS.into_iter().chain([EPHEMERAL_TEXT]) {
                    expect_text(session, path, text).await?;
                }
                expect_missing(session, "delete_me.txt").await?;
                let (dirs, files) = workspace_tree(session).await?;
                if dirs != RESTORED_DIRS || files != RESTORED_FILES {
                    return Err(format!("restored tree is {dirs:?} and {files:?}"));
                }
                Ok("restored lifecycle state verified".to_owned())
            }
            "read_runtime_note" => session
                .read("runtime_note.txt".into(), None)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|error| error.to_string()),
            "approval_tool" => Ok("approved".to_owned()),
            other => Err(format!("no tool {other}")),
        }
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn origin(&self) -> &ToolOrigin {
        &self.inner.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.inner.schema
    }

    fn options(&self) -> ra_core::tool::ToolOptions {
        let options = ra_core::tool::ToolOptions::new();
        if self.inner.origin.name() == "approval_tool" {
            options.with_approval(ra_core::tool::ToolApprovalPolicy::Always)
        } else {
            options
        }
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text(
            self.run()
                .await
                .unwrap_or_else(|error| format!("error: {error}")),
        ))
    }
}

async fn expect_text(
    session: &dyn ra_core::sandbox::SandboxSession,
    path: &str,
    expected: &str,
) -> std::result::Result<(), String> {
    let bytes = session
        .read(path.into(), None)
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    let actual = String::from_utf8_lossy(&bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{path} holds {actual:?}, not {expected:?}"))
    }
}

async fn expect_missing(
    session: &dyn ra_core::sandbox::SandboxSession,
    path: &str,
) -> std::result::Result<(), String> {
    match session.read(path.into(), None).await {
        Err(error) if error.error_code() == ra_core::sandbox::ErrorCode::WorkspaceReadNotFound => {
            Ok(())
        }
        other => Err(format!("{path} should be missing, got {other:?}")),
    }
}

/// Read, write and patch are all refused for a path that leaves the workspace.
async fn expect_blocked(
    session: &dyn ra_core::sandbox::SandboxSession,
    path: &str,
) -> std::result::Result<(), String> {
    use ra_core::sandbox::ErrorCode;
    let read = session.read(path.into(), None).await.map(|_| ());
    let write = session
        .write(path.into(), b"outside write\n".to_vec(), None)
        .await;
    let patch = ra_tools::sandbox::apply_patch::WorkspaceEditor::new(session)
        .apply_patch(&[ra_patch::ApplyPatchOperation::create_file(
            path,
            "+outside patch\n",
        )])
        .await
        .map(|_| ());
    for (operation, outcome) in [("read", read), ("write", write), ("patch", patch)] {
        match outcome {
            Err(error)
                if matches!(
                    error.error_code(),
                    ErrorCode::InvalidManifestPath | ErrorCode::ApplyPatchInvalidPath
                ) => {}
            other => return Err(format!("{operation} of {path} was not blocked: {other:?}")),
        }
    }
    Ok(())
}

/// Every directory and file under the workspace, relative to its root, sorted.
async fn workspace_tree(
    session: &dyn ra_core::sandbox::SandboxSession,
) -> std::result::Result<(Vec<String>, Vec<String>), String> {
    let root = std::fs::canonicalize(session.state().manifest().root.clone())
        .map_err(|error| error.to_string())?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut pending = vec![".".to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in session
            .ls((&directory).into(), None)
            .await
            .map_err(|error| format!("{directory}: {error}"))?
        {
            let path =
                std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone().into());
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| format!("{} is outside the workspace", entry.path))?
                .to_string_lossy()
                .into_owned();
            if entry.is_dir() {
                pending.push(relative.clone());
                dirs.push(relative);
            } else {
                files.push(relative);
            }
        }
    }
    dirs.sort();
    files.sort();
    Ok((dirs, files))
}

/// The text of every tool output the model was shown, each once, in the order it first appeared.
///
/// The model is shown the whole history on every request, so each output is recorded once per
/// request that follows it.
fn tool_texts(model: &ScriptedModel) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for output in model.tool_outputs.lock().unwrap().iter() {
        let value: serde_json::Value = serde_json::from_str(output).unwrap();
        let text = value["blocks"][0]["text"].as_str().unwrap().to_owned();
        if !texts.contains(&text) {
            texts.push(text);
        }
    }
    texts
}

/// `integration_tests/test_runner_pause_resume.py::test_runner_preserves_unix_local_lifecycle_state_across_pause_and_resume`:
/// a run works in a real local workspace, stops for an approval, and is resumed from its state
/// read back from JSON; the resumed session finds the workspace the first one left — patched,
/// extended by an archive, with the ephemeral file written again and the deleted one still gone.
#[tokio::test]
async fn a_run_paused_for_approval_resumes_with_the_workspace_it_left() {
    let scratch = tempfile::tempdir().unwrap();
    let snapshots = tempfile::tempdir().unwrap();
    // Resolved first, as pytest's `tmp_path` is: a grant is compared after its symlinks are
    // followed and a copy source before, in the reference and here, so on macOS a grant spelled
    // `/var/...` does not cover a source spelled the same way.
    let manifest = lifecycle_sources(&std::fs::canonicalize(scratch.path()).unwrap());
    let agent = || {
        AgentSpec::builder()
            .id(AgentId::new("sandbox"))
            .name("sandbox")
            .instructions("Use the sandbox lifecycle tools.")
            .sandbox(
                SandboxAgentConfig::empty()
                    .with_default_manifest(manifest.clone())
                    .with_capability(Arc::new(LifecycleProbe { binding: None })),
            )
            .build()
            .unwrap()
    };

    let first_model = ScriptedModel::new(vec![
        call("call_manifest_materialized", "assert_manifest_materialized"),
        call("call_write_runtime_note", "write_runtime_note"),
        call("call_apply_lifecycle_patch", "apply_lifecycle_patch"),
        call(
            "call_assert_workspace_escape_blocked",
            "assert_workspace_escape_blocked",
        ),
        call(
            "call_extract_lifecycle_archive",
            "extract_lifecycle_archive",
        ),
        call("call_start_lifecycle_pty", "start_lifecycle_pty"),
        call("call_approval", "approval_tool"),
    ]);
    let first = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent()),
            Arc::new(FixedResolver(Arc::clone(&first_model))),
            RunId::new("run-pause"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(
                "verify the UnixLocal sandbox lifecycle and wait for approval",
            ))],
        )
        .with_config(RunConfig::new().with_sandbox(sandbox(&snapshots))),
    )
    .await
    .unwrap();

    assert_eq!(
        tool_texts(&first_model),
        [
            "manifest materialized",
            "wrote runtime_note.txt",
            "lifecycle patch applied",
            "workspace escape blocked",
            "archive extracted",
            "pty started and echoed stdin",
        ]
    );
    let ra_runtime::runner::RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(items.len(), 1);
    let payload = first.state().sandbox_resume_state().unwrap().clone();
    assert_eq!(payload["backend_id"], "unix_local");
    assert_eq!(payload["current_agent_name"], "sandbox");
    let session_state = &payload["session_state"];
    assert_eq!(session_state["snapshot"]["type"], "local");
    assert_eq!(session_state["workspace_root_owned"], true);
    assert_eq!(session_state["workspace_root_ready"], true);
    let workspace_root = PathBuf::from(session_state["manifest"]["root"].as_str().unwrap());
    assert!(!workspace_root.exists(), "{}", workspace_root.display());

    let mut restored: RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    restored.approve(&items[0], false).unwrap();
    let resumed_model = ScriptedModel::new(vec![
        call(
            "call_assert_restored_lifecycle_state",
            "assert_restored_lifecycle_state",
        ),
        call("call_read_runtime_note", "read_runtime_note"),
        answer("m-final"),
    ]);
    let resumed = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent()),
            Arc::new(FixedResolver(Arc::clone(&resumed_model))),
            RunId::new("run-pause"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_state(restored)
        .with_config(RunConfig::new().with_sandbox(sandbox(&snapshots))),
    )
    .await
    .unwrap();

    assert_eq!(resumed.final_text(), "done");
    assert!(!workspace_root.exists(), "{}", workspace_root.display());
    let outputs = tool_texts(&resumed_model);
    assert_eq!(
        outputs[outputs.len().saturating_sub(3)..],
        [
            "approved",
            "restored lifecycle state verified",
            "runtime note v1\n",
        ]
    );
}
