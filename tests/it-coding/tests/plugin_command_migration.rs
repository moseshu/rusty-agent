// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Source command fixtures exercise the public migration and Store installation paths.

use ra_coding::plugins::{
    AbsolutePathBuf, PluginId,
    command_migration::{
        CommandDescriptionMode, CommandMigrationProfile, RewriteProfile,
        count_missing_commands_with_profile, import_commands_with_profile,
        missing_command_names_with_profile,
    },
    discovery::migrated_command_skills_root,
    store::PluginStore,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn profile(mode: CommandDescriptionMode) -> CommandMigrationProfile {
    CommandMigrationProfile::new(
        RewriteProfile::new(
            "CLAUDE.md",
            &[
                "claude code",
                "claude-code",
                "claude_code",
                "claudecode",
                "claude",
            ],
        ),
        mode,
    )
}
fn import(root: &Path, mode: CommandDescriptionMode) -> Vec<String> {
    import_commands_with_profile(&root.join("commands"), &root.join("skills"), profile(mode))
        .unwrap()
}
fn install(root: &Path, manifest: &str) -> (PathBuf, PathBuf) {
    let source = root.join("source");
    write(&source, ".codex-plugin/plugin.json", manifest);
    let result = PluginStore::new(root.join("home"))
        .install(
            AbsolutePathBuf::try_from(source.clone()).unwrap(),
            PluginId::parse("demo@debug").unwrap(),
        )
        .unwrap();
    let migrated = migrated_command_skills_root(&result.installed_path).into_path_buf();
    (source, migrated)
}

#[test]
fn command_skill_names_must_fit_codex_skill_loader_limit() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "commands/this/is/a/deeply/nested/command/with/a/very/long/name.md",
        "---\ndescription: Review PR\n---\nReview\n",
    );
    assert!(import(root.path(), CommandDescriptionMode::RequireFrontmatter).is_empty());
    assert!(
        missing_command_names_with_profile(
            &root.path().join("commands"),
            &root.path().join("skills"),
            profile(CommandDescriptionMode::RequireFrontmatter)
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn commands_with_overlong_descriptions_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let description = "x".repeat(1025);
    write(
        root.path(),
        "commands/review.md",
        &format!("---\ndescription: {description}\n---\nReview\n"),
    );
    assert_eq!(
        import(root.path(), CommandDescriptionMode::RequireFrontmatter),
        ["source-command-review"]
    );
    let rendered =
        fs::read_to_string(root.path().join("skills/source-command-review/SKILL.md")).unwrap();
    assert!(rendered.contains(&format!("description: \"{description}\"")));
}

#[test]
fn commands_with_provider_runtime_expansion_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    for (index, body) in [
        "Deploy $ARGUMENTS from @release.yaml",
        "Use $1",
        "Use $9suffix",
        "{{ context }}",
        "!`echo hi`",
        "! `echo hi`",
    ]
    .into_iter()
    .enumerate()
    {
        write(
            root.path(),
            &format!("commands/unsupported-{index}.md"),
            &format!("---\ndescription: Deploy\n---\n{body}\n"),
        );
    }
    write(
        root.path(),
        "commands/supported.md",
        "---\ndescription: Review\n---\nemail@example.org $name @\n",
    );
    assert_eq!(
        import(root.path(), CommandDescriptionMode::RequireFrontmatter),
        ["source-command-supported"]
    );
}

#[test]
fn commands_without_description_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "commands/review.md",
        "Review the current change.\n",
    );
    assert!(import(root.path(), CommandDescriptionMode::RequireFrontmatter).is_empty());
}

#[test]
fn commands_can_derive_descriptions_from_source_names() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "commands/review-code.md",
        "Review the current change.\n",
    );
    assert_eq!(
        count_missing_commands_with_profile(
            &root.path().join("commands"),
            &root.path().join("skills"),
            profile(CommandDescriptionMode::UseSourceNameFallback)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        import(root.path(), CommandDescriptionMode::UseSourceNameFallback),
        ["source-command-review-code"]
    );
    let rendered = fs::read_to_string(
        root.path()
            .join("skills/source-command-review-code/SKILL.md"),
    )
    .unwrap();
    assert!(rendered.contains("description: \"Migrated source command `review-code`\""));
    assert!(rendered.contains("Review the current change."));
    assert!(import(root.path(), CommandDescriptionMode::UseSourceNameFallback).is_empty());
    assert_eq!(
        count_missing_commands_with_profile(
            &root.path().join("commands"),
            &root.path().join("skills"),
            profile(CommandDescriptionMode::UseSourceNameFallback)
        )
        .unwrap(),
        0
    );
}

#[test]
fn command_slug_collisions_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    for name in ["foo-bar", "foo_bar", "README"] {
        write(
            root.path(),
            &format!("commands/{name}.md"),
            "---\ndescription: Review\n---\nReview\n",
        );
    }
    assert!(import(root.path(), CommandDescriptionMode::RequireFrontmatter).is_empty());
}

#[test]
fn command_profiles_preserve_rewrite_boundaries_scalar_descriptions_and_crlf() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "commands/review.md",
        "---\r\ndescription: true\r\n---\r\nRead CLAUDE.md with Claude Code and claudecode. Keep preclaude and claude_suffix.\r\n",
    );
    assert_eq!(
        import(root.path(), CommandDescriptionMode::RequireFrontmatter),
        ["source-command-review"]
    );
    let output = root.path().join("skills/source-command-review/SKILL.md");
    let rendered = fs::read_to_string(&output).unwrap();
    assert!(rendered.contains("description: \"true\""));
    assert!(
        rendered.contains("Read AGENTS.md with Codex and Codex. Keep preclaude and claude_suffix.")
    );
    write(
        root.path(),
        "commands/review.md",
        "---\ndescription: Changed\n---\nChanged\n",
    );
    assert!(import(root.path(), CommandDescriptionMode::RequireFrontmatter).is_empty());
    assert_eq!(fs::read_to_string(output).unwrap(), rendered);
}

#[test]
fn install_materializes_default_command_skills_and_removes_stale_outputs() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    write(
        &source,
        "commands/pr/review.md",
        "---\ndescription: Review a pull request\n---\nInspect the proposed changes.\n",
    );
    write(
        &source,
        "custom-skills/source-command-pr-review/SKILL.md",
        "---\nname: source-command-pr-review\ndescription: Native review skill\n---\n",
    );
    write(
        &source,
        "commands/oversized.md",
        &format!("---\ndescription: Oversized\n---\n{}", "x".repeat(4000)),
    );
    write(
        &source,
        ".codex-plugin/migrated-command-skills/stale/SKILL.md",
        "stale",
    );
    let (_, migrated) = install(root.path(), r#"{"name":"demo","skills":"./custom-skills"}"#);
    assert_eq!(
        fs::read_to_string(migrated.join("source-command-pr-review/SKILL.md")).unwrap(),
        "---\nname: \"source-command-pr-review\"\ndescription: \"Review a pull request\"\n---\n\n# source-command-pr-review\n\nUse this skill when the user asks to run the migrated source command `pr-review`.\n\n## Command Template\n\nInspect the proposed changes.\n"
    );
    assert!(!migrated.join("stale").exists());
    assert!(!migrated.join("source-command-oversized").exists());
    let installed = migrated.parent().unwrap().parent().unwrap();
    assert!(
        installed
            .join("custom-skills/source-command-pr-review/SKILL.md")
            .is_file()
    );
    assert!(
        source
            .join(".codex-plugin/migrated-command-skills/stale/SKILL.md")
            .is_file()
    );
    assert!(
        !source
            .join(".codex-plugin/migrated-command-skills/source-command-pr-review")
            .exists()
    );
}

#[test]
fn declared_command_files_and_overlapping_roots_deduplicate_sources() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    write(
        &source,
        "custom/review.md",
        "---\ndescription: Review\n---\nInspect\n",
    );
    write(
        &source,
        "commands/ignored.md",
        "---\ndescription: Ignored\n---\nIgnored\n",
    );
    let (_, migrated) = install(
        root.path(),
        r#"{"name":"demo","commands":["./custom","./custom/review.md"]}"#,
    );
    assert!(migrated.join("source-command-review/SKILL.md").is_file());
    assert_eq!(fs::read_dir(migrated).unwrap().count(), 1);
}

#[test]
fn invalid_commands_manifest_field_skips_default_commands() {
    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("source"),
        "commands/review.md",
        "---\ndescription: Review\n---\nReview\n",
    );
    let (_, migrated) = install(root.path(), r#"{"name":"demo","commands":{}}"#);
    assert!(!migrated.exists());
}

#[test]
fn command_migration_errors_warn_and_do_not_fail_installation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(source.join("commands")).unwrap();
    fs::write(source.join("commands/review.md"), [0xff]).unwrap();
    let (_, migrated) = install(
        root.path(),
        r#"{"name":"demo","commands":"./commands/review.md"}"#,
    );
    assert!(
        migrated
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("commands/review.md")
            .is_file()
    );
    assert!(!migrated.exists());
}
