//! The shell capability run end to end: a sandbox agent given `Shell` works through the real local
//! backend, from the run's working directory, with the tools the session offers.
//!
//! Ported from the shell parts of the reference's `tests/sandbox/test_run_cwd.py`, plus an
//! interactive round trip the reference covers only below the runner.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
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
        CreateRequest, Manifest, SandboxAgentConfig, SandboxClient, SandboxSession,
        SandboxWorkspaceScope,
    },
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
    sandbox::SandboxRunConfig,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use ra_tools::sandbox::NeedsApproval;
use ra_tools::sandbox::shell::{Shell, ShellToolSet};
use ra_tools::sandbox::shell_tool::{
    ExecCommandArgs, ExecCommandTool, WriteStdinArgs, WriteStdinTool,
};
use serde_json::{Value, json};
use tokio::sync::Barrier;

/// One model turn, given the tool outputs the model has seen so far.
type Step = Box<dyn Fn(&[String]) -> ModelResponse + Send + Sync>;

/// A model that answers from a script, optionally meeting other runs before its first answer.
struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    outputs: Mutex<Vec<String>>,
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
            rendezvous,
        })
    }

    fn outputs(&self) -> Vec<String> {
        self.outputs.lock().unwrap().clone()
    }

    fn assert_complete(&self) {
        assert!(self.steps.lock().unwrap().is_empty(), "unused model steps");
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let first = self.outputs.lock().unwrap().is_empty();
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
    AgentSpec::builder()
        .id(AgentId::new(name))
        .name(name)
        .instructions("do the task")
        .sandbox(SandboxAgentConfig::new().with_capability(Arc::new(shell)))
        .build()
        .unwrap()
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

// The shell step of `test_concurrent_runs_scope_relative_paths_with_shared_live_session`; its
// `view_image` and `apply_patch` steps arrive with the filesystem capability.
#[tokio::test]
async fn concurrent_runs_on_one_session_keep_their_relative_paths_apart() {
    let (_directory, root, session) = live_session().await;
    let tasks = ["task-a", "task-b"];
    for task in tasks {
        let directory = root.join("tasks").join(task);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("seed.png"), task.as_bytes()).unwrap();
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
                answer(&format!("{task}_message")),
            ],
            Some(Arc::clone(&rendezvous)),
        );
        let run = Runner::run(request(
            shell_agent(task, Shell::new()),
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
        assert_eq!(
            std::fs::read(root.join("tasks").join(task).join("plot.png")).unwrap(),
            task.as_bytes()
        );
        model.assert_complete();
    }
    assert!(!root.join("plot.png").exists());
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
