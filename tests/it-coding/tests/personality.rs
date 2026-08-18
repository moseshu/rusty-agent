use ra_coding::prompt::personality::PersonalityPromptBuilder;

#[test]
fn test_personality_values_and_policies() {
    let section = PersonalityPromptBuilder::build_personality_section().expect("personality");
    let content = section.content();

    // Core engineering personality values
    assert!(content.contains("Clarity"));
    assert!(content.contains("Pragmatism"));
    assert!(content.contains("Rigor"));

    // Anti-fluffiness ban
    assert!(content.contains("Anti-Fluffiness Policy"));
    assert!(content.contains("cheerleading"));
    assert!(content.contains("motivational language"));
    assert!(content.contains("artificial reassurance"));

    // Code style matching
    assert!(content.contains("Code Style Matching"));

    // Truthful reporting
    assert!(content.contains("Truthful Reporting"));

    assert!(section.stability().is_stable());
    assert!(section.position().is_prefix());
}
