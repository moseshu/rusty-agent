use ra_core::prompt::{PromptRole, PromptSectionName, PromptSource};
use ra_prompt::assembler::PromptAssembler;
use ra_prompt::section::PromptSectionBuilder;
use ra_prompt::stability::{InvalidationReason, PrefixStabilityTracker, assert_prefix_stable};

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

fn build_standard_assembler() -> PromptAssembler {
    let identity = PromptSectionBuilder::new(PromptSectionName::IDENTITY)
        .purpose("Agent identity")
        .source(PromptSource::Builtin)
        .content("Identity: collaborate in the shared workspace.")
        .build()
        .expect("valid identity section");

    let core = PromptSectionBuilder::new(PromptSectionName::CORE_BEHAVIOR)
        .purpose("Core behavior")
        .source(PromptSource::Builtin)
        .content("Core instructions: solve problems methodically and accurately.")
        .build()
        .expect("valid core section");

    let tools = PromptSectionBuilder::new(PromptSectionName::TOOL_USE)
        .purpose("Tool usage guidance")
        .source(PromptSource::Builtin)
        .content("Tool use rules: call tools deliberately and inspect outputs.")
        .build()
        .expect("valid tools section");

    let safety = PromptSectionBuilder::new(PromptSectionName::SAFETY)
        .purpose("Safety boundaries")
        .source(PromptSource::Builtin)
        .content("Safety constraints: operate within designated workspace boundaries.")
        .build()
        .expect("valid safety section");

    let editing = PromptSectionBuilder::new(PromptSectionName::EDITING_VERIFICATION)
        .purpose("Editing verification")
        .source(PromptSource::Builtin)
        .content("Verification: verify all edits before concluding.")
        .build()
        .expect("valid editing section");

    let final_ans = PromptSectionBuilder::new(PromptSectionName::FINAL_ANSWER)
        .purpose("Final answer delivery")
        .source(PromptSource::Builtin)
        .content("Final deliverable: present structured and clear summary.")
        .build()
        .expect("valid final answer section");

    let durability = PromptSectionBuilder::new(PromptSectionName::CONTEXT_DURABILITY)
        .purpose("Context durability")
        .source(PromptSource::Builtin)
        .content("Context durability: keep critical state concise and enduring.")
        .build()
        .expect("valid durability section");

    let personality = fixture_section(PromptSectionName::PERSONALITY, "Fixture tone guidance.");
    let role = fixture_section(PromptSectionName::ROLE, "Fixture role guidance.");

    PromptAssembler::new()
        .add_section(safety)
        .expect("add safety")
        .add_section(durability)
        .expect("add durability")
        .add_section(identity)
        .expect("add identity")
        .add_section(core)
        .expect("add core")
        .add_section(personality)
        .expect("add personality")
        .add_section(tools)
        .expect("add tools")
        .add_section(final_ans)
        .expect("add final_ans")
        .add_section(role)
        .expect("add role")
        .add_section(editing)
        .expect("add editing")
}

#[test]
fn test_stable_prefix_assembly_order_determinism() {
    let assembler = build_standard_assembler();
    let prefix = assembler.assemble().expect("assembly must succeed");

    let section_names: Vec<String> = prefix
        .sections()
        .iter()
        .map(|s| s.name().as_str().to_string())
        .collect();

    assert_eq!(
        section_names,
        vec![
            "identity",
            "core_behavior",
            "tool_use",
            "safety",
            "editing_verification",
            "final_answer",
            "context_durability",
            "personality",
            "role"
        ],
        "sections must be ordered according to canonical priority"
    );
}

#[test]
fn test_100_runs_prefix_byte_invariance() {
    let assembler = build_standard_assembler();
    let baseline = assembler.assemble().expect("first assembly");

    for i in 0..100 {
        let current = assembler.assemble().expect("subsequent assembly");
        assert_eq!(
            current.prefix_hash(),
            baseline.prefix_hash(),
            "prefix hash must be byte-identical on run {i}"
        );
        assert_eq!(
            current.system_instructions(),
            baseline.system_instructions(),
            "prefix text must be byte-identical on run {i}"
        );
    }
}

#[test]
fn test_prefix_stability_tracker_invariance_and_invalidation() {
    let assembler = build_standard_assembler();
    let prefix = assembler.assemble().expect("assembly");

    let mut tracker = PrefixStabilityTracker::new();

    // First turn records baseline
    tracker
        .record_turn(prefix.prefix_hash(), None)
        .expect("record turn 1");

    // Subsequent turns with same hash succeed
    tracker
        .record_turn(prefix.prefix_hash(), None)
        .expect("record turn 2");
    tracker
        .record_turn(prefix.prefix_hash(), None)
        .expect("record turn 3");

    assert_eq!(tracker.invalidation_count(), 0);

    // A different role contributes different text, so the prefix it assembles into is a different
    // cached span. The two fixtures must differ for this test to be testing anything.
    let ro_role = fixture_section(PromptSectionName::ROLE, "Fixture read-only role guidance.");
    let modified_assembler = assembler.add_section(ro_role).expect("swap role");
    let new_prefix = modified_assembler.assemble().expect("assemble new prefix");

    assert_ne!(
        new_prefix.prefix_hash(),
        prefix.prefix_hash(),
        "changing role must change prefix hash"
    );

    // Unexpected change without invalidation reason fails
    let fail_result = tracker.record_turn(new_prefix.prefix_hash(), None);
    assert!(
        fail_result.is_err(),
        "unexpected hash change must return an error"
    );

    // Intentional change with invalidation reason succeeds
    tracker
        .record_turn(
            new_prefix.prefix_hash(),
            Some(InvalidationReason::RoleChanged(
                PromptRole::ReadOnlySpecialist,
            )),
        )
        .expect("intentional invalidation must succeed");

    assert_eq!(tracker.invalidation_count(), 1);
    assert_eq!(tracker.last_prefix_hash(), Some(new_prefix.prefix_hash()));
}

#[test]
fn test_assert_prefix_stable_helper() {
    let assembler = build_standard_assembler();
    let prefix = assembler.assemble().expect("prefix");

    let ro_role = fixture_section(PromptSectionName::ROLE, "Fixture read-only role guidance.");
    let modified = assembler.add_section(ro_role).expect("swap role");
    let new_prefix = modified.assemble().expect("new prefix");

    assert!(assert_prefix_stable(prefix.prefix_hash(), prefix.prefix_hash(), None).is_ok());
    assert!(assert_prefix_stable(prefix.prefix_hash(), new_prefix.prefix_hash(), None).is_err());
    assert!(
        assert_prefix_stable(
            prefix.prefix_hash(),
            new_prefix.prefix_hash(),
            Some(&InvalidationReason::AgentSpecChanged),
        )
        .is_ok()
    );
}

/// A tail section is rejected at insertion rather than filtered out at assembly.
///
/// Accepting it and returning `Ok` would tell the caller their text is in the prefix right up
/// until they notice it is not in the output.
#[test]
fn test_assembler_rejects_sections_it_would_not_assemble() {
    let tail = PromptSectionBuilder::new(PromptSectionName::new("tail_note"))
        .purpose("Tail content offered to the prefix assembler")
        .source(PromptSource::Dynamic("runtime".into()))
        .volatile()
        .tail_message()
        .content("Per-turn delta")
        .build()
        .expect("a volatile tail section is valid on its own");

    let Err(err) = PromptAssembler::new().add_section(tail) else {
        panic!("the prefix assembler must refuse tail content");
    };
    assert!(
        err.to_string().contains("tail_note"),
        "the error must name the rejected section: {err}"
    );
}

/// The assembled total is the sum of the section rows, so a dump reconciles with its own breakdown.
#[test]
fn test_prefix_token_total_equals_the_sum_of_its_sections() {
    let prefix = build_standard_assembler()
        .assemble()
        .expect("assembly must succeed");

    let sum: usize = prefix.sections().iter().map(|s| s.token_estimate()).sum();
    assert_eq!(prefix.token_estimate(), sum);
}

/// An exact count from a real tokenizer reaches the total instead of being re-estimated away.
#[test]
fn test_external_token_estimates_survive_assembly() {
    let core = PromptSectionBuilder::new(PromptSectionName::CORE_BEHAVIOR)
        .purpose("Core behavior")
        .source(PromptSource::Builtin)
        .content("Core instructions.")
        .token_estimate(1_234)
        .build()
        .expect("valid core section");

    let prefix = PromptAssembler::new()
        .add_section(core)
        .expect("add core")
        .assemble()
        .expect("assemble");

    assert_eq!(prefix.token_estimate(), 1_234);
}
