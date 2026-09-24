//! `ra-core::sandbox::state`: what a session needs in order to be resumed.
//!
//! The behavior pinned here is what a resume depends on:
//! - exposed ports normalize the way the reference normalizes them, including which values it
//!   refuses and why port 0 is among them
//! - a fingerprint and the scheme that produced it travel together or not at all
//! - fields a host does not model survive the round trip, so a state written by one backend still
//!   describes the same sandbox when another host reads it
//! - a modelled field cannot also live among the backend-specific ones, where the two could
//!   disagree
//! - a persisted state carries no host path source, gets its grants back only from a trusted
//!   manifest, and a payload that fails to read is never quoted back
//!
//! Mount authority in persisted states is pinned in `sandbox_mount_security.rs`.
//!
//! What a manifest declares is pinned in `sandbox_manifest.rs`.

use async_trait::async_trait;
use ra_core::sandbox::{
    CreateRequest, DiscriminatedPayload, Entry, Environment, ErrorCode, InvalidSessionStatePayload,
    Manifest, ManifestRegistries, REDACTED_HOST_PATH_GRANT_PATHS_KEY, REDACTED_MOUNT_AUTHORITY_KEY,
    SandboxClient, SandboxPathGrant, SandboxResult, SandboxSession, SandboxSessionState, Snapshot,
    builtin_snapshot_registry, normalize_exposed_ports,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn state() -> SandboxSessionState {
    SandboxSessionState::new("stub", Snapshot::new("local", "snap-1"), Manifest::new())
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

    // And forgetting one is a decision, not an omission: a persist that could not hash the
    // workspace has to clear what an earlier persist recorded, or the next resume compares a new
    // workspace against an old workspace's hash.
    let forgotten = state.without_snapshot_fingerprint();
    assert_eq!(forgotten.snapshot_fingerprint(), None);
    let rendered = forgotten.to_json().expect("persistable");
    assert!(rendered.get("snapshot_fingerprint").is_none());
    assert!(rendered.get("snapshot_fingerprint_version").is_none());
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
fn an_entry_forging_a_mount_type_is_refused_rather_than_persisted() {
    // Something that only names a built-in mount type would have its fields read by rules written
    // for a shape it does not have, so it cannot cross the credential boundary at all.
    let forged = SandboxSessionState::new(
        "stub",
        Snapshot::new("local", "snap-1"),
        Manifest::new().with_entry(
            "data",
            Entry::new(ra_core::sandbox::EntryContent::Extension(
                DiscriminatedPayload::new("s3_mount")
                    .with_field("secret_access_key", "AKIAsecret")
                    .with_field("session_token", "tok"),
            )),
        ),
    );

    let error = forged.to_json().expect_err("refuse");
    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(
        error.message().contains("custom mount implementations"),
        "{error}"
    );

    // The refusal has to bite on the serde path too, which is the one a host actually persists
    // through.
    let message = serde_json::to_string(&forged)
        .expect_err("serde must refuse as well")
        .to_string();
    assert!(!message.contains("AKIAsecret"), "leaked: {message}");
    assert!(!message.contains("tok"), "leaked: {message}");
}

#[test]
fn the_environment_and_host_sources_are_written_by_the_state_itself() {
    // The state renders them as they are, as the reference's state does. Dropping host sources is
    // the client's job when it writes a state for storage, and reading one back drops any that got
    // through and marks them for a rebind.
    let mut manifest = Manifest::new().with_path_grant(
        SandboxPathGrant::new("/w")
            .expect("absolute")
            .with_host_path("/home/user/.aws")
            .expect("absolute host source"),
    );
    manifest.environment = Environment::new().with("AWS_REGION", "us-east-1");
    let state = SandboxSessionState::new("stub", Snapshot::noop(), manifest);

    let rendered = state.to_json().expect("persistable");

    assert_eq!(
        rendered["manifest"]["environment"]["value"]["AWS_REGION"],
        json!("us-east-1")
    );
    assert_eq!(
        rendered["manifest"]["extra_path_grants"][0]["host_path"],
        json!("/home/user/.aws")
    );
    assert!(rendered.get(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
}

// --- reading a persisted state ---------------------------------------------------------------
//
// Ported from the reference's `tests/sandbox/test_session_state_roundtrip.py`.

/// A client for the `stub` backend, used only for its state round trip.
struct StubClient;

#[async_trait]
impl SandboxClient for StubClient {
    fn backend_id(&self) -> &str {
        "stub"
    }

    async fn create(&self, _request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        panic!("create is not used by these tests")
    }

    async fn resume(&self, _state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        panic!("resume is not used by these tests")
    }

    async fn delete(&self, _session: &dyn SandboxSession) -> SandboxResult<()> {
        panic!("delete is not used by these tests")
    }
}

fn parse(payload: Value) -> Result<SandboxSessionState, InvalidSessionStatePayload> {
    SandboxSessionState::parse(
        payload,
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
}

fn deserialize(payload: Value) -> SandboxResult<SandboxSessionState> {
    StubClient.deserialize_session_state(
        payload,
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
}

fn host_backed_grant() -> SandboxPathGrant {
    SandboxPathGrant::new("/mnt/shared-data")
        .expect("absolute")
        .with_host_path("/srv/shared-data")
        .expect("absolute host source")
        .read_only(true)
}

#[test]
fn parse_rejects_invalid_payloads() {
    assert_eq!(
        parse(json!({})).expect_err("refuse"),
        InvalidSessionStatePayload::Invalid
    );
    assert_eq!(
        parse(json!({"type": "missing"})).expect_err("refuse"),
        InvalidSessionStatePayload::Invalid
    );
    let error = parse(json!("not-a-state")).expect_err("refuse");
    assert_eq!(error, InvalidSessionStatePayload::NotAnObject);
    assert!(error.to_string().contains("session state payload must be"));
}

#[test]
fn parse_redacts_malformed_payload_errors() {
    let sentinel = "session-state-parse-secret";
    for payload in [
        json!({"type": sentinel}),
        json!({
            "type": "stub",
            "snapshot": {"type": "noop", "id": "snapshot"},
            "manifest": {"entries": {"data": {"type": "unknown", "token": sentinel}}},
        }),
        json!({
            "type": "stub",
            "session_id": [],
            "snapshot": {"type": "noop", "id": "snapshot"},
            "manifest": {"entries": {"data": {
                "type": "s3_mount",
                "bucket": "bucket",
                "secret_access_key": {"secret": sentinel},
                "mount_strategy": {"type": "docker_volume", "driver": "rclone"},
            }}},
        }),
        json!({
            "type": "stub",
            "snapshot": {"type": "noop", "id": "snapshot"},
            "manifest": [sentinel],
        }),
    ] {
        let error = parse(payload).expect_err("refuse");

        assert_eq!(
            error.to_string(),
            "sandbox session state payload is invalid"
        );
        assert!(!format!("{error:?}").contains(sentinel));
    }
}

#[test]
fn client_serialization_redacts_host_paths_and_rebinds_from_trusted_manifest() {
    let trusted = Manifest::new().with_path_grant(host_backed_grant());
    let state = SandboxSessionState::new("stub", Snapshot::noop(), trusted.clone());

    let payload = StubClient
        .serialize_session_state(&state)
        .expect("serialize");

    assert!(!payload.to_string().contains("/srv/shared-data"));
    assert_eq!(
        payload[REDACTED_HOST_PATH_GRANT_PATHS_KEY],
        json!(["/mnt/shared-data"])
    );
    // An empty collection is left out of a rendered manifest.
    assert!(payload["manifest"].get("extra_path_grants").is_none());

    let restored = deserialize(payload).expect("deserialize");
    assert!(restored.manifest().extra_path_grants.is_empty());
    assert_eq!(restored.path_grants_require_rebind(), ["/mnt/shared-data"]);
    let error = restored.assert_path_grants_rebound().expect_err("refuse");
    assert!(error.message().contains("must be rebound"), "{error}");

    let rebound = restored
        .rebind_persisted_path_grants(Some(&trusted))
        .expect("rebind");
    assert_eq!(
        rebound.manifest().extra_path_grants,
        trusted.extra_path_grants
    );
    assert!(rebound.path_grants_require_rebind().is_empty());
    assert!(restored.manifest().extra_path_grants.is_empty());
    rebound.assert_path_grants_rebound().expect("resumable");
}

#[test]
fn a_rebind_needs_a_trusted_host_source_for_every_dropped_path() {
    let state = SandboxSessionState::new(
        "stub",
        Snapshot::noop(),
        Manifest::new().with_path_grant(host_backed_grant()),
    );
    let restored = deserialize(
        StubClient
            .serialize_session_state(&state)
            .expect("serialize"),
    )
    .expect("deserialize");

    let error = restored
        .rebind_persisted_path_grants(None)
        .expect_err("refuse");
    assert!(
        error
            .message()
            .contains("require a current trusted manifest"),
        "{error}"
    );

    let path_only = Manifest::new()
        .with_path_grant(SandboxPathGrant::new("/mnt/shared-data").expect("absolute"));
    let error = restored
        .rebind_persisted_path_grants(Some(&path_only))
        .expect_err("refuse");
    assert!(
        error.message().ends_with("path grants: /mnt/shared-data"),
        "{error}"
    );
}

#[test]
fn path_only_grants_preserve_a_direct_round_trip() {
    let manifest = Manifest::new()
        .with_path_grant(
            SandboxPathGrant::new("/mnt/shared-data")
                .expect("absolute")
                .read_only(true),
        )
        .with_path_grant(SandboxPathGrant::new("/mnt/shared-data").expect("absolute"));
    let state = SandboxSessionState::new("stub", Snapshot::noop(), manifest.clone());

    let restored = deserialize(
        StubClient
            .serialize_session_state(&state)
            .expect("serialize"),
    )
    .expect("deserialize");

    assert!(restored.path_grants_require_rebind().is_empty());
    restored.assert_path_grants_rebound().expect("resumable");
    assert_eq!(
        restored.manifest().extra_path_grants,
        manifest.extra_path_grants
    );
}

#[test]
fn a_removed_redaction_marker_does_not_restore_a_host_backed_grant() {
    let state = SandboxSessionState::new(
        "stub",
        Snapshot::noop(),
        Manifest::new().with_path_grant(host_backed_grant()),
    );
    let mut payload = StubClient
        .serialize_session_state(&state)
        .expect("serialize");
    payload
        .as_object_mut()
        .expect("object")
        .remove(REDACTED_HOST_PATH_GRANT_PATHS_KEY);

    let restored = deserialize(payload).expect("deserialize");

    // Forgetting that a grant was wanted is all a payload can do; it cannot bring the source back.
    assert!(restored.path_grants_require_rebind().is_empty());
    assert!(restored.manifest().extra_path_grants.is_empty());
    restored
        .assert_path_grants_rebound()
        .expect("nothing left to rebind");
}

#[test]
fn deserialization_discards_an_unmarked_serialized_host_path() {
    let trusted = Manifest::new().with_path_grant(host_backed_grant());
    let state = SandboxSessionState::new("stub", Snapshot::noop(), trusted.clone());
    // Written by the state itself, which leaves host sources in.
    let payload = state.to_json().expect("render");

    let restored = deserialize(payload).expect("deserialize");

    assert!(restored.manifest().extra_path_grants.is_empty());
    assert_eq!(restored.path_grants_require_rebind(), ["/mnt/shared-data"]);
    let error = restored.assert_path_grants_rebound().expect_err("refuse");
    assert!(error.message().contains("must be rebound"), "{error}");

    let rebound = restored
        .rebind_persisted_path_grants(Some(&trusted))
        .expect("rebind");
    rebound.assert_path_grants_rebound().expect("resumable");
    assert_eq!(
        rebound.manifest().extra_path_grants,
        trusted.extra_path_grants
    );
}

#[test]
fn authority_markers_are_flags_rather_than_backend_fields() {
    let state = SandboxSessionState::new("stub", Snapshot::noop(), Manifest::new())
        .with_field(REDACTED_MOUNT_AUTHORITY_KEY, true)
        .with_field(REDACTED_HOST_PATH_GRANT_PATHS_KEY, json!(["/x"]));

    // A caller cannot set one by hand...
    assert!(state.field(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
    assert!(
        state
            .to_json()
            .expect("render")
            .get(REDACTED_MOUNT_AUTHORITY_KEY)
            .is_none()
    );

    // ...and one read from a payload becomes the flag, not a field carried along.
    let mut payload = state.to_json().expect("render");
    payload[REDACTED_MOUNT_AUTHORITY_KEY] = json!(true);
    let restored = parse(payload).expect("parse");
    assert!(restored.mount_authority_redacted());
    assert!(restored.field(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
}

#[test]
fn parse_reads_legacy_discriminator_free_str_env_values() {
    let mut payload = SandboxSessionState::new("stub", Snapshot::noop(), Manifest::new())
        .to_json()
        .expect("render");
    payload["manifest"]["environment"] = json!({"value": {
        "DIRECT": {"value": "direct-value"},
        "ENTRY": {
            "description": "typed entry",
            "ephemeral": true,
            "value": {"value": "entry-value"},
        },
    }});

    let restored = parse(payload).expect("parse");

    assert_eq!(
        restored.manifest().environment.to_json()["value"]["DIRECT"],
        json!({"type": "str", "value": "direct-value"})
    );
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
