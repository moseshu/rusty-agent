//! `ra-core::sandbox::registry` and `::snapshot`: the discriminated-payload mechanism, carried over
//! from the reference implementation's three copies of it — `BaseSandboxClientOptions`,
//! `SnapshotBase` and `SandboxSessionState`.
//!
//! Four rules are pinned, because each of them is a way a resume can go quietly wrong:
//! - a type must be non-empty, so nothing registers under a discriminator nobody can name
//! - a type is claimed once, so a payload cannot be rerouted to a different backend
//! - an unknown type is refused rather than defaulted, so work never runs somewhere other than
//!   where the payload said
//! - the discriminator is always serialized, so a round-tripped payload can still be routed
//!
//! The refusal messages are asserted literally: they are the reference's wording, they name the
//! family that refused, and a host reads them to find out which file to look in.

use ra_core::sandbox::{
    DiscriminatedPayload, RegistryError, Snapshot, TypeRegistry, client_options_kind,
    session_state_kind, snapshot_kind,
};
use serde_json::{Value, json};

fn options_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new(client_options_kind());
    registry
        .register("docker", "DockerSandboxClientOptions")
        .expect("register docker");
    registry
        .register("unix_local", "UnixLocalSandboxClientOptions")
        .expect("register unix_local");
    registry
}

// --- the four rules -----------------------------------------------------------------------

#[test]
fn a_payload_is_routed_by_its_discriminator() {
    let registry = options_registry();

    let parsed = registry
        .parse(&json!({"type": "docker", "image": "python:3.14-slim", "exposed_ports": [8080]}))
        .expect("parse");

    assert_eq!(parsed.type_name(), "docker");
    assert_eq!(parsed.field("image"), Some(&json!("python:3.14-slim")));
    assert_eq!(parsed.field("exposed_ports"), Some(&json!([8080])));
}

#[test]
fn an_unknown_type_is_refused_rather_than_defaulted() {
    let registry = options_registry();

    let error = registry
        .parse(&json!({"type": "unknown"}))
        .expect_err("refuse");

    assert_eq!(
        error.to_string(),
        "unknown sandbox client options type `unknown`"
    );
    assert!(matches!(error, RegistryError::UnknownType { .. }));
}

#[test]
fn a_payload_that_is_not_an_object_names_the_base_to_supply() {
    let registry = options_registry();

    let error = registry.parse(&json!("docker")).expect_err("refuse");

    assert_eq!(
        error.to_string(),
        "sandbox client options payload must be a BaseSandboxClientOptions or object payload"
    );
}

#[test]
fn a_missing_type_is_reported_as_an_unknown_one() {
    // The reference funnels "no type" and "unroutable type" into the same refusal, so a caller
    // branching on the error sees one case rather than two. Python renders the absent value as
    // `None`; JSON renders it as `null`, which is the only difference.
    let registry = options_registry();

    for (payload, rendered) in [
        (json!({}), "null"),
        (json!({"type": null}), "null"),
        (json!({"type": 7}), "7"),
    ] {
        let error = registry.parse(&payload).expect_err("refuse");
        assert_eq!(
            error.to_string(),
            format!("unknown sandbox client options type `{rendered}`")
        );
        assert!(matches!(error, RegistryError::UnknownType { .. }));
    }
}

#[test]
fn a_redacted_family_says_the_type_was_missing_without_quoting_it() {
    // Session state cannot quote the value, so it is the one family that reports a missing
    // discriminator separately.
    let states = TypeRegistry::new(session_state_kind());

    let error = states.parse(&json!({})).expect_err("refuse");

    assert_eq!(
        error.to_string(),
        "sandbox session state payload must include a string `type`"
    );
}

#[test]
fn a_type_can_be_claimed_only_once() {
    let mut registry = options_registry();

    let error = registry
        .register("docker", "ImpostorSandboxClientOptions")
        .expect_err("refuse");

    assert_eq!(
        error.to_string(),
        "sandbox client options type `docker` is already registered by DockerSandboxClientOptions"
    );
}

#[test]
fn re_registering_the_same_owner_is_not_a_conflict() {
    // Registering an assembly twice is a host's own doing and changes nothing; a *different* owner
    // taking a live type is what silently reroutes payloads.
    let mut registry = options_registry();

    registry
        .register("docker", "DockerSandboxClientOptions")
        .expect("idempotent");

    assert!(registry.is_registered("docker"));
}

#[test]
fn an_empty_discriminator_cannot_be_registered() {
    let mut registry = TypeRegistry::new(client_options_kind());

    let error = registry
        .register("", "NamelessSandboxClientOptions")
        .expect_err("refuse");

    assert_eq!(
        error.to_string(),
        "NamelessSandboxClientOptions must define a non-empty string default for `type`"
    );
}

#[test]
fn the_discriminator_is_always_serialized() {
    // The reference emits `type` even when nothing else was set, because a payload that lost it
    // cannot be routed back to anything.
    let payload = DiscriminatedPayload::new("unix_local");

    assert_eq!(payload.to_json(), json!({"type": "unix_local"}));
}

// --- round trips --------------------------------------------------------------------------

#[test]
fn a_round_trip_preserves_the_concrete_type_and_its_fields() {
    let registry = options_registry();
    let original = json!({
        "type": "docker",
        "image": "python:3.14-slim",
        "labels": {"com.example.owner": "worker-123"},
        "exposed_ports": [8080],
    });

    let parsed = registry.parse(&original).expect("parse");
    let rendered = parsed.to_json();
    let reparsed = registry.parse(&rendered).expect("reparse");

    assert_eq!(rendered, original);
    assert_eq!(reparsed, parsed);
    assert_eq!(
        parsed.field("labels"),
        Some(&json!({"com.example.owner": "worker-123"}))
    );
}

#[test]
fn fields_a_host_does_not_recognize_survive_the_trip() {
    // A state written where a backend was registered can be read where its fields mean nothing.
    // Dropping them would hand back a payload that no longer describes anything.
    let registry = options_registry();
    let original = json!({"type": "docker", "future_field": {"nested": [1, 2, 3]}});

    let rendered = registry.parse(&original).expect("parse").to_json();

    assert_eq!(rendered, original);
}

#[test]
fn readmitting_a_payload_checks_the_type_still_exists_here() {
    // A payload may reach a host where its backend is not assembled.
    let parsed = options_registry()
        .parse(&json!({"type": "docker"}))
        .expect("parse");

    let elsewhere = TypeRegistry::new(client_options_kind());
    let error = elsewhere.readmit(parsed.clone()).expect_err("refuse");
    assert_eq!(
        error.to_string(),
        "unknown sandbox client options type `docker`"
    );

    assert_eq!(
        options_registry().readmit(parsed.clone()).expect("readmit"),
        parsed
    );
}

// --- per-family wording -------------------------------------------------------------------

#[test]
fn each_family_refuses_in_its_own_words() {
    // A message that says "snapshot" when a session state was rejected sends someone to the wrong
    // file, so the family noun is part of the contract.
    let snapshots = TypeRegistry::new(snapshot_kind());
    assert_eq!(
        snapshots
            .parse(&json!({"type": "local"}))
            .expect_err("refuse")
            .to_string(),
        "unknown snapshot type `local`"
    );
    assert_eq!(
        snapshots.parse(&json!(3)).expect_err("refuse").to_string(),
        "snapshot payload must be a SnapshotBase or object payload"
    );

    // Session state is the exception, and deliberately so: the reference parses it inside a block
    // that discards the payload and redacts the error, because a state carries mount authority. Its
    // refusal names the family and stops there.
    let states = TypeRegistry::new(session_state_kind());
    let refusal = states
        .parse(&json!({"type": "docker"}))
        .expect_err("refuse");
    assert_eq!(refusal.to_string(), "unknown sandbox session state type");
    assert!(matches!(refusal, RegistryError::UnknownTypeRedacted { .. }));
}

// --- snapshots ----------------------------------------------------------------------------

#[test]
fn the_snapshot_that_stores_nothing_is_a_value_not_an_absence() {
    // The reference makes the field non-optional so "nothing was stored" is something a caller
    // reads rather than a null it has to test for.
    let noop = Snapshot::noop();

    assert!(noop.is_noop());
    assert_eq!(noop.snapshot_type(), "noop");
    assert_eq!(noop.id(), "");
}

#[test]
fn a_snapshot_keeps_the_fields_its_backend_wrote() {
    let snapshot = Snapshot::new("local", "snap-1")
        .with_field("path", "/var/snapshots/snap-1")
        .with_field("restorable", true);

    let rendered: Value = snapshot.clone().into();
    assert_eq!(
        rendered,
        json!({
            "type": "local",
            "id": "snap-1",
            "path": "/var/snapshots/snap-1",
            "restorable": true,
        })
    );

    let mut registry = TypeRegistry::new(snapshot_kind());
    registry
        .register("local", "TestLocalSnapshot")
        .expect("register");
    let back = Snapshot::parse(&registry, &rendered).expect("round trip");
    assert_eq!(back, snapshot);
    assert_eq!(back.id(), "snap-1");
    assert_eq!(back.field("restorable"), Some(&json!(true)));
}

#[test]
fn a_snapshots_id_has_one_home() {
    // `id` is typed, so letting it also be set as a loose field would let the two disagree.
    let snapshot = Snapshot::new("local", "snap-1").with_field("id", "snap-2");

    assert_eq!(snapshot.id(), "snap-1");
    assert_eq!(snapshot.field("id"), Some(&json!("snap-1")));
}

#[test]
fn a_snapshot_without_a_string_id_is_refused() {
    let mut registry = TypeRegistry::new(snapshot_kind());
    registry
        .register("local", "TestLocalSnapshot")
        .expect("register");
    for payload in [json!({"type": "local"}), json!({"type": "local", "id": 7})] {
        let error = Snapshot::parse(&registry, &payload).expect_err("refuse");
        assert_eq!(
            error.to_string(),
            "snapshot payload is invalid for type `local`: snapshot payload must include a string `id`"
        );
    }
}

#[test]
fn extension_fields_cannot_replace_the_discriminator() {
    let mut registry = TypeRegistry::new(snapshot_kind());
    Snapshot::register_noop(&mut registry).expect("register");
    for replacement in [json!("remote"), json!(null), json!(42)] {
        let payload = DiscriminatedPayload::new("noop").with_field("type", replacement.clone());
        assert_eq!(payload.type_name(), "noop");
        assert!(!payload.fields().contains_key("type"));
        assert_eq!(payload.to_json(), json!({"type": "noop"}));
        let restored: DiscriminatedPayload =
            serde_json::from_value(serde_json::to_value(&payload).expect("serialize"))
                .expect("deserialize raw payload");
        assert_eq!(restored, payload);

        let snapshot = Snapshot::noop().with_field("type", replacement);
        let restored = Snapshot::parse(
            &registry,
            &serde_json::to_value(&snapshot).expect("serialize snapshot"),
        )
        .expect("restore snapshot");
        assert_eq!(restored, snapshot);
        assert!(restored.is_noop());
    }
}

#[test]
fn state_parse_errors_discard_sensitive_data_in_every_diagnostic_format() {
    const SECRET: &str = "credential=secret-sentinel";
    let mut registry = TypeRegistry::new(session_state_kind());
    registry
        .register_with(SECRET, "TestState", |_| Err(SECRET.to_owned()))
        .expect("register");
    let failures = [
        registry
            .parse(&json!({"type": "unknown-credential=secret-sentinel"}))
            .expect_err("unknown"),
        registry
            .parse(&json!({"type": SECRET}))
            .expect_err("invalid fields"),
        registry
            .parse(&json!({"type": [SECRET]}))
            .expect_err("non-string type"),
        registry.parse(&json!([SECRET])).expect_err("non-object"),
        registry
            .readmit(DiscriminatedPayload::new(SECRET))
            .expect_err("invalid readmission"),
    ];
    assert!(matches!(
        failures[1],
        RegistryError::InvalidPayloadRedacted { .. }
    ));
    for error in failures {
        for rendered in [
            error.to_string(),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            assert!(
                !rendered.contains(SECRET),
                "sensitive data escaped: {rendered}"
            );
        }
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn readmission_revalidates_constructed_deserialized_and_modified_payloads() {
    let mut registry = TypeRegistry::new(client_options_kind());
    registry
        .register_with("custom", "CustomOptions", |payload| {
            match payload.field("image") {
                Some(Value::String(image)) if !image.is_empty() => Ok(payload),
                _ => Err("image must be a non-empty string".into()),
            }
        })
        .expect("register");
    let valid = registry
        .parse(&json!({"type": "custom", "image": "example"}))
        .expect("valid");
    let forged: DiscriminatedPayload =
        serde_json::from_value(json!({"type": "custom", "image": 42}))
            .expect("raw deserialization");
    for payload in [
        DiscriminatedPayload::new("custom"),
        forged,
        valid.with_field("image", ""),
    ] {
        let expected = registry
            .parse(&payload.to_json())
            .expect_err("invalid JSON");
        assert_eq!(
            registry.readmit(payload).expect_err("invalid payload"),
            expected
        );
    }
}

#[test]
fn readmission_runs_normalization_in_the_receiving_registry() {
    let mut registry = TypeRegistry::new(client_options_kind());
    registry
        .register_with("custom", "CustomOptions", |payload| {
            Ok(payload.with_field("normalized", true))
        })
        .expect("register");
    let payload = DiscriminatedPayload::new("custom");
    let expected = registry.parse(&payload.to_json()).expect("parse");
    assert_eq!(registry.readmit(payload).expect("readmit"), expected);
    assert_eq!(expected.field("normalized"), Some(&json!(true)));
}

#[test]
fn snapshot_restore_validates_and_normalizes_registered_backend_fields() {
    let mut registry = TypeRegistry::new(snapshot_kind());
    registry
        .register_with("custom", "CustomSnapshot", |payload| {
            match payload.field("storage") {
                Some(Value::String(_)) => Ok(payload.with_field("version", 1)),
                _ => Err("storage must be a string".into()),
            }
        })
        .expect("register");
    let raw: Value = serde_json::from_str(
        r#"{"type":"custom","id":"s","storage":"example","extension":{"future":1}}"#,
    )
    .expect("deserialize raw JSON");
    let snapshot = Snapshot::parse(&registry, &raw).expect("restore");
    assert_eq!(snapshot.field("version"), Some(&json!(1)));
    assert_eq!(snapshot.field("extension"), Some(&json!({"future": 1})));
    let serialized = serde_json::to_value(&snapshot).expect("serialize");
    assert_eq!(
        Snapshot::parse(&registry, &serialized).expect("round trip"),
        snapshot
    );
    assert!(Snapshot::parse(&registry, &json!({"type": "custom", "id": "s"})).is_err());
    assert!(Snapshot::parse(&registry, &json!({"type": "unknown", "id": "s"})).is_err());
}

#[test]
fn a_normalizer_cannot_reroute_a_payload_to_another_type() {
    for kind in [client_options_kind(), session_state_kind()] {
        let mut registry = TypeRegistry::new(kind);
        registry
            .register_with("custom", "CustomType", |_| {
                Ok(DiscriminatedPayload::new("unregistered"))
            })
            .expect("register");
        assert!(registry.parse(&json!({"type": "custom"})).is_err());
        assert!(
            registry
                .readmit(DiscriminatedPayload::new("custom"))
                .is_err()
        );
    }
}

#[test]
fn a_snapshot_routes_through_the_registry_before_it_is_built() {
    let mut registry = TypeRegistry::new(snapshot_kind());
    Snapshot::register_noop(&mut registry).expect("register noop");

    let parsed = Snapshot::parse(&registry, &json!({"type": "noop", "id": ""})).expect("parse");
    assert!(parsed.is_noop());

    let error = Snapshot::parse(&registry, &json!({"type": "local", "id": "x"}))
        .expect_err("refuse an unassembled backend");
    assert_eq!(error.to_string(), "unknown snapshot type `local`");
}

#[test]
fn a_registrant_can_normalize_what_it_accepts() {
    // The reference coerces ports on the way in; the registry stays ignorant of any family's
    // fields by letting the owner do it.
    let mut registry = TypeRegistry::new(client_options_kind());
    registry
        .register_with(
            "docker",
            "DockerSandboxClientOptions",
            |payload| match payload.field("exposed_ports") {
                Some(Value::Number(port)) => {
                    let single = port.clone();
                    Ok(payload.with_field("exposed_ports", json!([single])))
                }
                Some(Value::Array(_)) | None => Ok(payload),
                Some(_) => Err("exposed_ports must be an iterable of TCP port integers".to_owned()),
            },
        )
        .expect("register");

    let parsed = registry
        .parse(&json!({"type": "docker", "exposed_ports": 8080}))
        .expect("parse");
    assert_eq!(parsed.field("exposed_ports"), Some(&json!([8080])));

    let error = registry
        .parse(&json!({"type": "docker", "exposed_ports": "8080"}))
        .expect_err("refuse");
    assert_eq!(
        error.to_string(),
        "sandbox client options payload is invalid for type `docker`: \
         exposed_ports must be an iterable of TCP port integers"
    );
}
