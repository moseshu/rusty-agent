//! `ra-core::sandbox::entries`: what a manifest declares should exist in the workspace.
//!
//! The behavior pinned here is what a backend reads before it materializes anything:
//! - the five built-in kinds, their defaults, and the fields every entry carries
//! - an entry type nobody registered is refused rather than carried as something unknown
//! - a type a host did register, and this crate does not model, survives with its fields intact
//! - an entry path that leaves the workspace is refused on the declaration alone

use ra_core::sandbox::{
    DEFAULT_GIT_HOST, Entry, EntryContent, EntryOwner, ErrorCode, FileMode, Group,
    ManifestRegistries, Permissions, User, default_entry_permissions, resolve_workspace_path,
};
use serde_json::{Value, json};

fn parse(value: &Value) -> Entry {
    Entry::parse(&ManifestRegistries::builtin(), value).expect("entry parses")
}

fn round_trip(entry: &Entry) -> Entry {
    parse(&entry.to_json().expect("entry renders"))
}

// --- the entries every manifest can declare -----------------------------------------------

#[test]
fn a_fresh_entry_is_readable_by_whatever_the_sandbox_runs_as() {
    let entry = Entry::file("hello");

    // 0755, which is wider than a bare `Permissions` default of 0700: manifest content is meant to
    // be reachable by the account the sandbox runs as, not only by whatever materialized it.
    assert_eq!(entry.permissions().to_mode(), 0o755);
    assert_eq!(entry.permissions(), default_entry_permissions());
    assert_ne!(entry.permissions(), Permissions::default());
    assert_eq!(entry.description(), None);
    assert_eq!(entry.group(), None);
    assert!(!entry.is_ephemeral());
    assert!(!entry.is_dir());
}

#[test]
fn content_that_puts_a_directory_somewhere_is_marked_as_one() {
    for entry in [
        Entry::dir(),
        Entry::local_dir(Some("./src".to_owned())),
        Entry::git_repo("owner/name", "main"),
    ] {
        assert!(entry.is_dir(), "{} must be a directory", entry.entry_type());
        assert!(entry.permissions().directory);
        assert_eq!(entry.permissions().to_mode() & 0o777, 0o755);
    }

    for entry in [Entry::file("x"), Entry::local_file("cfg.json")] {
        assert!(
            !entry.is_dir(),
            "{} must not be a directory",
            entry.entry_type()
        );
        assert!(!entry.permissions().directory);
    }
}

#[test]
fn a_directory_keeps_its_directory_flag_whatever_mode_is_requested() {
    // Whether a path is a directory follows from what is being put there. A mode that claimed
    // otherwise would render a directory's permissions as a file's.
    let entry = Entry::dir().with_permissions(Permissions::default().owner_can(FileMode::All));

    assert!(entry.permissions().directory);
    assert_eq!(entry.permissions().to_mode() & 0o777, 0o700);
}

#[test]
fn the_directory_flag_and_the_permission_bits_are_set_separately() {
    // The flag says what the sandbox filesystem should see; the mode says what may be done with it.
    // The reference keeps them apart, so an entry can claim one without the other.
    let entry = Entry::file("x").as_directory(true);

    assert!(entry.is_dir());
    assert!(!entry.permissions().directory);
    assert_eq!(round_trip(&entry), entry);
}

#[test]
fn an_entry_records_who_it_is_for_and_whether_it_survives_a_stop() {
    let entry = Entry::file("cache")
        .with_description("a warm cache")
        .ephemeral(true)
        .owned_by(EntryOwner::User(User::new("agent")));

    assert_eq!(entry.description(), Some("a warm cache"));
    assert!(entry.is_ephemeral());
    assert_eq!(entry.group(), Some(&EntryOwner::User(User::new("agent"))));
    assert_eq!(round_trip(&entry), entry);
}

#[test]
fn an_owner_may_be_named_by_a_user_or_by_a_group() {
    // The field is called `group` and what it does is `chgrp`. On a system where every account has
    // a group of its own, naming the user names that group, so both shapes have to read back.
    let by_group = Entry::dir().owned_by(EntryOwner::Group(Group::new(
        "dev",
        vec![User::new("agent")],
    )));
    let by_user = Entry::dir().owned_by(EntryOwner::User(User::new("agent")));

    assert_eq!(round_trip(&by_group), by_group);
    assert_eq!(round_trip(&by_user), by_user);
    assert_eq!(
        by_user.to_json().expect("renders")["group"],
        json!({"name": "agent"})
    );
}

#[test]
fn a_git_entry_clones_from_github_unless_it_says_otherwise() {
    let entry = Entry::git_repo("owner/name", "main");

    assert_eq!(
        entry.content(),
        &EntryContent::GitRepo {
            host: DEFAULT_GIT_HOST.to_owned(),
            repo: "owner/name".to_owned(),
            reference: "main".to_owned(),
            subpath: None,
        }
    );

    let narrowed = entry.from_git_host("git.example.com").with_subpath("pkg");
    let rendered = narrowed.to_json().expect("renders");
    assert_eq!(rendered["host"], json!("git.example.com"));
    // `ref` is the wire name; it is a keyword in Rust, so the field is `reference`.
    assert_eq!(rendered["ref"], json!("main"));
    assert_eq!(rendered["subpath"], json!("pkg"));
    assert_eq!(round_trip(&narrowed), narrowed);
}

#[test]
fn a_child_is_only_accepted_by_content_that_has_somewhere_to_put_it() {
    let dir = Entry::dir().with_child("README.md", Entry::file("hi"));
    assert_eq!(dir.children().map(|children| children.len()), Some(1));

    // A file has nowhere to put a child, and inventing somewhere would materialize content the
    // manifest never said was there.
    let file = Entry::file("hi").with_child("README.md", Entry::file("nested"));
    assert_eq!(file.children(), None);
    assert_eq!(file.content(), Entry::file("hi").content());
}

#[test]
fn a_directory_carries_its_children_down_and_back() {
    let entry = Entry::dir().with_description("project root").with_child(
        "src",
        Entry::dir().with_child("main.rs", Entry::file("fn main() {}").ephemeral(true)),
    );

    let rendered = entry.to_json().expect("renders");
    assert_eq!(rendered["type"], json!("dir"));
    assert_eq!(
        rendered["children"]["src"]["children"]["main.rs"]["content"],
        json!("fn main() {}")
    );
    assert_eq!(
        rendered["children"]["src"]["children"]["main.rs"]["ephemeral"],
        json!(true)
    );
    assert_eq!(round_trip(&entry), entry);
}

#[test]
fn every_entry_renders_the_fields_a_backend_reads() {
    let rendered = Entry::local_file("cfg.json")
        .with_description("configuration")
        .to_json()
        .expect("renders");

    assert_eq!(
        rendered,
        json!({
            "type": "local_file",
            "src": "cfg.json",
            "description": "configuration",
            "ephemeral": false,
            "group": Value::Null,
            "is_dir": false,
            "permissions": {"owner": 7, "group": 5, "other": 5, "directory": false},
        })
    );
}

#[test]
fn a_copied_directory_may_name_no_source_at_all() {
    let created = Entry::local_dir(None);

    assert_eq!(created.content(), &EntryContent::LocalDir { src: None });
    assert_eq!(created.to_json().expect("renders")["src"], Value::Null);
    assert_eq!(round_trip(&created), created);
}

#[test]
fn inline_content_that_is_not_text_cannot_be_written_out() {
    // Inline content is written as a JSON string so a manifest stays readable and editable by hand,
    // which is the reference's rendering: it refuses the same manifests rather than reaching for a
    // second encoding only one of the two implementations could read back.
    let entry = Entry::file(vec![0xff, 0xfe]);

    assert!(entry.to_json().is_err());
    assert!(serde_json::to_value(&entry).is_err());
}

// --- the open family ----------------------------------------------------------------------

#[test]
fn the_built_in_registry_holds_exactly_the_kinds_this_crate_models() {
    let registries = ManifestRegistries::builtin();

    assert_eq!(
        registries.entries().registered_types().collect::<Vec<_>>(),
        [
            "azure_blob_mount",
            "box_mount",
            "dir",
            "file",
            "gcs_mount",
            "git_repo",
            "local_dir",
            "local_file",
            "r2_mount",
            "s3_files_mount",
            "s3_mount",
        ]
    );
}

#[test]
fn an_entry_type_nobody_registered_is_refused_rather_than_carried() {
    // An entry whose meaning is unknown is one materialization would have to guess at, and the
    // guess would run somewhere other than where the manifest said.
    let error = Entry::parse(
        &ManifestRegistries::builtin(),
        &json!({"type": "blaxel_drive_mount", "drive_name": "b"}),
    )
    .unwrap_err();

    assert!(error.to_string().contains("blaxel_drive_mount"), "{error}");
}

#[test]
fn a_registered_type_this_crate_does_not_model_survives_with_its_fields() {
    let mut registries = ManifestRegistries::builtin();
    registries
        .entries_mut()
        .register("blaxel_drive_mount", "test")
        .expect("the type is free");

    let entry = Entry::parse(
        &registries,
        &json!({
            "type": "blaxel_drive_mount",
            "drive_name": "shared",
            "is_dir": true,
            "ephemeral": true,
            "permissions": {"owner": 7, "group": 0, "other": 0, "directory": true},
        }),
    )
    .expect("entry parses");

    assert_eq!(entry.entry_type(), "blaxel_drive_mount");
    assert!(entry.is_dir());
    assert!(entry.is_ephemeral());
    assert_eq!(entry.permissions().to_mode() & 0o777, 0o700);
    let EntryContent::Extension(payload) = entry.content() else {
        panic!("an unmodelled type is carried as an extension");
    };
    assert_eq!(payload.field("drive_name"), Some(&json!("shared")));
    // The fields every entry carries live on the entry, not twice over in the payload, where the
    // two copies could disagree and rendering would have to pick a winner.
    assert_eq!(payload.field("is_dir"), None);
    assert_eq!(payload.field("permissions"), None);

    assert_eq!(
        Entry::parse(&registries, &entry.to_json().expect("renders")).expect("re-parses"),
        entry
    );
}

#[test]
fn a_child_of_an_unknown_type_refuses_the_whole_entry() {
    // A directory that materialized every child it understood and skipped the rest would leave a
    // workspace that looks complete and is not.
    let error = Entry::parse(
        &ManifestRegistries::builtin(),
        &json!({
            "type": "dir",
            "children": {"data": {"type": "blaxel_drive_mount", "drive_name": "b"}},
        }),
    )
    .unwrap_err();

    assert!(error.to_string().contains("blaxel_drive_mount"), "{error}");
}

#[test]
fn a_built_in_entry_missing_a_required_field_is_refused() {
    let registries = ManifestRegistries::builtin();

    for payload in [
        json!({"type": "file"}),
        json!({"type": "local_file"}),
        json!({"type": "git_repo", "repo": "owner/name"}),
        json!({"type": "git_repo", "ref": "main"}),
        json!({"type": "dir", "children": []}),
        json!({"type": "file", "content": "x", "ephemeral": "yes"}),
    ] {
        assert!(
            Entry::parse(&registries, &payload).is_err(),
            "{payload} must be refused"
        );
    }
}

// --- where an entry lands -------------------------------------------------------------------

#[test]
fn an_entry_path_is_resolved_under_the_workspace_root() {
    assert_eq!(
        resolve_workspace_path("/workspace", "pkg/file.py")
            .expect("relative")
            .as_str(),
        "/workspace/pkg/file.py"
    );
    // An empty path names the root itself.
    assert_eq!(
        resolve_workspace_path("/workspace", "")
            .expect("root")
            .as_str(),
        "/workspace"
    );
}

#[test]
fn an_entry_path_is_held_to_the_segments_as_written() {
    // `pkg/../pkg/file` resolves back inside the workspace, and the reference refuses it anyway: an
    // entry path is written by hand in a manifest rather than typed by a model mid-run, so it is
    // checked as written rather than after normalization.
    let error = resolve_workspace_path("/workspace", "pkg/../pkg/file.py").unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(
        error.message(),
        "manifest path must not escape root: pkg/../pkg/file.py"
    );
    assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
}

#[test]
fn an_absolute_entry_path_is_refused_in_either_flavour() {
    for path in ["/tmp/outside.txt", "C:\\tmp\\outside.txt"] {
        let error = resolve_workspace_path("/workspace", path).unwrap_err();
        assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
        assert_eq!(error.context().get("reason"), Some(&json!("absolute")));
    }

    let windows = resolve_workspace_path("/workspace", "C:\\tmp\\outside.txt").unwrap_err();
    assert_eq!(
        windows.message(),
        "manifest path must be relative: C:/tmp/outside.txt"
    );
    assert_eq!(
        windows.context().get("rel"),
        Some(&json!("C:/tmp/outside.txt"))
    );
}
