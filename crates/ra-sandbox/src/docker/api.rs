//! The Docker Engine operations the backend uses, as an interface a host supplies.
//!
//! The reference's client takes a `docker-py` client object and calls into it; its tests replace
//! that object with fakes that record calls and answer from a temporary directory. Here the same
//! seam is a trait. The implementation that talks to a real daemon is
//! [`super::BollardDockerApi`], behind this crate's `docker` feature; everything else in the backend
//! depends only on this trait, so it builds and is tested without a daemon or the client library.
//!
//! The operations are the ones the reference reaches for, named after the Engine API calls they
//! make, and shaped so that the reference's fakes translate one for one: container inspection
//! answers with the same attribute document `docker-py` exposes as `attrs`, and a one-shot command
//! answers with the demultiplexed streams and the possibly missing exit code of `exec_run`.

use std::collections::BTreeMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::io::AsyncWrite;

/// Why a Docker operation failed, as far as the backend needs to tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerApiError {
    kind: DockerApiErrorKind,
    status_code: Option<u16>,
    message: String,
}

/// The kinds of failure the backend branches on.
///
/// Not-found is its own kind because the backend treats it as an answer — the container is gone,
/// the image needs pulling, the volume was already removed — rather than as a failure.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerApiErrorKind {
    /// The daemon said the object does not exist.
    NotFound,
    /// The daemon answered with an error status.
    Api,
    /// The request never got an answer: the connection failed, broke, or timed out.
    Transport,
}

impl DockerApiError {
    /// A failure of the given kind.
    #[must_use]
    pub fn new(kind: DockerApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status_code: None,
            message: message.into(),
        }
    }

    /// The daemon's "no such object".
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(DockerApiErrorKind::NotFound, message).with_status_code(404)
    }

    /// An error status from the daemon.
    #[must_use]
    pub fn api(status_code: u16, message: impl Into<String>) -> Self {
        Self::new(DockerApiErrorKind::Api, message).with_status_code(status_code)
    }

    /// A request that never got an answer.
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(DockerApiErrorKind::Transport, message)
    }

    /// Records the HTTP status the daemon answered with.
    #[must_use]
    pub const fn with_status_code(mut self, status_code: u16) -> Self {
        self.status_code = Some(status_code);
        self
    }

    /// What kind of failure this is.
    #[must_use]
    pub const fn kind(&self) -> DockerApiErrorKind {
        self.kind
    }

    /// The HTTP status the daemon answered with, when it answered.
    #[must_use]
    pub const fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    /// Whether the daemon said the object does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.kind == DockerApiErrorKind::NotFound
    }
}

impl std::fmt::Display for DockerApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status_code {
            Some(status) => write!(formatter, "docker API error ({status}): {}", self.message),
            None => write!(formatter, "docker API error: {}", self.message),
        }
    }
}

impl std::error::Error for DockerApiError {}

/// What a container is created with.
///
/// The keyword arguments the reference hands `containers.create`, field for field, with the
/// optional ones absent exactly when the reference leaves them out. `detach=True` has no field: a
/// container created through the Engine API is always detached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerCreateSpec {
    entrypoint: Vec<String>,
    image: String,
    command: Vec<String>,
    environment: Option<BTreeMap<String, String>>,
    network_mode: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    mounts: Option<Vec<DockerMount>>,
    devices: Option<Vec<String>>,
    cap_add: Option<Vec<String>>,
    security_opt: Option<Vec<String>>,
    ports: Option<Vec<PublishedPort>>,
}

impl ContainerCreateSpec {
    /// A container from `image` that idles: entrypoint `tail`, arguments `-f /dev/null`, and
    /// nothing else set.
    #[must_use]
    pub fn idle(image: impl Into<String>) -> Self {
        Self {
            entrypoint: vec!["tail".to_owned()],
            image: image.into(),
            command: vec!["-f".to_owned(), "/dev/null".to_owned()],
            environment: None,
            network_mode: None,
            labels: None,
            mounts: None,
            devices: None,
            cap_add: None,
            security_opt: None,
            ports: None,
        }
    }

    /// Sets the environment; `None` is none resolved at all, which is not the same as an empty one.
    #[must_use]
    pub fn with_environment(mut self, environment: Option<BTreeMap<String, String>>) -> Self {
        self.environment = environment;
        self
    }

    /// Sets the network mode.
    #[must_use]
    pub fn with_network_mode(mut self, mode: impl Into<String>) -> Self {
        self.network_mode = Some(mode.into());
        self
    }

    /// Sets the labels.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// Sets the mounts.
    #[must_use]
    pub fn with_mounts(mut self, mounts: Vec<DockerMount>) -> Self {
        self.mounts = Some(mounts);
        self
    }

    /// Passes these host devices through.
    #[must_use]
    pub fn with_devices(mut self, devices: Vec<String>) -> Self {
        self.devices = Some(devices);
        self
    }

    /// Adds these capabilities.
    #[must_use]
    pub fn with_cap_add(mut self, capabilities: Vec<String>) -> Self {
        self.cap_add = Some(capabilities);
        self
    }

    /// Sets these security options.
    #[must_use]
    pub fn with_security_opt(mut self, options: Vec<String>) -> Self {
        self.security_opt = Some(options);
        self
    }

    /// Publishes these ports.
    #[must_use]
    pub fn with_ports(mut self, ports: Vec<PublishedPort>) -> Self {
        self.ports = Some(ports);
        self
    }

    /// The entrypoint: `["tail"]`, so the container idles until something is run in it.
    #[must_use]
    pub fn entrypoint(&self) -> &[String] {
        &self.entrypoint
    }

    /// The image reference.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// The entrypoint's arguments: `["-f", "/dev/null"]`.
    #[must_use]
    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// The environment, or `None` when there was no manifest to resolve one from.
    #[must_use]
    pub const fn environment(&self) -> Option<&BTreeMap<String, String>> {
        self.environment.as_ref()
    }

    /// The network mode, only when one was chosen.
    #[must_use]
    pub fn network_mode(&self) -> Option<&str> {
        self.network_mode.as_deref()
    }

    /// Labels, only when there are any.
    #[must_use]
    pub const fn labels(&self) -> Option<&BTreeMap<String, String>> {
        self.labels.as_ref()
    }

    /// Bind and volume mounts, only when there are any.
    #[must_use]
    pub fn mounts(&self) -> Option<&[DockerMount]> {
        self.mounts.as_deref()
    }

    /// Devices passed through, by host path.
    #[must_use]
    pub fn devices(&self) -> Option<&[String]> {
        self.devices.as_deref()
    }

    /// Capabilities added.
    #[must_use]
    pub fn cap_add(&self) -> Option<&[String]> {
        self.cap_add.as_deref()
    }

    /// Security options.
    #[must_use]
    pub fn security_opt(&self) -> Option<&[String]> {
        self.security_opt.as_deref()
    }

    /// Published ports, in the order they were configured.
    #[must_use]
    pub fn ports(&self) -> Option<&[PublishedPort]> {
        self.ports.as_deref()
    }
}

/// One published container port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    container_port: String,
    host_ip: String,
    host_port: Option<u16>,
}

impl PublishedPort {
    /// Publishes `container_port` (as `<port>/tcp`) on `host_ip`, at `host_port` or at one the
    /// daemon picks.
    #[must_use]
    pub fn new(
        container_port: impl Into<String>,
        host_ip: impl Into<String>,
        host_port: Option<u16>,
    ) -> Self {
        Self {
            container_port: container_port.into(),
            host_ip: host_ip.into(),
            host_port,
        }
    }

    /// The container side, as `<port>/tcp`.
    #[must_use]
    pub fn container_port(&self) -> &str {
        &self.container_port
    }

    /// The host address to bind on.
    #[must_use]
    pub fn host_ip(&self) -> &str {
        &self.host_ip
    }

    /// The host port, or `None` to let the daemon choose one.
    #[must_use]
    pub const fn host_port(&self) -> Option<u16> {
        self.host_port
    }
}

/// One mount a container is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerMount {
    target: String,
    source: String,
    kind: DockerMountKind,
    read_only: bool,
    driver_config: Option<DockerDriverConfig>,
}

impl DockerMount {
    /// A host path bound at `target`.
    #[must_use]
    pub fn bind(target: impl Into<String>, source: impl Into<String>, read_only: bool) -> Self {
        Self {
            target: target.into(),
            source: source.into(),
            kind: DockerMountKind::Bind,
            read_only,
            driver_config: None,
        }
    }

    /// A named volume attached at `target`, created through `driver` when it has one.
    #[must_use]
    pub fn volume(
        target: impl Into<String>,
        name: impl Into<String>,
        read_only: bool,
        driver: Option<DockerDriverConfig>,
    ) -> Self {
        Self {
            target: target.into(),
            source: name.into(),
            kind: DockerMountKind::Volume,
            read_only,
            driver_config: driver,
        }
    }

    /// Where it appears inside the container.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The host path of a bind mount, or the name of a volume.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Which of the two it is.
    #[must_use]
    pub const fn kind(&self) -> DockerMountKind {
        self.kind
    }

    /// Whether the container may only read through it.
    #[must_use]
    pub const fn read_only(&self) -> bool {
        self.read_only
    }

    /// The volume driver and its options, for a volume that has one.
    #[must_use]
    pub const fn driver_config(&self) -> Option<&DockerDriverConfig> {
        self.driver_config.as_ref()
    }
}

/// Whether a mount is a host path or a volume.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerMountKind {
    /// A host path bound into the container.
    Bind,
    /// A named volume, created through its driver.
    Volume,
}

/// A volume driver and what it is told.
#[derive(Clone, PartialEq, Eq)]
pub struct DockerDriverConfig {
    name: String,
    options: BTreeMap<String, String>,
}

impl DockerDriverConfig {
    /// Names a driver and its options.
    #[must_use]
    pub fn new(name: impl Into<String>, options: BTreeMap<String, String>) -> Self {
        Self {
            name: name.into(),
            options,
        }
    }

    /// The driver's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its options, which carry credentials.
    #[must_use]
    pub const fn options(&self) -> &BTreeMap<String, String> {
        &self.options
    }
}

impl std::fmt::Debug for DockerDriverConfig {
    /// Names the option keys, never their values: they are the storage's credentials.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerDriverConfig")
            .field("name", &self.name)
            .field("options", &self.options.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A command run to completion inside a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRunRequest {
    cmd: Vec<String>,
    workdir: Option<String>,
    user: Option<String>,
}

impl ExecRunRequest {
    /// Runs `cmd` as written, in the image's default directory, as the container's default account.
    #[must_use]
    pub fn new(cmd: Vec<String>) -> Self {
        Self {
            cmd,
            workdir: None,
            user: None,
        }
    }

    /// Runs it in `workdir`, or the default for `None`.
    #[must_use]
    pub fn in_dir(mut self, workdir: Option<String>) -> Self {
        self.workdir = workdir;
        self
    }

    /// Runs it as `user`, or the default for `None`.
    #[must_use]
    pub fn as_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    /// The argument vector, run as written.
    #[must_use]
    pub fn cmd(&self) -> &[String] {
        &self.cmd
    }

    /// The directory it runs in, or `None` for the image's default.
    #[must_use]
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }

    /// The account it runs as, or `None` for the container's default.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
}

/// What a one-shot command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecRunOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i64>,
}

impl ExecRunOutput {
    /// A command's two streams and its exit status, which the daemon does not always report.
    #[must_use]
    pub const fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: Option<i64>) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
        }
    }

    /// Its standard output.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Its standard error.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Its exit status, when the daemon reported one.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i64> {
        self.exit_code
    }

    /// The two streams and the exit status, by value.
    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, Vec<u8>, Option<i64>) {
        (self.stdout, self.stderr, self.exit_code)
    }
}

/// A command set up to be started and attached to.
///
/// Four flags because the Engine API's exec creation takes four, and `docker-py`'s `exec_create`
/// passes them through one for one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCreateRequest {
    cmd: Vec<String>,
    stdin: bool,
    stdout: bool,
    stderr: bool,
    tty: bool,
    workdir: Option<String>,
    user: Option<String>,
}

impl ExecCreateRequest {
    /// Sets `cmd` up with its output attached and its input not, without a terminal, in the image's
    /// default directory, as the container's default account.
    #[must_use]
    pub fn new(cmd: Vec<String>) -> Self {
        Self {
            cmd,
            stdin: false,
            stdout: true,
            stderr: true,
            tty: false,
            workdir: None,
            user: None,
        }
    }

    /// Attaches its standard input, or not.
    #[must_use]
    pub const fn with_stdin(mut self, stdin: bool) -> Self {
        self.stdin = stdin;
        self
    }

    /// Runs it on a terminal, or not.
    #[must_use]
    pub const fn with_tty(mut self, tty: bool) -> Self {
        self.tty = tty;
        self
    }

    /// Runs it in `workdir`, or the default for `None`.
    #[must_use]
    pub fn in_dir(mut self, workdir: Option<String>) -> Self {
        self.workdir = workdir;
        self
    }

    /// Runs it as `user`, or the default for `None`.
    #[must_use]
    pub fn as_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    /// The argument vector, run as written.
    #[must_use]
    pub fn cmd(&self) -> &[String] {
        &self.cmd
    }

    /// Whether its standard input is attached.
    #[must_use]
    pub const fn stdin(&self) -> bool {
        self.stdin
    }

    /// Whether its standard output is attached.
    #[must_use]
    pub const fn stdout(&self) -> bool {
        self.stdout
    }

    /// Whether its standard error is attached.
    #[must_use]
    pub const fn stderr(&self) -> bool {
        self.stderr
    }

    /// Whether it runs on a terminal.
    #[must_use]
    pub const fn tty(&self) -> bool {
        self.tty
    }

    /// The directory it runs in, or `None` for the image's default.
    #[must_use]
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }

    /// The account it runs as, or `None` for the container's default.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
}

/// Which stream a piece of attached output came from.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStreamKind {
    /// Standard output of a command without a terminal.
    Stdout,
    /// Standard error of a command without a terminal.
    Stderr,
    /// The terminal of a command that has one, where the two are not told apart.
    Console,
}

/// One piece of attached output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecFrame {
    stream: ExecStreamKind,
    data: Vec<u8>,
}

impl ExecFrame {
    /// `data`, from `stream`.
    #[must_use]
    pub const fn new(stream: ExecStreamKind, data: Vec<u8>) -> Self {
        Self { stream, data }
    }

    /// Where it came from.
    #[must_use]
    pub const fn stream(&self) -> ExecStreamKind {
        self.stream
    }

    /// What it said.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// A started command's output, demultiplexed, until its streams close.
pub type ExecOutputStream = BoxStream<'static, Result<ExecFrame, DockerApiError>>;

/// A started command's standard input.
pub type ExecInput = Pin<Box<dyn AsyncWrite + Send>>;

/// A started command's output and, when attached, its input.
pub struct ExecAttachment {
    output: ExecOutputStream,
    input: ExecInput,
}

impl ExecAttachment {
    /// A command's demultiplexed output, read until its streams close, and its standard input.
    ///
    /// Shutting the input down is how end-of-input is signalled — over transports that carry a
    /// half-close.
    #[must_use]
    pub fn new(output: ExecOutputStream, input: ExecInput) -> Self {
        Self { output, input }
    }

    /// The output stream and the input, by value.
    #[must_use]
    pub fn into_parts(self) -> (ExecOutputStream, ExecInput) {
        (self.output, self.input)
    }
}

impl std::fmt::Debug for ExecAttachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecAttachment")
            .finish_non_exhaustive()
    }
}

/// Whether a started command is still running, and how it ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecInspect {
    running: bool,
    exit_code: Option<i64>,
}

impl ExecInspect {
    /// Whether it is running, and its exit status once it has one.
    #[must_use]
    pub const fn new(running: bool, exit_code: Option<i64>) -> Self {
        Self { running, exit_code }
    }

    /// Whether it is still running.
    #[must_use]
    pub const fn running(&self) -> bool {
        self.running
    }

    /// Its exit status, once it has one.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i64> {
        self.exit_code
    }
}

/// The Docker Engine operations the backend is built on.
#[async_trait]
pub trait DockerApi: Send + Sync {
    /// Whether an image is present, answered by inspecting it.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when it is not, and the daemon's failure otherwise.
    async fn inspect_image(&self, image: &str) -> Result<(), DockerApiError>;

    /// Pulls an image; `tag` is the part after the last `:` or `@` of the reference, if any.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to pull.
    async fn pull_image(&self, repository: &str, tag: Option<&str>) -> Result<(), DockerApiError>;

    /// Creates a container and returns its id.
    ///
    /// # Errors
    ///
    /// Returns the daemon's refusal.
    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, DockerApiError>;

    /// Starts a created or stopped container.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to start it.
    async fn start_container(&self, id: &str) -> Result<(), DockerApiError>;

    /// Stops a running container.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to stop it.
    async fn stop_container(&self, id: &str) -> Result<(), DockerApiError>;

    /// Removes a container; `force` removes it even while it runs.
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a container that is gone, and the daemon's failure otherwise.
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerApiError>;

    /// A container's attribute document: the Engine API's inspect response, as `docker-py`'s
    /// `Container.attrs` holds it.
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a container that is gone, and the daemon's failure otherwise.
    async fn inspect_container(&self, id: &str) -> Result<serde_json::Value, DockerApiError>;

    /// Whether a volume exists, answered by inspecting it.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when it does not, and the daemon's failure otherwise.
    async fn inspect_volume(&self, name: &str) -> Result<(), DockerApiError>;

    /// Removes a volume.
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a volume that is gone, and the daemon's failure otherwise.
    async fn remove_volume(&self, name: &str) -> Result<(), DockerApiError>;

    /// Sets up a command inside a container and returns the exec id.
    ///
    /// # Errors
    ///
    /// Returns the daemon's refusal.
    async fn exec_create(
        &self,
        container_id: &str,
        request: &ExecCreateRequest,
    ) -> Result<String, DockerApiError>;

    /// Starts a command set up with [`Self::exec_create`] and attaches to it.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to start or attach.
    async fn exec_start(&self, exec_id: &str, tty: bool) -> Result<ExecAttachment, DockerApiError>;

    /// Whether a started command is still running, and its exit status.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to say.
    async fn exec_inspect(&self, exec_id: &str) -> Result<ExecInspect, DockerApiError>;

    /// Runs a command to completion and collects its output: `docker-py`'s `exec_run` with
    /// `demux=True`.
    ///
    /// The default sets the command up with output attached, reads every frame into the stream it
    /// came from, and then asks for the exit status — which, as in `docker-py`, the daemon may not
    /// have yet, and is then reported missing rather than guessed.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure at any of the three steps.
    async fn exec_run(
        &self,
        container_id: &str,
        request: &ExecRunRequest,
    ) -> Result<ExecRunOutput, DockerApiError> {
        let exec_id = self
            .exec_create(
                container_id,
                &ExecCreateRequest::new(request.cmd.clone())
                    .in_dir(request.workdir.clone())
                    .as_user(request.user.clone()),
            )
            .await?;
        let (mut frames, _input) = self.exec_start(&exec_id, false).await?.into_parts();
        let mut output = ExecRunOutput::default();
        while let Some(frame) = frames.next().await {
            let frame = frame?;
            match frame.stream {
                ExecStreamKind::Stderr => output.stderr.extend_from_slice(&frame.data),
                ExecStreamKind::Stdout | ExecStreamKind::Console => {
                    output.stdout.extend_from_slice(&frame.data);
                }
            }
        }
        output.exit_code = self.exec_inspect(&exec_id).await?.exit_code;
        Ok(output)
    }

    /// Reads a path out of a container as a tar archive, rooted at the path's own name.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure to produce the archive.
    async fn get_archive(&self, container_id: &str, path: &str) -> Result<Vec<u8>, DockerApiError>;
}
