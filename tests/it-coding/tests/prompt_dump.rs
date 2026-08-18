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

use ra_coding::prompt::assemble_stable_prefix;
use ra_core::prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS, PromptRole};
use ra_prompt::dump::PromptDump;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .join("prompt-dump.txt")
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

    let path = snapshot_path();
    if std::env::var_os("BLESS_PROMPT_DUMP").is_some() {
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
