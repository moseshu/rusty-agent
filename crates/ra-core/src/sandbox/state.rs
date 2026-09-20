//! What a session needs in order to be resumed somewhere else, or later.
//!
//! A session state is the durable half of a session: which backend made it, which sandbox it is,
//! what the workspace was supposed to contain, and what was persisted. A host writes one when a run
//! pauses and hands it back when the run continues, possibly in a different process.
//!
//! # Building one is safe; parsing an untrusted one is not, and is not here
//!
//! Everything in this module builds, reads and renders a state the caller already holds. Reading a
//! state back from storage is a different operation with a different threat model — a persisted
//! payload carries mount authority, so the reference sanitizes it, refuses to take path grants from
//! the payload rather than from a trusted manifest, and discards the payload before any error can
//! quote it. That path lands with the task that ports mount security. Building it here first, with
//! the sanitization left as a later addition, would put the unsafe version in reach for however
//! long that took.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::manifest::Manifest;
use super::registry::{DiscriminatedPayload, RegistryError, session_state_kind};
use super::snapshot::Snapshot;

/// The lowest TCP port a session may expose.
const MIN_PORT: u64 = 1;

/// The highest TCP port a session may expose.
const MAX_PORT: u64 = 65535;

/// The durable half of a sandbox session.
///
/// Fields a host does not model are carried as written, so a state travelling between two hosts
/// that know different backends still describes the same sandbox when it arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSessionState {
    state_type: String,
    session_id: Uuid,
    snapshot: Snapshot,
    manifest: Manifest,
    exposed_ports: Vec<u16>,
    snapshot_fingerprint: Option<String>,
    snapshot_fingerprint_version: Option<String>,
    workspace_root_ready: bool,
    extra: DiscriminatedPayload,
}

/// Why a set of exposed ports was rejected.
///
/// The wording is the reference's: these reach whoever wrote the configuration.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExposedPortsError {
    /// The value was neither a port nor a sequence of them.
    #[error("exposed_ports must be an iterable of TCP port integers")]
    NotIterable,
    /// A member of the sequence was not an integer.
    #[error("exposed_ports must contain integers")]
    NotAnInteger,
    /// A port fell outside the TCP range.
    #[error("exposed_ports entries must be between 1 and 65535")]
    OutOfRange,
}

/// Normalizes whatever a caller offered as a set of exposed ports.
///
/// A bare number is one port, a sequence is many, and `null` is none. Duplicates are dropped and
/// the first appearance sets the order: the list is a set with a stable rendering, so two states
/// that expose the same ports compare equal however they were written.
///
/// # Errors
///
/// Returns [`ExposedPortsError`] for a value that is not a port or a sequence of them, a member
/// that is not an integer, or a port outside 1–65535. Port 0 is refused with the rest: it means
/// "any port" to a socket API and nothing at all to a session that has to publish an address.
///
/// A boolean counts as an integer, so `true` normalizes to port 1 and `false` is refused as out of
/// range. That falls out of the reference testing membership with `isinstance(port, int)`, under
/// which Python's booleans are integers. It is a wart, and tightening it here would still be the
/// wrong place: a port set that one implementation accepts and the other refuses turns a portable
/// configuration into one that depends on which runtime read it.
pub fn normalize_exposed_ports(value: &Value) -> Result<Vec<u16>, ExposedPortsError> {
    let candidates: Vec<&Value> = match value {
        Value::Null => return Ok(Vec::new()),
        // A bare integer is one port, and a boolean is an integer here for the reason above.
        Value::Number(_) | Value::Bool(_) => vec![value],
        Value::Array(items) => items.iter().collect(),
        // Iterating a mapping yields its keys, which are strings and so never ports. An empty one
        // yields nothing at all, which is the same as no ports rather than a refusal.
        Value::Object(fields) if fields.is_empty() => return Ok(Vec::new()),
        Value::Object(_) => return Err(ExposedPortsError::NotAnInteger),
        // A string is iterable in the reference too, and is singled out there precisely so that
        // "8080" is not read as a sequence of characters.
        Value::String(_) => return Err(ExposedPortsError::NotIterable),
    };

    let mut normalized: Vec<u16> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let port = port_number(candidate)?;
        let port = u16::try_from(port)
            .ok()
            .filter(|port| (MIN_PORT..=MAX_PORT).contains(&u64::from(*port)))
            .ok_or(ExposedPortsError::OutOfRange)?;
        if !normalized.contains(&port) {
            normalized.push(port);
        }
    }
    Ok(normalized)
}

/// Reads one member as the integer the reference would have seen.
///
/// Whole numbers too large for a port are out of range rather than non-integers: the reference's
/// integers are unbounded, so a value of 70000 fails the range check rather than the type check,
/// and a caller reading the message should be told which of the two it got wrong.
fn port_number(value: &Value) -> Result<i64, ExposedPortsError> {
    match value {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Number(number) => number.as_i64().map_or_else(
            || {
                if number.is_f64() {
                    // A float is not an integer, however round it looks.
                    Err(ExposedPortsError::NotAnInteger)
                } else {
                    Err(ExposedPortsError::OutOfRange)
                }
            },
            Ok,
        ),
        _ => Err(ExposedPortsError::NotAnInteger),
    }
}

impl SandboxSessionState {
    /// Records the state of a session this caller owns.
    ///
    /// The session id is minted here. A state read back from storage keeps the one it was written
    /// with — see [`Self::from_payload`].
    #[must_use]
    pub fn new(state_type: impl Into<String>, snapshot: Snapshot, manifest: Manifest) -> Self {
        let state_type = state_type.into();
        Self {
            extra: DiscriminatedPayload::new(state_type.clone()),
            state_type,
            session_id: Uuid::new_v4(),
            snapshot,
            manifest,
            exposed_ports: Vec::new(),
            snapshot_fingerprint: None,
            snapshot_fingerprint_version: None,
            workspace_root_ready: false,
        }
    }

    /// Names the sandbox this state describes, instead of the freshly minted identifier.
    #[must_use]
    pub const fn with_session_id(mut self, session_id: Uuid) -> Self {
        self.session_id = session_id;
        self
    }

    /// Records which ports the session publishes.
    ///
    /// # Errors
    ///
    /// Returns [`ExposedPortsError`] when a port is outside 1–65535.
    pub fn with_exposed_ports(
        mut self,
        ports: impl IntoIterator<Item = u16>,
    ) -> Result<Self, ExposedPortsError> {
        let mut normalized = Vec::new();
        for port in ports {
            if port < 1 {
                return Err(ExposedPortsError::OutOfRange);
            }
            if !normalized.contains(&port) {
                normalized.push(port);
            }
        }
        self.exposed_ports = normalized;
        Ok(self)
    }

    /// Records the fingerprint the snapshot was taken at, and which scheme produced it.
    ///
    /// The scheme travels with the value because a fingerprint compared under the wrong scheme is
    /// worse than no fingerprint: it can report a match that is not one, and a session would then
    /// skip restoring a snapshot it needed.
    #[must_use]
    pub fn with_snapshot_fingerprint(
        mut self,
        fingerprint: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        self.snapshot_fingerprint = Some(fingerprint.into());
        self.snapshot_fingerprint_version = Some(version.into());
        self
    }

    /// Records that the workspace root was confirmed to exist.
    #[must_use]
    pub const fn with_workspace_root_ready(mut self, ready: bool) -> Self {
        self.workspace_root_ready = ready;
        self
    }

    /// Attaches one backend-specific field.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        if MODELLED_FIELDS.contains(&key.as_str()) {
            return self;
        }
        self.extra = self.extra.with_field(key, value);
        self
    }

    /// Which backend wrote this state, and can read it.
    #[must_use]
    pub fn state_type(&self) -> &str {
        &self.state_type
    }

    /// Which sandbox this state describes.
    #[must_use]
    pub const fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// What was persisted, which may be the snapshot that stores nothing.
    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// What the workspace was supposed to contain.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Which ports the session publishes.
    #[must_use]
    pub fn exposed_ports(&self) -> &[u16] {
        &self.exposed_ports
    }

    /// The fingerprint the snapshot was taken at, with the scheme that produced it.
    ///
    /// Both or neither: a fingerprint without its scheme cannot be compared safely.
    #[must_use]
    pub fn snapshot_fingerprint(&self) -> Option<(&str, &str)> {
        match (
            self.snapshot_fingerprint.as_deref(),
            self.snapshot_fingerprint_version.as_deref(),
        ) {
            (Some(fingerprint), Some(version)) => Some((fingerprint, version)),
            _ => None,
        }
    }

    /// Whether the workspace root was confirmed to exist when this state was written.
    #[must_use]
    pub const fn workspace_root_ready(&self) -> bool {
        self.workspace_root_ready
    }

    /// A backend-specific field, or `None` when the state does not carry it.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.extra.field(key)
    }

    /// Renders the state, discriminator included.
    ///
    /// # Errors
    ///
    /// Refuses a manifest carrying entries, an environment or extra path grants. Those three are
    /// exactly the fields still held as written, and they are where mount authority lives: an S3
    /// mount's secret key sits in an entry, and a grant that names a host path has to be dropped and
    /// recorded as needing rebinding rather than written out. The reference does that work while
    /// serializing, not while reading back, so leaving it for the read side would put the
    /// credentials on disk first and sanitize them afterwards. Until that path is ported this
    /// refuses rather than writes.
    pub fn to_json(&self) -> Result<Value, UnsupportedPersistence> {
        if !self.manifest.entries.is_empty() {
            return Err(UnsupportedPersistence { field: "entries" });
        }
        if !self.manifest.environment.is_empty() {
            return Err(UnsupportedPersistence {
                field: "environment",
            });
        }
        if self.manifest.grants_extra_paths() {
            return Err(UnsupportedPersistence {
                field: "extra_path_grants",
            });
        }
        Ok(self.render())
    }

    /// Renders the state without asking whether it is safe to persist.
    fn render(&self) -> Value {
        let mut payload = self.extra.clone();
        payload = payload
            .with_field("session_id", self.session_id.to_string())
            .with_field("snapshot", Value::from(self.snapshot.clone()))
            .with_field(
                "manifest",
                serde_json::to_value(&self.manifest).unwrap_or(Value::Null),
            )
            .with_field("exposed_ports", Value::from(self.exposed_ports.clone()))
            .with_field("workspace_root_ready", self.workspace_root_ready);
        if let Some(fingerprint) = &self.snapshot_fingerprint {
            payload = payload.with_field("snapshot_fingerprint", fingerprint.clone());
        }
        if let Some(version) = &self.snapshot_fingerprint_version {
            payload = payload.with_field("snapshot_fingerprint_version", version.clone());
        }
        payload.to_json()
    }

    /// Rebuilds a state from a payload this caller already trusts.
    ///
    /// **This does not sanitize.** It is for a payload the caller produced or has already vetted —
    /// a state handed between components of one host, or one a test built. Reading a persisted
    /// payload back is the operation that must sanitize mount authority and rebind path grants from
    /// a trusted manifest, and it lands with the task that ports mount security.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidPayload`] when a modelled field is present but malformed.
    pub fn from_payload(
        payload: &DiscriminatedPayload,
        snapshot: Snapshot,
        manifest: Manifest,
    ) -> Result<Self, RegistryError> {
        let invalid = |reason: String| RegistryError::InvalidPayload {
            noun: session_state_kind().noun(),
            type_name: payload.type_name().to_owned(),
            reason,
        };

        let session_id = match payload.field("session_id") {
            Some(Value::String(text)) => Uuid::parse_str(text).map_err(|_| {
                invalid("sandbox session state `session_id` must be a UUID".to_owned())
            })?,
            None => Uuid::new_v4(),
            Some(_) => {
                return Err(invalid(
                    "sandbox session state `session_id` must be a string".to_owned(),
                ));
            }
        };

        let exposed_ports = payload
            .field("exposed_ports")
            .map_or_else(|| Ok(Vec::new()), normalize_exposed_ports)
            .map_err(|error| invalid(error.to_string()))?;

        let workspace_root_ready = match payload.field("workspace_root_ready") {
            Some(Value::Bool(ready)) => *ready,
            None => false,
            Some(_) => {
                return Err(invalid(
                    "sandbox session state `workspace_root_ready` must be a boolean".to_owned(),
                ));
            }
        };

        let text_field = |key: &str| match payload.field(key) {
            Some(Value::String(text)) => Ok(Some(text.clone())),
            None | Some(Value::Null) => Ok(None),
            Some(_) => Err(invalid(format!(
                "sandbox session state `{key}` must be a string"
            ))),
        };
        let snapshot_fingerprint = text_field("snapshot_fingerprint")?;
        let snapshot_fingerprint_version = text_field("snapshot_fingerprint_version")?;

        let mut extra = DiscriminatedPayload::new(payload.type_name());
        for (key, value) in payload.fields() {
            if !MODELLED_FIELDS.contains(&key.as_str()) {
                extra = extra.with_field(key.clone(), value.clone());
            }
        }

        Ok(Self {
            state_type: payload.type_name().to_owned(),
            session_id,
            snapshot,
            manifest,
            exposed_ports,
            snapshot_fingerprint,
            snapshot_fingerprint_version,
            workspace_root_ready,
            extra,
        })
    }
}

/// Fields this type models, which therefore never live among the backend-specific ones.
///
/// A field in both places could disagree, and the renderer would have to pick a winner.
const MODELLED_FIELDS: [&str; 7] = [
    "session_id",
    "snapshot",
    "manifest",
    "exposed_ports",
    "snapshot_fingerprint",
    "snapshot_fingerprint_version",
    "workspace_root_ready",
];

/// A state that cannot be written out yet without putting credentials on disk.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "refusing to persist a sandbox session state whose manifest carries `{field}`: mount-authority \
     redaction is not ported yet, and writing it would put credentials on disk"
)]
pub struct UnsupportedPersistence {
    /// The manifest field that carries, or may carry, authority.
    pub field: &'static str,
}

impl Serialize for SandboxSessionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialization is the persistence boundary, so the refusal has to bite here too rather
        // than only on the inherent method a caller might not use.
        self.to_json()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
