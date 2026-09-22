//! The local backend: the workspace is a directory on this host, and commands run on it.
//!
//! There is no machine boundary here. A session's workspace is a real directory, `exec` starts a
//! real process, and the account it runs as is the one running the SDK. What this backend does
//! offer is the *shape* of a sandbox session — a workspace that is created, materialized, stopped
//! and deleted as a unit, with path access decided by the workspace root and its grants — which is
//! what makes the same agent run here and against a container without knowing which it got.
//!
//! On macOS the reference narrows each command with the operating system's own sandbox, and so does
//! this. That is a fence around one command, not an isolation claim for the backend: it is the same
//! filesystem, the same network and the same user.
//!
//! # Unix only
//!
//! The reference refuses to import on Windows and names the container backend instead. The
//! equivalent here is that the session and client are compiled only for unix; a Windows build gets
//! a crate without them rather than a type whose methods all fail.

#[cfg(unix)]
mod archive;
#[cfg(unix)]
mod client;
#[cfg(unix)]
mod confine;
#[cfg(unix)]
mod exec;
#[cfg(unix)]
mod files;
#[cfg(unix)]
mod session;

#[cfg(unix)]
pub use client::UnixLocalSandboxClient;
#[cfg(unix)]
pub use exec::prepare_exec_command;
#[cfg(unix)]
pub use session::UnixLocalSandboxSession;

use ra_core::sandbox::{
    DiscriminatedPayload, ErrorCode, ExposedPortsError, OpName, RegistryError, SandboxError,
    SandboxSessionState, TypeRegistry, normalize_exposed_ports,
};

/// The discriminator this backend's options, states and sessions carry.
pub const UNIX_LOCAL_BACKEND_ID: &str = "unix_local";

/// The prefix of a workspace directory this backend created for itself.
pub(crate) const DEFAULT_WORKSPACE_PREFIX: &str = "sandbox-local-";

/// The state field recording whether this backend created the workspace root.
pub(crate) const WORKSPACE_ROOT_OWNED_FIELD: &str = "workspace_root_owned";

/// The host variables a session sees when it is told not to inherit the environment.
///
/// Locale, certificates, the search path, and the two colour switches — enough for a command to
/// behave like the host's own, and nothing that names a credential. A host that wants a different
/// set passes its own; this is only the answer when it does not say.
pub const HOST_ENVIRONMENT_ALLOWLIST: [&str; 20] = [
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "TZ",
    "TERM",
    "TMPDIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "UV_PYTHON",
    "NO_COLOR",
    "FORCE_COLOR",
    "CI",
];

/// What a caller can configure when it asks this backend for a session.
///
/// One field, because a local workspace has nothing to choose: no image, no endpoint, no template.
/// The options type exists anyway so that a run configuration written against one backend routes to
/// the right one rather than being read by whichever client happens to be installed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnixLocalSandboxClientOptions {
    exposed_ports: Vec<u16>,
}

impl UnixLocalSandboxClientOptions {
    /// Asks for a session with nothing published.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes these ports.
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

    /// The ports the session publishes.
    #[must_use]
    pub fn exposed_ports(&self) -> &[u16] {
        &self.exposed_ports
    }

    /// Renders the options as the routed payload a run configuration carries.
    #[must_use]
    pub fn to_payload(&self) -> DiscriminatedPayload {
        DiscriminatedPayload::new(UNIX_LOCAL_BACKEND_ID)
            .with_field("exposed_ports", self.exposed_ports.clone())
    }

    /// Reads options out of a payload addressed to this backend.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the payload names another backend or its
    /// port list is not one.
    pub fn from_payload(payload: &DiscriminatedPayload) -> Result<Self, SandboxError> {
        if payload.type_name() != UNIX_LOCAL_BACKEND_ID {
            return Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Start,
                format!(
                    "sandbox client options for `{}` cannot configure the `{UNIX_LOCAL_BACKEND_ID}` backend",
                    payload.type_name()
                ),
            )
            .with_context("options_type", payload.type_name())
            .with_context("backend", UNIX_LOCAL_BACKEND_ID));
        }
        let exposed_ports = payload
            .field("exposed_ports")
            .map_or_else(|| Ok(Vec::new()), normalize_exposed_ports)
            .map_err(|error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
                .with_context("backend", UNIX_LOCAL_BACKEND_ID)
            })?;
        Ok(Self { exposed_ports })
    }

    /// Registers this backend's options with a host's routing table.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::AlreadyRegistered`] when something else already claimed the name.
    pub fn register(registry: &mut TypeRegistry) -> Result<(), RegistryError> {
        registry.register(UNIX_LOCAL_BACKEND_ID, "ra-sandbox::unix_local")
    }
}

/// Whether this backend created the workspace root, and may therefore delete it.
///
/// **Carried as a state field rather than a state subclass.** The reference subclasses its state
/// type per backend; here a state is one type whose backend-specific fields travel alongside the
/// modelled ones, so a host that does not know this backend still round-trips the field. What it
/// buys is that a state written by a newer build and read by an older one keeps its ownership
/// record instead of silently answering "no" and leaking a directory.
#[must_use]
pub fn workspace_root_owned(state: &SandboxSessionState) -> bool {
    state
        .field(WORKSPACE_ROOT_OWNED_FIELD)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}
