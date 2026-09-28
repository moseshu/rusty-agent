//! `ra-tools::sandbox::memory` and `ra-core::sandbox::memory`: the memory capability's read side
//! and the configuration it carries.
//!
//! Ported from the capability and configuration tests of the reference's
//! `tests/sandbox/test_memory.py`, in upstream order, against a real local session where the
//! reference uses its in-memory filesystem session. The reference's `Memory(...)` keywords are
//! [`Memory::builder`] here, and its `instructions(manifest)` is [`Memory::instructions_for`].

use std::path::PathBuf;
use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    item::{AgentId, ModelResponse},
    model::{Effort, Model, ModelRequest, ModelSettings},
    sandbox::{
        CreateRequest, Manifest, MemoryGenerateConfig, MemoryLayoutConfig, MemoryModel,
        MemoryReadConfig, PosixPath, SandboxClient, SandboxSession, SandboxWorkspaceScope,
        SessionPath,
    },
    state::RunId,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use ra_tools::sandbox::memory::{MEMORY_SUMMARY_MAX_TOKENS, Memory, render_memory_read_prompt};
use serde_json::{Value, json};

const READ_LIVE_UPDATE: &str = include_str!("fixtures/sandbox_memory/read_live_update.md");
const READ_ONLY: &str = include_str!("fixtures/sandbox_memory/read_only.md");

// ---- helpers --------------------------------------------------------------------------------

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

/// The reference's `Memory(generate=None)`.
fn read_only_generation_off() -> Memory {
    Memory::builder().generate(None).build().unwrap()
}

/// Writes the summary as the reference's tests do, through `Path(memories_dir)`.
async fn write_summary(session: &Arc<dyn SandboxSession>, memories_dir: &str, summary: &[u8]) {
    let directory = PosixPath::new(memories_dir);
    session
        .mkdir(SessionPath::Posix(&directory), true, None)
        .await
        .unwrap();
    session
        .write(
            SessionPath::Posix(&directory.join("memory_summary.md")),
            summary.to_vec(),
            None,
        )
        .await
        .unwrap();
}

async fn instructions(capability: &Memory, session: &Arc<dyn SandboxSession>) -> Option<String> {
    capability
        .instructions_for(session.state().manifest())
        .await
        .unwrap()
}

fn scope(cwd: &str) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::from_cwd(Some(cwd)).unwrap()
}

fn build_error(builder: ra_tools::sandbox::memory::MemoryBuilder) -> String {
    builder.build().unwrap_err().to_string()
}

fn config_from(value: Value) -> Result<MemoryGenerateConfig, serde_json::Error> {
    serde_json::from_value(value)
}

// ---- test_memory.py -------------------------------------------------------------------------

#[tokio::test]
async fn without_a_summary_there_are_no_memory_instructions() {
    let (_directory, _root, session) = live_session().await;
    let capability = read_only_generation_off().bound_to(Arc::clone(&session));

    assert_eq!(instructions(&capability, &session).await, None);

    write_summary(&session, "memories", b"").await;

    assert_eq!(instructions(&capability, &session).await, None);
    session.close().await.unwrap();
}

#[rstest::rstest]
#[case::absolute("/memory", "memories_dir must be relative")]
#[case::escaping("../memory", "memories_dir must not escape root")]
#[case::empty("", "memories_dir must be non-empty")]
#[case::dot(".", "memories_dir must be non-empty")]
fn an_invalid_memories_dir_is_refused(#[case] memories_dir: &str, #[case] expected: &str) {
    let error = build_error(
        Memory::builder()
            .layout(MemoryLayoutConfig::new().with_memories_dir(memories_dir))
            .generate(None),
    );

    assert!(error.contains(expected), "{error}");
}

#[rstest::rstest]
#[case::absolute("/sessions", "sessions_dir must be relative")]
#[case::escaping("../sessions", "sessions_dir must not escape root")]
#[case::empty("", "sessions_dir must be non-empty")]
#[case::dot(".", "sessions_dir must be non-empty")]
fn an_invalid_sessions_dir_is_refused(#[case] sessions_dir: &str, #[case] expected: &str) {
    let error = build_error(
        Memory::builder()
            .layout(MemoryLayoutConfig::new().with_sessions_dir(sessions_dir))
            .generate(None),
    );

    assert!(error.contains(expected), "{error}");
}

#[test]
fn memory_needs_reading_or_generating() {
    let error = build_error(Memory::builder().read(None).generate(None));

    assert!(
        error.contains("Memory requires at least one of `read` or `generate`"),
        "{error}"
    );
}

#[tokio::test]
async fn reading_needs_a_bound_session() {
    let error = read_only_generation_off()
        .instructions_for(&Manifest::new())
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("Memory capability is not bound to a SandboxSession"),
        "{error}"
    );
}

#[test]
fn a_consolidation_limit_of_zero_is_refused() {
    let error = MemoryGenerateConfig::new()
        .with_max_raw_memories_for_consolidation(0)
        .unwrap_err();

    assert!(
        error.to_string().contains(
            "MemoryGenerateConfig.max_raw_memories_for_consolidation must be greater than 0"
        ),
        "{error}"
    );
}

#[test]
fn the_default_layout_uses_codex_s_names() {
    let config = MemoryLayoutConfig::new();

    assert_eq!(config.memories_dir(), "memories");
    assert_eq!(config.sessions_dir(), "sessions");
}

#[test]
fn the_consolidation_limit_is_configurable() {
    let config = MemoryGenerateConfig::new()
        .with_max_raw_memories_for_consolidation(123)
        .unwrap();

    assert_eq!(config.max_raw_memories_for_consolidation(), 123);
}

#[test]
fn phase_settings_are_read_from_their_serialized_form() {
    let config = config_from(json!({
        "phase_one_model_settings": {"effort": "low", "retry": {"max_retries": 0}},
        "phase_two_model_settings": {"temperature": 0.0},
    }))
    .unwrap();

    let phase_one = config.phase_one_model_settings().unwrap();
    assert_eq!(phase_one.effort(), Some(Effort::Low));
    assert_eq!(phase_one.retry().unwrap().max_retries(), Some(0));
    assert_eq!(
        config.phase_two_model_settings().unwrap().temperature(),
        Some(0.0)
    );
}

#[test]
fn phase_settings_given_as_values_are_kept_as_given() {
    let phase_one = ModelSettings::new().with_effort(Effort::Low);
    let phase_two = ModelSettings::new().with_temperature(0.2);

    let config = MemoryGenerateConfig::new()
        .with_phase_one_model_settings(Some(phase_one.clone()))
        .with_phase_two_model_settings(Some(phase_two.clone()));

    assert_eq!(config.phase_one_model_settings(), Some(&phase_one));
    assert_eq!(config.phase_two_model_settings(), Some(&phase_two));
}

#[rstest::rstest]
#[case::phase_one("phase_one_model_settings")]
#[case::phase_two("phase_two_model_settings")]
fn unknown_phase_settings_survive_a_round_trip(#[case] field: &str) {
    let config = config_from(json!({field: {"future_option": "enabled"}})).unwrap();

    let written = serde_json::to_value(&config).unwrap();

    assert_eq!(written[field]["future_option"], json!("enabled"));
}

#[rstest::rstest]
#[case::phase_one("phase_one_model_settings")]
#[case::phase_two("phase_two_model_settings")]
fn phase_settings_that_are_not_settings_are_refused(#[case] field: &str) {
    assert!(config_from(json!({field: "invalid"})).is_err());
}

#[test]
fn phase_settings_can_be_turned_off() {
    let built = MemoryGenerateConfig::new()
        .with_phase_one_model_settings(None)
        .with_phase_two_model_settings(None);
    let read = config_from(json!({
        "phase_one_model_settings": null,
        "phase_two_model_settings": null,
    }))
    .unwrap();

    for config in [built, read] {
        assert_eq!(config.phase_one_model_settings(), None);
        assert_eq!(config.phase_two_model_settings(), None);
    }
}

#[test]
fn a_consolidation_limit_above_4096_is_refused() {
    let built = MemoryGenerateConfig::new()
        .with_max_raw_memories_for_consolidation(4097)
        .unwrap_err()
        .to_string();
    let read = config_from(json!({"max_raw_memories_for_consolidation": 4097}))
        .unwrap_err()
        .to_string();

    for error in [built, read] {
        assert!(
            error.contains(
                "MemoryGenerateConfig.max_raw_memories_for_consolidation must be less than or \
                 equal to 4096"
            ),
            "{error}"
        );
    }
}

/// The reference lowers the token limit to 8 to trigger truncation; the limit is a constant here,
/// so the summary is made longer than it instead.
#[tokio::test]
async fn a_long_summary_is_truncated_into_the_instructions() {
    let (_directory, _root, session) = live_session().await;
    let limit = usize::try_from(MEMORY_SUMMARY_MAX_TOKENS).unwrap();
    let summary = "abcdefghijklmnopqrstuvwxyz".repeat(limit);
    write_summary(&session, "memories", summary.as_bytes()).await;
    let capability = read_only_generation_off().bound_to(Arc::clone(&session));

    let text = instructions(&capability, &session).await.unwrap();

    assert!(
        text.contains("memories/memory_summary.md (already provided below; do NOT open again)")
    );
    assert!(text.contains("MEMORY_SUMMARY BEGINS"));
    assert!(text.contains("tokens truncated"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn live_updates_let_the_agent_edit_memory() {
    let (_directory, _root, session) = live_session().await;
    write_summary(&session, "memories", b"summary entry").await;
    let capability = read_only_generation_off().bound_to(Arc::clone(&session));

    let text = instructions(&capability, &session).await.unwrap();

    assert!(text.contains("Memory is writable."));
    assert!(text.contains("memories/MEMORY.md"));
    assert!(text.contains("same turn"));
    assert!(!text.contains("Never update memories."));
    // Byte for byte what the reference renders.
    assert_eq!(text, READ_LIVE_UPDATE);
    session.close().await.unwrap();
}

#[tokio::test]
async fn with_a_run_working_directory_memory_paths_are_absolute() {
    let (_directory, root, session) = live_session().await;
    write_summary(&session, "memories", b"summary entry").await;
    let capability = read_only_generation_off()
        .bound_to(Arc::clone(&session))
        .with_workspace_scope(scope("tasks/task-a"));

    let text = instructions(&capability, &session).await.unwrap();

    let root = root.to_string_lossy();
    assert!(text.contains(&format!(
        "{root}/memories/memory_summary.md (already provided below; do NOT open again)"
    )));
    assert!(text.contains(&format!("{root}/memories/MEMORY.md")));
    assert!(text.contains("summary entry"));
    session.close().await.unwrap();
}

/// The reference keeps the configured spelling when there is no working directory, and on a POSIX
/// host that spelling is the directory the summary was read from, a backslash included.
#[rstest::rstest]
#[case::backslash("team\\memory", "team\\memory")]
#[case::doubled_separator("team//memory", "team//memory")]
#[tokio::test]
async fn without_a_working_directory_the_layout_is_shown_as_configured(
    #[case] memories_dir: &str,
    #[case] shown: &str,
) {
    let (_directory, root, session) = live_session().await;
    write_summary(&session, memories_dir, b"summary entry").await;
    let capability = Memory::builder()
        .layout(MemoryLayoutConfig::new().with_memories_dir(memories_dir))
        .generate(None)
        .build()
        .unwrap()
        .bound_to(Arc::clone(&session));

    let text = instructions(&capability, &session).await.unwrap();

    assert!(text.contains(&format!("{shown}/memory_summary.md")));
    assert!(text.contains(&format!("{shown}/MEMORY.md")));
    assert!(root.join(shown).join("memory_summary.md").is_file());
    session.close().await.unwrap();
}

/// The reference's typed path keeps a backslash as part of the name, and so does the session here:
/// the absolute path the prompt names is the file that was read, a directory called `team\\memory`.
#[tokio::test]
async fn with_a_working_directory_the_layout_is_a_typed_path() {
    let (_directory, root, session) = live_session().await;
    write_summary(&session, "team\\memory", b"summary entry").await;
    let capability = Memory::builder()
        .layout(MemoryLayoutConfig::new().with_memories_dir("team\\memory"))
        .generate(None)
        .build()
        .unwrap()
        .bound_to(Arc::clone(&session))
        .with_workspace_scope(scope("tasks/task-a"));

    let text = instructions(&capability, &session).await.unwrap();

    let memory_dir = root.join("team\\memory");
    assert!(memory_dir.join("memory_summary.md").is_file());
    assert!(!root.join("team").exists());
    assert!(text.contains(&format!("{}/memory_summary.md", memory_dir.display())));
    assert!(text.contains(&format!("{}/MEMORY.md", memory_dir.display())));
    session.close().await.unwrap();
}

/// A backslash does not make a layout directory climb out: `..\\memory` is one name, as the
/// reference's `Path` parts read it.
#[test]
fn a_backslash_is_part_of_a_layout_directory_name() {
    for layout in [
        MemoryLayoutConfig::new().with_memories_dir("..\\memory"),
        MemoryLayoutConfig::new().with_sessions_dir("..\\sessions"),
    ] {
        Memory::builder()
            .layout(layout)
            .generate(None)
            .build()
            .unwrap();
    }
}

// ---- beyond the reference's tests -----------------------------------------------------------

#[test]
fn the_read_only_prompt_is_the_reference_s() {
    assert_eq!(
        render_memory_read_prompt("/workspace/memories", "summary entry", false),
        READ_ONLY
    );
}

#[test]
fn reading_needs_a_shell_and_live_updates_a_filesystem_too() {
    let live = Memory::new();
    let read_only = Memory::builder()
        .read(Some(MemoryReadConfig::new().with_live_update(false)))
        .build()
        .unwrap();
    let generate_only = Memory::builder().read(None).build().unwrap();

    assert_eq!(live.kind(), CapabilityFamily::MEMORY);
    assert_eq!(
        live.required_capabilities().into_iter().collect::<Vec<_>>(),
        [CapabilityFamily::FILESYSTEM, CapabilityFamily::SHELL]
    );
    assert_eq!(
        read_only
            .required_capabilities()
            .into_iter()
            .collect::<Vec<_>>(),
        [CapabilityFamily::SHELL]
    );
    assert!(generate_only.required_capabilities().is_empty());
}

#[tokio::test]
async fn without_reading_there_are_no_instructions_even_unbound() {
    let capability = Memory::builder().read(None).build().unwrap();

    assert_eq!(
        capability.instructions_for(&Manifest::new()).await.unwrap(),
        None
    );
}

#[test]
fn the_defaults_are_the_reference_s() {
    let memory = Memory::new();
    let generate = memory.generate().unwrap();

    assert!(memory.read().unwrap().live_update());
    assert_eq!(generate.max_raw_memories_for_consolidation(), 256);
    assert_eq!(generate.phase_one_model().name(), Some("gpt-5.4-mini"));
    assert_eq!(generate.phase_two_model().name(), Some("gpt-5.5"));
    let medium = ModelSettings::new().with_effort(Effort::Medium);
    assert_eq!(generate.phase_one_model_settings(), Some(&medium));
    assert_eq!(generate.phase_two_model_settings(), Some(&medium));
    assert_eq!(generate.extra_prompt(), None);
    assert_eq!(config_from(json!({})).unwrap(), MemoryGenerateConfig::new());
}

#[tokio::test]
async fn a_bound_capability_reads_through_the_binding() {
    let (_directory, root, session) = live_session().await;
    write_summary(&session, "memories", b"summary entry").await;
    let binding = SandboxBinding::new(
        Arc::clone(&session),
        None,
        scope("tasks/task-a"),
        session.state().manifest().clone(),
    );

    let bound = read_only_generation_off()
        .bind_sandbox(&binding)
        .unwrap()
        .unwrap();
    let section = bound.instructions().await.unwrap().unwrap();

    assert!(
        section
            .content()
            .contains(&format!("{}/memories/MEMORY.md", root.to_string_lossy()))
    );
    assert_eq!(
        read_only_generation_off().instructions().await.unwrap(),
        None
    );
    session.close().await.unwrap();
}

#[test]
fn an_unbound_capability_is_refused_on_the_run_configuration() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("memory"))
        .name("Memory")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("run-sandbox-memory"), &agent);

    let error = Memory::new().bind(&run).err().unwrap();

    assert!(
        error
            .to_string()
            .contains("Memory capability is not bound to a SandboxSession")
    );
}

/// A model that is only ever held, never called.
struct IdleModel;

#[async_trait::async_trait]
impl Model for IdleModel {
    async fn get_response(&self, _request: ModelRequest) -> ra_core::error::Result<ModelResponse> {
        unreachable!("a phase model held by the configuration is not called here")
    }
}

#[test]
fn a_phase_model_is_a_name_or_a_shared_instance() {
    let first: Arc<dyn Model> = Arc::new(IdleModel);
    let second: Arc<dyn Model> = Arc::new(IdleModel);
    let config = MemoryGenerateConfig::new()
        .with_phase_one_model(Arc::clone(&first))
        .with_phase_two_model("custom-consolidator");

    let copied = config.clone();

    assert!(Arc::ptr_eq(
        copied.phase_one_model().instance().unwrap(),
        &first
    ));
    assert_eq!(copied.phase_one_model().name(), None);
    assert_eq!(copied.phase_two_model().name(), Some("custom-consolidator"));
    assert_eq!(copied, config);
    assert_ne!(
        config.clone().with_phase_one_model(Arc::clone(&second)),
        config
    );
    assert_ne!(
        MemoryModel::from(Arc::clone(&first)),
        MemoryModel::from("gpt-5.4-mini")
    );
}

#[test]
fn a_named_phase_model_round_trips_and_an_instance_does_not_serialize() {
    let named = MemoryGenerateConfig::new().with_phase_one_model("custom-extractor");
    let value = serde_json::to_value(&named).unwrap();

    assert_eq!(value["phase_one_model"], json!("custom-extractor"));
    assert_eq!(config_from(value).unwrap(), named);

    let live = MemoryGenerateConfig::new().with_phase_one_model(Arc::new(IdleModel));
    let error = serde_json::to_value(&live).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("a live memory model instance cannot be serialized"),
        "{error}"
    );
}
