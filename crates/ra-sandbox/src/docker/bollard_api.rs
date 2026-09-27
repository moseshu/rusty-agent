//! [`DockerApi`] over a real daemon, through the `bollard` Engine API client.
//!
//! Each operation is the Engine API call `docker-py` makes for the same method, with the arguments
//! translated the way `docker-py` translates them: an environment map becomes `KEY=value` strings, a
//! published port becomes an exposed port plus a binding with an empty host port, a device path
//! becomes a mapping with `rwm` permissions.

use std::collections::HashMap;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, HostConfig, Mount, MountType, MountVolumeOptions,
    MountVolumeOptionsDriverConfig, PortBinding,
};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder, RemoveContainerOptionsBuilder,
};
use futures::{StreamExt, TryStreamExt};

use super::api::{
    ContainerCreateSpec, DockerApi, DockerApiError, DockerMountKind, ExecAttachment,
    ExecCreateRequest, ExecFrame, ExecInspect, ExecStreamKind,
};

/// Talks to a Docker daemon.
#[derive(Debug, Clone)]
pub struct BollardDockerApi {
    docker: Docker,
}

impl BollardDockerApi {
    /// Connects the way `docker.from_env()` does: through `DOCKER_HOST`, with TLS when
    /// `DOCKER_TLS_VERIFY` is set, and through the platform's default socket otherwise.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the address cannot be used. Nothing is sent to the daemon
    /// yet, so an address that parses but has no daemon behind it fails on first use instead.
    pub fn connect_with_defaults() -> Result<Self, DockerApiError> {
        Docker::connect_with_defaults()
            .map(Self::from_client)
            .map_err(|error| DockerApiError::transport(error.to_string()))
    }

    /// Wraps a client the host configured itself.
    #[must_use]
    pub const fn from_client(docker: Docker) -> Self {
        Self { docker }
    }

    /// The client underneath.
    #[must_use]
    pub const fn client(&self) -> &Docker {
        &self.docker
    }
}

/// Classifies a client error: the daemon's own answers keep their status, 404 is not-found, and
/// everything that never reached an answer is transport.
fn classify(error: BollardError) -> DockerApiError {
    match error {
        BollardError::DockerResponseServerError {
            status_code,
            message,
        } => {
            if status_code == 404 {
                DockerApiError::not_found(message)
            } else {
                DockerApiError::api(status_code, message)
            }
        }
        other => DockerApiError::transport(other.to_string()),
    }
}

/// Translates the backend's create arguments into the Engine API's request body.
fn create_body(spec: &ContainerCreateSpec) -> ContainerCreateBody {
    let owned = |items: Option<&[String]>| items.map(<[String]>::to_vec);
    let mut host_config = HostConfig {
        network_mode: spec.network_mode().map(str::to_owned),
        cap_add: owned(spec.cap_add()),
        security_opt: owned(spec.security_opt()),
        ..HostConfig::default()
    };
    if let Some(devices) = spec.devices() {
        host_config.devices = Some(
            devices
                .iter()
                .map(|device| DeviceMapping {
                    path_on_host: Some(device.clone()),
                    path_in_container: Some(device.clone()),
                    cgroup_permissions: Some("rwm".to_owned()),
                })
                .collect(),
        );
    }
    if let Some(mounts) = spec.mounts() {
        host_config.mounts = Some(
            mounts
                .iter()
                .map(|mount| Mount {
                    target: Some(mount.target().to_owned()),
                    source: Some(mount.source().to_owned()),
                    typ: Some(match mount.kind() {
                        DockerMountKind::Bind => MountType::BIND,
                        _ => MountType::VOLUME,
                    }),
                    read_only: Some(mount.read_only()),
                    volume_options: mount.driver_config().map(|driver| MountVolumeOptions {
                        driver_config: Some(MountVolumeOptionsDriverConfig {
                            name: Some(driver.name().to_owned()),
                            options: Some(
                                driver
                                    .options()
                                    .iter()
                                    .map(|(key, value)| (key.clone(), value.clone()))
                                    .collect(),
                            ),
                        }),
                        ..MountVolumeOptions::default()
                    }),
                    ..Mount::default()
                })
                .collect(),
        );
    }
    let mut exposed_ports = None;
    if let Some(ports) = spec.ports() {
        exposed_ports = Some(
            ports
                .iter()
                .map(|port| port.container_port().to_owned())
                .collect(),
        );
        host_config.port_bindings = Some(
            ports
                .iter()
                .map(|port| {
                    (
                        port.container_port().to_owned(),
                        Some(vec![PortBinding {
                            host_ip: Some(port.host_ip().to_owned()),
                            host_port: Some(
                                port.host_port()
                                    .map_or_else(String::new, |port| port.to_string()),
                            ),
                        }]),
                    )
                })
                .collect::<HashMap<_, _>>(),
        );
    }
    ContainerCreateBody {
        image: Some(spec.image().to_owned()),
        entrypoint: Some(spec.entrypoint().to_vec()),
        cmd: Some(spec.command().to_vec()),
        env: spec.environment().map(|environment| {
            environment
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect()
        }),
        labels: spec
            .labels()
            .map(|labels| labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        exposed_ports,
        host_config: Some(host_config),
        ..ContainerCreateBody::default()
    }
}

#[async_trait]
impl DockerApi for BollardDockerApi {
    async fn inspect_image(&self, image: &str) -> Result<(), DockerApiError> {
        self.docker
            .inspect_image(image)
            .await
            .map(|_| ())
            .map_err(classify)
    }

    async fn pull_image(&self, repository: &str, tag: Option<&str>) -> Result<(), DockerApiError> {
        // A digest is pulled as `repository@digest`, as `docker-py` pulls it.
        let options = match tag {
            Some(digest) if digest.starts_with("sha256:") => CreateImageOptionsBuilder::new()
                .from_image(&format!("{repository}@{digest}"))
                .build(),
            Some(tag) => CreateImageOptionsBuilder::new()
                .from_image(repository)
                .tag(tag)
                .build(),
            None => CreateImageOptionsBuilder::new()
                .from_image(repository)
                .tag("latest")
                .build(),
        };
        self.docker
            .create_image(Some(options), None, None)
            .try_for_each(|_| async { Ok(()) })
            .await
            .map_err(classify)
    }

    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, DockerApiError> {
        self.docker
            .create_container(None, create_body(spec))
            .await
            .map(|created| created.id)
            .map_err(classify)
    }

    async fn start_container(&self, id: &str) -> Result<(), DockerApiError> {
        self.docker
            .start_container(id, None)
            .await
            .map_err(classify)
    }

    async fn stop_container(&self, id: &str) -> Result<(), DockerApiError> {
        self.docker.stop_container(id, None).await.map_err(classify)
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerApiError> {
        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptionsBuilder::new().force(force).build()),
            )
            .await
            .map_err(classify)
    }

    async fn inspect_container(&self, id: &str) -> Result<serde_json::Value, DockerApiError> {
        let inspected = self
            .docker
            .inspect_container(id, None)
            .await
            .map_err(classify)?;
        serde_json::to_value(inspected)
            .map_err(|error| DockerApiError::transport(error.to_string()))
    }

    async fn inspect_volume(&self, name: &str) -> Result<(), DockerApiError> {
        self.docker
            .inspect_volume(name)
            .await
            .map(|_| ())
            .map_err(classify)
    }

    async fn remove_volume(&self, name: &str) -> Result<(), DockerApiError> {
        self.docker
            .remove_volume(name, None::<bollard::query_parameters::RemoveVolumeOptions>)
            .await
            .map_err(classify)
    }

    async fn exec_create(
        &self,
        container_id: &str,
        request: &ExecCreateRequest,
    ) -> Result<String, DockerApiError> {
        self.docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(request.cmd().to_vec()),
                    attach_stdin: Some(request.stdin()),
                    attach_stdout: Some(request.stdout()),
                    attach_stderr: Some(request.stderr()),
                    tty: Some(request.tty()),
                    working_dir: request.workdir().map(str::to_owned),
                    user: request.user().map(str::to_owned),
                    ..CreateExecOptions::default()
                },
            )
            .await
            .map(|created| created.id)
            .map_err(classify)
    }

    async fn exec_start(&self, exec_id: &str, tty: bool) -> Result<ExecAttachment, DockerApiError> {
        let started = self
            .docker
            .start_exec(
                exec_id,
                Some(StartExecOptions {
                    detach: false,
                    tty,
                    output_capacity: None,
                }),
            )
            .await
            .map_err(classify)?;
        let StartExecResults::Attached { output, input } = started else {
            return Err(DockerApiError::transport(
                "the daemon started the exec detached",
            ));
        };
        let output = output
            .filter_map(|frame| async move {
                match frame {
                    Ok(LogOutput::StdOut { message }) => {
                        Some(Ok(ExecFrame::new(ExecStreamKind::Stdout, message.to_vec())))
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        Some(Ok(ExecFrame::new(ExecStreamKind::Stderr, message.to_vec())))
                    }
                    Ok(LogOutput::Console { message }) => Some(Ok(ExecFrame::new(
                        ExecStreamKind::Console,
                        message.to_vec(),
                    ))),
                    Ok(LogOutput::StdIn { .. }) => None,
                    Err(error) => Some(Err(classify(error))),
                }
            })
            .boxed();
        Ok(ExecAttachment::new(output, input))
    }

    async fn exec_inspect(&self, exec_id: &str) -> Result<ExecInspect, DockerApiError> {
        let inspected = self.docker.inspect_exec(exec_id).await.map_err(classify)?;
        Ok(ExecInspect::new(
            inspected.running.unwrap_or(false),
            inspected.exit_code,
        ))
    }

    async fn get_archive(&self, container_id: &str, path: &str) -> Result<Vec<u8>, DockerApiError> {
        let chunks: Vec<_> = self
            .docker
            .download_from_container(
                container_id,
                Some(
                    DownloadFromContainerOptionsBuilder::new()
                        .path(path)
                        .build(),
                ),
            )
            .try_collect()
            .await
            .map_err(classify)?;
        Ok(chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect())
    }
}
