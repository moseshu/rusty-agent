//! One set of runner-level sandbox scenarios, run unchanged against every built-in backend.
//!
//! The reference keeps its agent definition fixed and switches backends by handing the run
//! configuration a different client. Each scenario here is written once against the session
//! protocol and run through the runner on the local backend and on Docker: a synchronous run that is
//! continued from its checkpoint, a streamed run, a handoff whose agents each keep their own
//! workspace, the reference's pause-and-resume lifecycle, and a session the caller owns.
//!
//! # Running and accounting
//!
//! **Ignored unless asked for**: they need real backends, and `cargo xtask sandbox-parity` runs them
//! with `--ignored`. A backend whose prerequisites are missing on this machine — no Unix host, no
//! reachable Docker daemon, no local copy of the image — is **skipped, and says so**: each test
//! appends one line to the file `RA_SANDBOX_PARITY_REPORT` names, and the gate counts the skips
//! rather than reading them as passes. A backend named in `RA_SANDBOX_PARITY_REQUIRE`
//! (comma-separated backend ids, or `all`) is never skipped: missing prerequisites fail the test, so
//! a formal job that lost its daemon turns red instead of green.
//!
//! The image is `python:3.14-slim` unless `RA_DOCKER_TEST_IMAGE` names another. It is not pulled
//! here: a job pulls it up front, so that a registry outage reads as a missing prerequisite rather
//! than as a backend failure.

use std::{
    io::Write as _,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
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
    sandbox::{
        CreateRequest, DiscriminatedPayload, Entry, ErrorCode, ExecRequest, Manifest,
        PtyStartRequest, PtyWriteRequest, SandboxAgentConfig, SandboxClient, SandboxPathGrant,
        SandboxSession, ShellInvocation, SnapshotSpec,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
    sandbox::SandboxRunConfig,
};
use ra_sandbox::{
    docker::{
        BollardDockerApi, DEFAULT_PYTHON_SANDBOX_IMAGE, DockerApi, DockerSandboxClient,
        DockerSandboxClientOptions,
    },
    unix_local::UnixLocalSandboxClient,
};
use serde_json::{Value, json};

// -- backends ----------------------------------------------------------------------------------

/// The backends every scenario runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    UnixLocal,
    Docker,
}

impl Backend {
    /// The id the backend records in the states it writes, which is also how a job names it.
    const fn id(self) -> &'static str {
        match self {
            Self::UnixLocal => "unix_local",
            Self::Docker => "docker",
        }
    }

    /// Whether the job running this test requires the backend, so that it may not be skipped.
    ///
    /// A name that is neither a backend nor `all` fails the test: ignoring a misspelled name would
    /// quietly turn a required backend back into a skippable one.
    fn required(self) -> bool {
        let names = std::env::var("RA_SANDBOX_PARITY_REQUIRE").unwrap_or_default();
        if names.trim().is_empty() {
            return false;
        }
        let mut required = false;
        for name in names.split(',').map(str::trim) {
            let known = [Self::UnixLocal.id(), Self::Docker.id(), "all"];
            assert!(
                known.contains(&name),
                "RA_SANDBOX_PARITY_REQUIRE names `{name}`, which is not one of {known:?}"
            );
            required |= name == "all" || name == self.id();
        }
        required
    }

    /// A client for this backend, or why this machine cannot give one.
    async fn prepare(self) -> std::result::Result<Fixture, String> {
        let snapshots = tempfile::tempdir().map_err(|error| error.to_string())?;
        match self {
            Self::UnixLocal => {
                if !cfg!(unix) {
                    return Err("the local backend runs on macOS and Linux only".to_owned());
                }
                Ok(Fixture {
                    backend: self,
                    client: Arc::new(UnixLocalSandboxClient::new()),
                    options: None,
                    docker: None,
                    snapshots,
                })
            }
            Self::Docker => {
                let api = Arc::new(
                    BollardDockerApi::connect_with_defaults()
                        .map_err(|error| format!("no usable Docker daemon address: {error}"))?,
                );
                let image = docker_image();
                match api.inspect_image(&image).await {
                    Ok(()) => {}
                    Err(error) if error.is_not_found() => {
                        return Err(format!(
                            "image `{image}` is not on the daemon; run `docker pull {image}` first"
                        ));
                    }
                    Err(error) => return Err(format!("the Docker daemon did not answer: {error}")),
                }
                Ok(Fixture {
                    backend: self,
                    client: Arc::new(DockerSandboxClient::new(api.clone())),
                    options: Some(DockerSandboxClientOptions::new(image).to_payload()),
                    docker: Some(api),
                    snapshots,
                })
            }
        }
    }
}

fn docker_image() -> String {
    std::env::var("RA_DOCKER_TEST_IMAGE")
        .unwrap_or_else(|_| DEFAULT_PYTHON_SANDBOX_IMAGE.to_owned())
}

/// Appends one line to the report file the gate reads, when one is named.
fn report(status: &str, backend: Backend, scenario: &str, detail: &str) {
    let Some(path) = std::env::var_os("RA_SANDBOX_PARITY_REPORT") else {
        return;
    };
    let line = format!(
        "{status}\t{}\t{scenario}\t{}\n",
        backend.id(),
        detail.replace(['\t', '\n'], " ")
    );
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("the parity report file opens for appending");
    file.write_all(line.as_bytes())
        .expect("the parity report line is written");
}

/// The fixture for `backend`, or `None` after recording a skip; a required backend never skips.
async fn fixture(backend: Backend, scenario: &str) -> Option<Fixture> {
    // Read before anything else, so a misspelled requirement fails even where every backend is
    // available.
    let required = backend.required();
    match backend.prepare().await {
        Ok(fixture) => Some(fixture),
        Err(reason) => {
            assert!(
                !required,
                "the `{}` backend is required by RA_SANDBOX_PARITY_REQUIRE but unavailable: {reason}",
                backend.id()
            );
            eprintln!("SKIP {} {scenario}: {reason}", backend.id());
            report("SKIP", backend, scenario, &reason);
            None
        }
    }
}

/// One backend, ready to run scenarios against.
struct Fixture {
    backend: Backend,
    client: Arc<dyn SandboxClient>,
    options: Option<DiscriminatedPayload>,
    /// The daemon, for checking what a run left behind.
    docker: Option<Arc<BollardDockerApi>>,
    snapshots: tempfile::TempDir,
}

impl Fixture {
    /// The run configuration every scenario uses: this backend's client and options, with
    /// snapshots in a directory of the test's own.
    fn sandbox(&self) -> SandboxRunConfig {
        let config = SandboxRunConfig::new()
            .with_client(Arc::clone(&self.client))
            .with_snapshot_spec(SnapshotSpec::Local {
                base_path: self.snapshots.path().to_path_buf(),
            });
        match &self.options {
            Some(options) => config.with_options(options.clone()),
            None => config,
        }
    }

    /// A session the caller creates and starts itself.
    async fn create_session(&self, manifest: Manifest) -> Arc<dyn SandboxSession> {
        let mut request = CreateRequest::new().with_manifest(manifest);
        if let Some(options) = &self.options {
            request = request.with_options(options.clone());
        }
        let session: Arc<dyn SandboxSession> =
            Arc::from(self.client.create(request).await.expect("created"));
        session.start().await.expect("started");
        session
    }

    /// Asserts that nothing is left of the session a persisted state describes: the local backend's
    /// workspace directory is gone, and so is Docker's container.
    async fn assert_released(&self, session_state: &Value) {
        match self.backend {
            Backend::UnixLocal => {
                let root = session_state["manifest"]["root"]
                    .as_str()
                    .expect("a persisted root");
                assert!(
                    !std::path::Path::new(root).exists(),
                    "the workspace {root} was left behind"
                );
            }
            Backend::Docker => {
                let container = session_state["container_id"]
                    .as_str()
                    .expect("a persisted container id");
                let api = self.docker.as_ref().expect("a daemon");
                let gone = api
                    .inspect_container(container)
                    .await
                    .expect_err("the container was left behind");
                assert!(gone.is_not_found(), "{gone}");
            }
        }
    }
}

// -- a scripted model --------------------------------------------------------------------------

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

    /// The text of every tool output the model was shown, each once, in the order it first
    /// appeared.
    ///
    /// The model is shown the whole history on every request, so each output is recorded once per
    /// request that follows it.
    fn tool_texts(&self) -> Vec<String> {
        let mut texts: Vec<String> = Vec::new();
        for output in self.tool_outputs.lock().unwrap().iter() {
            let value: Value = serde_json::from_str(output).unwrap();
            let text = value["blocks"][0]["text"].as_str().unwrap().to_owned();
            if !texts.contains(&text) {
                texts.push(text);
            }
        }
        texts
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

fn answer(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn request(
    agent: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    input: &str,
    config: RunConfig,
) -> RunRequest {
    let input = if input.is_empty() {
        Vec::new()
    } else {
        vec![ModelInputItem::Message(Message::user(input))]
    };
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver(Arc::clone(model))),
        RunId::new("run-parity"),
        CancelScope::root(),
        input,
    )
    .with_config(config)
}

fn resume_payload(result: &RunResult) -> Value {
    result
        .state()
        .sandbox_resume_state()
        .cloned()
        .expect("a finished sandbox run records what resumes it")
}

/// A checkpoint, as a host that stored it would read it back.
fn through_json(state: &RunState) -> RunState {
    serde_json::from_str(&serde_json::to_string(state).unwrap()).unwrap()
}

// -- a capability that works through its session -----------------------------------------------

/// The tools every scenario drives, each working only through the session the capability was bound
/// to, so that what they see is what the backend did.
///
/// `label` is written into the note, so that two agents' workspaces can be told apart.
struct Probe {
    label: &'static str,
    binding: Option<SandboxBinding>,
}

impl Probe {
    fn unbound(label: &'static str) -> Arc<dyn Capability> {
        Arc::new(Self {
            label,
            binding: None,
        })
    }
}

#[async_trait]
impl Capability for Probe {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("sandbox_parity_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let Some(binding) = &self.binding else {
            return Vec::new();
        };
        [
            "write_note",
            "read_note",
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
        .map(|name| {
            Arc::new(ProbeTool {
                label: self.label,
                binding: binding.clone(),
                origin: ToolOrigin::new(name).unwrap(),
                schema: ToolSchema::new(
                    name,
                    json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
                )
                .unwrap(),
            }) as Arc<dyn Tool>
        })
        .collect()
    }

    fn bind_sandbox(&self, binding: &SandboxBinding) -> Result<Option<Arc<dyn Capability>>> {
        Ok(Some(Arc::new(Self {
            label: self.label,
            binding: Some(binding.clone()),
        })))
    }
}

struct ProbeTool {
    label: &'static str,
    binding: SandboxBinding,
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for ProbeTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        if self.origin.name() == "approval_tool" {
            ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
        } else {
            ToolOptions::new()
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

impl ProbeTool {
    async fn run(&self) -> std::result::Result<String, String> {
        let session = self.binding.session().as_ref();
        match self.origin.name() {
            "write_note" => {
                session
                    .write(
                        "notes/note.txt".into(),
                        format!("written by {}", self.label).into_bytes(),
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("{} wrote the note", self.label))
            }
            "read_note" => match session.read("notes/note.txt".into(), None).await {
                Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {
                    Ok(format!("{} found no note", self.label))
                }
                Err(error) => Err(error.to_string()),
            },
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
                let root = session.state().manifest().root.clone();
                let outside = format!("{}/symlink-outside.txt", parent_of(&root));
                for path in ["../outside.txt", "/tmp/sandbox-outside.txt"] {
                    expect_blocked(session, path).await?;
                }
                // A link planted inside the workspace that points out of it, made where the
                // backend keeps the workspace: on the host for the local backend, whose own
                // commands may not write outside the workspace, and inside the container for
                // Docker.
                let link = format!("{root}/symlink_escape.txt");
                let local = session.backend_id() == Backend::UnixLocal.id();
                if local {
                    std::fs::write(&outside, "outside symlink target\n")
                        .map_err(|error| error.to_string())?;
                    std::os::unix::fs::symlink(&outside, &link)
                        .map_err(|error| error.to_string())?;
                } else {
                    shell(
                        session,
                        &format!(
                            "printf 'outside symlink target\\n' > '{outside}' && \
                             ln -s '{outside}' '{link}'"
                        ),
                    )
                    .await?;
                }
                let blocked = expect_blocked(session, "symlink_escape.txt").await;
                let untouched = if local {
                    let text =
                        std::fs::read_to_string(&outside).map_err(|error| error.to_string())?;
                    std::fs::remove_file(&link).map_err(|error| error.to_string())?;
                    std::fs::remove_file(&outside).map_err(|error| error.to_string())?;
                    text
                } else {
                    shell(
                        session,
                        &format!("cat '{outside}'; rm -f '{link}' '{outside}'"),
                    )
                    .await?
                };
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
                let (dirs, files) = workspace_tree(session, &self.binding).await?;
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

// -- workspace helpers -------------------------------------------------------------------------

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
fn lifecycle_manifest(directory: &std::path::Path) -> Manifest {
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
        .with_path_grant(SandboxPathGrant::new(&source_root.to_string_lossy()).unwrap())
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

/// The directory holding `path`, for an absolute POSIX path.
fn parent_of(path: &str) -> &str {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "",
        Some((parent, _)) => parent,
    }
}

/// Runs `script` through the session's shell and returns its standard output, failing on a
/// non-zero exit.
async fn shell(session: &dyn SandboxSession, script: &str) -> std::result::Result<String, String> {
    let result = session
        .exec(ExecRequest::new([script.to_owned()]))
        .await
        .map_err(|error| format!("{script}: {error}"))?;
    if result.exit_code != 0 {
        return Err(format!(
            "{script} exited {}: {}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

async fn expect_text(
    session: &dyn SandboxSession,
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
    session: &dyn SandboxSession,
    path: &str,
) -> std::result::Result<(), String> {
    match session.read(path.into(), None).await {
        Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => Ok(()),
        other => Err(format!("{path} should be missing, got {other:?}")),
    }
}

/// Read, write and patch are all refused for a path that leaves the workspace.
async fn expect_blocked(
    session: &dyn SandboxSession,
    path: &str,
) -> std::result::Result<(), String> {
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
    session: &dyn SandboxSession,
    binding: &SandboxBinding,
) -> std::result::Result<(Vec<String>, Vec<String>), String> {
    // The local backend reports host paths, which on macOS reach the temporary directory through a
    // symbolic link; Docker's are container paths, compared as written.
    let local = session.backend_id() == Backend::UnixLocal.id();
    let spell = |path: &str| {
        if local {
            std::fs::canonicalize(path).map_or_else(
                |_| path.to_owned(),
                |path| path.to_string_lossy().into_owned(),
            )
        } else {
            path.to_owned()
        }
    };
    let root = spell(&binding.manifest().root);
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut pending = vec![".".to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in session
            .ls((&directory).into(), None)
            .await
            .map_err(|error| format!("{directory}: {error}"))?
        {
            let path = spell(&entry.path);
            let relative = path
                .strip_prefix(&root)
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| format!("{} is outside the workspace {root}", entry.path))?
                .to_owned();
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

// -- agents ------------------------------------------------------------------------------------

fn transfer_schema(target: &str) -> ToolSchema {
    ToolSchema::new(
        format!("transfer_to_{target}"),
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    )
    .unwrap()
}

/// A sandbox agent with the probe, and with a handoff to each of `handoffs`.
fn agent(id: &'static str, manifest: Manifest, handoffs: &[&str]) -> Arc<AgentSpec> {
    let mut builder = AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions("Use the sandbox tools.");
    for target in handoffs {
        builder = builder.handoff(HandoffSpec::new(
            AgentId::new(*target),
            transfer_schema(target),
        ));
    }
    builder
        .sandbox(
            SandboxAgentConfig::new()
                .with_default_manifest(manifest)
                .with_capability(Probe::unbound(id)),
        )
        .build()
        .unwrap()
}

fn note_manifest() -> Manifest {
    Manifest::new().with_entry("README.md", Entry::file(b"hello".to_vec()))
}

// -- scenarios ---------------------------------------------------------------------------------

/// A run creates its session, works in it, releases it, and records what resumes it; a run
/// continued from that record, read back from JSON, gets the workspace back.
///
/// The reference's `test_runner_restores_sandbox_from_run_state`, on each backend.
async fn synchronous_run(fixture: &Fixture) {
    let coder = || agent("coder", note_manifest(), &[]);
    let first_model = ScriptedModel::new(vec![call("call-1", "write_note"), answer("m1", "done")]);
    let first = Runner::run(request(
        coder(),
        &first_model,
        "write a note",
        RunConfig::new().with_sandbox(fixture.sandbox()),
    ))
    .await
    .unwrap();

    assert_eq!(first.final_text(), "done");
    assert_eq!(first_model.tool_texts(), ["coder wrote the note"]);
    let payload = resume_payload(&first);
    assert_eq!(payload["backend_id"], fixture.backend.id());
    fixture.assert_released(&payload["session_state"]).await;

    let second_model = ScriptedModel::new(vec![call("call-2", "read_note"), answer("m2", "done")]);
    let second = Runner::run(
        request(
            coder(),
            &second_model,
            "read it back",
            RunConfig::new().with_sandbox(fixture.sandbox()),
        )
        .with_state(through_json(first.state())),
    )
    .await
    .unwrap();

    assert_eq!(second.final_text(), "done");
    assert_eq!(second_model.tool_texts(), ["written by coder"]);
    fixture
        .assert_released(&resume_payload(&second)["session_state"])
        .await;
}

/// The streamed entry point creates, uses and releases its session the same way.
///
/// The reference's `test_runner_streamed_cleans_runner_owned_session`, on each backend.
async fn streamed_run(fixture: &Fixture) {
    let model = ScriptedModel::new(vec![
        call("call-1", "write_note"),
        call("call-2", "read_note"),
        answer("m", "streamed"),
    ]);
    let result = Runner::run_streamed(request(
        agent("coder", note_manifest(), &[]),
        &model,
        "write and read a note",
        RunConfig::new().with_sandbox(fixture.sandbox()),
    ))
    .finish()
    .await
    .unwrap();

    assert_eq!(result.final_text(), "streamed");
    assert_eq!(
        model.tool_texts(),
        ["coder wrote the note", "written by coder"]
    );
    let payload = resume_payload(&result);
    assert_eq!(payload["backend_id"], fixture.backend.id());
    fixture.assert_released(&payload["session_state"]).await;
}

/// Each agent across a handoff works in a session of its own, and a continued run resumes each
/// from its own entry.
///
/// The reference's `test_runner_rebuilds_sandbox_resources_for_handoff_target_agent` and
/// `test_runner_restores_all_sandbox_agents_from_run_state_across_handoffs`, on each backend.
async fn handoff(fixture: &Fixture) {
    let planner = agent("planner", note_manifest(), &["reviewer"]);
    let reviewer = agent("reviewer", note_manifest(), &["planner"]);
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&planner))
        .register(Arc::clone(&reviewer))
        .build()
        .unwrap();
    let config = || {
        RunConfig::new()
            .with_agent_registry(registry.clone())
            .with_sandbox(fixture.sandbox())
    };

    let first_model = ScriptedModel::new(vec![
        call("call-1", "write_note"),
        call("call-2", "transfer_to_reviewer"),
        call("call-3", "read_note"),
        call("call-4", "write_note"),
        answer("m1", "reviewed"),
    ]);
    let first = Runner::run(request(
        Arc::clone(&planner),
        &first_model,
        "plan, then review",
        config(),
    ))
    .await
    .unwrap();

    assert_eq!(first.final_text(), "reviewed");
    let texts = first_model.tool_texts();
    for expected in [
        "planner wrote the note",
        "reviewer found no note",
        "reviewer wrote the note",
    ] {
        assert!(texts.iter().any(|text| text == expected), "{texts:?}");
    }
    let payload = resume_payload(&first);
    assert_eq!(payload["current_agent_key"], "reviewer");
    let sessions = payload["sessions_by_agent"].as_object().unwrap();
    assert_eq!(sessions.keys().collect::<Vec<_>>(), ["planner", "reviewer"]);
    for entry in sessions.values() {
        fixture.assert_released(&entry["session_state"]).await;
    }

    // Continued from the checkpoint with the reviewer that last spoke, handing back to the
    // planner: each finds the note it wrote.
    let second_model = ScriptedModel::new(vec![
        call("call-5", "read_note"),
        call("call-6", "transfer_to_planner"),
        call("call-7", "read_note"),
        answer("m2", "planned again"),
    ]);
    let second = Runner::run(
        request(Arc::clone(&reviewer), &second_model, "again", config())
            .with_state(through_json(first.state())),
    )
    .await
    .unwrap();

    assert_eq!(second.final_text(), "planned again");
    let texts = second_model.tool_texts();
    for expected in ["written by reviewer", "written by planner"] {
        assert!(texts.iter().any(|text| text == expected), "{texts:?}");
    }
    let payload = resume_payload(&second);
    assert_eq!(payload["current_agent_key"], "planner");
    for entry in payload["sessions_by_agent"].as_object().unwrap().values() {
        fixture.assert_released(&entry["session_state"]).await;
    }
}

/// A run works in a real workspace, stops for an approval, and is resumed from its state read back
/// from JSON; the resumed session finds the workspace the first one left — patched, extended by an
/// archive, with the ephemeral file written again and the deleted one still gone.
///
/// The reference's
/// `integration_tests/test_runner_pause_resume.py::test_runner_preserves_unix_local_lifecycle_state_across_pause_and_resume`,
/// on each backend.
async fn pause_and_resume(fixture: &Fixture) {
    let scratch = tempfile::tempdir().unwrap();
    // Resolved first, as pytest's `tmp_path` is: a grant is compared after its symlinks are
    // followed and a copy source before, so on macOS a grant spelled `/var/...` does not cover a
    // source spelled the same way.
    let manifest = lifecycle_manifest(&std::fs::canonicalize(scratch.path()).unwrap());
    let sandbox = || agent("sandbox", manifest.clone(), &[]);

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
    let first = Runner::run(request(
        sandbox(),
        &first_model,
        "verify the sandbox lifecycle and wait for approval",
        RunConfig::new().with_sandbox(fixture.sandbox()),
    ))
    .await
    .unwrap();

    assert_eq!(
        first_model.tool_texts(),
        [
            "manifest materialized",
            "wrote runtime_note.txt",
            "lifecycle patch applied",
            "workspace escape blocked",
            "archive extracted",
            "pty started and echoed stdin",
        ]
    );
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(items.len(), 1);
    let payload = resume_payload(&first);
    assert_eq!(payload["backend_id"], fixture.backend.id());
    assert_eq!(payload["current_agent_name"], "sandbox");
    assert_eq!(payload["session_state"]["snapshot"]["type"], "local");
    fixture.assert_released(&payload["session_state"]).await;

    let mut restored = through_json(first.state());
    restored.approve(&items[0], false).unwrap();
    let resumed_model = ScriptedModel::new(vec![
        call(
            "call_assert_restored_lifecycle_state",
            "assert_restored_lifecycle_state",
        ),
        call("call_read_runtime_note", "read_runtime_note"),
        answer("m-final", "done"),
    ]);
    let resumed = Runner::run(
        request(
            sandbox(),
            &resumed_model,
            "",
            RunConfig::new().with_sandbox(fixture.sandbox()),
        )
        .with_state(restored),
    )
    .await
    .unwrap();

    assert_eq!(resumed.final_text(), "done");
    let outputs = resumed_model.tool_texts();
    assert_eq!(
        outputs[outputs.len().saturating_sub(3)..],
        [
            "approved",
            "restored lifecycle state verified",
            "runtime note v1\n",
        ]
    );
    fixture
        .assert_released(&resume_payload(&resumed)["session_state"])
        .await;
}

/// A session the caller created is used as it is by every run handed it, is neither stopped nor
/// deleted by them, and is not recorded in their checkpoints; the caller ends it.
///
/// The reference's `test_runner_does_not_close_injected_sandbox_session` and
/// `test_runner_does_not_restart_running_injected_sandbox_session`, on each backend.
async fn caller_owned_session(fixture: &Fixture) {
    let session = fixture.create_session(note_manifest()).await;
    let config = || {
        RunConfig::new().with_sandbox(SandboxRunConfig::new().with_session(Arc::clone(&session)))
    };

    let first_model = ScriptedModel::new(vec![call("call-1", "write_note"), answer("m1", "done")]);
    let first = Runner::run(request(
        agent("coder", note_manifest(), &[]),
        &first_model,
        "write a note",
        config(),
    ))
    .await
    .unwrap();
    assert_eq!(first_model.tool_texts(), ["coder wrote the note"]);
    assert_eq!(first.state().sandbox_resume_state(), None);
    assert!(
        session.running().await.expect("running"),
        "the run stopped the caller's session"
    );

    let second_model = ScriptedModel::new(vec![call("call-2", "read_note"), answer("m2", "done")]);
    let second = Runner::run(request(
        agent("coder", note_manifest(), &[]),
        &second_model,
        "read it back",
        config(),
    ))
    .await
    .unwrap();
    assert_eq!(second_model.tool_texts(), ["written by coder"]);
    assert_eq!(second.state().sandbox_resume_state(), None);

    // Still the caller's: its workspace holds what the runs wrote, until the caller ends it.
    assert_eq!(
        session
            .read("notes/note.txt".into(), None)
            .await
            .expect("read"),
        b"written by coder"
    );
    let state = session.state();
    let state = json!({
        "manifest": {"root": state.manifest().root},
        "container_id": state.field("container_id"),
    });
    session.close().await.expect("closed");
    fixture
        .client
        .delete(session.as_ref())
        .await
        .expect("deleted");
    fixture.assert_released(&state).await;
}

// -- the suite, once per backend ---------------------------------------------------------------

macro_rules! parity_suite {
    ($module:ident, $backend:expr) => {
        mod $module {
            use super::*;

            async fn run<F, Fut>(scenario: &str, body: F)
            where
                F: FnOnce(Fixture) -> Fut,
                Fut: std::future::Future<Output = ()>,
            {
                let Some(fixture) = fixture($backend, scenario).await else {
                    return;
                };
                body(fixture).await;
                report("PASS", $backend, scenario, "");
            }

            #[tokio::test]
            #[ignore = "sandbox parity; run with `cargo xtask sandbox-parity`"]
            async fn a_synchronous_run_is_continued_from_its_checkpoint() {
                run("synchronous_run", |fixture| async move {
                    synchronous_run(&fixture).await;
                })
                .await;
            }

            #[tokio::test]
            #[ignore = "sandbox parity; run with `cargo xtask sandbox-parity`"]
            async fn a_streamed_run_creates_uses_and_releases_its_session() {
                run("streamed_run", |fixture| async move {
                    streamed_run(&fixture).await;
                })
                .await;
            }

            #[tokio::test]
            #[ignore = "sandbox parity; run with `cargo xtask sandbox-parity`"]
            async fn each_agent_across_a_handoff_keeps_its_own_workspace() {
                run("handoff", |fixture| async move {
                    handoff(&fixture).await;
                })
                .await;
            }

            #[tokio::test]
            #[ignore = "sandbox parity; run with `cargo xtask sandbox-parity`"]
            async fn a_run_paused_for_approval_resumes_with_the_workspace_it_left() {
                run("pause_and_resume", |fixture| async move {
                    pause_and_resume(&fixture).await;
                })
                .await;
            }

            #[tokio::test]
            #[ignore = "sandbox parity; run with `cargo xtask sandbox-parity`"]
            async fn a_session_the_caller_owns_outlives_the_runs_it_serves() {
                run("caller_owned_session", |fixture| async move {
                    caller_owned_session(&fixture).await;
                })
                .await;
            }
        }
    };
}

parity_suite!(unix_local, Backend::UnixLocal);
parity_suite!(docker, Backend::Docker);
