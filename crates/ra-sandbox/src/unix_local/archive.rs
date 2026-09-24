//! Streaming the workspace out as a tar, and back in.
//!
//! This is how a workspace moves: a snapshot writes one of these somewhere durable, a resume reads
//! it back, and a session on another backend can be handed the same bytes. Member paths are
//! workspace-relative — `./src/main.rs`, never the host directory a provider happened to pick — so
//! an archive taken here extracts into a container without carrying this machine's layout with it.
//!
//! # Extraction is the dangerous direction
//!
//! An archive is untrusted input: it decides where its own members land. A member named `../../` or
//! one that is a symlink to `/etc` is how an extractor is made to write outside the directory it was
//! pointed at. Everything in [`hydrate`] before the first byte is written is there for that, and it
//! is checked before extraction starts rather than member by member, so a refusal leaves the
//! workspace untouched instead of half-written.
//!
//! The reference's extractor also enforces archive size and member-count limits and a wider set of
//! member-kind rules. Those arrive with the task that ports snapshots, where the limits are
//! configured; what is here is the containment half, which cannot wait for it.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ra_core::sandbox::{PosixPath, SandboxError};

/// Writes the workspace into a tar stream.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveReadError`] when the workspace root is
/// missing or cannot be walked.
pub(crate) fn persist(root: &Path, skip: &BTreeSet<PosixPath>) -> Result<Vec<u8>, SandboxError> {
    let rendered = root.to_string_lossy().into_owned();
    if !root.exists() {
        return Err(SandboxError::workspace_archive_read(&rendered)
            .with_context("reason", "workspace_root_not_found"));
    }

    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let failure = |error: &std::io::Error| {
        SandboxError::workspace_archive_read(&rendered).with_context("os_error", error.to_string())
    };
    builder
        .append_dir(Path::new("."), root)
        .map_err(|error| failure(&error))?;
    append_children(&mut builder, root, Path::new(""), skip).map_err(|error| failure(&error))?;
    builder.into_inner().map_err(|error| failure(&error))
}

/// Walks one directory, adding what is not excluded.
fn append_children(
    builder: &mut tar::Builder<Vec<u8>>,
    directory: &Path,
    relative: &Path,
    skip: &BTreeSet<PosixPath>,
) -> std::io::Result<()> {
    let mut children: Vec<_> = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    // Sorted so two archives of the same workspace are the same bytes, which is what makes a
    // fingerprint comparison mean anything.
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let name = child.file_name();
        let child_relative = relative.join(&name);
        if is_skipped(&child_relative, skip) {
            continue;
        }
        let archive_name = Path::new(".").join(&child_relative);
        let path = child.path();
        let metadata = path.symlink_metadata()?;

        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder.append_link(&mut header, &archive_name, &target)?;
        } else if metadata.is_dir() {
            builder.append_dir(&archive_name, &path)?;
            append_children(builder, &path, &child_relative, skip)?;
        } else if metadata.is_file() {
            let mut file = std::fs::File::open(&path)?;
            builder.append_file(&archive_name, &mut file)?;
        }
        // Anything else — a socket, a device, a fifo — is not workspace content and is left out
        // rather than archived as something it is not.
    }
    Ok(())
}

/// Whether a workspace-relative path is one the manifest asked not to persist.
fn is_skipped(relative: &Path, skip: &BTreeSet<PosixPath>) -> bool {
    let rendered = PosixPath::coerce(&relative.to_string_lossy());
    skip.iter().any(|prefix| rendered.is_under(prefix))
}

/// Replaces the workspace with the contents of a tar stream.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] when the archive is not
/// readable, when a member would land outside the workspace, or when writing one fails.
pub(crate) fn hydrate(root: &Path, data: &[u8]) -> Result<(), SandboxError> {
    let rendered = root.to_string_lossy().into_owned();
    let refuse = |reason: &str, member: &str| {
        SandboxError::workspace_archive_write(&rendered)
            .with_context("reason", reason)
            .with_context("member", member)
    };
    let failure = |error: &std::io::Error| {
        SandboxError::workspace_archive_write(&rendered).with_context("os_error", error.to_string())
    };

    std::fs::create_dir_all(root).map_err(|error| failure(&error))?;
    let resolved_root = crate::host_paths::resolve_without_strictness(root).map_err(|error| {
        SandboxError::workspace_archive_write(&rendered).with_sandbox_cause(error)
    })?;

    // Two passes. The first decides whether the archive is safe at all; only then does anything get
    // written, so a refusal cannot leave a workspace half replaced by an archive that was going to
    // be rejected anyway.
    let plan = validate(data, &refuse)?;
    for member in &plan {
        let destination = resolved_root.join(&member.relative);
        ensure_no_symlink_parents(&resolved_root, &destination, &refuse, &member.name)?;
        match &member.content {
            Content::Directory => {
                std::fs::create_dir_all(&destination).map_err(|error| failure(&error))?;
            }
            Content::File { bytes, mode } => {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| failure(&error))?;
                }
                replace_leaf(
                    &destination,
                    &member.relative,
                    &member.name,
                    &refuse,
                    &failure,
                )?;
                std::fs::write(&destination, bytes).map_err(|error| failure(&error))?;
                set_restored_mode(&destination, *mode).map_err(|error| failure(&error))?;
            }
            Content::Symlink { target } => {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| failure(&error))?;
                }
                replace_leaf(
                    &destination,
                    &member.relative,
                    &member.name,
                    &refuse,
                    &failure,
                )?;
                std::os::unix::fs::symlink(target, &destination)
                    .map_err(|error| failure(&error))?;
            }
        }
    }
    Ok(())
}

/// What one archive member will become on disk.
struct Member {
    name: String,
    relative: PathBuf,
    content: Content,
}

/// The three kinds of member that are allowed to exist.
enum Content {
    Directory,
    File { bytes: Vec<u8>, mode: u32 },
    Symlink { target: PathBuf },
}

/// Reads the archive and refuses anything that would write outside the workspace.
fn validate(
    data: &[u8],
    refuse: &impl Fn(&str, &str) -> SandboxError,
) -> Result<Vec<Member>, SandboxError> {
    let mut archive = tar::Archive::new(data);
    let entries = archive
        .entries()
        .map_err(|error| refuse("unreadable_archive", &error.to_string()))?;

    let mut members: Vec<Member> = Vec::new();
    let mut symlink_members: Vec<PathBuf> = Vec::new();

    for entry in entries {
        let mut entry = entry.map_err(|error| refuse("unreadable_member", &error.to_string()))?;
        let raw = entry
            .path()
            .map_err(|error| refuse("unreadable_member_path", &error.to_string()))?
            .into_owned();
        let name = raw.to_string_lossy().into_owned();
        let relative =
            workspace_relative(&raw).ok_or_else(|| refuse("path_escapes_root", &name))?;

        let kind = entry.header().entry_type();
        if relative.as_os_str().is_empty() {
            if kind.is_dir() {
                continue;
            }
            let reason = if kind.is_symlink() {
                "archive root symlink"
            } else if kind.is_hard_link() {
                "archive root hardlink"
            } else {
                "archive root member must be directory"
            };
            return Err(refuse(reason, &name));
        }

        // A member under an earlier symlink member would be written through that link, which is a
        // way of naming a destination outside the workspace without the member path saying so.
        if symlink_members
            .iter()
            .any(|link| relative.starts_with(link) && relative != *link)
        {
            return Err(refuse("member_under_symlink", &name));
        }

        let content = if kind.is_dir() {
            Content::Directory
        } else if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|error| refuse("unreadable_link_target", &error.to_string()))?
                .ok_or_else(|| refuse("missing_link_target", &name))?
                .into_owned();
            if !symlink_target_stays_inside(&relative, &target) {
                return Err(refuse("symlink_target_escapes_root", &name));
            }
            symlink_members.push(relative.clone());
            Content::Symlink { target }
        } else if kind.is_file() {
            let mode = entry.header().mode().unwrap_or(0o644);
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| refuse("unreadable_member_payload", &error.to_string()))?;
            Content::File { bytes, mode }
        } else {
            // Hardlinks, devices, fifos: a workspace archive has no business carrying them, and
            // a hardlink in particular names a destination the archive did not have to declare.
            return Err(refuse("unsupported_member_kind", &name));
        };

        members.push(Member {
            name,
            relative,
            content,
        });
    }
    Ok(members)
}

/// Re-measures a member path from the workspace root, or refuses it.
///
/// Absolute members and `..` are both refused outright rather than normalized away: a normalized
/// `a/../../b` still asked to leave, and accepting it would mean accepting every path that happens
/// to climb back in.
fn workspace_relative(path: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

/// Whether a symlink member points at something inside the workspace.
fn symlink_target_stays_inside(relative: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = relative.parent().map_or(0, |parent| {
        i64::try_from(parent.components().count()).unwrap_or(i64::MAX)
    });
    for component in target.components() {
        match component {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Refuses a destination reached through a symlink that already exists on disk.
fn ensure_no_symlink_parents(
    root: &Path,
    destination: &Path,
    refuse: &impl Fn(&str, &str) -> SandboxError,
    name: &str,
) -> Result<(), SandboxError> {
    let Ok(relative) = destination.strip_prefix(root) else {
        return Err(refuse("path_escapes_root", name));
    };
    let mut walked = root.to_owned();
    for component in relative.components() {
        walked.push(component);
        if walked == destination {
            break;
        }
        if walked
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(refuse("parent_is_symlink", name));
        }
    }
    Ok(())
}

/// Removes a replaceable leaf, preserving directories and their contents on conflicts.
fn replace_leaf(
    destination: &Path,
    relative: &Path,
    name: &str,
    refuse: &impl Fn(&str, &str) -> SandboxError,
    failure: &impl Fn(&std::io::Error) -> SandboxError,
) -> Result<(), SandboxError> {
    match destination.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => Err(refuse(
            &format!(
                "destination directory already exists: {}",
                relative.display()
            ),
            name,
        )),
        Ok(_) => std::fs::remove_file(destination).map_err(|error| failure(&error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(failure(&error)),
    }
}

/// Applies the permission bits an extracted file is allowed to keep.
///
/// The reference's policy, and the standard library filter's before it: setuid, setgid, the sticky
/// bit and group or other write access are dropped, execute survives only where the owner had it,
/// and the owner can always read and write what was extracted.
fn set_restored_mode(destination: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut restored = mode & 0o755;
    if restored & 0o100 == 0 {
        restored &= !0o111;
    }
    std::fs::set_permissions(
        destination,
        std::fs::Permissions::from_mode(restored | 0o600),
    )
}
