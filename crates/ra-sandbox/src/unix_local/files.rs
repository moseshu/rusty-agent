//! The workspace as a directory: listing, creating, removing, reading and writing.
//!
//! Every operation here acts on the host filesystem directly, because for this backend that *is*
//! the workspace. The path it acts on has already been through the resolving policy, so a symlink
//! that points out of the workspace has already been refused.
//!
//! # Running as another account is a check, not a channel
//!
//! When a caller names a user, the reference does not do the work through `sudo`; it asks, as that
//! user, whether the operation would be permitted, and then does it locally. The shell scripts that
//! ask are here. Doing the work through `sudo` instead would mean the SDK could write files the
//! account running it cannot read back, which is a workspace only half of the process can use.

use std::path::Path;

use ra_core::sandbox::{EntryKind, ExecResult, FileEntry, Permissions, SandboxError};

/// Asks whether a file is readable by the account the check runs as.
pub(crate) const READ_ACCESS_CHECK_SCRIPT: &str = r#"[ -r "$1" ]"#;

/// How long the existence probe may take before a refused read is reported as a read failure.
pub(crate) const READ_PATH_PROBE_TIMEOUT_S: f64 = 10.0;

/// Tells "is not there" from "may not be looked at", as the account that was refused.
///
/// A failed `[ -r ]` means either, and the difference is the difference between a not-found and a
/// read failure. The probe resolves the path's symlinks itself — as that account, so a link the
/// account cannot follow is not followed — and then walks up to the deepest ancestor that exists.
/// Exit 0 is "it is there after all", 1 is "the account can see that it is missing", and 2 is
/// everything it could not decide, including a parent the account may not search.
///
/// The reference's script, byte for byte; the marker on its first line is how a copy is recognised.
pub(crate) const READ_PATH_PROBE_SCRIPT: &str = r#"# READ_PATH_PROBE_V3
LC_ALL=C
export LC_ALL
path=$1
resolved_path=
symlink_depth=0

resolve_probe_path() {
    if [ "$symlink_depth" -gt 40 ] || [ "${#1}" -gt 4095 ]; then
        return 2
    fi
    if [ "$1" = "/" ]; then
        resolved_path=/
        return 0
    fi

    parent=${1%/*}
    if [ -z "$parent" ] || [ "$parent" = "$1" ]; then
        parent=/
    fi
    resolve_probe_path "$parent" || return 2
    resolved_parent=$resolved_path
    base=${1##*/}
    if [ "${#base}" -gt 255 ]; then
        return 2
    fi
    if [ "$resolved_parent" = "/" ]; then
        candidate=/$base
    else
        candidate=$resolved_parent/$base
    fi

    if [ -L "$candidate" ]; then
        target_with_marker=$(readlink -n "$candidate" && printf .) || return 2
        target=${target_with_marker%.}
        symlink_depth=$((symlink_depth + 1))
        if [ "$symlink_depth" -gt 40 ]; then
            return 2
        fi
        case "$target" in
            /*)
                resolve_probe_path "$target"
                ;;
            *)
                resolve_probe_path "$resolved_parent/$target"
                ;;
        esac
        return $?
    fi

    resolved_path=$candidate
}

resolve_probe_path "$path" || exit 2
path=$resolved_path
candidate=$path
child=

while :; do
    if [ -e "$candidate" ]; then
        if [ "$candidate" = "$path" ]; then
            exit 0
        fi
        if [ ! -d "$candidate" ] || [ ! -x "$candidate" ]; then
            exit 2
        fi
        lookup_result=$(
            find "$child" -prune -print 2>&1 >/dev/null
            lookup_status=$?
            printf '.%s' "$lookup_status"
        )
        lookup_status=${lookup_result##*.}
        lookup_error=${lookup_result%.*}
        if [ "$lookup_status" -eq 1 ]; then
            lookup_error=$(printf %s "$lookup_error")
            case "$lookup_error" in
                *": No such file or directory")
                    exit 1
                    ;;
            esac
        fi
        exit 2
    fi
    if [ "$candidate" = "/" ]; then
        exit 2
    fi
    child=$candidate
    candidate=${candidate%/*}
    if [ -z "$candidate" ]; then
        candidate=/
    fi
done"#;

/// How much of a command's error output a failure keeps.
const DIAGNOSTIC_MAX_CHARS: usize = 4096;

/// Decodes a command's output for an error context, truncated the way the reference truncates it.
///
/// Counted in characters rather than bytes, and marked with an ellipsis when cut, so a long stream
/// of output is still readable text and still says that it is not all there.
pub(crate) fn diagnostic_text(bytes: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    match decoded.char_indices().nth(DIAGNOSTIC_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &decoded[..cut]),
        None => decoded.into_owned(),
    }
}

/// Asks whether a directory could be created; the second argument says whether parents may be too.
pub(crate) const MKDIR_ACCESS_CHECK_SCRIPT: &str = concat!(
    "target=\"$1\"\n",
    "parents=\"$2\"\n",
    "if [ -e \"$target\" ] || [ -L \"$target\" ]; then\n",
    "    [ -d \"$target\" ] && [ -x \"$target\" ]\n",
    "    exit $?\n",
    "fi\n",
    "parent=$(dirname \"$target\")\n",
    "if [ \"$parents\" = \"1\" ]; then\n",
    "    while [ ! -e \"$parent\" ]; do\n",
    "        next=$(dirname \"$parent\")\n",
    "        if [ \"$next\" = \"$parent\" ]; then\n",
    "            exit 1\n",
    "        fi\n",
    "        parent=\"$next\"\n",
    "    done\n",
    "fi\n",
    "[ -d \"$parent\" ] && [ -w \"$parent\" ] && [ -x \"$parent\" ]\n",
);

/// Asks whether a path could be removed; the second argument says whether a missing one is fine.
pub(crate) const RM_ACCESS_CHECK_SCRIPT: &str = concat!(
    "target=\"$1\"\n",
    "recursive=\"$2\"\n",
    "if [ ! -e \"$target\" ] && [ ! -L \"$target\" ]; then\n",
    "    [ \"$recursive\" = \"1\" ]\n",
    "    exit $?\n",
    "fi\n",
    "parent=$(dirname \"$target\")\n",
    "[ -d \"$parent\" ] && [ -w \"$parent\" ] && [ -x \"$parent\" ]\n",
);

/// Lists a directory by reading it, rather than by running `ls`.
///
/// Symlinks are reported as symlinks instead of as whatever they point at: a caller deciding what
/// to do with an entry needs to know it is a link, and a listing that silently followed one would
/// describe a file that is not in this directory at all.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] shaped as the `ls` that would have failed,
/// which is what the reference reports so that a local listing and a remote one fail alike.
pub(crate) fn list_directory(path: &Path) -> Result<Vec<FileEntry>, SandboxError> {
    let rendered = path.to_string_lossy().into_owned();
    let failure = |error: &std::io::Error| {
        SandboxError::exec_nonzero(
            ExecResult::new(Vec::new(), error.to_string().into_bytes(), 1),
            vec![
                "ls".to_owned(),
                "-la".to_owned(),
                "--".to_owned(),
                rendered.clone(),
            ],
        )
    };

    let mut listed = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| failure(&error))? {
        let entry = entry.map_err(|error| failure(&error))?;
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|error| failure(&error))?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        listed.push(
            FileEntry::new(
                entry.path().to_string_lossy().into_owned(),
                permissions_of(&metadata),
            )
            .with_ownership(owner_of(&metadata), group_of(&metadata))
            .with_size(size_of(&metadata))
            .with_kind(kind),
        );
    }
    Ok(listed)
}

/// The permission bits and directory flag a listing reports.
fn permissions_of(metadata: &std::fs::Metadata) -> Permissions {
    use std::os::unix::fs::MetadataExt;

    Permissions::from_mode(metadata.mode())
}

/// The owning account, as the numeric id the filesystem stores.
///
/// The reference reports the number rather than looking the name up, and so does this: resolving it
/// would consult this host's account database to describe a workspace that may not share it.
fn owner_of(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    metadata.uid().to_string()
}

/// The owning group, as the numeric id the filesystem stores.
fn group_of(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    metadata.gid().to_string()
}

/// The size a listing reports.
fn size_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.size()
}

/// Creates a directory, and its parents when asked.
///
/// An existing directory is success, as it is for the reference: materializing a manifest twice
/// must not fail the second time.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] when the directory could not
/// be created.
pub(crate) fn make_directory(path: &Path, parents: bool) -> Result<(), SandboxError> {
    let result = if parents {
        std::fs::create_dir_all(path)
    } else {
        match std::fs::create_dir(path) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
                Ok(())
            }
            other => other,
        }
    };
    result.map_err(|error| write_failure(path, &error))
}

/// Removes a path, and everything under it when asked.
///
/// A directory is removed as a directory and a symlink to one is removed as a link, which is the
/// difference between deleting a shortcut and deleting what it points at.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] for a non-recursive removal of something
/// that is not there — a recursive one treats it as already done — and
/// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] for any other failure.
pub(crate) fn remove(path: &Path, recursive: bool) -> Result<(), SandboxError> {
    let metadata = path.symlink_metadata();
    let is_directory = metadata
        .as_ref()
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());

    let result = if is_directory {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };

    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if recursive {
                return Ok(());
            }
            Err(SandboxError::exec_nonzero(
                ExecResult::new(Vec::new(), error.to_string().into_bytes(), 1),
                vec![
                    "rm".to_owned(),
                    "--".to_owned(),
                    path.to_string_lossy().into_owned(),
                ],
            ))
        }
        Err(error) => Err(write_failure(path, &error)),
    }
}

/// Reads a file out of the workspace.
///
/// `requested` is the path the caller named, which is what a refusal quotes: a caller that asked
/// for `notes.md` should not be told about a provider's temporary directory.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceReadNotFound`] when the file is not there, and
/// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveReadError`] for any other failure.
pub(crate) fn read_file(path: &Path, requested: &str) -> Result<Vec<u8>, SandboxError> {
    std::fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SandboxError::workspace_read_not_found(requested).with_cause(error)
        } else {
            SandboxError::workspace_archive_read(requested)
                .with_context("os_error", error.to_string())
                .with_cause(error)
        }
    })
}

/// Writes a file into the workspace, creating the directories above it.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] when the file could not be
/// written.
pub(crate) fn write_file(path: &Path, data: &[u8]) -> Result<(), SandboxError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| write_failure(path, &error))?;
    }
    std::fs::write(path, data).map_err(|error| write_failure(path, &error))
}

/// Reports a workspace write that did not happen.
fn write_failure(path: &Path, error: &std::io::Error) -> SandboxError {
    SandboxError::workspace_archive_write(&path.to_string_lossy())
        .with_context("os_error", error.to_string())
}
