use std::path::PathBuf;

use ra_core::compat::{Compatibility, SchemaVersion};
use ra_patch::{
    CommittedPatchDelta, PATCH_SCHEMA_VERSION, PatchAction, PatchConflict, PatchHunk,
    PatchMatchLevel, PatchPlan,
};
use serde_json::json;

#[test]
fn test_patch_plan_and_action_construction() {
    let hunk = PatchHunk::new()
        .with_line_hint(42)
        .with_context_before(["fn foo() {"])
        .with_removed_lines(["    old_code();"])
        .with_added_lines(["    new_code();"])
        .with_context_after(["}"]);

    assert_eq!(hunk.line_hint(), Some(42));
    assert_eq!(hunk.context_before(), &["fn foo() {"]);
    assert_eq!(hunk.removed_lines(), &["    old_code();"]);
    assert_eq!(hunk.added_lines(), &["    new_code();"]);
    assert_eq!(hunk.context_after(), &["}"]);
    assert!(hunk.unknown().is_empty());

    let actions = vec![
        PatchAction::UpdateFile {
            path: PathBuf::from("src/main.rs"),
            hunks: vec![hunk],
        },
        PatchAction::AddFile {
            path: PathBuf::from("src/lib.rs"),
            content: "pub fn init() {}\n".to_owned(),
        },
        PatchAction::DeleteFile {
            path: PathBuf::from("src/old.rs"),
        },
        PatchAction::MoveFile {
            from: PathBuf::from("src/legacy.rs"),
            to: PathBuf::from("src/modern.rs"),
        },
    ];

    let plan = PatchPlan::new(actions).with_match_level(PatchMatchLevel::TrimEnd);

    assert_eq!(plan.schema_version(), PATCH_SCHEMA_VERSION);
    assert_eq!(plan.match_level(), PatchMatchLevel::TrimEnd);
    assert!(!plan.has_conflicts());
    assert!(plan.unknown().is_empty());

    // target_files must contain both source and destination of MoveFile, plus the other actions
    let targets = plan.target_files();
    assert_eq!(targets.len(), 5);
    assert!(targets.contains(&PathBuf::from("src/main.rs")));
    assert!(targets.contains(&PathBuf::from("src/lib.rs")));
    assert!(targets.contains(&PathBuf::from("src/old.rs")));
    assert!(targets.contains(&PathBuf::from("src/legacy.rs")));
    assert!(targets.contains(&PathBuf::from("src/modern.rs")));
}

#[test]
fn test_patch_conflicts_reporting() {
    let plan = PatchPlan::new(vec![PatchAction::DeleteFile {
        path: PathBuf::from("missing.rs"),
    }])
    .with_conflicts(vec![
        PatchConflict::FileNotFound {
            path: PathBuf::from("missing.rs"),
        },
        PatchConflict::AmbiguousMatch {
            path: PathBuf::from("ambiguous.rs"),
            hunk_index: 0,
            candidate_count: 3,
        },
        PatchConflict::HunkFailed {
            path: PathBuf::from("broken.rs"),
            hunk_index: 1,
            level: PatchMatchLevel::Exact,
        },
    ]);

    assert!(plan.has_conflicts());
    assert_eq!(plan.conflicts().len(), 3);
}

#[test]
fn test_committed_patch_delta() {
    let delta = CommittedPatchDelta::new(
        vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
        15,
        4,
    );

    assert_eq!(delta.schema_version(), PATCH_SCHEMA_VERSION);
    assert!(!delta.is_empty());
    assert_eq!(delta.applied_files().len(), 2);
    assert_eq!(delta.lines_added(), 15);
    assert_eq!(delta.lines_removed(), 4);
    assert!(delta.unknown().is_empty());

    let empty_delta = CommittedPatchDelta::empty();
    assert!(empty_delta.is_empty());
    assert_eq!(empty_delta.lines_added(), 0);
    assert_eq!(empty_delta.lines_removed(), 0);
}

#[test]
fn test_patch_serialization_roundtrip_and_forward_compat() {
    let plan = PatchPlan::new(vec![
        PatchAction::AddFile {
            path: PathBuf::from("new.rs"),
            content: "// hello".to_owned(),
        },
        PatchAction::MoveFile {
            from: PathBuf::from("old.rs"),
            to: PathBuf::from("new_loc.rs"),
        },
    ]);

    let json_val = serde_json::to_value(&plan).expect("must serialize plan");
    assert_eq!(json_val["schema_version"], 1);

    let restored: PatchPlan = serde_json::from_value(json_val).expect("must deserialize plan");
    assert_eq!(plan, restored);
    assert_eq!(restored.target_files().len(), 3);
    assert!(restored.unknown().is_empty());

    // Forward compatibility: unknown top-level and hunk fields must be retained
    let future_payload = json!({
        "schema_version": 2,
        "actions": [{
            "action": "update_file",
            "path": "src/lib.rs",
            "hunks": [{
                "line_hint": 10,
                "added_lines": ["pub fn v2() {}"],
                "future_hunk_tag": "optimistic_merge"
            }]
        }],
        "match_level": "exact",
        "future_execution_mode": "atomic_tx"
    });

    let future_plan: PatchPlan =
        serde_json::from_value(future_payload).expect("must deserialize future plan");
    assert_eq!(future_plan.schema_version(), SchemaVersion::new(2));
    assert_eq!(
        future_plan.unknown().get("future_execution_mode"),
        Some(&json!("atomic_tx"))
    );

    if let PatchAction::UpdateFile { hunks, .. } = &future_plan.actions()[0] {
        assert_eq!(
            hunks[0].unknown().get("future_hunk_tag"),
            Some(&json!("optimistic_merge"))
        );
    } else {
        panic!("expected UpdateFile action");
    }

    let reserialized = serde_json::to_value(&future_plan).expect("must reserialize");
    assert_eq!(reserialized["future_execution_mode"], "atomic_tx");
    assert_eq!(
        reserialized["actions"][0]["hunks"][0]["future_hunk_tag"],
        "optimistic_merge"
    );

    // CommittedPatchDelta forward compatibility
    let future_delta_payload = json!({
        "schema_version": 2,
        "applied_files": ["src/lib.rs"],
        "lines_added": 5,
        "lines_removed": 1,
        "backup_snapshot_ref": "snap-99"
    });

    let future_delta: CommittedPatchDelta =
        serde_json::from_value(future_delta_payload).expect("must deserialize future delta");
    assert_eq!(future_delta.schema_version(), SchemaVersion::new(2));
    assert_eq!(
        future_delta.unknown().get("backup_snapshot_ref"),
        Some(&json!("snap-99"))
    );

    let delta_reserialized = serde_json::to_value(&future_delta).expect("must reserialize delta");
    assert_eq!(delta_reserialized["backup_snapshot_ref"], "snap-99");
}

#[test]
fn test_patch_records_use_the_core_compat_vocabulary() {
    // The version a patch record carries is `ra_core::compat::SchemaVersion` itself, not a
    // same-named type declared in this crate. It has to be: a caller holding both a patch record
    // and a framework record answers "does this need migrating" through one `Compatibility`, and
    // two types would make that answer reachable for one and not the other. These annotations are
    // the assertion — they stop compiling the day a second pair of types comes back.
    let plan: PatchPlan = PatchPlan::new(vec![PatchAction::DeleteFile {
        path: PathBuf::from("gone.rs"),
    }]);
    let plan_version: SchemaVersion = plan.schema_version();
    assert_eq!(plan_version, PATCH_SCHEMA_VERSION);
    assert_eq!(
        plan_version.compatibility(PATCH_SCHEMA_VERSION),
        Compatibility::Same
    );

    let delta_version: SchemaVersion = CommittedPatchDelta::empty().schema_version();
    assert_eq!(delta_version, PATCH_SCHEMA_VERSION);

    // A record from a newer build is readable and reports why, rather than being rejected.
    let newer: PatchPlan = serde_json::from_value(json!({
        "schema_version": 9,
        "actions": [],
        "match_level": "exact"
    }))
    .expect("a newer record must still be readable");
    assert_eq!(
        newer.schema_version().compatibility(PATCH_SCHEMA_VERSION),
        Compatibility::Newer
    );
    assert!(!newer.schema_version().compatibility(PATCH_SCHEMA_VERSION).needs_migration());

    // And an older one reports that it does need one.
    let older = SchemaVersion::new(0);
    assert_eq!(
        older.compatibility(PATCH_SCHEMA_VERSION),
        Compatibility::Older
    );
    assert!(older.compatibility(PATCH_SCHEMA_VERSION).needs_migration());
}
