//! The regression lock on the product's own prompt fragments.
//!
//! Three things are held still here, and each answers a question the committed dump report cannot.
//!
//! **The wording.** `api/prompt-dump.txt` records hashes, so it can say a section changed but never
//! what it now says. The prose is assembled from conditional builders — the editing text names an
//! entry only when that entry is advertised and the role may use it — so reading the Rust source
//! does not tell a reviewer what a given role's agent actually reads either. The snapshots below
//! are that text, one per shipped role, reviewed with `cargo insta review`.
//!
//! **The allowance.** Every section declares what share of the cached prefix it may spend, and the
//! assembler refuses one that overruns it. What is checked here is the other direction: that every
//! shipped section declares an allowance at all, and that the declarations still add up to the
//! ceiling the product chose.
//!
//! **The provenance.** Third-party prompts were read for structure, and the corpus that was read
//! from is deliberately not in this repository. The no-copy policy is that no marker of that
//! material may appear in what ships: not as a section's declared source, and not as a name in its
//! text. A prompt that says who it was taken from is the one form of copying that is trivial to
//! detect and therefore worth a gate.

use ra_coding::{
    CodingHost, HOST_BACKED_PROFILE, build_agent_with_host, host_backed_surface,
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_surface},
};
use ra_core::agent::AgentInstructions;
use ra_core::capability::CapabilityFamily;
use ra_core::prompt::{
    MIN_CACHEABLE_PREFIX_TOKENS, PromptRole, PromptSection, PromptSource, SectionPosition,
    SectionStability,
};
use ra_prompt::assembler::StablePrefix;

/// The roles the product ships, in the order the dump report offers them.
static SHIPPED_ROLES: [PromptRole; 5] = [
    PromptRole::Main,
    PromptRole::ReadOnlySpecialist,
    PromptRole::Planner,
    PromptRole::OneOffAnswer,
    PromptRole::Coordinator,
];

/// The ceiling on the assembled stable prefix, as the sum of the sections' own allowances.
///
/// The declared sum of the product's section allowances. The floor and the ceiling are different
/// kinds of fact — below the floor no provider caches the span at all, above the ceiling it is
/// cached and simply costs more on every turn than this product has decided a system prompt is
/// worth.
const PREFIX_TOKEN_CEILING: usize = 2_912;

/// The capability families whose fragments this repository writes.
///
/// Listed rather than accepting any `Capability` provenance: the tag says a capability contributed
/// the text, not that this repository wrote it, and a plugin's fragment reaching the cached prefix
/// is precisely the case the provenance check is here to notice.
static BUILT_IN_FAMILIES: [CapabilityFamily; 4] = [
    CapabilityFamily::FILESYSTEM,
    CapabilityFamily::SEARCH,
    CapabilityFamily::APPLY_PATCH,
    CapabilityFamily::SHELL,
];

/// Names whose appearance in shipped prompt text would be a marker of borrowed material.
///
/// Vendors and assistants whose system prompts were studied for structure. None of them has any
/// business in text this product wrote itself, so a match is either an attribution that should not
/// be there or a passage that came with one.
const THIRD_PARTY_SOURCE_MARKERS: [&str; 13] = [
    "codex",
    "claude",
    "anthropic",
    "openai",
    "chatgpt",
    "gpt-4",
    "gpt-5",
    "gemini",
    "copilot",
    "cursor",
    "devin",
    "windsurf",
    "llama",
];

/// Attribution wording that only appears when someone pasted a source note along with the text.
const ATTRIBUTION_MARKERS: [&str; 4] =
    ["copyright", "all rights reserved", "©", "system prompt of"];

/// The prefix an agent the product actually builds carries when it has a host.
///
/// Assembled from the same object the builder installs from, and then checked against the agent
/// that builder produced. Re-deriving it from the agent's tool list alone would leave out every
/// section an installed capability contributed — which is most of what a host-backed prefix has
/// that a bare one does not, and exactly the text this file exists to hold still.
async fn host_backed_prefix(role: &PromptRole, host: &CodingHost) -> StablePrefix {
    let assembled = host_backed_surface(role, host, HOST_BACKED_PROFILE)
        .await
        .expect("the host-backed surface must assemble");
    let prefix = assemble_stable_prefix_for_surface(
        role,
        assembled.tool_surface(),
        assembled.capability_sections(),
    )
    .expect("the host-backed product prefix must assemble");

    let agent = build_agent_with_host(
        ra_core::agent::AgentId::new("prompt-regression-agent"),
        "Prompt Regression Agent",
        role,
        host,
    )
    .await
    .expect("the host-backed product agent must build");
    assert_eq!(
        agent
            .instructions()
            .and_then(AgentInstructions::as_static)
            .expect("the agent carries a stable prefix"),
        prefix.system_instructions(),
        "the snapshot below would be of a prefix the `{role}` agent does not carry"
    );
    prefix
}

/// Every prefix the product can ship: each role's own and every host-backed tool variant.
async fn shipped_prefixes() -> Vec<(String, StablePrefix)> {
    let mut prefixes: Vec<(String, StablePrefix)> = SHIPPED_ROLES
        .iter()
        .map(|role| {
            (
                role.role_name().to_owned(),
                assemble_stable_prefix(role).expect("the product prefix must assemble"),
            )
        })
        .collect();
    let workspace = tempfile::tempdir().expect("workspace");
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    for role in [PromptRole::Main, PromptRole::Coordinator] {
        prefixes.push((
            format!("{}_host_backed", role.role_name()),
            host_backed_prefix(&role, &host).await,
        ));
    }
    // The read-only variant is the one whose capability set is narrowed, and its fragments are the
    // half a role filter can silently keep: text describing an editing entry, in the prefix of an
    // agent whose own role section says it has none.
    prefixes.push((
        format!("{}_host_backed", PromptRole::ReadOnlySpecialist.role_name()),
        host_backed_prefix(&PromptRole::ReadOnlySpecialist, &host).await,
    ));
    prefixes
}

/// The assembled text of every shipped prefix, held against a reviewed snapshot.
///
/// This is the artifact the hash in `api/prompt-dump.txt` stands for. Both are committed because
/// they answer different questions: the report says a cached span was invalidated and by which
/// section, and this says what the model now reads.
///
/// Re-run with `INSTA_UPDATE=always` (or `cargo insta review`) after an intentional edit.
#[tokio::test]
async fn test_the_shipped_prompt_text_matches_its_reviewed_snapshot() {
    for (label, prefix) in shipped_prefixes().await {
        insta::assert_snapshot!(
            format!("stable_prefix_{label}"),
            prefix.system_instructions()
        );
    }
}

/// Every section that reaches a shipped prefix declares what it may spend.
///
/// The assembler enforces a declared allowance but cannot require one — it has no way to know which
/// sections belong to this product. So the requirement that a *new* topic arrive with a stated cost
/// lives here, where the product's own section list is known.
#[tokio::test]
async fn test_every_shipped_section_declares_a_prefix_allowance() {
    for (label, prefix) in shipped_prefixes().await {
        for section in prefix.sections() {
            let budget = section.token_budget().unwrap_or_else(|| {
                panic!(
                    "section `{}` reaches the `{label}` prefix without declaring what it may \
                     spend; every turn of every run pays for it",
                    section.name()
                )
            });
            assert!(
                section.token_estimate() <= budget,
                "`{}` in the `{label}` prefix spends {} of its declared {budget}",
                section.name(),
                section.token_estimate(),
            );
        }
    }
}

/// The declared allowances still add up to the ceiling the product chose.
///
/// Stated per section rather than as one total on purpose: a single total is the number nobody
/// defends, because any one topic can grow into the room the others left and the review that would
/// have caught it sees only a prefix that still fits. This assertion is what makes raising one
/// section's allowance a visible change to the size of the whole cached span.
#[tokio::test]
async fn test_the_declared_allowances_sum_to_the_prefix_ceiling() {
    let mut allowances: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (label, prefix) in shipped_prefixes().await {
        for section in prefix.sections() {
            let budget = section
                .token_budget()
                .expect("covered by the declaration test above");
            if let Some(previous) = allowances.insert(section.name().as_str().to_owned(), budget) {
                assert_eq!(
                    previous,
                    budget,
                    "`{}` declares a different allowance in the `{label}` prefix; one section is \
                     one line in the cached span, not one per role",
                    section.name()
                );
            }
        }
    }

    let total: usize = allowances.values().sum();
    assert_eq!(
        total, PREFIX_TOKEN_CEILING,
        "the sections' declared allowances now sum to {total}; if that is the intended size of \
         the cached prefix, move the ceiling deliberately: {allowances:?}"
    );
}

/// The largest prefix the product ships sits between the caching floor and the declared ceiling.
#[tokio::test]
async fn test_every_shipped_prefix_sits_between_the_caching_floor_and_the_ceiling() {
    for (label, prefix) in shipped_prefixes().await {
        let tokens = prefix.token_estimate();
        assert!(
            tokens >= MIN_CACHEABLE_PREFIX_TOKENS,
            "the `{label}` prefix is {tokens} tokens, below the \
             {MIN_CACHEABLE_PREFIX_TOKENS}-token caching floor"
        );
        assert!(
            tokens <= PREFIX_TOKEN_CEILING,
            "the `{label}` prefix is {tokens} tokens, over the {PREFIX_TOKEN_CEILING}-token \
             ceiling its own sections declare"
        );
    }
}

/// No shipped section carries a provenance that points outside this product.
///
/// Two provenances ship, and both are things this repository wrote: the product's own sections
/// under `Agent`, and one built-in capability's fragment under its own family. A capability's text
/// is admitted by family rather than by the tag alone — `Capability` is the tag a third-party
/// plugin's text would also arrive under, and "installed here" is not the same claim as "written
/// here". Everything else is refused: `Custom` is the tag borrowed material would travel under, and
/// `Dynamic` is text that is not stable enough for a span every turn pays for.
#[tokio::test]
async fn test_no_shipped_section_declares_a_third_party_provenance() {
    let built_in: Vec<PromptSource> = BUILT_IN_FAMILIES
        .iter()
        .map(CapabilityFamily::prompt_source)
        .collect();

    for (label, prefix) in shipped_prefixes().await {
        for section in prefix.sections() {
            assert!(
                section.source() == &PromptSource::Agent || built_in.contains(section.source()),
                "`{}` in the `{label}` prefix declares provenance `{}`; the shipped prefix is the \
                 product's own text and the built-in capabilities' own fragments",
                section.name(),
                section.source()
            );
        }
    }
}

/// No shipped prompt text names the third-party material its structure was studied against.
#[tokio::test]
async fn test_no_shipped_prompt_text_carries_a_third_party_source_marker() {
    for (label, prefix) in shipped_prefixes().await {
        for section in prefix.sections() {
            if let Some(marker) = source_marker_in(section) {
                panic!(
                    "`{}` in the `{label}` prefix carries the source marker `{marker}`; the \
                     shipped prompt is original text and may not name what it was modelled on",
                    section.name()
                );
            }
        }
    }
}

/// The marker scan actually catches what it is written to catch.
///
/// Without this, a scan that silently matched nothing — a mis-cased needle, a field it forgot to
/// read — would report the same clean result as a prefix that is genuinely original.
#[test]
fn test_the_source_marker_scan_catches_a_planted_marker() {
    let planted = |purpose: &str, content: &str| {
        PromptSection::new(
            ra_core::prompt::PromptSectionName::IDENTITY,
            purpose,
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .expect("valid section")
    };

    // Each field the scan reads, and the casing a pasted line would actually arrive in.
    assert_eq!(
        source_marker_in(&planted(
            "identity",
            "You are Codex, based on GPT-5, running as a coding agent."
        )),
        Some("codex".to_owned())
    );
    assert_eq!(
        source_marker_in(&planted(
            "Adapted from the Claude Code identity section",
            "You are Rusty."
        )),
        Some("claude".to_owned())
    );
    assert_eq!(
        source_marker_in(&planted(
            "identity",
            "You are Rusty. Copyright 2026, all rights reserved."
        )),
        Some("copyright".to_owned())
    );
    assert_eq!(
        source_marker_in(&planted("identity", "You are Rusty.")),
        None
    );
}

/// The first marker from the lists above that appears in a section's name, purpose, or text.
///
/// All three fields are read because all three are recorded: a dump carries the purpose next to
/// the content, so an attribution pasted into either one ships just as surely.
fn source_marker_in(section: &PromptSection) -> Option<String> {
    let haystack = format!(
        "{}\n{}\n{}",
        section.name(),
        section.purpose(),
        section.content()
    )
    .to_lowercase();

    THIRD_PARTY_SOURCE_MARKERS
        .iter()
        .chain(ATTRIBUTION_MARKERS.iter())
        .find(|marker| haystack.contains(**marker))
        .map(|marker| (*marker).to_owned())
}
