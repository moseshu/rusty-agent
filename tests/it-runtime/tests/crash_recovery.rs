//! A recorded run whose process is killed outright, mid-run, is resumed from what its rollout holds.
//!
//! The run lives in a child process: this test binary run again with only
//! [`killed_process_child`] selected. It records into a rollout directory, settles a turn that
//! records a plan and one that changes a file, and is killed with `SIGKILL` while it waits on its
//! third model call. Nothing in the child gets to flush, shut down or record how the run ended, so
//! what the parent resumes is exactly what the rollout's per-record writes put on disk: the
//! message context, the plan's last state, the file change and the settled turns.

#![cfg(unix)]

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    event::{
        HostEventBody,
        file::{FileChangeKind, FileChangedEvent, FileEvent},
    },
    item::{
        CallId, InputItemNormalizer, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase,
        RunItem, RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{SessionId, rollout::RolloutRunEnd},
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use ra_session::{
    CreateThreadParams, ResumeThreadParams, ResumedThread, RolloutPayload, RolloutReader,
    RolloutRecord, RolloutSessionMeta, RolloutThreadDirectory, ThreadStore,
};
use ra_tools::update_plan::UpdatePlanTool;
use serde_json::{Value, json};

/// Where the child keeps its rollouts; set only for the child process.
const CHILD_DIR: &str = "RA_CRASH_RECOVERY_DIR";
/// Created by the child's model when it is called the third time, after two settled turns.
const WAITING: &str = "waiting-on-model";
const SESSION: &str = "session-crash";

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    /// Creates the file and never answers.
    SignalAndHang(PathBuf),
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Vec<ModelInputItem>>>,
}

struct ScriptedResolver(Arc<Script>);

impl ModelResolver for ScriptedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel(Arc<Script>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push(request.input().to_vec());
        let step = self.0.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::SignalAndHang(path)) => {
                std::fs::write(path, b"").unwrap();
                std::future::pending().await
            }
            None => Err(Error::caller("the model ran out of responses")),
        }
    }
}

/// A tool that changes a file, reporting it on the host event channel.
struct EditTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl EditTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("edit").unwrap(),
            schema: ToolSchema::new(
                "edit",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        if let Some(emitter) = context.event_emitter() {
            emitter.emit_file(FileEvent::Changed(FileChangedEvent::new(
                context.call_id().clone(),
                "src/lib.rs",
                FileChangeKind::Updated,
            )))?;
        }
        Ok(ToolOutput::text("edited src/lib.rs"))
    }
}

fn lead() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("lead")
        .instructions("lead")
        .tools(vec![
            Arc::new(UpdatePlanTool::new().unwrap()) as Arc<dyn Tool>,
            Arc::new(EditTool::new()),
        ])
        .build()
        .unwrap()
}

fn tool_call(id: &str, call_id: &str, name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )])
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn plan() -> Value {
    json!({"plan": [
        {"step": "read the module", "status": "completed"},
        {"step": "edit the module", "status": "in_progress"},
        {"step": "run the tests", "status": "pending"},
    ]})
}

fn request(script: &Arc<Script>, run_id: &str, input: Vec<ModelInputItem>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(lead()),
        Arc::new(ScriptedResolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        RunId::new(run_id),
        CancelScope::root(),
        input,
    )
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("rusty_agent_tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn records(path: &Path) -> Vec<RolloutRecord> {
    RolloutReader::open(path)
        .read_all()
        .await
        .unwrap_or_default()
}

fn has_output(records: &[RolloutRecord], call_id: &str) -> bool {
    records.iter().any(|record| {
        matches!(
            record.payload(),
            Ok(RolloutPayload::Item(item))
                if matches!(item.kind(), RunItemKind::ToolCallOutput(output)
                    if output.call_id().as_str() == call_id)
        )
    })
}

fn normalized(items: &[ModelInputItem]) -> Vec<ModelInputItem> {
    InputItemNormalizer::new()
        .normalize_model_items(items)
        .unwrap()
        .into_items()
}

// ---------------------------------------------------------------------------------------------
// The killed process
// ---------------------------------------------------------------------------------------------

/// The child: records a run that never ends into the directory [`CHILD_DIR`] names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs only as the child process of `a_run_killed_mid_flight_resumes_from_what_its_rollout_holds`"]
async fn killed_process_child() {
    let dir = PathBuf::from(std::env::var_os(CHILD_DIR).expect("set by the parent test"));
    let store = RolloutThreadDirectory::new(&dir);
    let recorder = store
        .create_thread_with(&CreateThreadParams::new(
            RolloutSessionMeta::new(SessionId::new(SESSION)).with_cwd("/work"),
        ))
        .await
        .unwrap();
    let script = Arc::new(Script::default());
    {
        let mut steps = script.steps.lock().unwrap();
        steps.push_back(Step::Respond(tool_call(
            "m-1",
            "plan-call",
            "update_plan",
            plan(),
        )));
        steps.push_back(Step::Respond(tool_call(
            "m-2",
            "edit-call",
            "edit",
            json!({}),
        )));
        steps.push_back(Step::SignalAndHang(dir.join(WAITING)));
    }
    let _ = Runner::run(
        request(
            &script,
            "run-1",
            vec![ModelInputItem::Message(Message::user("fix the module"))],
        )
        .with_rollout_recorder(recorder),
    )
    .await;
    unreachable!("the run waits on its model until the process is killed");
}

/// `kill -9` mid-run, then resume: the message context, the plan's last state, the file change
/// and the settled turns all come back, and the next run continues from them.
#[test]
fn a_run_killed_mid_flight_resumes_from_what_its_rollout_holds() {
    let dir = temp_dir("crash_recovery_killed");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "killed_process_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_DIR, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = RolloutThreadDirectory::new(&dir);
    let session_id = SessionId::new(SESSION);
    let path = store.rollout_path(&session_id).unwrap();

    // The child is killed once it waits on its third call and its second turn is on disk: its
    // writer task writes each record as it is recorded, with no barrier in between.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if dir.join(WAITING).exists() && has_output(&runtime.block_on(records(&path)), "edit-call")
        {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the child exited before it was killed: {status}");
        }
        assert!(Instant::now() < deadline, "timed out waiting for the child");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    runtime.block_on(async {
        // The dead process's writer lock went with it, so the thread is resumed at once.
        let resumed = ResumedThread::resume(&store, &ResumeThreadParams::new(session_id.clone()))
            .await
            .unwrap();
        let reconstruction = resumed.reconstruction();
        let last = reconstruction.last_run().unwrap();
        assert_eq!(last.run_id(), &RunId::new("run-1"));
        assert!(
            last.end().is_none(),
            "the process died before the run ended"
        );
        assert_eq!(
            reconstruction.turn_context().unwrap().model(),
            Some("canonical-model")
        );

        // The message context and the settled turns: the task, the plan, the edit.
        let history = reconstruction.history().to_vec();
        assert_eq!(
            history.first(),
            Some(&ModelInputItem::Message(Message::user("fix the module")))
        );
        let calls: Vec<(&str, &Value)> = history
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::ToolCall(call) => Some((call.name(), call.arguments())),
                _ => None,
            })
            .collect();
        // The plan's last state is the last plan call's arguments.
        let plan_value = plan();
        let empty = json!({});
        assert_eq!(calls, vec![("update_plan", &plan_value), ("edit", &empty)]);
        let outputs: Vec<&str> = history
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::ToolCallOutput(output) => Some(output.call_id().as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["plan-call", "edit-call"]);

        // The file the run changed is on its timeline.
        let stored = resumed.history().records().to_vec();
        let changed: Vec<String> = stored
            .iter()
            .filter_map(|record| match record.payload().unwrap() {
                RolloutPayload::Event(event) => match event.body() {
                    HostEventBody::File(FileEvent::Changed(changed)) => {
                        Some(changed.path().to_owned())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(changed, vec!["src/lib.rs".to_owned()]);

        // The next run continues from the history and records only what is new.
        let script = Arc::new(Script::default());
        script
            .steps
            .lock()
            .unwrap()
            .push_back(Step::Respond(final_message("m-3", "done")));
        let mut input = history.clone();
        input.push(ModelInputItem::Message(Message::user("carry on")));
        let result = Runner::run(
            request(&script, "run-2", input.clone())
                .with_rollout_recorder(Arc::clone(resumed.recorder()))
                .with_recorded_input(history.len()),
        )
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(script.requests.lock().unwrap()[0], normalized(&input));
        resumed.recorder().shutdown().await.unwrap();

        let after = records(&path).await;
        let rebuilt = ra_session::reconstruct_history(&after).unwrap();
        let last = rebuilt.last_run().unwrap();
        assert_eq!(last.run_id(), &RunId::new("run-2"));
        assert_eq!(
            last.end().map(|end| end.end()),
            Some(RolloutRunEnd::Completed)
        );
        assert!(rebuilt.history().starts_with(&history));
    });
}
