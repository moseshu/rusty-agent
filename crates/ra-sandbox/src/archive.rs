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
use std::sync::LazyLock;

use ra_core::sandbox::{
    CompressionScheme, EntryKind, ErrorCode, OpName, PosixPath, SandboxArchiveLimits, SandboxError,
    SandboxResult, SandboxSession, file_name_suffix,
};
use zip::read::HasZipMetadata;

use crate::tar_utils::windows_drive;

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
                self.apply(path, &destination_root, plan, |member| {
                    Self::tar_content(path, &tar_data, member)
                })
                .await
            }
            CompressionScheme::Zip => {
                let plan = Self::validate_zip(path, &data, limits)?;
                let mut archive = Self::open_zip(path, &data)?;
                self.apply(path, &destination_root, plan, |member| {
                    Self::zip_content(path, &mut archive, member)
                })
                .await
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
            // One gzip member only: Python's streaming tar reader uses a single zlib
            // decompressor, so a later member cannot complete a truncated tar.
            Some(Box::new(flate2::read::GzDecoder::new(data)))
        } else if data.starts_with(b"BZh") {
            Some(Box::new(bzip2::read::BzDecoder::new(data)))
        } else if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            // Python's tarfile streaming mode uses one LZMADecompressor and does not continue
            // into another xz stream to complete a truncated tar.
            Some(Box::new(lzma_rust2::XzReader::new(data, false)))
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

    /// Reads a zip's central directory.
    fn open_zip<'d>(
        path: &str,
        data: &'d [u8],
    ) -> SandboxResult<zip::ZipArchive<Cursor<&'d [u8]>>> {
        zip::ZipArchive::new(Cursor::new(data)).map_err(|error| {
            Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
        })
    }

    /// Reads every zip member's entry in the central directory, and refuses the archive if any of
    /// them is not extractable.
    fn validate_zip(
        path: &str,
        data: &[u8],
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<Vec<PlannedMember<ZipContent>>> {
        let mut archive = Self::open_zip(path, data)?;
        Self::validate_collapsed_zip_entries(path, data, &archive, limits)?;
        let mut plan = Vec::new();
        let mut members: BTreeMap<PosixPath, bool> = BTreeMap::new();
        let mut extracted_bytes = 0_u64;
        for index in 0..archive.len() {
            let member = archive.by_index(index).map_err(|error| {
                Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
            })?;
            // The name is decoded from its raw bytes by the same rules as the collapsed-entry pass,
            // not taken from the crate: that one decodes invalid UTF-8 lossily and keeps whatever
            // follows a NUL, where the reference refuses the one and truncates at the other.
            let metadata = member.get_metadata();
            let name =
                zip_member_name(&metadata.file_name_raw, metadata.is_utf8).map_err(|error| {
                    Self::refuse(path, "unreadable archive")
                        .with_context("os_error", error.to_string())
                })?;
            let is_directory = name.ends_with('/');
            let relative = Self::safe_zip_path(path, &name, metadata.external_attributes)?;
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
            plan.push(PlannedMember {
                name,
                relative,
                is_directory,
                content: ZipContent {
                    index,
                    size: member.size(),
                },
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

    /// Checks every original record when the ZIP reader has collapsed same-named entries.
    ///
    /// Repeated directories are allowed only after each one's attributes and declared size have
    /// been checked. They still count as separate members, as they do in Python's `infolist()`.
    fn validate_collapsed_zip_entries(
        path: &str,
        data: &[u8],
        archive: &zip::ZipArchive<Cursor<&[u8]>>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        let listed =
            zip_central_entries(data, archive.central_directory_start()).map_err(|error| {
                Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
            })?;
        if listed.len() <= archive.len() {
            return Ok(());
        }
        let mut members: BTreeMap<PosixPath, bool> = BTreeMap::new();
        let mut paths = Vec::new();
        let mut extracted_bytes = 0_u64;
        for member in listed {
            let name = member.name;
            let Some(relative) = Self::safe_zip_path(path, &name, member.external_attributes)?
            else {
                continue;
            };
            let count = paths.len() + 1;
            if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_members)
                && count > limit
            {
                return Err(Self::refuse(path, "archive member count exceeds limit")
                    .with_context("limit", limit as u64)
                    .with_context("actual", count as u64)
                    .with_context("member", name));
            }
            extracted_bytes = extracted_bytes.saturating_add(member.size);
            if let Some(limit) = limits.and_then(SandboxArchiveLimits::max_extracted_bytes)
                && extracted_bytes > limit
            {
                return Err(Self::refuse(path, "archive extracted size exceeds limit")
                    .with_context("limit", limit)
                    .with_context("actual", extracted_bytes)
                    .with_context("member", name));
            }
            let is_directory = name.ends_with('/');
            if let Some(previous) = members.get(&relative)
                && !(*previous && is_directory)
            {
                return Err(
                    Self::refuse(path, &format!("duplicate archive path: {relative}"))
                        .with_context("member", name),
                );
            }
            members.insert(relative.clone(), is_directory);
            paths.push((name, relative));
        }
        for (name, relative) in paths {
            for parent in ancestors(&relative) {
                if members.get(&parent).is_some_and(|is_dir| !is_dir) {
                    return Err(Self::refuse(
                        path,
                        &format!("archive path descends through non-directory: {parent}"),
                    )
                    .with_context("member", name));
                }
            }
        }
        Ok(())
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

    /// A zip member's bytes, held to the size its entry declares.
    fn zip_content(
        path: &str,
        archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
        member: &PlannedMember<ZipContent>,
    ) -> SandboxResult<Vec<u8>> {
        let unreadable = |error: &dyn std::fmt::Display| {
            Self::refuse(path, "unreadable archive").with_context("os_error", error.to_string())
        };
        let declared = member.content.size;
        let mut file = archive
            .by_index(member.content.index)
            .map_err(|error| unreadable(&error))?;
        // The limits were checked against the size the archive declares, and nothing in the zip
        // reader holds a member to it: a deflate stream can inflate far past what its header
        // claims. One byte past the declared size is enough to know it lied.
        let mut bytes = Vec::new();
        file.by_ref()
            .take(declared.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| unreadable(&error))?;
        if bytes.len() as u64 != declared {
            return Err(
                Self::refuse(path, "archive member size does not match header")
                    .with_context("member", member.name.clone())
                    .with_context("declared", declared),
            );
        }
        Ok(bytes)
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

    /// A tar member's bytes, taken by the range the header pass recorded.
    ///
    /// Not from a live tar reader: that reader is not `Sync`, and a session write is an `await`.
    fn tar_content(
        archive_path: &str,
        data: &[u8],
        member: &PlannedMember,
    ) -> SandboxResult<Vec<u8>> {
        data.get(member.content.clone())
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                Self::refuse(archive_path, "archive member content is truncated")
                    .with_context("member", member.name.clone())
            })
    }

    /// Writes every member the plan describes, in the order the archive declared it, taking each
    /// file's bytes from `content`.
    async fn apply<C>(
        &mut self,
        archive_path: &str,
        destination_root: &str,
        plan: Vec<PlannedMember<C>>,
        mut content: impl FnMut(&PlannedMember<C>) -> SandboxResult<Vec<u8>>,
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
            let bytes = content(&member)?;
            self.session.write(&destination, bytes, None).await?;
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

/// One member the archive is allowed to write, and where its bytes are: a range of the tar for a
/// tar member, an entry of the central directory for a zip one.
struct PlannedMember<C = std::ops::Range<usize>> {
    name: String,
    relative: PosixPath,
    is_directory: bool,
    content: C,
}

/// Where a zip member's bytes are.
struct ZipContent {
    index: usize,
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

/// The metadata used to validate one original central-directory record.
struct ZipCentralEntry {
    name: String,
    size: u64,
    external_attributes: u32,
}

/// Reads original central records, including entries hidden by the reader's name index.
/// Only the uncompressed size from ZIP64 is needed; payload decoding remains the ZIP reader's job.
fn zip_central_entries(data: &[u8], directory_start: u64) -> std::io::Result<Vec<ZipCentralEntry>> {
    const HEADER_LEN: usize = 46;
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid ZIP central directory",
        )
    };
    let mut entries = Vec::new();
    let mut at = usize::try_from(directory_start).map_err(|_| invalid())?;
    while data.get(at..at.saturating_add(4)) == Some(b"PK\x01\x02") {
        let header = data
            .get(at..at.saturating_add(HEADER_LEN))
            .ok_or_else(invalid)?;
        let field =
            |offset: usize| usize::from(u16::from_le_bytes([header[offset], header[offset + 1]]));
        let name_start = at + HEADER_LEN;
        let name_end = name_start.checked_add(field(28)).ok_or_else(invalid)?;
        let extra_end = name_end.checked_add(field(30)).ok_or_else(invalid)?;
        let record_end = extra_end.checked_add(field(32)).ok_or_else(invalid)?;
        let raw_name = data.get(name_start..name_end).ok_or_else(invalid)?;
        let extra = data.get(name_end..extra_end).ok_or_else(invalid)?;
        data.get(extra_end..record_end).ok_or_else(invalid)?;
        let name = zip_member_name(raw_name, field(8) & (1 << 11) != 0)?;
        let size = u32::from_le_bytes(header[24..28].try_into().map_err(|_| invalid())?);
        let size = if size == u32::MAX {
            zip64_uncompressed_size(extra)?
        } else {
            u64::from(size)
        };
        let external_attributes =
            u32::from_le_bytes(header[38..42].try_into().map_err(|_| invalid())?);
        entries.push(ZipCentralEntry {
            name,
            size,
            external_attributes,
        });
        at = record_end;
    }
    Ok(entries)
}

/// ZIP64 puts the uncompressed size first when its ordinary field contains the sentinel.
fn zip64_uncompressed_size(mut extra: &[u8]) -> std::io::Result<u64> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing or truncated ZIP64 size",
        )
    };
    while extra.len() >= 4 {
        let tag = u16::from_le_bytes([extra[0], extra[1]]);
        let length = usize::from(u16::from_le_bytes([extra[2], extra[3]]));
        let body = extra.get(4..4 + length).ok_or_else(invalid)?;
        if tag == 1 {
            let size = body.get(..8).ok_or_else(invalid)?;
            return Ok(u64::from_le_bytes(size.try_into().map_err(|_| invalid())?));
        }
        extra = &extra[4 + length..];
    }
    Err(invalid())
}

/// The characters CP437 gives bytes `0x80..=0xff`, in order; the lower half is ASCII. Split
/// into a table once rather than for every name decoded.
static CP437_HIGH: LazyLock<Vec<char>> = LazyLock::new(|| CP437_HIGH_CHARS.chars().collect());

const CP437_HIGH_CHARS: &str = "\u{c7}\u{fc}\u{e9}\u{e2}\u{e4}\u{e0}\u{e5}\u{e7}\u{ea}\u{eb}\u{e8}\u{ef}\u{ee}\u{ec}\u{c4}\u{c5}\u{c9}\u{e6}\u{c6}\u{f4}\u{f6}\u{f2}\u{fb}\u{f9}\u{ff}\u{d6}\u{dc}\u{a2}\u{a3}\u{a5}\u{20a7}\u{192}\u{e1}\u{ed}\u{f3}\u{fa}\u{f1}\u{d1}\u{aa}\u{ba}\u{bf}\u{2310}\u{ac}\u{bd}\u{bc}\u{a1}\u{ab}\u{bb}\u{2591}\u{2592}\u{2593}\u{2502}\u{2524}\u{2561}\u{2562}\u{2556}\u{2555}\u{2563}\u{2551}\u{2557}\u{255d}\u{255c}\u{255b}\u{2510}\u{2514}\u{2534}\u{252c}\u{251c}\u{2500}\u{253c}\u{255e}\u{255f}\u{255a}\u{2554}\u{2569}\u{2566}\u{2560}\u{2550}\u{256c}\u{2567}\u{2568}\u{2564}\u{2565}\u{2559}\u{2558}\u{2552}\u{2553}\u{256b}\u{256a}\u{2518}\u{250c}\u{2588}\u{2584}\u{258c}\u{2590}\u{2580}\u{3b1}\u{df}\u{393}\u{3c0}\u{3a3}\u{3c3}\u{b5}\u{3c4}\u{3a6}\u{398}\u{3a9}\u{3b4}\u{221e}\u{3c6}\u{3b5}\u{2229}\u{2261}\u{b1}\u{2265}\u{2264}\u{2320}\u{2321}\u{f7}\u{2248}\u{b0}\u{2219}\u{b7}\u{221a}\u{207f}\u{b2}\u{25a0}\u{a0}";

/// Python's ZIP reader uses UTF-8 when flagged, otherwise the fixed CP437 character set.
fn zip_member_name(raw: &[u8], utf8: bool) -> std::io::Result<String> {
    let name = if utf8 {
        std::str::from_utf8(raw)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .to_owned()
    } else {
        raw.iter()
            .map(|byte| match byte.checked_sub(128) {
                Some(high) => CP437_HIGH[usize::from(high)],
                None => char::from(*byte),
            })
            .collect()
    };
    // ZipInfo truncates a filename at the first NUL before the framework inspects it.
    Ok(name.split('\0').next().unwrap_or_default().to_owned())
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
