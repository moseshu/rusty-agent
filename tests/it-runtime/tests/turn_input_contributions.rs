//! New input is contributed once, apart from session history and resumable model preparation.

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::{Capability, CapabilityFamily},
    error::{Error, Result},
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::Session,
    state::{RunId, RunState},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{PendingInput, RunConfig, RunRequest, Runner},
};
use ra_session::InMemorySession;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

fn input(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}
fn texts(items: &[ModelInputItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ModelInputItem::Message(m) => Some(
                m.content()
                    .iter()
                    .filter_map(|c| c.as_text())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}
struct Contributor {
    seen: Mutex<Vec<Vec<ModelInputItem>>>,
    fail: AtomicBool,
}
impl Contributor {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(vec![]),
            fail: AtomicBool::new(fail),
        })
    }
}
#[async_trait]
impl Capability for Contributor {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SKILLS
    }
    fn tools(&self) -> Vec<Arc<dyn ra_core::tool::Tool>> {
        vec![]
    }
    async fn contribute_turn_input(&self, input: &[ModelInputItem]) -> Result<Vec<Message>> {
        self.seen.lock().unwrap().push(input.to_vec());
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(Error::caller("preparation failed"));
        }
        Ok(texts(input)
            .into_iter()
            .map(|text| Message::user(format!("context for {text}")))
            .collect())
    }
}
struct Capture {
    seen: Mutex<Vec<Vec<ModelInputItem>>>,
    fail: AtomicBool,
    pending: Option<PendingInput>,
}
#[async_trait]
impl Model for Capture {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let first = self.seen.lock().unwrap().is_empty();
        self.seen.lock().unwrap().push(request.input().to_vec());
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(Error::caller("model failed")
                .with_run_state(RunState::start(RunId::new("contribute"))));
        }
        if first && let Some(pending) = &self.pending {
            pending.push(vec![Message::user("follow-up")]).unwrap();
        }
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new(if first { "first" } else { "second" }),
            RunItemKind::Message(Message::assistant("answer", OutputPhase::Final)),
        )]))
    }
}
struct Resolver(Arc<Capture>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}
fn capture(fail: bool, pending: Option<PendingInput>) -> Arc<Capture> {
    Arc::new(Capture {
        seen: Mutex::new(vec![]),
        fail: AtomicBool::new(fail),
        pending,
    })
}
fn request(model: Arc<Capture>, cap: Arc<Contributor>, initial: Vec<ModelInputItem>) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model)),
        RunId::new("contribute"),
        CancelScope::root(),
        initial,
    )
    .with_config(RunConfig::new().with_capability(cap))
}

#[tokio::test]
async fn session_history_is_not_reselected_and_contributions_are_saved_as_user_messages() {
    let session = Arc::new(InMemorySession::new_with_items(
        "history",
        vec![RunItem::new(
            ItemId::new("old"),
            RunItemKind::Message(Message::user("old input")),
        )],
    ));
    let model = capture(false, None);
    let cap = Contributor::new(false);
    let result = Runner::run(
        request(model.clone(), cap.clone(), vec![input("new input")]).with_session(session.clone()),
    )
    .await
    .unwrap();
    assert_eq!(*cap.seen.lock().unwrap(), vec![vec![input("new input")]]);
    assert_eq!(
        texts(&model.seen.lock().unwrap()[0]),
        ["old input", "new input", "context for new input"]
    );
    let saved = session.get_items(None).await.unwrap();
    assert_eq!(
        saved
            .iter()
            .filter_map(RunItem::to_model_input)
            .filter(|item| *item == input("context for new input"))
            .count(),
        1
    );
    assert!(result.state().pending_input_contributions().is_empty());
}

#[tokio::test]
async fn pending_input_contributes_at_its_delivery_boundary_once() {
    let pending = PendingInput::new();
    let model = capture(false, Some(pending.clone()));
    let cap = Contributor::new(false);
    let result = Runner::run(
        request(model.clone(), cap.clone(), vec![input("initial")]).with_pending_input(pending),
    )
    .await
    .unwrap();
    assert_eq!(
        *cap.seen.lock().unwrap(),
        vec![vec![input("initial")], vec![input("follow-up")]]
    );
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        texts(&seen[1])
            .iter()
            .filter(|text| text.as_str() == "context for initial")
            .count(),
        1
    );
    assert_eq!(
        texts(&seen[1])
            .iter()
            .filter(|text| text.as_str() == "context for follow-up")
            .count(),
        1
    );
    assert!(result.state().pending_input_contributions().is_empty());
}

#[tokio::test]
async fn failed_contribution_retains_new_input_across_serialized_resume() {
    let model = capture(false, None);
    let cap = Contributor::new(true);
    let error = match Runner::run(request(model.clone(), cap.clone(), vec![input("initial")])).await
    {
        Err(e) => e,
        Ok(_) => panic!("expected preparation failure"),
    };
    assert!(model.seen.lock().unwrap().is_empty());
    let state: RunState =
        serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap()).unwrap();
    assert_eq!(state.pending_input_contributions(), [input("initial")]);
    let result = Runner::run(request(model.clone(), cap.clone(), vec![]).with_state(state))
        .await
        .unwrap();
    assert_eq!(cap.seen.lock().unwrap().len(), 2);
    assert_eq!(
        texts(&model.seen.lock().unwrap()[0]),
        ["initial", "context for initial"]
    );
    assert!(result.state().pending_input_contributions().is_empty());
}

#[tokio::test]
async fn checkpointed_model_failure_does_not_reinject_committed_contributions_on_resume() {
    let model = capture(true, None);
    let cap = Contributor::new(false);
    let session = Arc::new(InMemorySession::new("resume"));
    let error = match Runner::run(
        request(model.clone(), cap.clone(), vec![input("initial")]).with_session(session.clone()),
    )
    .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected model failure"),
    };
    let state: RunState =
        serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap()).unwrap();
    assert!(state.pending_input_contributions().is_empty());
    Runner::run(
        request(model.clone(), cap.clone(), vec![])
            .with_state(state)
            .with_session(session),
    )
    .await
    .unwrap();
    assert_eq!(
        *cap.seen.lock().unwrap(),
        vec![vec![input("initial")], vec![]]
    );
    let seen = model.seen.lock().unwrap();
    assert_eq!(texts(&seen[0]), texts(&seen[1]));
}

struct RawInputCheck;
#[async_trait]
impl ra_core::guardrail::InputGuardrail for RawInputCheck {
    fn name(&self) -> &str {
        "raw_input"
    }
    fn run_in_parallel(&self) -> bool {
        false
    }
    async fn check(
        &self,
        _: &ra_core::context::RunContext,
        input: &[ModelInputItem],
    ) -> Result<ra_core::guardrail::GuardrailFunctionOutput> {
        assert_eq!(texts(input), ["initial"]);
        Ok(ra_core::guardrail::GuardrailFunctionOutput::pass())
    }
}
#[tokio::test]
async fn sdk_input_guardrails_receive_raw_input_before_contextual_messages() {
    let model = capture(false, None);
    let cap = Contributor::new(false);
    let request = request(model.clone(), cap.clone(), vec![input("initial")]).with_config(
        RunConfig::new()
            .with_capability(cap)
            .with_input_guardrail(Arc::new(RawInputCheck)),
    );
    Runner::run(request).await.unwrap();
    assert_eq!(
        texts(&model.seen.lock().unwrap()[0]),
        ["initial", "context for initial"]
    );
}

#[tokio::test]
async fn session_callbacks_cannot_trigger_context_for_input_they_dropped() {
    let model = capture(false, None);
    let cap = Contributor::new(false);
    let callback = Arc::new(
        |history: &mut Vec<RunItem>, _: &mut Vec<RunItem>| -> Result<Vec<RunItem>> {
            Ok(history.clone())
        },
    );
    let request = request(model.clone(), cap.clone(), vec![input("dropped input")])
        .with_config(
            RunConfig::new()
                .with_capability(cap.clone())
                .with_session_input_callback(callback),
        )
        .with_session(Arc::new(InMemorySession::new("filtered")));
    Runner::run(request).await.unwrap();
    assert_eq!(*cap.seen.lock().unwrap(), vec![vec![]]);
    assert!(model.seen.lock().unwrap()[0].is_empty());
}

struct CancelContributor {
    cancel: CancelScope,
}
#[async_trait]
impl Capability for CancelContributor {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SKILLS
    }
    fn tools(&self) -> Vec<Arc<dyn ra_core::tool::Tool>> {
        vec![]
    }
    async fn contribute_turn_input(&self, _: &[ModelInputItem]) -> Result<Vec<Message>> {
        self.cancel
            .cancel(ra_core::cancel::CancelReason::UserInterrupt);
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancellation_during_contribution_keeps_accepted_input_for_resume() {
    let model = capture(false, None);
    let cancel = CancelScope::root();
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let cancelled_request = RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model.clone())),
        RunId::new("contribute"),
        cancel.clone(),
        vec![input("initial")],
    )
    .with_config(RunConfig::new().with_capability(Arc::new(CancelContributor { cancel })));
    let error = match Runner::run(cancelled_request).await {
        Err(error) => error,
        Ok(_) => panic!("expected cancellation"),
    };
    let state: RunState =
        serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap()).unwrap();
    assert_eq!(state.pending_input_contributions(), [input("initial")]);
    assert!(model.seen.lock().unwrap().is_empty());
    Runner::run(request(model.clone(), Contributor::new(false), vec![]).with_state(state))
        .await
        .unwrap();
    assert_eq!(
        texts(&model.seen.lock().unwrap()[0]),
        ["initial", "context for initial"]
    );
}
