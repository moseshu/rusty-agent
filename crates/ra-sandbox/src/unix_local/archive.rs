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
//! No size or member-count limit applies here, as none applies in the reference: limits belong to
//! an archive a caller hands to `extract`, and a restore reads back an archive a snapshot wrote.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ra_core::sandbox::{PosixPath, SandboxError};

use crate::archive::windows_drive;
use crate::host_paths::resolve_without_strictness;

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
/// The reference's `safe_extract_tarfile`, run as its local backend runs it: symlink members are
/// restored, but only when their targets stay inside the archive. The whole archive is checked
/// before the first member is written, so a refusal leaves the workspace as it was; directories
/// and files are written next, and symlinks last, so nothing the archive carries is ever written
/// through a link the same archive created. A gzip, bzip2 or xz stream is decompressed first, as
/// the reference's `r:*` reader does.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`]: with the reference's
/// `reason` and `member` in its context when a member is refused, and with `os_error` when the
/// archive cannot be read or a member cannot be written.
pub(crate) fn hydrate(root: &Path, data: &[u8]) -> Result<(), SandboxError> {
    let rendered = root.to_string_lossy().into_owned();
    extract(root, data).map_err(|error| {
        let refused = SandboxError::workspace_archive_write(&rendered);
        match error {
            ExtractError::Unsafe(member) => refused
                .with_context("reason", member.reason)
                .with_context("member", member.member),
            ExtractError::Io(error) => refused
                .with_context("os_error", error.to_string())
                .with_cause(error),
            ExtractError::Resolve(error) => refused.with_sandbox_cause(error),
        }
    })
}

/// Why an extraction stopped.
enum ExtractError {
    /// A member the archive may not carry, or may not write where it asked to.
    Unsafe(UnsafeMember),
    /// The archive could not be read, or a member could not be written.
    Io(std::io::Error),
    /// A destination's symlinks could not be resolved.
    Resolve(SandboxError),
}

impl From<std::io::Error> for ExtractError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<UnsafeMember> for ExtractError {
    fn from(member: UnsafeMember) -> Self {
        Self::Unsafe(member)
    }
}

/// A refused member: which one, and the reference's words for why.
struct UnsafeMember {
    member: String,
    reason: String,
}

impl UnsafeMember {
    fn new(member: &str, reason: impl Into<String>) -> Self {
        Self {
            member: member.to_owned(),
            reason: reason.into(),
        }
    }
}

/// One archive member, read out of the stream.
struct Member {
    /// Diagnostic text only; filesystem operations use the original bytes in `path`.
    name: String,
    /// The original Unix bytes, used for validation and filesystem operations.
    path: PathBuf,
    kind: Kind,
    link_name: PathBuf,
    mode: u32,
    bytes: Vec<u8>,
}

/// What a member is, in the terms the rules are written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Directory,
    File,
    Symlink,
    HardLink,
    Other,
}

/// Validates the archive, then writes it into `root`.
fn extract(root: &Path, data: &[u8]) -> Result<(), ExtractError> {
    std::fs::create_dir_all(root)?;
    let root = resolve_without_strictness(root).map_err(ExtractError::Resolve)?;

    let members = read_members(&decompressed(data)?)?;
    validate(&members)?;

    for member in &members {
        let Some(relative) = safe_member_rel_path(member)? else {
            continue;
        };
        let destination = root.join(&relative);
        match member.kind {
            Kind::Directory => {
                prepare_directory_leaf(&root, &destination)?;
                std::fs::create_dir_all(&destination)?;
            }
            Kind::File => write_file(&root, &destination, &relative, member)?,
            // Symlinks come after everything else; the other two kinds were refused above.
            Kind::Symlink | Kind::HardLink | Kind::Other => {}
        }
    }
    for member in &members {
        if member.kind != Kind::Symlink {
            continue;
        }
        let Some(relative) = safe_member_rel_path(member)? else {
            continue;
        };
        let destination = root.join(&relative);
        prepare_replaceable_leaf(&root, &destination, &relative, &member.name)?;
        std::os::unix::fs::symlink(&member.link_name, &destination)?;
    }
    Ok(())
}

/// The tar inside `data`, decompressed when it is a gzip, bzip2 or xz stream.
///
/// Every stream in the input is read, not only the first: the reference opens a restored
/// workspace in `r:*` mode, whose readers continue into a following gzip member, bzip2 stream or xz
/// stream (checked with Python 3.11). That is the opposite of the streaming reader an `extract` uses.
fn decompressed(data: &[u8]) -> std::io::Result<Cow<'_, [u8]>> {
    let mut decoder: Box<dyn Read + '_> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(data))
    } else if data.starts_with(b"BZh") {
        Box::new(bzip2::read::MultiBzDecoder::new(data))
    } else if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Box::new(lzma_rust2::XzReader::new(data, true))
    } else {
        return Ok(Cow::Borrowed(data));
    };
    let mut tar = Vec::new();
    decoder.read_to_end(&mut tar)?;
    Ok(Cow::Owned(tar))
}

/// Reads every member out of an uncompressed tar.
///
/// The tar crate handles local PAX records but does not inherit global ones. Collect extension
/// records first, then supply the effective attributes as one local header to its member reader.
/// In particular, `size` must take effect before reading a payload or locating the next header.
fn read_members(tar_bytes: &[u8]) -> std::io::Result<Vec<Member>> {
    let mut input = Cursor::new(tar_bytes);
    let mut global = BTreeMap::<String, Vec<u8>>::new();
    let mut pending = BTreeMap::<String, Vec<u8>>::new();
    let mut members = Vec::new();
    let mut needs_member = false;
    loop {
        let start = input.position();
        let mut raw = tar::Archive::new(&mut input);
        let mut entries = raw.entries()?.raw(true);
        let Some(entry) = entries.next() else {
            if needs_member {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "missing tar member after extension header",
                ));
            }
            break;
        };
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            needs_member = true;
            let mut attributes = BTreeMap::new();
            if let Some(extensions) = entry.pax_extensions()? {
                for extension in extensions {
                    let extension = extension?;
                    let key = extension.key().map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })?;
                    attributes.insert(key.to_owned(), extension.value_bytes().to_vec());
                }
            }
            if kind.is_pax_global_extensions() {
                global.extend(attributes);
            } else {
                for (key, value) in attributes {
                    pending.entry(key).or_insert(value);
                }
            }
        } else if kind.is_gnu_longname() || kind.is_gnu_longlink() {
            needs_member = true;
            let mut value = Vec::new();
            entry.read_to_end(&mut value)?;
            value.truncate(
                value
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(value.len()),
            );
            let key = if kind.is_gnu_longname() {
                "path"
            } else {
                "linkpath"
            };
            // An earlier extension wraps a later one in Python's recursive reader, so the
            // earlier name wins when GNU and local PAX names appear together.
            pending.entry(key.to_owned()).or_insert(value);
        } else {
            needs_member = false;
            drop(entry);
            input.set_position(start);
            let mut effective = global.clone();
            effective.append(&mut pending);
            let mut prefix = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut prefix);
                builder.append_pax_extensions(
                    effective
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_slice())),
                )?;
                // Capture only the extension, excluding the end-of-archive blocks emitted on drop.
                let length = builder.get_ref().len();
                drop(builder);
                prefix.truncate(length);
            }
            let mut archive = tar::Archive::new(prefix.as_slice().chain(&mut input));
            let mut entries = archive.entries()?;
            let mut entry = entries.next().transpose()?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "missing tar member")
            })?;
            members.push(read_member(&mut entry)?);
            // Consume non-file payloads too, so the cursor always points past this member.
            std::io::copy(&mut entry, &mut std::io::sink())?;
        }
        // Payloads are padded to a tar block. The underlying cursor also accounts for GNU sparse
        // extension blocks, which the member reader consumes before yielding its contents.
        let next = input.position().checked_add(511).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "tar offset overflow")
        })? & !511;
        input.set_position(next);
    }
    Ok(members)
}

/// Keeps path bytes intact, just as Python's surrogate-escaped Unix paths do.
fn read_member<R: Read>(entry: &mut tar::Entry<'_, R>) -> std::io::Result<Member> {
    let header = entry.header();
    let entry_type = header.entry_type();
    let old_style = header.as_old().linkflag[0] == 0;
    let mode = header.mode().unwrap_or(0o644);
    let mut raw_name = entry.path_bytes().into_owned();
    let link_name = entry.link_name_bytes().unwrap_or_default().into_owned();
    let mut kind = match entry_type {
        tar::EntryType::Directory => Kind::Directory,
        tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
            Kind::File
        }
        tar::EntryType::Symlink => Kind::Symlink,
        tar::EntryType::Link => Kind::HardLink,
        _ => Kind::Other,
    };
    if kind == Kind::File && old_style && raw_name.ends_with(b"/") {
        kind = Kind::Directory;
    }
    if kind == Kind::Directory {
        while raw_name.last() == Some(&b'/') {
            raw_name.pop();
        }
    }
    let mut bytes = Vec::new();
    if kind == Kind::File {
        entry.read_to_end(&mut bytes)?;
    }
    Ok(Member {
        name: String::from_utf8_lossy(&raw_name).into_owned(),
        path: PathBuf::from(std::ffi::OsStr::from_bytes(&raw_name)),
        kind,
        link_name: PathBuf::from(std::ffi::OsStr::from_bytes(&link_name)),
        mode,
        bytes,
    })
}

/// Validates one member's path and kind, or answers `None` for the archive's own root entry.
///
/// The reference's `safe_tar_member_rel_path` with symlink members allowed.
fn safe_member_rel_path(member: &Member) -> Result<Option<PathBuf>, UnsafeMember> {
    let name = member.path.as_os_str().as_bytes();
    let refuse = |reason: &str| Err(UnsafeMember::new(&member.name, reason));
    if matches!(name, b"" | b"." | b"./") {
        // Skipped rather than written, but only when it really is a directory: anything else
        // there is a claim about the destination itself.
        return match member.kind {
            Kind::Directory => Ok(None),
            Kind::Symlink => refuse("archive root symlink"),
            Kind::HardLink => refuse("archive root hardlink"),
            Kind::File | Kind::Other => refuse("archive root member must be directory"),
        };
    }
    if windows_drive(&member.name) {
        return refuse("windows drive path");
    }
    if name.contains(&b'\\') {
        return refuse("windows path separator");
    }
    if name.starts_with(b"/") {
        return refuse("absolute path");
    }
    let parts = posix_parts(name);
    if parts.contains(&b"..".as_slice()) {
        return refuse("parent traversal");
    }
    match member.kind {
        Kind::HardLink => refuse("hardlink member not allowed"),
        Kind::Other => refuse("unsupported member type"),
        Kind::Directory | Kind::File | Kind::Symlink => Ok(Some(
            parts
                .iter()
                .map(|part| std::ffi::OsStr::from_bytes(part))
                .collect(),
        )),
    }
}

/// A POSIX path's components, without the empty and `.` ones.
fn posix_parts(path: &[u8]) -> Vec<&[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect()
}

/// Renders a relative path with forward slashes, as the reference's reasons do.
fn as_posix(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Checks the archive as a whole before anything is written.
///
/// The reference's `validate_tarfile` with external symlink targets refused and nothing skipped or
/// protected: every member's path and kind, no path twice unless both are directories, no symlink
/// that points out of the archive, and no member beneath a symlink or beneath something that is not
/// a directory.
fn validate(members: &[Member]) -> Result<(), UnsafeMember> {
    let mut kinds: BTreeMap<PathBuf, Kind> = BTreeMap::new();
    let mut symlinks: BTreeSet<PathBuf> = BTreeSet::new();
    let mut checked: Vec<(&Member, PathBuf)> = Vec::new();

    for member in members {
        let Some(relative) = safe_member_rel_path(member)? else {
            continue;
        };
        if let Some(previous) = kinds.get(&relative)
            && !(*previous == Kind::Directory && member.kind == Kind::Directory)
        {
            return Err(UnsafeMember::new(
                &member.name,
                format!("duplicate archive path: {}", as_posix(&relative)),
            ));
        }
        kinds.insert(relative.clone(), member.kind);

        if member.kind == Kind::Symlink {
            validate_symlink_target(member, &relative)?;
            symlinks.insert(relative.clone());
        }
        checked.push((member, relative));
    }

    for (member, relative) in checked {
        for parent in relative
            .ancestors()
            .skip(1)
            .take_while(|parent| !parent.as_os_str().is_empty())
        {
            if symlinks.contains(parent) {
                return Err(UnsafeMember::new(
                    &member.name,
                    format!(
                        "archive path descends through symlink: {}",
                        as_posix(parent)
                    ),
                ));
            }
            if kinds
                .get(parent)
                .is_some_and(|kind| *kind != Kind::Directory)
            {
                return Err(UnsafeMember::new(
                    &member.name,
                    format!(
                        "archive path descends through non-directory: {}",
                        as_posix(parent)
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Refuses a symlink member whose target leaves the archive.
fn validate_symlink_target(member: &Member, relative: &Path) -> Result<(), UnsafeMember> {
    let target = member.link_name.as_os_str().as_bytes();
    let rendered_target = member.link_name.to_string_lossy();
    if target.starts_with(b"/") {
        return Err(UnsafeMember::new(
            &member.name,
            format!("absolute symlink target not allowed: {rendered_target}"),
        ));
    }
    // Normalized from the link's own directory, with `..` allowed as long as it never climbs above
    // the archive root on the way.
    let parent = relative
        .parent()
        .unwrap_or(Path::new(""))
        .as_os_str()
        .as_bytes();
    let mut depth: usize = 0;
    for part in posix_parts(parent).into_iter().chain(posix_parts(target)) {
        if part == b".." {
            let Some(shallower) = depth.checked_sub(1) else {
                return Err(UnsafeMember::new(
                    &member.name,
                    format!("symlink target escapes archive root: {rendered_target}"),
                ));
            };
            depth = shallower;
        } else {
            depth += 1;
        }
    }
    Ok(())
}

/// Refuses a destination whose parent resolves outside the root, or passes through a symlink that
/// is already on disk.
///
/// The reference's `_ensure_no_symlink_parents` with the leaf left out: the leaf is about to be
/// replaced, and whether it is a link is the caller's question.
fn ensure_no_symlink_parents(root: &Path, destination: &Path) -> Result<(), ExtractError> {
    let parent = destination.parent().unwrap_or(root);
    let resolved = resolve_without_strictness(parent).map_err(ExtractError::Resolve)?;
    if !resolved.starts_with(root) {
        return Err(UnsafeMember::new(
            &destination.to_string_lossy(),
            "path escapes root after resolution",
        )
        .into());
    }

    let relative = destination.strip_prefix(root).unwrap_or(destination);
    let components: Vec<_> = relative.components().collect();
    let mut walked = root.to_owned();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        walked.push(component);
        if walked.exists() && walked.is_symlink() {
            return Err(UnsafeMember::new(&as_posix(relative), "symlink in parent path").into());
        }
    }
    Ok(())
}

/// Makes room for a directory: a link or a file in its place is removed, a directory is kept.
fn prepare_directory_leaf(root: &Path, destination: &Path) -> Result<(), ExtractError> {
    ensure_no_symlink_parents(root, destination)?;
    if destination.is_symlink() || (destination.exists() && !destination.is_dir()) {
        std::fs::remove_file(destination)?;
    }
    Ok(())
}

/// Makes room for a file or a link, refusing to replace a directory.
///
/// A link to a directory is replaced — it is the link that is removed, not what it names — but a
/// real directory is refused rather than emptied: whatever it holds was not in the archive.
fn prepare_replaceable_leaf(
    root: &Path,
    destination: &Path,
    relative: &Path,
    name: &str,
) -> Result<(), ExtractError> {
    ensure_no_symlink_parents(root, destination)?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if destination.is_dir() && !destination.is_symlink() {
        return Err(UnsafeMember::new(
            name,
            format!(
                "destination directory already exists: {}",
                as_posix(relative)
            ),
        )
        .into());
    }
    match std::fs::remove_file(destination) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Writes one file member.
///
/// Created exclusively and without following a link, so a link that appeared since the leaf was
/// cleared is not written through. The file stays private while it holds partial content and gets
/// its restored mode once the content is complete, which is the reference's order.
fn write_file(
    root: &Path,
    destination: &Path,
    relative: &Path,
    member: &Member,
) -> Result<(), ExtractError> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use rustix::fs::{Mode, OFlags};

    prepare_replaceable_leaf(root, destination, relative, &member.name)?;
    let descriptor = rustix::fs::open(
        destination,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?;
    let mut file = std::fs::File::from(descriptor);
    file.write_all(&member.bytes)?;
    file.flush()?;
    file.set_permissions(std::fs::Permissions::from_mode(restored_mode(member.mode)))?;
    Ok(())
}

/// The permission bits an extracted file is allowed to keep.
///
/// The reference's policy, and the standard library filter's before it: setuid, setgid, the sticky
/// bit and group or other write access are dropped, execute survives only where the owner had it,
/// and the owner can always read and write what was extracted.
const fn restored_mode(mode: u32) -> u32 {
    let mut restored = mode & 0o755;
    if restored & 0o100 == 0 {
        restored &= !0o111;
    }
    restored | 0o600
}
