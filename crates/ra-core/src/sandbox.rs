//! The sandbox protocol: what a workspace execution environment offers, stated without choosing one.
//!
//! **Stability**: `Evolving`. The protocol is being carried over from the reference implementation
//! one layer at a time; names and fields already here are meant to match it and will not be
//! renamed for taste, but the surface is still growing.
//!
//! **Boundary**: types, traits and codes only. A backend — a local one, a container, a hosted
//! provider — implements this from a service crate. Nothing here opens a file, spawns a process or
//! knows what a container is, which is what keeps the loop kernel able to name a sandbox without
//! depending on any implementation of one.
//!
//! # This is not the process fence
//!
//! `ra-exec` also has something called a sandbox: a mechanism that wraps one command so the
//! operating system confines it. That is a per-command fence chosen by a host that already has a
//! workspace. This module is the workspace itself — an environment that outlives a command, holds
//! files, and can be stopped, serialized and resumed. The two compose, and neither substitutes for
//! the other.

pub mod agent;
pub mod archive;
pub mod dependencies;
pub mod entries;
pub mod environment;
pub mod error;
pub mod events;
pub mod files;
pub mod manifest;
pub mod manifest_render;
pub mod materialization;
pub mod mount_security;
pub mod pty;
pub mod registry;
pub mod remote_mount_policy;
pub mod resources;
pub mod session;
pub mod shell;
pub mod sinks;
pub mod snapshot;
pub mod state;
pub mod token_truncation;
pub mod types;
pub mod workspace_paths;

pub use agent::{
    SandboxAgentConfig, SandboxAgentRunLease, manifest_with_run_as_user, process_manifest,
};
pub use archive::{
    ArchiveLimitError, CompressionScheme, DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES,
    DEFAULT_MAX_ARCHIVE_INPUT_BYTES, DEFAULT_MAX_ARCHIVE_MEMBERS, SandboxArchiveLimits,
    file_name_suffix,
};
pub use dependencies::{
    CloseDependency, Dependencies, DependenciesError, DependencyFactory, DependencyFactoryError,
    DependencyFactoryResult, DependencyKey, DependencyValue, FactoryOptions, dependency_factory,
};
pub use entries::mounts::{
    AzureBlobMount, BUILTIN_MOUNT_TYPES, BoxMount, BoxSubType, DEFAULT_S3_PROVIDER,
    DOCKER_VOLUME_STRATEGY_TYPE, FuseCacheType, FuseOptions, GcsMount, IN_CONTAINER_STRATEGY_TYPE,
    Mount, MountConfigError, MountPattern, MountProvider, MountStrategy, MountpointOptions,
    R2Mount, RcloneMode, RcloneOptions, S3FilesMount, S3FilesOptions, S3Mount, authority_fields,
    authority_file_fields, builtin_mount_strategy_registry, credential_set, mount_strategy_kind,
    url_carries_inline_authority, url_fields,
};
pub use entries::{
    DEFAULT_GIT_HOST, Entry, EntryContent, EntryOwner, EntryRenderError, builtin_entry_registry,
    default_entry_permissions, entry_kind, resolve_workspace_path,
};
pub use environment::{
    EnvEntry, EnvMember, EnvValue, EnvValueResolver, Environment, STR_ENV_VALUE_TYPE,
    UnresolvableEnvValues, builtin_env_value_registry, env_value_kind,
};
pub use error::{
    ApplyPatchPathReason, ErrorCategory, ErrorCode, OpName, PTY_STDIN_UNAVAILABLE_MESSAGE,
    SandboxError, SandboxErrorDetails,
};
pub use events::{
    DEFAULT_MAX_STDERR_CHARS, DEFAULT_MAX_STDOUT_CHARS, EventPayloadPolicy, EventPhase,
    SANDBOX_SESSION_EVENT_VERSION, SandboxSessionEvent, SandboxSessionEventBase,
    SandboxSessionFinishEvent, SandboxSessionStartEvent, event_to_json_line,
    format_event_timestamp, parse_event_timestamp, safe_decode, validate_sandbox_session_event,
};
pub use files::{EntryKind, FileEntry};
pub use manifest::{
    DEFAULT_MANIFEST_ROOT, DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST, MANIFEST_VERSION, Manifest,
    ManifestParseError, ManifestRegistries, MountCredentialAuthority, MountExposureError,
};
pub use manifest_render::{
    MAX_MANIFEST_DESCRIPTION_CHARS, render_manifest_description, truncate_manifest_description,
};
pub use materialization::{
    ConcurrencyLimitError, DEFAULT_MAX_LOCAL_DIR_FILE_CONCURRENCY,
    DEFAULT_MAX_MANIFEST_ENTRY_CONCURRENCY, MaterializationResult, MaterializedFile,
    SandboxConcurrencyLimits,
};
pub use mount_security::{
    CREDENTIALLESS_MOUNT_AUTHORITY_KEY, InvalidRawManifest, InvalidRunStateSandbox,
    REDACTED_MOUNT_AUTHORITY_KEY, configured_authority_fields,
    manifest_has_configured_mount_authority, mount_has_configured_authority,
    rclone_config_value_fields, rebind_manifest_mount_authority, replace_protected_mount_error,
    sanitize_manifest_mount_authority, sanitize_raw_manifest_mount_authority,
    sanitize_raw_session_state_mount_authority, sanitize_run_state_sandbox_mount_authority,
    validate_manifest_mount_credential_boundaries, validate_manifest_mount_provenance,
    validate_mount_activation_credential_boundary,
};
pub use pty::{
    PTY_EMPTY_YIELD_TIME_MS_MIN, PTY_PROCESS_ID_MAX_EXCLUSIVE, PTY_PROCESS_ID_MIN,
    PTY_PROCESSES_MAX, PTY_PROCESSES_PROTECTED_RECENT, PTY_PROCESSES_WARNING,
    PTY_YIELD_TIME_MS_MAX, PTY_YIELD_TIME_MS_MIN, PtyExecUpdate, PtyProcessId, PtyProcessMeta,
    PtyStartRequest, PtyWriteRequest, allocate_pty_process_id, clamp_pty_yield_time_ms,
    process_id_to_prune_from_meta, resolve_pty_write_yield_time_ms, truncate_text_by_tokens,
};
pub use registry::{
    DiscriminatedPayload, RegistryError, RegistryKind, TypeRegistry, client_options_kind,
    session_state_kind, snapshot_kind,
};
pub use remote_mount_policy::{build_remote_mount_policy_instructions, remote_mounts};
pub use resources::{PreStopHook, SessionResources, pre_stop_hook};
pub use session::{
    AsUser, CreateRequest, ExecRequest, SandboxClient, SandboxResult, SandboxSession,
    ShellInvocation, invalid_state_payload, parse_session_state_for_backend,
    render_session_state_for_storage,
};
pub use sinks::{DeliveryMode, EventSink, OnErrorPolicy, SinkError, undecorated_session};
pub use snapshot::{
    LOCAL_SNAPSHOT_TYPE, NOOP_SNAPSHOT_TYPE, REMOTE_SNAPSHOT_TYPE, Snapshot, SnapshotFingerprint,
    SnapshotPathError, SnapshotSource, SnapshotSpec, builtin_snapshot_registry, resolve_snapshot,
};
pub use state::{
    ExposedPortsError, InvalidSessionStatePayload, REDACTED_HOST_PATH_GRANT_PATHS_KEY,
    SandboxSessionState, normalize_exposed_ports,
};
pub use types::{
    ErrorContext, ExecResult, ExposedPortEndpoint, FileMode, Group, Permissions,
    PermissionsParseError, UnsupportedScheme, User,
};
pub use workspace_paths::{
    CwdError, InvalidWorkspaceRoot, PathGrantError, PosixPath, SandboxPathGrant,
    SandboxWorkspaceScope, ScopePathError, WorkspacePathPolicy, normalize_sandbox_cwd,
    windows_absolute_path,
};
