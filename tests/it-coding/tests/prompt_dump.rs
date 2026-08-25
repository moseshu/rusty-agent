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

use std::sync::Arc;

use ra_coding::{
    CodingHost,
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_tools, tool_surface_snapshot},
};
use ra_core::prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS, PromptRole};
use ra_core::tool::Tool;
use ra_prompt::dump::PromptDump;
use ra_tools::{exec_command::ExecCommandTool, read_file::ReadFileTool};

const TOOL_SURFACE_RENDER_COUNT: usize = 100;

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
    vec![host.apply_patch_tool().expect("apply_patch builds")]
}

fn host_backed_prefix() -> ra_prompt::assembler::StablePrefix {
    assemble_stable_prefix_for_tools(&PromptRole::Main, &host_backed_tools())
        .expect("host-backed product prefix must assemble")
}

fn tool_surface_content(prefix: &ra_prompt::assembler::StablePrefix) -> String {
    prefix
        .sections()
        .iter()
        .find(|section| section.name().as_str() == "tool_surface")
        .expect("host-backed prompt must record its advertised tool surface")
        .content()
        .to_owned()
}

/// The assembled prefix matches the committed snapshot, section for section and hash for hash.
#[test]
fn test_stable_prefix_matches_the_committed_snapshot() {
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
        let prefix = assemble_stable_prefix(&role).expect("the product prefix must assemble");
        // The real plan, not `None`: whether this prefix is cacheable at all is the costliest fact
        // about it, and the snapshot is where a change to it should become visible.
        let plan = CachePlan::for_prefix(prefix.system_instructions(), Some("<run>"));
        rendered.push_str(&format!("### role: {role}\n"));
        rendered
            .push_str(&PromptDump::from_assembled(&prefix, Some(plan), None, None).render_text());
        rendered.push('\n');
    }

    let prefix = host_backed_prefix();
    let plan = CachePlan::for_prefix(prefix.system_instructions(), Some("<run>"));
    rendered.push_str("### host_backed_role: main\n");
    rendered.push_str(&PromptDump::from_assembled(&prefix, Some(plan), None, None).render_text());
    rendered.push('\n');

    let path = snapshot_path();
    if std::env::var_os("BLESS_PROMPT_DUMP").is_some() {
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

/// The host-backed prompt records the exact tool that the same builder installs on the agent.
#[test]
fn test_host_backed_agent_carries_the_tool_surface_prefix() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    let agent = ra_coding::build_agent_with_host(
        ra_core::agent::AgentId::new("coding-agent"),
        "Coding Agent",
        &PromptRole::Main,
        &host,
    )
    .expect("host-backed agent builds");
    let prefix = host_backed_prefix();

    assert_eq!(
        agent
            .instructions()
            .and_then(ra_core::agent::AgentInstructions::as_static),
        Some(prefix.system_instructions())
    );
    assert_eq!(agent.tools().len(), 1);
    assert!(prefix.system_instructions().contains("`apply_patch`"));
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

    assert_eq!(tool_surface_content(&first), tool_surface_content(&second));
    assert_eq!(first.prefix_hash(), second.prefix_hash());
    assert!(
        tool_surface_content(&first).contains("`apply_patch`\n- `exec_command`\n- `read_file`")
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
            tool_surface_content(&rendered),
            tool_surface_content(&first),
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
/// **Both snapshots have to be guarded, not just the tool-surface one.** The two files are written
/// by separate tests that the harness runs in parallel, and the prompt dump embeds the surface
/// fingerprint. A guard on one write alone would still leave the other file rewritten with a
/// fingerprint no reviewer approved, and the next run would then fail on a snapshot other than the
/// one that actually changed.
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

/// The product's assembled prefix is currently too short for any provider to cache.
///
/// This pins a fact that is otherwise invisible. The floor is 1024 estimated tokens — every
/// provider ignores a shorter prefix — and today's prefix carries only the tone and role sections,
/// landing in the low hundreds. So the real product gets no cache plan, and no `prompt_cache_key`
/// is sent even to an endpoint that declared support for one.
///
/// **This is missing content, not a broken threshold.** The remaining sections are unwritten and
/// the tool table, which shares the same cached prefix and is usually the larger half, is not
/// attached to this agent yet. The assertion is deliberately written to fail once either lands: at
/// that point caching starts applying to the product, and that is a change worth noticing rather
/// than discovering on a bill.
#[test]
fn test_the_product_prefix_is_still_below_the_caching_floor() {
    let prefix = assemble_stable_prefix(&PromptRole::Main).expect("prefix");
    let tokens = prefix.token_estimate();

    assert!(
        tokens < MIN_CACHEABLE_PREFIX_TOKENS,
        "the product prefix now reaches {tokens} tokens, at or past the {MIN_CACHEABLE_PREFIX_TOKENS}-token \
         floor. Caching now applies to the product: re-check the cache plan path end to end and \
         update this test"
    );
}
