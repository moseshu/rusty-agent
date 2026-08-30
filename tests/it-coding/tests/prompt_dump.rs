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
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_tools, tool_surface_snapshot},
};
use ra_core::prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS, PromptRole};
use ra_core::tool::{DEFAULT_MAX_NO_PROGRESS_STREAK, Tool};
use ra_prompt::dump::PromptDump;
use ra_tools::{exec_command::ExecCommandTool, read_file::ReadFileTool};

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
    vec![host.apply_patch_tool().expect("apply_patch builds")]
}

fn host_backed_prefix() -> ra_prompt::assembler::StablePrefix {
    assemble_stable_prefix_for_tools(&PromptRole::Main, &host_backed_tools())
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
#[test]
fn test_the_product_prefix_places_engineering_judgment_after_identity() {
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
    let host_backed = host_backed_prefix();
    let names = section_names(&host_backed);
    assert!(
        names.iter().position(|name| *name == "core_behavior")
            < names.iter().position(|name| *name == "tool_surface"),
        "engineering judgment must still precede the advertised tool surface"
    );
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
#[test]
fn test_the_product_prefix_includes_editing_and_git_safety_rules() {
    let main_prefix = host_backed_prefix();
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

    // The pair, not the whole list: the full order is asserted where autonomy, the section that
    // most recently joined it, is the subject.
    let names = section_names(&main_prefix);
    let position = |wanted: &str| names.iter().position(|name| *name == wanted);
    assert!(
        position("tool_surface") < position("editing_verification"),
        "the editing entry must still follow the surface that advertises it"
    );
}

/// The autonomy section follows editing safety and directs a run to use failure results as
/// evidence instead of cycling through the same uninformative tool failure.
///
/// It says nothing about the role's authority or the deliverable: `identity` owns that contract,
/// and a second cached copy of it is the thing this section is written to avoid.
#[test]
fn test_the_product_prefix_includes_autonomous_progress_and_stop_loss() {
    let prefix = host_backed_prefix();
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

    assert_eq!(
        section_names(&prefix),
        [
            "identity",
            "core_behavior",
            "tool_surface",
            "editing_verification",
            "autonomy",
            "final_answer",
            "personality",
            "role"
        ]
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

/// The UI renders GitHub-flavored Markdown, so the stable prefix must state the exact conventions
/// whose violation would visibly break a response: short bold headers, flat lists, usable file
/// links, and the product's deliberately narrow punctuation policy.
#[test]
fn test_the_product_prefix_includes_output_formatting_rules() {
    let prefix = host_backed_prefix();
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
#[test]
fn test_the_editing_entry_requires_an_advertised_apply_patch_tool() {
    let bare_main = assemble_stable_prefix(&PromptRole::Main).expect("bare main prefix");
    assert!(
        !section_content(&bare_main, "editing_verification").contains("`apply_patch`"),
        "a prefix assembled without tools must not promise apply_patch"
    );

    let host_backed_main = host_backed_prefix();
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

/// The product's assembled prefix is currently too short for any provider to cache.
///
/// This pins a fact that is otherwise invisible. The floor is 1024 estimated tokens — every
/// provider ignores a shorter prefix — and today's prefix carries the identity, engineering,
/// editing, autonomy, final-answer formatting, tone, role and advertised-tool sections, landing
/// short of it. So the real product gets no cache plan, and no `prompt_cache_key` is sent even to
/// an endpoint that declared support for one.
///
/// Measured on the host-backed prefix, which is the longer of the two the product assembles and
/// the one an agent with tools installed actually carries. A prefix that only cleared the floor
/// once tools were attached would otherwise reach the floor without this test noticing.
///
/// **This is missing content, not a broken threshold.** Most sections are still unwritten, and the
/// provider's tool table — a different part of the request, which the same cache span covers — is
/// one tool wide. The assertion is deliberately written to fail once that changes: at that point
/// caching starts applying to the product, and that is a change worth noticing rather than
/// discovering on a bill.
#[test]
fn test_the_product_prefix_is_still_below_the_caching_floor() {
    let tokens = host_backed_prefix().token_estimate();

    assert!(
        tokens < MIN_CACHEABLE_PREFIX_TOKENS,
        "the product prefix now reaches {tokens} tokens, at or past the {MIN_CACHEABLE_PREFIX_TOKENS}-token \
         floor. Caching now applies to the product: re-check the cache plan path end to end and \
         update this test"
    );
}
