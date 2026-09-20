//! `ra-core::sandbox::workspace_paths`: where a sandbox path may point, and how it is rendered.
//!
//! The behavior pinned here is what decides whether a path is reachable:
//! - a Windows drive path is refused as absolute rather than coerced into a relative-looking one
//! - a relative path is anchored under the workspace root, and one that climbs out of it is refused
//!   even when a grant covers where it landed
//! - a grant widens access to exactly one subtree, the innermost grant decides, and a read-only one
//!   refuses writes
//! - a run working directory changes where relative paths are measured from and nothing else

use ra_core::sandbox::{
    CwdError, ErrorCode, PathGrantError, PosixPath, SandboxPathGrant, SandboxWorkspaceScope,
    ScopePathError, WorkspacePathPolicy, normalize_sandbox_cwd, windows_absolute_path,
};
use serde_json::json;

fn policy(root: &str) -> WorkspacePathPolicy {
    WorkspacePathPolicy::new(root, Vec::new()).expect("root is absolute")
}

fn granted(root: &str, grants: Vec<SandboxPathGrant>) -> WorkspacePathPolicy {
    WorkspacePathPolicy::new(root, grants).expect("root is absolute")
}

fn grant(path: &str) -> SandboxPathGrant {
    SandboxPathGrant::new(path).expect("grant path is absolute")
}

// --- POSIX path algebra -------------------------------------------------------------------

#[test]
fn a_posix_path_names_its_anchor_and_components() {
    assert_eq!(
        PosixPath::new("/workspace/pkg").parts(),
        ["/", "workspace", "pkg"]
    );
    assert_eq!(PosixPath::new("pkg/file.py").parts(), ["pkg", "file.py"]);
    // A bare `.` names nothing, so it contributes no component; `..` names something and stays.
    assert_eq!(PosixPath::new("a/./b").parts(), ["a", "b"]);
    assert_eq!(PosixPath::new("a/../b").parts(), ["a", "..", "b"]);
    assert!(PosixPath::new(".").parts().is_empty());
}

#[test]
fn constructing_a_path_tidies_its_spelling_but_leaves_parent_segments_alone() {
    // The reference's path type does this much when it is handed a string, and a refusal quotes
    // the path after it. A `..` survives: resolving one decides where a path lands, which is a
    // separate step from how it was spelled.
    for (written, tidied) in [
        ("a/./b", "a/b"),
        ("a//b", "a/b"),
        ("a/", "a"),
        ("", "."),
        (".", "."),
        ("///", "/"),
        ("/", "/"),
        ("//", "//"),
        ("./plot.png", "plot.png"),
        ("pkg/../../secret.txt", "pkg/../../secret.txt"),
    ] {
        assert_eq!(
            PosixPath::new(written).as_str(),
            tidied,
            "{written:?} must tidy to {tidied:?}"
        );
    }
}

#[test]
fn exactly_two_leading_slashes_are_their_own_anchor() {
    // POSIX leaves `//foo` to the implementation, so it is not the same path as `/foo`. Three or
    // more collapse, which is why the distinction has to be made on the count rather than on
    // "starts with a slash".
    assert_eq!(PosixPath::new("//srv").parts(), ["//", "srv"]);
    assert_eq!(PosixPath::new("///srv").parts(), ["/", "srv"]);
    assert!(PosixPath::new("//").is_filesystem_root());
    assert!(PosixPath::new("/").is_filesystem_root());
    assert!(!PosixPath::new("/srv").is_filesystem_root());
}

#[test]
fn normalizing_drops_a_parent_segment_that_would_climb_past_an_absolute_root() {
    assert_eq!(
        PosixPath::new("/workspace/pkg/../file.py")
            .normalized()
            .as_str(),
        "/workspace/file.py"
    );
    // An absolute path cannot climb above `/`, so the segment is dropped and the result lands
    // somewhere real — which the workspace check then refuses, rather than resolving elsewhere.
    assert_eq!(
        PosixPath::new("/workspace/../../etc").normalized().as_str(),
        "/etc"
    );
    // A relative path keeps a leading `..`: there is nothing above it to drop the segment against.
    assert_eq!(
        PosixPath::new("pkg/../../secret.txt").normalized().as_str(),
        "../secret.txt"
    );
    assert_eq!(PosixPath::new("a/..").normalized().as_str(), ".");
    assert_eq!(PosixPath::new("").normalized().as_str(), ".");
    assert_eq!(PosixPath::new("///").normalized().as_str(), "/");
}

#[test]
fn a_shared_name_prefix_is_not_containment() {
    let root = PosixPath::new("/workspace");

    assert!(PosixPath::new("/workspace").is_under(&root));
    assert!(PosixPath::new("/workspace/pkg").is_under(&root));
    // Comparison is by component: `/workspace-alias` shares a character prefix and nothing else.
    assert!(!PosixPath::new("/workspace-alias/pkg").is_under(&root));
    assert_eq!(
        PosixPath::new("/workspace/pkg/file.py")
            .relative_to(&root)
            .map(PosixPath::into),
        Some(String::from("pkg/file.py"))
    );
    assert_eq!(
        PosixPath::new("/workspace")
            .relative_to(&root)
            .map(PosixPath::into),
        Some(String::from("."))
    );
}

// --- Windows drive syntax -----------------------------------------------------------------

#[test]
fn only_drive_absolute_syntax_counts_as_a_windows_absolute_path() {
    assert_eq!(
        windows_absolute_path("C:\\tmp\\secret.txt").as_deref(),
        Some("C:/tmp/secret.txt")
    );
    assert_eq!(windows_absolute_path("c:/tmp").as_deref(), Some("c:/tmp"));
    // A rooted path without a drive is not absolute on Windows, and a UNC path is absolute in both
    // flavours: neither needs the refusal that exists for drive paths.
    assert_eq!(windows_absolute_path("/tmp"), None);
    assert_eq!(windows_absolute_path("//server/share"), None);
    // A drive with no root is relative on Windows too.
    assert_eq!(windows_absolute_path("C:tmp"), None);
    assert_eq!(windows_absolute_path("pkg/file.py"), None);
}

#[test]
fn a_windows_drive_path_is_refused_as_absolute_rather_than_anchored_under_the_root() {
    let policy = policy("/workspace");

    // Coerced to POSIX this would read as the relative path `C:/tmp/secret.txt` and be anchored
    // under the workspace, which is how a path that names another drive ends up looking internal.
    for path in ["C:/tmp/secret.txt", "C:\\tmp\\secret.txt"] {
        for error in [
            policy.absolute_workspace_path(path).unwrap_err(),
            policy.relative_path(path).unwrap_err(),
            policy.normalize_sandbox_path(path, false).unwrap_err(),
        ] {
            assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
            assert_eq!(
                error.message(),
                "manifest path must be relative: C:/tmp/secret.txt"
            );
            assert_eq!(
                error.context().get("rel"),
                Some(&json!("C:/tmp/secret.txt"))
            );
            assert_eq!(error.context().get("reason"), Some(&json!("absolute")));
        }
    }
}

// --- workspace anchoring ------------------------------------------------------------------

#[test]
fn a_relative_path_anchors_under_the_root_and_an_absolute_one_inside_it_is_accepted() {
    let policy = policy("/workspace");

    for (path, expected) in [
        ("pkg/file.py", "/workspace/pkg/file.py"),
        ("/workspace/pkg/file.py", "/workspace/pkg/file.py"),
        ("/workspace/pkg/../file.py", "/workspace/file.py"),
        ("pkg/../secret.txt", "/workspace/secret.txt"),
    ] {
        assert_eq!(
            policy.absolute_workspace_path(path).expect(path).as_str(),
            expected
        );
    }
}

#[test]
fn a_path_that_leaves_the_workspace_is_refused_and_quoted_as_written() {
    let policy = policy("/workspace");

    let absolute = policy
        .absolute_workspace_path("/tmp/secret.txt")
        .unwrap_err();
    assert_eq!(
        absolute.message(),
        "manifest path must be relative: /tmp/secret.txt"
    );
    assert_eq!(absolute.context().get("reason"), Some(&json!("absolute")));

    // The refusal quotes the path the caller wrote, not the normalized one it resolved to: a
    // message naming `/secret.txt` would send them looking for a path they never asked for.
    for path in ["../secret.txt", "pkg/../../secret.txt"] {
        let error = policy.absolute_workspace_path(path).unwrap_err();
        assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
        assert_eq!(
            error.message(),
            format!("manifest path must not escape root: {path}")
        );
        assert_eq!(error.context().get("rel"), Some(&json!(path)));
        assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
    }
}

#[test]
fn a_workspace_path_is_handed_back_measured_from_the_root() {
    let policy = policy("/workspace");

    assert_eq!(
        policy
            .relative_path("pkg/file.py")
            .expect("relative")
            .as_str(),
        "pkg/file.py"
    );
    assert_eq!(
        policy
            .relative_path("/workspace/pkg/file.py")
            .expect("absolute")
            .as_str(),
        "pkg/file.py"
    );
    assert_eq!(
        policy
            .relative_path("pkg/../secret.txt")
            .expect("normalized")
            .as_str(),
        "secret.txt"
    );
    assert_eq!(
        policy.relative_path("/workspace").expect("root").as_str(),
        "."
    );
}

#[test]
fn a_provider_root_is_not_exposed_by_a_relative_answer() {
    // Where a provider put the workspace is not the model's business, so the answer is always
    // measured from the root even when the question named the absolute path.
    let policy = policy("/provider/private/root");

    assert_eq!(
        policy
            .relative_path("/provider/private/root/images/dot.png")
            .expect("inside root")
            .as_str(),
        "images/dot.png"
    );
}

#[test]
fn a_relative_workspace_root_is_refused() {
    // A relative root would measure every check from wherever the process happened to be.
    assert!(WorkspacePathPolicy::new("workspace", Vec::new()).is_err());
    assert_eq!(policy("/workspace").sandbox_root().as_str(), "/workspace");
}

// --- path grants --------------------------------------------------------------------------

#[test]
fn a_grant_widens_access_to_exactly_one_subtree() {
    let policy = granted("/workspace", vec![grant("/tmp")]);

    assert_eq!(
        policy
            .normalize_sandbox_path("/tmp/result.txt", false)
            .expect("granted")
            .as_str(),
        "/tmp/result.txt"
    );
    assert_eq!(
        policy
            .normalize_sandbox_path("pkg/file.py", false)
            .expect("workspace")
            .as_str(),
        "/workspace/pkg/file.py"
    );

    let error = policy
        .normalize_sandbox_path("/var/result.txt", false)
        .unwrap_err();
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(
        error.message(),
        "manifest path must be relative: /var/result.txt"
    );
}

#[test]
fn a_grant_is_reachable_only_by_naming_it_outright() {
    let policy = granted("/workspace", vec![grant("/tmp")]);

    // `/workspace/../tmp/result.txt` normalizes into the granted subtree. Accepting it would mean
    // a relative path could climb out of the workspace whenever a grant happened to catch it.
    let error = policy
        .normalize_sandbox_path("../tmp/result.txt", false)
        .unwrap_err();
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
}

#[test]
fn a_read_only_grant_allows_reads_and_refuses_writes() {
    let policy = granted("/workspace", vec![grant("/opt/toolchain").read_only(true)]);

    assert_eq!(
        policy
            .normalize_sandbox_path("/opt/toolchain/cache.db", false)
            .expect("read is allowed")
            .as_str(),
        "/opt/toolchain/cache.db"
    );

    let error = policy
        .normalize_sandbox_path("/opt/toolchain/cache.db", true)
        .unwrap_err();
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error.message(),
        "failed to write archive for path: /opt/toolchain/cache.db"
    );
    assert_eq!(
        error.context().get("path"),
        Some(&json!("/opt/toolchain/cache.db"))
    );
    assert_eq!(
        error.context().get("reason"),
        Some(&json!("read_only_extra_path_grant"))
    );
    assert_eq!(
        error.context().get("grant_path"),
        Some(&json!("/opt/toolchain"))
    );
}

#[test]
fn the_innermost_grant_decides_whether_a_write_is_allowed() {
    // A read-only grant nested inside a writable one is a narrowing. First-match would make the
    // narrower rule depend on the order the grants happened to be listed in.
    let policy = granted(
        "/workspace",
        vec![grant("/opt"), grant("/opt/toolchain").read_only(true)],
    );

    assert!(
        policy
            .normalize_sandbox_path("/opt/scratch/out.txt", true)
            .is_ok()
    );
    let error = policy
        .normalize_sandbox_path("/opt/toolchain/cache.db", true)
        .unwrap_err();
    assert_eq!(
        error.context().get("grant_path"),
        Some(&json!("/opt/toolchain"))
    );
}

#[test]
fn grant_rules_report_each_granted_root_and_its_access() {
    let policy = granted(
        "/workspace",
        vec![grant("/tmp"), grant("/opt/toolchain").read_only(true)],
    );

    let rules = policy
        .extra_path_grant_rules()
        .expect("grants are POSIX absolute");
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].0.as_str(), "/tmp");
    assert!(!rules[0].1);
    assert_eq!(rules[1].0.as_str(), "/opt/toolchain");
    assert!(rules[1].1);
}

#[test]
fn a_grant_must_name_an_absolute_path_that_is_not_the_filesystem_root() {
    assert_eq!(
        SandboxPathGrant::new("tmp").unwrap_err(),
        PathGrantError::PathNotPosixAbsolute
    );
    // Granting the root would grant every path at once, which is not a grant.
    assert_eq!(
        SandboxPathGrant::new("/").unwrap_err(),
        PathGrantError::FilesystemRoot
    );
    assert_eq!(
        SandboxPathGrant::new("//").unwrap_err(),
        PathGrantError::FilesystemRoot
    );
}

#[test]
fn a_grant_path_is_normalized_and_keeps_what_it_was_given() {
    let grant = grant("/tmp/../opt/toolchain")
        .read_only(true)
        .with_description("compiler runtime");

    assert_eq!(grant.path(), "/opt/toolchain");
    assert!(grant.is_read_only());
    assert_eq!(grant.description(), Some("compiler runtime"));
    assert_eq!(grant.host_path(), None);
}

#[test]
fn a_host_source_is_rendered_the_way_the_host_would_write_it() {
    let split = grant("/mnt/shared-data")
        .with_host_path("C:/Users/example/shared-data")
        .expect("drive-absolute host path");

    assert_eq!(split.path(), "/mnt/shared-data");
    assert_eq!(split.host_path(), Some("C:\\Users\\example\\shared-data"));

    // Trailing whitespace is part of the name on a POSIX filesystem, so it survives.
    let spaced = grant("/mnt/shared-data")
        .with_host_path("/srv/shared ")
        .expect("absolute host path");
    assert_eq!(spaced.host_path(), Some("/srv/shared "));
}

#[test]
fn a_host_source_must_be_absolute_and_free_of_parent_segments() {
    let cases = [
        ("relative/path", PathGrantError::HostPathNotAbsolute),
        ("/", PathGrantError::FilesystemRoot),
        ("C:\\", PathGrantError::FilesystemRoot),
        ("/srv/../secret", PathGrantError::HostPathParentSegments),
        ("//server/share", PathGrantError::HostPathUnc),
        ("\\\\server\\share", PathGrantError::HostPathUnc),
    ];

    for (host_path, expected) in cases {
        assert_eq!(
            grant("/mnt/shared-data")
                .with_host_path(host_path)
                .unwrap_err(),
            expected,
            "{host_path}"
        );
    }
}

#[test]
fn a_grant_round_trips_through_serialization_and_is_revalidated_on_the_way_back() {
    let original = grant("/opt/toolchain")
        .read_only(true)
        .with_description("compiler runtime");
    let rendered = serde_json::to_value(&original).expect("grant serializes");

    // `host_path` is omitted when the two halves are the same place; the rest is always written.
    assert_eq!(
        rendered,
        json!({"path": "/opt/toolchain", "read_only": true, "description": "compiler runtime"})
    );
    assert_eq!(
        serde_json::from_value::<SandboxPathGrant>(rendered).expect("grant parses"),
        original
    );

    // Reading a grant back is exactly where an unchecked path would matter, so the same rules
    // apply as when one is constructed.
    assert!(serde_json::from_value::<SandboxPathGrant>(json!({"path": "/"})).is_err());
    assert!(serde_json::from_value::<SandboxPathGrant>(json!({"path": "tmp"})).is_err());
    assert!(
        serde_json::from_value::<SandboxPathGrant>(
            json!({"path": "/mnt/data", "host_path": "/srv/../secret"})
        )
        .is_err()
    );
}

// --- run working directory ----------------------------------------------------------------

#[test]
fn a_run_working_directory_must_name_a_place_inside_the_workspace() {
    let cases = [
        ("", CwdError::Empty),
        ("   ", CwdError::Empty),
        ("/workspace/tasks/a", CwdError::NotRelative),
        ("C:/tasks/a", CwdError::NotRelative),
        ("tasks/../a", CwdError::ParentSegments),
        ("tasks\\a", CwdError::Separators),
    ];

    for (cwd, expected) in cases {
        assert_eq!(normalize_sandbox_cwd(cwd).unwrap_err(), expected, "{cwd}");
    }
    assert_eq!(
        normalize_sandbox_cwd("tasks/./a").expect("valid").as_str(),
        "tasks/a"
    );
}

#[test]
fn a_parent_segment_is_refused_rather_than_normalized_away() {
    // `tasks/../a` resolves to `a`, which is inside the workspace. Accepting it would mean
    // accepting the form up to the point where normalization happens to save it.
    assert_eq!(
        normalize_sandbox_cwd("tasks/../a").unwrap_err(),
        CwdError::ParentSegments
    );
    assert!(SandboxWorkspaceScope::from_cwd(Some("tasks/../a")).is_err());
}

#[test]
fn a_scope_anchors_relative_paths_and_leaves_absolute_ones_alone() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks/a")).expect("valid cwd");

    assert_eq!(scope.anchor("plot.png"), "tasks/a/plot.png");
    assert_eq!(scope.anchor("reports/plot.png"), "tasks/a/reports/plot.png");
    // The anchored path is spelled the way the reference's path type spells it.
    assert_eq!(scope.anchor("./plot.png"), "tasks/a/plot.png");
    // A path that already says where it starts is not measured from anywhere.
    assert_eq!(scope.anchor("/workspace/plot.png"), "/workspace/plot.png");
    assert_eq!(scope.anchor("C:/plot.png"), "C:/plot.png");
}

#[test]
fn anchoring_passes_a_model_written_path_through_verbatim() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks/a")).expect("valid cwd");

    // At this point the text is still whatever a model wrote. A backslash here is part of a name
    // on a POSIX filesystem, and translating it would invent a directory boundary.
    assert_eq!(
        scope.anchor("reports\\plot.png"),
        "tasks/a/reports\\plot.png"
    );
}

#[test]
fn a_scope_without_a_working_directory_measures_from_the_root() {
    let scope = SandboxWorkspaceScope::root();

    assert_eq!(scope.anchor("plot.png"), "plot.png");
    assert_eq!(scope.cwd(), None);
    assert_eq!(
        scope
            .model_path("reports/plot.png")
            .expect("relative")
            .as_str(),
        "reports/plot.png"
    );
}

#[test]
fn a_model_path_is_measured_from_the_working_directory() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks/a")).expect("valid cwd");

    assert_eq!(
        scope
            .model_path("tasks/a/plot.png")
            .expect("inside cwd")
            .as_str(),
        "plot.png"
    );
    assert_eq!(
        scope
            .model_path("shared/skill/SKILL.md")
            .expect("outside cwd")
            .as_str(),
        "../../shared/skill/SKILL.md"
    );
    assert_eq!(
        scope.model_path("/workspace/plot.png").unwrap_err(),
        ScopePathError::DisplayPathAbsolute
    );
}

#[test]
fn a_result_path_answers_in_the_form_the_question_was_asked() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks")).expect("valid cwd");

    assert_eq!(
        scope
            .display_path("../shared.txt", "shared.txt")
            .expect("relative question")
            .as_str(),
        "../shared.txt"
    );
    assert_eq!(
        scope
            .display_path("/workspace/shared.txt", "shared.txt")
            .expect("absolute question")
            .as_str(),
        "shared.txt"
    );
}

#[test]
fn a_session_resource_becomes_absolute_once_there_is_a_working_directory() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks/a")).expect("valid cwd");

    // A resource named in instructions outlives the directory the model was in when it read them:
    // a shell command that selects a nested workdir would strand a cwd-relative path.
    assert_eq!(
        scope
            .model_resource_path("/workspace", ".agents/my-skill")
            .expect("resource")
            .as_str(),
        "/workspace/.agents/my-skill"
    );
    assert_eq!(
        scope
            .model_resource_path("C:/workspace", ".agents/my-skill")
            .expect("drive root")
            .as_str(),
        "C:/workspace/.agents/my-skill"
    );
    // Without one, the existing workspace-root-relative representation is preserved.
    assert_eq!(
        SandboxWorkspaceScope::root()
            .model_resource_path("/workspace", ".agents/my-skill")
            .expect("resource")
            .as_str(),
        ".agents/my-skill"
    );
}

#[test]
fn a_session_resource_must_be_a_non_empty_workspace_relative_posix_path() {
    let scope = SandboxWorkspaceScope::from_cwd(Some("tasks/a")).expect("valid cwd");
    let cases = [
        (
            "/workspace/.agents/my-skill",
            ScopePathError::ResourceNotRelative,
        ),
        ("../my-skill", ScopePathError::ResourceNotRelative),
        ("C:/skills/my-skill", ScopePathError::ResourceNotRelative),
        (".agents\\my-skill", ScopePathError::ResourceSeparators),
        ("", ScopePathError::ResourceEmpty),
    ];

    for (path, expected) in cases {
        assert_eq!(
            scope.model_resource_path("/workspace", path).unwrap_err(),
            expected,
            "{path}"
        );
    }

    for root in ["\\workspace", "workspace"] {
        assert_eq!(
            scope
                .model_resource_path(root, ".agents/my-skill")
                .unwrap_err(),
            ScopePathError::RootNotPosixAbsolute,
            "{root}"
        );
    }
}
