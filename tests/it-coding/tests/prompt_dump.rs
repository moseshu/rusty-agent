//! The committed stable-prefix snapshot.
//!
//! The prefix is the span every provider's prompt cache holds, and cache hit rate is the dominant
//! cost driver of a long run. A one-word edit to a section, or two sections swapping order,
//! invalidates the cached span for every subsequent call — and reviewing the change that caused it
//! is impossible if the prefix only exists at runtime. So the assembled dump is written down and
//! diffed, exactly as the public API baseline is.
//!
//! Run with `BLESS_PROMPT_DUMP=1` to rewrite the snapshot after an intentional change, and let the
//! diff appear in review.

use std::path::PathBuf;

use std::sync::{Arc, Mutex, PoisonError};

use ra_coding::{
    CodingHost,
    prompt::{
        assemble_stable_prefix, assemble_stable_prefix_for_surface,
        assemble_stable_prefix_for_tools,
        dump::{
            PLACEHOLDER_CACHE_SCOPE, PromptDumpRequest, SectionChange, compare_prompt_dump,
            render_prompt_dump, render_prompt_dump_json, shipped_role_names,
        },
        tool_surface_snapshot,
    },
};
use ra_core::item::{Message, ModelInputItem, OutputPhase};
use ra_core::model::{Model, ModelRequest, ModelSettings, ProviderKey};
use ra_core::prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS, PromptRole};
use ra_core::tool::{DEFAULT_MAX_NO_PROGRESS_STREAK, Tool};
use ra_model::anthropic::{AnthropicAuth, AnthropicMessagesModel};
use ra_tools::{exec_command::ExecCommandTool, read_file::ReadFileTool};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TOOL_SURFACE_RENDER_COUNT: usize = 100;

/// Serializes the two snapshot writers against each other.
///
/// The two files are blessed by separate tests that the harness runs in parallel, and both read
/// the tool-surface baseline before writing. Unsynchronized, one test's write can truncate the
/// file the other is reading, turning a legitimate bless into a panic about a missing revision
/// marker — or, worse, into a bogus one about an unraised revision.
static BLESS_SNAPSHOTS: Mutex<()> = Mutex::new(());

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .join("prompt-dump.txt")
}

fn tool_surface_snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .join("tool-surface.txt")
}

/// The tools the product's host-backed builder actually installs.
///
/// The temporary workspace is dropped on the way out: what the surface reads is the advertised
/// schema, which must not depend on where the host happens to be rooted.
fn host_backed_tools() -> Vec<Arc<dyn Tool>> {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    vec![
        host.read_file_tool().expect("read_file builds"),
        host.grep_tool().expect("grep builds"),
        host.glob_tool().expect("glob builds"),
        host.apply_patch_tool().expect("apply_patch builds"),
        host.exec_command_tool().expect("exec_command builds"),
        host.write_stdin_tool().expect("write_stdin builds"),
    ]
}

/// The prefix the product's own host-backed builder assembles for the main role.
///
/// Read through `host_backed_surface` rather than rebuilt from a tool list, because a host-backed
/// prefix is no longer a function of the tools alone: each installed capability contributes a
/// section of its own, and a helper that skipped them would compare the agent against a prefix
/// nothing ships.
async fn host_backed_prefix() -> ra_prompt::assembler::StablePrefix {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    let assembled =
        ra_coding::host_backed_surface(&PromptRole::Main, &host, ra_coding::HOST_BACKED_PROFILE)
            .await
            .expect("the host-backed surface assembles");
    assemble_stable_prefix_for_surface(
        &PromptRole::Main,
        assembled.tool_surface(),
        assembled.capability_sections(),
    )
    .expect("host-backed product prefix must assemble")
}

fn section_content(prefix: &ra_prompt::assembler::StablePrefix, name: &str) -> String {
    prefix
        .sections()
        .iter()
        .find(|section| section.name().as_str() == name)
        .unwrap_or_else(|| panic!("the assembled prefix must carry a `{name}` section"))
        .content()
        .to_owned()
}

/// The assembled prefix matches the committed snapshot, section for section and hash for hash.
///
/// Rendered through [`render_prompt_dump`] — the entry point `ra prompt dump` calls — rather than
/// by composing a report here. Two compositions would have drifted at the first edit, and the point
/// of shipping the command is that a user can reproduce this file rather than take it on faith.
#[tokio::test]
async fn test_stable_prefix_matches_the_committed_snapshot() {
    let mut rendered = String::new();
    // Every role that ships gets a section, because the read-only roles are the ones whose text has
    // to agree with a narrowed tool surface — a prefix edit that quietly re-enables writing is
    // exactly what this snapshot is here to surface.
    for role in [
        PromptRole::Main,
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
        PromptRole::Coordinator,
    ] {
        rendered.push_str(&format!("### role: {role}\n"));
        rendered.push_str(
            &render_prompt_dump(&PromptDumpRequest::new().with_role(role.role_name()))
                .await
                .expect("the product prefix must assemble"),
        );
        rendered.push('\n');
    }

    let workspace = tempfile::tempdir().expect("workspace");
    rendered.push_str("### host_backed_role: main\n");
    rendered.push_str(
        &render_prompt_dump(
            &PromptDumpRequest::new()
                .with_role(PromptRole::Main.role_name())
                .with_workspace(workspace.path()),
        )
        .await
        .expect("the host-backed product prefix must assemble"),
    );
    rendered.push('\n');

    let path = snapshot_path();
    if std::env::var_os("BLESS_PROMPT_DUMP").is_some() {
        let _writing = lock_snapshots();
        refuse_bless_without_revision_bump(&rendered_tool_surface());
        std::fs::write(&path, &rendered).expect("snapshot must be writable");
        return;
    }

    let baseline = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing prompt dump snapshot at {}: {err}. Run with BLESS_PROMPT_DUMP=1 to create it",
            path.display()
        )
    });

    assert_eq!(
        baseline, rendered,
        "the assembled stable prefix changed; every cached prefix is invalidated by this. \
         Re-run with BLESS_PROMPT_DUMP=1 and let the diff be reviewed"
    );
}

/// A role that does not ship is refused, rather than reported on as a custom role.
///
/// `PromptRole::Custom` has no guidance text, so accepting a typo would answer it with a report
/// that looks entirely real and whose role section is a generated placeholder. A verification entry
/// point may fail; it may not quietly describe a different agent.
#[tokio::test]
async fn test_a_role_the_product_does_not_ship_is_refused() {
    let error = render_prompt_dump(&PromptDumpRequest::new().with_role("archaeologist"))
        .await
        .expect_err("an unknown role must not produce a report");
    let message = error.to_string();

    assert!(message.contains("archaeologist"));
    for role in shipped_role_names() {
        assert!(
            message.contains(role),
            "the refusal must list the roles that do ship, missing `{role}`: {message}"
        );
    }
}

/// A dump is rendered outside any run, so it records a placeholder scope rather than inventing one.
///
/// This is what makes the committed snapshot reproducible from a command line: a real run id would
/// differ on every invocation and the bytes would never match.
#[tokio::test]
async fn test_a_dump_records_a_placeholder_cache_scope_until_a_run_names_one() {
    let default = render_prompt_dump(&PromptDumpRequest::new())
        .await
        .expect("dump renders");
    assert!(default.contains(&format!("Cache Scope:          {PLACEHOLDER_CACHE_SCOPE}")));

    let named = render_prompt_dump(&PromptDumpRequest::new().with_cache_scope("session-42"))
        .await
        .expect("dump renders");
    assert!(named.contains("Cache Scope:          session-42"));
}

/// Comparing a build against its own recorded dump reports nothing, and says the cache survives.
#[tokio::test]
async fn test_a_dump_compared_against_itself_reports_no_change() {
    let request = PromptDumpRequest::new();
    let recorded = render_prompt_dump_json(&request)
        .await
        .expect("dump serializes");

    let diff = compare_prompt_dump(&request, &recorded)
        .await
        .expect("baseline is a dump");

    assert!(!diff.prefix_changed());
    assert!(diff.changes().is_empty());
    assert!(
        diff.render_text()
            .contains("Invalidated by:       nothing; cached prefixes survive")
    );
}

/// An insertion must not report every section below it as having moved.
///
/// Advertising a tool inserts the selection rules and the inventory they point at, which shifts the
/// index of everything after them, and a report that called all of those moved would bury the two
/// insertions under seven consequences of them. The comparison ranks sections among the ones both
/// dumps share, so only the real causes are named.
#[tokio::test]
async fn test_an_insertion_is_not_reported_as_moving_every_section_below_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let toolless = render_prompt_dump_json(&PromptDumpRequest::new())
        .await
        .expect("dump serializes");
    let host_backed = PromptDumpRequest::new().with_workspace(workspace.path());

    let diff = compare_prompt_dump(&host_backed, &toolless)
        .await
        .expect("baseline is a dump");

    assert!(diff.prefix_changed());
    let named: Vec<&str> = diff.changes().iter().map(SectionChange::name).collect();
    assert_eq!(
        named,
        [
            "tool_use",
            "tool_surface",
            "filesystem",
            "search",
            "apply_patch",
            "shell",
            "editing_verification"
        ],
        "only the added sections and the editing text that names an entry actually changed"
    );
    assert!(
        !diff
            .changes()
            .iter()
            .any(|change| matches!(change, SectionChange::Moved { .. })),
        "the relative order of the shared sections is unchanged, so nothing moved"
    );
}

/// Two sections swapping places is caught even though every section hash holds.
///
/// This is the invalidation no per-section hash can show: the joined prefix — the span a provider
/// caches — is rewritten while every row above is byte-identical. The baseline is hand-built,
/// because the assembler's canonical order is exactly what stops the product from producing one.
#[tokio::test]
async fn test_a_reorder_is_named_even_though_every_section_hash_holds() {
    let request = PromptDumpRequest::new();
    let mut recorded: serde_json::Value = serde_json::from_str(
        &render_prompt_dump_json(&request)
            .await
            .expect("dump serializes"),
    )
    .expect("a dump is JSON");

    let sections = recorded["sections"]
        .as_array_mut()
        .expect("a dump lists its sections");
    let autonomy = sections
        .iter()
        .position(|section| section["name"] == "autonomy")
        .expect("the prefix carries an autonomy section");
    sections.swap(autonomy, autonomy + 1);
    // A real reorder moves the prefix hash too; leaving the recorded one in place would make the
    // fixture describe a prefix that cannot exist.
    recorded["prefix_hash"] = serde_json::Value::String("0".repeat(64));

    let diff = compare_prompt_dump(&request, &recorded.to_string())
        .await
        .expect("baseline is a dump");

    assert!(diff.prefix_changed());
    let moved: Vec<&str> = diff
        .changes()
        .iter()
        .filter(|change| matches!(change, SectionChange::Moved { .. }))
        .map(SectionChange::name)
        .collect();
    assert_eq!(moved, ["autonomy", "channels"]);
    assert!(
        diff.render_text()
            .contains("Invalidated by:       autonomy, channels")
    );
}

/// The host-backed prompt records the exact tool that the same builder installs on the agent.
#[tokio::test]
async fn test_host_backed_agent_carries_the_tool_surface_prefix() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    let agent = ra_coding::build_agent_with_host(
        ra_core::agent::AgentId::new("coding-agent"),
        "Coding Agent",
        &PromptRole::Main,
        &host,
    )
    .await
    .expect("host-backed agent builds");
    let prefix = host_backed_prefix().await;

    assert_eq!(
        agent
            .instructions()
            .and_then(ra_core::agent::AgentInstructions::as_static),
        Some(prefix.system_instructions())
    );
    assert_eq!(agent.tools().len(), 6);
    assert!(prefix.system_instructions().contains("`apply_patch`"));
    assert!(prefix.system_instructions().contains("`exec_command`"));
    assert!(prefix.system_instructions().contains("`grep`"));
    assert!(prefix.system_instructions().contains("`glob`"));
    assert!(prefix.system_instructions().contains("`read_file`"));
    assert!(prefix.system_instructions().contains("`write_stdin`"));
}

/// Opening a workspace does not override a role's tool boundary.
#[tokio::test]
async fn test_host_backed_one_off_agents_carry_no_tools() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");

    let agent = ra_coding::build_agent_with_host(
        ra_core::agent::AgentId::new("coding-agent"),
        "Coding Agent",
        &PromptRole::OneOffAnswer,
        &host,
    )
    .await
    .expect("host-backed agent builds");

    assert!(agent.tools().is_empty(), "a one-off role answers with none");
    let instructions = agent
        .instructions()
        .and_then(ra_core::agent::AgentInstructions::as_static)
        .expect("agent has a stable prefix")
        .to_owned();
    // With no entries to choose among, the prefix carries neither the inventory nor the rules for
    // reading it.
    assert!(
        !instructions.contains("Available tools:") && !instructions.contains("Tool Use:"),
        "tool-free agents must not carry tool-selection guidance"
    );
}

/// A read-only role keeps the observing entries and loses what writes.
///
/// The two halves are one guarantee: withholding the editing entries is what the role text
/// promises, and withholding the search entries as well would leave a read-only specialist unable
/// to inspect either an identified file or an unknown workspace.
#[tokio::test]
async fn test_host_backed_read_only_agents_keep_the_observing_entries() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");

    for role in [PromptRole::ReadOnlySpecialist, PromptRole::Planner] {
        let agent = ra_coding::build_agent_with_host(
            ra_core::agent::AgentId::new("coding-agent"),
            "Coding Agent",
            &role,
            &host,
        )
        .await
        .expect("host-backed agent builds");

        let names = agent
            .tools()
            .iter()
            .map(|tool| tool.origin().name().to_owned())
            .collect::<Vec<_>>();
        // Lookup-key order, not the order the capabilities contributed them: the surface is
        // assembled by a profile, and a tool table whose bytes depended on host startup order
        // would never hit a cached prefix twice.
        assert_eq!(
            names,
            vec!["glob", "grep", "read_file"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "role `{role}`"
        );

        let instructions = agent
            .instructions()
            .and_then(ra_core::agent::AgentInstructions::as_static)
            .expect("agent has a stable prefix")
            .to_owned();
        assert!(
            instructions.contains("`read_file`"),
            "role `{role}` must inventory the entry it received"
        );
        assert!(
            instructions.contains("`grep`"),
            "role `{role}` must inventory grep"
        );
        assert!(
            instructions.contains("`glob`"),
            "role `{role}` must inventory glob"
        );
        // Both halves of the denial: the entry is absent from the inventory, and the editing
        // section does not instruct the agent to reach for it anyway.
        assert!(
            !instructions.contains("`apply_patch`"),
            "role `{role}` must not name an unavailable editing entry"
        );
        assert!(
            !instructions.contains("`exec_command`") && !instructions.contains("`write_stdin`"),
            "role `{role}` must not name an unavailable execution entry"
        );
    }
}

/// The tool list and its fingerprint are insensitive to host registration order.
#[test]
fn test_tool_surface_is_sorted_and_byte_stable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    let read_file: Arc<dyn Tool> = Arc::new(ReadFileTool::new().expect("read_file builds"));
    let exec_command: Arc<dyn Tool> =
        Arc::new(ExecCommandTool::new().expect("exec_command builds"));
    let apply_patch = host.apply_patch_tool().expect("apply_patch builds");
    let one_order = vec![
        Arc::clone(&exec_command),
        Arc::clone(&apply_patch),
        Arc::clone(&read_file),
    ];
    let other_order = vec![
        Arc::clone(&read_file),
        Arc::clone(&apply_patch),
        Arc::clone(&exec_command),
    ];
    let first =
        assemble_stable_prefix_for_tools(&PromptRole::Main, &one_order).expect("first assembles");
    let second = assemble_stable_prefix_for_tools(&PromptRole::Main, &other_order)
        .expect("second assembles");

    assert_eq!(
        section_content(&first, "tool_surface"),
        section_content(&second, "tool_surface")
    );
    assert_eq!(first.prefix_hash(), second.prefix_hash());
    assert!(
        section_content(&first, "tool_surface")
            .contains("`apply_patch`\n- `exec_command`\n- `read_file`")
    );
    // The digest is the other artifact built from the same list, so it has to be order-insensitive
    // for the same reason — a snapshot that moved with registration order would fail the gate on
    // startup detail rather than on a schema change.
    assert_eq!(
        tool_surface_snapshot(&one_order).expect("one order renders"),
        tool_surface_snapshot(&other_order).expect("other order renders")
    );

    for iteration in 2..=TOOL_SURFACE_RENDER_COUNT {
        let tools = if iteration % 2 == 0 {
            &one_order
        } else {
            &other_order
        };
        let rendered = assemble_stable_prefix_for_tools(&PromptRole::Main, tools)
            .expect("repeated prompt assembles");
        assert_eq!(
            section_content(&rendered, "tool_surface"),
            section_content(&first, "tool_surface"),
            "tool surface changed at render {iteration}"
        );
        assert_eq!(
            tool_surface_snapshot(tools).expect("repeated digest renders"),
            tool_surface_snapshot(&one_order).expect("one order renders"),
            "tool surface digest changed at render {iteration}"
        );
    }
}

/// A changed advertised schema must raise the product-owned revision before the baseline moves.
#[test]
fn test_tool_surface_matches_the_committed_snapshot_and_requires_a_revision_bump() {
    let rendered = rendered_tool_surface();
    let path = tool_surface_snapshot_path();

    if std::env::var_os("BLESS_PROMPT_DUMP").is_some() {
        let _writing = lock_snapshots();
        refuse_bless_without_revision_bump(&rendered);
        std::fs::write(&path, rendered).expect("tool-surface snapshot must be writable");
        return;
    }

    let baseline = std::fs::read_to_string(&path).ok().unwrap_or_else(|| {
        panic!(
            "missing tool-surface snapshot at {}. Run with BLESS_PROMPT_DUMP=1 to create it",
            path.display()
        )
    });
    assert_eq!(
        baseline, rendered,
        "the advertised tool surface changed; raise TOOL_SCHEMA_REVISION, then re-run with \
         BLESS_PROMPT_DUMP=1 so the cached-prefix diff can be reviewed"
    );
}

/// The committed tool-surface record: revision, fingerprint, and the advertised names.
fn rendered_tool_surface() -> String {
    tool_surface_snapshot(&host_backed_tools())
        .expect("the advertised tool surface must render")
        .expect("the host-backed agent advertises at least one tool")
}

/// Refuses a bless that would move the tool surface without raising its revision.
///
/// **Both bless paths call this, not just the tool-surface one.** Adding or removing an advertised
/// tool moves both files, and guarding one write alone would leave the tree half-blessed — the
/// names list rewritten, the fingerprint not — with the next run failing on the file that was left
/// behind rather than on the change that caused it. A description- or schema-only edit moves only
/// the tool-surface snapshot, because the prompt section carries names and never the digest, so
/// guarding the prompt dump costs nothing in that case.
fn refuse_bless_without_revision_bump(rendered: &str) {
    let Ok(baseline) = std::fs::read_to_string(tool_surface_snapshot_path()) else {
        return;
    };
    if baseline == rendered {
        return;
    }
    assert!(
        revision(rendered) > revision(&baseline),
        "the advertised tool surface changed without a schema revision bump; raise \
         TOOL_SCHEMA_REVISION before blessing"
    );
}

/// Takes the write lock, ignoring poisoning.
///
/// A poisoned lock means the other writer already failed its own guard and said why. Panicking
/// here on the poison instead would replace that explanation with a lock error.
fn lock_snapshots() -> std::sync::MutexGuard<'static, ()> {
    BLESS_SNAPSHOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn revision(surface: &str) -> u32 {
    let marker = "revision ";
    let start = surface
        .find(marker)
        .unwrap_or_else(|| panic!("tool-surface snapshot has no `{marker}` marker"))
        + marker.len();
    let end = surface[start..]
        .find(';')
        .unwrap_or_else(|| panic!("tool-surface snapshot has no revision terminator"));
    surface[start..start + end]
        .parse()
        .expect("tool-surface revision must be an unsigned integer")
}

/// The prefix reaches the agent, rather than the agent carrying an unassembled string.
///
/// This is the assertion that makes the snapshot mean anything. Without it the dump would describe
/// an artifact nothing sends, and the governance would apply to a prefix the model never sees.
#[test]
fn test_the_built_agent_carries_the_assembled_prefix() {
    let agent = ra_coding::build_agent(
        ra_core::agent::AgentId::new("coding-agent"),
        "Coding Agent",
        &PromptRole::Main,
    )
    .expect("the product agent must build");

    let prefix = assemble_stable_prefix(&PromptRole::Main).expect("prefix");
    assert_eq!(
        agent
            .instructions()
            .and_then(ra_core::agent::AgentInstructions::as_static),
        Some(prefix.system_instructions()),
        "the agent's instructions must be the assembled prefix, not an unstructured string"
    );
}

/// Identity is the first stable section so every following instruction inherits its collaboration
/// and role-neutral completion context.
#[test]
fn test_the_product_prefix_begins_with_the_identity_contract() {
    let prefix = assemble_stable_prefix(&PromptRole::Main).expect("prefix");
    let identity = prefix
        .sections()
        .first()
        .expect("the product prefix includes an identity section");

    assert_eq!(identity.name().as_str(), "identity");
    assert!(identity.content().contains("same workspace"));
    assert!(identity.content().contains("assigned role"));
    assert!(identity.content().contains("authority and capabilities"));
}

/// Engineering judgment follows identity so implementation decisions inherit the collaboration
/// and completion contract without being entangled with the advertised tool surface.
///
/// The tool-surface half is asserted on the host-backed prefix, because that is the only one where
/// a `tool_surface` section exists at all: checked on the bare prefix, "engineering judgment comes
/// first" would hold no matter which way the two were ranked.
#[tokio::test]
async fn test_the_product_prefix_places_engineering_judgment_after_identity() {
    let prefix = assemble_stable_prefix(&PromptRole::Main).expect("prefix");
    let engineering = prefix
        .sections()
        .get(1)
        .expect("the product prefix includes an engineering-judgment section");

    assert_eq!(
        prefix
            .sections()
            .first()
            .expect("the product prefix includes an identity section")
            .name()
            .as_str(),
        "identity"
    );
    assert_eq!(engineering.name().as_str(), "core_behavior");
    assert!(
        engineering
            .content()
            .contains("existing public APIs, helpers, and mechanisms")
    );

    // The pair, not the whole list: the full order is asserted once, where the section that most
    // recently joined it is the subject.
    let host_backed = host_backed_prefix().await;
    let names = section_names(&host_backed);
    assert!(
        position_of(&names, "core_behavior") < position_of(&names, "tool_surface"),
        "engineering judgment must still precede the advertised tool surface"
    );
}

/// Tool selection is a shared behavior contract, while the adjacent surface supplies the changing
/// list of names. This keeps a schema addition from becoming an instruction to use a capability
/// that the current request never advertises.
#[tokio::test]
async fn test_the_product_prefix_includes_the_tool_selection_contract() {
    let host_backed = host_backed_prefix().await;
    let tool_use = section_content(&host_backed, "tool_use");

    assert!(tool_use.contains("Use only the entries listed under Available tools"));
    assert!(tool_use.contains("do not infer or invoke an unlisted capability"));
    assert!(tool_use.contains("advertised specialist"));
    assert!(tool_use.contains("long-tail command-line work"));
    // Naming no tool is the property, not the absence of one particular name: a backtick in this
    // section is a tool name, and every prefix that carries the section carries it whatever the
    // host advertised.
    assert!(
        !tool_use.contains('`'),
        "the selection contract must name no tool, but reads: {tool_use}"
    );

    // The heading the rules point at is the one the inventory actually renders, and the rule that
    // restricts the model to it is stated in one of the two sections rather than both.
    let inventory = section_content(&host_backed, "tool_surface");
    assert!(inventory.starts_with("Available tools:"));
    assert!(
        !inventory.contains("Use only"),
        "one restriction, one span of the cached prefix: {inventory}"
    );

    let names = section_names(&host_backed);
    assert!(
        position_of(&names, "core_behavior") < position_of(&names, "tool_use")
            && position_of(&names, "tool_use") < position_of(&names, "tool_surface"),
        "tool selection must follow engineering judgment and precede the advertised inventory"
    );
}

/// An agent whose request carries no tools carries no rules for choosing among them.
///
/// The pair is one decision in two sections. Assembled alone, the selection rules would tell such
/// an agent to choose from a list its prompt does not contain — and they rank ahead of the role
/// section, so that would be the instruction a one-off or read-only agent reads *first*, with the
/// denial arriving several hundred tokens later. `editing` refuses to name `apply_patch` on the
/// same grounds.
#[test]
fn test_a_tool_free_prefix_carries_neither_the_inventory_nor_its_selection_rules() {
    for role in [
        PromptRole::Main,
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
        PromptRole::Coordinator,
    ] {
        let prefix = assemble_stable_prefix(&role).expect("bare prefix");
        let names = section_names(&prefix);
        assert!(
            !names.contains(&"tool_use") && !names.contains(&"tool_surface"),
            "`{role}` advertises no tool, so neither half of the pair belongs in its prefix: \
             {names:?}"
        );
    }
}

/// Every shipped role shares the engineering-judgment portion of the stable prefix. Role-specific
/// scope belongs exclusively to the role section, preserving the common cached-prefix skeleton.
#[test]
fn test_the_one_off_prefix_includes_engineering_judgment() {
    let prefix = assemble_stable_prefix(&PromptRole::OneOffAnswer).expect("prefix");

    assert_eq!(
        section_names(&prefix),
        [
            "identity",
            "core_behavior",
            "editing_verification",
            "autonomy",
            "channels",
            "final_answer",
            "personality",
            "role"
        ],
        "role-specific scope must not remove a stable-prefix section"
    );
}

/// The editing section sits after the advertised tool surface and names the dedicated entry the
/// host-backed agent installs — displacing the shell write path, without claiming to be the only
/// mechanism that may ever write a file.
#[tokio::test]
async fn test_the_product_prefix_includes_editing_and_git_safety_rules() {
    let main_prefix = host_backed_prefix().await;
    let editing = section_content(&main_prefix, "editing_verification");

    assert!(editing.contains("`apply_patch` for direct workspace edits"));
    assert!(editing.contains("dedicated tool"));
    assert!(
        editing.contains("Do not create or edit files with `cat`, heredocs, or other shell write"),
        "naming the entry only constrains anything if the shell path it displaces is named too"
    );
    assert!(
        editing.contains("bulk mechanical rewrites do not need `apply_patch`"),
        "apply_patch is the dedicated editing tool, not the only mechanism allowed to write a file"
    );
    assert!(editing.contains("pre-existing changes"));

    // The commands this product's own dangerous-action report flags, so the prefix warns about the
    // set the runtime actually stops on rather than an overlapping one.
    assert!(editing.contains("`git reset --hard`"));
    assert!(editing.contains("`git clean`"));
    assert!(editing.contains("`git checkout --`"));

    // The pair, not the whole list: the full order is asserted where channels, the section that
    // most recently joined it, is the subject.
    let names = section_names(&main_prefix);
    assert!(
        position_of(&names, "tool_surface") < position_of(&names, "editing_verification"),
        "the editing entry must still follow the surface that advertises it"
    );
}

/// The autonomy section follows editing safety and directs a run to use failure results as
/// evidence instead of cycling through the same uninformative tool failure.
///
/// It says nothing about the role's authority or the deliverable: `identity` owns that contract,
/// and a second cached copy of it is the thing this section is written to avoid.
#[tokio::test]
async fn test_the_product_prefix_includes_autonomous_progress_and_stop_loss() {
    let prefix = host_backed_prefix().await;
    let autonomy = section_content(&prefix, "autonomy");

    assert!(autonomy.contains("Do not stop at analysis or a proposal"));
    assert!(autonomy.contains("let it change the next attempt"));
    assert!(autonomy.contains("three consecutive failures that returned nothing new"));
    assert!(
        autonomy.contains("the tool is not lost and the task is not done"),
        "the breaker clears its streak when it fires, so a refusal must not read as a lost tool"
    );
    assert!(autonomy.contains("information or authority would unblock it"));

    // The prefix spells the runtime's default out in words. Pinning it here is what forces a
    // change to the threshold back through this text, rather than leaving a cached prompt
    // asserting a limit nobody enforces.
    assert_eq!(
        DEFAULT_MAX_NO_PROGRESS_STREAK.get(),
        3,
        "the autonomy section reads `three consecutive failures`; update the prompt text with it"
    );

    // The pair, not the whole list: the full order is asserted where channels, the section that
    // most recently joined it, is the subject.
    let names = section_names(&prefix);
    assert!(
        position_of(&names, "editing_verification") < position_of(&names, "autonomy"),
        "autonomy must still follow the editing safety rules it builds on"
    );
}

/// Formatting is a product-wide rendering contract, not role guidance. Keeping it in the stable
/// prefix means every role produces UI-compatible text without duplicating the rules in each role
/// section.
#[test]
fn test_every_role_shares_the_output_formatting_contract() {
    let mut expected: Option<String> = None;
    for role in [
        PromptRole::Main,
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
        PromptRole::Coordinator,
    ] {
        let formatting = section_content(
            &assemble_stable_prefix(&role).expect("product prefix assembles"),
            "final_answer",
        );
        let previous = expected.get_or_insert_with(|| formatting.clone());
        assert_eq!(
            *previous, formatting,
            "{role} must keep the shared output-formatting contract"
        );
    }
}

/// Channel choice is a product-wide UI contract. Role guidance may restrict which actions are
/// available, but it must not change when the user receives progress or a completed delivery.
#[test]
fn test_every_role_shares_the_dual_channel_contract() {
    let mut expected: Option<String> = None;
    for role in [
        PromptRole::Main,
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
        PromptRole::Coordinator,
    ] {
        let channels = section_content(
            &assemble_stable_prefix(&role).expect("product prefix assembles"),
            "channels",
        );
        let previous = expected.get_or_insert_with(|| channels.clone());
        assert_eq!(
            *previous, channels,
            "{role} must keep the shared dual-channel contract"
        );
    }
}

/// The runtime owns the mechanical phase assignment. This prompt section instead tells the agent
/// which of the two channels each thing belongs in, and paces progress against tool batches — the
/// only cadence a model without a clock can act on.
///
/// It says nothing about *whether* a blocker must be reported: `autonomy` owns that duty and
/// `personality` owns reporting it truthfully. A second cached copy of either is what this section
/// is scoped to avoid.
#[tokio::test]
async fn test_the_product_prefix_includes_dual_channel_response_rules() {
    let prefix = host_backed_prefix().await;
    let channels = section_content(&prefix, "channels");

    assert!(channels.contains("Use `commentary` for short, scannable progress updates"));
    assert!(channels.contains("before your first tool call and again before each later batch"));
    assert!(channels.contains("Use `final` only for the completed response"));
    assert!(channels.contains("Everything that response depends on belongs there"));
    assert!(channels.contains("Repeat what an earlier update already said"));
    assert!(channels.contains("Commentary is progress, not delivery"));

    // The prefix spells the runtime's own phase labels. Pinning them here is what forces a rename
    // of `OutputPhase` back through this text, rather than leaving a cached prompt directing the
    // model to a channel that no longer exists.
    assert!(channels.contains(&format!("`{}`", OutputPhase::Commentary.label())));
    assert!(channels.contains(&format!("`{}`", OutputPhase::Final.label())));

    // The whole list, asserted here because channels is the section that most recently joined it.
    // The four in the middle are not this crate's text: each installed capability contributes the
    // paragraph describing its own entries, ranked between the inventory that names them and the
    // policy sections that assume they have been described.
    assert_eq!(
        section_names(&prefix),
        [
            "identity",
            "core_behavior",
            "tool_use",
            "tool_surface",
            "filesystem",
            "search",
            "apply_patch",
            "shell",
            "editing_verification",
            "autonomy",
            "channels",
            "final_answer",
            "personality",
            "role"
        ]
    );
}

/// The UI renders GitHub-flavored Markdown, so the stable prefix must state the exact conventions
/// whose violation would visibly break a response: short bold headers, flat lists, usable file
/// links, and the product's deliberately narrow punctuation policy.
#[tokio::test]
async fn test_the_product_prefix_includes_output_formatting_rules() {
    let prefix = host_backed_prefix().await;
    let formatting = section_content(&prefix, "final_answer");

    assert!(formatting.contains("GitHub-flavored Markdown"));
    assert!(formatting.contains("`**Short Header**`"));
    assert!(formatting.contains("one to three words"));
    assert!(formatting.contains("Do not nest bullets"));
    assert!(formatting.contains("only `1.` numbering, never `1)`"));
    assert!(formatting.contains("`[app.rs](/absolute/path/app.rs:12)`"));
    assert!(formatting.contains("Do not use emoji or em dashes"));

    let names = section_names(&prefix);
    let position = |wanted: &str| names.iter().position(|name| *name == wanted);
    assert!(
        position("autonomy") < position("final_answer")
            && position("final_answer") < position("personality"),
        "output formatting must remain between autonomous progress and personality"
    );
}

/// This rule is capability-aware rather than main-agent-only. Every product role needs the same
/// completion and no-progress boundary, with the role section determining which actions it may
/// actually take.
#[test]
fn test_every_role_shares_the_autonomy_section() {
    let mut expected: Option<String> = None;
    for role in [
        PromptRole::Main,
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
        PromptRole::Coordinator,
    ] {
        let autonomy = section_content(
            &assemble_stable_prefix(&role).expect("product prefix assembles"),
            "autonomy",
        );
        let previous = expected.get_or_insert_with(|| autonomy.clone());
        assert_eq!(
            *previous, autonomy,
            "{role} must keep the shared autonomy contract"
        );
    }
}

/// Tool-specific guidance must come from the same advertised surface as the provider request.
/// A role that could edit in another product configuration still cannot call a tool the current
/// agent omitted.
#[tokio::test]
async fn test_the_editing_entry_requires_an_advertised_apply_patch_tool() {
    let bare_main = assemble_stable_prefix(&PromptRole::Main).expect("bare main prefix");
    assert!(
        !section_content(&bare_main, "editing_verification").contains("`apply_patch`"),
        "a prefix assembled without tools must not promise apply_patch"
    );

    let host_backed_main = host_backed_prefix().await;
    assert!(
        section_content(&host_backed_main, "editing_verification").contains("`apply_patch`"),
        "the host-backed agent advertises apply_patch, so its editing guidance must name it"
    );
}

/// A role whose own guidance denies it editing tools is never told to use one.
///
/// The two halves of this section have different scopes. Worktree preservation and destructive-Git
/// avoidance hold for anything that reaches a shell, so every role keeps them; the sentence naming
/// `apply_patch` would contradict the read-only role text, and it is ranked *ahead* of that text,
/// so an agent would read the instruction before the denial.
#[test]
fn test_read_only_roles_keep_git_safety_without_the_editing_entry() {
    let mut safety_only: Option<String> = None;
    for role in [
        PromptRole::ReadOnlySpecialist,
        PromptRole::Planner,
        PromptRole::OneOffAnswer,
    ] {
        let prefix = assemble_stable_prefix(&role).expect("prefix");
        let editing = section_content(&prefix, "editing_verification");

        assert!(
            !prefix.system_instructions().contains("apply_patch"),
            "{role} states it has no editing tools; its prefix must not name one"
        );
        assert!(
            editing.contains("pre-existing changes") && editing.contains("`git reset --hard`"),
            "{role} must retain the worktree and Git-safety guidance"
        );
        // Byte-identical across these roles, or the shared half is not shared and each one is
        // paying for its own cached span.
        let previous = safety_only.get_or_insert_with(|| editing.clone());
        assert_eq!(*previous, editing, "{role} rewrote the shared safety half");
    }

    let coordinator =
        assemble_stable_prefix_for_tools(&PromptRole::Coordinator, &host_backed_tools())
            .expect("prefix");
    assert!(
        section_content(&coordinator, "editing_verification").contains("`apply_patch`"),
        "a role that is not read-only keeps the editing entry"
    );
}

fn section_names(prefix: &ra_prompt::assembler::StablePrefix) -> Vec<&str> {
    prefix
        .sections()
        .iter()
        .map(|section| section.name().as_str())
        .collect()
}

/// Where a section sits in the assembled order, panicking when it is not there at all.
///
/// The obvious spelling compares two `Option<usize>`, and `None` sorts *below* every `Some` — so a
/// pairwise order assertion written that way is satisfied by the earlier section having vanished,
/// which is the failure it exists to catch.
fn position_of(names: &[&str], wanted: &str) -> usize {
    names
        .iter()
        .position(|name| *name == wanted)
        .unwrap_or_else(|| panic!("the assembled prefix must carry a `{wanted}` section"))
}

/// Lowers one set of system instructions through the production Anthropic codec and hands back the
/// `system` block that went on the wire.
///
/// The compatibility preview in `ra_model::anthropic::smoke` never applies a cache breakpoint, so
/// sending a request is the only way to observe one. The mocked response is ignored; what is under
/// test is what the adapter wrote.
async fn anthropic_system_block(instructions: &str) -> serde_json::Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_cache",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    let model = AnthropicMessagesModel::new(
        "claude-cache-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("anthropic model builds");

    let settings = ModelSettings::new().with_max_tokens(256);
    let resolved = ModelSettings::new().resolve(
        &ProviderKey::new("anthropic"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    );
    // The same two moves the runtime's turn preparation makes: name the stable prefix in a plan,
    // then hand the adapter the prefix that plan names.
    let request = ModelRequest::new(vec![ModelInputItem::Message(Message::user("go"))], resolved)
        .with_cache_plan(CachePlan::for_prefix(instructions, Some("run-7")))
        .with_system_instructions(instructions);
    model
        .get_response(request)
        .await
        .expect("the request must lower and its cache plan must match its instructions");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock retains requests");
    serde_json::from_slice::<serde_json::Value>(&requests[0].body).expect("body is JSON")["system"]
        .clone()
}

/// The product's assembled prefix now clears the common caching floor, and spends a breakpoint.
///
/// Crossing 1024 estimated tokens is not a number in a report; it switches on provider cache
/// directives for every real request. So the assertion is made where that switch is observable —
/// on the wire — rather than on a [`CachePlan`], which is built from a hash and a scope and never
/// consults a token count. The control below is what keeps this honest: without it the test would
/// pass on a prefix a tenth of this size.
///
/// A passing lowering also proves the plan names the prefix it was attached to, because the codec
/// re-hashes the instructions and refuses a plan that disagrees.
///
/// Measured on the host-backed prefix, the version actually installed on an agent with tools. A
/// prefix that only cleared the floor after tools were attached would otherwise go unnoticed.
#[tokio::test]
async fn test_the_product_prefix_reaches_the_caching_floor_and_earns_a_breakpoint() {
    let prefix = host_backed_prefix().await;
    let tokens = prefix.token_estimate();

    assert!(
        tokens >= MIN_CACHEABLE_PREFIX_TOKENS,
        "the product prefix fell back to {tokens} tokens, below the \
         {MIN_CACHEABLE_PREFIX_TOKENS}-token caching floor"
    );

    let system = anthropic_system_block(prefix.system_instructions()).await;
    assert_eq!(
        system[0]["cache_control"],
        json!({"type": "ephemeral"}),
        "the product prefix clears the floor, so the adapter must mark it as a cache breakpoint"
    );

    let below_floor = anthropic_system_block("You are a helpful assistant.").await;
    assert!(
        below_floor[0].get("cache_control").is_none(),
        "a prefix below the floor must not spend a breakpoint; without this the assertion above \
         would hold for any prefix at all"
    );
}
