//! The seven lifecycle moments, the two scopes told about them, and what each callback is handed.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
    },
    lifecycle::{
        AgentEndInput, AgentStartInput, HandoffInput, LifecycleEvent, LifecycleHook,
        LifecycleScope, LlmEndInput, LlmStartInput, ToolEndInput, ToolStartInput,
    },
    model::{ApiProtocol, ModelSelector, ProviderKey},
    state::RunId,
    tool::{ToolOrigin, ToolOutput, ToolServices},
};
use serde_json::json;

/// A hook that overrides nothing, so every moment reaches the default.
struct Silent {
    name: String,
}

impl Silent {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
        })
    }
}

#[async_trait]
impl LifecycleHook for Silent {
    fn name(&self) -> &str {
        &self.name
    }
}

/// A hook that records which scope it was told as, so both halves stay distinguishable.
#[derive(Default)]
struct Counting {
    run_scoped: AtomicUsize,
    agent_scoped: AtomicUsize,
}

#[async_trait]
impl LifecycleHook for Counting {
    fn name(&self) -> &str {
        "counting"
    }

    async fn on_agent_start(
        &self,
        scope: LifecycleScope,
        _: &AgentStartInput<'_>,
    ) -> ra_core::error::Result<()> {
        match scope {
            LifecycleScope::Run => self.run_scoped.fetch_add(1, Ordering::SeqCst),
            _ => self.agent_scoped.fetch_add(1, Ordering::SeqCst),
        };
        Ok(())
    }
}

fn run_context() -> RunContext {
    RunContext::new(RunId::new("run-lifecycle"), &agent())
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .build()
        .expect("an agent with an identity and a name")
}

fn other_agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .build()
        .expect("an agent with an identity and a name")
}

fn origin(name: &str) -> ToolOrigin {
    ToolOrigin::new(name).expect("a tool identity")
}

fn selector() -> ModelSelector {
    ModelSelector::new(
        ProviderKey::new("test-provider"),
        Some("canonical-model".to_owned()),
        ApiProtocol::OpenAiResponses,
    )
}

fn delivered_item() -> RunItem {
    RunItem::new(
        ItemId::new("answer"),
        RunItemKind::Message(Message::assistant("all done", OutputPhase::Final)),
    )
}

// ---------------------------------------------------------------------------
// the vocabulary
// ---------------------------------------------------------------------------

#[test]
fn the_vocabulary_is_the_reference_implementations_seven_moments() {
    // The reference implementation's two classes declare the same set; the two spellings of the
    // agent bracket (`on_agent_start` / `on_start`) are one moment seen from the two scopes.
    assert_eq!(LifecycleEvent::ALL.len(), 7);
    let mut codes: Vec<&str> = LifecycleEvent::ALL
        .iter()
        .map(|event| event.code())
        .collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        [
            "agent_end",
            "agent_start",
            "handoff",
            "llm_end",
            "llm_start",
            "tool_end",
            "tool_start",
        ]
    );
    codes.dedup();
    assert_eq!(codes.len(), 7, "two moments must not share a code word");
}

#[test]
fn a_moment_displays_as_its_code_word() {
    for event in LifecycleEvent::ALL {
        assert_eq!(event.to_string(), event.code());
    }
}

#[test]
fn the_two_scopes_have_distinct_code_words_so_a_report_can_tell_them_apart() {
    assert_eq!(LifecycleScope::Run.code(), "run");
    assert_eq!(LifecycleScope::Agent.code(), "agent");
    assert_ne!(LifecycleScope::Run.code(), LifecycleScope::Agent.code());
    assert_eq!(LifecycleScope::Agent.to_string(), "agent");
}

// ---------------------------------------------------------------------------
// where a hook is declared
// ---------------------------------------------------------------------------

#[test]
fn an_agent_carries_its_hooks_and_a_derived_builder_keeps_them() {
    // The hooks travel with the agent, so one reached by a handoff brings its own.
    let spec = AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .lifecycle_hook(Silent::new("metrics"))
        .build()
        .expect("an agent that narrates");
    assert_eq!(spec.lifecycle_hooks().len(), 1);
    assert_eq!(spec.lifecycle_hooks()[0].name(), "metrics");

    let derived = spec
        .to_builder()
        .build()
        .expect("a variant of a narrating agent");
    assert_eq!(derived.lifecycle_hooks().len(), 1);

    let cleared = spec
        .to_builder()
        .clear_lifecycle_hooks()
        .build()
        .expect("an agent that narrates nothing");
    assert!(cleared.lifecycle_hooks().is_empty());
}

#[test]
fn two_hooks_may_share_a_display_name_because_nothing_looks_one_up_by_it() {
    // The same rule a run-level guardrail and a host hook follow. An earlier draft refused the
    // second one; refusing a configuration the reference implementation accepts is a reason to
    // record more, not to reject it.
    let spec = AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .lifecycle_hooks([
            Silent::new("metrics") as Arc<dyn LifecycleHook>,
            Silent::new("metrics") as Arc<dyn LifecycleHook>,
        ])
        .build()
        .expect("two hooks under one display name are both installed");
    assert_eq!(spec.lifecycle_hooks().len(), 2);
}

#[test]
fn an_agents_debug_names_its_hooks_without_printing_them() {
    let spec = AgentSpec::builder()
        .id(AgentId::new("lifecycle-agent"))
        .name("Lifecycle agent")
        .lifecycle_hook(Silent::new("metrics"))
        .build()
        .expect("an agent that narrates");
    assert!(format!("{spec:?}").contains("lifecycle_hooks: [\"metrics\"]"));
}

// ---------------------------------------------------------------------------
// what a callback is handed
// ---------------------------------------------------------------------------

#[test]
fn an_activation_reports_what_the_segment_started_from_and_whether_it_continues_a_run() {
    let run = run_context();
    let input = vec![ModelInputItem::Message(Message::user("改一下文件"))];

    let fresh = AgentStartInput::new(&run, &input);
    assert_eq!(fresh.input().len(), 1);
    assert!(
        !fresh.is_resumed(),
        "a run that nobody continued is not a continuation"
    );
    assert_eq!(fresh.run().run_id().as_str(), "run-lifecycle");
    assert_eq!(fresh.agent().id().as_str(), "lifecycle-agent");

    // A resumed segment raises the moment again, because the hook object is new in this process.
    assert!(AgentStartInput::new(&run, &input).resumed().is_resumed());
}

#[test]
fn an_ending_carries_the_reason_and_the_answer_that_was_delivered() {
    let run = run_context();
    let message = Message::assistant("all done", OutputPhase::Final);

    let completed = AgentEndInput::new(&run, FinishReason::Final).with_message(&message);
    assert_eq!(completed.finish_reason(), FinishReason::Final);
    assert_eq!(
        completed.message().map(Message::text_content).as_deref(),
        Some("all done")
    );
    assert_eq!(completed.agent().name(), "Lifecycle agent");

    // A run that promoted a tool result to its answer has no assistant message to carry, and the
    // reason is what tells that apart from a model that said nothing.
    let promoted = AgentEndInput::new(&run, FinishReason::ToolStop);
    assert!(promoted.message().is_none());
    assert_eq!(promoted.finish_reason(), FinishReason::ToolStop);
}

#[test]
fn a_model_call_reports_the_resolved_model_and_the_content_the_model_will_read() {
    let run = run_context();
    let selector = selector();
    let input = vec![ModelInputItem::Message(Message::user("改一下文件"))];

    let calling = LlmStartInput::new(&run, &selector, &input);
    assert_eq!(calling.model().model(), Some("canonical-model"));
    assert_eq!(calling.input().len(), 1);
    assert_eq!(
        calling.system_instructions(),
        None,
        "a request without instructions says so rather than inventing empty ones"
    );

    let with_instructions = calling.with_system_instructions(Some("do the thing"));
    assert_eq!(
        with_instructions.system_instructions(),
        Some("do the thing")
    );
}

#[test]
fn a_finished_model_call_reports_the_response_it_produced() {
    let run = run_context();
    let selector = selector();
    let response = ModelResponse::new(vec![delivered_item()]);

    let answered = LlmEndInput::new(&run, &selector, &response);
    assert_eq!(answered.response().output().len(), 1);
    assert_eq!(answered.model().provider().as_str(), "test-provider");
}

#[test]
fn the_two_tool_moments_describe_one_call_the_same_way() {
    let run = run_context();
    let origin = origin("apply_patch");
    let call_id = CallId::new("call-1");
    let arguments = json!({"path": "src/lib.rs"});

    let started = ToolStartInput::new(&run, &origin, &call_id, &arguments);
    let output = Ok(ToolOutput::text("raw output"));
    let ended = ToolEndInput::new(started, &output);

    assert_eq!(
        ended.output().expect("raw tool output").as_text(),
        Some("raw output")
    );
    assert_eq!(ended.origin().qualified_name(), "apply_patch");
    assert_eq!(ended.call_id(), started.call_id());
    assert_eq!(ended.arguments(), started.arguments());
    assert_eq!(ended.call().origin(), started.origin());
}

#[test]
fn a_settled_call_preserves_the_raw_invocation_error() {
    // Failure handling has not replaced the invocation error with model-facing text.
    let run = run_context();
    let origin = origin("run_tests");
    let call_id = CallId::new("call-1");
    let arguments = json!({});
    let output = Ok(ToolOutput::text("raw output"));
    let started = ToolStartInput::new(&run, &origin, &call_id, &arguments);

    assert_eq!(ToolEndInput::new(started, &output).failure_code(), None);
    let failure = Err(ra_core::error::Error::tool(
        ra_core::error::ToolErrorKind::ExecutionFailed,
        "run_tests",
        "raw failure",
    ));
    let ended = ToolEndInput::new(started, &failure);
    assert_eq!(ended.failure_code(), Some("tool.execution_failed"));
    assert!(
        ended
            .output()
            .expect_err("the raw error")
            .to_string()
            .contains("raw failure")
    );
}

#[test]
fn a_transfer_names_both_sides_and_hands_over_neither_declaration() {
    // Neither side is handed an `AgentSpec`: it reaches transport headers and the other agent's
    // executable tools. The target arrives as a declaration and is projected inside the input.
    let run = run_context();
    let arriving = other_agent();

    let transfer = HandoffInput::new(&run, &arriving);
    assert_eq!(transfer.from().id().as_str(), "lifecycle-agent");
    assert_eq!(transfer.from().name(), "Lifecycle agent");
    assert_eq!(transfer.to().id().as_str(), "reviewer");
    assert_eq!(transfer.to().name(), "Reviewer");
}

#[test]
fn every_input_starts_with_no_ports_installed_and_takes_the_ones_it_is_given() {
    let run = run_context();
    let services = ToolServices::new();
    let input: Vec<ModelInputItem> = Vec::new();
    let selector = selector();
    let arriving = other_agent();

    assert!(std::ptr::eq(
        AgentStartInput::new(&run, &input).services(),
        ToolServices::none()
    ));
    assert!(std::ptr::eq(
        AgentStartInput::new(&run, &input)
            .with_services(&services)
            .services(),
        &services
    ));
    assert!(std::ptr::eq(
        AgentEndInput::new(&run, FinishReason::Final)
            .with_services(&services)
            .services(),
        &services
    ));
    assert!(std::ptr::eq(
        LlmStartInput::new(&run, &selector, &input)
            .with_services(&services)
            .services(),
        &services
    ));
    assert!(std::ptr::eq(
        HandoffInput::new(&run, &arriving)
            .with_services(&services)
            .services(),
        &services
    ));
}

// ---------------------------------------------------------------------------
// the contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_hook_that_overrides_nothing_succeeds_at_every_moment() {
    // Every default has to be one a host reaches by not caring about that moment, which for a
    // family that decides nothing means doing nothing and saying so.
    let hooks = Silent::new("metrics");
    let run = run_context();
    let selector = selector();
    let origin = origin("apply_patch");
    let call_id = CallId::new("call-1");
    let arguments = json!({});
    let output = Ok(ToolOutput::text("raw output"));
    let response = ModelResponse::new(Vec::new());
    let input: Vec<ModelInputItem> = Vec::new();
    let arriving = other_agent();
    let started = ToolStartInput::new(&run, &origin, &call_id, &arguments);
    let scope = LifecycleScope::Run;

    hooks
        .on_agent_start(scope, &AgentStartInput::new(&run, &input))
        .await
        .expect("the default changes nothing");
    hooks
        .on_agent_end(scope, &AgentEndInput::new(&run, FinishReason::Final))
        .await
        .expect("the default changes nothing");
    hooks
        .on_llm_start(scope, &LlmStartInput::new(&run, &selector, &input))
        .await
        .expect("the default changes nothing");
    hooks
        .on_llm_end(scope, &LlmEndInput::new(&run, &selector, &response))
        .await
        .expect("the default changes nothing");
    hooks
        .on_tool_start(scope, &started)
        .await
        .expect("the default changes nothing");
    hooks
        .on_tool_end(scope, &ToolEndInput::new(started, &output))
        .await
        .expect("the default changes nothing");
    hooks
        .on_handoff(scope, &HandoffInput::new(&run, &arriving))
        .await
        .expect("the default changes nothing");
}

#[tokio::test]
async fn the_scope_reaches_the_callback_so_one_object_can_serve_both_installations() {
    // Without it, a metrics collector installed on the run and on an agent reports both streams
    // under one name and every per-agent number silently includes the run-wide one.
    let hooks = Arc::new(Counting::default());
    let run = run_context();
    let input: Vec<ModelInputItem> = Vec::new();
    let starting = AgentStartInput::new(&run, &input);

    hooks
        .on_agent_start(LifecycleScope::Run, &starting)
        .await
        .expect("an observer changes nothing");
    hooks
        .on_agent_start(LifecycleScope::Agent, &starting)
        .await
        .expect("an observer changes nothing");

    assert_eq!(hooks.run_scoped.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.agent_scoped.load(Ordering::SeqCst), 1);
}
