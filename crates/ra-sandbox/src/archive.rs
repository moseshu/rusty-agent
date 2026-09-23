//! Unpacking an archive into a workspace, through the session that owns it.
//!
//! Everything here is written with the session's own `mkdir` and `write`, never with the host's
//! filesystem, so one extractor serves every backend: the archive is read on the machine running
//! the SDK and the files land wherever that session's workspace is.
//!
//! # An archive is input, and is treated as such
//!
//! The members decide where their own bytes go, which is the whole problem. A member can name an
//! absolute path, climb out with `..`, spell a Windows drive, arrive twice, claim to be a directory
//! and a file at once, or land under a symlink that already exists in the workspace and points
//! somewhere else entirely. Each of those turns "unpack into this directory" into "write anywhere",
//! so the archive is checked through to the end **before the first member is written**: a refusal
//! then leaves the workspace as it was rather than half unpacked.
//!
//! Size is the other half. A few kilobytes of members can declare gigabytes of files, so the
//! ceilings a caller supplied are checked against the headers, before any member's content is read.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use ra_core::sandbox::{
    CompressionScheme, EntryKind, ErrorCode, OpName, PosixPath, SandboxArchiveLimits, SandboxError,
    SandboxResult, SandboxSession, file_name_suffix,
};
use zip::read::HasZipMetadata;

/// Unpacks archives into one session's workspace.
pub struct WorkspaceArchiveExtractor<'a> {
    session: &'a dyn SandboxSession,
    /// What each listed directory was seen to contain, so a member's parents are not listed again
    /// for every member that shares them.
    listings: BTreeMap<String, BTreeMap<String, EntryKind>>,
}

impl<'a> WorkspaceArchiveExtractor<'a> {
    /// Unpacks into `session`'s workspace.
    #[must_use]
    pub const fn new(session: &'a dyn SandboxSession) -> Self {
        Self {
            session,
            listings: BTreeMap::new(),
        }
    }

    /// Writes the archive into the workspace and unpacks it beside itself.
    ///
    /// The archive itself lands at `path` — the reference writes it before unpacking, and a caller
    /// that asked for a bad archive still finds the bytes it handed over rather than nothing at
    /// all.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidCompressionScheme`] when the format is unstated and cannot be
    /// read from the name, and [`ErrorCode::WorkspaceArchiveWriteError`] for an archive that is
    /// malformed, that would write outside the workspace, or that exceeds one of `limits`.
    pub async fn extract(
        &mut self,
        path: &str,
        data: Vec<u8>,
        scheme: Option<CompressionScheme>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        // Listings only describe one extraction. A later call may see replaced directories or
        // links, including after a previous call failed partway through.
        self.listings.clear();
        let scheme = match scheme {
            Some(scheme) => scheme,
            None => Self::infer_scheme(path)?,
        };
        // Infer the format from the caller's name, but place members beside the backend-resolved
        // archive. A leaf symlink can put the archive in a different directory.
        let normalized_path = self.session.validate_path_access(path, true).await?;
        let path = normalized_path.as_str();

        if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_input_bytes)
            && data.len() as u64 > limit
        {
            return Err(Self::refuse(path, "archive input size exceeds limit")
                .with_context("limit", limit)
                .with_context("actual", data.len() as u64));
        }

        // The archive itself lands first, as the reference writes it: it is what the caller handed
        // over, and a caller whose archive is then refused still has the bytes to look at.
        let destination_root = parent_of(path);
        self.session.write(path, data.clone(), None).await?;

        // Its members are read through to the end before any of them is written, so a refusal
        // leaves the workspace holding the archive and nothing unpacked from it. Only the headers
        // are read here: an archive that declares more than the ceilings allow is refused without
        // its content ever being held.
        match scheme {
            CompressionScheme::Tar => {
                // A compressed tar is checked while it streams out of its decoder, so a few
                // kilobytes that decompress to gigabytes are refused on their headers rather than
                // after being inflated. Only what the plan needs is then decoded to be written.
                let plan = match Self::tar_decoder(&data) {
                    Some(decoder) => Self::validate_tar(path, decoder, limits)?,
                    None => Self::validate_tar(path, data.as_slice(), limits)?,
                };
                let tar_data = Self::decode_tar(path, &data, &plan)?;
                self.apply(path, &destination_root, &tar_data, plan).await
            }
            CompressionScheme::Zip => {
                let plan = Self::validate_zip(path, &data, limits)?;
                self.apply_zip(path, &destination_root, &data, plan).await
            }
            _ => Err(SandboxError::new(
                ErrorCode::InvalidCompressionScheme,
                OpName::Write,
                "compression scheme must be one of 'zip' 'tar'",
            )),
        }
    }

    /// A decoder for a compressed tar, chosen by its magic bytes, or `None` for a plain one.
    fn tar_decoder(data: &[u8]) -> Option<Box<dyn Read + '_>> {
        if data.starts_with(&[0x1f, 0x8b]) {
            Some(Box::new(flate2::read::MultiGzDecoder::new(data)))
        } else if data.starts_with(b"BZh") {
            Some(Box::new(bzip2::read::BzDecoder::new(data)))
        } else if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            Some(Box::new(xz2::read::XzDecoder::new(data)))
        } else {
            None
        }
    }

    /// The tar bytes the plan's members are taken from.
    ///
    /// A compressed tar is decoded only as far as the last planned member's content ends: the
    /// plan has already been checked against the limits, and whatever the stream holds after
    /// that is never needed.
    fn decode_tar<'d>(
        path: &str,
        data: &'d [u8],
        plan: &[PlannedMember],
    ) -> SandboxResult<Cow<'d, [u8]>> {
        let Some(decoder) = Self::tar_decoder(data) else {
            return Ok(Cow::Borrowed(data));
        };
        let needed = plan
            .iter()
            .map(|member| member.content.end)
            .max()
            .unwrap_or(0);
        let mut tar_data = Vec::new();
        decoder
            .take(u64::try_from(needed).unwrap_or(u64::MAX))
            .read_to_end(&mut tar_data)
            .map_err(|error| {
                Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
            })?;
        Ok(Cow::Owned(tar_data))
    }

    fn validate_zip(
        path: &str,
        data: &[u8],
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<Vec<ZipMember>> {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).map_err(|error| {
            Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
        })?;
        let mut plan = Vec::new();
        let mut members: BTreeMap<PosixPath, bool> = BTreeMap::new();
        let mut extracted_bytes = 0_u64;
        for index in 0..archive.len() {
            let member = archive.by_index(index).map_err(|error| {
                Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
            })?;
            let name = member.name().to_owned();
            let is_directory = member.is_dir();
            let relative =
                Self::safe_zip_path(path, &name, member.get_metadata().external_attributes)?;
            let Some(relative) = relative else { continue };
            let count = plan.len() + 1;
            if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_members)
                && count > limit
            {
                return Err(Self::refuse(path, "archive member count exceeds limit")
                    .with_context("limit", limit as u64)
                    .with_context("actual", count as u64)
                    .with_context("member", name));
            }
            extracted_bytes = extracted_bytes.saturating_add(member.size());
            if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_extracted_bytes)
                && extracted_bytes > limit
            {
                return Err(Self::refuse(path, "archive extracted size exceeds limit")
                    .with_context("limit", limit)
                    .with_context("actual", extracted_bytes)
                    .with_context("member", name));
            }
            if let Some(previous) = members.get(&relative)
                && !(*previous && is_directory)
            {
                return Err(
                    Self::refuse(path, &format!("duplicate archive path: {relative}"))
                        .with_context("member", name),
                );
            }
            members.insert(relative.clone(), is_directory);
            plan.push(ZipMember {
                index,
                name,
                relative,
                is_directory,
                size: member.size(),
            });
        }
        for member in &plan {
            for parent in ancestors(&member.relative) {
                if members.get(&parent).is_some_and(|is_dir| !is_dir) {
                    return Err(Self::refuse(
                        path,
                        &format!("archive path descends through non-directory: {parent}"),
                    )
                    .with_context("member", member.name.clone()));
                }
            }
        }
        Ok(plan)
    }

    /// Validates one zip member's path and type, or answers `None` for the archive's own root.
    ///
    /// The type comes from the high half of the external attributes whatever system the archive
    /// says made it, as the reference reads it: the `zip` crate's `unix_mode` invents a regular
    /// file for a DOS-made member, which would let a link through under that label. Path rules
    /// come first, so a member that is both a link and a climb is reported as the climb.
    fn safe_zip_path(
        path: &str,
        name: &str,
        external_attributes: u32,
    ) -> SandboxResult<Option<PosixPath>> {
        if matches!(name, "" | "." | "./") {
            return Ok(None);
        }
        let reason = if windows_drive(name) {
            Some("windows drive path")
        } else if name.contains('\\') {
            Some("windows path separator")
        } else if name.starts_with('/') {
            Some("absolute path")
        } else if PosixPath::new(name).parts().contains(&"..") {
            Some("parent traversal")
        } else if (external_attributes >> 16) & 0o170_000 == 0o120_000 {
            Some("link member not allowed")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Self::refuse(path, reason).with_context("member", name.to_owned()));
        }
        Ok(Some(PosixPath::new(name).normalized()))
    }

    async fn apply_zip(
        &mut self,
        path: &str,
        destination_root: &str,
        data: &[u8],
        plan: Vec<ZipMember>,
    ) -> SandboxResult<()> {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).map_err(|error| {
            Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
        })?;
        for member in plan {
            self.refuse_symlink_parents(path, destination_root, &member.name, &member.relative)
                .await?;
            let destination = join(destination_root, &member.relative);
            if member.is_directory {
                self.session.mkdir(&destination, true, None).await?;
                self.record(destination_root, &member.relative, EntryKind::Directory);
                continue;
            }
            let parent = parent_of(&destination);
            self.session.mkdir(&parent, true, None).await?;
            if let Some(relative_parent) = parent_relative(&member.relative) {
                self.record(destination_root, &relative_parent, EntryKind::Directory);
            }
            let bytes = {
                let mut file = archive.by_index(member.index).map_err(|error| {
                    Self::refuse(path, "unreadable archive")
                        .with_context("os_error", error.to_string())
                })?;
                // The limits were checked against the size the archive declares, and nothing in
                // the zip reader holds a member to it: a deflate stream can inflate far past what
                // its header claims. One byte past the declared size is enough to know it lied.
                let mut bytes = Vec::new();
                file.by_ref()
                    .take(member.size.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        Self::refuse(path, "unreadable archive")
                            .with_context("os_error", error.to_string())
                    })?;
                if bytes.len() as u64 != member.size {
                    return Err(
                        Self::refuse(path, "archive member size does not match header")
                            .with_context("member", member.name.clone())
                            .with_context("declared", member.size),
                    );
                }
                bytes
            };
            self.session.write(&destination, bytes, None).await?;
            self.record(destination_root, &member.relative, EntryKind::File);
        }
        Ok(())
    }

    /// Reads the format from the archive's own name.
    fn infer_scheme(path: &str) -> SandboxResult<CompressionScheme> {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        if let Some(scheme) = CompressionScheme::from_file_name(file_name) {
            return Ok(scheme);
        }

        // Two different refusals, as the reference has: a name with no extension says nothing about
        // its format, while `archive.gz` says something this cannot honour.
        let suffix = file_name_suffix(file_name);
        let message = match suffix {
            Some(_) => "compression scheme must be one of 'zip' 'tar'",
            None => "could not determine compression scheme",
        };
        Err(
            SandboxError::new(ErrorCode::InvalidCompressionScheme, OpName::Write, message)
                .with_context("path", path)
                .with_context("scheme", suffix.map(str::to_owned)),
        )
    }

    /// Reads every member's header, and refuses the archive if any of them is not extractable.
    ///
    /// Headers only: a member's content is read when it is written, so an archive that declares
    /// more than the ceilings allow is refused without ever being held.
    fn validate_tar<R: Read>(
        archive_path: &str,
        data: R,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<Vec<PlannedMember>> {
        let mut plan: Vec<PlannedMember> = Vec::new();
        let mut member_count: usize = 0;
        let mut members: BTreeMap<PosixPath, bool> = BTreeMap::new();
        let mut descendant_of: BTreeMap<PosixPath, String> = BTreeMap::new();
        let mut extracted_bytes: u64 = 0;

        let mut archive = tar::Archive::new(data);
        let unreadable = |error: &std::io::Error| {
            SandboxError::workspace_archive_write(archive_path)
                .with_context("reason", "unreadable archive")
                .with_context("os_error", error.to_string())
        };

        for entry in archive.entries().map_err(|error| unreadable(&error))? {
            let entry = entry.map_err(|error| unreadable(&error))?;
            let name = entry
                .path()
                .map_err(|error| unreadable(&error))?
                .to_string_lossy()
                .into_owned();
            let header = entry.header();
            let kind = MemberKind::from(header.entry_type());
            // PAX extensions can override the raw header size. Use the effective size both for
            // resource accounting and for the payload range.
            let size = entry.size();
            // Where this member's bytes sit in the archive. Recorded now so that writing them later
            // needs nothing from the tar reader, which holds state that cannot cross an await. A
            // position this machine cannot address is treated as a truncated member, which is what
            // it is from here: the bytes are not reachable.
            let start = usize::try_from(entry.raw_file_position()).unwrap_or(usize::MAX);
            let content = start..start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
            let Some(relative) = Self::safe_member_path(archive_path, &name, kind)? else {
                continue;
            };
            let is_directory = kind == MemberKind::Directory;

            member_count += 1;
            if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_members)
                && member_count > limit
            {
                return Err(
                    Self::refuse(archive_path, "archive member count exceeds limit")
                        .with_context("limit", limit as u64)
                        .with_context("actual", member_count as u64)
                        .with_context("member", name),
                );
            }
            if !is_directory {
                extracted_bytes = extracted_bytes.saturating_add(size);
                if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_extracted_bytes)
                    && extracted_bytes > limit
                {
                    return Err(
                        Self::refuse(archive_path, "archive extracted size exceeds limit")
                            .with_context("limit", limit)
                            .with_context("actual", extracted_bytes)
                            .with_context("member", name),
                    );
                }
            }

            // The same path twice is only ever ambiguous, except for two directories, which are the
            // same request made twice.
            if let Some(previous_is_directory) = members.get(&relative)
                && !(*previous_is_directory && is_directory)
            {
                return Err(Self::refuse(
                    archive_path,
                    &format!("duplicate archive path: {relative}"),
                )
                .with_context("member", name));
            }

            // A member under something that is not a directory cannot be written without turning
            // that thing into one, which is not what either member asked for.
            for parent in ancestors(&relative) {
                if members.get(&parent).is_some_and(|is_dir| !is_dir) {
                    return Err(Self::refuse(
                        archive_path,
                        &format!("archive path descends through non-directory: {parent}"),
                    )
                    .with_context("member", name));
                }
            }
            if !is_directory && let Some(descendant) = descendant_of.get(&relative) {
                // The same conflict, found from the other side: this member arrived after the one
                // that needs it to be a directory, so the refusal names that one.
                return Err(Self::refuse(
                    archive_path,
                    &format!("archive path descends through non-directory: {relative}"),
                )
                .with_context("member", descendant.clone()));
            }

            for parent in ancestors(&relative) {
                descendant_of.entry(parent).or_insert_with(|| name.clone());
            }
            members.insert(relative.clone(), is_directory);
            plan.push(PlannedMember {
                name,
                relative,
                is_directory,
                content,
            });
        }

        Ok(plan)
    }

    /// Writes every member the plan describes, in the order the archive declared it.
    ///
    /// Each member's bytes are taken from the archive by the range the header pass recorded rather
    /// than from a live tar reader: that reader is not `Sync`, and a session write is an `await`.
    async fn apply(
        &mut self,
        archive_path: &str,
        destination_root: &str,
        data: &[u8],
        plan: Vec<PlannedMember>,
    ) -> SandboxResult<()> {
        for member in plan {
            self.refuse_symlink_parents(
                archive_path,
                destination_root,
                &member.name,
                &member.relative,
            )
            .await?;
            let destination = join(destination_root, &member.relative);

            if member.is_directory {
                self.session.mkdir(&destination, true, None).await?;
                self.record(destination_root, &member.relative, EntryKind::Directory);
                continue;
            }

            let parent = parent_of(&destination);
            self.session.mkdir(&parent, true, None).await?;
            if let Some(relative_parent) = parent_relative(&member.relative) {
                self.record(destination_root, &relative_parent, EntryKind::Directory);
            }
            let bytes = data.get(member.content).ok_or_else(|| {
                Self::refuse(archive_path, "archive member content is truncated")
                    .with_context("member", member.name.clone())
            })?;
            self.session
                .write(&destination, bytes.to_vec(), None)
                .await?;
            self.record(destination_root, &member.relative, EntryKind::File);
        }
        Ok(())
    }

    /// Refuses a member whose path leads through a symlink that is already there.
    ///
    /// The workspace is not the archive's to reshape: a link at `data` pointing at `/etc` turns
    /// every member under `data/` into a write outside the workspace, and the session's own path
    /// checks would resolve it and refuse — one member at a time, after earlier members had already
    /// been written.
    async fn refuse_symlink_parents(
        &mut self,
        archive_path: &str,
        destination_root: &str,
        member: &str,
        relative: &PosixPath,
    ) -> SandboxResult<()> {
        let mut directory = destination_root.to_owned();
        let mut traversed = String::new();

        for part in relative.parts() {
            let Some(kind) = self.child_kind(&directory, part).await else {
                // Nothing there yet, so nothing below it can be a link either.
                return Ok(());
            };
            if !traversed.is_empty() {
                traversed.push('/');
            }
            traversed.push_str(part);
            if kind == EntryKind::Symlink {
                return Err(Self::refuse(
                    archive_path,
                    &format!("symlink in parent path: {traversed}"),
                )
                .with_context("member", member.to_owned()));
            }
            directory = join(&directory, &PosixPath::new(part));
        }
        Ok(())
    }

    /// What a directory holds, read once and remembered.
    async fn child_kind(&mut self, directory: &str, child: &str) -> Option<EntryKind> {
        if !self.listings.contains_key(directory) {
            let listed = match self.session.ls(directory, None).await {
                Ok(entries) => entries
                    .into_iter()
                    .map(|entry| {
                        let name = entry
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&entry.path)
                            .to_owned();
                        (name, entry.kind)
                    })
                    .collect(),
                // A directory that is not there holds nothing, which is the answer every caller
                // here wants: the member below it is about to create it.
                Err(_) => BTreeMap::new(),
            };
            self.listings.insert(directory.to_owned(), listed);
        }
        self.listings.get(directory)?.get(child).copied()
    }

    /// Remembers what this extraction just put somewhere, so later members see it.
    fn record(&mut self, destination_root: &str, relative: &PosixPath, kind: EntryKind) {
        let mut directory = destination_root.to_owned();
        let parts = relative.parts();
        for (index, part) in parts.iter().enumerate() {
            let child_kind = if index + 1 == parts.len() {
                kind
            } else {
                EntryKind::Directory
            };
            if let Some(listed) = self.listings.get_mut(&directory) {
                listed.insert((*part).to_owned(), child_kind);
            }
            directory = join(&directory, &PosixPath::new(*part));
        }
    }

    /// Validates one member's path, or answers `None` for the archive's own root entry.
    fn safe_member_path(
        archive_path: &str,
        name: &str,
        kind: MemberKind,
    ) -> SandboxResult<Option<PosixPath>> {
        let refuse = |reason: &str| {
            Err(Self::refuse(archive_path, reason).with_context("member", name.to_owned()))
        };

        if matches!(name, "" | "." | "./") {
            // The entry for the archive's own root is skipped rather than written, but only when it
            // really is a directory: anything else there is a claim about the destination itself.
            return match kind {
                MemberKind::Directory => Ok(None),
                MemberKind::Symlink => refuse("archive root symlink"),
                MemberKind::HardLink => refuse("archive root hardlink"),
                _ => refuse("archive root member must be directory"),
            };
        }
        if windows_drive(name) {
            return refuse("windows drive path");
        }
        if name.contains('\\') {
            return refuse("windows path separator");
        }

        let relative = PosixPath::new(name);
        if relative.is_absolute() {
            return refuse("absolute path");
        }
        if relative.parts().contains(&"..") {
            return refuse("parent traversal");
        }
        match kind {
            // No links at all on this path, in either direction. Unpacking one means writing
            // through whatever it names, and the archive is the party that chose the name.
            MemberKind::Symlink => refuse("symlink member not allowed"),
            MemberKind::HardLink => refuse("hardlink member not allowed"),
            MemberKind::Other => refuse("unsupported member type"),
            MemberKind::Directory | MemberKind::File => Ok(Some(relative.normalized())),
        }
    }

    /// The refusal every archive failure is reported as.
    fn refuse(archive_path: &str, reason: &str) -> SandboxError {
        SandboxError::workspace_archive_write(archive_path).with_context("reason", reason)
    }
}

/// One member the archive is allowed to write, and where its bytes are.
struct PlannedMember {
    name: String,
    relative: PosixPath,
    is_directory: bool,
    content: std::ops::Range<usize>,
}

struct ZipMember {
    index: usize,
    name: String,
    relative: PosixPath,
    is_directory: bool,
    /// The uncompressed size the archive declares, which is what the limits were checked against.
    size: u64,
}

/// What kind of thing an archive member claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberKind {
    Directory,
    File,
    Symlink,
    HardLink,
    Other,
}

impl From<tar::EntryType> for MemberKind {
    fn from(entry_type: tar::EntryType) -> Self {
        if entry_type.is_dir() {
            Self::Directory
        } else if entry_type.is_file() {
            Self::File
        } else if entry_type.is_symlink() {
            Self::Symlink
        } else if entry_type.is_hard_link() {
            Self::HardLink
        } else {
            Self::Other
        }
    }
}

/// Whether a member name starts with a Windows drive, which names a filesystem of its own.
fn windows_drive(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.next() == Some(':')
}

/// Every proper ancestor of a workspace-relative path, nearest first.
fn ancestors(relative: &PosixPath) -> Vec<PosixPath> {
    let parts = relative.parts();
    (1..parts.len())
        .rev()
        .map(|end| PosixPath::new(parts[..end].join("/")))
        .collect()
}

/// The path holding `path`, as the session spells paths.
fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".to_owned(),
        Some((parent, _)) => parent.to_owned(),
        None => ".".to_owned(),
    }
}

/// A member's parent, relative to the destination root.
fn parent_relative(relative: &PosixPath) -> Option<PosixPath> {
    let parts = relative.parts();
    (parts.len() > 1).then(|| PosixPath::new(parts[..parts.len() - 1].join("/")))
}

/// Where a member goes, given the directory the archive is being unpacked into.
fn join(directory: &str, relative: &PosixPath) -> String {
    if directory == "." {
        return relative.as_str().to_owned();
    }
    format!("{}/{}", directory.trim_end_matches('/'), relative.as_str())
}
