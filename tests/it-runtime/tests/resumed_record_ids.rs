//! Records the runner names by the run's turn stay unique when a run continues after an approval.
//!
//! A context processor's records, a stop hook's continuation and the other records numbered by the
//! run's turn must not repeat an identity an earlier segment of the same run used: a duplicate
//! either overwrites history or, for context records, fails the run. The numbering must not depend
//! on optional features such as context filters.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::{
        ContextProcessor, ContextProcessorRequest, ContextProcessorResult, ContextSummarizer,
    },
    error::{Error, Result},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{
        CallId, ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    hook::UserHookRegistration,
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
};
use serde_json::json;

/// Answers from a fixed script shared by every segment of the run.
struct Scripted(Arc<Mutex<Vec<ModelResponse>>>);

#[async_trait]
impl Model for Scripted {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        let mut script = self.0.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("the model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

struct Resolver(Arc<Mutex<Vec<ModelResponse>>>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(Scripted(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// A tool that only runs once approved.
struct Guarded {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl Guarded {
    fn shared() -> Arc<dyn Tool> {
        Arc::new(Self {
            origin: ToolOrigin::new("guarded").unwrap(),
            schema: ToolSchema::new(
                "guarded",
                json!({"type": "object", "properties": {}, "required": [],
                       "additionalProperties": false}),
            )
            .unwrap(),
        })
    }
}

#[async_trait]
impl Tool for Guarded {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
    }

    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("done"))
    }
}

/// Records one note under the identity the runner reserved, every time it runs.
struct NoteEachTurn;

#[async_trait]
impl ContextProcessor for NoteEachTurn {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        Ok(
            ContextProcessorResult::new(request.input().to_vec()).with_generated_items(vec![
                RunItem::new(
                    request.record_id().clone(),
                    RunItemKind::Message(Message::developer(format!(
                        "note for {}",
                        request.record_id()
                    ))),
                ),
            ]),
        )
    }
}

/// Continues the answers `first` and `second`, so both segments leave a continuation record.
#[derive(Default)]
struct ContinueOnce(Mutex<usize>);

#[async_trait]
impl UserHook for ContinueOnce {
    fn name(&self) -> &str {
        "continue-once"
    }

    async fn call(&self, _: &UserHookContext<'_>, event: &HookEvent<'_>) -> Result<HookDecision> {
        let HookEvent::Stop(delivery) = event else {
            return Ok(HookDecision::Continue);
        };
        let answer = delivery.message().map(Message::text_content);
        if !matches!(answer.as_deref(), Some("first" | "second")) {
            return Ok(HookDecision::Continue);
        }
        *self.0.lock().unwrap() += 1;
        Ok(HookDecision::Block {
            prompt: "check once more".to_owned(),
        })
    }
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn guarded_call(id: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), "guarded", json!({}))),
    )])
}

fn request(script: &Arc<Mutex<Vec<ModelResponse>>>, config: RunConfig) -> RunRequest {
    request_with(
        script,
        config,
        vec![ModelInputItem::Message(Message::user("go"))],
    )
}

/// A continuation from a checkpoint, with no input of its own, as a thread continues its turn.
fn resumed(
    script: &Arc<Mutex<Vec<ModelResponse>>>,
    config: RunConfig,
    state: RunState,
) -> RunRequest {
    request_with(script, config, Vec::new()).with_state(state)
}

fn request_with(
    script: &Arc<Mutex<Vec<ModelResponse>>>,
    config: RunConfig,
    input: Vec<ModelInputItem>,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tool(Guarded::shared())
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        CancelScope::root(),
        input,
    )
    .with_config(config)
}

/// Runs a segment that must stop for approval, approves the call and returns the checkpoint, as
/// a host does between segments.
fn approved(result: &RunResult) -> RunState {
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("expected an approval, got {:?}", result.outcome());
    };
    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(result.state()).unwrap()).unwrap();
    for item in items {
        state.approve(item, false).unwrap();
    }
    state
}

fn ids(state: &RunState) -> Vec<String> {
    state
        .generated_items()
        .iter()
        .map(|item| item.id().to_string())
        .collect()
}

fn assert_unique(ids: &[String]) {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        assert!(seen.insert(id), "record `{id}` repeats: {ids:?}");
    }
}

/// Without context filters, a context processor's record in the segment that continues after an
/// approval gets an identity the first segment did not use, and the run completes.
#[tokio::test]
async fn context_records_stay_unique_across_approval_resumes() {
    let script = Arc::new(Mutex::new(vec![
        guarded_call("call-1"),
        guarded_call("call-2"),
        final_message("answer", "done"),
    ]));
    let config = || RunConfig::new().with_context_processor(Arc::new(NoteEachTurn));

    let first = Runner::run(request(&script, config())).await.unwrap();
    let second = Runner::run(resumed(&script, config(), approved(&first)))
        .await
        .unwrap();
    let third = Runner::run(resumed(&script, config(), approved(&second)))
        .await
        .unwrap();

    assert!(
        matches!(third.outcome(), RunOutcome::Completed { .. }),
        "{:?}",
        third.outcome()
    );
    let ids = ids(third.state());
    assert_eq!(
        ids.iter().filter(|id| id.starts_with("context-")).count(),
        3,
        "{ids:?}"
    );
    assert_unique(&ids);
}

/// A stop hook's continuation in the segment that continues after an approval does not reuse the
/// continuation identity of an earlier segment.
#[tokio::test]
async fn continuation_records_stay_unique_across_approval_resumes() {
    let script = Arc::new(Mutex::new(vec![
        final_message("answer-1", "first"),
        guarded_call("call-1"),
        final_message("answer-2", "second"),
        final_message("answer-3", "third"),
    ]));
    let hook = Arc::new(ContinueOnce::default());
    let config = || {
        RunConfig::new().with_user_hook(UserHookRegistration::new(
            HookEventName::Stop,
            Arc::clone(&hook) as Arc<dyn UserHook>,
        ))
    };

    // The first stop is continued; the model then asks for the guarded tool.
    let first = Runner::run(request(&script, config())).await.unwrap();
    let second = Runner::run(resumed(&script, config(), approved(&first)))
        .await
        .unwrap();

    assert!(
        matches!(second.outcome(), RunOutcome::Completed { .. }),
        "{:?}",
        second.outcome()
    );
    let continuations: Vec<_> = second
        .state()
        .generated_items()
        .iter()
        .filter(|item| {
            matches!(item.kind(), RunItemKind::Message(message)
                if message.role() == MessageRole::User && message.text_content() == "check once more")
        })
        .map(|item| item.id().to_string())
        .collect();
    assert_eq!(continuations.len(), 2, "{continuations:?}");
    assert_unique(&ids(second.state()));
}
