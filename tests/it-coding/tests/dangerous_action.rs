//! Structured dangerous-action detection for the coding product.

use std::{num::NonZeroUsize, path::Path};

use ra_coding::dangerous_action::{
    BroadDeletionKind, DangerousAction, DangerousActionDetector, DangerousActionReport,
    DangerousShellCommand,
};
use ra_patch::{PatchAction, PatchPlan};
use tempfile::TempDir;

fn detector(workspace: &TempDir) -> DangerousActionDetector {
    DangerousActionDetector::new(workspace.path()).expect("workspace detector")
}

fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).expect("canonical path")
}

fn removes_files(report: &DangerousActionReport) -> bool {
    report.findings().iter().any(|finding| {
        matches!(
            finding,
            DangerousAction::DangerousShellCommand {
                kind: DangerousShellCommand::FileRemoval,
                ..
            }
        )
    })
}

fn deletes_broadly(report: &DangerousActionReport, expected: BroadDeletionKind) -> bool {
    report
        .findings()
        .iter()
        .any(|finding| matches!(finding, DangerousAction::BroadDeletion { kind, .. } if *kind == expected))
}

fn overwrites(report: &DangerousActionReport, expected: &Path) -> bool {
    report
        .findings()
        .iter()
        .any(|finding| matches!(finding, DangerousAction::OverwriteExistingFile { path, .. } if path == expected))
}

fn escapes_workspace(report: &DangerousActionReport, expected: &Path) -> bool {
    report
        .findings()
        .iter()
        .any(|finding| matches!(finding, DangerousAction::WriteOutsideWorkspace { path, .. } if path == expected))
}

#[test]
fn shell_detection_reads_commands_from_the_syntax_tree_not_quoted_data() {
    let workspace = TempDir::new().expect("workspace");
    let report = detector(&workspace).inspect_shell(
        "printf '%s\\n' 'rm -rf /'; if true; then rm -rf ./cache/*; fi",
        None,
    );

    assert!(removes_files(&report), "{:?}", report.findings());
    assert_eq!(
        report
            .findings()
            .iter()
            .filter(|finding| {
                matches!(
                    finding,
                    DangerousAction::DangerousShellCommand {
                        program,
                        kind: DangerousShellCommand::FileRemoval,
                        ..
                    } if program == "rm"
                )
            })
            .count(),
        1,
        "a word in a quoted printf argument is not another command"
    );
    assert!(deletes_broadly(&report, BroadDeletionKind::ShellRemoval));
}

#[test]
fn shell_detection_reports_existing_overwrites_and_workspace_escapes() {
    let workspace = TempDir::new().expect("workspace");
    let root = canonical(workspace.path());
    let existing = root.join("existing.txt");
    std::fs::write(&existing, "before\n").expect("existing file");
    let outside = root.parent().expect("temporary parent").join("outside.txt");
    let report = detector(&workspace).inspect_shell(
        "printf replacement > existing.txt; printf escape > ../outside.txt; touch ../created.txt; rm ../deleted.txt",
        None,
    );

    assert!(overwrites(&report, &existing), "{:?}", report.findings());
    assert!(escapes_workspace(&report, &outside));
    for filename in ["created.txt", "deleted.txt"] {
        assert!(escapes_workspace(
            &report,
            &outside.with_file_name(filename)
        ));
    }
}

#[test]
fn shell_detection_catches_symlink_escapes_and_structural_deletion_forms() {
    let workspace = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside");
    let canonical_outside = canonical(outside.path());
    let link = workspace.path().join("linked-outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), &link).expect("symlink");

    let report = detector(&workspace).inspect_shell(
        "printf escape > linked-outside/new.txt; find . -type f -delete; sudo mkfs.ext4 /dev/sda",
        None,
    );

    assert!(
        escapes_workspace(&report, &canonical_outside.join("new.txt")),
        "{:?}",
        report.findings()
    );
    assert!(deletes_broadly(&report, BroadDeletionKind::FindDelete));
    assert!(report.findings().iter().any(|finding| {
        matches!(
            finding,
            DangerousAction::DangerousShellCommand {
                kind: DangerousShellCommand::PrivilegeEscalation,
                ..
            }
        )
    }));
    assert!(report.findings().iter().any(|finding| {
        matches!(
            finding,
            DangerousAction::DangerousShellCommand {
                program,
                kind: DangerousShellCommand::FilesystemFormat,
                ..
            } if program == "mkfs.ext4"
        )
    }));
}

/// A `..` after a symbolic link climbs from the link's target, not from its lexical parent.
#[test]
fn parent_components_do_not_re_enter_the_workspace_through_a_link() {
    let workspace = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside");
    let canonical_outside = canonical(outside.path());
    std::fs::create_dir(canonical_outside.join("nested")).expect("nested directory");
    let link = workspace.path().join("linked-outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&canonical_outside, &link).expect("symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&canonical_outside, &link).expect("symlink");

    let report = detector(&workspace)
        .inspect_shell("printf escape > linked-outside/nested/../new.txt", None);

    assert!(
        escapes_workspace(&report, &canonical_outside.join("new.txt")),
        "{:?}",
        report.findings()
    );
}

#[test]
fn malformed_shell_is_not_reported_as_structurally_safe() {
    let workspace = TempDir::new().expect("workspace");
    let report = detector(&workspace).inspect_shell("printf broken >", None);

    assert!(
        report
            .findings()
            .contains(&DangerousAction::UnparseableShell)
    );
}

/// Silence on safe input is what makes the noisy cases worth reading.
#[test]
fn ordinary_commands_produce_no_findings() {
    let workspace = TempDir::new().expect("workspace");
    let root = canonical(workspace.path());
    std::fs::write(root.join("existing.txt"), "before\n").expect("existing file");
    let detector = detector(&workspace);

    for source in [
        "ls -la",
        "cargo build --release",
        "cat existing.txt | grep foo",
        "mkdir -p build/out",
        "printf x 2>&1",
        // A discard device is a stream, not a file that gets replaced.
        "echo hi > /dev/null",
        // Appending destroys nothing.
        "printf more >> existing.txt",
        "tee --append existing.txt",
        // A dry run deletes nothing.
        "git clean -nd",
    ] {
        let report = detector.inspect_shell(source, None);
        assert!(
            report.is_empty(),
            "{source:?} should be quiet, got {:?}",
            report.findings()
        );
    }
}

#[test]
fn output_operands_resolve_their_own_path() {
    let workspace = TempDir::new().expect("workspace");
    let root = canonical(workspace.path());
    let existing = root.join("existing.txt");
    std::fs::write(&existing, "before\n").expect("existing file");
    let detector = detector(&workspace);

    let report = detector.inspect_shell("dd if=/dev/zero of=existing.txt", None);
    assert!(overwrites(&report, &existing), "{:?}", report.findings());

    let report = detector.inspect_shell("dd if=/dev/zero of=../outside.txt", None);
    assert!(
        escapes_workspace(
            &report,
            &root.parent().expect("temporary parent").join("outside.txt")
        ),
        "{:?}",
        report.findings()
    );
}

/// A wrapper's own option values are not the program it runs.
#[test]
fn wrappers_reveal_the_program_behind_their_options() {
    let workspace = TempDir::new().expect("workspace");
    let detector = detector(&workspace);

    for source in [
        "timeout 5 rm -rf /",
        "timeout -s KILL 5 rm -rf /",
        "nice -n 10 rm -rf /",
        "sudo -u root rm -rf /",
        "sudo -i rm -rf /",
        "xargs rm -rf /",
        "xargs -n 1 rm -rf /",
        "sudo timeout 5 rm -rf /",
        // Every value-taking option of a listed wrapper must be known, or its value is read as the
        // program and the real command disappears.
        "stdbuf -o 0 rm -rf /",
        "stdbuf -i 4096 -o L rm -rf /",
        "env -S 'x y' rm -rf /",
        "env -u PATH rm -rf /",
        "doas -u root rm -rf /",
    ] {
        let report = detector.inspect_shell(source, None);
        assert!(
            removes_files(&report),
            "{source:?}: {:?}",
            report.findings()
        );
        assert!(
            escapes_workspace(&report, Path::new("/")),
            "{source:?}: {:?}",
            report.findings()
        );
        assert!(
            deletes_broadly(&report, BroadDeletionKind::ShellRemoval),
            "{source:?}: {:?}",
            report.findings()
        );
    }
}

#[test]
fn nested_scripts_are_inspected_or_declared_unreadable() {
    let workspace = TempDir::new().expect("workspace");
    let detector = detector(&workspace);

    let report = detector.inspect_shell("bash -c \"rm -rf /\"", None);
    assert!(removes_files(&report), "{:?}", report.findings());
    assert!(deletes_broadly(&report, BroadDeletionKind::ShellRemoval));

    let report = detector.inspect_shell("sh -c \"$COMMAND\"", None);
    assert!(
        report
            .findings()
            .contains(&DangerousAction::UnparseableShell),
        "{:?}",
        report.findings()
    );
}

#[test]
fn git_options_do_not_hide_the_destructive_subcommand() {
    let workspace = TempDir::new().expect("workspace");
    let detector = detector(&workspace);

    let report = detector.inspect_shell("git -C . reset --hard", None);
    assert!(
        report.findings().iter().any(|finding| {
            matches!(
                finding,
                DangerousAction::DangerousShellCommand {
                    kind: DangerousShellCommand::VersionControlDestruction,
                    ..
                }
            )
        }),
        "{:?}",
        report.findings()
    );

    // `d` and `x` count wherever they sit in a short-flag cluster.
    for source in ["git clean -fdx", "git clean -fd", "git clean -d -f"] {
        let report = detector.inspect_shell(source, None);
        assert!(
            deletes_broadly(&report, BroadDeletionKind::GitClean),
            "{source:?}: {:?}",
            report.findings()
        );
    }
}

/// A deletion target the detector cannot read is the least bounded case, not a reason to go quiet.
#[test]
fn unreadable_deletion_targets_count_as_broad() {
    let workspace = TempDir::new().expect("workspace");
    let detector = detector(&workspace);

    for source in ["rm -rf \"$HOME\"", "rm -rf $TARGET", "rm -rf \"${HOME}/x\""] {
        let report = detector.inspect_shell(source, None);
        assert!(
            deletes_broadly(&report, BroadDeletionKind::ShellRemoval),
            "{source:?}: {:?}",
            report.findings()
        );
    }
}

#[test]
fn directory_changes_are_followed_or_declared_unknown() {
    let workspace = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside");
    let canonical_outside = canonical(outside.path());
    let detector = detector(&workspace);

    let followed = format!("cd {} && rm -rf .", canonical_outside.display());
    let report = detector.inspect_shell(&followed, None);
    assert!(
        escapes_workspace(&report, &canonical_outside),
        "a followed `cd` reports the path actually at risk: {:?}",
        report.findings()
    );

    let report = detector.inspect_shell("cd \"$SOMEWHERE\" && rm -rf .", None);
    assert!(
        report
            .findings()
            .iter()
            .any(|finding| matches!(finding, DangerousAction::UnknownWorkingDirectory { .. })),
        "{:?}",
        report.findings()
    );
    assert!(
        !report.findings().iter().any(|finding| matches!(
            finding,
            DangerousAction::WriteOutsideWorkspace { .. }
                | DangerousAction::OverwriteExistingFile { .. }
        )),
        "an unresolvable base must not be reported as a concrete path: {:?}",
        report.findings()
    );
}

/// A syntax tree is not an execution trace: a `cd` the shell may skip must not silently rebase the
/// paths after it, because the workspace escape then resolves to a path inside the workspace.
#[test]
fn a_directory_change_that_may_not_run_keeps_both_readings() {
    let workspace = TempDir::new().expect("workspace");
    let root = canonical(workspace.path());
    std::fs::create_dir(root.join("subdir")).expect("subdir");
    let escaped = root.parent().expect("temporary parent").join("created.txt");
    let detector = detector(&workspace);

    for source in [
        // The right operand of `&&` and `||` runs only sometimes.
        "false && cd subdir; touch ../created.txt",
        "false || cd subdir; touch ../created.txt",
        // A branch body runs only sometimes.
        "if true; then cd subdir; fi; touch ../created.txt",
        // A pipeline stage and a subshell run in a child process.
        "echo x | cd subdir; touch ../created.txt",
        "(cd subdir); touch ../created.txt",
    ] {
        let report = detector.inspect_shell(source, None);
        assert!(
            escapes_workspace(&report, &escaped),
            "{source:?} writes outside the workspace: {:?}",
            report.findings()
        );
    }

    // A loop body runs an unknown number of times, so no directory can be named at all.
    let report =
        detector.inspect_shell("while true; do cd subdir; done; touch ../created.txt", None);
    assert!(
        report
            .findings()
            .iter()
            .any(|finding| matches!(finding, DangerousAction::UnknownWorkingDirectory { .. })),
        "{:?}",
        report.findings()
    );

    // An unconditional change is still followed, so the ordinary case stays quiet.
    let report = detector.inspect_shell("cd subdir; touch ../created.txt", None);
    assert!(report.is_empty(), "{:?}", report.findings());
}

/// A `cd` that fails leaves the shell where it was, which is a known directory, not an unknown one.
#[test]
fn a_directory_change_that_cannot_succeed_keeps_the_old_directory() {
    let workspace = TempDir::new().expect("workspace");
    let root = canonical(workspace.path());
    std::fs::write(root.join("afile.txt"), "x\n").expect("plain file");
    let escaped = root.parent().expect("temporary parent").join("escaped.txt");
    let detector = detector(&workspace);

    for source in [
        // The destination does not exist, so `cd` fails and the touch escapes from the old
        // directory. Reporting only uncertainty here would lose the escape entirely.
        "cd nowhere; touch ../escaped.txt",
        // The destination exists but is not a directory, so `cd` can never succeed.
        "cd afile.txt; touch ../escaped.txt",
    ] {
        let report = detector.inspect_shell(source, None);
        assert!(
            escapes_workspace(&report, &escaped),
            "{source:?} writes outside the workspace: {:?}",
            report.findings()
        );
    }

    // Deleting `.` still lands on the workspace root, because that is where the shell still is.
    let report = detector.inspect_shell("cd nowhere; rm -rf .", None);
    assert!(
        deletes_broadly(&report, BroadDeletionKind::ShellRemoval),
        "{:?}",
        report.findings()
    );

    // A `cd` onto a plain file changes nothing at all, so it raises nothing at all.
    let report = detector.inspect_shell("cd afile.txt; touch inside.txt", None);
    assert!(report.is_empty(), "{:?}", report.findings());
}

/// Tree depth is bounded only by input length, so the traversal must not use the call stack.
#[test]
fn deeply_nested_shell_does_not_overflow_the_stack() {
    let workspace = TempDir::new().expect("workspace");
    let depth = 50_000;
    let source = format!("{}true{}", "(".repeat(depth), ")".repeat(depth));

    let report = detector(&workspace).inspect_shell(&source, None);

    assert!(report.is_empty(), "{:?}", report.findings());
}

#[test]
fn the_large_delete_threshold_is_configurable() {
    let workspace = TempDir::new().expect("workspace");
    let threshold = NonZeroUsize::new(2).expect("non-zero threshold");
    let detector = detector(&workspace).with_large_delete_threshold(threshold);

    assert_eq!(detector.large_delete_threshold(), threshold);
    assert!(deletes_broadly(
        &detector.inspect_shell("rm -rf a b", None),
        BroadDeletionKind::ShellRemoval
    ));
    assert!(!deletes_broadly(
        &detector.inspect_shell("rm -rf a", None),
        BroadDeletionKind::ShellRemoval
    ));
}

/// `UpdateFile` exists to rewrite a file that is already there, so it is an overwrite by definition.
#[test]
fn patch_detection_reports_an_update_of_an_existing_file() {
    let workspace = TempDir::new().expect("workspace");
    let existing = canonical(workspace.path()).join("existing.txt");
    std::fs::write(&existing, "before\n").expect("existing file");
    let plan = PatchPlan::new(vec![PatchAction::UpdateFile {
        path: Path::new("existing.txt").to_path_buf(),
        hunks: Vec::new(),
    }]);

    let report = detector(&workspace).inspect_patch(&plan);

    assert!(overwrites(&report, &existing), "{:?}", report.findings());
}

#[test]
fn patch_detection_reports_existing_destinations_and_broad_deletion() {
    let workspace = TempDir::new().expect("workspace");
    let existing = canonical(workspace.path()).join("existing.txt");
    std::fs::write(&existing, "before\n").expect("existing file");
    let plan = PatchPlan::new(vec![
        PatchAction::AddFile {
            path: Path::new("existing.txt").to_path_buf(),
            content: "replacement\n".to_owned(),
        },
        PatchAction::DeleteFile {
            path: Path::new("one.txt").to_path_buf(),
        },
        PatchAction::DeleteFile {
            path: Path::new("two.txt").to_path_buf(),
        },
        PatchAction::DeleteFile {
            path: Path::new("three.txt").to_path_buf(),
        },
    ]);
    let report = detector(&workspace).inspect_patch(&plan);

    assert!(
        report.findings().iter().any(|finding| {
            matches!(
                finding,
                DangerousAction::OverwriteExistingFile { path, source: None } if path == &existing
            )
        }),
        "{:?}",
        report.findings()
    );
    assert!(report.findings().iter().any(|finding| {
        matches!(
            finding,
            DangerousAction::BroadDeletion {
                kind: BroadDeletionKind::PatchDelete,
                targets,
                source: None,
            } if targets.len() == 3
        )
    }));
}
