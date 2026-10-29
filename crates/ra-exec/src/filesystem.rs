//! Native implementation of the executor filesystem contract.
//! URI conversion happens only at the owning native filesystem boundary.

use ra_executor_protocol as protocol;
use ra_filesystem::{
    CopyOptions, CreateDirectoryOptions, ExecutorFileSystem, ExecutorFileSystemFuture,
    FILE_READ_CHUNK_SIZE, FileMetadata, FileSystemReadStream, FileSystemResult,
    FileSystemSandboxContext, GetMetadataOptions, ReadDirectoryEntry, ReadFileOptions,
    RemoveOptions, WalkEntry, WalkEntryKind, WalkError, WalkOptions, WalkOutcome, WriteFileOptions,
};
mod local_file_system;
mod local_file_system_read;
mod no_follow;
mod regular_file;
pub use local_file_system::{LOCAL_FS, LocalFileSystem};

mod runtime_options;
pub use runtime_options::{ExecServerRuntimeOptions, LinuxSandboxPidNamespace};
mod fs_helper;
mod fs_helper_main;
mod fs_sandbox;
#[cfg(any(target_os = "linux", feature = "test-api"))]
mod helper_linux;
mod sandboxed_file_open;
mod sandboxed_file_system;
mod wire_error;
pub use fs_helper::CODEX_FS_HELPER_ARG1;
pub use fs_helper_main::main as run_fs_helper_main;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api;
