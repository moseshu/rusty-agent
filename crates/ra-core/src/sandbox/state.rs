//! What a session needs in order to be resumed somewhere else, or later.
//!
//! A session state is the durable half of a session: which backend made it, which sandbox it is,
//! what the workspace was supposed to contain, and what was persisted. A host writes one when a run
//! pauses and hands it back when the run continues, possibly in a different process.
//!
//! # Authority does not survive the round trip, on purpose
//!
//! A manifest carries authority — mount credentials, and grants naming host paths — and a persisted
//! state must not. Rendering strips mount authority and records that it did
//! ([`crate::sandbox::mount_security::REDACTED_MOUNT_AUTHORITY_KEY`]); a client rendering a state
//! also drops every grant with a host source and lists the paths it dropped. Reading one back with
//! [`SandboxSessionState::parse`] sanitizes again before anything is interpreted, strips any host
//! source that made it through anyway, and remembers what has to be rebound. A state in that
//! condition refuses to resume ([`SandboxSessionState::assert_path_grants_rebound`]) until the host
//! rebinds it from a manifest it trusts *now*: the payload is never the source of authority.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::error::{ErrorCode, OpName, SandboxError};
use super::manifest::{Manifest, ManifestRegistries};
use super::mount_security::{
    CREDENTIALLESS_MOUNT_AUTHORITY_KEY, REDACTED_MOUNT_AUTHORITY_KEY,
    rebind_manifest_mount_authority, sanitize_manifest_mount_authority,
    sanitize_raw_session_state_mount_authority, validate_manifest_mount_credential_boundaries,
};
use super::registry::{DiscriminatedPayload, RegistryError, TypeRegistry, session_state_kind};
use super::snapshot::Snapshot;
use super::workspace_paths::SandboxPathGrant;

/// The state field listing the path grants whose host source was dropped when it was written.
///
/// The reference's key, spelled exactly, so states travel between the two implementations.
pub const REDACTED_HOST_PATH_GRANT_PATHS_KEY: &str =
    "__openai_agents_redacted_host_path_grant_paths";

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
    /// Grant paths whose host source was dropped and must come from a trusted manifest.
    path_grants_require_rebind: Vec<String>,
    /// Whether mount authority was stripped and must come from a trusted manifest.
    mount_authority_redacted: bool,
    /// Whether mount authority was rebound from a trusted manifest.
    mount_authority_rebound: bool,
}

/// Why a persisted session state could not be read.
///
/// Says nothing about what was wrong with it. The payload is what carries credentials, and a parse
/// failure is exactly where a detailed message would quote them: a field of the wrong type, an
/// unknown discriminator that happens to be a secret, a malformed URL with a password in it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSessionStatePayload {
    /// The payload was not an object.
    #[error("session state payload must be an object")]
    NotAnObject,
    /// The payload could not be sanitized or read.
    #[error("sandbox session state payload is invalid")]
    Invalid,
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
            path_grants_require_rebind: Vec::new(),
            mount_authority_redacted: false,
            mount_authority_rebound: false,
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

    /// Forgets the fingerprint, because there is no longer one that can be trusted.
    ///
    /// Not the same as leaving the old one in place. A snapshot that was persisted without a
    /// fingerprint, or one whose fingerprint could not be computed, must not be compared against
    /// the value some earlier persist left behind — that comparison can only report a match that is
    /// not one.
    #[must_use]
    pub fn without_snapshot_fingerprint(mut self) -> Self {
        self.snapshot_fingerprint = None;
        self.snapshot_fingerprint_version = None;
        self
    }

    /// Describes the same session with a different manifest.
    ///
    /// Every other field — including what still has to be rebound from a trusted manifest — is
    /// kept: swapping the manifest is not a rebind, and a state that forgot its pending rebind here
    /// would resume with whatever authority the new manifest happened to carry.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = manifest;
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
        if MODELLED_FIELDS.contains(&key.as_str()) || MARKER_FIELDS.contains(&key.as_str()) {
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

    /// Grant paths whose host source was dropped, and which a trusted manifest has to supply.
    #[must_use]
    pub fn path_grants_require_rebind(&self) -> &[String] {
        &self.path_grants_require_rebind
    }

    /// Whether mount authority was stripped from this state and has not been rebound.
    #[must_use]
    pub const fn mount_authority_redacted(&self) -> bool {
        self.mount_authority_redacted
    }

    /// Whether this state's mount topology was rebound from a current trusted manifest.
    #[must_use]
    pub const fn mount_authority_rebound(&self) -> bool {
        self.mount_authority_rebound
    }

    /// Renders the state for storage, with no mount authority in it.
    ///
    /// Authority is stripped from the manifest while rendering — not afterwards on the read side,
    /// which would put credentials on disk first — and the rendering records that it was, so the
    /// reader knows the state has to be rebound. Grants with a host source are left in; a client
    /// rendering a state for storage drops them as well (see
    /// [`crate::sandbox::SandboxClient::serialize_session_state`]).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::MountConfigInvalid`] for a manifest holding a custom mount or mount
    /// strategy, and for one carrying authority that cannot be rendered safely, such as a
    /// credential file populated from the host.
    pub fn to_json(&self) -> Result<Value, SandboxError> {
        let (manifest, redacted) = sanitize_manifest_mount_authority(&self.manifest)?;
        let mut payload = self.render(manifest);
        if (redacted || self.mount_authority_redacted)
            && let Value::Object(fields) = &mut payload
        {
            fields.insert(REDACTED_MOUNT_AUTHORITY_KEY.to_owned(), Value::Bool(true));
        }
        Ok(payload)
    }

    /// Renders the state around an already rendered manifest.
    ///
    /// Every modelled field is written, the fingerprint pair as `null` when there is none: that is
    /// the reference's released shape, and a reader that checks it field by field should find the
    /// same keys whichever implementation wrote the state.
    fn render(&self, manifest: Value) -> Value {
        self.extra
            .clone()
            .with_field("session_id", self.session_id.to_string())
            .with_field("snapshot", Value::from(self.snapshot.clone()))
            .with_field("manifest", manifest)
            .with_field("exposed_ports", Value::from(self.exposed_ports.clone()))
            .with_field(
                "snapshot_fingerprint",
                self.snapshot_fingerprint
                    .clone()
                    .map_or(Value::Null, Value::from),
            )
            .with_field(
                "snapshot_fingerprint_version",
                self.snapshot_fingerprint_version
                    .clone()
                    .map_or(Value::Null, Value::from),
            )
            .with_field("workspace_root_ready", self.workspace_root_ready)
            .to_json()
    }

    /// The state as a client writes it: grants with a host source dropped, and their paths.
    ///
    /// The paths include those already awaiting a rebind, so a state that is written again before
    /// it was rebound does not forget what it is missing.
    pub(crate) fn without_host_path_grants(&self) -> (Self, BTreeSet<String>) {
        let mut dropped: BTreeSet<String> =
            self.path_grants_require_rebind.iter().cloned().collect();
        let mut persistable = self.clone();
        persistable.manifest.extra_path_grants.retain(|grant| {
            if grant.host_path().is_some() {
                dropped.insert(grant.path().to_owned());
                false
            } else {
                true
            }
        });
        (persistable, dropped)
    }

    /// Reads a persisted state back, sanitizing it before anything is interpreted.
    ///
    /// Takes the payload by value: it is what carries whatever credentials a writer let through,
    /// and the caller does not keep it once it has been read. The embedded manifest's mount
    /// authority is stripped first; then any grant with a host source is dropped and remembered as
    /// needing a rebind, alongside the paths the writer listed under
    /// [`REDACTED_HOST_PATH_GRANT_PATHS_KEY`]. Removing that list from a payload does not bring a
    /// host source back — it only forgets that one was wanted.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSessionStatePayload`] for anything that is not an object, cannot be
    /// sanitized, lacks a string `type`, or whose snapshot, manifest or modelled fields do not read.
    /// The error never says which.
    pub fn parse(
        payload: Value,
        snapshots: &TypeRegistry,
        manifests: &ManifestRegistries,
    ) -> Result<Self, InvalidSessionStatePayload> {
        if !payload.is_object() {
            return Err(InvalidSessionStatePayload::NotAnObject);
        }
        let state = Self::read_persisted(&payload, snapshots, manifests);
        // Gone before any error reaches the caller, whichever way the read went.
        drop(payload);
        state.ok_or(InvalidSessionStatePayload::Invalid)
    }

    /// Reads a persisted state, discarding why it failed.
    fn read_persisted(
        payload: &Value,
        snapshots: &TypeRegistry,
        manifests: &ManifestRegistries,
    ) -> Option<Self> {
        let (sanitized, _) =
            sanitize_raw_session_state_mount_authority(payload, manifests.entries()).ok()?;
        let Value::Object(fields) = sanitized else {
            return None;
        };
        let Some(Value::String(state_type)) = fields.get("type") else {
            return None;
        };
        let snapshot = Snapshot::parse(snapshots, fields.get("snapshot")?).ok()?;
        let manifest = Manifest::parse(manifests, fields.get("manifest")?).ok()?;
        let mut discriminated = DiscriminatedPayload::new(state_type.clone());
        for (key, value) in &fields {
            if key != "type" {
                discriminated = discriminated.with_field(key.clone(), value.clone());
            }
        }
        let state = Self::from_payload(&discriminated, snapshot, manifest).ok()?;
        Some(state.mark_persisted_authority(&fields))
    }

    /// Records what a persisted payload says must be rebound, and drops host sources it carried.
    fn mark_persisted_authority(mut self, fields: &serde_json::Map<String, Value>) -> Self {
        let listed = match fields.get(REDACTED_HOST_PATH_GRANT_PATHS_KEY) {
            Some(Value::Array(paths)) => paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let mut serialized = Vec::new();
        self.manifest.extra_path_grants.retain(|grant| {
            if grant.host_path().is_some() {
                serialized.push(grant.path().to_owned());
                false
            } else {
                true
            }
        });
        for path in listed.into_iter().chain(serialized) {
            if !self.path_grants_require_rebind.contains(&path) {
                self.path_grants_require_rebind.push(path);
            }
        }
        self.mount_authority_redacted = self.mount_authority_redacted
            || fields.get(REDACTED_MOUNT_AUTHORITY_KEY) == Some(&Value::Bool(true));
        self
    }

    /// Replaces the persisted path grants with those of a manifest the host trusts now.
    ///
    /// A state with nothing awaiting a rebind is returned unchanged. Otherwise every grant comes
    /// from the trusted manifest — not only the ones that were dropped — because the trusted
    /// manifest is the current statement of what the session may reach.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when no trusted manifest is given, or when it
    /// lacks a host source for a path that was dropped.
    pub fn rebind_persisted_path_grants(
        &self,
        trusted_manifest: Option<&Manifest>,
    ) -> Result<Self, SandboxError> {
        if self.path_grants_require_rebind.is_empty() {
            return Ok(self.clone());
        }
        let Some(trusted) = trusted_manifest else {
            return Err(resume_refused(
                "Sandbox session state contains path grants that require a current trusted \
                 manifest before resume",
            ));
        };
        let trusted_host_paths: BTreeSet<&str> = trusted
            .extra_path_grants
            .iter()
            .filter(|grant| grant.host_path().is_some())
            .map(SandboxPathGrant::path)
            .collect();
        let missing: Vec<&str> = self
            .path_grants_require_rebind
            .iter()
            .map(String::as_str)
            .filter(|path| !trusted_host_paths.contains(path))
            .collect();
        if !missing.is_empty() {
            return Err(resume_refused(format!(
                "Sandbox session state requires current trusted host_path values for these path \
                 grants: {}",
                missing.join(", ")
            )));
        }
        let mut rebound = self.clone();
        rebound
            .manifest
            .extra_path_grants
            .clone_from(&trusted.extra_path_grants);
        rebound.path_grants_require_rebind.clear();
        Ok(rebound)
    }

    /// Restores stripped mount authority from a manifest the host trusts now.
    ///
    /// A state with nothing stripped is returned unchanged. Otherwise the trusted manifest has to
    /// pass the credential boundary for `provider_backend_id` and match this state's credential-free
    /// mount topology exactly (see
    /// [`crate::sandbox::mount_security::rebind_manifest_mount_authority`]).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when no trusted manifest is given, and the
    /// rebind's own failure otherwise.
    pub fn rebind_persisted_mount_authority(
        &self,
        trusted_manifest: Option<&Manifest>,
        provider_backend_id: &str,
    ) -> Result<Self, SandboxError> {
        if !self.mount_authority_redacted {
            return Ok(self.clone());
        }
        let Some(trusted) = trusted_manifest else {
            return Err(resume_refused(
                "Sandbox session state contains redacted cloud mount credentials and requires a \
                 current trusted manifest before resume",
            ));
        };
        let manifest =
            rebind_manifest_mount_authority(&self.manifest, trusted, provider_backend_id)?;
        let mut rebound = self.clone();
        rebound.manifest = manifest;
        rebound.mount_authority_redacted = false;
        rebound.mount_authority_rebound = true;
        Ok(rebound)
    }

    /// Refuses to resume a state that still needs authority it does not have.
    ///
    /// Checks the manifest against the credential boundary of the backend that wrote the state,
    /// then that nothing stripped on the way to storage is still missing.
    ///
    /// # Errors
    ///
    /// Returns the boundary failure, or [`ErrorCode::SandboxConfigInvalid`] when mount authority
    /// or path grants still have to be rebound.
    pub fn assert_path_grants_rebound(&self) -> Result<(), SandboxError> {
        validate_manifest_mount_credential_boundaries(&self.manifest, Some(&self.state_type))?;
        if self.mount_authority_redacted {
            return Err(resume_refused(
                "Sandbox session state with cloud mount credentials cannot be resumed; resume \
                 through Runner with the current trusted manifest",
            ));
        }
        if self.path_grants_require_rebind.is_empty() {
            return Ok(());
        }
        Err(resume_refused(
            "Sandbox session state path grants must be rebound from a current trusted manifest \
             before resume; resume through Runner with SandboxRunConfig.manifest",
        ))
    }

    /// Rebuilds a state from a payload this caller already trusts.
    ///
    /// **This does not sanitize.** It is for a payload the caller produced or has already vetted —
    /// a state handed between components of one host, or one a test built. A payload read back
    /// from storage goes through [`Self::parse`], which strips mount authority and host sources
    /// before any of it is interpreted and marks the state as needing a rebind.
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
            if !MODELLED_FIELDS.contains(&key.as_str()) && !MARKER_FIELDS.contains(&key.as_str()) {
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
            path_grants_require_rebind: Vec::new(),
            mount_authority_redacted: false,
            mount_authority_rebound: false,
        })
    }
}

/// A resume refused for want of trusted authority.
fn resume_refused(message: impl Into<String>) -> SandboxError {
    SandboxError::new(ErrorCode::SandboxConfigInvalid, OpName::Start, message)
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

/// Markers about authority, which the state models as flags rather than carrying as fields.
///
/// Carried as fields they would travel on regardless of what the flags say: a state that was
/// rebound would still claim to need rebinding, and a payload could set one by hand.
const MARKER_FIELDS: [&str; 3] = [
    REDACTED_MOUNT_AUTHORITY_KEY,
    REDACTED_HOST_PATH_GRANT_PATHS_KEY,
    CREDENTIALLESS_MOUNT_AUTHORITY_KEY,
];

impl Serialize for SandboxSessionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialization is the persistence boundary, so authority has to be stripped here too
        // rather than only on the inherent method a caller might not use.
        self.to_json()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
