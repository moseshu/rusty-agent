// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Filesystem and capability-discovery wire records.
#![allow(
    clippy::struct_excessive_bools,
    reason = "Keep the source wire feature flags independently configurable."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use crate::{JSONRPCErrorError, capabilities::SelectedCapabilityRoot};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ra_filesystem::{
    FileSystemSandboxContext, WalkOptions, WalkOutcome, WireFileSystemSandboxContext,
};
use ra_paths::PathUri;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

pub const INITIALIZE_METHOD: &str = "initialize";
pub const INITIALIZED_METHOD: &str = "initialized";
pub const ENVIRONMENT_INFO_METHOD: &str = "environment/info";
pub const ENVIRONMENT_STATUS_METHOD: &str = "environment/status";
pub const FS_READ_FILE_METHOD: &str = "fs/readFile";
pub const FS_OPEN_METHOD: &str = "fs/open";
pub const FS_READ_BLOCK_METHOD: &str = "fs/readBlock";
pub const FS_WRITE_BLOCK_METHOD: &str = "fs/writeBlock";
pub const FS_CLOSE_METHOD: &str = "fs/close";
pub const FS_WRITE_FILE_METHOD: &str = "fs/writeFile";
pub const FS_CREATE_DIRECTORY_METHOD: &str = "fs/createDirectory";
pub const FS_GET_METADATA_METHOD: &str = "fs/getMetadata";
pub const FS_CANONICALIZE_METHOD: &str = "fs/canonicalize";
pub const FS_READ_DIRECTORY_METHOD: &str = "fs/readDirectory";
pub const FS_WALK_METHOD: &str = "fs/walk";
pub const FS_REMOVE_METHOD: &str = "fs/remove";
pub const FS_COPY_METHOD: &str = "fs/copy";
/// Discovers capability manifests below selected roots using executor-local filesystem access.
pub const CAPABILITY_ROOTS_DISCOVER_METHOD: &str = "capabilityRoots/discoverV1";
/// Ordered plugin manifest paths recognized beneath a plugin root.
pub const DISCOVERABLE_PLUGIN_MANIFEST_PATHS: &[&str] = &[
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_name: String,
    #[serde(default)]
    pub resume_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub session_id: String,
    /// Executor metadata at initialization, with the same shape as `environment/info`.
    // TODO: Make this required once all supported exec-server versions return environmentInfo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_info: Option<EnvironmentInfo>,
}

/// Information about an execution/filesystem environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentInfo {
    pub shell: ShellInfo,
    /// Executor release version for version-based compatibility decisions.
    /// `0.0.0` when unknown, including responses from legacy executors.
    #[serde(default = "unknown_executor_version")]
    pub executor_version: String,
    /// Opaque executor build identity for looking up behavioral verification.
    /// Derived from the compiled commit and target for standard builds;
    /// absent for legacy or unstamped builds. This is not an artifact checksum
    /// or a security attestation, and evidence must not be shared across build variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Working directory inherited by the exec-server process.
    #[serde(default)]
    pub cwd: Option<PathUri>,
    /// Executor user home used to expand `~` in path-bearing values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_home_dir: Option<PathUri>,
    /// Operating system reported by the executor; absent for legacy exec-servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_os: Option<String>,
    /// Executor directories to prepend to `PATH` when missing, in priority order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepend_path_dirs: Vec<PathUri>,
    /// Executor-local default directories for resolving `:tmpdir`, when reported.
    /// On Windows, a command's `TEMP` or `TMP` overrides take precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary_directories: Option<Vec<PathUri>>,
    /// Executor-native temporary directory for child-visible sidecars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathUri>,
    /// Optional executor features that clients must gate before sending newer request fields.
    #[serde(default)]
    pub capabilities: EnvironmentCapabilities,
}

fn unknown_executor_version() -> String {
    "0.0.0".to_string()
}

/// Features supported by the selected exec-server environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentCapabilities {
    /// Whether `exec` accepts instructions for launching an executor-local network proxy.
    #[serde(default)]
    pub network_proxy_launch: bool,
    /// Whether capability discovery applies the filesystem sandbox sent with each root.
    #[serde(default)]
    pub capability_discovery_sandbox: bool,
    /// Whether this executor supports V2 capability discovery.
    #[serde(default)]
    pub capability_discovery_v2: bool,
    /// Whether this executor supports the `environmentConfig/read` request.
    #[serde(default)]
    pub environment_config_read: bool,
    /// Whether HTTP headers can resolve values from the executor environment.
    #[serde(default)]
    pub http_header_env_vars: bool,
    /// Whether filesystem streams can use the requested platform sandbox.
    #[serde(default)]
    pub sandboxed_file_streaming: bool,
    /// Whether `fs/open` supports replacement mode and `fs/writeBlock` is supported.
    #[serde(default)]
    pub file_write_streaming: bool,
    /// Whether shell state can be cached and restored entirely inside the executor.
    #[serde(default)]
    pub shell_snapshot_v2: bool,
    /// Whether requests may explicitly select the MXC Windows sandbox backend.
    #[serde(default)]
    pub windows_mxc: bool,
    /// Whether a Linux filesystem sandbox preserves standard devices when `/` is writable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linux_root_write_preserves_devices: bool,
    /// Whether approved Linux root writes preserve devices and denied root-metadata symlink targets.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linux_approved_root_write_preserves_restrictions: bool,
}

/// Status returned by an initialized exec-server connection.
///
/// The response is intentionally small today. New status details can be added
/// without changing the method used by clients to verify that an initialized
/// exec-server connection is still responsive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatus {
    pub status: EnvironmentStatusKind,
}

/// High-level status reported by exec-server itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvironmentStatusKind {
    /// The connection is initialized and exec-server can handle requests.
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellInfo {
    /// Stable shell name, for example `zsh`, `bash`, `powershell`, `sh`, or `cmd`.
    pub name: String,
    /// Target-native shell executable path or command name. Fallbacks such as `cmd.exe` need not
    /// be absolute, so this is not a [`PathUri`].
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileParams {
    pub path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsReadFileParams {
    path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_symlinks: Option<bool>,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileResponse {
    pub data_base64: String,
}

/// Defaults preserve read-only opens for legacy callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FsOpenMode {
    /// Open an existing file for reading.
    #[default]
    Read,
    /// Open for writing, creating a missing file or truncating an existing file.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsOpenParams {
    pub handle_id: String,
    pub path: PathUri,
    #[serde(default)]
    pub mode: FsOpenMode,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsOpenParams {
    handle_id: String,
    path: PathUri,
    #[serde(default)]
    mode: FsOpenMode,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsOpenResponse {
    pub handle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadBlockParams {
    pub handle_id: String,
    pub offset: u64,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadBlockResponse {
    pub chunk: ByteChunk,
    pub eof: bool,
}

/// Writes a nonempty block of at most [`ra_filesystem::FILE_WRITE_CHUNK_SIZE`] decoded bytes at `offset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteBlockParams {
    pub handle_id: String,
    pub offset: u64,
    pub chunk: ByteChunk,
}

/// A successful response confirms that every byte in the requested block was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteBlockResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCloseParams {
    pub handle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCloseResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteFileParams {
    pub path: PathUri,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsWriteFileParams {
    path: PathUri,
    data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_symlinks: Option<bool>,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteFileResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCreateDirectoryParams {
    pub path: PathUri,
    pub recursive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsCreateDirectoryParams {
    path: PathUri,
    recursive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_symlinks: Option<bool>,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCreateDirectoryResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataParams {
    pub path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsGetMetadataParams {
    path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_symlinks: Option<bool>,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataResponse {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub created_at_ms: i64,
    pub modified_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeParams {
    pub path: PathUri,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsCanonicalizeParams {
    path: PathUri,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeResponse {
    pub path: PathUri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryParams {
    pub path: PathUri,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsReadDirectoryParams {
    path: PathUri,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryResponse {
    pub entries: Vec<FsReadDirectoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWalkParams {
    pub path: PathUri,
    pub options: WalkOptions,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsWalkParams {
    path: PathUri,
    options: WalkOptions,
    sandbox: Option<WireFileSystemSandboxContext>,
}

pub type FsWalkResponse = WalkOutcome;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRemoveParams {
    pub path: PathUri,
    pub recursive: Option<bool>,
    pub force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsRemoveParams {
    path: PathUri,
    recursive: Option<bool>,
    force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_symlinks: Option<bool>,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRemoveResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCopyParams {
    pub source_path: PathUri,
    pub destination_path: PathUri,
    pub recursive: bool,
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// Filesystem RPC wire request with legacy optional sandbox policy cwd.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFsCopyParams {
    source_path: PathUri,
    destination_path: PathUri,
    recursive: bool,
    sandbox: Option<WireFileSystemSandboxContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCopyResponse {}

/// Roots to inspect for plugin and skill capability manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootsDiscoverParams {
    pub roots: Vec<CapabilityRootDiscoverRequest>,
}

/// Executor ingress for capability roots whose sandbox policy cwd may be absent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireCapabilityRootsDiscoverParams {
    roots: Vec<WireCapabilityRootDiscoverRequest>,
}

/// One caller-selected capability root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootDiscoverRequest {
    /// Opaque caller identity returned unchanged in the response.
    pub id: String,
    /// Absolute root URI interpreted using the exec-server host's path rules.
    pub path: PathUri,
    /// Filesystem permissions for this root and its symlink targets.
    #[serde(default)]
    pub sandbox: Option<FileSystemSandboxContext>,
}

/// One executor-ingress capability root with legacy optional sandbox policy cwd.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireCapabilityRootDiscoverRequest {
    id: String,
    path: PathUri,
    sandbox: Option<WireFileSystemSandboxContext>,
}

// Destructuring both types makes adding a request field require updating the wire conversion.
macro_rules! impl_wire_filesystem_request {
    ($($wire:ident => $request:ident { $($field:ident),+ $(,)? }),+ $(,)?) => {$(
        impl From<$request> for $wire {
            fn from(request: $request) -> Self {
                let $request { $($field,)+ sandbox } = request;
                Self { $($field,)+ sandbox: sandbox.map(Into::into) }
            }
        }

        impl $wire {
            /// Resolves optional wire sandbox intent before it reaches an executor handler.
            pub fn try_into_request(
                self,
                resolve: fn(WireFileSystemSandboxContext) -> Result<FileSystemSandboxContext, JSONRPCErrorError>,
            ) -> Result<$request, JSONRPCErrorError> {
                let Self { $($field,)+ sandbox } = self;
                Ok($request { $($field,)+ sandbox: sandbox.map(resolve).transpose()? })
            }
        }
    )+};
}

impl_wire_filesystem_request! {
    WireFsReadFileParams => FsReadFileParams { path, follow_symlinks },
    WireFsOpenParams => FsOpenParams { handle_id, path, mode },
    WireFsWriteFileParams => FsWriteFileParams { path, data_base64, follow_symlinks },
    WireFsCreateDirectoryParams => FsCreateDirectoryParams { path, recursive, follow_symlinks },
    WireFsGetMetadataParams => FsGetMetadataParams { path, follow_symlinks },
    WireFsCanonicalizeParams => FsCanonicalizeParams { path },
    WireFsReadDirectoryParams => FsReadDirectoryParams { path },
    WireFsWalkParams => FsWalkParams { path, options },
    WireFsRemoveParams => FsRemoveParams { path, recursive, force, follow_symlinks },
    WireFsCopyParams => FsCopyParams { source_path, destination_path, recursive },
}

impl WireCapabilityRootDiscoverRequest {
    /// Resolves optional wire sandbox intent before it reaches an executor handler.
    pub fn try_into_request(
        self,
        resolve: fn(
            WireFileSystemSandboxContext,
        ) -> Result<FileSystemSandboxContext, JSONRPCErrorError>,
    ) -> Result<CapabilityRootDiscoverRequest, JSONRPCErrorError> {
        let Self { id, path, sandbox } = self;
        Ok(CapabilityRootDiscoverRequest {
            id,
            path,
            sandbox: sandbox.map(resolve).transpose()?,
        })
    }
}

impl WireCapabilityRootsDiscoverParams {
    /// Resolves each root's optional wire sandbox before it reaches an executor handler.
    pub fn try_into_request(
        self,
        resolve: fn(
            WireFileSystemSandboxContext,
        ) -> Result<FileSystemSandboxContext, JSONRPCErrorError>,
    ) -> Result<CapabilityRootsDiscoverParams, JSONRPCErrorError> {
        let mut roots = Vec::with_capacity(self.roots.len());
        for root in self.roots {
            roots.push(root.try_into_request(resolve)?);
        }
        Ok(CapabilityRootsDiscoverParams { roots })
    }
}

/// Executor-local discovery results in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootsDiscoverResponse {
    pub roots: Vec<CapabilityRootDiscovery>,
}

/// UTF-8 capability contents materialized by the exec-server.
/// `path` identifies the source file; V2 inline MCP contents are extracted from its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityTextFile {
    pub path: PathUri,
    pub contents: String,
}

/// Plugin files declared directly by a selected root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPluginFiles {
    pub manifest: CapabilityTextFile,
    /// MCP declarations from a selected file, including the conventional `.mcp.json` fallback.
    /// V2 also materializes inline declarations, using the manifest as the source path.
    /// V1 leaves inline declarations only in `manifest`.
    #[serde(default)]
    pub mcp_config: Option<CapabilityTextFile>,
    /// File-backed connector declarations.
    #[serde(default)]
    pub apps_config: Option<CapabilityTextFile>,
}

/// A skill instructions file and its optional sibling metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSkillFiles {
    pub instructions: CapabilityTextFile,
    #[serde(default)]
    pub metadata: Option<CapabilityTextFile>,
}

/// Manifest bundle for one selected root.
///
/// Discovery failures are root-local so one broken package does not discard valid siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootDiscovery {
    pub id: String,
    pub path: PathUri,
    #[serde(default)]
    pub plugin: Option<DiscoveredPluginFiles>,
    #[serde(default)]
    pub skills: Vec<DiscoveredSkillFiles>,
    /// Plugin manifests found while scanning the root, used to namespace nested skills.
    #[serde(default)]
    pub namespace_manifests: Vec<CapabilityTextFile>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Immutable results for the selected capability roots visible in one model step.
#[derive(Clone, Debug)]
pub struct ExecutorCapabilityDiscoverySnapshot {
    roots: Arc<[ExecutorCapabilityDiscoverySnapshotEntry]>,
    sandbox_contexts: Arc<HashMap<String, FileSystemSandboxContext>>,
}

#[derive(Clone, Debug)]
pub struct ExecutorCapabilityDiscoverySnapshotEntry {
    pub selected_root: SelectedCapabilityRoot,
    pub result: Result<Arc<CapabilityRootDiscovery>, String>,
}

impl ExecutorCapabilityDiscoverySnapshot {
    #[must_use]
    pub fn new(
        selected_roots: &[SelectedCapabilityRoot],
        discoveries: Vec<Result<Arc<CapabilityRootDiscovery>, String>>,
        sandbox_contexts: HashMap<String, FileSystemSandboxContext>,
    ) -> Self {
        debug_assert_eq!(selected_roots.len(), discoveries.len());
        Self {
            roots: selected_roots
                .iter()
                .cloned()
                .zip(discoveries)
                .map(
                    |(selected_root, result)| ExecutorCapabilityDiscoverySnapshotEntry {
                        selected_root,
                        result,
                    },
                )
                .collect(),
            sandbox_contexts: Arc::new(sandbox_contexts),
        }
    }

    #[must_use]
    pub fn roots(&self) -> &[ExecutorCapabilityDiscoverySnapshotEntry] {
        &self.roots
    }

    #[must_use]
    pub fn sandbox_contexts(&self) -> &HashMap<String, FileSystemSandboxContext> {
        self.sandbox_contexts.as_ref()
    }
}

pub const EXEC_OUTPUT_DELTA_METHOD: &str = "process/output";
pub const EXEC_EXITED_METHOD: &str = "process/exited";
pub const EXEC_CLOSED_METHOD: &str = "process/closed";
pub const HTTP_REQUEST_BODY_DELTA_METHOD: &str = "http/request/bodyDelta";
pub const MAX_HTTP_BODY_DELTA_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByteChunk(#[serde(with = "base64_bytes")] pub Arc<Vec<u8>>);

impl ByteChunk {
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl From<Vec<u8>> for ByteChunk {
    fn from(value: Vec<u8>) -> Self {
        Self(Arc::new(value))
    }
}

impl AsRef<[u8]> for ByteChunk {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

mod base64_bytes {
    use super::BASE64_STANDARD;
    use base64::Engine as _;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use std::sync::Arc;

    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Arc<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        BASE64_STANDARD
            .decode(encoded)
            .map(Arc::new)
            .map_err(serde::de::Error::custom)
    }
}

impl EnvironmentInfo {
    /// Returns executor-local default directories used to resolve `:tmpdir`.
    ///
    /// This is separate from `local` so orchestrator startup can cache the
    /// directories without repeating local shell detection.
    #[must_use]
    pub fn local_temporary_directories() -> Vec<PathUri> {
        let cwd = std::env::current_dir().ok();
        Self::local_temporary_directories_with_cwd(cwd.as_deref())
    }

    fn local_temporary_directories_with_cwd(cwd: Option<&std::path::Path>) -> Vec<PathUri> {
        let temporary_directory_env_vars: &[&str] = if cfg!(windows) {
            &["TEMP", "TMP"]
        } else {
            &["TMPDIR"]
        };
        let normalize_temp_path = |path: std::ffi::OsString| {
            PathUri::from_host_native_path(&path).ok().or_else(|| {
                if cfg!(unix) {
                    PathUri::from_host_native_path(cwd.as_ref()?.join(path)).ok()
                } else {
                    None
                }
            })
        };
        let mut temporary_directories = Vec::new();
        for name in temporary_directory_env_vars {
            if let Some(path) = std::env::var_os(name)
                .filter(|path| !path.is_empty())
                .filter(|path| cfg!(unix) || std::path::Path::new(path).is_absolute())
                .and_then(&normalize_temp_path)
                && !temporary_directories.contains(&path)
            {
                temporary_directories.push(path);
            }
        }
        temporary_directories
    }
}
