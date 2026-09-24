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
                .write("notes/note.txt", b"written in the first run".to_vec(), None)
                .await
                .map(|()| "written".to_owned())
        } else {
            session
                .read("notes/note.txt", None)
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
            SandboxAgentConfig::new()
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
        .sandbox(SandboxAgentConfig::new().with_run_as(ra_core::sandbox::User::new("builder")))
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
