// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
#![allow(
    clippy::too_many_lines,
    reason = "Keep the source lifecycle and policy lowering order visible in one operation."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ra_executor_protocol::JSONRPCErrorError;
use ra_paths::PathUri;
use serde::Deserialize;
use serde::Serialize;
use tokio::io;

use super::CopyOptions;
use super::CreateDirectoryOptions;
use super::ExecutorFileSystem;
use super::GetMetadataOptions;
use super::ReadFileOptions;
use super::RemoveOptions;
use super::WriteFileOptions;
use super::local_file_system::DirectFileSystem;
use super::protocol::FS_CANONICALIZE_METHOD;
use super::protocol::FS_COPY_METHOD;
use super::protocol::FS_CREATE_DIRECTORY_METHOD;
use super::protocol::FS_GET_METADATA_METHOD;
use super::protocol::FS_OPEN_METHOD;
use super::protocol::FS_READ_DIRECTORY_METHOD;
use super::protocol::FS_READ_FILE_METHOD;
use super::protocol::FS_REMOVE_METHOD;
use super::protocol::FS_WALK_METHOD;
use super::protocol::FS_WRITE_FILE_METHOD;
use super::protocol::FsCanonicalizeParams;
use super::protocol::FsCanonicalizeResponse;
use super::protocol::FsCopyParams;
use super::protocol::FsCopyResponse;
use super::protocol::FsCreateDirectoryParams;
use super::protocol::FsCreateDirectoryResponse;
use super::protocol::FsGetMetadataParams;
use super::protocol::FsGetMetadataResponse;
use super::protocol::FsOpenMode;
use super::protocol::FsReadDirectoryEntry;
use super::protocol::FsReadDirectoryParams;
use super::protocol::FsReadDirectoryResponse;
use super::protocol::FsReadFileParams;
use super::protocol::FsReadFileResponse;
use super::protocol::FsRemoveParams;
use super::protocol::FsRemoveResponse;
use super::protocol::FsWalkParams;
use super::protocol::FsWalkResponse;
use super::protocol::FsWriteFileParams;
use super::protocol::FsWriteFileResponse;
use super::wire_error::internal_error;
use super::wire_error::invalid_request;
use super::wire_error::not_found;

pub const CODEX_FS_HELPER_ARG1: &str = "--codex-run-as-fs-helper";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params")]
pub(crate) enum FsHelperRequest {
    #[serde(rename = "fs/open")]
    Open(FsHelperOpenParams),
    #[serde(rename = "fs/readFile")]
    ReadFile(FsReadFileParams),
    #[serde(rename = "fs/writeFile")]
    WriteFile(FsWriteFileParams),
    #[serde(rename = "fs/createDirectory")]
    CreateDirectory(FsCreateDirectoryParams),
    #[serde(rename = "fs/getMetadata")]
    GetMetadata(FsGetMetadataParams),
    #[serde(rename = "fs/canonicalize")]
    Canonicalize(FsCanonicalizeParams),
    #[serde(rename = "fs/readDirectory")]
    ReadDirectory(FsReadDirectoryParams),
    #[serde(rename = "fs/walk")]
    Walk(FsWalkParams),
    #[serde(rename = "fs/remove")]
    Remove(FsRemoveParams),
    #[serde(rename = "fs/copy")]
    Copy(FsCopyParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FsHelperOpenParams {
    pub(crate) path: PathUri,
    #[serde(default)]
    pub(crate) mode: FsOpenMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "camelCase")]
pub(crate) enum FsHelperResponse {
    Ok(FsHelperPayload),
    Error(JSONRPCErrorError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FsHelperOpenResponse {
    // Windows duplicates the handle from the helper process.
    #[cfg(windows)]
    pub(crate) process_id: u32,
    // Unix passes the fd directly instead.
    #[cfg(windows)]
    pub(crate) file_handle: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "response")]
pub(crate) enum FsHelperPayload {
    #[serde(rename = "fs/open")]
    Open(FsHelperOpenResponse),
    #[serde(rename = "fs/readFile")]
    ReadFile(FsReadFileResponse),
    #[serde(rename = "fs/writeFile")]
    WriteFile(FsWriteFileResponse),
    #[serde(rename = "fs/createDirectory")]
    CreateDirectory(FsCreateDirectoryResponse),
    #[serde(rename = "fs/getMetadata")]
    GetMetadata(FsGetMetadataResponse),
    #[serde(rename = "fs/canonicalize")]
    Canonicalize(FsCanonicalizeResponse),
    #[serde(rename = "fs/readDirectory")]
    ReadDirectory(FsReadDirectoryResponse),
    #[serde(rename = "fs/walk")]
    Walk(FsWalkResponse),
    #[serde(rename = "fs/remove")]
    Remove(FsRemoveResponse),
    #[serde(rename = "fs/copy")]
    Copy(FsCopyResponse),
}

impl FsHelperPayload {
    fn operation(&self) -> &'static str {
        match self {
            Self::Open(_) => FS_OPEN_METHOD,
            Self::ReadFile(_) => FS_READ_FILE_METHOD,
            Self::WriteFile(_) => FS_WRITE_FILE_METHOD,
            Self::CreateDirectory(_) => FS_CREATE_DIRECTORY_METHOD,
            Self::GetMetadata(_) => FS_GET_METADATA_METHOD,
            Self::Canonicalize(_) => FS_CANONICALIZE_METHOD,
            Self::ReadDirectory(_) => FS_READ_DIRECTORY_METHOD,
            Self::Walk(_) => FS_WALK_METHOD,
            Self::Remove(_) => FS_REMOVE_METHOD,
            Self::Copy(_) => FS_COPY_METHOD,
        }
    }

    pub(crate) fn expect_read_file(self) -> Result<FsReadFileResponse, JSONRPCErrorError> {
        match self {
            Self::ReadFile(response) => Ok(response),
            other => Err(unexpected_response(FS_READ_FILE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_write_file(self) -> Result<FsWriteFileResponse, JSONRPCErrorError> {
        match self {
            Self::WriteFile(response) => Ok(response),
            other => Err(unexpected_response(FS_WRITE_FILE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_create_directory(
        self,
    ) -> Result<FsCreateDirectoryResponse, JSONRPCErrorError> {
        match self {
            Self::CreateDirectory(response) => Ok(response),
            other => Err(unexpected_response(
                FS_CREATE_DIRECTORY_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_get_metadata(self) -> Result<FsGetMetadataResponse, JSONRPCErrorError> {
        match self {
            Self::GetMetadata(response) => Ok(response),
            other => Err(unexpected_response(
                FS_GET_METADATA_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_canonicalize(self) -> Result<FsCanonicalizeResponse, JSONRPCErrorError> {
        match self {
            Self::Canonicalize(response) => Ok(response),
            other => Err(unexpected_response(
                FS_CANONICALIZE_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_read_directory(
        self,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        match self {
            Self::ReadDirectory(response) => Ok(response),
            other => Err(unexpected_response(
                FS_READ_DIRECTORY_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_walk(self) -> Result<FsWalkResponse, JSONRPCErrorError> {
        match self {
            Self::Walk(response) => Ok(response),
            other => Err(unexpected_response(FS_WALK_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_remove(self) -> Result<FsRemoveResponse, JSONRPCErrorError> {
        match self {
            Self::Remove(response) => Ok(response),
            other => Err(unexpected_response(FS_REMOVE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_copy(self) -> Result<FsCopyResponse, JSONRPCErrorError> {
        match self {
            Self::Copy(response) => Ok(response),
            other => Err(unexpected_response(FS_COPY_METHOD, other.operation())),
        }
    }
}

fn unexpected_response(expected: &str, actual: &str) -> JSONRPCErrorError {
    internal_error(format!(
        "unexpected fs sandbox helper response: expected {expected}, got {actual}"
    ))
}

pub(crate) async fn run_direct_request(
    request: FsHelperRequest,
) -> Result<FsHelperPayload, JSONRPCErrorError> {
    let file_system = DirectFileSystem;
    match request {
        FsHelperRequest::Open(_) => Err(invalid_request(
            "opening a file requires descriptor handoff".to_string(),
        )),
        FsHelperRequest::ReadFile(params) => {
            let data = file_system
                .read_file(
                    &params.path,
                    ReadFileOptions {
                        follow_symlinks: params.follow_symlinks.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::ReadFile(FsReadFileResponse {
                data_base64: STANDARD.encode(data),
            }))
        }
        FsHelperRequest::WriteFile(params) => {
            let bytes = STANDARD.decode(params.data_base64).map_err(|err| {
                invalid_request(format!(
                    "{FS_WRITE_FILE_METHOD} requires valid base64 dataBase64: {err}"
                ))
            })?;
            file_system
                .write_file(
                    &params.path,
                    bytes,
                    WriteFileOptions {
                        follow_symlinks: params.follow_symlinks.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::WriteFile(FsWriteFileResponse {}))
        }
        FsHelperRequest::CreateDirectory(params) => {
            file_system
                .create_directory(
                    &params.path,
                    CreateDirectoryOptions {
                        recursive: params.recursive.unwrap_or(true),
                        follow_symlinks: params.follow_symlinks.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::CreateDirectory(
                FsCreateDirectoryResponse {},
            ))
        }
        FsHelperRequest::GetMetadata(params) => {
            let metadata = file_system
                .get_metadata(
                    &params.path,
                    GetMetadataOptions {
                        follow_symlinks: params.follow_symlinks.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::GetMetadata(FsGetMetadataResponse {
                is_directory: metadata.is_directory,
                is_file: metadata.is_file,
                is_symlink: metadata.is_symlink,
                size: metadata.size,
                created_at_ms: metadata.created_at_ms,
                modified_at_ms: metadata.modified_at_ms,
            }))
        }
        FsHelperRequest::Canonicalize(params) => {
            let path = file_system
                .canonicalize(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Canonicalize(FsCanonicalizeResponse {
                path,
            }))
        }
        FsHelperRequest::ReadDirectory(params) => {
            let entries = file_system
                .read_directory(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?
                .into_iter()
                .map(|entry| FsReadDirectoryEntry {
                    file_name: entry.file_name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                })
                .collect();
            Ok(FsHelperPayload::ReadDirectory(FsReadDirectoryResponse {
                entries,
            }))
        }
        FsHelperRequest::Walk(params) => {
            let outcome = file_system
                .walk(&params.path, params.options, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Walk(outcome))
        }
        FsHelperRequest::Remove(params) => {
            file_system
                .remove(
                    &params.path,
                    RemoveOptions {
                        recursive: params.recursive.unwrap_or(true),
                        force: params.force.unwrap_or(true),
                        follow_symlinks: params.follow_symlinks.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Remove(FsRemoveResponse {}))
        }
        FsHelperRequest::Copy(params) => {
            file_system
                .copy(
                    &params.source_path,
                    &params.destination_path,
                    CopyOptions {
                        recursive: params.recursive,
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Copy(FsCopyResponse {}))
        }
    }
}

pub(crate) fn map_fs_error(err: io::Error) -> JSONRPCErrorError {
    match err.kind() {
        io::ErrorKind::NotFound => not_found(err.to_string()),
        io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied => {
            invalid_request(err.to_string())
        }
        _ => internal_error(err.to_string()),
    }
}
