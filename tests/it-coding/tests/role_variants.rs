use ra_coding::prompt::role::RolePromptBuilder;
use ra_core::prompt::PromptRole;

#[test]
fn test_read_only_specialist_role_constraints() {
    let section = RolePromptBuilder::build_role_section(&PromptRole::ReadOnlySpecialist)
        .expect("build read only section");

    let content = section.content();

    assert!(content.contains("non-mutating exploration"));
    assert!(content.contains("The dedicated file editing tool is not advertised"));
    assert!(content.contains(
        "No collaboration tools are advertised; do not delegate tasks or direct other agents"
    ));
    assert!(content.contains("parent session's sandbox and permissions"));

    assert!(PromptRole::ReadOnlySpecialist.is_read_only());
    assert!(!PromptRole::ReadOnlySpecialist.is_one_off());
}

#[test]
fn test_one_off_answer_role_constraints() {
    let section = RolePromptBuilder::build_role_section(&PromptRole::OneOffAnswer)
        .expect("build one off section");

    let content = section.content();

    // Three hard constraints for one-off answer role:
    // 1. Do not refer to previous in-progress work / interruption
    assert!(
        content.contains(
            "Do not refer to previous in-progress work, interruptions, or context switches"
        )
    );
    // 2. Ban action promises
    assert!(content.contains("Do not promise future actions"));
    assert!(content.contains("Let me check..."));
    // 3. Admit unknown without offering to search
    assert!(content.contains("If the answer is unknown"));

    assert!(!PromptRole::OneOffAnswer.is_read_only());
    assert!(PromptRole::OneOffAnswer.is_one_off());
}

#[test]
fn test_planner_and_coordinator_roles() {
    let planner_sec = RolePromptBuilder::build_role_section(&PromptRole::Planner).expect("planner");
    assert!(planner_sec.content().contains("planning agent"));
    assert!(PromptRole::Planner.is_read_only());

    let coordinator_sec =
        RolePromptBuilder::build_role_section(&PromptRole::Coordinator).expect("coordinator");
    assert!(coordinator_sec.content().contains("coordinator agent"));
}

#[test]
fn test_custom_role() {
    let custom_role = PromptRole::Custom("SecurityAuditor".into());
    let custom_sec = RolePromptBuilder::build_role_section_with_custom_text(
        &custom_role,
        Some("Custom security auditor prompt"),
    )
    .expect("custom");

    assert_eq!(custom_sec.content(), "Custom security auditor prompt");
}

/// An override reaches the section for a builtin role too, rather than losing to the stock text.
///
/// The builtin used to win, so a caller tailoring `Main` got the stock guidance and no sign their
/// text had gone nowhere — the silent fallback the dynamic-prompt contract rejects, one layer down.
#[test]
fn test_custom_text_overrides_a_builtin_role_rather_than_being_dropped() {
    let stock = RolePromptBuilder::build_role_section(&PromptRole::Main).expect("stock main");
    let overridden = RolePromptBuilder::build_role_section_with_custom_text(
        &PromptRole::Main,
        Some("You are the deployment agent for this repository."),
    )
    .expect("overridden main");

    assert_eq!(
        overridden.content(),
        "You are the deployment agent for this repository."
    );
    assert_ne!(
        overridden.content(),
        stock.content(),
        "the override must not fall back to the builtin text"
    );

    // Omitting the override still yields the builtin.
    let unchanged = RolePromptBuilder::build_role_section_with_custom_text(&PromptRole::Main, None)
        .expect("no override");
    assert_eq!(unchanged.content(), stock.content());
}

#[test]
fn role_guidance_tracks_the_advertised_editing_and_collaboration_tools() {
    for role in [PromptRole::ReadOnlySpecialist, PromptRole::Planner] {
        let inherited = RolePromptBuilder::build_role_section_for_tools(
            &role,
            &[
                "apply_patch",
                "exec_command",
                "write_stdin",
                "collaboration.spawn_agent",
                "update_plan",
                "request_user_input",
            ],
        )
        .unwrap();
        assert!(
            inherited
                .content()
                .contains("The dedicated file editing tool is available")
        );
        assert!(
            !inherited
                .content()
                .contains("The dedicated file editing tool is not advertised")
        );
        assert!(!inherited.content().contains(
            "No collaboration tools are advertised; do not delegate tasks or direct other agents"
        ));
        let restricted = RolePromptBuilder::build_role_section_for_tools(
            &role,
            &["exec_command", "write_stdin"],
        )
        .unwrap();
        assert!(
            restricted
                .content()
                .contains("The dedicated file editing tool is not advertised")
        );
        assert!(restricted.content().contains(
            "No collaboration tools are advertised; do not delegate tasks or direct other agents"
        ));
        for content in [inherited.content(), restricted.content()] {
            assert!(!content.contains("client can render it specially"));
            assert!(content.contains("Dry-run style commands"));
            assert!(content.contains("Tests, builds, or checks that may write to caches"));
            assert!(content.contains("Running formatters or linters that rewrite files"));
            assert!(content.contains("Applying patches, migrations, or codegen"));
        }
        if matches!(role, PromptRole::Planner) {
            assert!(
                inherited
                    .content()
                    .contains("## Plan Mode vs update_plan tool")
            );
            assert!(!restricted.content().contains("update_plan"));
            assert!(!restricted.content().contains("request_user_input"));
            assert!(
                inherited
                    .content()
                    .contains("Tests, builds, or checks that may write to caches")
            );
            assert!(inherited.content().contains("<proposed_plan>"));
        }
    }
}
