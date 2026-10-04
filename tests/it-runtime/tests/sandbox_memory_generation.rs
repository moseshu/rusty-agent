//! `ra-runtime::sandbox::memory::{manager, rollouts, phase_one, phase_two}` and the runner's
//! memory hooks: run segments recorded while a sandbox session is open, extracted and consolidated
//! when it closes.
//!
//! Ported from the generation tests of the reference's `tests/sandbox/test_memory.py`, in upstream
//! order, and the two memory cases of `tests/sandbox/test_runtime.py`, against a real local session
//! where the reference uses its in-memory filesystem session. Models are scripted; the phase
//! models are given to the configuration as instances, as the reference's tests give them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
    cancel::CancelScope,
    capability::Capability,
    error::{Error, Result},
    item::{
        CallId, Compaction, ItemId, Message, MessageRole, ModelInputItem, ModelResponse,
        OutputPhase, Reasoning, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    sandbox::{
        CreateRequest, Manifest, MemoryGenerateConfig, MemoryLayoutConfig, SandboxAgentConfig,
        SandboxClient, SandboxMemory, SandboxSession,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
    sandbox::{
        SandboxRunConfig,
        memory::{
            manager::{get_or_create_memory_generation_manager, memory_generation_managers},
            rollouts::{
                RolloutTerminalMetadata, RolloutTerminalState, build_rollout_payload,
                terminal_metadata_for_error,
            },
        },
    },
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use ra_tools::sandbox::{filesystem::default_capabilities, memory::Memory};
use serde_json::{Value, json};

// ---- helpers --------------------------------------------------------------------------------

/// Answers from a script, and records the instructions and first input text of every call.
#[derive(Default)]
struct ScriptedModel {
    script: Mutex<Vec<Vec<RunItem>>>,
    calls: Mutex<Vec<(Option<String>, String)>>,
    /// Never answers: the call waits until it is dropped.
    blocks: bool,
    /// Never concludes: every call edits `memories/MEMORY.md` again, with a line of its own.
    keeps_working: bool,
}

impl ScriptedModel {
    fn keeps_working() -> Arc<Self> {
        Arc::new(Self {
            keeps_working: true,
            ..Self::default()
        })
    }

    fn new(script: Vec<Vec<RunItem>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            ..Self::default()
        })
    }

    fn blocking() -> Arc<Self> {
        Arc::new(Self {
            blocks: true,
            ..Self::default()
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The reference test's `_extract_user_text`: the first input item's text, from the first
    /// call.
    fn user_text(&self) -> String {
        self.calls.lock().unwrap()[0].1.clone()
    }

    fn system_instructions(&self) -> Option<String> {
        self.calls.lock().unwrap()[0].0.clone()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let first_text = request
            .input()
            .iter()
            .find_map(|item| match item {
                ModelInputItem::Message(message) => Some(message.text_content()),
                _ => None,
            })
            .unwrap_or_default();
        self.calls
            .lock()
            .unwrap()
            .push((request.system_instructions().map(str::to_owned), first_text));
        if self.blocks {
            std::future::pending::<()>().await;
        }
        if self.keeps_working {
            let call = self.call_count();
            return Ok(ModelResponse::new(vec![patch_update_call(
                &format!("work-{call}"),
                "memories/MEMORY.md",
                &format!("work {call}"),
            )]));
        }
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(ModelResponse::new(script.remove(0)))
    }
}

/// Resolves every name to the worker's model.
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

/// Knows one named model and has no default, recording every name it is asked for.
struct NamedOnlyResolver {
    model: Arc<ScriptedModel>,
    asked: Mutex<Vec<Option<String>>>,
}

impl ModelResolver for NamedOnlyResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.asked
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        if model_name != Some("worker-model") {
            return Err(Error::config("No default model configured"));
        }
        FixedResolver(Arc::clone(&self.model)).resolve_model(model_name)
    }
}

fn resolver(model: &Arc<ScriptedModel>) -> Arc<dyn ModelResolver> {
    Arc::new(FixedResolver(Arc::clone(model)))
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

/// The reference's `get_final_output_message`.
fn final_message(text: &str) -> Vec<RunItem> {
    vec![item(
        &format!("msg-{text}"),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )]
}

/// The reference test's `_phase_one_message`.
fn phase_one_message(raw_memory: &str) -> Vec<RunItem> {
    final_message(
        &json!({
            "rollout_slug": "task_memory",
            "rollout_summary": "# Task summary\n",
            "raw_memory": raw_memory,
        })
        .to_string(),
    )
}

/// The reference test's `_patch_update_call`.
fn patch_update_call(call_id: &str, path: &str, text: &str) -> RunItem {
    let diff = format!(
        "@@\n{}",
        text.lines()
            .map(|line| format!("+{line}\n"))
            .collect::<String>()
    );
    item(
        call_id,
        RunItemKind::ToolCall(ToolCall::custom(
            CallId::new(call_id),
            "apply_patch",
            json!({"type": "update_file", "path": path, "diff": diff}).to_string(),
        )),
    )
}

/// A consolidation that writes `MEMORY.md` and `memory_summary.md` under `memories_dir`.
fn phase_two_model(memories_dir: &str, memory: &str, summary: &str) -> Arc<ScriptedModel> {
    ScriptedModel::new(vec![
        vec![
            patch_update_call("memory-md", &format!("{memories_dir}/MEMORY.md"), memory),
            patch_update_call(
                "memory-summary",
                &format!("{memories_dir}/memory_summary.md"),
                summary,
            ),
        ],
        final_message("consolidated"),
    ])
}

/// The reference test's `_memory_config`: generation only, with scripted phase models.
fn memory_config(
    layout: MemoryLayoutConfig,
    extra_prompt: Option<&str>,
    phase_one: &Arc<ScriptedModel>,
    phase_two: &Arc<ScriptedModel>,
) -> Arc<Memory> {
    Arc::new(
        Memory::builder()
            .layout(layout)
            .read(None)
            .generate(Some(
                MemoryGenerateConfig::new()
                    .with_extra_prompt(extra_prompt.map(str::to_owned))
                    .with_phase_one_model(Arc::clone(phase_one))
                    .with_phase_two_model(Arc::clone(phase_two)),
            ))
            .build()
            .unwrap(),
    )
}

fn default_memory() -> Arc<Memory> {
    memory_config(
        MemoryLayoutConfig::new(),
        None,
        &ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]),
        &phase_two_model("memories", "memory entry", "summary entry"),
    )
}

fn sandbox_memory(memory: &Memory) -> SandboxMemory {
    memory.sandbox_memory().unwrap()
}

fn worker(id: &str, capabilities: Vec<Arc<dyn Capability>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions("Worker.")
        .sandbox(SandboxAgentConfig::empty().with_capabilities(capabilities))
        .build()
        .unwrap()
}

/// A started local session over a fresh directory, and the workspace root.
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

/// The reference test's `_run_config_for_session`.
fn run_config(session: &Arc<dyn SandboxSession>) -> RunConfig {
    RunConfig::new().with_sandbox(SandboxRunConfig::new().with_session(Arc::clone(session)))
}

async fn run(
    agent: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    input: &str,
    config: RunConfig,
) -> Result<RunResult> {
    Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            resolver(model),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(input))],
        )
        .with_config(config),
    )
    .await
}

fn files_in(directory: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|found| found == extension))
        .collect();
    files.sort();
    files
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Asks for approval before it runs.
struct ApprovalTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl ApprovalTool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("approval_tool").unwrap(),
            schema: ToolSchema::new(
                "approval_tool",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
        })
    }
}

#[async_trait]
impl Tool for ApprovalTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default().with_approval(ToolApprovalPolicy::Always)
    }
}

// ---- test_memory.py -------------------------------------------------------------------------

#[test]
fn a_rollout_segment_keeps_the_conversation_and_drops_instructions_and_noise() {
    let input = [
        ModelInputItem::Message(Message::text(MessageRole::System, "system prompt")),
        ModelInputItem::Message(Message::user("hello")),
    ];
    let generated = [
        item(
            "msg-1",
            RunItemKind::Message(Message::assistant("assistant", OutputPhase::Final)),
        ),
        item("rs-1", RunItemKind::Reasoning(Reasoning::new())),
        item(
            "cmp-1",
            RunItemKind::Compaction(Compaction::new("summary", Vec::new())),
        ),
        item(
            "call-1",
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "lookup", json!({}))),
        ),
        item(
            "call-1.output",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new("call-1"), json!("found"))),
        ),
    ];

    let payload = build_rollout_payload(
        &input,
        &generated,
        None,
        &[],
        RolloutTerminalMetadata::new(RolloutTerminalState::Completed, false),
    );

    assert_eq!(payload.input(), &input[1..]);
    let labels: Vec<&str> = payload
        .generated_items()
        .iter()
        .map(ModelInputItem::label)
        .collect();
    assert_eq!(labels, ["message", "tool_call", "tool_call_output"]);
    let value = serde_json::to_value(&payload).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(!keys.contains(&"interruptions"));
    assert!(!keys.contains(&"final_output"));
}

#[test]
fn a_rollout_segment_records_the_approvals_it_stopped_for() {
    let approval = item(
        "approval-1",
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new("approval-call"),
            "approval_tool",
            json!({}),
        )),
    );

    let payload = build_rollout_payload(
        &[],
        &[],
        None,
        std::slice::from_ref(&approval),
        RolloutTerminalMetadata::new(RolloutTerminalState::Interrupted, false),
    );

    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        value["interruptions"],
        json!([serde_json::to_value(approval.kind()).unwrap()])
    );
    assert_eq!(value["interruptions"][0]["type"], "tool_approval");
}

#[test]
fn a_failed_run_is_classified_by_its_error() {
    let cases = [
        (
            Error::budget(ra_core::error::BudgetKind::MaxTurns, "too many turns"),
            "max_turns_exceeded",
        ),
        (Error::cancelled("user interrupt"), "cancelled"),
        (Error::config("broken"), "failed"),
    ];
    for (error, expected) in cases {
        let metadata = serde_json::to_value(terminal_metadata_for_error(&error)).unwrap();

        assert_eq!(metadata["terminal_state"], expected, "{error}");
        assert_eq!(metadata["exception_type"], error.code());
        assert_eq!(metadata["exception_message"], error.to_string());
        assert_eq!(metadata["has_final_output"], false);

        // A failed Session write returns the run's checkpoint with its error; how the run ended
        // is still the original error's.
        let retained = error.with_run_state(RunState::start(RunId::new("run-retained")));
        let wrapped = serde_json::to_value(terminal_metadata_for_error(&retained)).unwrap();
        assert_eq!(wrapped, metadata, "{retained}");
    }
}

#[tokio::test]
async fn a_long_rollout_reaches_phase_one_truncated_and_without_instructions() {
    let (_directory, root, session) = live_session().await;
    let phase_one = ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]);
    let memory = memory_config(
        MemoryLayoutConfig::new(),
        None,
        &phase_one,
        &phase_two_model("memories", "memory entry", "summary entry"),
    );
    let model = ScriptedModel::new(vec![final_message("done")]);
    let long_input = "x ".repeat(400_000);

    run(
        worker("worker", vec![memory]),
        &model,
        &long_input,
        run_config(&session),
    )
    .await
    .unwrap();
    session.close().await.unwrap();

    let prompt = phase_one.user_text();
    assert!(prompt.contains("[rollout content omitted: this phase-one memory prompt contains"));
    assert!(!prompt.contains("Worker."));
    assert_eq!(files_in(&root.join("memories/raw_memories"), "md").len(), 1);
}

#[tokio::test]
async fn a_sandbox_agent_without_memory_records_nothing() {
    let (_directory, root, session) = live_session().await;
    let model = ScriptedModel::new(vec![final_message("done")]);

    let result = run(
        worker("worker", Vec::new()),
        &model,
        "hello",
        run_config(&session),
    )
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    assert!(!root.join("sessions").exists());
    assert!(!root.join("memories").exists());
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_run_writes_its_rollout_and_closing_the_session_writes_memory() {
    let (_directory, root, session) = live_session().await;
    let phase_one = ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]);
    let phase_two = phase_two_model("memories", "memory entry", "summary entry");
    let memory = memory_config(
        MemoryLayoutConfig::new(),
        Some("Track durable user preferences."),
        &phase_one,
        &phase_two,
    );
    let model = ScriptedModel::new(vec![final_message("done")]);

    let result = run(
        worker("worker", vec![memory]),
        &model,
        "hello",
        run_config(&session),
    )
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    assert_eq!(files_in(&root.join("sessions"), "jsonl").len(), 1);
    assert_eq!(phase_one.call_count(), 0);

    session.close().await.unwrap();

    assert_eq!(files_in(&root.join("memories/raw_memories"), "md").len(), 1);
    let summaries = files_in(&root.join("memories/rollout_summaries"), "md");
    assert_eq!(summaries.len(), 1);
    assert_eq!(read(&root.join("memories/MEMORY.md")), "memory entry\n");
    assert_eq!(
        read(&root.join("memories/memory_summary.md")),
        "summary entry\n"
    );
    let raw_memories = read(&root.join("memories/raw_memories.md"));
    for expected in [
        "rollout_id: ",
        "updated_at: ",
        "rollout_path: sessions/",
        "rollout_summary_file: rollout_summaries/",
        "terminal_state: completed",
    ] {
        assert!(raw_memories.contains(expected), "{expected}");
    }
    let summary = read(&summaries[0]);
    for expected in [
        "session_id: ",
        "updated_at: ",
        "rollout_path: sessions/",
        "terminal_state: completed",
    ] {
        assert!(summary.contains(expected), "{expected}");
    }
    assert!(
        phase_one
            .user_text()
            .contains("\"terminal_state\":\"completed\"")
    );
    let system_instructions = phase_one.system_instructions().unwrap();
    assert!(system_instructions.contains("DEVELOPER-SPECIFIC EXTRA GUIDANCE"));
    assert!(system_instructions.contains("Track durable user preferences."));
    assert!(phase_two.call_count() > 0);
    assert!(
        phase_two
            .user_text()
            .contains("DEVELOPER-SPECIFIC EXTRA GUIDANCE")
    );
    assert!(
        phase_two
            .user_text()
            .contains("Track durable user preferences.")
    );
}

#[tokio::test]
async fn a_custom_layout_is_where_rollouts_and_memory_go() {
    let (_directory, root, session) = live_session().await;
    let memory = memory_config(
        MemoryLayoutConfig::new()
            .with_memories_dir("agent_memory")
            .with_sessions_dir("agent_sessions"),
        None,
        &ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]),
        &phase_two_model("agent_memory", "memory entry", "summary entry"),
    );
    let model = ScriptedModel::new(vec![final_message("done")]);

    run(
        worker("worker", vec![memory]),
        &model,
        "hello",
        run_config(&session),
    )
    .await
    .unwrap();

    assert_eq!(files_in(&root.join("agent_sessions"), "jsonl").len(), 1);

    session.close().await.unwrap();

    assert_eq!(read(&root.join("agent_memory/MEMORY.md")), "memory entry\n");
    assert_eq!(
        read(&root.join("agent_memory/memory_summary.md")),
        "summary entry\n"
    );
}

#[tokio::test]
async fn one_session_can_generate_memory_for_two_layouts() {
    let (_directory, root, session) = live_session().await;
    let memory_a = memory_config(
        MemoryLayoutConfig::new()
            .with_memories_dir("agent_a_memory")
            .with_sessions_dir("agent_a_sessions"),
        None,
        &ScriptedModel::new(vec![phase_one_message("agent a raw\n")]),
        &phase_two_model("agent_a_memory", "agent a entry", "agent a summary"),
    );
    let memory_b = memory_config(
        MemoryLayoutConfig::new()
            .with_memories_dir("agent_b_memory")
            .with_sessions_dir("agent_b_sessions"),
        None,
        &ScriptedModel::new(vec![phase_one_message("agent b raw\n")]),
        &phase_two_model("agent_b_memory", "agent b entry", "agent b summary"),
    );

    run(
        worker("agent-a", vec![memory_a]),
        &ScriptedModel::new(vec![final_message("a done")]),
        "first",
        run_config(&session),
    )
    .await
    .unwrap();
    run(
        worker("agent-b", vec![memory_b]),
        &ScriptedModel::new(vec![final_message("b done")]),
        "second",
        run_config(&session),
    )
    .await
    .unwrap();

    assert_eq!(files_in(&root.join("agent_a_sessions"), "jsonl").len(), 1);
    assert_eq!(files_in(&root.join("agent_b_sessions"), "jsonl").len(), 1);

    session.close().await.unwrap();

    assert_eq!(
        read(&root.join("agent_a_memory/MEMORY.md")),
        "agent a entry\n"
    );
    assert_eq!(
        read(&root.join("agent_b_memory/MEMORY.md")),
        "agent b entry\n"
    );
}

#[tokio::test]
async fn one_layout_cannot_generate_two_ways() {
    let (_directory, _root, session) = live_session().await;
    let resolver = resolver(&ScriptedModel::new(Vec::new()));
    let memory = default_memory();
    let different = memory_config(
        MemoryLayoutConfig::new(),
        None,
        &ScriptedModel::new(vec![phase_one_message("different\n")]),
        &phase_two_model("memories", "memory entry", "summary entry"),
    );

    let first =
        get_or_create_memory_generation_manager(&session, &sandbox_memory(&memory), &resolver)
            .unwrap();
    let again =
        get_or_create_memory_generation_manager(&session, &sandbox_memory(&memory), &resolver)
            .unwrap();
    let error =
        get_or_create_memory_generation_manager(&session, &sandbox_memory(&different), &resolver)
            .unwrap_err();

    assert!(Arc::ptr_eq(&first, &again));
    assert!(
        error
            .to_string()
            .contains("different Memory generation config"),
        "{error}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_segment_is_recorded_under_the_rollout_id_it_is_appended_to() {
    let (_directory, root, session) = live_session().await;
    let manager = get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&default_memory()),
        &resolver(&ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let payload = build_rollout_payload(
        &[],
        &[],
        None,
        &[],
        RolloutTerminalMetadata::new(RolloutTerminalState::Completed, false),
    )
    .with_rollout_id("payload-id");

    manager
        .enqueue_rollout_payload(payload, " canonical-id ")
        .await
        .unwrap();

    let line: Value =
        serde_json::from_str(&read(&root.join("sessions/canonical-id.jsonl"))).unwrap();
    assert_eq!(line["rollout_id"], "canonical-id");
    let keys: Vec<&str> = line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "generated_items",
            "input",
            "rollout_id",
            "terminal_metadata",
            "updated_at"
        ]
    );
    let text = read(&root.join("sessions/canonical-id.jsonl"));
    assert!(text.starts_with("{\"updated_at\":"), "{text}");
    assert!(
        text.contains(",\"rollout_id\":\"canonical-id\",\"input\":"),
        "{text}"
    );
}

#[tokio::test]
async fn a_memories_directory_serves_one_sessions_directory() {
    let (_directory, _root, session) = live_session().await;
    let resolver = resolver(&ScriptedModel::new(Vec::new()));
    let layout = |memories: &str, sessions: &str| {
        memory_config(
            MemoryLayoutConfig::new()
                .with_memories_dir(memories)
                .with_sessions_dir(sessions),
            None,
            &ScriptedModel::new(Vec::new()),
            &ScriptedModel::new(Vec::new()),
        )
    };

    get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&layout("shared_memory", "sessions_a")),
        &resolver,
    )
    .unwrap();
    let error = get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&layout("shared_memory", "sessions_b")),
        &resolver,
    )
    .unwrap_err();

    assert!(
        error.to_string().contains(
            "already has a Memory generation capability for memories_dir='shared_memory'"
        ),
        "{error}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_sessions_directory_serves_one_memories_directory() {
    let (_directory, _root, session) = live_session().await;
    let resolver = resolver(&ScriptedModel::new(Vec::new()));
    let layout = |memories: &str, sessions: &str| {
        memory_config(
            MemoryLayoutConfig::new()
                .with_memories_dir(memories)
                .with_sessions_dir(sessions),
            None,
            &ScriptedModel::new(Vec::new()),
            &ScriptedModel::new(Vec::new()),
        )
    };

    get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&layout("memory_a", "shared_sessions")),
        &resolver,
    )
    .unwrap();
    let error = get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&layout("memory_b", "shared_sessions")),
        &resolver,
    )
    .unwrap_err();

    assert!(
        error.to_string().contains(
            "already has a Memory generation capability for sessions_dir='shared_sessions'"
        ),
        "{error}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn runs_sharing_a_group_id_share_one_rollout() {
    let (_directory, root, session) = live_session().await;
    let model = ScriptedModel::new(vec![
        final_message("first done"),
        final_message("second done"),
    ]);
    let agent = worker("worker", vec![default_memory()]);
    let config = run_config(&session).with_group_id("trace-thread-123");

    let first = run(Arc::clone(&agent), &model, "first", config.clone())
        .await
        .unwrap();
    let second = run(agent, &model, "second", config).await.unwrap();

    let rollouts = files_in(&root.join("sessions"), "jsonl");
    assert_eq!(first.final_text(), "first done");
    assert_eq!(second.final_text(), "second done");
    assert_eq!(rollouts.len(), 1);
    assert_eq!(rollouts[0].file_name().unwrap(), "trace-thread-123.jsonl");
    assert_eq!(read(&rollouts[0]).lines().count(), 2);
    session.close().await.unwrap();
}

#[tokio::test]
async fn runs_without_a_group_are_rollouts_of_their_own() {
    let (_directory, root, session) = live_session().await;
    let model = ScriptedModel::new(vec![
        final_message("first done"),
        final_message("second done"),
    ]);
    let agent = worker("worker", vec![default_memory()]);

    run(Arc::clone(&agent), &model, "first", run_config(&session))
        .await
        .unwrap();
    run(agent, &model, "second", run_config(&session))
        .await
        .unwrap();

    let names: Vec<String> = files_in(&root.join("sessions"), "jsonl")
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(
        names
            .iter()
            .all(|name| name.starts_with("run-") && name.ends_with(".jsonl")),
        "{names:?}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn consolidation_sees_at_most_its_limit_and_what_was_added() {
    let (_directory, root, session) = live_session().await;
    let phase_one = ScriptedModel::new(vec![
        phase_one_message("first raw\n"),
        phase_one_message("second raw\n"),
    ]);
    let phase_two = phase_two_model("memories", "first entry", "first summary");
    let memory = Arc::new(
        Memory::builder()
            .read(None)
            .generate(Some(
                MemoryGenerateConfig::new()
                    .with_max_raw_memories_for_consolidation(1)
                    .unwrap()
                    .with_phase_one_model(Arc::clone(&phase_one))
                    .with_phase_two_model(Arc::clone(&phase_two)),
            ))
            .build()
            .unwrap(),
    );
    let model = ScriptedModel::new(vec![
        final_message("first done"),
        final_message("second done"),
    ]);
    let agent = worker("worker", vec![memory]);

    run(
        Arc::clone(&agent),
        &model,
        "first",
        run_config(&session).with_group_id("first-chat"),
    )
    .await
    .unwrap();
    run(
        agent,
        &model,
        "second",
        run_config(&session).with_group_id("second-chat"),
    )
    .await
    .unwrap();

    assert_eq!(files_in(&root.join("sessions"), "jsonl").len(), 2);

    session.close().await.unwrap();

    let selection: Value =
        serde_json::from_str(&read(&root.join("memories/phase_two_selection.json"))).unwrap();
    let selected: Vec<&str> = selection["selected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["rollout_id"].as_str().unwrap())
        .collect();
    assert_eq!(selected, ["second-chat"]);
    let merged = read(&root.join("memories/raw_memories.md"));
    assert!(merged.contains("second raw"));
    assert!(!merged.contains("first raw"));
    let prompt = phase_two.user_text();
    assert!(prompt.contains("newly added since the last successful Phase 2 run: 1"));
    assert!(prompt.contains("rollout_id=second-chat"));
}

/// A session the run created is cleaned up when the run ends, and that cleanup runs the flush.
#[tokio::test]
async fn a_session_the_run_owns_generates_memory_as_the_run_cleans_it_up() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    let phase_one = ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]);
    let phase_two = phase_two_model("memories", "memory entry", "summary entry");
    let memory = memory_config(MemoryLayoutConfig::new(), None, &phase_one, &phase_two);
    let model = ScriptedModel::new(vec![final_message("done")]);
    let config = RunConfig::new().with_sandbox(
        SandboxRunConfig::new()
            .with_client(Arc::new(UnixLocalSandboxClient::new()) as Arc<dyn SandboxClient>)
            .with_manifest(Manifest::new().with_root(root.to_string_lossy().into_owned())),
    );

    let result = run(worker("worker", vec![memory]), &model, "hello", config)
        .await
        .unwrap();

    assert_eq!(result.final_text(), "done");
    assert_eq!(phase_one.call_count(), 1);
    assert!(
        phase_one
            .user_text()
            .contains("\"terminal_state\":\"completed\"")
    );
    assert!(phase_two.call_count() > 0);
    // A root the host named is left in place by the local client, so what was written can be read.
    assert_eq!(read(&root.join("memories/MEMORY.md")), "memory entry\n");
    assert_eq!(
        read(&root.join("memories/memory_summary.md")),
        "summary entry\n"
    );
}

#[tokio::test]
async fn memory_is_written_when_the_session_closes_and_not_before() {
    let (_directory, root, session) = live_session().await;
    let memory = memory_config(
        MemoryLayoutConfig::new(),
        None,
        &ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]),
        &phase_two_model("memories", "shutdown entry", "shutdown summary"),
    );
    let model = ScriptedModel::new(vec![final_message("done")]);

    run(
        worker("worker", vec![memory]),
        &model,
        "hello",
        run_config(&session),
    )
    .await
    .unwrap();

    assert_eq!(read(&root.join("memories/MEMORY.md")), "");

    session.close().await.unwrap();

    assert_eq!(read(&root.join("memories/MEMORY.md")), "shutdown entry\n");
    assert_eq!(
        read(&root.join("memories/memory_summary.md")),
        "shutdown summary\n"
    );
}

#[tokio::test]
async fn closing_the_session_unregisters_its_managers() {
    let (_directory, _root, session) = live_session().await;
    let manager = get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&default_memory()),
        &resolver(&ScriptedModel::new(Vec::new())),
    )
    .unwrap();

    let registered = memory_generation_managers(&session);
    assert_eq!(registered.len(), 1);
    assert!(Arc::ptr_eq(&registered[0], &manager));

    session.close().await.unwrap();

    assert!(memory_generation_managers(&session).is_empty());
}

#[tokio::test]
async fn a_flush_dropped_part_way_unregisters_and_does_not_consolidate() {
    let (_directory, root, session) = live_session().await;
    let phase_one = ScriptedModel::blocking();
    let phase_two = phase_two_model("memories", "memory entry", "summary entry");
    let memory = memory_config(MemoryLayoutConfig::new(), None, &phase_one, &phase_two);
    let manager = get_or_create_memory_generation_manager(
        &session,
        &sandbox_memory(&memory),
        &resolver(&ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    manager
        .enqueue_rollout_payload(
            build_rollout_payload(
                &[],
                &[],
                None,
                &[],
                RolloutTerminalMetadata::new(RolloutTerminalState::Completed, false),
            ),
            "cancelled-flush",
        )
        .await
        .unwrap();

    let flushed = tokio::time::timeout(Duration::from_millis(500), manager.flush()).await;

    assert!(flushed.is_err());
    assert_eq!(phase_one.call_count(), 1);
    assert!(memory_generation_managers(&session).is_empty());
    assert_eq!(phase_two.call_count(), 0);
    assert!(files_in(&root.join("memories/raw_memories"), "md").is_empty());
    // A flush runs once: closing the session afterwards extracts nothing.
    session.close().await.unwrap();
    assert_eq!(phase_one.call_count(), 1);
}

#[tokio::test]
async fn a_segment_that_cannot_be_recorded_does_not_fail_the_run() {
    let (_directory, root, session) = live_session().await;
    let model = ScriptedModel::new(vec![final_message("done")]);

    let result = run(
        worker("worker", vec![default_memory()]),
        &model,
        "hello",
        run_config(&session).with_group_id("../not-a-file-name"),
    )
    .await
    .unwrap();

    assert_eq!(result.final_text(), "done");
    assert!(files_in(&root.join("sessions"), "jsonl").is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn an_interrupted_run_reaches_phase_one_as_interrupted() {
    let (_directory, _root, session) = live_session().await;
    let phase_one = ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]);
    let memory = memory_config(
        MemoryLayoutConfig::new(),
        None,
        &phase_one,
        &phase_two_model("memories", "interrupted entry", "interrupted summary"),
    );
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("worker")
        .instructions("Worker.")
        .tool(ApprovalTool::new())
        .sandbox(SandboxAgentConfig::empty().with_capability(memory))
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![vec![item(
        "approval-call",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("approval-call"),
            "approval_tool",
            json!({}),
        )),
    )]]);

    let result = run(agent, &model, "interrupt me", run_config(&session))
        .await
        .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
    session.close().await.unwrap();
    assert!(
        phase_one
            .user_text()
            .contains("\"terminal_state\":\"interrupted\"")
    );
}

#[tokio::test]
async fn a_failed_run_is_recorded_with_how_it_failed() {
    let (_directory, root, session) = live_session().await;
    // The worker's model has nothing to say, so the run fails on its first turn.
    let model = ScriptedModel::new(Vec::new());

    let error = run(
        worker("worker", vec![default_memory()]),
        &model,
        "hello",
        run_config(&session).with_group_id("failed-run"),
    )
    .await
    .unwrap_err();

    let line: Value = serde_json::from_str(&read(&root.join("sessions/failed-run.jsonl"))).unwrap();
    assert_eq!(line["terminal_metadata"]["terminal_state"], "failed");
    assert_eq!(line["terminal_metadata"]["exception_type"], error.code());
    assert_eq!(line["input"][0]["type"], "message");
    session.close().await.unwrap();
}

/// The reference's consolidation raises `MaxTurnsExceeded` at its turn cap and the manager skips
/// the selection; a run here stops softly there, and must count as a failure all the same.
#[tokio::test]
async fn a_consolidation_cut_off_at_its_turn_cap_is_not_recorded_as_done() {
    let (_directory, root, session) = live_session().await;
    let phase_two = ScriptedModel::keeps_working();
    let memory = memory_config(
        MemoryLayoutConfig::new(),
        None,
        &ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]),
        &phase_two,
    );
    let model = ScriptedModel::new(vec![final_message("done")]);

    run(
        worker("worker", vec![memory]),
        &model,
        "hello",
        run_config(&session),
    )
    .await
    .unwrap();
    session.close().await.unwrap();

    assert_eq!(phase_two.call_count(), 500);
    assert_eq!(files_in(&root.join("memories/raw_memories"), "md").len(), 1);
    assert!(!root.join("memories/phase_two_selection.json").exists());
}

/// The reference uses a model instance without asking its provider for anything
/// (`test_agent_model_object_is_used_when_present`), so memory generates with instances on a run
/// whose resolver has no default model.
#[tokio::test]
async fn phase_models_given_as_instances_need_no_default_model() {
    let (_directory, root, session) = live_session().await;
    let phase_one = ScriptedModel::new(vec![phase_one_message("raw memory entry\n")]);
    let phase_two = phase_two_model("memories", "memory entry", "summary entry");
    let memory = memory_config(MemoryLayoutConfig::new(), None, &phase_one, &phase_two);
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("worker")
        .instructions("Worker.")
        .model("worker-model")
        .sandbox(SandboxAgentConfig::empty().with_capability(memory))
        .build()
        .unwrap();
    let resolver = Arc::new(NamedOnlyResolver {
        model: ScriptedModel::new(vec![final_message("done")]),
        asked: Mutex::new(Vec::new()),
    });

    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::clone(&resolver) as Arc<dyn ModelResolver>,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("hello"))],
        )
        .with_config(run_config(&session)),
    )
    .await
    .unwrap();
    session.close().await.unwrap();

    assert_eq!(result.final_text(), "done");
    assert_eq!(phase_one.call_count(), 1);
    assert_eq!(read(&root.join("memories/MEMORY.md")), "memory entry\n");
    assert!(
        resolver
            .asked
            .lock()
            .unwrap()
            .iter()
            .all(|name| name.as_deref() == Some("worker-model"))
    );
}

// ---- test_runtime.py ------------------------------------------------------------------------

fn transfer_to(target: &str) -> Vec<RunItem> {
    vec![item(
        "transfer",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("transfer"),
            format!("transfer_to_{target}"),
            json!({}),
        )),
    )]
}

fn handoff_to(target: &str) -> HandoffSpec {
    HandoffSpec::new(
        AgentId::new(target),
        ToolSchema::new(
            format!("transfer_to_{target}"),
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )
        .unwrap(),
    )
}

async fn run_with_handoff(
    first: Arc<AgentSpec>,
    second: Arc<AgentSpec>,
    session: &Arc<dyn SandboxSession>,
) {
    let target = second.id().as_str().to_owned();
    let first = first
        .to_builder()
        .handoff(handoff_to(&target))
        .build()
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&first))
        .register(second)
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![transfer_to(&target), final_message("done")]);
    let result = run(
        first,
        &model,
        "hello",
        run_config(session)
            .with_group_id("handoff-run")
            .with_agent_registry(registry),
    )
    .await
    .unwrap();
    assert_eq!(result.last_agent().id().as_str(), target);
}

#[tokio::test]
async fn a_handoff_to_an_agent_that_does_not_generate_stops_the_recording() {
    let (_directory, root, session) = live_session().await;
    let reader = Arc::new(Memory::builder().generate(None).build().unwrap());
    let mut reviewer: Vec<Arc<dyn Capability>> = vec![reader];
    reviewer.extend(default_capabilities().into_iter().take(2));

    run_with_handoff(
        worker("triage", vec![default_memory()]),
        worker("reviewer", reviewer),
        &session,
    )
    .await;

    assert!(files_in(&root.join("sessions"), "jsonl").is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_handoff_to_an_agent_that_generates_starts_the_recording() {
    let (_directory, root, session) = live_session().await;

    run_with_handoff(
        worker("triage", Vec::new()),
        worker("worker", vec![default_memory()]),
        &session,
    )
    .await;

    assert_eq!(
        files_in(&root.join("sessions"), "jsonl"),
        [root.join("sessions/handoff-run.jsonl")]
    );
    session.close().await.unwrap();
}
