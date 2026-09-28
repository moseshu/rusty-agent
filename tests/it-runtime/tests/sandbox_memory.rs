//! `ra-runtime::sandbox::memory`: the files, prompts and rules memory generation runs on.
//!
//! Ported from the parts of the reference's `tests/sandbox/test_memory.py` that exercise them
//! directly, in upstream order; the generation manager and the runner hooks come with the next
//! batch. Storage runs against a real local session, where the reference uses its in-memory
//! filesystem session. The rendered prompts, the phase-one input, a rollout line and the selection
//! file are compared byte for byte with fixtures the reference itself rendered.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use ra_core::sandbox::{
    CreateRequest, ErrorCode, Manifest, MemoryLayoutConfig, PosixPath, SandboxClient,
    SandboxSession,
};
use ra_runtime::sandbox::memory::phase_one::{
    PHASE_ONE_ROLLOUT_TOKEN_LIMIT, RolloutExtractionArtifacts, normalize_rollout_slug,
    render_phase_one_prompt, rollout_extraction_artifacts_json_schema,
    rollout_id_from_rollout_path, validate_rollout_artifacts,
};
use ra_runtime::sandbox::memory::prompts::{
    MEMORY_CONSOLIDATION_PROMPT_TEMPLATE, ROLLOUT_EXTRACTION_PROMPT_TEMPLATE,
    render_memory_consolidation_prompt, render_rollout_extraction_prompt,
    render_rollout_extraction_user_prompt,
};
use ra_runtime::sandbox::memory::rollouts::{
    RolloutTerminalMetadata, RolloutTerminalState, dump_rollout_json,
    rollout_file_name_for_rollout_id, write_rollout,
};
use ra_runtime::sandbox::memory::storage::{
    PhaseTwoInputSelection, PhaseTwoSelectionItem, SandboxMemoryStorage, updated_at_sort_key,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use serde::Serialize;
use serde_json::{Value, json};

const EXTRACTION_EXTRA: &str = include_str!("fixtures/sandbox_memory/extraction_extra.md");
const CONSOLIDATION: &str = include_str!("fixtures/sandbox_memory/consolidation.md");
const PHASE_ONE_SINGLE: &str = include_str!("fixtures/sandbox_memory/phase_one_single.md");
const PHASE_ONE_MULTI: &str = include_str!("fixtures/sandbox_memory/phase_one_multi.md");
const SEGMENT_LINE: &str = include_str!("fixtures/sandbox_memory/segment.jsonl");
const SELECTION_FILE: &str = include_str!("fixtures/sandbox_memory/selection.json");

// ---- helpers --------------------------------------------------------------------------------

/// A started local session over a fresh directory, and that directory.
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

fn item(
    rollout_id: &str,
    updated_at: &str,
    rollout_path: &str,
    rollout_summary_file: &str,
    terminal_state: &str,
) -> PhaseTwoSelectionItem {
    PhaseTwoSelectionItem::new(
        rollout_id,
        updated_at,
        rollout_path,
        rollout_summary_file,
        terminal_state,
    )
}

/// The reference test's `_raw_memory_record`: the header the manager writes, then the memory.
fn raw_memory_record(
    rollout_id: &str,
    updated_at: &str,
    rollout_summary_file: &str,
    raw_memory: &str,
) -> String {
    format!(
        "rollout_id: {rollout_id}\nupdated_at: {updated_at}\nrollout_path: \
         sessions/{rollout_id}.jsonl\nrollout_summary_file: {rollout_summary_file}\n\
         terminal_state: completed\n\n{raw_memory}\n"
    )
}

fn empty_selection() -> PhaseTwoInputSelection {
    PhaseTwoInputSelection::default()
}

/// A segment as the manager writes one, fields in the reference's order.
#[derive(Serialize)]
struct Segment {
    updated_at: &'static str,
    rollout_id: &'static str,
    input: Vec<InputMessage>,
    generated_items: Vec<Value>,
    terminal_metadata: RolloutTerminalMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_output: Option<&'static str>,
}

#[derive(Serialize)]
struct InputMessage {
    role: &'static str,
    content: &'static str,
}

fn completed_segment() -> Segment {
    Segment {
        updated_at: "2026-01-01T00:00:00+00:00",
        rollout_id: "chat-1",
        input: vec![InputMessage {
            role: "user",
            content: "h\u{e9}llo \u{1f600}",
        }],
        generated_items: Vec::new(),
        terminal_metadata: RolloutTerminalMetadata::new(RolloutTerminalState::Completed, true),
        final_output: Some("done"),
    }
}

fn failed_segment() -> Segment {
    Segment {
        updated_at: "2026-01-01T00:01:00+00:00",
        rollout_id: "chat-1",
        input: vec![InputMessage {
            role: "user",
            content: "again",
        }],
        generated_items: Vec::new(),
        terminal_metadata: RolloutTerminalMetadata::new(RolloutTerminalState::Failed, false)
            .with_exception("RuntimeError", "boom"),
        final_output: None,
    }
}

// ---- test_memory.py -------------------------------------------------------------------------

#[test]
fn a_file_safe_rollout_id_names_its_file_directly() {
    assert_eq!(
        rollout_file_name_for_rollout_id("chat-session.2026_04").unwrap(),
        "chat-session.2026_04.jsonl"
    );
}

#[test]
fn a_path_like_rollout_id_is_refused() {
    let error = rollout_file_name_for_rollout_id("../chat-session").unwrap_err();

    assert!(error.to_string().contains("file-safe ID"), "{error}");
}

#[test]
fn an_empty_rollout_id_is_refused() {
    let error = rollout_file_name_for_rollout_id(" ").unwrap_err();

    assert!(error.to_string().contains("file-safe ID"), "{error}");
}

#[test]
fn a_large_rollout_is_truncated_in_the_phase_one_prompt_with_a_notice() {
    let content = format!(
        "start{}middle{}end",
        "a".repeat(700_000),
        "z".repeat(700_000)
    );
    let payload = json!({
        "input": [{"role": "user", "content": content}],
        "generated_items": [],
        "terminal_metadata": {"terminal_state": "completed", "has_final_output": false},
    });

    let prompt = render_phase_one_prompt(&dump_rollout_json(&payload).unwrap()).unwrap();

    assert!(prompt.contains("start"));
    assert!(prompt.contains("end"));
    assert!(!prompt.contains("middle"));
    assert!(prompt.contains("tokens truncated"));
    assert!(prompt.contains("rollout content omitted"));
    assert!(prompt.contains("Do not assume the rendered rollout below is complete"));
}

#[test]
fn the_prompts_carry_no_guidance_section_by_default() {
    let rollout_prompt = render_rollout_extraction_prompt(None);
    let consolidation_prompt =
        render_memory_consolidation_prompt("memory", &empty_selection(), None);

    for prompt in [&rollout_prompt, &consolidation_prompt] {
        assert!(!prompt.contains("{{ extra_prompt_section }}"));
        assert!(!prompt.contains("DEVELOPER-SPECIFIC EXTRA GUIDANCE"));
    }
    assert_eq!(
        rollout_prompt,
        ROLLOUT_EXTRACTION_PROMPT_TEMPLATE.replace("{{ extra_prompt_section }}", "")
    );
}

#[test]
fn the_prompts_carry_the_developer_s_guidance() {
    let rollout_prompt = render_rollout_extraction_prompt(Some("Focus on user preferences."));
    let consolidation_prompt = render_memory_consolidation_prompt(
        "memory",
        &empty_selection(),
        Some("Focus on user preferences."),
    );

    for prompt in [&rollout_prompt, &consolidation_prompt] {
        assert!(prompt.contains("DEVELOPER-SPECIFIC EXTRA GUIDANCE"));
        assert!(prompt.contains("Focus on user preferences."));
    }
}

#[test]
fn the_consolidation_prompt_lists_removed_rollouts() {
    let selection = PhaseTwoInputSelection::new(
        Vec::new(),
        BTreeSet::new(),
        vec![item(
            "old-rollout",
            "",
            "sessions/old-rollout.jsonl",
            "memories/rollout_summaries/old.md",
            "completed",
        )],
    );

    let prompt = render_memory_consolidation_prompt("memory", &selection, None);

    assert!(prompt.contains("- removed from the last successful Phase 2 run: 1"));
    assert!(prompt.contains("rollout_id=old-rollout"));
    assert!(prompt.contains("updated_at=unknown"));
}

#[test]
fn raw_memories_with_unknown_timestamps_sort_last() {
    assert!(
        updated_at_sort_key("updated_at: 2025-03-01T00:00:00Z\n")
            > updated_at_sort_key("updated_at: unknown\n")
    );
    assert_eq!(
        updated_at_sort_key("updated_at: unknown\n"),
        updated_at_sort_key("updated_at:\n")
    );
    assert_eq!(
        updated_at_sort_key("updated_at: unknown\n"),
        updated_at_sort_key("no metadata\n")
    );
}

#[tokio::test]
async fn the_selection_tracks_added_retained_and_removed_rollouts() {
    let (_directory, _root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());
    storage.ensure_layout().await.unwrap();
    let old_item = item(
        "old-rollout",
        "2025-03-01T00:00:00Z",
        "sessions/old-rollout.jsonl",
        "rollout_summaries/old-rollout.md",
        "completed",
    );
    storage
        .write_text(
            &storage.raw_memories_dir().join("old-rollout.md"),
            &raw_memory_record(
                "old-rollout",
                "2025-03-01T00:00:00Z",
                "rollout_summaries/old-rollout.md",
                "old raw",
            ),
        )
        .await
        .unwrap();
    storage
        .write_text(
            &storage.raw_memories_dir().join("new-rollout.md"),
            &raw_memory_record(
                "new-rollout",
                "2025-03-02T00:00:00Z",
                "rollout_summaries/new-rollout.md",
                "new raw",
            ),
        )
        .await
        .unwrap();
    storage
        .write_phase_two_selection(std::slice::from_ref(&old_item))
        .await
        .unwrap();

    let selection = storage.build_phase_two_input_selection(1).await.unwrap();

    let ids = |items: &[PhaseTwoSelectionItem]| -> Vec<String> {
        items
            .iter()
            .map(|item| item.rollout_id().to_owned())
            .collect()
    };
    assert_eq!(ids(selection.selected()), ["new-rollout"]);
    assert!(selection.retained_rollout_ids().is_empty());
    assert_eq!(ids(selection.removed()), ["old-rollout"]);
    session.close().await.unwrap();
}

// ---- beyond the reference's tests -----------------------------------------------------------

#[test]
fn the_guidance_and_the_selection_render_as_the_reference_s() {
    let selection = PhaseTwoInputSelection::new(
        vec![
            item(
                "new-rollout",
                "2025-03-02T00:00:00Z",
                "sessions/new-rollout.jsonl",
                "rollout_summaries/new-rollout_slug.md",
                "completed",
            ),
            item(
                "kept-rollout",
                "",
                "sessions/kept-rollout.jsonl",
                "rollout_summaries/kept-rollout_slug.md",
                "failed",
            ),
        ],
        BTreeSet::from(["kept-rollout".to_owned()]),
        vec![item(
            "old-rollout",
            "",
            "sessions/old-rollout.jsonl",
            "rollout_summaries/old.md",
            "completed",
        )],
    );

    assert_eq!(
        render_rollout_extraction_prompt(Some("  Focus on user preferences.\n")),
        EXTRACTION_EXTRA
    );
    assert_eq!(
        render_memory_consolidation_prompt(
            "memories",
            &selection,
            Some("Focus on user preferences.")
        ),
        CONSOLIDATION
    );
    assert!(!MEMORY_CONSOLIDATION_PROMPT_TEMPLATE.is_empty());
}

#[test]
fn a_rollout_line_is_written_as_the_reference_writes_it() {
    assert_eq!(
        dump_rollout_json(&completed_segment()).unwrap(),
        SEGMENT_LINE
    );
}

#[test]
fn the_phase_one_input_is_the_reference_s_for_one_and_for_several_segments() {
    let single = dump_rollout_json(&completed_segment()).unwrap();
    let several = format!("{single}{}", dump_rollout_json(&failed_segment()).unwrap());

    assert_eq!(render_phase_one_prompt(&single).unwrap(), PHASE_ONE_SINGLE);
    assert_eq!(render_phase_one_prompt(&several).unwrap(), PHASE_ONE_MULTI);
}

#[test]
fn the_phase_one_input_needs_a_record_and_valid_json() {
    let empty = render_phase_one_prompt("\n  \n").unwrap_err();
    let invalid = render_phase_one_prompt("{not json}\n").unwrap_err();

    assert!(
        empty
            .to_string()
            .contains("rollout_contents must contain at least one JSONL record"),
        "{empty}"
    );
    assert!(invalid.to_string().contains("invalid rollout JSONL record"));
}

#[test]
fn the_user_message_is_filled_in_one_pass() {
    let prompt = render_rollout_extraction_user_prompt("{rollout_contents}", "body");

    assert!(prompt.contains("```json\n{rollout_contents}\n```"));
    assert!(prompt.contains("Filtered session:\nbody\n"));
}

#[test]
fn rollout_slugs_are_normalized_or_refused() {
    assert_eq!(
        normalize_rollout_slug(" task_memory.md ").unwrap(),
        "task_memory"
    );
    assert_eq!(normalize_rollout_slug("a-1").unwrap(), "a-1");
    for invalid in ["", "Task", "-task", "task memory", &"a".repeat(81)] {
        let error = normalize_rollout_slug(invalid).unwrap_err();
        assert!(
            error.to_string().contains("Invalid rollout_slug: '"),
            "{error}"
        );
    }
}

#[test]
fn a_rollout_id_is_read_from_its_file_name() {
    assert_eq!(
        rollout_id_from_rollout_path("sessions/chat-1.jsonl").unwrap(),
        "chat-1"
    );
    assert_eq!(
        rollout_id_from_rollout_path("chat.2026.jsonl").unwrap(),
        "chat.2026"
    );
    let error = rollout_id_from_rollout_path("sessions/.jsonl").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Invalid rollout id for memory: 'sessions/.jsonl'")
    );
}

#[test]
fn blank_artifacts_decline_and_partly_blank_ones_are_refused() {
    let blank = RolloutExtractionArtifacts::new(" ", "", "\n");
    let partial = RolloutExtractionArtifacts::new("slug", " ", "memory");
    let complete = RolloutExtractionArtifacts::new("slug", "summary", "memory");

    assert!(!validate_rollout_artifacts(&blank).unwrap());
    assert!(
        validate_rollout_artifacts(&partial)
            .unwrap_err()
            .to_string()
            .contains("Phase 1 returned partially-empty memory artifacts.")
    );
    assert!(validate_rollout_artifacts(&complete).unwrap());
    assert_eq!(
        rollout_extraction_artifacts_json_schema()["required"],
        json!(["rollout_slug", "rollout_summary", "raw_memory"])
    );
    assert_eq!(PHASE_ONE_ROLLOUT_TOKEN_LIMIT, 150_000);
}

#[tokio::test]
async fn the_layout_is_created_without_overwriting_existing_memory() {
    let (_directory, root, session) = live_session().await;
    std::fs::create_dir_all(root.join("memories")).unwrap();
    std::fs::write(root.join("memories/MEMORY.md"), "kept").unwrap();
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());

    storage.ensure_layout().await.unwrap();

    for directory in [
        "sessions",
        "memories/raw_memories",
        "memories/rollout_summaries",
        "memories/skills",
    ] {
        assert!(root.join(directory).is_dir(), "{directory}");
    }
    assert_eq!(
        std::fs::read_to_string(root.join("memories/MEMORY.md")).unwrap(),
        "kept"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("memories/memory_summary.md")).unwrap(),
        ""
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn segments_are_appended_to_one_rollout_file_or_a_fresh_one() {
    let (_directory, root, session) = live_session().await;

    let first = write_rollout(
        &session,
        &completed_segment(),
        "sessions",
        Some("chat-1.jsonl"),
    )
    .await
    .unwrap();
    let second = write_rollout(
        &session,
        &failed_segment(),
        "sessions",
        Some(" chat-1.jsonl "),
    )
    .await
    .unwrap();
    let fresh = write_rollout(&session, &json!({"a": 1}), "sessions", None)
        .await
        .unwrap();

    assert_eq!(first, PosixPath::new("sessions/chat-1.jsonl"));
    assert_eq!(second, first);
    let contents = std::fs::read_to_string(root.join("sessions/chat-1.jsonl")).unwrap();
    assert_eq!(
        contents,
        format!(
            "{}{}",
            dump_rollout_json(&completed_segment()).unwrap(),
            dump_rollout_json(&failed_segment()).unwrap()
        )
    );
    assert!(fresh.as_str().starts_with("sessions/") && fresh.as_str().ends_with(".jsonl"));
    assert_ne!(fresh, first);
    assert_eq!(
        std::fs::read_to_string(root.join(fresh.as_str())).unwrap(),
        "{\"a\":1}\n"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn rollout_file_names_and_directories_are_checked() {
    let (_directory, root, session) = live_session().await;

    for file_name in ["nested/chat.jsonl", "chat.json", "/chat.jsonl"] {
        let error = write_rollout(&session, &json!({}), "sessions", Some(file_name))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("file_name must be a simple .jsonl filename"),
            "{file_name}: {error}"
        );
    }
    for (path, message) in [
        (
            "/sessions",
            "rollouts_path must be relative to the sandbox workspace root",
        ),
        ("../sessions", "rollouts_path must not escape root"),
        (".", "rollouts_path must be non-empty"),
    ] {
        let error = write_rollout(&session, &json!({}), path, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{path}: {error}");
    }

    // A backslash is part of a name, as in the reference's `Path(...)`: `nested\\chat.jsonl` is one
    // file name, and `..\\sessions` one directory inside the workspace.
    let written = write_rollout(
        &session,
        &json!({}),
        "..\\sessions",
        Some("nested\\chat.jsonl"),
    )
    .await
    .unwrap();
    assert_eq!(written, PosixPath::new("..\\sessions/nested\\chat.jsonl"));
    assert!(
        root.join("..\\sessions")
            .join("nested\\chat.jsonl")
            .is_file()
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_segment_that_is_not_json_is_refused_and_nothing_is_written() {
    let (_directory, root, session) = live_session().await;
    write_rollout(&session, &json!({"a": 1}), "sessions", Some("chat-1.jsonl"))
        .await
        .unwrap();
    let unencodable = std::collections::BTreeMap::from([((1, 2), "tuple key")]);

    let error = write_rollout(&session, &unencodable, "sessions", Some("chat-1.jsonl"))
        .await
        .unwrap_err();
    let fresh = write_rollout(&session, &unencodable, "fresh", None)
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("rollout_contents must be valid JSON text"),
        "{error}"
    );
    assert!(
        fresh
            .to_string()
            .contains("rollout_contents must be valid JSON text")
    );
    assert!(dump_rollout_json(&unencodable).is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("sessions/chat-1.jsonl")).unwrap(),
        "{\"a\":1}\n"
    );
    assert!(!root.join("fresh").exists());
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_backslash_in_the_layout_is_part_of_the_directory_name() {
    let (_directory, root, session) = live_session().await;
    let layout = MemoryLayoutConfig::new()
        .with_memories_dir("team\\memory")
        .with_sessions_dir("team\\sessions");
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), layout);

    storage.ensure_layout().await.unwrap();
    let written = write_rollout(
        &session,
        &json!({"a": 1}),
        "team\\sessions",
        Some("chat-1.jsonl"),
    )
    .await
    .unwrap();

    assert_eq!(storage.memories_dir(), PosixPath::new("team\\memory"));
    assert_eq!(storage.sessions_dir(), PosixPath::new("team\\sessions"));
    assert!(root.join("team\\memory/MEMORY.md").is_file());
    assert!(root.join("team\\memory/raw_memories").is_dir());
    assert!(!root.join("team").exists());
    assert_eq!(written, PosixPath::new("team\\sessions/chat-1.jsonl"));
    assert!(root.join(written.as_str()).is_file());
    session.close().await.unwrap();
}

#[tokio::test]
async fn the_selection_file_is_written_as_the_reference_writes_it_and_read_back() {
    let (_directory, root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());
    storage.ensure_layout().await.unwrap();
    let items = vec![
        item(
            "new-rollout",
            "2025-03-02T00:00:00Z",
            "sessions/new-rollout.jsonl",
            "rollout_summaries/new-rollout_slug.md",
            "completed",
        ),
        item(
            "kept-rollout",
            "",
            "sessions/kept-rollout.jsonl",
            "rollout_summaries/kept-rollout_slug.md",
            "failed",
        ),
        item("\u{fc}n\u{ef}", "", "", "s.md", ""),
    ];

    storage.write_phase_two_selection(&items).await.unwrap();

    let written = std::fs::read_to_string(root.join("memories/phase_two_selection.json")).unwrap();
    let updated_at = serde_json::from_str::<Value>(&written).unwrap()["updated_at"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(updated_at.ends_with("+00:00"), "{updated_at}");
    assert_eq!(
        written.replace(&format!("\"{updated_at}\""), "\"X\""),
        SELECTION_FILE
    );
    assert_eq!(storage.read_phase_two_selection().await.unwrap(), items);
    session.close().await.unwrap();
}

#[tokio::test]
async fn an_unreadable_selection_is_no_selection() {
    let (_directory, root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());
    assert!(storage.read_phase_two_selection().await.unwrap().is_empty());
    storage.ensure_layout().await.unwrap();

    let valid_entry = item("7", "", "", "s.md", "");
    for (contents, expected) in [
        ("not json", Vec::new()),
        ("[]", Vec::new()),
        ("{\"selected\": {}}", Vec::new()),
        (
            "{\"selected\": [1, {\"rollout_id\": \"x\"}, \
             {\"rollout_id\": 7, \"rollout_summary_file\": \" s.md \"}]}",
            vec![valid_entry.clone()],
        ),
    ] {
        std::fs::write(root.join("memories/phase_two_selection.json"), contents).unwrap();
        assert_eq!(
            storage.read_phase_two_selection().await.unwrap(),
            expected,
            "{contents}"
        );
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn raw_memories_are_joined_for_consolidation_skipping_missing_ones() {
    let (_directory, root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());
    storage.ensure_layout().await.unwrap();
    std::fs::write(root.join("memories/raw_memories/a.md"), "first\n\n").unwrap();
    std::fs::write(root.join("memories/raw_memories/b.md"), "second\n").unwrap();
    let selected = |ids: &[&str]| -> Vec<PhaseTwoSelectionItem> {
        ids.iter().map(|id| item(id, "", "", "s.md", "")).collect()
    };

    assert!(
        !storage
            .rebuild_raw_memories(&selected(&["missing"]))
            .await
            .unwrap()
    );
    assert!(!root.join("memories/raw_memories.md").exists());
    assert!(
        storage
            .rebuild_raw_memories(&selected(&["a", "missing", "b"]))
            .await
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("memories/raw_memories.md")).unwrap(),
        "first\n\nsecond"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn the_selection_is_the_most_recent_raw_memories_that_name_their_rollout() {
    let (_directory, root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());
    storage.ensure_layout().await.unwrap();
    let raw = root.join("memories/raw_memories");
    std::fs::write(
        raw.join("old.md"),
        raw_memory_record(
            "old",
            "2025-01-01T00:00:00Z",
            "rollout_summaries/old.md",
            "x",
        ),
    )
    .unwrap();
    std::fs::write(
        raw.join("unknown.md"),
        raw_memory_record("unknown", "unknown", "rollout_summaries/unknown.md", "x"),
    )
    .unwrap();
    std::fs::write(
        raw.join("new.md"),
        raw_memory_record(
            "new",
            "2025-02-01T00:00:00Z",
            "rollout_summaries/new.md",
            "x",
        ),
    )
    .unwrap();
    std::fs::write(raw.join("headerless.md"), "no header\n").unwrap();
    std::fs::write(
        raw.join("notes.txt"),
        raw_memory_record("txt", "2030", "s.md", "x"),
    )
    .unwrap();
    std::fs::create_dir_all(raw.join("dir.md")).unwrap();

    let selection = storage.build_phase_two_input_selection(10).await.unwrap();

    let ids: Vec<&str> = selection
        .selected()
        .iter()
        .map(PhaseTwoSelectionItem::rollout_id)
        .collect();
    assert_eq!(ids, ["new", "old", "unknown"]);
    assert_eq!(selection.selected()[0].rollout_path(), "sessions/new.jsonl");
    assert_eq!(selection.selected()[0].terminal_state(), "completed");
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_missing_file_is_reported_as_missing() {
    let (_directory, _root, session) = live_session().await;
    let storage = SandboxMemoryStorage::new(Arc::clone(&session), MemoryLayoutConfig::new());

    let error = storage
        .read_text(&PosixPath::new("memories/absent.md"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    session.close().await.unwrap();
}
