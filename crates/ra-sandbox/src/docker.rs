//! The container backend: the workspace lives inside a Docker container, and commands run there.
//!
//! The reference's `sandboxes/docker.py`, carried over type for type: a client that creates,
//! resumes and deletes containers, a session that runs commands and moves files through them, the
//! options a run configuration selects the backend with, and the state a paused run resumes from.
//! A run switches to this backend by handing its run configuration a different client; the agent
//! definition does not change.
//!
//! # What the container is, and what it is not
//!
//! One container per session, created from the options' image with an entrypoint that idles, so
//! that every command is an `exec` into it. Nothing here makes it a hardened boundary beyond what
//! Docker itself provides: `network_mode="none"` is the one network setting the reference offers,
//! and it takes the container off every network — which is not the same as the local backend's
//! optional socket fence, and neither implies the other.
//!
//! # The daemon is behind an interface
//!
//! The client is handed a [`DockerApi`], as the reference's is handed a `docker-py` client. The
//! implementation that talks to a daemon, [`BollardDockerApi`], is behind this crate's `docker`
//! feature; tests hand in fakes, as the reference's do.

mod api;
#[cfg(feature = "docker")]
mod bollard_api;
mod client;
mod container;
#[cfg(feature = "docker")]
mod raw_attach;
mod session;
mod stream;

use std::collections::BTreeMap;

pub use api::{
    ContainerCreateSpec, DockerApi, DockerApiError, DockerApiErrorKind, DockerDriverConfig,
    DockerMount, DockerMountKind, ExecAttachment, ExecCreateRequest, ExecFrame, ExecInput,
    ExecInspect, ExecOutputStream, ExecRunOutput, ExecRunRequest, ExecStreamKind, PublishedPort,
};
#[cfg(feature = "docker")]
pub use bollard_api::BollardDockerApi;
pub use client::DockerSandboxClient;
// `Internal`: helpers the reference keeps private (`_docker_volume_name`, `_manifest_requires_fuse`
// and the rest), public only so the separate test workspace can reach them, with no compatibility
// promise.
#[doc(hidden)]
pub use container::{
    docker_volume_name, docker_volume_names_for_manifest, manifest_requires_fuse,
    manifest_requires_sys_admin, parse_repository_tag,
};
pub use session::DockerSandboxSession;
#[doc(hidden)]
pub use stream::LENGTH_FRAMED_STDIN_SCRIPT;

use ra_core::sandbox::{
    DiscriminatedPayload, ErrorCode, OpName, RegistryError, SandboxError, SandboxSessionState,
    TypeRegistry, normalize_exposed_ports,
};
use serde_json::Value;

/// The discriminator this backend's options, states and sessions carry.
pub const DOCKER_BACKEND_ID: &str = "docker";

/// The image the reference's examples and tests run: a slim Python.
///
/// Not a default — the options require an image — but the one a caller that has no reason to pick
/// another is pointed at.
pub const DEFAULT_PYTHON_SANDBOX_IMAGE: &str = "python:3.14-slim";

/// The state field naming the image the container was created from.
pub(crate) const IMAGE_FIELD: &str = "image";
/// The state field naming the container.
pub(crate) const CONTAINER_ID_FIELD: &str = "container_id";
/// The state field recording the network mode.
pub(crate) const NETWORK_MODE_FIELD: &str = "network_mode";
/// The state field recording the labels.
pub(crate) const LABELS_FIELD: &str = "labels";

/// The network modes a Docker session may be configured with.
///
/// One, as in the reference: its field is `Literal["none"] | None`. Leaving the mode unset is the
/// daemon's default network, which is what a caller that says nothing gets.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerNetworkMode {
    /// No network at all: the container is attached to nothing.
    None,
}

impl DockerNetworkMode {
    /// The mode's wire string, which is also what the daemon is told.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
        }
    }

    /// Reads a persisted mode: `null` is unset, `"none"` is [`Self::None`], anything else is refused.
    fn from_json(value: Option<&Value>) -> Result<Option<Self>, DockerFieldError> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(mode)) if mode == "none" => Ok(Some(Self::None)),
            Some(_) => Err(DockerFieldError::NetworkMode),
        }
    }
}

/// Why a Docker field could not be read or combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockerFieldError {
    /// The image is missing or not a string.
    Image,
    /// The container id is missing or not a string.
    ContainerId,
    /// The network mode is something other than unset or `"none"`.
    NetworkMode,
    /// The labels are not a map of strings.
    Labels,
    /// Ports are published on a container with no network.
    PortsWithoutNetwork,
}

impl DockerFieldError {
    fn message(self) -> &'static str {
        match self {
            Self::Image => "docker `image` must be a string",
            Self::ContainerId => "docker `container_id` must be a string",
            Self::NetworkMode => "docker `network_mode` must be \"none\" or null",
            Self::Labels => "docker `labels` must map strings to strings",
            Self::PortsWithoutNetwork => "exposed_ports cannot be used when network_mode='none'",
        }
    }
}

/// Refuses a published port on a container that has no network to publish it on.
///
/// The reference's `_validate_docker_network_configuration`, run by both the options and the state.
fn validate_network_configuration(
    network_mode: Option<DockerNetworkMode>,
    exposed_ports: &[u16],
) -> Result<(), DockerFieldError> {
    if network_mode == Some(DockerNetworkMode::None) && !exposed_ports.is_empty() {
        return Err(DockerFieldError::PortsWithoutNetwork);
    }
    Ok(())
}

/// Reads labels: absent or `null` is none, otherwise every value must be a string.
fn labels_from_json(value: Option<&Value>) -> Result<BTreeMap<String, String>, DockerFieldError> {
    match value {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(fields)) => fields
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => Ok((key.clone(), text.clone())),
                _ => Err(DockerFieldError::Labels),
            })
            .collect(),
        Some(_) => Err(DockerFieldError::Labels),
    }
}

/// A configuration error, with the backend named.
fn config_invalid(message: impl Into<String>) -> SandboxError {
    SandboxError::new(ErrorCode::SandboxConfigInvalid, OpName::Start, message)
        .with_context("backend", DOCKER_BACKEND_ID)
}

/// What a caller configures when it asks this backend for a session.
///
/// The reference's `DockerSandboxClientOptions`: an image, which is required, the ports to publish,
/// the network mode and the labels. Publishing a port on a container with no network is refused
/// wherever the two meet — here, and again when a state is read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSandboxClientOptions {
    image: String,
    exposed_ports: Vec<u16>,
    network_mode: Option<DockerNetworkMode>,
    labels: BTreeMap<String, String>,
}

impl DockerSandboxClientOptions {
    /// Asks for a container from `image`, on the daemon's default network, publishing nothing.
    #[must_use]
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            exposed_ports: Vec::new(),
            network_mode: None,
            labels: BTreeMap::new(),
        }
    }

    /// Publishes these ports, each on the host's loopback at a port the daemon picks.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] for port 0, or when the network mode is `none`.
    pub fn with_exposed_ports(
        mut self,
        ports: impl IntoIterator<Item = u16>,
    ) -> Result<Self, SandboxError> {
        let mut normalized = Vec::new();
        for port in ports {
            if port < 1 {
                return Err(config_invalid(
                    ra_core::sandbox::ExposedPortsError::OutOfRange.to_string(),
                ));
            }
            if !normalized.contains(&port) {
                normalized.push(port);
            }
        }
        validate_network_configuration(self.network_mode, &normalized)
            .map_err(|error| config_invalid(error.message()))?;
        self.exposed_ports = normalized;
        Ok(self)
    }

    /// Runs the container with `mode`, or on the default network for `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when ports are already published and the mode is
    /// `none`.
    pub fn with_network_mode(
        mut self,
        mode: Option<DockerNetworkMode>,
    ) -> Result<Self, SandboxError> {
        validate_network_configuration(mode, &self.exposed_ports)
            .map_err(|error| config_invalid(error.message()))?;
        self.network_mode = mode;
        Ok(self)
    }

    /// Labels the container with these.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// The image the container is created from.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// The ports the container publishes.
    #[must_use]
    pub fn exposed_ports(&self) -> &[u16] {
        &self.exposed_ports
    }

    /// The network mode, or `None` for the daemon's default.
    #[must_use]
    pub const fn network_mode(&self) -> Option<DockerNetworkMode> {
        self.network_mode
    }

    /// The labels the container carries.
    #[must_use]
    pub const fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// Renders the options as the routed payload a run configuration carries.
    ///
    /// Every field is written, `network_mode` as `null` when unset: the reference's dump.
    #[must_use]
    pub fn to_payload(&self) -> DiscriminatedPayload {
        DiscriminatedPayload::new(DOCKER_BACKEND_ID)
            .with_field(IMAGE_FIELD, self.image.clone())
            .with_field("exposed_ports", self.exposed_ports.clone())
            .with_field(
                NETWORK_MODE_FIELD,
                self.network_mode
                    .map_or(Value::Null, |mode| Value::from(mode.as_str())),
            )
            .with_field(
                LABELS_FIELD,
                Value::Object(
                    self.labels
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::from(value.clone())))
                        .collect(),
                ),
            )
    }

    /// Reads options out of a payload addressed to this backend.
    ///
    /// A payload without `network_mode` or `labels` reads as unset and empty, so options written
    /// before either field existed still read.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the payload names another backend, lacks an
    /// image, or carries a port list, network mode or labels that do not read — including a port
    /// published on a container with no network.
    pub fn from_payload(payload: &DiscriminatedPayload) -> Result<Self, SandboxError> {
        if payload.type_name() != DOCKER_BACKEND_ID {
            return Err(config_invalid(format!(
                "sandbox client options for `{}` cannot configure the `{DOCKER_BACKEND_ID}` backend",
                payload.type_name()
            ))
            .with_context("options_type", payload.type_name()));
        }
        let image = match payload.field(IMAGE_FIELD) {
            Some(Value::String(image)) => image.clone(),
            _ => return Err(config_invalid(DockerFieldError::Image.message())),
        };
        let exposed_ports = payload
            .field("exposed_ports")
            .map_or_else(|| Ok(Vec::new()), normalize_exposed_ports)
            .map_err(|error| config_invalid(error.to_string()))?;
        let network_mode = DockerNetworkMode::from_json(payload.field(NETWORK_MODE_FIELD))
            .map_err(|error| config_invalid(error.message()))?;
        let labels = labels_from_json(payload.field(LABELS_FIELD))
            .map_err(|error| config_invalid(error.message()))?;
        validate_network_configuration(network_mode, &exposed_ports)
            .map_err(|error| config_invalid(error.message()))?;
        Ok(Self {
            image,
            exposed_ports,
            network_mode,
            labels,
        })
    }

    /// Registers this backend's options with a host's routing table.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::AlreadyRegistered`] when something else already claimed the name.
    pub fn register(registry: &mut TypeRegistry) -> Result<(), RegistryError> {
        registry.register(DOCKER_BACKEND_ID, "ra-sandbox::docker")
    }
}

/// The Docker half of a session state: which image, which container, and how it was configured.
///
/// **A view over state fields rather than a state subclass**, as the local backend's ownership flag
/// is: the reference subclasses its state per backend, and here the backend-specific fields travel
/// alongside the modelled ones, so a host that does not know this backend still round-trips them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerStateFields {
    image: String,
    container_id: String,
    network_mode: Option<DockerNetworkMode>,
    labels: BTreeMap<String, String>,
}

impl DockerStateFields {
    /// A container from `image`, named `container_id`, on the default network, without labels.
    #[must_use]
    pub fn new(image: impl Into<String>, container_id: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            container_id: container_id.into(),
            network_mode: None,
            labels: BTreeMap::new(),
        }
    }

    /// Records the network mode, or the default for `None`.
    #[must_use]
    pub const fn with_network_mode(mut self, mode: Option<DockerNetworkMode>) -> Self {
        self.network_mode = mode;
        self
    }

    /// Records the labels.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// The image the container was created from.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// The container's id; empty when the persisted identity was scrubbed.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// The network mode, or `None` for the daemon's default.
    #[must_use]
    pub const fn network_mode(&self) -> Option<DockerNetworkMode> {
        self.network_mode
    }

    /// The labels the container carries.
    #[must_use]
    pub const fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// Reads the Docker fields of a state, and checks them against its exposed ports.
    ///
    /// Missing `network_mode` and `labels` read as unset and empty, as a state written before
    /// either existed should.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the image or container id is missing, when a
    /// field does not read, or when ports are published on a container with no network.
    pub fn read(state: &SandboxSessionState) -> Result<Self, SandboxError> {
        Self::read_fields(state).map_err(|error| config_invalid(error.message()))
    }

    fn read_fields(state: &SandboxSessionState) -> Result<Self, DockerFieldError> {
        let image = match state.field(IMAGE_FIELD) {
            Some(Value::String(image)) => image.clone(),
            _ => return Err(DockerFieldError::Image),
        };
        let container_id = match state.field(CONTAINER_ID_FIELD) {
            Some(Value::String(id)) => id.clone(),
            _ => return Err(DockerFieldError::ContainerId),
        };
        let network_mode = DockerNetworkMode::from_json(state.field(NETWORK_MODE_FIELD))?;
        let labels = labels_from_json(state.field(LABELS_FIELD))?;
        validate_network_configuration(network_mode, state.exposed_ports())?;
        Ok(Self {
            image,
            container_id,
            network_mode,
            labels,
        })
    }

    /// Writes these fields onto a state, replacing whatever it carried.
    #[must_use]
    pub fn apply(&self, state: SandboxSessionState) -> SandboxSessionState {
        state
            .with_persisted_identity_redaction(
                [CONTAINER_ID_FIELD],
                "openai-agents:mount-authority-redacted:",
            )
            .with_field(IMAGE_FIELD, self.image.clone())
            .with_field(CONTAINER_ID_FIELD, self.container_id.clone())
            .with_field(
                NETWORK_MODE_FIELD,
                self.network_mode
                    .map_or(Value::Null, |mode| Value::from(mode.as_str())),
            )
            .with_field(
                LABELS_FIELD,
                Value::Object(
                    self.labels
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::from(value.clone())))
                        .collect(),
                ),
            )
    }
}
