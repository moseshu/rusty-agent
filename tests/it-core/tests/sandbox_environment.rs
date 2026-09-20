//! `ra-core::sandbox::environment`: the environment a workspace is materialized with.
//!
//! The behavior pinned here is what keeps secrets out of a manifest and resolution honest:
//! - the three shapes a member can be written in, and that each renders back the way it was written
//! - a value reference round-trips without ever carrying the value it stands for
//! - the discriminator-free literal shape still reads, and the ambiguous one is refused
//! - a failed lookup cancels the lookups still in flight

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ra_core::sandbox::{
    DiscriminatedPayload, EnvEntry, EnvMember, EnvValue, EnvValueResolver, Environment, ErrorCode,
    Manifest, ManifestRegistries, OpName, SandboxError, UnresolvableEnvValues,
    builtin_env_value_registry,
};
use serde_json::json;
use tokio::sync::Notify;

fn registry() -> ra_core::sandbox::TypeRegistry {
    let mut registry = builtin_env_value_registry();
    registry
        .register("test.secret_reference", "test")
        .expect("the type is free");
    registry
}

fn parse(value: &serde_json::Value) -> Environment {
    Environment::parse(&registry(), value).expect("environment parses")
}

fn secret(key: &str) -> EnvValue {
    EnvValue::reference(DiscriminatedPayload::new("test.secret_reference").with_field("key", key))
}

/// Resolves the test reference type, and records whether it was ever cancelled.
struct Secrets {
    released: Arc<Notify>,
    started: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
    fail: bool,
}

#[async_trait]
impl EnvValueResolver for Secrets {
    async fn resolve(&self, value: &EnvValue) -> Result<String, SandboxError> {
        let key = value
            .field("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if self.fail && key == "failing" {
            // Fail only once the sibling is genuinely in flight, so the test pins the interleaving
            // rather than racing the two lookups.
            self.started.notified().await;
            return Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Materialize,
                "secret backend rejected the request",
            ));
        }
        if self.fail {
            // `notify_one` stores a permit when nobody is waiting yet, so the ordering of the two
            // lookups does not decide whether the signal is seen.
            self.started.notify_one();
            let guard = CancelFlag(Arc::clone(&self.cancelled));
            self.released.notified().await;
            // Only reached if the lookup was not cancelled.
            std::mem::forget(guard);
            return Ok("unreachable".to_owned());
        }
        Ok(format!("resolved-secret-for-{key}"))
    }
}

/// Records that the future holding it was dropped before finishing.
struct CancelFlag(Arc<AtomicBool>);

impl Drop for CancelFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// --- the three shapes ---------------------------------------------------------------------------

#[test]
fn a_member_keeps_the_shape_it_was_written_in() {
    let environment = Environment::new()
        .with("PLAIN", "plain")
        .with_value("TYPED", EnvValue::literal("typed"))
        .with_entry(
            "ENTRY",
            EnvEntry::new(secret("entry"))
                .with_description("secret reference")
                .ephemeral(true),
        );

    assert_eq!(
        environment.to_json(),
        json!({"value": {
            "PLAIN": "plain",
            "TYPED": {"type": "str", "value": "typed"},
            "ENTRY": {
                "description": "secret reference",
                "ephemeral": true,
                "value": {"type": "test.secret_reference", "key": "entry"},
            },
        }})
    );
    assert_eq!(parse(&environment.to_json()), environment);
}

#[test]
fn every_shape_flattens_into_an_entry() {
    let environment = Environment::new()
        .with("PLAIN", "plain")
        .with_value("TYPED", EnvValue::literal("typed"))
        .with_entry("ENTRY", EnvEntry::new(secret("entry")).ephemeral(true));

    let normalized = environment.normalized();
    assert_eq!(normalized["PLAIN"].value().as_literal(), Some("plain"));
    assert!(!normalized["PLAIN"].is_ephemeral());
    assert_eq!(normalized["TYPED"].value().as_literal(), Some("typed"));
    // A reference has no value until it is resolved, so there is nothing to read out of it here.
    assert_eq!(normalized["ENTRY"].value().as_literal(), None);
    assert!(normalized["ENTRY"].is_ephemeral());
}

#[test]
fn a_reference_never_carries_the_value_it_stands_for() {
    // The whole point of the shape: a manifest is written down, passed around and persisted, and a
    // resolved secret in it would be a secret in all three places.
    let environment = Environment::new().with_value("TOKEN", secret("token"));
    let rendered = serde_json::to_string(&environment).expect("serializes");

    assert!(rendered.contains("test.secret_reference"));
    assert!(rendered.contains("token"));
    assert!(!rendered.contains("resolved-secret"));
}

// --- reading one back ---------------------------------------------------------------------------

#[test]
fn a_literal_without_a_discriminator_still_reads() {
    // That shape predates the discriminator and still appears in manifests written by hand.
    let environment = parse(&json!({"value": {
        "DIRECT": {"value": "direct-value"},
        "ENTRY": {
            "description": "typed entry",
            "ephemeral": true,
            "value": {"value": "entry-value"},
        },
    }}));

    let normalized = environment.normalized();
    assert_eq!(
        normalized["DIRECT"].value().as_literal(),
        Some("direct-value")
    );
    assert_eq!(
        normalized["ENTRY"].value().as_literal(),
        Some("entry-value")
    );
    assert_eq!(normalized["ENTRY"].description(), Some("typed entry"));
    assert!(normalized["ENTRY"].is_ephemeral());
}

#[test]
fn a_mapping_that_could_be_either_shape_is_refused_rather_than_guessed_at() {
    // `{"value": "plain", "description": ...}` could be a literal with extra fields or an entry
    // whose value is a string. Picking one would silently drop the other reading.
    let ambiguous = json!({"value": {
        "AMBIGUOUS": {"value": "plain", "description": "not a literal"},
    }});

    let error = Environment::parse(&registry(), &ambiguous).expect_err("refuse");
    assert!(error.to_string().contains("type"), "{error}");
}

#[test]
fn a_value_type_nobody_registered_is_refused() {
    let unknown = json!({"value": {"TOKEN": {"type": "unknown.env.value"}}});

    let error = Environment::parse(&registry(), &unknown).expect_err("refuse");
    assert!(error.to_string().contains("unknown.env.value"), "{error}");
}

#[test]
fn an_empty_environment_reads_back_from_anything_that_sets_nothing() {
    assert!(parse(&json!({})).is_empty());
    assert!(parse(&json!({"value": {}})).is_empty());
    assert_eq!(Environment::new().to_json(), json!({"value": {}}));
}

#[test]
fn a_manifest_carries_its_environment_through_the_registry() {
    let mut manifest = Manifest::new();
    manifest.environment = Environment::new().with_value("TOKEN", secret("token"));

    let rendered = serde_json::to_value(&manifest).expect("serializes");
    assert_eq!(
        rendered["environment"]["value"]["TOKEN"]["type"],
        json!("test.secret_reference")
    );

    // A host that has not registered the value type gets a refusal rather than a manifest it
    // cannot resolve; one that has reads it back.
    assert!(Manifest::parse(&ManifestRegistries::builtin(), &rendered).is_err());

    let mut registries = ManifestRegistries::builtin();
    registries
        .env_values_mut()
        .register("test.secret_reference", "test")
        .expect("the type is free");
    assert_eq!(
        Manifest::parse(&registries, &rendered).expect("host knows the type"),
        manifest
    );
}

// --- resolving ------------------------------------------------------------------------------------

#[tokio::test]
async fn resolving_returns_every_member_whatever_shape_it_was_written_in() {
    let environment = Environment::new()
        .with("PLAIN", "literal")
        .with_value("REF", secret("alpha"))
        .with_entry("ENTRY", EnvEntry::new(secret("beta")));
    let resolver = Secrets {
        released: Arc::new(Notify::new()),
        started: Arc::new(Notify::new()),
        cancelled: Arc::new(AtomicBool::new(false)),
        fail: false,
    };

    let resolved = environment.resolve(&resolver).await.expect("resolves");

    assert_eq!(resolved["PLAIN"], "literal");
    assert_eq!(resolved["REF"], "resolved-secret-for-alpha");
    assert_eq!(resolved["ENTRY"], "resolved-secret-for-beta");
}

#[tokio::test]
async fn a_failed_lookup_cancels_the_ones_still_in_flight() {
    // The manifest has already failed. Leaving sibling fetches running means credentials still
    // arriving for a workspace that will never exist.
    let cancelled = Arc::new(AtomicBool::new(false));
    let resolver = Secrets {
        released: Arc::new(Notify::new()),
        started: Arc::new(Notify::new()),
        cancelled: Arc::clone(&cancelled),
        fail: true,
    };
    let environment = Environment::new()
        .with_value("BLOCKING", secret("blocking"))
        .with_value("FAILING", secret("failing"));

    let error = environment.resolve(&resolver).await.expect_err("fails");

    assert_eq!(error.message(), "secret backend rejected the request");
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the sibling lookup was left running"
    );
}

#[tokio::test]
async fn a_host_that_resolves_nothing_says_so_rather_than_returning_an_empty_string() {
    let environment = Environment::new()
        .with("PLAIN", "literal")
        .with_value("TOKEN", secret("token"));

    // A literal never reaches a resolver, so an environment of literals resolves without one.
    let literals = Environment::new().with("PLAIN", "literal");
    assert_eq!(
        literals
            .resolve(&UnresolvableEnvValues)
            .await
            .expect("literals need no resolver")["PLAIN"],
        "literal"
    );

    let error = environment
        .resolve(&UnresolvableEnvValues)
        .await
        .expect_err("no resolver");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(
        error.message().contains("test.secret_reference"),
        "{}",
        error.message()
    );
}

#[test]
fn a_member_renders_the_same_way_whether_it_is_read_or_built() {
    let built = EnvMember::Value(EnvValue::literal("x"));
    let read = parse(&json!({"value": {"X": {"type": "str", "value": "x"}}}));

    assert_eq!(read.members()["X"], built);
    assert_eq!(read.members()["X"].to_json(), built.to_json());
}
