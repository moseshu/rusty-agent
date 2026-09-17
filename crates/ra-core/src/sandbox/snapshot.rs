//! The snapshot a session starts from and persists back to.
//!
//! A snapshot names stored workspace content. What the storage is — a local directory, a remote
//! object store, nothing at all — is the backend's business; what every snapshot has in common is a
//! type that says who can read it and an id that says which one it is.
//!
//! Only the shared base lands here. The concrete kinds, their specs, and the resolution that turns
//! a spec into a snapshot belong with the task that ports persistence, and defining them now would
//! fix a storage model before anything reads or writes one.

use serde::Serialize;
use serde_json::Value;

use super::registry::{DiscriminatedPayload, RegistryError, TypeRegistry, snapshot_kind};

/// The type every backend can read, which stores nothing.
///
/// A session with no snapshot still has one: the reference makes the field non-optional so that
/// "nothing was stored" is a value rather than an absence a caller has to test for.
pub const NOOP_SNAPSHOT_TYPE: &str = "noop";

/// What a session starts from and persists back to.
///
/// Fields beyond the type and id are kept as the backend wrote them, subject to its registered
/// normalizer. Unknown fields survive a round trip; unknown snapshot types are refused during
/// parsing. A host without that backend can retain the raw JSON without constructing a snapshot.
///
/// Restore persisted data through [`Self::parse`] with an explicitly assembled registry. Ordinary
/// serde deserialization cannot receive that registry, so this type deliberately only implements
/// `Serialize`. Deserialize a [`Value`] or [`DiscriminatedPayload`] first, then validate it here.
///
/// ```compile_fail
/// use ra_core::sandbox::Snapshot;
/// let _: Snapshot = serde_json::from_str(r#"{"type":"unknown","id":"s"}"#).unwrap();
/// ```
///
/// ```compile_fail
/// use ra_core::sandbox::Snapshot;
/// let _ = Snapshot::try_from(serde_json::json!({"type": "unknown", "id": "s"}));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(into = "Value")]
pub struct Snapshot {
    payload: DiscriminatedPayload,
    id: String,
}

/// The `id` field every snapshot carries.
const ID_FIELD: &str = "id";

impl Snapshot {
    /// Names a snapshot from trusted backend configuration, without consulting a registry.
    ///
    /// Persisted or otherwise untrusted data must go through [`Self::parse`].
    #[must_use]
    pub fn new(snapshot_type: impl Into<String>, id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            payload: DiscriminatedPayload::new(snapshot_type).with_field(ID_FIELD, id.clone()),
            id,
        }
    }

    /// The snapshot that stores nothing.
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NOOP_SNAPSHOT_TYPE, String::new())
    }

    /// Attaches one backend-specific field. Reserved `id` and `type` keys are ignored.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        // The id has a typed home; letting it also be set here would allow the two to disagree.
        if key == ID_FIELD {
            return self;
        }
        self.payload = self.payload.with_field(key, value);
        self
    }

    /// Which backend can read this snapshot.
    #[must_use]
    pub fn snapshot_type(&self) -> &str {
        self.payload.type_name()
    }

    /// Which snapshot this is.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether this snapshot stores nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.snapshot_type() == NOOP_SNAPSHOT_TYPE
    }

    /// A backend-specific field, or `None` when the snapshot does not carry it.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.payload.field(key)
    }

    /// The payload underneath, discriminator included.
    #[must_use]
    pub const fn payload(&self) -> &DiscriminatedPayload {
        &self.payload
    }

    /// Routes a payload to a registered snapshot type.
    ///
    /// # Errors
    ///
    /// Returns the registry's refusal when the payload is not an object, carries no string type, or
    /// names a type this host does not have.
    pub fn parse(registry: &TypeRegistry, value: &Value) -> Result<Self, RegistryError> {
        let payload = registry.parse(value)?;
        Self::from_payload(payload)
    }

    /// Builds a snapshot from an already-routed payload.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidPayload`] when `id` is missing or is not a string.
    fn from_payload(payload: DiscriminatedPayload) -> Result<Self, RegistryError> {
        let id = match payload.field(ID_FIELD) {
            Some(Value::String(id)) => id.clone(),
            _ => {
                return Err(RegistryError::InvalidPayload {
                    noun: snapshot_kind().noun(),
                    type_name: payload.type_name().to_owned(),
                    reason: "snapshot payload must include a string `id`".to_owned(),
                });
            }
        };
        Ok(Self { payload, id })
    }

    /// Registers the snapshot type that stores nothing.
    ///
    /// # Errors
    ///
    /// Returns the registry's refusal when something else already claims `noop`.
    pub fn register_noop(registry: &mut TypeRegistry) -> Result<(), RegistryError> {
        registry.register(NOOP_SNAPSHOT_TYPE, "NoopSnapshot")
    }
}

impl From<Snapshot> for Value {
    fn from(snapshot: Snapshot) -> Self {
        snapshot.payload.to_json()
    }
}
