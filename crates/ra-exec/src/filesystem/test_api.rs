//! Hidden bridges to the real native algorithms used by source tests.
#![allow(missing_docs, reason = "Hidden source-test facade access.")]
pub mod fs_sandbox {
    pub use super::super::fs_sandbox::{
        FileSystemSandboxRunner, SandboxCwd, add_helper_runtime_permissions,
        bind_windows_cwd_relative_deny_read_globs, helper_env, helper_env_from_vars,
        helper_env_key_is_allowed, helper_read_roots, sandbox_cwd,
    };
    pub use super::super::{ExecServerRuntimeOptions, sandboxed_file_system::SandboxedFileSystem};
    pub use ra_filesystem::ExecutorFileSystem;
    pub use ra_filesystem::FileSystemSandboxContext;
}
pub mod local_file_system {
    #[cfg(windows)]
    pub use super::super::local_file_system::symlink_points_to_directory;
    pub use super::super::local_file_system::{
        DirectFileSystem, LOCAL_FS, UnsandboxedFileSystem, push_walk_error,
        reserve_walk_response_bytes, resolve_existing_path, walk_read_directory,
    };
    pub use ra_filesystem::ExecutorFileSystem;
}
pub mod regular_file {
    pub use super::super::regular_file::*;
}
pub mod local_file_system_read {
    pub use super::super::local_file_system_read::{CancellableReader, MAX_READ_FILE_BYTES};
}
pub mod helper_linux {
    pub use super::super::helper_linux::*;
}
