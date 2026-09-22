//! The snapshot a session starts from and persists back to.
//!
//! A snapshot names stored workspace content. What the storage is — a local directory, a remote
//! object store, nothing at all — is the backend's business; what every snapshot has in common is a
//! type that says who can read it and an id that says which one it is.
//!
//! # Three kinds, and a spec for each
//!
//! A [`Snapshot`] names storage that already has an identity: it is what a session state carries
//! across a stop, so it holds the id that was chosen when the session was made. A [`SnapshotSpec`]
//! is the same choice made before there is anything to name — "put it here, whatever this session
//! turns out to be called" — which is what a caller configures a run with.
//! [`resolve_snapshot`] turns one into the other by supplying the id.
//!
//! # Declaration only
//!
//! Reading and writing the stored bytes is a backend's job, and none of it is here: this module
//! says which storage a snapshot names and what its fields mean. The reference puts `persist`,
//! `restore` and `restorable` on the snapshot object itself; carrying that over literally would
//! give this crate a filesystem.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::{DiscriminatedPayload, RegistryError, TypeRegistry, snapshot_kind};

/// The type every backend can read, which stores nothing.
///
/// A session with no snapshot still has one: the reference makes the field non-optional so that
/// "nothing was stored" is a value rather than an absence a caller has to test for.
pub const NOOP_SNAPSHOT_TYPE: &str = "noop";

/// The type stored as a tar file on the machine running the SDK.
pub const LOCAL_SNAPSHOT_TYPE: &str = "local";

/// The type stored by a client the host supplies.
pub const REMOTE_SNAPSHOT_TYPE: &str = "remote";

/// The directory a local snapshot's tar file lives in.
const BASE_PATH_FIELD: &str = "base_path";

/// The dependency a remote snapshot's storage client is bound under.
const CLIENT_DEPENDENCY_KEY_FIELD: &str = "client_dependency_key";

/// Who claims the built-in snapshot types, so a host's own type cannot silently take one over.
const BUILTIN_REGISTRANT: &str = "ra_core::sandbox::snapshot";

/// A host directory that cannot be named in a snapshot payload.
///
/// Carries the path as the host would show it — which is lossy, and is why this exists: the lossy
/// rendering is fine for a message a person reads and wrong for the value that decides where an
/// archive is written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a snapshot's base path must be valid UTF-8: {shown}")]
pub struct SnapshotPathError {
    shown: String,
}

impl SnapshotPathError {
    /// Reports the path that could not be named.
    fn new(path: &Path) -> Self {
        Self {
            shown: path.to_string_lossy().into_owned(),
        }
    }

    /// The path as the host would show it, with whatever it could not read replaced.
    #[must_use]
    pub fn shown(&self) -> &str {
        &self.shown
    }
}

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

    /// A snapshot stored as `<id>.tar` under `base_path`, on the machine running the SDK.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotPathError`] when the directory is not valid UTF-8. The payload this
    /// becomes is JSON, and a session state is stored as JSON, so a path that cannot be written as
    /// a JSON string cannot be named here — the same constraint the workspace root already carries.
    /// **Refused rather than rendered lossily**: the lossy rendering maps every invalid byte to one
    /// replacement character, so two directories that differ only in such bytes would become the
    /// same stored path, and one session's workspace would come back as another's.
    pub fn local(
        id: impl Into<String>,
        base_path: impl AsRef<Path>,
    ) -> Result<Self, SnapshotPathError> {
        let base_path = base_path.as_ref();
        let base_path = base_path
            .to_str()
            .ok_or_else(|| SnapshotPathError::new(base_path))?;
        Ok(Self::new(LOCAL_SNAPSHOT_TYPE, id).with_field(BASE_PATH_FIELD, base_path.to_owned()))
    }

    /// A snapshot stored by whichever client the host bound under `client_dependency_key`.
    #[must_use]
    pub fn remote(id: impl Into<String>, client_dependency_key: impl Into<String>) -> Self {
        Self::new(REMOTE_SNAPSHOT_TYPE, id)
            .with_field(CLIENT_DEPENDENCY_KEY_FIELD, client_dependency_key.into())
    }

    /// Where a local snapshot's tar file lives, or `None` for any other kind.
    #[must_use]
    pub fn local_base_path(&self) -> Option<PathBuf> {
        if self.snapshot_type() != LOCAL_SNAPSHOT_TYPE {
            return None;
        }
        match self.field(BASE_PATH_FIELD) {
            Some(Value::String(base_path)) => Some(PathBuf::from(base_path)),
            _ => None,
        }
    }

    /// Which dependency holds a remote snapshot's storage client, or `None` for any other kind.
    #[must_use]
    pub fn remote_client_dependency_key(&self) -> Option<&str> {
        if self.snapshot_type() != REMOTE_SNAPSHOT_TYPE {
            return None;
        }
        match self.field(CLIENT_DEPENDENCY_KEY_FIELD) {
            Some(Value::String(key)) => Some(key),
            _ => None,
        }
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

/// The registry holding the three snapshot kinds every host has.
///
/// The field checks are the point: a persisted local snapshot without a `base_path` names a file
/// nobody can find, and refusing it while parsing state is the difference between a resume that
/// fails and a resume that starts an empty workspace as if that were what was stored. The
/// reference gets the same refusal from the field being required on the model, and this asks for
/// exactly what that model does — present, and a string — rather than adding conditions of its own.
#[must_use]
pub fn builtin_snapshot_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new(snapshot_kind());
    let builtins = [
        (NOOP_SNAPSHOT_TYPE, None),
        (LOCAL_SNAPSHOT_TYPE, Some(BASE_PATH_FIELD)),
        (REMOTE_SNAPSHOT_TYPE, Some(CLIENT_DEPENDENCY_KEY_FIELD)),
    ];
    for (type_name, required) in builtins {
        let registered = registry.register_with(type_name, BUILTIN_REGISTRANT, move |payload| {
            let Some(field) = required else {
                return Ok(payload);
            };
            match payload.field(field) {
                Some(Value::String(_)) => Ok(payload),
                _ => Err(format!("`{field}` must be a string")),
            }
        });
        debug_assert!(
            registered.is_ok(),
            "built-in snapshot types must be distinct"
        );
    }
    registry
}

/// Where to put a snapshot, decided before there is a session to name it after.
///
/// The closed set is the reference's: its snapshot *types* are open to a host's own subclass, but
/// the specs a run configuration accepts are a discriminated union of these three.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SnapshotSpec {
    /// Store it as a tar file under this directory, on the machine running the SDK.
    Local {
        /// The directory the tar file goes in.
        base_path: PathBuf,
    },
    /// Store it nowhere.
    Noop,
    /// Store it through the client bound under this dependency key.
    Remote {
        /// Which dependency holds the storage client.
        client_dependency_key: String,
    },
}

impl SnapshotSpec {
    /// Names the snapshot this spec describes for a session called `snapshot_id`.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotPathError`] when a local spec's directory is not valid UTF-8. The
    /// reference cannot fail here because a Python path needs no encoding to be carried; the field
    /// is public, so this is where a directory that cannot be written down is caught.
    pub fn build(&self, snapshot_id: &str) -> Result<Snapshot, SnapshotPathError> {
        match self {
            Self::Local { base_path } => Snapshot::local(snapshot_id, base_path),
            Self::Noop => Ok(Snapshot::new(NOOP_SNAPSHOT_TYPE, snapshot_id)),
            Self::Remote {
                client_dependency_key,
            } => Ok(Snapshot::remote(snapshot_id, client_dependency_key.clone())),
        }
    }
}

/// What a caller can hand in when asking for a session: a snapshot, or where to put one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSource {
    /// A snapshot that already has an id, used as it stands.
    Snapshot(Snapshot),
    /// Storage to name once the session's id is known.
    Spec(SnapshotSpec),
}

impl From<Snapshot> for SnapshotSource {
    fn from(snapshot: Snapshot) -> Self {
        Self::Snapshot(snapshot)
    }
}

impl From<SnapshotSpec> for SnapshotSource {
    fn from(spec: SnapshotSpec) -> Self {
        Self::Spec(spec)
    }
}

/// Settles what a session starts from and persists back to.
///
/// A snapshot handed in is used as it stands, **id included**: the caller named a specific stored
/// workspace, and renaming it to this session would make the session persist somewhere nobody asked
/// for. A spec is built with `snapshot_id`, and nothing at all means the snapshot that stores
/// nothing — which still takes the id, so a session that later gets real storage does not change
/// identity.
///
/// # Errors
///
/// Returns [`SnapshotPathError`] when a local spec names a directory that is not valid UTF-8.
pub fn resolve_snapshot(
    source: Option<&SnapshotSource>,
    snapshot_id: &str,
) -> Result<Snapshot, SnapshotPathError> {
    match source {
        Some(SnapshotSource::Snapshot(snapshot)) => Ok(snapshot.clone()),
        Some(SnapshotSource::Spec(spec)) => spec.build(snapshot_id),
        None => SnapshotSpec::Noop.build(snapshot_id),
    }
}
