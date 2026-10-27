//! Environment-owned filesystem access, capability discovery and executor transport.
//!
//! **Stability**: `Internal`. These operations preserve the pinned executor contract.

pub use ra_executor_protocol as protocol;
pub use ra_executor_protocol::{
    ByteChunk, CAPABILITY_ROOTS_DISCOVER_METHOD, CapabilityRootDiscoverRequest,
    CapabilityRootDiscovery, CapabilityRootsDiscoverParams, CapabilityRootsDiscoverResponse,
    CapabilityTextFile, DISCOVERABLE_PLUGIN_MANIFEST_PATHS, DiscoveredPluginFiles,
    DiscoveredSkillFiles, ENVIRONMENT_INFO_METHOD, ENVIRONMENT_STATUS_METHOD, EXEC_CLOSED_METHOD,
    EXEC_EXITED_METHOD, EXEC_OUTPUT_DELTA_METHOD, EnvironmentCapabilities, EnvironmentInfo,
    EnvironmentStatus, EnvironmentStatusKind, ExecutorCapabilityDiscoverySnapshot,
    ExecutorCapabilityDiscoverySnapshotEntry, FS_CANONICALIZE_METHOD, FS_CLOSE_METHOD,
    FS_COPY_METHOD, FS_CREATE_DIRECTORY_METHOD, FS_GET_METADATA_METHOD, FS_OPEN_METHOD,
    FS_READ_BLOCK_METHOD, FS_READ_DIRECTORY_METHOD, FS_READ_FILE_METHOD, FS_REMOVE_METHOD,
    FS_WALK_METHOD, FS_WRITE_BLOCK_METHOD, FS_WRITE_FILE_METHOD, FsCanonicalizeParams,
    FsCanonicalizeResponse, FsCloseParams, FsCloseResponse, FsCopyParams, FsCopyResponse,
    FsCreateDirectoryParams, FsCreateDirectoryResponse, FsGetMetadataParams, FsGetMetadataResponse,
    FsOpenMode, FsOpenParams, FsOpenResponse, FsReadBlockParams, FsReadBlockResponse,
    FsReadDirectoryEntry, FsReadDirectoryParams, FsReadDirectoryResponse, FsReadFileParams,
    FsReadFileResponse, FsRemoveParams, FsRemoveResponse, FsWalkParams, FsWalkResponse,
    FsWriteBlockParams, FsWriteBlockResponse, FsWriteFileParams, FsWriteFileResponse,
    HTTP_REQUEST_BODY_DELTA_METHOD, INITIALIZE_METHOD, INITIALIZED_METHOD, InitializeParams,
    InitializeResponse, JSONRPC_VERSION, JSONRPCError, JSONRPCErrorError, JSONRPCMessage,
    JSONRPCNotification, JSONRPCRequest, JSONRPCResponse, MAX_HTTP_BODY_DELTA_BYTES, RequestId,
    ShellInfo, WireCapabilityRootDiscoverRequest, WireCapabilityRootsDiscoverParams,
    WireFsCanonicalizeParams, WireFsCopyParams, WireFsCreateDirectoryParams,
    WireFsGetMetadataParams, WireFsOpenParams, WireFsReadDirectoryParams, WireFsReadFileParams,
    WireFsRemoveParams, WireFsWalkParams, WireFsWriteFileParams,
};
pub use ra_filesystem::*;
mod capability_discovery;
mod client_inbound_request_limit;
mod connection;
mod rpc;
mod rpc_server_requests;
mod rpc_timing;
pub use capability_discovery::{CapabilityDiscoveryError, discover_capability_roots};
pub use ra_exec::filesystem::{LOCAL_FS, LocalFileSystem};

pub(crate) fn background_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let command = tokio::process::Command::new(program);
    #[cfg(windows)]
    let command = {
        let mut command = command;
        command.creation_flags(0x0800_0000);
        command
    };
    command
}

mod client;
mod client_api;
mod client_transport;
mod environment;
mod environment_provider;
mod environment_toml;
mod error;
mod local_environment_info;
mod remote_file_system;
mod shell_environment;
pub use client::ExecServerClient;
pub use client_api::{ExecServerClientConnectOptions, RemoteExecServerConnectArgs};
pub use environment::{
    ConnectionAttemptOutcome, Environment, EnvironmentConnectionState, EnvironmentManager,
    EnvironmentObservedStatus, EnvironmentReadyInfo, LOCAL_ENVIRONMENT_ID,
    MAX_SELECTED_CAPABILITY_ROOTS, PreparedEnvironmentManager, REMOTE_ENVIRONMENT_ID,
    RemoteEnvironmentOptions,
};
pub use environment_provider::{
    DefaultEnvironmentProvider, EnvironmentProvider, EnvironmentProviderFuture,
};
pub use error::ExecServerError;
pub use ra_exec::filesystem::{ExecServerRuntimeOptions, LinuxSandboxPidNamespace};

pub use ra_executor_protocol::capabilities;

mod capability_discovery_cache;
pub use capability_discovery_cache::ExecutorCapabilityDiscoveryCache;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api;

mod resolved_capability;
pub use resolved_capability::SelectedCapabilityRootsStatus;
