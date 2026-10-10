//! The state a tool keeps with its approval: kept with an approval asked before the call, or with
//! one the tool asks for from inside its invocation, carried through a serialized checkpoint, and
//! handed back when the approved call resumes. A call that asked from inside its invocation
//! continues it: it is not announced again, and a rejected one is not invoked again.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall, ToolResumePoint},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::{Value, json};

struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
        })
    }

    fn next(&self) -> Result<ModelResponse> {
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next()
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next()
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// How a [`ResumingTool`] behaves.
#[derive(Clone, Copy)]
enum Mode {
    /// Asks before the call and keeps a decision with the approval.
    KeepsDecision,
    AsksBeforeAndDuring,
    /// Requests continuation and fails after its approved body begins.
    FailsDuringContinuation,
    /// Returns an ordinary failure with a resumable checkpoint after approval.
    ReportsCheckpointFailure,
    /// Timeout handling remains independent from ordinary failure handling.
    ReportsCheckpointTimeout,
    /// Starts, then asks from inside the invocation; runs the rest once resumed.
    AsksDuringCall,
}

struct ResumingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    mode: Mode,
    /// The resume state each invocation was handed.
    invocations: Mutex<Vec<Option<Value>>>,
}

impl ResumingTool {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("staged").unwrap(),
            schema: ToolSchema::new(
                "staged",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            mode,
            invocations: Mutex::new(Vec::new()),
        })
    }

    fn invocations(&self) -> Vec<Option<Value>> {
        self.invocations.lock().unwrap().clone()
    }
}

#[async_trait]
impl Tool for ResumingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        let handling = if matches!(self.mode, Mode::ReportsCheckpointFailure) {
            ra_core::tool::ToolFailureHandling::ModelVisible
        } else {
            ra_core::tool::ToolFailureHandling::Propagate
        };
        ToolOptions::new()
            .with_approval(ToolApprovalPolicy::Dynamic)
            .with_failure_handling(handling)
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(matches!(
            self.mode,
            Mode::KeepsDecision | Mode::AsksBeforeAndDuring
        ))
    }

    async fn approval_state(&self, _context: &ToolContext<'_>) -> Result<Option<Value>> {
        Ok(Some(json!({"decided": "sandboxed"})))
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let resume = context.resume_state().cloned();
        self.invocations.lock().unwrap().push(resume.clone());
        match (self.mode, resume) {
            (Mode::AsksBeforeAndDuring, Some(state)) if state.get("decided").is_some() => {
                Err(ToolApprovalRequest::new(json!({"stage": "second"})).into_error("staged"))
            }
            (Mode::ReportsCheckpointTimeout, Some(_)) => Err(Error::tool(
                ra_core::error::ToolErrorKind::Timeout,
                "staged",
                "checkpointed timeout",
            )
            .with_run_state(RunState::start(context.run().run_id().clone()))),
            (Mode::ReportsCheckpointFailure, Some(_)) => {
                Err(Error::caller("ordinary checkpointed failure")
                    .with_run_state(RunState::start(context.run().run_id().clone())))
            }
            (Mode::FailsDuringContinuation, Some(_)) => {
                Err(Error::caller("continuation failed after execution"))
            }
            (
                Mode::AsksDuringCall
                | Mode::FailsDuringContinuation
                | Mode::ReportsCheckpointFailure
                | Mode::ReportsCheckpointTimeout,
                None,
            ) => Err(ToolApprovalRequest::new(
                json!({"stage": "retry", "first_attempt": "refused"}),
            )
            .into_error("staged")),
            (_, resume) => Ok(ToolOutput::text(format!("ran with {resume:?}"))),
        }
    }
}

/// Records tool starts and ends.
#[derive(Default)]
struct Narration(Mutex<Vec<String>>);

#[async_trait]
impl LifecycleHook for Narration {
    fn name(&self) -> &str {
        "narration"
    }

    async fn on_tool_start(
        &self,
        _scope: LifecycleScope,
        _input: &ToolStartInput<'_>,
    ) -> Result<()> {
        self.0.lock().unwrap().push("start".to_owned());
        Ok(())
    }

    async fn on_tool_end(&self, _scope: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("end/{}", input.failure_code().unwrap_or("ok")));
        Ok(())
    }
}

fn request(
    tool: Arc<ResumingTool>,
    model: Arc<ScriptedModel>,
    narration: &Arc<Narration>,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("do the thing")
        .tools(vec![tool as Arc<dyn Tool>])
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model)),
        RunId::new("run-resume"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_config(
        RunConfig::new().with_lifecycle_hook(Arc::clone(narration) as Arc<dyn LifecycleHook>),
    )
}

fn calls_staged() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "staged", json!({}))),
    )])
}

fn finishes() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(ra_core::item::Message::assistant(
            "done",
            ra_core::item::OutputPhase::Final,
        )),
    )])
}

/// Runs the first segment to its interruption and returns the checkpoint, written to JSON and
/// read back as a host restoring it in another process would.
async fn interrupted(tool: &Arc<ResumingTool>, narration: &Arc<Narration>) -> (RunState, RunItem) {
    let result = Runner::run(request(
        Arc::clone(tool),
        ScriptedModel::new(vec![calls_staged()]),
        narration,
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("expected an approval, got {:?}", result.outcome());
    };
    let [item] = items.as_slice() else {
        panic!("expected one approval");
    };
    let json = serde_json::to_string(result.state()).unwrap();
    (serde_json::from_str(&json).unwrap(), item.clone())
}

fn resume_of(item: &RunItem) -> (ToolResumePoint, Value) {
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected a tool approval");
    };
    let resume = approval
        .resume()
        .expect("the approval keeps the tool's state");
    (resume.point(), resume.state().clone())
}

/// The state a tool keeps with an approval asked before the call survives the checkpoint and is
/// handed to the approved call, which is dispatched as any approved call is.
#[tokio::test]
async fn a_decision_kept_with_an_approval_reaches_the_approved_call() {
    let tool = ResumingTool::new(Mode::KeepsDecision);
    let narration = Arc::new(Narration::default());
    let (mut state, item) = interrupted(&tool, &narration).await;
    assert_eq!(
        resume_of(&item),
        (ToolResumePoint::BeforeCall, json!({"decided": "sandboxed"}))
    );
    assert!(tool.invocations().is_empty());

    state.approve(&item, false).unwrap();
    let resumed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [Some(json!({"decided": "sandboxed"}))]);
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);
}

/// A tool that asks from inside its invocation leaves an approval with its state and no output;
/// once approved, the call continues that invocation with the state, announced only once.
#[tokio::test]
async fn a_call_that_asks_during_its_invocation_continues_once_approved() {
    let tool = ResumingTool::new(Mode::AsksDuringCall);
    let narration = Arc::new(Narration::default());
    let (mut state, item) = interrupted(&tool, &narration).await;
    let retry = json!({"stage": "retry", "first_attempt": "refused"});
    assert_eq!(
        resume_of(&item),
        (ToolResumePoint::DuringCall, retry.clone())
    );
    assert_eq!(tool.invocations(), [None]);
    assert!(
        !state
            .generated_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_))),
        "a call waiting for its approval has no output yet"
    );
    assert_eq!(*narration.0.lock().unwrap(), ["start"]);

    state.approve(&item, false).unwrap();
    let resumed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None, Some(retry)]);
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);
}

/// A rejected request from inside an invocation is answered with the rejection, and the tool is
/// not invoked again.
#[tokio::test]
async fn a_rejected_request_during_an_invocation_does_not_run_again() {
    let tool = ResumingTool::new(Mode::AsksDuringCall);
    let narration = Arc::new(Narration::default());
    let (mut state, item) = interrupted(&tool, &narration).await;

    state
        .reject_with_message(&item, false, "rejected by user")
        .unwrap();
    let resumed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();

    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None]);
    let output = resumed
        .state()
        .generated_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output.clone()),
            _ => None,
        })
        .expect("the rejection answers the call");
    assert_eq!(output.output(), &json!("rejected by user"));
}

#[tokio::test]
async fn before_call_approval_can_be_followed_by_during_call_approval() {
    let tool = ResumingTool::new(Mode::AsksBeforeAndDuring);
    let narration = Arc::new(Narration::default());
    let (mut state, first) = interrupted(&tool, &narration).await;
    state.approve(&first, false).unwrap();
    let resumed = Runner::run(
        request(Arc::clone(&tool), ScriptedModel::new(vec![]), &narration).with_state(state),
    )
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = resumed.outcome() else {
        panic!("expected a second approval");
    };
    let [second] = items.as_slice() else {
        panic!("expected one question");
    };
    assert_ne!(first.id(), second.id());
    assert_eq!(
        resume_of(second),
        (ToolResumePoint::DuringCall, json!({"stage": "second"}))
    );
    assert_eq!(*narration.0.lock().unwrap(), ["start"]);
    assert!(
        resumed
            .state()
            .pending_interruption_resolutions()
            .is_empty()
    );
    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(resumed.state()).unwrap()).unwrap();
    state.approve(second, false).unwrap();
    let completed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();
    assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        tool.invocations(),
        [
            Some(json!({"decided": "sandboxed"})),
            Some(json!({"stage": "second"}))
        ]
    );
    assert_eq!(*narration.0.lock().unwrap(), ["start", "end/ok"]);
}

#[tokio::test]
async fn subsequent_questions_preserve_other_answers_in_the_same_batch() {
    let tool = ResumingTool::new(Mode::AsksBeforeAndDuring);
    let narration = Arc::new(Narration::default());
    let two_calls = ModelResponse::new(vec![
        RunItem::new(
            ItemId::new("c-1"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "staged", json!({}))),
        ),
        RunItem::new(
            ItemId::new("c-2"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call-2"), "staged", json!({}))),
        ),
    ]);
    let interrupted = Runner::run(request(
        Arc::clone(&tool),
        ScriptedModel::new(vec![two_calls]),
        &narration,
    ))
    .await
    .unwrap();
    let RunOutcome::Interrupted { items: first } = interrupted.outcome() else {
        panic!("expected approval");
    };
    assert_eq!(first.len(), 2);
    let mut state = interrupted.state().clone();
    for item in first {
        state.approve(item, false).unwrap();
    }
    let resumed = Runner::run(
        request(Arc::clone(&tool), ScriptedModel::new(vec![]), &narration).with_state(state),
    )
    .await
    .unwrap();
    let RunOutcome::Interrupted { items: second } = resumed.outcome() else {
        panic!("expected subsequent approvals");
    };
    assert_eq!(second.len(), 2);
    assert!(
        second
            .iter()
            .all(|item| first.iter().all(|old| old.id() != item.id()))
    );
    assert!(
        resumed
            .state()
            .pending_interruption_resolutions()
            .is_empty()
    );
    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(resumed.state()).unwrap()).unwrap();
    for item in second {
        state.approve(item, false).unwrap();
    }
    let completed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();
    assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations().len(), 4);
    let narration = narration.0.lock().unwrap();
    assert_eq!(
        narration.iter().filter(|event| *event == "start").count(),
        2
    );
    assert_eq!(
        narration.iter().filter(|event| *event == "end/ok").count(),
        2
    );
}

#[tokio::test]
async fn a_rejected_second_question_settles_without_another_invocation() {
    let tool = ResumingTool::new(Mode::AsksBeforeAndDuring);
    let narration = Arc::new(Narration::default());
    let (mut state, first) = interrupted(&tool, &narration).await;
    state.approve(&first, false).unwrap();
    let resumed = Runner::run(
        request(Arc::clone(&tool), ScriptedModel::new(vec![]), &narration).with_state(state),
    )
    .await
    .unwrap();
    let RunOutcome::Interrupted { items } = resumed.outcome() else {
        panic!("expected second approval");
    };
    let mut state = resumed.state().clone();
    state
        .reject_with_message(&items[0], false, "stop here")
        .unwrap();
    let completed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_state(state),
    )
    .await
    .unwrap();
    assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations().len(), 1);
    let narration = narration.0.lock().unwrap();
    assert_eq!(narration.len(), 2);
    assert_eq!(narration[0], "start");
    assert!(narration[1].starts_with("end/"));
    assert!(completed.state().generated_items().iter().any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(output) if output.output() == &json!("stop here"))));
}

#[tokio::test]
async fn a_denied_continuation_closes_the_started_invocation() {
    use ra_core::permission::{PermissionDecision, PermissionRule};
    let tool = ResumingTool::new(Mode::AsksDuringCall);
    let narration = Arc::new(Narration::default());
    let (mut state, item) = interrupted(&tool, &narration).await;
    state.approve(&item, false).unwrap();
    let config = RunConfig::new()
        .with_lifecycle_hook(Arc::clone(&narration) as Arc<dyn LifecycleHook>)
        .with_permission_rules([
            PermissionRule::new(PermissionDecision::Deny).with_tool_name("staged")
        ]);
    let completed = Runner::run(
        request(
            Arc::clone(&tool),
            ScriptedModel::new(vec![finishes()]),
            &narration,
        )
        .with_config(config)
        .with_state(state),
    )
    .await
    .unwrap();
    assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(tool.invocations(), [None]);
    let narration = narration.0.lock().unwrap();
    assert_eq!(narration.len(), 2);
    assert_eq!(narration[0], "start");
    assert!(narration[1].starts_with("end/"));
}

#[tokio::test]
async fn a_failed_continuation_cannot_be_replayed_from_its_error_checkpoint() {
    let tool = ResumingTool::new(Mode::FailsDuringContinuation);
    let narration = Arc::new(Narration::default());
    let (mut state, item) = interrupted(&tool, &narration).await;
    state.approve(&item, false).unwrap();
    let error = Runner::run(
        request(Arc::clone(&tool), ScriptedModel::new(vec![]), &narration).with_state(state),
    )
    .await
    .unwrap_err();
    assert_eq!(tool.invocations().len(), 2);
    let failed = error
        .run_state()
        .expect("a failed resume carries its checkpoint");
    let restored: RunState = serde_json::from_str(&serde_json::to_string(failed).unwrap()).unwrap();
    let replacement = ResumingTool::new(Mode::FailsDuringContinuation);
    let error = Runner::run(
        request(
            Arc::clone(&replacement),
            ScriptedModel::new(vec![]),
            &narration,
        )
        .with_state(restored),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already executed"), "{error}");
    assert!(replacement.invocations().is_empty());
}

#[tokio::test]
async fn an_ordinary_checkpointed_tool_failure_uses_its_failure_policy() {
    for mode in [
        Mode::ReportsCheckpointFailure,
        Mode::ReportsCheckpointTimeout,
    ] {
        let tool = ResumingTool::new(mode);
        let narration = Arc::new(Narration::default());
        let (mut state, item) = interrupted(&tool, &narration).await;
        state.approve(&item, false).unwrap();
        let completed = Runner::run(
            request(
                Arc::clone(&tool),
                ScriptedModel::new(vec![finishes()]),
                &narration,
            )
            .with_state(state),
        )
        .await
        .unwrap();
        assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
        assert!(completed.state().nested_runs().is_empty());
        assert_eq!(tool.invocations().len(), 2);
        assert!(completed.state().generated_items().iter().any(
            |item| matches!(item.kind(), RunItemKind::ToolCallOutput(output) if output.is_error())
        ));
        let narration = narration.0.lock().unwrap();
        assert_eq!(narration.len(), 2);
        assert_eq!(narration[0], "start");
        assert!(narration[1].starts_with("end/"));
    }
}
