use ra_core::prompt::{CachePlan, PromptSectionName, PromptSource};
use ra_prompt::assembler::PromptAssembler;
use ra_prompt::dump::PromptDump;
use ra_prompt::section::PromptSectionBuilder;

/// A stand-in for a product's own section.
///
/// The assembler is mechanism, so its tests build their own fixtures rather than importing a
/// product's text: borrowing real prompt content would make an edit to that product's wording fail
/// tests about ordering and hashing, which is not what those tests are about.
fn fixture_section(name: PromptSectionName, text: &str) -> ra_core::prompt::PromptSection {
    PromptSectionBuilder::new(name)
        .purpose("fixture")
        .source(PromptSource::Builtin)
        .content(text)
        .build()
        .expect("valid fixture section")
}

/// A prefix long enough that a provider will actually cache it.
///
/// A short fixture would make the dump report a `None` cache plan and quietly stop covering the
/// cache-plan rendering, which is part of what a prompt change is reviewed against.
fn cacheable_core_text() -> String {
    "Core instructions content here. ".repeat(200)
}

#[test]
fn test_prompt_dump_rendering_and_json() {
    let core = PromptSectionBuilder::new(PromptSectionName::CORE_BEHAVIOR)
        .purpose("Core behavioral constitution")
        .source(PromptSource::Builtin)
        .content(cacheable_core_text())
        .build()
        .expect("core");

    let personality = fixture_section(PromptSectionName::PERSONALITY, "Fixture tone guidance.");
    let role = fixture_section(PromptSectionName::ROLE, "Fixture role guidance.");

    let assembler = PromptAssembler::new()
        .add_section(core)
        .expect("add core")
        .add_section(personality)
        .expect("add personality")
        .add_section(role)
        .expect("add role");

    let prefix = assembler.assemble().expect("assemble");
    // A real prefix is long enough to be cacheable; the dump has to show the plan that a turn
    // would actually carry, not one hand-built past the length floor.
    let cache_plan = CachePlan::for_prefix(prefix.system_instructions(), Some("run-7"));
    assert_eq!(cache_plan.cache_scope(), Some("run-7"));

    let dump = PromptDump::from_assembled(
        &prefix,
        Some(cache_plan),
        Some("anthropic"),
        Some("claude-3-5-sonnet"),
    );

    assert_eq!(dump.provider(), Some("anthropic"));
    assert_eq!(dump.model(), Some("claude-3-5-sonnet"));
    assert_eq!(dump.sections().len(), 3);
    assert_eq!(dump.prefix_hash(), prefix.prefix_hash().to_string());

    let text_report = dump.render_text();
    assert!(text_report.contains("PROMPT DUMP REPORT"));
    assert!(text_report.contains("Provider: anthropic"));
    assert!(text_report.contains("Model:    claude-3-5-sonnet"));
    assert!(text_report.contains("core_behavior"));
    assert!(text_report.contains("personality"));
    assert!(text_report.contains("role"));
    assert!(text_report.contains("Cache Scope:          run-7"));
    // The dump reports the plan's intent, not a lowering. Breakpoints and cache-key fields are the
    // adapter's decision, and printing them here would report a choice this layer does not make.
    assert!(!text_report.contains("Ephemeral"));
    assert!(!text_report.contains("prompt_cache_key"));

    // The header total has to reconcile with the rows below it, or the report cannot be used to
    // review a change.
    let rows_total: usize = dump.sections().iter().map(|s| s.token_estimate()).sum();
    assert!(text_report.contains(&format!("Total Prefix Tokens:  ~{rows_total}")));

    let json_str = dump.to_json().expect("to json");
    assert!(json_str.contains("\"provider\": \"anthropic\""));
    assert!(json_str.contains("\"prefix_hash\":"));
}

/// The report carries each section's provenance and the allowance it declared.
///
/// Neither is recoverable from the text of the prompt, and both are what a reader of a committed
/// dump is checking: that this wording is the product's own, and that the tokens it spends are the
/// tokens it said it would.
#[test]
fn test_the_report_shows_provenance_and_the_declared_allowance() {
    let budgeted = PromptSectionBuilder::new(PromptSectionName::CORE_BEHAVIOR)
        .purpose("Core behavioral constitution")
        .source(PromptSource::Capability("planning".into()))
        .content(cacheable_core_text())
        .token_budget(4_096)
        .build()
        .expect("core");
    let unbudgeted = fixture_section(PromptSectionName::ROLE, "Fixture role guidance.");

    let prefix = PromptAssembler::new()
        .add_section(budgeted)
        .expect("add core")
        .add_section(unbudgeted)
        .expect("add role")
        .assemble()
        .expect("assemble");
    let report = PromptDump::from_assembled(&prefix, None, None, None).render_text();

    let row = |name: &str| {
        report
            .lines()
            .find(|line| line.starts_with(name))
            .unwrap_or_else(|| panic!("the report must carry a `{name}` row:\n{report}"))
            .to_owned()
    };

    assert!(report.contains("Source"), "the header must name the column");
    assert!(report.contains("Budget"), "the header must name the column");
    assert!(row("core_behavior").contains("capability(planning)"));
    assert!(row("core_behavior").contains("4096"));
    // A section that declared nothing must not read like one that declared a large allowance.
    assert!(row("role").contains("builtin"));
    assert!(
        row("role").contains(" - "),
        "an undeclared allowance prints as one: {}",
        row("role")
    );
}
