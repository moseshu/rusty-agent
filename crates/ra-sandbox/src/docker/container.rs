//! What a container is created with, and whether an existing one still matches.
//!
//! The reference's module-level helpers around `_create_container`: the keyword arguments, the bind
//! mounts for split path grants and the volumes for driver-backed mounts, the capabilities a FUSE or
//! NFS mount needs, the names generated volumes get, and the checks a resumed container has to pass
//! before it is reused rather than replaced.

use std::collections::{BTreeMap, BTreeSet};

use ra_core::sandbox::{
    EntryContent, ErrorCode, Manifest, Mount, MountPattern, MountStrategy, OpName, PosixPath,
    RcloneMode, SandboxError, SandboxResult,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::host_paths::sandbox_path_grant_host_path;
use crate::mounts::config::{DockerVolumeDriverConfig, docker_volume_driver_config};

use super::api::{ContainerCreateSpec, DockerDriverConfig, DockerMount, PublishedPort};
use super::{DOCKER_BACKEND_ID, DockerNetworkMode};

/// How many hex digits of the mount path's digest a volume name carries.
const VOLUME_NAME_DIGEST_LENGTH: usize = 12;

/// Splits an image reference into repository and tag, as `docker-py`'s `parse_repository_tag` does.
///
/// A digest after `@` is the tag; otherwise the part after the last `:` is, unless it contains a
/// `/` — in which case that colon belonged to a registry port and there is no tag.
#[must_use]
pub fn parse_repository_tag(image: &str) -> (String, Option<String>) {
    if let Some((repository, digest)) = image.rsplit_once('@') {
        return (repository.to_owned(), Some(digest.to_owned()));
    }
    if let Some((repository, tag)) = image.rsplit_once(':')
        && !tag.contains('/')
    {
        return (repository.to_owned(), Some(tag.to_owned()));
    }
    (image.to_owned(), None)
}

/// The key a published port goes under: `<port>/tcp`.
pub(crate) fn docker_port_key(port: u16) -> String {
    format!("{port}/tcp")
}

/// The in-container mounts of a manifest, with the pattern each one runs.
fn in_container_patterns(manifest: &Manifest) -> SandboxResult<Vec<&MountPattern>> {
    Ok(manifest
        .iter_entries()?
        .into_iter()
        .filter_map(|(_, entry)| match entry.content() {
            EntryContent::Mount(mount) => match mount.strategy() {
                MountStrategy::InContainer { pattern } => Some(pattern),
                _ => None,
            },
            _ => None,
        })
        .collect())
}

/// Whether a manifest has a mount that needs `/dev/fuse`: a FUSE or Mountpoint mount, or rclone in
/// FUSE mode, attached from inside the container.
///
/// A manifest whose entries cannot be walked answers no; creating the container fails on the same
/// entries soon after, with the reason.
#[must_use]
pub fn manifest_requires_fuse(manifest: &Manifest) -> bool {
    in_container_patterns(manifest).is_ok_and(|patterns| {
        patterns.into_iter().any(|pattern| match pattern {
            MountPattern::Fuse(_) | MountPattern::Mountpoint(_) => true,
            MountPattern::Rclone(options) => options.mode == RcloneMode::Fuse,
            _ => false,
        })
    })
}

/// Whether a manifest has a mount that needs `SYS_ADMIN` without `/dev/fuse`: rclone in NFS mode, or
/// S3 Files, attached from inside the container.
#[must_use]
pub fn manifest_requires_sys_admin(manifest: &Manifest) -> bool {
    in_container_patterns(manifest).is_ok_and(|patterns| {
        patterns.into_iter().any(|pattern| match pattern {
            MountPattern::Rclone(options) => options.mode == RcloneMode::Nfs,
            MountPattern::S3Files(_) => true,
            _ => false,
        })
    })
}

/// The driver-backed mounts of a manifest, where each attaches, and what its driver is told.
///
/// In the manifest's own order rather than by depth: the order the reference builds its mount list
/// in. A mount whose strategy the container runtime does not attach is left out; one whose type has
/// no volume driver is refused, as creating the container would refuse it.
fn docker_volume_mounts(
    manifest: &Manifest,
) -> SandboxResult<Vec<(&Mount, PosixPath, DockerVolumeDriverConfig)>> {
    let targets = manifest.mount_targets()?;
    let mut mounts = Vec::new();
    for (_, entry) in manifest.iter_entries()? {
        let EntryContent::Mount(mount) = entry.content() else {
            continue;
        };
        let Some(config) = docker_volume_driver_config(mount, mount.strategy())? else {
            continue;
        };
        let Some((_, target)) = targets
            .iter()
            .find(|(candidate, _)| std::ptr::eq(*candidate, mount.as_ref()))
        else {
            continue;
        };
        mounts.push((mount.as_ref(), target.clone(), config));
    }
    Ok(mounts)
}

/// The name of the volume generated for a mount attached at `mount_path`.
///
/// `sandbox_`, the session's id in hex when there is one, twelve hex digits of the path's digest,
/// and the path with everything outside `[A-Za-z0-9_.-]` replaced — so `/workspace/a_b` and
/// `/workspace/a/b` read alike but never collide.
#[must_use]
pub fn docker_volume_name(session_id: Option<Uuid>, mount_path: &PosixPath) -> String {
    let prefix = session_id.map_or_else(String::new, |id| format!("{}_", id.simple()));
    let rendered = mount_path.as_str();
    let digest = format!("{:x}", Sha256::digest(rendered.as_bytes()));
    let sanitized: String = rendered
        .trim_matches('/')
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "workspace".to_owned()
    } else {
        sanitized
    };
    format!(
        "sandbox_{prefix}{}_{sanitized}",
        &digest[..VOLUME_NAME_DIGEST_LENGTH]
    )
}

/// The names of every volume a session with `session_id` generates for this manifest.
///
/// # Errors
///
/// Returns the manifest's failure to resolve its mounts, or a mount type with no volume driver.
pub fn docker_volume_names_for_manifest(
    manifest: &Manifest,
    session_id: Option<Uuid>,
) -> SandboxResult<Vec<String>> {
    Ok(docker_volume_mounts(manifest)?
        .into_iter()
        .map(|(_, target, _)| docker_volume_name(session_id, &target))
        .collect())
}

/// A path-grant refusal, which the reference raises as a plain value error.
fn grant_refused(message: String) -> SandboxError {
    SandboxError::new(ErrorCode::SandboxConfigInvalid, OpName::Start, message)
        .with_context("backend", DOCKER_BACKEND_ID)
}

/// Checks the manifest's path grants against what a container can bind.
///
/// A target named twice is refused when either naming carries a host source, because the container
/// could bind only one of them; a host-backed target has to lie wholly outside the workspace root,
/// neither inside it nor containing it; and it must not be where a volume attaches.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] for any of those, or a host source that is relative
/// or resolves to the filesystem root, and the manifest's failure to resolve its mounts.
pub(crate) fn validate_docker_path_grants(manifest: &Manifest) -> SandboxResult<()> {
    let root = PosixPath::coerce(&manifest.root);
    let volume_targets: BTreeSet<String> = docker_volume_mounts(manifest)?
        .into_iter()
        .map(|(_, target, _)| PosixPath::coerce(target.as_str()).as_str().to_owned())
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut explicit: BTreeSet<String> = BTreeSet::new();
    for grant in &manifest.extra_path_grants {
        let target = PosixPath::coerce(grant.path());
        let target_str = target.as_str().to_owned();
        if seen.contains(&target_str)
            && (grant.host_path().is_some() || explicit.contains(&target_str))
        {
            return Err(grant_refused(format!(
                "duplicate Docker sandbox path grant target: {}",
                grant.path()
            )));
        }
        seen.insert(target_str.clone());
        if grant.host_path().is_none() {
            continue;
        }
        explicit.insert(target_str.clone());
        sandbox_path_grant_host_path(grant)?;
        if target.is_under(&root) || root.is_under(&target) {
            return Err(grant_refused(format!(
                "Docker sandbox path grant host_path target must be outside the workspace root: {}",
                grant.path()
            )));
        }
        if volume_targets.contains(&target_str) {
            return Err(grant_refused(format!(
                "Docker sandbox path grant target conflicts with a manifest mount: {}",
                grant.path()
            )));
        }
    }
    Ok(())
}

/// The bind mounts for split path grants, then the volumes for driver-backed mounts.
///
/// # Errors
///
/// Returns a host source that does not resolve, the manifest's failure to resolve its mounts, or a
/// mount type with no volume driver.
pub(crate) fn build_docker_volume_mounts(
    manifest: &Manifest,
    session_id: Option<Uuid>,
) -> SandboxResult<Vec<DockerMount>> {
    let mut mounts = Vec::new();
    for grant in &manifest.extra_path_grants {
        if grant.host_path().is_none() {
            continue;
        }
        mounts.push(DockerMount::bind(
            grant.path(),
            sandbox_path_grant_host_path(grant)?.to_string_lossy(),
            grant.is_read_only(),
        ));
    }
    for (_, target, config) in docker_volume_mounts(manifest)? {
        mounts.push(DockerMount::volume(
            target.as_str(),
            docker_volume_name(session_id, &target),
            config.read_only,
            Some(DockerDriverConfig::new(config.driver, config.options)),
        ));
    }
    Ok(mounts)
}

/// Everything about a container except the environment, which has to be resolved first.
pub(crate) struct ContainerShape<'a> {
    pub(crate) image: &'a str,
    pub(crate) manifest: Option<&'a Manifest>,
    pub(crate) exposed_ports: &'a [u16],
    pub(crate) network_mode: Option<DockerNetworkMode>,
    pub(crate) session_id: Option<Uuid>,
    pub(crate) labels: &'a BTreeMap<String, String>,
}

/// The create arguments for a container, as the reference assembles them.
///
/// # Errors
///
/// Returns the mount list's failure to build.
pub(crate) fn container_create_spec(
    shape: &ContainerShape<'_>,
    environment: Option<BTreeMap<String, String>>,
) -> SandboxResult<ContainerCreateSpec> {
    let mut spec = ContainerCreateSpec::idle(shape.image).with_environment(environment);
    if let Some(mode) = shape.network_mode {
        spec = spec.with_network_mode(mode.as_str());
    }
    if !shape.labels.is_empty() {
        spec = spec.with_labels(shape.labels.clone());
    }
    if let Some(manifest) = shape.manifest {
        let mounts = build_docker_volume_mounts(manifest, shape.session_id)?;
        if !mounts.is_empty() {
            spec = spec.with_mounts(mounts);
        }
        let sys_admin = || {
            (
                vec!["SYS_ADMIN".to_owned()],
                vec!["apparmor:unconfined".to_owned()],
            )
        };
        if manifest_requires_fuse(manifest) {
            let (cap_add, security_opt) = sys_admin();
            spec = spec
                .with_devices(vec!["/dev/fuse".to_owned()])
                .with_cap_add(cap_add)
                .with_security_opt(security_opt);
        } else if manifest_requires_sys_admin(manifest) {
            let (cap_add, security_opt) = sys_admin();
            spec = spec.with_cap_add(cap_add).with_security_opt(security_opt);
        }
    }
    if !shape.exposed_ports.is_empty() {
        spec = spec.with_ports(
            shape
                .exposed_ports
                .iter()
                .map(|port| PublishedPort::new(docker_port_key(*port), "127.0.0.1", None))
                .collect(),
        );
    }
    Ok(spec)
}

/// A reused container that does not match, which the reference raises as a plain value error.
fn mismatch(message: impl Into<String>) -> SandboxError {
    SandboxError::new(ErrorCode::SandboxConfigInvalid, OpName::Start, message)
        .with_context("backend", DOCKER_BACKEND_ID)
}

/// Refuses to reuse a container that is not isolated the way the state says it was created.
///
/// Only checked when the state asks for no network: then the host configuration must say `none` and
/// the container must be attached to no network other than `none` — a container that was attached
/// to one after it was created is not the isolated container the state describes.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when it does not match.
pub(crate) fn assert_existing_container_network_configuration_matches(
    attrs: &Value,
    network_mode: Option<DockerNetworkMode>,
) -> SandboxResult<()> {
    if network_mode.is_none() {
        return Ok(());
    }
    let actual_mode = attrs
        .get("HostConfig")
        .and_then(Value::as_object)
        .and_then(|config| config.get("NetworkMode"))
        .and_then(Value::as_str);
    let networks = attrs
        .get("NetworkSettings")
        .and_then(Value::as_object)
        .and_then(|settings| settings.get("Networks"))
        .and_then(Value::as_object);
    let isolated = actual_mode == Some("none")
        && networks.is_some_and(|networks| networks.keys().all(|name| name == "none"));
    if isolated {
        return Ok(());
    }
    Err(mismatch(
        "Existing Docker sandbox network configuration does not match persisted \
         network_mode='none'; create a fresh sandbox session",
    ))
}

/// Refuses to reuse a container missing a label the state recorded, or carrying a different value.
///
/// Labels the container has beyond those are left alone.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when a label does not match.
pub(crate) fn assert_existing_container_labels_match(
    attrs: &Value,
    labels: &BTreeMap<String, String>,
) -> SandboxResult<()> {
    if labels.is_empty() {
        return Ok(());
    }
    let actual = attrs
        .get("Config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("Labels"))
        .and_then(Value::as_object);
    let matches = labels.iter().all(|(key, value)| {
        actual
            .and_then(|actual| actual.get(key))
            .and_then(Value::as_str)
            == Some(value.as_str())
    });
    if matches {
        return Ok(());
    }
    Err(mismatch(
        "Existing Docker sandbox labels do not match persisted labels; create a fresh sandbox \
         session",
    ))
}

/// Normalizes a host path lexically, as `os.path.normpath` does on POSIX.
///
/// Including its one oddity: exactly two leading slashes are kept, because POSIX leaves the meaning
/// of `//` implementation-defined, while three or more collapse to one.
fn normalized_host_path(path: &str) -> String {
    let leading = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if leading.is_empty() {
                    parts.push("..");
                }
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if leading.is_empty() && joined.is_empty() {
        ".".to_owned()
    } else {
        format!("{leading}{joined}")
    }
}

/// Refuses to reuse a container whose bind mounts are not exactly the trusted manifest's grants.
///
/// Every bind mount must be one the manifest grants with a host source, and every such grant must be
/// bound exactly once, from the same host path, with the same read-only setting. A container that
/// binds something the current manifest no longer grants would hand out access nobody granted.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] for a bind mount with no destination, one not
/// granted, or a grant bound differently; and a host source that does not resolve.
pub(crate) fn assert_existing_container_path_grants_match(
    attrs: &Value,
    manifest: &Manifest,
) -> SandboxResult<()> {
    let mounts = attrs
        .get("Mounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let expected: BTreeMap<&str, &ra_core::sandbox::SandboxPathGrant> = manifest
        .extra_path_grants
        .iter()
        .filter(|grant| grant.host_path().is_some())
        .map(|grant| (grant.path(), grant))
        .collect();
    let mut actual: BTreeMap<String, Vec<&serde_json::Map<String, Value>>> = BTreeMap::new();
    for mount in &mounts {
        let Some(mount) = mount.as_object() else {
            continue;
        };
        if mount.get("Type").and_then(Value::as_str) != Some("bind") {
            continue;
        }
        let Some(destination) = mount.get("Destination").and_then(Value::as_str) else {
            return Err(mismatch(
                "Existing Docker sandbox has a bind mount without a valid destination; create a \
                 fresh sandbox session",
            ));
        };
        actual
            .entry(destination.to_owned())
            .or_default()
            .push(mount);
    }

    let unexpected: Vec<&str> = actual
        .keys()
        .map(String::as_str)
        .filter(|destination| !expected.contains_key(destination))
        .collect();
    if !unexpected.is_empty() {
        return Err(mismatch(format!(
            "Existing Docker sandbox has bind mounts that are not present in the current trusted \
             manifest: {}; create a fresh sandbox session",
            unexpected.join(", ")
        )));
    }

    for (path, grant) in expected {
        let differs = || {
            mismatch(format!(
                "Existing Docker sandbox path grant mount does not match the current trusted \
                 manifest for '{path}'; create a fresh sandbox session"
            ))
        };
        let bound = actual.get(path).map_or(&[][..], Vec::as_slice);
        let [mount] = bound else {
            return Err(differs());
        };
        let expected_source =
            normalized_host_path(&sandbox_path_grant_host_path(grant)?.to_string_lossy());
        let source = mount
            .get("Source")
            .and_then(Value::as_str)
            .map(normalized_host_path);
        let read_only = mount.get("RW") == Some(&Value::Bool(false));
        if source.as_deref() != Some(expected_source.as_str()) || read_only != grant.is_read_only()
        {
            return Err(differs());
        }
    }
    Ok(())
}
