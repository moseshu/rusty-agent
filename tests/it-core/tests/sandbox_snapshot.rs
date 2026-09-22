//! `ra_core::sandbox::snapshot`: the three kinds of storage a session can name, and the specs that
//! name one before the session exists.
//!
//! A snapshot is the only thing that survives a stop, so what it says has to survive a JSON round
//! trip too: the type that says which storage holds it, the id that says which stored workspace it
//! is, and the one field that storage needs to find it. The refusals matter as much as the values.
//! A stored `local` payload with no directory in it names a file nobody can find, and a resume that
//! accepted it would come up with an empty workspace and report success.

use ra_core::sandbox::{
    DiscriminatedPayload, LOCAL_SNAPSHOT_TYPE, Manifest, NOOP_SNAPSHOT_TYPE, REMOTE_SNAPSHOT_TYPE,
    SandboxSessionState, Snapshot, SnapshotSource, SnapshotSpec, builtin_snapshot_registry,
    resolve_snapshot,
};
use serde_json::json;

#[test]
fn a_local_snapshot_carries_the_directory_its_archive_lives_in() {
    let snapshot = Snapshot::local("snap-1", "/tmp/snapshots").expect("named");

    assert_eq!(snapshot.snapshot_type(), LOCAL_SNAPSHOT_TYPE);
    assert_eq!(snapshot.id(), "snap-1");
    assert_eq!(
        snapshot.local_base_path(),
        Some(std::path::PathBuf::from("/tmp/snapshots"))
    );
    assert_eq!(
        serde_json::to_value(snapshot).expect("render"),
        json!({"type": "local", "id": "snap-1", "base_path": "/tmp/snapshots"})
    );
}

#[test]
fn a_remote_snapshot_carries_the_dependency_its_client_is_bound_under() {
    let snapshot = Snapshot::remote("snap-1", "tests.remote_snapshot_client");

    assert_eq!(snapshot.snapshot_type(), REMOTE_SNAPSHOT_TYPE);
    assert_eq!(
        snapshot.remote_client_dependency_key(),
        Some("tests.remote_snapshot_client")
    );
    assert_eq!(
        serde_json::to_value(snapshot).expect("render"),
        json!({
            "type": "remote",
            "id": "snap-1",
            "client_dependency_key": "tests.remote_snapshot_client",
        })
    );
}

#[test]
fn a_field_reader_answers_only_for_the_kind_it_belongs_to() {
    // The payload is open, so a snapshot of one kind can be carrying a field named after another.
    // Reading it as though the kind agreed would hand a remote key to something about to open a
    // file with it.
    let remote = Snapshot::remote("snap-1", "tests.client").with_field("base_path", "/tmp/x");

    assert_eq!(remote.local_base_path(), None);
    assert_eq!(Snapshot::noop().remote_client_dependency_key(), None);
    assert_eq!(
        Snapshot::local("snap-1", "/tmp/snapshots").expect("named").remote_client_dependency_key(),
        None
    );
}

#[test]
fn stored_snapshots_that_name_no_storage_are_refused() {
    let registry = builtin_snapshot_registry();
    let refused = [
        json!({"type": "local", "id": "snap-1"}),
        json!({"type": "local", "id": "snap-1", "base_path": 7}),
        json!({"type": "local", "id": "snap-1", "base_path": null}),
        json!({"type": "remote", "id": "snap-1"}),
        json!({"type": "remote", "id": "snap-1", "client_dependency_key": ["a"]}),
        // A host's own storage is not one of the three, and is refused until that host registers it.
        json!({"type": "custom", "id": "snap-1"}),
    ];

    for payload in refused {
        assert!(
            Snapshot::parse(&registry, &payload).is_err(),
            "expected a refusal for {payload}"
        );
    }

    // The one that stores nothing has nothing to name, and needs no field.
    assert!(Snapshot::parse(&registry, &json!({"type": "noop", "id": "snap-1"})).is_ok());
    // Present and a string is the whole requirement, as it is on the reference's model. An empty
    // directory means the process's own working directory there, and it means that here too.
    assert!(Snapshot::parse(&registry, &json!({"type": "local", "id": "s", "base_path": ""})).is_ok());
}

#[test]
fn a_snapshot_survives_the_trip_through_a_session_state() {
    let registry = builtin_snapshot_registry();
    let state = SandboxSessionState::new(
        "stub",
        Snapshot::local("snap-1", "/tmp/snapshots").expect("named"),
        Manifest::new(),
    );

    let rendered = state.to_json().expect("persistable");
    let snapshot = Snapshot::parse(&registry, &rendered["snapshot"]).expect("restore");

    assert_eq!(snapshot, Snapshot::local("snap-1", "/tmp/snapshots").expect("named"));

    let payload: DiscriminatedPayload = serde_json::from_value(rendered).expect("payload");
    let back =
        SandboxSessionState::from_payload(&payload, snapshot, Manifest::new()).expect("rebuild");

    assert_eq!(back, state);
}

#[test]
fn a_spec_names_its_snapshot_after_the_session() {
    assert_eq!(
        SnapshotSpec::Local {
            base_path: "/tmp/snapshots".into(),
        }
        .build("snap-1")
        .expect("named"),
        Snapshot::local("snap-1", "/tmp/snapshots").expect("named")
    );
    assert_eq!(
        SnapshotSpec::Remote {
            client_dependency_key: "tests.client".to_owned(),
        }
        .build("snap-1")
        .expect("named"),
        Snapshot::remote("snap-1", "tests.client")
    );

    // The snapshot that stores nothing still takes the id: a session that is given real storage
    // later should not also change what it is called.
    let noop = SnapshotSpec::Noop.build("snap-1").expect("named");
    assert!(noop.is_noop());
    assert_eq!(noop.id(), "snap-1");
}

#[cfg(unix)]
#[test]
fn two_directories_that_differ_only_in_unreadable_bytes_cannot_name_the_same_storage() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // Rendering these two lossily gives the same string — both invalid bytes become U+FFFD — so a
    // snapshot built from either would be stored in one place. The second session's archive would
    // overwrite the first's, and the first would resume from someone else's workspace.
    let one = std::path::PathBuf::from(OsStr::from_bytes(b"/tmp/snapshots-\xff"));
    let other = std::path::PathBuf::from(OsStr::from_bytes(b"/tmp/snapshots-\xfe"));
    assert_eq!(one.to_string_lossy(), other.to_string_lossy());

    for base_path in [one, other] {
        let refused = Snapshot::local("snap-1", &base_path).expect_err("refused");
        assert!(refused.to_string().contains("valid UTF-8"), "{refused}");

        // And the same refusal wherever a spec is turned into a snapshot, since the field is public
        // and a caller can put any path in it.
        let spec = SnapshotSpec::Local {
            base_path: base_path.clone(),
        };
        assert_eq!(spec.build("snap-1"), Err(refused.clone()));
        let source = SnapshotSource::from(spec);
        assert_eq!(resolve_snapshot(Some(&source), "snap-1"), Err(refused));
    }
}

#[test]
fn a_spec_that_was_written_down_reads_back() {
    let specs = [
        (
            SnapshotSpec::Local {
                base_path: "/tmp/snapshots".into(),
            },
            json!({"type": "local", "base_path": "/tmp/snapshots"}),
        ),
        (SnapshotSpec::Noop, json!({"type": "noop"})),
        (
            SnapshotSpec::Remote {
                client_dependency_key: "tests.client".to_owned(),
            },
            json!({"type": "remote", "client_dependency_key": "tests.client"}),
        ),
    ];

    for (spec, rendered) in specs {
        assert_eq!(serde_json::to_value(&spec).expect("render"), rendered);
        assert_eq!(
            serde_json::from_value::<SnapshotSpec>(rendered).expect("read back"),
            spec
        );
    }
}

#[test]
fn a_snapshot_handed_in_keeps_the_id_it_came_with() {
    // The caller named a specific stored workspace. Renaming it after this session would persist
    // the workspace somewhere nobody asked for, and restore from somewhere that holds nothing.
    let stored = Snapshot::local("last-weeks-run", "/tmp/snapshots").expect("named");
    let source = SnapshotSource::Snapshot(stored.clone());

    assert_eq!(resolve_snapshot(Some(&source), "snap-1").expect("named"), stored);
}

#[test]
fn asking_for_no_snapshot_still_names_one() {
    let resolved = resolve_snapshot(None, "snap-1").expect("named");

    assert_eq!(resolved.snapshot_type(), NOOP_SNAPSHOT_TYPE);
    assert_eq!(resolved.id(), "snap-1");
}

#[test]
fn a_spec_is_resolved_against_the_session_it_is_for() {
    let source = SnapshotSource::from(SnapshotSpec::Local {
        base_path: "/tmp/snapshots".into(),
    });

    assert_eq!(
        resolve_snapshot(Some(&source), "snap-1").expect("named"),
        Snapshot::local("snap-1", "/tmp/snapshots").expect("named")
    );
}
