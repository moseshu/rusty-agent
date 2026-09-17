//! `ra-core::sandbox::{manifest, state}`: what a session declares its workspace should contain, and
//! what it needs in order to be resumed.
//!
//! The behavior pinned here is what a resume depends on:
//! - exposed ports normalize the way the reference normalizes them, including which values it
//!   refuses and why port 0 is among them
//! - a fingerprint and the scheme that produced it travel together or not at all
//! - fields a host does not model survive the round trip, so a state written by one backend still
//!   describes the same sandbox when another host reads it
//! - a modelled field cannot also live among the backend-specific ones, where the two could
//!   disagree

use ra_core::sandbox::{
    DEFAULT_MANIFEST_ROOT, DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST, DiscriminatedPayload, Group,
    MANIFEST_VERSION, Manifest, SandboxSessionState, Snapshot, User, normalize_exposed_ports,
};
use serde_json::json;
use uuid::Uuid;

fn state() -> SandboxSessionState {
    SandboxSessionState::new("stub", Snapshot::new("local", "snap-1"), Manifest::new())
}

// --- manifest -----------------------------------------------------------------------------

#[test]
fn a_fresh_manifest_carries_the_reference_defaults() {
    let manifest = Manifest::new();

    assert_eq!(manifest.version, MANIFEST_VERSION);
    assert_eq!(manifest.root, DEFAULT_MANIFEST_ROOT);
    assert_eq!(manifest.root, "/workspace");
    assert!(manifest.entries.is_empty());
    assert!(manifest.users.is_empty());
    assert!(manifest.groups.is_empty());
    assert!(!manifest.grants_extra_paths());
}

#[test]
fn the_remote_mount_allowlist_reads_and_moves_but_does_not_fetch_or_execute() {
    let allowlist = &DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST;

    assert_eq!(allowlist.len(), 18);
    for expected in ["ls", "find", "stat", "cat", "grep", "cp", "mkdir", "rm"] {
        assert!(allowlist.contains(&expected), "{expected} must be allowed");
    }
    // Nothing that reaches the network or runs a program: widening this is a backend's own
    // manifest to declare, not a default anybody inherits.
    for refused in ["curl", "wget", "sh", "bash", "chmod", "sudo", "ssh"] {
        assert!(
            !allowlist.contains(&refused),
            "{refused} must not be allowed by default"
        );
    }
}

#[test]
fn a_manifest_round_trips_including_the_parts_not_yet_modelled() {
    // Entries and grants are carried as written until materialization is ported. A host that does
    // not model them must still hand back a manifest that describes the same workspace.
    let manifest = Manifest::new()
        .with_root("/srv/work")
        .with_user(User::new("agent"))
        .with_group(Group::new("staff", vec![User::new("agent")]))
        .with_entry("README.md", json!({"type": "local_file", "path": "/tmp/r"}));

    let rendered = serde_json::to_value(&manifest).expect("serialize");
    let back: Manifest = serde_json::from_value(rendered.clone()).expect("deserialize");

    assert_eq!(back, manifest);
    assert_eq!(back.root, "/srv/work");
    assert_eq!(
        rendered["entries"]["README.md"],
        json!({"type": "local_file", "path": "/tmp/r"})
    );
}

#[test]
fn a_manifest_reads_back_with_every_field_omitted() {
    // `{}` is a valid manifest: an empty workspace at the default root. Making any field required
    // would reject configuration the reference accepts.
    let empty: Manifest = serde_json::from_value(json!({})).expect("empty manifest");
    assert_eq!(empty, Manifest::new());

    let partial: Manifest =
        serde_json::from_value(json!({"version": 1, "root": "/workspace"})).expect("partial");
    assert_eq!(partial, Manifest::new());

    let rooted: Manifest = serde_json::from_value(json!({"root": "/srv"})).expect("root only");
    assert_eq!(rooted.version, MANIFEST_VERSION);
    assert_eq!(rooted.root, "/srv");
    assert_eq!(
        rooted.remote_mount_command_allowlist.len(),
        DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST.len()
    );
}

#[test]
fn a_manifest_version_this_does_not_understand_is_refused() {
    // Reading a later format with rules written for this one would materialize a workspace from
    // entries that no longer mean what they used to.
    for unsupported in [json!({"version": 2}), json!({"version": 0})] {
        let error = serde_json::from_value::<Manifest>(unsupported.clone()).expect_err("refuse");
        assert!(
            error.to_string().contains("unsupported manifest version"),
            "{unsupported} must be refused, got {error}"
        );
    }
}

// --- exposed ports ------------------------------------------------------------------------

#[test]
fn exposed_ports_normalize_the_way_the_reference_normalizes_them() {
    assert_eq!(
        normalize_exposed_ports(&json!(null)).expect("none"),
        Vec::<u16>::new()
    );
    assert_eq!(
        normalize_exposed_ports(&json!(8080)).expect("bare number"),
        vec![8080]
    );
    // Duplicates drop and the first appearance sets the order, so the rendering is stable.
    assert_eq!(
        normalize_exposed_ports(&json!([8080, 9000, 8080])).expect("sequence"),
        vec![8080, 9000]
    );
}

#[test]
fn exposed_ports_refuse_what_the_reference_refuses() {
    // A string is not a sequence of ports, however much it looks like one.
    assert_eq!(
        normalize_exposed_ports(&json!("8080"))
            .expect_err("refuse")
            .to_string(),
        "exposed_ports must be an iterable of TCP port integers"
    );
    assert_eq!(
        normalize_exposed_ports(&json!([8080, "9000"]))
            .expect_err("refuse")
            .to_string(),
        "exposed_ports must contain integers"
    );
}

#[test]
fn an_out_of_range_port_is_told_apart_from_a_non_integer() {
    // Whole numbers are integers however far outside the TCP range they fall, so a negative port
    // and a port past 65535 both fail the range check. Reporting either as "not an integer" would
    // send whoever wrote the configuration looking for a type mistake they did not make.
    for out_of_range in [
        json!([0]),
        json!([65536]),
        json!([-1]),
        json!([1_099_511_627_776_i64]),
        json!([u64::MAX]),
    ] {
        assert_eq!(
            normalize_exposed_ports(&out_of_range)
                .expect_err("refuse")
                .to_string(),
            "exposed_ports entries must be between 1 and 65535",
            "{out_of_range} must be out of range"
        );
    }

    // A float is not an integer, however round it looks.
    assert_eq!(
        normalize_exposed_ports(&json!([8080.5]))
            .expect_err("refuse")
            .to_string(),
        "exposed_ports must contain integers"
    );
}

#[test]
fn a_boolean_counts_as_an_integer_the_way_the_reference_counts_it() {
    // The reference tests membership with `isinstance(port, int)`, and Python's booleans are
    // integers, so `true` is port 1 and `false` is 0 and therefore out of range. It is a wart, but
    // a port set that one implementation accepts and the other refuses turns a portable
    // configuration into one that depends on which runtime read it.
    assert_eq!(
        normalize_exposed_ports(&json!(true)).expect("bare boolean"),
        vec![1]
    );
    assert_eq!(
        normalize_exposed_ports(&json!([true, 8080])).expect("in a sequence"),
        vec![1, 8080]
    );
    assert_eq!(
        normalize_exposed_ports(&json!([false]))
            .expect_err("refuse")
            .to_string(),
        "exposed_ports entries must be between 1 and 65535"
    );
}

#[test]
fn a_state_refuses_port_zero_from_typed_callers_too() {
    assert!(state().with_exposed_ports([0]).is_err());
    assert_eq!(
        state()
            .with_exposed_ports([8080, 9000, 8080])
            .expect("normalize")
            .exposed_ports(),
        [8080, 9000]
    );
}

// --- state --------------------------------------------------------------------------------

#[test]
fn a_fresh_state_starts_unproven() {
    let state = state();

    assert_eq!(state.state_type(), "stub");
    assert!(!state.workspace_root_ready());
    assert!(state.exposed_ports().is_empty());
    assert_eq!(state.snapshot_fingerprint(), None);
}

#[test]
fn a_fingerprint_and_its_scheme_travel_together_or_not_at_all() {
    // A fingerprint compared under the wrong scheme can report a match that is not one, and the
    // session would then skip restoring a snapshot it needed.
    let state = state().with_snapshot_fingerprint("abc123", "v2");

    assert_eq!(state.snapshot_fingerprint(), Some(("abc123", "v2")));
}

#[test]
fn a_state_renders_with_its_discriminator_and_reads_back() {
    let id = Uuid::new_v4();
    let original = state()
        .with_session_id(id)
        .with_workspace_root_ready(true)
        .with_exposed_ports([8080])
        .expect("ports")
        .with_snapshot_fingerprint("abc123", "v2")
        .with_field("container_id", "c-1");

    let rendered = original.to_json().expect("persistable");
    assert_eq!(rendered["type"], json!("stub"));
    assert_eq!(rendered["session_id"], json!(id.to_string()));
    assert_eq!(rendered["exposed_ports"], json!([8080]));
    assert_eq!(rendered["workspace_root_ready"], json!(true));
    assert_eq!(rendered["container_id"], json!("c-1"));

    let payload: DiscriminatedPayload = serde_json::from_value(rendered).expect("payload");
    let back = SandboxSessionState::from_payload(
        &payload,
        Snapshot::new("local", "snap-1"),
        Manifest::new(),
    )
    .expect("rebuild");

    assert_eq!(back, original);
    assert_eq!(back.session_id(), id);
    assert_eq!(back.field("container_id"), Some(&json!("c-1")));
}

#[test]
fn fields_a_host_does_not_model_survive_the_trip() {
    let original = state().with_field("future", json!({"nested": [1, 2]}));

    let payload: DiscriminatedPayload =
        serde_json::from_value(original.to_json().expect("persistable")).expect("payload");
    let back = SandboxSessionState::from_payload(
        &payload,
        Snapshot::new("local", "snap-1"),
        Manifest::new(),
    )
    .expect("rebuild");

    assert_eq!(back.field("future"), Some(&json!({"nested": [1, 2]})));
}

#[test]
fn a_modelled_field_cannot_also_be_set_as_a_backend_field() {
    // Two homes for one value means the renderer has to pick a winner, and whichever it picks is
    // wrong half the time.
    let state = state()
        .with_field("session_id", "not-a-uuid")
        .with_field("exposed_ports", json!([1, 2, 3]))
        .with_field("workspace_root_ready", true);

    assert_eq!(state.field("session_id"), None);
    assert_eq!(state.field("exposed_ports"), None);
    assert_eq!(state.field("workspace_root_ready"), None);
    assert!(state.exposed_ports().is_empty());
    assert!(!state.workspace_root_ready());
}

#[test]
fn every_modelled_field_is_kept_out_of_the_backend_fields_on_the_way_back() {
    // A round trip is where a field can quietly acquire a second home: the renderer writes it, and
    // a rebuild that does not recognize it as modelled files it under the backend's own fields.
    let original = state()
        .with_workspace_root_ready(true)
        .with_exposed_ports([8080])
        .expect("ports")
        .with_snapshot_fingerprint("abc123", "v2");

    let payload: DiscriminatedPayload =
        serde_json::from_value(original.to_json().expect("persistable")).expect("payload");
    let back = SandboxSessionState::from_payload(
        &payload,
        Snapshot::new("local", "snap-1"),
        Manifest::new(),
    )
    .expect("rebuild");

    for modelled in [
        "session_id",
        "snapshot",
        "manifest",
        "exposed_ports",
        "snapshot_fingerprint",
        "snapshot_fingerprint_version",
        "workspace_root_ready",
    ] {
        assert_eq!(
            back.field(modelled),
            None,
            "{modelled} must not also live among the backend fields"
        );
    }
}

#[test]
fn a_malformed_modelled_field_is_refused_rather_than_ignored() {
    for (key, value, fragment) in [
        ("session_id", json!("not-a-uuid"), "must be a UUID"),
        ("session_id", json!(7), "must be a string"),
        ("workspace_root_ready", json!("yes"), "must be a boolean"),
        ("snapshot_fingerprint", json!(7), "must be a string"),
    ] {
        let payload = DiscriminatedPayload::new("stub").with_field(key, value.clone());
        let error = SandboxSessionState::from_payload(
            &payload,
            Snapshot::new("local", "snap-1"),
            Manifest::new(),
        )
        .expect_err("refuse");
        assert!(
            error.to_string().contains(fragment),
            "{key}={value} should mention {fragment}, got {error}"
        );
    }
}

#[test]
fn a_state_without_a_session_id_is_given_one() {
    // A payload that never carried an id still describes a sandbox; minting one here is what lets
    // a backend write a state before it has a name for the session.
    let payload = DiscriminatedPayload::new("stub");

    let built = SandboxSessionState::from_payload(
        &payload,
        Snapshot::new("local", "snap-1"),
        Manifest::new(),
    )
    .expect("rebuild");

    assert!(!built.session_id().is_nil());
}

#[test]
fn a_state_carrying_mount_authority_refuses_to_be_persisted() {
    // An S3 mount's secret key lives in an entry, and a grant naming a host path has to be dropped
    // and recorded as needing rebinding rather than written out. The reference does that while
    // serializing; leaving it for the read side would put the credentials on disk first and
    // sanitize them afterwards.
    let with_secret = SandboxSessionState::new(
        "stub",
        Snapshot::new("local", "snap-1"),
        Manifest::new().with_entry(
            "data",
            json!({
                "type": "s3_mount",
                "secret_access_key": "AKIAsecret",
                "session_token": "tok",
            }),
        ),
    );

    let error = with_secret.to_json().expect_err("refuse");
    assert_eq!(error.field, "entries");
    assert!(error.to_string().contains("credentials on disk"));

    // The refusal has to bite on the serde path too, which is the one a host actually persists
    // through.
    let rendered = serde_json::to_string(&with_secret);
    assert!(rendered.is_err(), "serde must refuse as well");
    let message = rendered.expect_err("refuse").to_string();
    assert!(!message.contains("AKIAsecret"), "leaked: {message}");
    assert!(!message.contains("tok"), "leaked: {message}");

    // A manifest with nothing unmodelled in it has no authority to redact and persists normally.
    assert!(state().to_json().is_ok());
}

#[test]
fn every_unmodelled_manifest_field_closes_the_persistence_path() {
    let mut environment = Manifest::new();
    environment.environment = Some(json!({"AWS_SECRET_ACCESS_KEY": "s"}));
    let mut grants = Manifest::new();
    grants.extra_path_grants = vec![json!({"path": "/w", "host_path": "/home/user/.aws"})];

    for (manifest, field) in [(environment, "environment"), (grants, "extra_path_grants")] {
        let state = SandboxSessionState::new("stub", Snapshot::noop(), manifest);
        assert_eq!(state.to_json().expect_err("refuse").field, field);
    }
}

#[test]
fn a_session_with_nothing_persisted_still_has_a_snapshot() {
    let state = SandboxSessionState::new("stub", Snapshot::noop(), Manifest::new());

    assert!(state.snapshot().is_noop());
    assert_eq!(
        state.to_json().expect("persistable")["snapshot"]["type"],
        json!("noop")
    );
}
