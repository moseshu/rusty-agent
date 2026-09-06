//! `skill`: progressive disclosure, and the two halves it is made of.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId},
    prompt::estimate_tokens,
    skill::{SkillCatalog, SkillCatalogError, SkillDocument, SkillId, SkillSummary},
    state::RunId,
    tool::{PermissionScope, Tool, ToolConcurrency, ToolContext, ToolOutput, TruncationStage},
};
use ra_tools::skill::{SkillLimits, SkillListingLimits, SkillTool, render_catalog_listing};
use serde_json::{Value, json};

/// A catalog holding what a test put in it.
struct StubCatalog {
    skills: Vec<SkillSummary>,
    document: Option<SkillDocument>,
    refusal: Option<fn(SkillId) -> SkillCatalogError>,
}

impl StubCatalog {
    fn holding(skills: Vec<SkillSummary>, document: SkillDocument) -> Self {
        Self {
            skills,
            document: Some(document),
            refusal: None,
        }
    }

    fn refusing(refusal: fn(SkillId) -> SkillCatalogError) -> Self {
        Self {
            skills: Vec::new(),
            document: None,
            refusal: Some(refusal),
        }
    }
}

#[async_trait]
impl SkillCatalog for StubCatalog {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(self.skills.clone())
    }

    async fn load(&self, skill: &SkillId) -> Result<SkillDocument> {
        if let Some(refusal) = self.refusal {
            return Err(refusal(skill.clone()).into_error("skill"));
        }
        self.document.clone().ok_or_else(|| {
            SkillCatalogError::NotFound {
                skill: skill.clone(),
            }
            .into_error("skill")
        })
    }
}

fn summary(id: &str, name: &str, description: &str) -> SkillSummary {
    SkillSummary::new(SkillId::new(id), name, description)
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-skill"), agent.as_ref())
}

/// Runs the call and, when it fails, the tool's own failure shaping.
async fn observe(tool: &SkillTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-skill");
    let run = run();
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("skill shapes every failure it produces"),
    }
}

fn body(output: &ToolOutput) -> &str {
    output.as_text().expect("a single text block")
}

fn guidance(output: &ToolOutput) -> String {
    output.metadata().guidance().join(" ")
}

/// The entry advertises what it is, and loading instructions reaches nothing outside the host.
#[tokio::test]
async fn test_the_skill_entry_advertises_a_strict_schema_and_a_read_only_scope() {
    let tool = SkillTool::new(Arc::new(StubCatalog::refusing(|skill| {
        SkillCatalogError::NotFound { skill }
    })))
    .expect("skill builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "skill");
    assert!(tool.schema().strict_json_schema());
    assert_eq!(
        tool.schema().input_schema()["required"],
        json!(["offset", "skill"])
    );
    // Read, unlike the web entries: a catalog is the host's own material, reached without anything
    // leaving the machine.
    assert_eq!(tool.options().permission_scope(), PermissionScope::Read);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
}

/// The loaded body arrives whole, under a header naming the skill and where its files are.
#[tokio::test]
async fn test_a_loaded_skill_carries_its_instructions_and_where_they_live() {
    let tool = SkillTool::new(Arc::new(StubCatalog::holding(
        vec![summary(
            "release",
            "Release checklist",
            "How this team ships.",
        )],
        SkillDocument::new(SkillId::new("release"), "Release checklist", "1. Tag it.")
            .with_location("skills/release"),
    )))
    .expect("skill builds");

    let output = observe(&tool, &json!({ "skill": "release" })).await;

    let text = body(&output);
    assert!(
        text.contains("Skill `release` — Release checklist"),
        "{text}"
    );
    assert!(text.contains("skills/release"), "{text}");
    assert!(text.contains("1. Tag it."), "{text}");
}

/// A body over the ceiling is cut here, and the cut is recorded rather than described in prose.
#[tokio::test]
async fn test_a_skill_over_the_ceiling_is_cut_and_the_cut_is_recorded() {
    let tool = SkillTool::new(Arc::new(StubCatalog::holding(
        Vec::new(),
        SkillDocument::new(SkillId::new("long"), "Long", "b".repeat(400)),
    )))
    .expect("skill builds")
    .with_limits(SkillLimits::new().with_max_body_bytes(64));

    let output = observe(&tool, &json!({ "skill": "long" })).await;

    let truncation = &output.metadata().truncations()[0];
    assert_eq!(truncation.stage(), TruncationStage::Tool);
    assert_eq!(truncation.original_bytes(), 400);
    assert_eq!(truncation.retained_bytes(), 64);
}

/// Each refusal keeps its own sentence, and only the correctable one says to fix the name.
///
/// A skill this deployment withholds is not a name to try again: no rewording reaches it, and
/// telling the model to check the identifier would spend a turn proving that.
#[tokio::test]
async fn test_a_missing_skill_and_a_withheld_one_lead_somewhere_different() {
    let missing = observe(
        &SkillTool::new(Arc::new(StubCatalog::refusing(|skill| {
            SkillCatalogError::NotFound { skill }
        })))
        .expect("skill builds"),
        &json!({ "skill": "absent" }),
    )
    .await;
    assert!(
        body(&missing).contains("No skill named"),
        "{}",
        body(&missing)
    );
    assert!(
        guidance(&missing).contains("exactly as the skill list gives it"),
        "{}",
        guidance(&missing)
    );

    let withheld = observe(
        &SkillTool::new(Arc::new(StubCatalog::refusing(|skill| {
            SkillCatalogError::NotAllowed { skill }
        })))
        .expect("skill builds"),
        &json!({ "skill": "internal" }),
    )
    .await;
    assert!(
        body(&withheld).contains("not available to this agent"),
        "{}",
        body(&withheld)
    );
    assert!(
        guidance(&withheld).contains("Continue without it"),
        "{}",
        guidance(&withheld)
    );
}

/// A catalog failing in its own vocabulary still becomes an observation.
///
/// The entry declares `Custom` failure handling, where returning nothing propagates the error and
/// ends the turn — so an unrecognized catalog failure would take down a whole run over instructions
/// the run can proceed without.
#[tokio::test]
async fn test_a_catalog_failure_this_crate_cannot_name_is_still_an_observation() {
    let tool = SkillTool::new(Arc::new(StubCatalog::refusing(|_| {
        SkillCatalogError::Unavailable {
            reason: "the skills volume is not mounted".to_owned(),
        }
    })))
    .expect("skill builds");

    let output = observe(&tool, &json!({ "skill": "release" })).await;

    assert!(
        !body(&output).contains("volume"),
        "the catalog's private detail reached the model: {}",
        body(&output)
    );
    assert!(
        guidance(&output).contains("not available"),
        "{}",
        guidance(&output)
    );
}

/// The listing names the identifier the entry takes, and says what each skill is for.
#[test]
fn test_the_listing_gives_the_identifier_the_entry_takes() {
    let listing = render_catalog_listing(
        &[
            summary("release", "Release checklist", "How this team ships."),
            summary("review", "Review checklist", "What a reviewer looks for."),
        ],
        SkillListingLimits::new(),
    );

    assert!(
        listing.contains("- `release`: How this team ships."),
        "{listing}"
    );
    assert!(
        listing.contains("- `review`: What a reviewer looks for."),
        "{listing}"
    );
}

/// A catalog larger than its declared share is cut, and the listing says how many it left out.
///
/// The alternative is worse than a short list: the listing lands in the cached prefix under a
/// declared allowance, so one that rendered everything would fail assembly — installing a skill
/// would break the agent instead of being visible in the record.
#[test]
fn test_a_catalog_past_its_share_is_cut_and_says_how_many_it_left_out() {
    let skills: Vec<SkillSummary> = (0..40)
        .map(|index| {
            summary(
                &format!("skill-{index}"),
                "A skill",
                "A description long enough to cost real tokens in the cached prefix.",
            )
        })
        .collect();
    let limits = SkillListingLimits::new().with_max_tokens(64);

    let listing = render_catalog_listing(&skills, limits);

    assert!(
        estimate_tokens(&listing) <= limits.max_tokens() + 32,
        "the listing overran its share: {} tokens",
        estimate_tokens(&listing)
    );
    assert!(listing.contains("- `skill-0`"), "{listing}");
    assert!(
        listing.contains("more are installed; call `skill`"),
        "the omitted skills went unreported: {listing}"
    );
}

/// A long description is clamped, so one verbose entry cannot crowd out the rest.
#[test]
fn test_one_verbose_entry_cannot_take_the_whole_listing() {
    let listing = render_catalog_listing(
        &[summary("verbose", "Verbose", &"word ".repeat(200))],
        SkillListingLimits::new().with_max_description_chars(24),
    );

    assert!(listing.chars().count() < 80, "{listing}");
    assert!(listing.contains('…'), "the cut went unmarked: {listing}");
}

/// An empty catalog renders nothing, leaving the capability to say so in its own words.
#[test]
fn test_an_empty_catalog_renders_no_lines() {
    assert!(render_catalog_listing(&[], SkillListingLimits::new()).is_empty());
}

/// Skills omitted from the prefix remain discoverable through successive tool pages.
#[tokio::test]
async fn omitted_skills_can_be_discovered_and_loaded() {
    let skills: Vec<_> = (0..40)
        .map(|i| summary(&format!("skill-{i}"), "Skill", "A procedure to follow."))
        .collect();
    let listing = render_catalog_listing(&skills, SkillListingLimits::new().with_max_tokens(64));
    assert!(!listing.contains("`skill-39`"));
    assert!(listing.contains("skill=null"));
    let tool = SkillTool::new(Arc::new(StubCatalog::holding(
        skills,
        SkillDocument::new(SkillId::new("skill-39"), "Last", "Last instructions."),
    )))
    .expect("skill builds");
    let mut discovered = String::new();
    for offset in [0, 16, 32] {
        let output = observe(&tool, &json!({"skill": null, "offset": offset})).await;
        discovered.push_str(body(&output));
        if offset < 32 {
            assert!(guidance(&output).contains(&format!("offset={}", offset + 16)));
        } else {
            assert!(output.metadata().guidance().is_empty());
        }
    }
    for i in 0..40 {
        assert!(discovered.contains(&format!("`skill-{i}`")));
    }
    let loaded = observe(&tool, &json!({"skill": "skill-39", "offset": null})).await;
    assert!(body(&loaded).contains("Last instructions."));
    let end = observe(&tool, &json!({"skill": null, "offset": 999})).await;
    assert_eq!(body(&end), "No more installed skills.");
}
