//! Real host assembly, backend dispatch and collaboration across live thread turns.

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;
#[path = "../../support/marked_sleep.rs"]
mod marked_sleep;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use marked_sleep::MarkedSleep;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_control, build_agent_with_profile,
    host_backed_surface,
};
use ra_core::{
    agent::{
        AgentId, AgentSpec,
        control::{AgentControlPort, AgentPath, AgentStatus},
    },
    cancel::CancelScope,
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer, SamplingContext,
    },
    context::RunContext,
    error::Result,
    item::{
        CallId, ContentBlock, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    prompt::{PromptRole, PromptSection, SectionPosition, SectionStability},
    session::SessionId,
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::{AgentBinding, control::AgentControl},
    runner::{RunRequest, Runner},
    thread::{AgentThread, ThreadEvent, TurnInputRequest},
};
use serde_json::{Value, json};
use tokio::{sync::Notify, time::timeout};

const PATIENCE: Duration = Duration::from_secs(10);

/// The namespace the collaboration tools are advertised in by default.
const COLLABORATION: &str = "collaboration";

#[derive(Clone)]
struct ProfileCapability {
    bound: bool,
    changes_declaration: bool,
    samples: Arc<Mutex<Vec<SamplingContext>>>,
}

impl ProfileCapability {
    fn section(&self, text: &str, deferred: bool) -> PromptSection {
        let family = self.kind();
        PromptSection::new(
            if deferred {
                family.deferred_prompt_section_name()
            } else {
                family.prompt_section_name()
            },
            "Profile behavior",
            family.prompt_source(),
            SectionStability::Stable,
            if deferred {
                SectionPosition::TailMessage
            } else {
                SectionPosition::Prefix
            },
            text,
        )
        .unwrap()
        .with_token_budget(128)
    }
}

#[async_trait]
impl Capability for ProfileCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("profile_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let name = if self.bound && self.changes_declaration {
            "changed_probe"
        } else {
            "profile_probe"
        };
        vec![Arc::new(ProfileTool {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            )
            .unwrap(),
            options: ToolOptions::new().with_action_classes([ra_tools::action_classes::EDIT]),
            bound: self.bound,
        })]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        Ok(Some(self.section(
            "Use `profile_probe` for the configured profile operation.",
            false,
        )))
    }

    fn bind(&self, _: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        Ok(Some(Arc::new(Self {
            bound: true,
            ..self.clone()
        })))
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        assert!(self.bound);
        Ok(Some(self.section("Profile runtime instructions.", false)))
    }

    async fn deferred_instructions(&self) -> Result<Option<PromptSection>> {
        assert!(self.bound);
        Ok(Some(self.section("Profile deferred instructions.", true)))
    }

    fn sampling_params_for(
        &self,
        settings: ModelSettings,
        context: &SamplingContext,
    ) -> ModelSettings {
        assert!(self.bound);
        self.samples.lock().unwrap().push(context.clone());
        settings.with_temperature(0.23)
    }

    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        Some(self)
    }
}

#[async_trait]
impl ContextProcessor for ProfileCapability {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        assert!(self.bound);
        let mut input = request.input().to_vec();
        input.push(ModelInputItem::Message(Message::user(
            "Profile context projection.",
        )));
        Ok(ContextProcessorResult::new(input))
    }
}

struct ProfileTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    bound: bool,
}

#[async_trait]
impl Tool for ProfileTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        assert!(
            self.bound,
            "the bound handler must replace the preinstalled handler"
        );
        Ok(ToolOutput::text("Profile bound tool result."))
    }
}

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    /// What spawned agents answer before their final message.
    child_responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    child_requests: Mutex<Vec<ModelRequest>>,
    hang_child: std::sync::atomic::AtomicBool,
    child_started: Notify,
    child_cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct ChildCall(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ChildCall {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Model for Script {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        // Fork-free children answer independently of the parent's scripted turn sequence.
        let child = request.input().iter().any(|item| {
            matches!(item, ModelInputItem::Message(message) if message.text_content().contains("worker task"))
        });
        if child {
            self.child_requests.lock().unwrap().push(request);
            if self.hang_child.load(std::sync::atomic::Ordering::SeqCst) {
                let _call = ChildCall(self.child_cancelled.clone());
                self.child_started.notify_one();
                return std::future::pending().await;
            }
            if let Some(response) = self.child_responses.lock().unwrap().pop_front() {
                return Ok(response);
            }
            return Ok(answer("worker answer"));
        }
        self.requests.lock().unwrap().push(request);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted response"))
    }
}

struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn call(name: &str, args: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(name), name, args)),
    )])
}

/// A call of a collaboration tool in its default namespace.
fn collab(name: &str, args: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::ToolCall(
            ToolCall::new(CallId::new(name), name, args).with_namespace(COLLABORATION),
        ),
    )])
}

fn qualified(name: &str) -> String {
    format!("{COLLABORATION}.{name}")
}

/// The model-facing names of the tools a request carries.
fn request_tools(request: &ModelRequest) -> Vec<String> {
    request
        .tools()
        .iter()
        .map(ra_core::model::ModelToolDefinition::qualified_name)
        .collect()
}

fn declared_tools(agent: &AgentSpec) -> Vec<String> {
    agent
        .tools()
        .iter()
        .filter(|tool| tool.options().is_advertised_to_model())
        .map(|tool| tool.model_definition().qualified_name())
        .collect()
}

fn next_id() -> ItemId {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    ItemId::new(format!("response-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

async fn complete_turn(thread: &AgentThread, text: &str) {
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text(text)]))
        .await;
    timeout(PATIENCE, async {
        loop {
            match thread.next_event().await.expect("a live thread") {
                ThreadEvent::TurnComplete { .. } => break,
                ThreadEvent::Error { message, .. } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the turn completes");
}

async fn configured_agent(host: &CodingHost, role: &PromptRole) -> (Arc<AgentSpec>, AgentControl) {
    let (agent, control, _config) = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        role,
        host,
        CodingProfile::CodexLike,
    )
    .await
    .expect("the configured standard tier builds")
    .into_parts(host.build_run_config());
    (
        agent,
        control.expect("the standard tier offers collaboration"),
    )
}

fn thread(
    id: &str,
    host: &CodingHost,
    agent: Arc<AgentSpec>,
    control: AgentControl,
    script: &Arc<Script>,
) -> AgentThread {
    AgentThread::builder(
        SessionId::new(id),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(script))),
    )
    .with_config(host.build_run_config())
    .with_services(host.build_tool_services())
    .with_agent_control(control)
    .build()
}

#[tokio::test]
async fn coding_thread_completes_beyond_the_previous_turn_limit() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let script = Arc::new(Script::default());
    let mut responses = VecDeque::new();
    for index in 0..33 {
        let path = format!("note-{index}.txt");
        std::fs::write(workspace.path().join(&path), format!("note {index}\n")).unwrap();
        responses.push_back(ModelResponse::new(vec![RunItem::new(
            next_id(),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(format!("read-{index}")),
                "read_file",
                json!({"path": path}),
            )),
        )]));
    }
    responses.push_back(answer("all notes read"));
    *script.responses.lock().unwrap() = responses;
    let thread = thread("unlimited-coding-turn", &host, agent, control, &script);

    complete_turn(&thread, "read all notes").await;

    assert_eq!(script.requests.lock().unwrap().len(), 34);
    assert!(script.responses.lock().unwrap().is_empty());
    thread.shutdown().await;
}

#[tokio::test]
async fn configured_profiles_preserve_independent_tools_and_role_boundaries() {
    let workspace = tempfile::tempdir().unwrap();
    let base = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    for restricted in [false, true] {
        let host = base.clone().with_read_only_role_restrictions(restricted);
        for tier in CodingProfile::SHIPPED {
            let inherited = host_backed_surface(&PromptRole::Main, &base, tier)
                .await
                .unwrap();
            let expected: Vec<_> = inherited.tool_surface().advertised_names().collect();
            for role in [
                PromptRole::Main,
                PromptRole::Coordinator,
                PromptRole::ReadOnlySpecialist,
                PromptRole::Planner,
                PromptRole::OneOffAnswer,
            ] {
                let assembled = host_backed_surface(&role, &host, tier).await.unwrap();
                let built =
                    build_agent_with_control(AgentId::new("coding"), "Coding", &role, &host, tier)
                        .await
                        .unwrap();
                let names: Vec<_> = assembled.tool_surface().advertised_names().collect();
                let mut wanted = expected.clone();
                if role.is_one_off() {
                    wanted.clear();
                } else if restricted && role.is_read_only() {
                    wanted.retain(|name| {
                        *name != "apply_patch" && !name.starts_with("collaboration.")
                    });
                }
                assert_eq!(
                    names, wanted,
                    "{tier:?} / {role:?} / restricted={restricted}"
                );
                assert_eq!(built.agent().tools().len(), names.len());
                assert_eq!(
                    built.agent_control().is_some(),
                    names.iter().any(|name| name.starts_with("collaboration."))
                );
                if !role.is_one_off() {
                    for name in ["exec_command", "write_stdin"] {
                        assert!(names.contains(&name));
                    }
                    if tier != CodingProfile::Core {
                        for name in ["web_search", "web_fetch", "request_user_input"] {
                            assert!(names.contains(&name));
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn standalone_binding_preserves_the_declared_builtin_tool_order() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Full,
    )
    .await
    .unwrap();
    let expected = declared_tools(built.agent());
    let keys: Vec<_> = built
        .agent()
        .tools()
        .iter()
        .map(|tool| tool.origin().lookup_key().clone())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let mut prefixes = Vec::new();
    for bind in [false, true] {
        let script = Arc::new(Script::default());
        script.responses.lock().unwrap().push_back(answer("done"));
        let request = RunRequest::new(
            AgentBinding::direct(built.agent().clone()),
            Arc::new(Resolver(script.clone())),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_config(host.build_run_config())
        .with_services(host.build_tool_services());
        Runner::run(if bind { built.bind(request) } else { request })
            .await
            .unwrap();
        let requests = script.requests.lock().unwrap();
        assert_eq!(request_tools(&requests[0]), expected, "bind={bind}");
        prefixes.push(requests[0].system_instructions().unwrap().to_owned());
    }
    assert_eq!(prefixes[0], prefixes[1]);
    built.agent_control().unwrap().shutdown().await;
}

#[tokio::test]
async fn switching_a_profile_moves_all_capability_contributions_together() {
    let workspace = tempfile::tempdir().unwrap();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_capability(Arc::new(ProfileCapability {
            bound: false,
            changes_declaration: false,
            samples: samples.clone(),
        }));
    for (tier, role, selected) in [
        (CodingProfile::Full, PromptRole::Main, true),
        (CodingProfile::Core, PromptRole::Main, false),
        (CodingProfile::Full, PromptRole::ReadOnlySpecialist, true),
        (CodingProfile::Full, PromptRole::OneOffAnswer, false),
    ] {
        let built = build_agent_with_control(AgentId::new("coding"), "Coding", &role, &host, tier)
            .await
            .unwrap();
        let expected_tools = declared_tools(built.agent());
        let script = Arc::new(Script::default());
        if selected {
            script
                .responses
                .lock()
                .unwrap()
                .push_back(call("profile_probe", json!({})));
        }
        script.responses.lock().unwrap().push_back(answer("done"));
        let request = RunRequest::new(
            AgentBinding::direct(built.agent().clone()),
            Arc::new(Resolver(script.clone())),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_config(host.build_run_config())
        .with_services(host.build_tool_services());
        let result = Runner::run(built.bind(request)).await.unwrap();
        assert_eq!(result.final_text(), "done");
        {
            let requests = script.requests.lock().unwrap();
            for request in requests.iter() {
                assert_eq!(request_tools(request), expected_tools);
                let instructions = request.system_instructions().unwrap();
                assert_eq!(
                    instructions.contains("configured profile operation"),
                    selected
                );
                assert_eq!(
                    instructions.contains("Profile runtime instructions."),
                    selected
                );
                assert_eq!(
                    instructions.matches("configured profile operation").count(),
                    usize::from(selected)
                );
                assert_eq!(
                    request_tools(request)
                        .iter()
                        .filter(|name| *name == "profile_probe")
                        .count(),
                    usize::from(selected)
                );
                assert_eq!(
                    request.model_settings().temperature(),
                    selected.then_some(0.23)
                );
                assert_eq!(request.input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile context projection.")), selected);
            }
            if selected {
                assert!(requests[1].input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile deferred instructions.")));
                assert!(!requests[0].input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile deferred instructions.")));
            }
        }
        if let Some(control) = built.agent_control() {
            control.shutdown().await;
        }
    }
    let samples = samples.lock().unwrap();
    assert_eq!(
        samples.len(),
        2,
        "only the main and default specialist Full surfaces bind this capability"
    );
    assert_eq!(samples[0].model(), Some("test-model"));
    assert_eq!(samples[0].provider(), Some(&ProviderKey::new("test")));
}

#[tokio::test]
async fn a_spawned_agent_binds_its_own_profile_contributions_and_tool_handlers() {
    let workspace = tempfile::tempdir().unwrap();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_capability(Arc::new(ProfileCapability {
            bound: false,
            changes_declaration: false,
            samples: samples.clone(),
        }));
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Full,
    )
    .await
    .unwrap();
    let parent_tools = declared_tools(built.agent());
    let child_tools: Vec<_> = parent_tools
        .iter()
        .filter(|name| !name.starts_with("collaboration."))
        .cloned()
        .collect();
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name":"worker", "message":"worker task", "fork_turns":"none"}),
        ),
        collab("wait_agent", json!({"timeout_ms":10000})),
        answer("delegated"),
    ]);
    script
        .child_responses
        .lock()
        .unwrap()
        .extend([call("profile_probe", json!({})), answer("worker answer")]);
    let config = built.configure_run(host.build_run_config());
    let count = config.capabilities().len();
    let config = built.configure_run(config);
    assert_eq!(
        config.capabilities().len(),
        count,
        "rebinding replaces the prepared surface"
    );
    assert!(
        config
            .capabilities()
            .iter()
            .any(|capability| capability.kind() == CapabilityFamily::COMPACTION)
    );
    let thread = AgentThread::builder(
        SessionId::new("profile-child"),
        AgentBinding::direct(built.agent().clone()),
        Arc::new(Resolver(script.clone())),
    )
    .with_config(config)
    .with_services(host.build_tool_services())
    .with_agent_control(built.agent_control().unwrap().clone())
    .build();
    complete_turn(&thread, "delegate").await;
    for request in script.requests.lock().unwrap().iter() {
        assert_eq!(request_tools(request), parent_tools);
        assert_eq!(
            request
                .system_instructions()
                .unwrap()
                .matches("configured profile operation")
                .count(),
            1
        );
        assert_eq!(request.input().iter().filter(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile context projection.")).count(), 1,
            "thread settings snapshots must retain the run-bound processor exactly once");
    }
    {
        let requests = script.child_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request_tools(request), child_tools);
            assert_eq!(
                request
                    .system_instructions()
                    .unwrap()
                    .matches("configured profile operation")
                    .count(),
                1
            );
            assert_eq!(request.input().iter().filter(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile context projection.")).count(), 1);
        }
        assert!(request_tools(&requests[0]).contains(&"profile_probe".to_owned()));
        assert!(!request_tools(&requests[0]).contains(&qualified("spawn_agent")));
        assert_eq!(requests[0].model_settings().temperature(), Some(0.23));
        assert!(requests[1].input().iter().any(|item| matches!(item, ModelInputItem::Message(message) if message.text_content() == "Profile deferred instructions.")));
        assert!(
            serde_json::to_string(requests[1].input())
                .unwrap()
                .contains("Profile bound tool result.")
        );
    }
    assert_eq!(
        samples.lock().unwrap().len(),
        2,
        "parent and child each bind once"
    );
    thread.shutdown().await;
}

#[tokio::test]
async fn a_bound_capability_cannot_invalidate_the_reviewed_product_surface() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_capability(Arc::new(ProfileCapability {
            bound: false,
            changes_declaration: true,
            samples: Arc::new(Mutex::new(Vec::new())),
        }));
    let bare_error = build_agent_with_profile(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap_err();
    assert!(bare_error.to_string().contains("build_agent_with_control"));
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Full,
    )
    .await
    .unwrap();
    let script = Arc::new(Script::default());
    let request = RunRequest::new(
        AgentBinding::direct(built.agent().clone()),
        Arc::new(Resolver(script.clone())),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(host.build_run_config());
    let error = Runner::run(built.bind(request)).await.unwrap_err();
    assert!(
        error.to_string().contains("changes the tool surface"),
        "{error}"
    );
    assert!(script.requests.lock().unwrap().is_empty());
    built.agent_control().unwrap().shutdown().await;
}

#[tokio::test]
async fn an_agent_offered_collaboration_is_built_with_its_tree_only() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let error = build_agent_with_profile(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("build_agent_with_control"),
        "{error}"
    );
    // Without collaboration tools on the surface there is no tree to own, and the plain builder
    // still serves.
    build_agent_with_profile(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_bare_collaboration_namespace_serves_providers_without_namespaces() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_collaboration_namespace(None);
    let assembled = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    let names: Vec<_> = assembled.tool_surface().advertised_names().collect();
    assert_eq!(names.len(), 18);
    assert!(names.contains(&"spawn_agent"));
    assert!(!names.iter().any(|name| name.starts_with("collaboration.")));
}

#[tokio::test]
async fn a_narrow_tier_does_not_load_an_unselected_catalog() {
    let workspace = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    services
        .fail_listing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let host = services.install(CodingHost::open(workspace.path()).unwrap());
    let core = host_backed_surface(&PromptRole::Main, &host, CodingProfile::Core)
        .await
        .unwrap();
    assert_eq!(core.tool_surface().advertised_count(), 6);
    let error = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("catalog unavailable"));
}

#[tokio::test]
async fn configured_backends_receive_real_calls_and_results_reach_history() {
    let workspace = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        call("web_search", json!({"query": "Rust", "max_results": null})),
        call("web_fetch", json!({"url": "https://example.test/guide"})),
        call("skill", json!({"skill": "rust-guide", "offset": null})),
        call("request_user_input", json!({"questions": [{"id": "next", "header": "Next", "question": "Continue?", "options": [{"label": "Proceed", "description": "Continue the work"}]}]})),
        answer("finished"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("backends", &host, agent, control, &script);
    complete_turn(&thread, "use the installed services").await;
    assert_eq!(
        *services.calls.lock().unwrap(),
        [
            "search:Rust",
            "fetch:https://example.test/guide",
            "skill:rust-guide",
            "user_input"
        ]
    );
    let history = serde_json::to_string(&thread.history()).unwrap();
    for text in [
        "Rust guide",
        "backend document body",
        "skill instructions from the catalog",
        "Proceed",
    ] {
        assert!(
            history.contains(text),
            "missing backend result {text}: {history}"
        );
    }
    assert_eq!(script.requests.lock().unwrap()[0].tools().len(), 18);
    thread.shutdown().await;
}

#[tokio::test]
async fn thread_control_keeps_children_across_turns_and_isolates_other_threads() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        collab("wait_agent", json!({})),
        answer("first turn"),
        collab("list_agents", json!({})),
        answer("second turn"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let (_, other) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("parent", &host, agent, control.clone(), &script);
    complete_turn(&thread, "delegate work").await;
    assert_eq!(control.agents().len(), 2);
    assert_eq!(other.agents().len(), 1);
    complete_turn(&thread, "inspect the live tree").await;
    let history = serde_json::to_string(&thread.history()).unwrap();
    assert!(history.contains("/root/worker"));
    assert!(history.contains("worker answer"));
    thread.shutdown().await;
    assert_eq!(
        control.status(&AgentPath::from_string("/root/worker".to_owned()).unwrap()),
        Some(AgentStatus::Shutdown)
    );
    assert_eq!(other.agents().len(), 1);
    other.shutdown().await;
}

/// Codex starts a spawned agent as a session of its own, with its own unified-exec process manager
/// and its own tool plan: the child's commands are not the parent's sessions, they stop when the
/// tree shuts down, and the child is not offered the collaboration tools unless its model takes
/// them.
#[tokio::test]
async fn a_spawned_agent_is_assembled_with_its_own_processes_and_tool_plan() {
    use ra_core::permission::{PermissionDecision, PermissionRule};
    use ra_runtime::permission::PermissionEngine;
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    let sleep = MarkedSleep::new();
    script.child_responses.lock().unwrap().push_back(call(
        "exec_command",
        json!({
            "cmd": format!("echo ready > child.ready; {}", sleep.command()),
            "justification": null,
            "prefix_rule": null,
            "sandbox_permissions": null,
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 500,
            "timeout_ms": null
        }),
    ));
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        answer("delegated"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    // The worker runs on the configuration of the run that spawned it, which lets the command
    // through without asking.
    let config =
        host.build_run_config()
            .with_permission_engine(PermissionEngine::new().with_rules([
                PermissionRule::new(PermissionDecision::Allow).with_tool_name("exec_command"),
            ]));
    let thread = AgentThread::builder(
        SessionId::new("processes"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(&script))),
    )
    .with_config(config)
    .with_services(host.build_tool_services())
    .with_agent_control(control.clone())
    .build();
    complete_turn(&thread, "delegate a command").await;
    let worker = AgentPath::from_string("/root/worker".to_owned()).unwrap();
    timeout(PATIENCE, async {
        while !matches!(control.status(&worker), Some(AgentStatus::Completed(_))) {
            assert!(
                control.paused_runs().is_empty(),
                "the worker stopped for approval"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the worker completes");

    std::fs::read_to_string(workspace.path().join("child.ready")).unwrap_or_else(|error| {
        let requests = script.child_requests.lock().unwrap();
        panic!(
            "the worker did not run its command ({error}): {}",
            serde_json::to_string(requests.last().unwrap().input()).unwrap()
        )
    });
    assert!(sleep.is_alive());
    // The worker's command lives in the worker's manager, not in the one the parent's tools use.
    assert!(host.process_manager().active_sessions().await.is_empty());
    let child_tools = request_tools(&script.child_requests.lock().unwrap()[0]);
    assert_eq!(child_tools.len(), 12, "{child_tools:?}");
    assert!(child_tools.contains(&"exec_command".to_owned()));
    assert!(
        !child_tools
            .iter()
            .any(|name| name.starts_with("collaboration."))
    );
    let parent_tools = request_tools(&script.requests.lock().unwrap()[0]);
    assert!(parent_tools.contains(&qualified("spawn_agent")));

    timeout(PATIENCE, thread.shutdown()).await.unwrap();
    timeout(PATIENCE, async {
        while sleep.is_alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("shutting the tree down stops the worker's processes");
}

/// A host that states its model takes the collaboration tools offers them to spawned agents too,
/// as Codex does for a model declaring its second multi-agent version.
#[tokio::test]
async fn spawned_agents_are_offered_collaboration_when_the_host_says_so() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_spawned_agent_collaboration();
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        collab("wait_agent", json!({"timeout_ms": 10000})),
        answer("delegated"),
    ]);
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Full,
    )
    .await
    .unwrap();
    let expected = declared_tools(built.agent());
    let thread = AgentThread::builder(
        SessionId::new("nested"),
        AgentBinding::direct(built.agent().clone()),
        Arc::new(Resolver(script.clone())),
    )
    .with_config(built.configure_run(host.build_run_config()))
    .with_services(host.build_tool_services())
    .with_agent_control(built.agent_control().unwrap().clone())
    .build();
    complete_turn(&thread, "delegate").await;
    let child_tools = request_tools(&script.child_requests.lock().unwrap()[0]);
    assert_eq!(child_tools.len(), 18, "{child_tools:?}");
    assert!(child_tools.contains(&qualified("spawn_agent")));
    for request in script.requests.lock().unwrap().iter() {
        assert_eq!(request_tools(request), expected);
    }
    for request in script.child_requests.lock().unwrap().iter() {
        assert_eq!(request_tools(request), expected);
    }
    thread.shutdown().await;
}

#[tokio::test]
async fn observational_roles_inherit_spawned_tools_and_respect_configured_depth() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_spawned_agent_collaboration();
    for role in [PromptRole::ReadOnlySpecialist, PromptRole::Planner] {
        let script = Arc::new(Script::default());
        script.responses.lock().unwrap().extend([
            collab("spawn_agent", json!({"task_name":"worker", "message":"worker task: Inspect the repository", "fork_turns":"none"})),
            collab("wait_agent", json!({"timeout_ms":10000})),
            answer("delegated"),
        ]);
        let built = build_agent_with_control(
            AgentId::new("coding"),
            "Coding",
            &role,
            &host,
            CodingProfile::Full,
        )
        .await
        .unwrap();
        let expected: Vec<_> = declared_tools(built.agent())
            .into_iter()
            .filter(|name| !name.starts_with("collaboration."))
            .collect();
        let control = built.agent_control().unwrap().clone().with_max_depth(1);
        let thread = AgentThread::builder(
            SessionId::new(role.role_name()),
            AgentBinding::direct(built.agent().clone()),
            Arc::new(Resolver(script.clone())),
        )
        .with_config(built.configure_run(host.build_run_config()))
        .with_services(host.build_tool_services())
        .with_agent_control(control.clone())
        .build();
        complete_turn(&thread, "delegate inspection").await;
        let child_tools = request_tools(&script.child_requests.lock().unwrap()[0]);
        assert_eq!(child_tools, expected);
        if matches!(role, PromptRole::Planner) {
            let parent_prompt = built.agent().instructions().unwrap().as_static().unwrap();
            assert!(parent_prompt.contains("Strongly prefer using the `request_user_input` tool"));
            let child_prompt = script.child_requests.lock().unwrap()[0]
                .system_instructions()
                .unwrap()
                .to_owned();
            assert!(!child_prompt.contains("Strongly prefer using the `request_user_input` tool"));
            assert!(child_prompt.contains("Ask direct questions when clarification is necessary"));
            assert!(
                child_prompt
                    .contains("Return your plan and any unresolved questions to the parent agent")
            );
            assert!(!child_prompt.contains("client can render it specially"));
            assert!(child_prompt.contains("plain-text plan markup"));
        }
        assert!(child_tools.contains(&"apply_patch".to_owned()));
        assert!(child_tools.contains(&"exec_command".to_owned()));
        assert!(
            control
                .handle(&AgentPath::from_string("/root/worker".to_owned()).unwrap())
                .unwrap()
                .spawn_depth_exceeded()
        );
        thread.shutdown().await;
    }
}

#[tokio::test]
async fn thread_shutdown_cancels_and_waits_for_a_running_child() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script
        .hang_child
        .store(true, std::sync::atomic::Ordering::SeqCst);
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        answer("parent complete"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("shutdown", &host, agent, control.clone(), &script);
    complete_turn(&thread, "start background work").await;
    timeout(PATIENCE, script.child_started.notified())
        .await
        .unwrap();
    let worker = AgentPath::from_string("/root/worker".to_owned()).unwrap();
    assert_eq!(control.status(&worker), Some(AgentStatus::Running));
    timeout(PATIENCE, thread.shutdown()).await.unwrap();
    assert!(
        script
            .child_cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(control.status(&worker), Some(AgentStatus::Shutdown));
}

#[tokio::test]
async fn an_approval_resume_keeps_the_thread_bound_to_its_control_tree() {
    use ra_core::permission::{PermissionDecision, PermissionRule};
    use ra_runtime::permission::PermissionEngine;
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script
        .responses
        .lock()
        .unwrap()
        .extend([collab("list_agents", json!({})), answer("resumed")]);
    let config = host.build_run_config().with_permission_engine(
        PermissionEngine::new().with_rules([PermissionRule::new(PermissionDecision::Ask)
            .with_tool_name("list_agents")
            .with_namespace(COLLABORATION)]),
    );
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = AgentThread::builder(
        SessionId::new("approval"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script)),
    )
    .with_config(config)
    .with_services(host.build_tool_services())
    .with_agent_control(control)
    .build();
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text(
            "list the live tree",
        )]))
        .await;
    timeout(PATIENCE, async {
        while !matches!(
            thread.next_event().await,
            Some(ThreadEvent::TurnAwaitingApproval { .. })
        ) {}
    })
    .await
    .unwrap();
    let mut state = thread.paused_run().unwrap();
    let item = state.pending_interruption_items().next().unwrap().clone();
    state.approve(&item, false).unwrap();
    thread.resume_turn(state).unwrap();
    timeout(PATIENCE, async {
        while !matches!(
            thread.next_event().await,
            Some(ThreadEvent::TurnComplete { .. })
        ) {}
    })
    .await
    .unwrap();
    let history = serde_json::to_string(&thread.history()).unwrap();
    assert!(history.contains("/root"), "{history}");
    assert!(!history.contains("not enabled for this run"));
    thread.shutdown().await;
}

#[tokio::test]
async fn restricted_roles_withhold_all_collaboration_handlers_in_every_namespace() {
    use ra_core::tool::ToolNamespace;
    let workspace = tempfile::tempdir().unwrap();
    let base = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    for namespace in [None, Some(ToolNamespace::new("agents").unwrap())] {
        let host = base
            .clone()
            .with_collaboration_namespace(namespace)
            .with_read_only_role_restrictions(true);
        for role in [PromptRole::ReadOnlySpecialist, PromptRole::Planner] {
            for tier in CodingProfile::SHIPPED {
                let built =
                    build_agent_with_control(AgentId::new("coding"), "Coding", &role, &host, tier)
                        .await
                        .unwrap();
                assert!(built.agent_control().is_none());
                let names: Vec<_> = built
                    .agent()
                    .tools()
                    .iter()
                    .map(|tool| tool.origin().name())
                    .collect();
                for unavailable in [
                    "apply_patch",
                    "spawn_agent",
                    "send_message",
                    "followup_task",
                    "interrupt_agent",
                    "wait_agent",
                    "list_agents",
                ] {
                    assert!(
                        !names.contains(&unavailable),
                        "{tier:?} {role:?}: {unavailable}"
                    );
                }
                assert!(names.contains(&"exec_command"));
                assert!(names.contains(&"write_stdin"));
                if tier != CodingProfile::Core {
                    assert!(names.contains(&"web_search"));
                    assert!(names.contains(&"web_fetch"));
                }
                let config = built.configure_run(host.build_run_config());
                assert!(
                    !config
                        .capabilities()
                        .iter()
                        .any(|capability| capability.kind().as_str() == "collaboration")
                );
                let prompt = built.agent().instructions().unwrap().as_static().unwrap();
                assert!(prompt.contains("No collaboration tools are advertised; do not delegate tasks or direct other agents"));
            }
        }
    }
}
