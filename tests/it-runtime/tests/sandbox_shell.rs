//! The shell and filesystem capabilities run end to end: a sandbox agent given `Shell` and
//! `Filesystem` works through the real local backend, from the run's working directory, with the
//! tools the session offers.
//!
//! Ported from the reference's `tests/sandbox/test_run_cwd.py`, plus an interactive round trip the
//! reference covers only below the runner, and a custom `apply_patch` call carried through an
//! approval. Its skills case runs a shared skill's script from a nested directory of the run's
//! working directory.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::Capability,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall, ToolCallKind, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    sandbox::{
        CreateRequest, Entry, Manifest, SandboxAgentConfig, SandboxClient, SandboxPathGrant,
        SandboxSession, SandboxWorkspaceScope,
    },
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
    sandbox::SandboxRunConfig,
};
use ra_sandbox::skills::LocalDirLazySkillSource;
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use ra_tools::sandbox::NeedsApproval;
use ra_tools::sandbox::filesystem::Filesystem;
use ra_tools::sandbox::shell::{Shell, ShellToolSet};
use ra_tools::sandbox::shell_tool::{
    ExecCommandArgs, ExecCommandTool, WriteStdinArgs, WriteStdinTool,
};
use ra_tools::sandbox::skills::{Skill, Skills};
use serde_json::{Value, json};
use tokio::sync::Barrier;

/// One model turn, given the tool outputs the model has seen so far.
type Step = Box<dyn Fn(&[String]) -> ModelResponse + Send + Sync>;

/// A model that answers from a script, optionally meeting other runs before its first answer.
struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    outputs: Mutex<Vec<String>>,
    instructions: Mutex<Vec<Option<String>>>,
    rendezvous: Option<Arc<Barrier>>,
}

impl ScriptedModel {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Self::meeting(steps, None)
    }

    fn meeting(steps: Vec<Step>, rendezvous: Option<Arc<Barrier>>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            outputs: Mutex::default(),
            instructions: Mutex::default(),
            rendezvous,
        })
    }

    fn outputs(&self) -> Vec<String> {
        self.outputs.lock().unwrap().clone()
    }

    /// The system instructions of each call, in order.
    fn instructions(&self) -> Vec<Option<String>> {
        self.instructions.lock().unwrap().clone()
    }

    fn assert_complete(&self) {
        assert!(self.steps.lock().unwrap().is_empty(), "unused model steps");
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let first = self.outputs.lock().unwrap().is_empty();
        self.instructions
            .lock()
            .unwrap()
            .push(request.system_instructions().map(str::to_owned));
        {
            let mut outputs = self.outputs.lock().unwrap();
            outputs.clear();
            for item in request.input() {
                if let ModelInputItem::ToolCallOutput(output) = item {
                    outputs.push(
                        output.output()["blocks"][0]["text"]
                            .as_str()
                            .map_or_else(|| output.output().to_string(), str::to_owned),
                    );
                }
            }
        }
        if first && let Some(rendezvous) = &self.rendezvous {
            rendezvous.wait().await;
        }
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("scripted model ran out of responses"))?;
        Ok(step(&self.outputs()))
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

fn call(id: &str, name: &str, arguments: Value) -> Step {
    let id = id.to_owned();
    let name = name.to_owned();
    Box::new(move |_| {
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(&id),
            RunItemKind::ToolCall(ToolCall::new(CallId::new(&id), &name, arguments.clone())),
        )])
    })
}

fn answer(id: &str) -> Step {
    let id = id.to_owned();
    Box::new(move |_| {
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(&id),
            RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
        )])
    })
}

fn shell_agent(name: &str, shell: Shell) -> Arc<AgentSpec> {
    sandbox_agent(name, vec![Arc::new(shell)])
}

fn sandbox_agent(name: &str, capabilities: Vec<Arc<dyn Capability>>) -> Arc<AgentSpec> {
    let mut config = SandboxAgentConfig::new();
    for capability in capabilities {
        config = config.with_capability(capability);
    }
    AgentSpec::builder()
        .id(AgentId::new(name))
        .name(name)
        .instructions("do the task")
        .sandbox(config)
        .build()
        .unwrap()
}

/// A model turn calling a custom tool with raw `input`.
fn custom_call(id: &str, name: &str, input: &str) -> Step {
    let id = id.to_owned();
    let name = name.to_owned();
    let input = input.to_owned();
    Box::new(move |_| {
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(&id),
            RunItemKind::ToolCall(ToolCall::custom(CallId::new(&id), &name, input.clone())),
        )])
    })
}

/// The output answering `call_id` among a run's new items.
fn tool_output<'a>(items: &'a [RunItem], call_id: &str) -> &'a ToolCallOutput {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no output for {call_id}"))
}

fn request(
    agent: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    run_id: &str,
    config: RunConfig,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver(Arc::clone(model))),
        RunId::new(run_id),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(config)
}

/// A started local session the tests hand to their runs, and the directory holding it.
async fn live_session() -> (tempfile::TempDir, PathBuf, Arc<dyn SandboxSession>) {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    let session = UnixLocalSandboxClient::new()
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(root.to_string_lossy().into_owned())),
        )
        .await
        .unwrap();
    session.start().await.unwrap();
    (directory, root, Arc::from(session))
}

fn in_session(session: &Arc<dyn SandboxSession>, cwd: &str) -> RunConfig {
    RunConfig::new().with_sandbox(
        SandboxRunConfig::new()
            .with_session(Arc::clone(session))
            .with_cwd(cwd)
            .unwrap(),
    )
}

// `test_resumed_run_rebinds_cwd_to_pending_sandbox_tool`
#[tokio::test]
async fn an_approved_command_runs_in_the_resumed_runs_working_directory() {
    let (_directory, root, session) = live_session().await;
    std::fs::create_dir_all(root.join("tasks/resumed-task")).unwrap();
    let agent = || {
        shell_agent(
            "resumed-task",
            Shell::new().with_configure_tools(|toolset: &mut ShellToolSet| {
                toolset
                    .exec_command_mut()
                    .set_needs_approval(NeedsApproval::Always);
            }),
        )
    };
    let model = ScriptedModel::new(vec![
        call(
            "resumed_shell",
            "exec_command",
            json!({"cmd": "printf resumed > marker.txt", "login": false}),
        ),
        answer("resumed_message"),
    ]);

    let first = Runner::run(request(
        agent(),
        &model,
        "run-resumed",
        in_session(&session, "tasks/resumed-task"),
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(items.len(), 1);
    assert!(!root.join("tasks/resumed-task/marker.txt").exists());

    let mut state = first.state().clone();
    state.approve(&items[0], false).unwrap();
    let resumed = Runner::run(
        request(
            agent(),
            &model,
            "run-resumed",
            in_session(&session, "tasks/resumed-task"),
        )
        .with_state(state),
    )
    .await
    .unwrap();

    assert_eq!(resumed.final_text(), "done");
    assert_eq!(
        std::fs::read(root.join("tasks/resumed-task/marker.txt")).unwrap(),
        b"resumed"
    );
    assert!(!root.join("marker.txt").exists());
    model.assert_complete();
    session.close().await.unwrap();
}

// `test_concurrent_runs_scope_relative_paths_with_shared_live_session`
#[tokio::test]
async fn concurrent_runs_on_one_session_keep_their_relative_paths_apart() {
    let (_directory, root, session) = live_session().await;
    let tasks = ["task-a", "task-b"];
    // A PNG and a JPEG signature, so each run's image is recognisably its own.
    let image = |task: &str| -> (&'static str, Vec<u8>) {
        if task == "task-a" {
            (
                "image/png",
                [b"\x89PNG\r\n\x1a\n".as_slice(), task.as_bytes()].concat(),
            )
        } else {
            (
                "image/jpeg",
                [b"\xff\xd8\xff".as_slice(), task.as_bytes()].concat(),
            )
        }
    };
    for task in tasks {
        let directory = root.join("tasks").join(task);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("seed.png"), image(task).1).unwrap();
    }

    // Both runs reach their first model call before either issues its command.
    let rendezvous = Arc::new(Barrier::new(tasks.len()));
    let runs = tasks.map(|task| {
        let model = ScriptedModel::meeting(
            vec![
                call(
                    &format!("{task}_shell"),
                    "exec_command",
                    json!({"cmd": "cp seed.png plot.png", "login": false}),
                ),
                call(
                    &format!("{task}_image"),
                    "view_image",
                    json!({"path": "plot.png"}),
                ),
                custom_call(
                    &format!("{task}_patch"),
                    "apply_patch",
                    &format!("*** Begin Patch\n*** Add File: notes.md\n+{task}\n*** End Patch\n"),
                ),
                answer(&format!("{task}_message")),
            ],
            Some(Arc::clone(&rendezvous)),
        );
        let run = Runner::run(request(
            sandbox_agent(
                task,
                vec![Arc::new(Shell::new()), Arc::new(Filesystem::new())],
            ),
            &model,
            &format!("run-{task}"),
            in_session(&session, &format!("tasks/{task}")),
        ));
        (model, run)
    });
    let [(model_a, run_a), (model_b, run_b)] = runs;
    let (result_a, result_b) = tokio::join!(run_a, run_b);

    for (task, result, model) in [
        ("task-a", result_a.unwrap(), model_a),
        ("task-b", result_b.unwrap(), model_b),
    ] {
        assert_eq!(result.final_text(), "done");
        let (media_type, bytes) = image(task);
        assert_eq!(
            std::fs::read(root.join("tasks").join(task).join("plot.png")).unwrap(),
            bytes
        );

        let image_output = tool_output(result.new_items(), &format!("{task}_image"));
        let block = &image_output.output()["blocks"][0];
        assert_eq!(block["source"]["data"]["media_type"], media_type, "{block}");
        assert_eq!(block["source"]["data"]["data"], BASE64.encode(&bytes));

        let patch_output = tool_output(result.new_items(), &format!("{task}_patch"));
        assert_eq!(patch_output.kind(), ToolCallKind::Custom);
        assert_eq!(
            patch_output.output()["blocks"][0]["text"],
            "Created notes.md"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tasks").join(task).join("notes.md")).unwrap(),
            task
        );
        model.assert_complete();
    }
    assert!(!root.join("plot.png").exists());
    assert!(!root.join("notes.md").exists());
    session.close().await.unwrap();
}

// `test_python_skill_uses_absolute_root_from_nested_workdir`
#[tokio::test]
async fn a_shared_skill_s_script_runs_from_a_nested_directory_and_keeps_task_files_local() {
    let skill_script = "from pathlib import Path\n\
                        skill_root = Path(__file__).parent.parent\n\
                        suffix = (skill_root / 'assets' / 'suffix.txt').read_text(encoding='utf-8')\n\
                        source = Path('input.txt').read_text(encoding='utf-8')\n\
                        Path('output.txt').write_text(source + suffix, encoding='utf-8')\n";
    let skills = Skills::builder()
        .skill(
            Skill::new(
                "python-proof",
                "Proves shared Python skills keep task files local.",
                "# Python proof\n",
            )
            .unwrap()
            .with_script("prove.py", Entry::file(skill_script))
            .unwrap()
            .with_asset("suffix.txt", Entry::file("-from-shared-skill"))
            .unwrap(),
        )
        .build()
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    let mut manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
    skills.process_manifest(&mut manifest).unwrap();
    let session: Arc<dyn SandboxSession> = Arc::from(
        UnixLocalSandboxClient::new()
            .create(CreateRequest::new().with_manifest(manifest))
            .await
            .unwrap(),
    );
    session.start().await.unwrap();
    session
        .mkdir("tasks/task-a/nested", true, None)
        .await
        .unwrap();
    session
        .write("tasks/task-a/nested/input.txt", b"task-a".to_vec(), None)
        .await
        .unwrap();
    let skill_root = format!("{}/.agents/python-proof", root.to_string_lossy());
    let model = ScriptedModel::new(vec![
        call(
            "python_skill",
            "exec_command",
            json!({
                "cmd": format!("python3 '{skill_root}/scripts/prove.py'"),
                "workdir": "nested",
                "login": false,
            }),
        ),
        answer("python_skill_message"),
    ]);

    let result = Runner::run(request(
        sandbox_agent(
            "python-skill-task",
            vec![Arc::new(Shell::new()), Arc::new(skills)],
        ),
        &model,
        "run-python-skill",
        in_session(&session, "tasks/task-a"),
    ))
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    assert_eq!(
        session
            .read("tasks/task-a/nested/output.txt", None)
            .await
            .unwrap(),
        b"task-a-from-shared-skill"
    );
    assert_eq!(
        session
            .read(".agents/python-proof/scripts/prove.py", None)
            .await
            .unwrap(),
        skill_script.as_bytes()
    );
    let instructions = model.instructions()[0]
        .clone()
        .expect("system instructions");
    assert!(
        instructions.contains(&format!("(file: {skill_root})")),
        "{instructions}"
    );
    assert!(instructions.contains("Treat each listed path as the skill root"));
    assert!(
        instructions.contains("Files outside the working directory may be visible to or shared")
    );
    model.assert_complete();
    session.close().await.unwrap();
}

/// A lazily indexed skill is staged into a real local workspace when the model asks for it, and
/// is then read from the path the index gave.
#[tokio::test]
async fn a_lazy_skill_is_staged_when_the_model_asks_and_read_from_the_indexed_path() {
    let sources = tempfile::tempdir().unwrap();
    let source_root = std::fs::canonicalize(sources.path())
        .unwrap()
        .join("skills");
    std::fs::create_dir_all(source_root.join("notes")).unwrap();
    std::fs::write(
        source_root.join("notes/SKILL.md"),
        "---\nname: notes\ndescription: Keeps notes.\n---\nWrite notes in notes.md.\n",
    )
    .unwrap();
    let skills = Skills::builder()
        .lazy_from(
            LocalDirLazySkillSource::new(Entry::local_dir(Some(
                source_root.to_string_lossy().into_owned(),
            )))
            .unwrap(),
        )
        .build()
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    let mut manifest = Manifest::new()
        .with_root(root.to_string_lossy().into_owned())
        .with_path_grant(SandboxPathGrant::new(&source_root.to_string_lossy()).unwrap());
    skills.process_manifest(&mut manifest).unwrap();
    let session: Arc<dyn SandboxSession> = Arc::from(
        UnixLocalSandboxClient::new()
            .create(CreateRequest::new().with_manifest(manifest))
            .await
            .unwrap(),
    );
    session.start().await.unwrap();
    std::fs::create_dir_all(root.join("tasks/task-a")).unwrap();
    let skill_root = format!("{}/.agents/notes", root.to_string_lossy());
    let model = ScriptedModel::new(vec![
        call("load", "load_skill", json!({"skill_name": "notes"})),
        call(
            "read",
            "exec_command",
            json!({"cmd": format!("cat '{skill_root}/SKILL.md'"), "login": false}),
        ),
        answer("done"),
    ]);

    let result = Runner::run(request(
        sandbox_agent(
            "lazy-skill-task",
            vec![Arc::new(Shell::new()), Arc::new(skills)],
        ),
        &model,
        "run-lazy-skill",
        in_session(&session, "tasks/task-a"),
    ))
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    let loaded: Value = serde_json::from_str(
        tool_output(result.new_items(), "load").output()["blocks"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        loaded,
        json!({"status": "loaded", "skill_name": "notes", "path": skill_root})
    );
    let read = tool_output(result.new_items(), "read").output()["blocks"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(read.contains("Write notes in notes.md."), "{read}");
    let instructions = model.instructions()[0]
        .clone()
        .expect("system instructions");
    assert!(
        instructions.contains(&format!("- notes: Keeps notes. (file: {skill_root})")),
        "{instructions}"
    );
    assert!(instructions.contains("### Lazy loading"));
    model.assert_complete();
    session.close().await.unwrap();
}

/// The session id `exec_command` reported, read out of its response as a model would.
fn session_id(output: &str) -> i64 {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Process running with session ID "))
        .unwrap_or_else(|| panic!("no session id in {output}"))
        .trim()
        .parse()
        .unwrap()
}

/// An interactive command started by `exec_command` is answered through `write_stdin`, using the
/// session id the first response carried.
#[tokio::test]
async fn a_command_started_on_a_terminal_is_answered_through_write_stdin() {
    let (_directory, _root, session) = live_session().await;
    let model = ScriptedModel::new(vec![
        call(
            "start",
            "exec_command",
            json!({
                "cmd": "IFS= read -r line; printf 'got %s\\n' \"$line\"",
                "tty": true,
                "yield_time_ms": 250,
            }),
        ),
        Box::new(|outputs: &[String]| {
            let id = session_id(outputs.last().expect("the start's response"));
            ModelResponse::new(vec![RunItem::new(
                ItemId::new("answer"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("answer"),
                    "write_stdin",
                    json!({"session_id": id, "chars": "hello\n", "yield_time_ms": 2000}),
                )),
            )])
        }),
        answer("done"),
    ]);

    let result = Runner::run(request(
        shell_agent("interactive", Shell::new()),
        &model,
        "run-interactive",
        in_session(&session, "."),
    ))
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    let outputs = model.outputs();
    let reply = outputs.last().expect("the write's response");
    assert!(reply.contains("Process exited with code 0"), "{reply}");
    assert!(reply.contains("got hello"), "{reply}");
    assert!(
        !reply.contains("Process running with session ID"),
        "{reply}"
    );
    model.assert_complete();
    session.close().await.unwrap();
}

// Below the runner: the two tools called directly against a live session.

fn tools(session: &Arc<dyn SandboxSession>) -> (ExecCommandTool, WriteStdinTool) {
    let toolset = Shell::new()
        .bound(Arc::clone(session), None, SandboxWorkspaceScope::root())
        .toolset()
        .unwrap();
    (
        toolset.exec_command().clone(),
        toolset
            .write_stdin()
            .expect("the local backend offers terminals")
            .clone(),
    )
}

/// Ctrl-C written to the terminal ends the command, and the response says it exited.
#[tokio::test]
async fn ctrl_c_through_write_stdin_interrupts_a_terminal_command() {
    let (_directory, _root, session) = live_session().await;
    let (exec, write) = tools(&session);

    let started = exec
        .run(
            &ExecCommandArgs::new("sleep 30")
                .with_tty(true)
                .with_yield_time_ms(250),
        )
        .await
        .unwrap();
    let id = session_id(&started);

    let interrupted = write
        .run(
            &WriteStdinArgs::new(id)
                .with_chars("\u{3}")
                .with_yield_time_ms(5000),
        )
        .await
        .unwrap();
    assert!(
        interrupted.contains("Process exited with code -2"),
        "{interrupted}"
    );
    assert!(!interrupted.contains("Process running"), "{interrupted}");

    let gone = write.run(&WriteStdinArgs::new(id)).await.unwrap();
    assert!(
        gone.contains(&format!("write_stdin failed: PTY session not found: {id}")),
        "{gone}"
    );
    session.close().await.unwrap();
}

/// A piped command that outlives the first wait keeps its session id, and an empty write collects
/// the rest of its output once it finishes.
#[tokio::test]
async fn a_piped_command_is_polled_until_it_finishes() {
    let (_directory, _root, session) = live_session().await;
    let (exec, write) = tools(&session);

    let started = exec
        .run(&ExecCommandArgs::new("printf start; sleep 1; printf end").with_yield_time_ms(250))
        .await
        .unwrap();
    assert!(started.contains("start"), "{started}");
    let id = session_id(&started);

    let refused = write
        .run(&WriteStdinArgs::new(id).with_chars("input"))
        .await
        .unwrap();
    assert!(
        refused.contains("stdin is not available for this process."),
        "{refused}"
    );
    assert!(refused.contains("Process exited with code 1"), "{refused}");

    let finished = write
        .run(&WriteStdinArgs::new(id).with_yield_time_ms(5000))
        .await
        .unwrap();
    assert!(
        finished.contains("Process exited with code 0"),
        "{finished}"
    );
    assert!(finished.ends_with("Output:\nend"), "{finished}");
    session.close().await.unwrap();
}

/// A custom call that waits for approval keeps its kind on the approval and on the output the
/// resumed run records, so a provider replays both as custom items; approving runs the patch in the
/// run's working directory, and rejecting answers the call without running it.
#[tokio::test]
async fn a_patch_awaiting_approval_resumes_as_a_custom_call() {
    for approve in [true, false] {
        let (_directory, root, session) = live_session().await;
        std::fs::create_dir_all(root.join("tasks/patch")).unwrap();
        let agent = || {
            sandbox_agent(
                "patcher",
                vec![Arc::new(Filesystem::new().with_configure_tools(
                    |toolset: &mut ra_tools::sandbox::filesystem::FilesystemToolSet| {
                        toolset.apply_patch_mut().set_needs_approval(true);
                    },
                ))],
            )
        };
        let model = ScriptedModel::new(vec![
            custom_call(
                "patch",
                "apply_patch",
                "*** Begin Patch\n*** Add File: notes.md\n+approved\n*** End Patch\n",
            ),
            answer("patch_message"),
        ]);

        let first = Runner::run(request(
            agent(),
            &model,
            "run-patch",
            in_session(&session, "tasks/patch"),
        ))
        .await
        .unwrap();
        let RunOutcome::Interrupted { items } = first.outcome() else {
            panic!("expected an approval interruption");
        };
        let RunItemKind::ToolApproval(approval) = items[0].kind() else {
            panic!("expected a tool approval");
        };
        assert_eq!(approval.kind(), ToolCallKind::Custom);
        assert!(!root.join("tasks/patch/notes.md").exists());

        let mut state = first.state().clone();
        if approve {
            state.approve(&items[0], false).unwrap();
        } else {
            state.reject(&items[0], false).unwrap();
        }
        let resumed = Runner::run(
            request(
                agent(),
                &model,
                "run-patch",
                in_session(&session, "tasks/patch"),
            )
            .with_state(state),
        )
        .await
        .unwrap();

        assert_eq!(resumed.final_text(), "done");
        let output = tool_output(resumed.new_items(), "patch");
        assert_eq!(output.kind(), ToolCallKind::Custom);
        assert_eq!(root.join("tasks/patch/notes.md").exists(), approve);
        if approve {
            assert_eq!(output.output()["blocks"][0]["text"], "Created notes.md");
        } else {
            assert!(output.is_error());
        }
        model.assert_complete();
        session.close().await.unwrap();
    }
}
