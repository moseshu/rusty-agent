//! The shared tar policy: which members a workspace archive may carry, and how one is extracted.
//!
//! The reference keeps this in `util/tar_utils.py` and exports it for every backend: a restore on
//! the local backend extracts through [`safe_extract_tarfile`], a container backend normalizes the
//! archive its runtime produced with [`strip_tar_member_prefix`], and a remote backend checks the
//! bytes it is about to hand over with [`validate_tar_bytes`]. The same rules apply in all three
//! places, so they are written once here. [`shell_tar_exclude_args`] is the reference's
//! `session/tar_workspace.py`, for a backend that archives with a shell `tar` instead.
//!
//! # Names are bytes
//!
//! A member's name and link target are kept as the archive's own bytes, as Python keeps them as
//! surrogate-escaped strings: the rules below split on `/` and compare components, which needs no
//! decoding, and a name that is not UTF-8 is extracted under its original bytes rather than under a
//! replacement. Only the diagnostics carry a lossy rendering. Paths are POSIX paths throughout: a
//! backslash is not a separator, it is a reason to refuse the member.
//!
//! # Not the archive a caller hands to `extract`
//!
//! That path ([`crate::archive`]) takes an archive from outside the session, refuses links outright
//! and applies size limits. The archives here are workspaces a session wrote, which routinely
//! contain links — a Python virtual environment is made of them — and carry no limits, as they
//! carry none in the reference.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{Cursor, Read};

use ra_core::sandbox::shell::quote;

/// A member the policy refuses: which one, and the reference's words for why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeTarMember {
    member: String,
    reason: String,
}

impl UnsafeTarMember {
    pub(crate) fn new(member: &str, reason: impl Into<String>) -> Self {
        Self {
            member: member.to_owned(),
            reason: reason.into(),
        }
    }

    /// The member's name, as the archive spelled it.
    #[must_use]
    pub fn member(&self) -> &str {
        &self.member
    }

    /// Why it was refused, in the reference's wording.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for UnsafeTarMember {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsafe tar member '{}': {}",
            self.member, self.reason
        )
    }
}

impl std::error::Error for UnsafeTarMember {}

/// What [`validate_tar_bytes`] checks beyond the rules every member is held to.
///
/// The defaults are the reference's: nothing protected, nothing skipped, and symlinks allowed to
/// point anywhere — a link is metadata, and extraction never follows one.
#[derive(Debug, Clone)]
pub struct TarValidation {
    rejected: Vec<String>,
    rejected_symlinks: Vec<String>,
    skipped: Vec<String>,
    root_name: Option<String>,
    external_symlink_targets: bool,
}

impl Default for TarValidation {
    fn default() -> Self {
        Self {
            rejected: Vec::new(),
            rejected_symlinks: Vec::new(),
            skipped: Vec::new(),
            root_name: None,
            external_symlink_targets: true,
        }
    }
}

impl TarValidation {
    /// The reference's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses any member at, under, or — unless it is a directory — above one of `paths`.
    ///
    /// The reference's `reject_rel_paths`: a backend passes the paths a mount occupies, so an
    /// archive cannot write into the mount or replace a directory on the way to it with a file.
    #[must_use]
    pub fn rejecting(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.rejected.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Refuses a symlink member at exactly one of `paths`; what is under them is not affected.
    ///
    /// The reference's `reject_symlink_rel_paths`.
    #[must_use]
    pub fn rejecting_symlinks_at(
        mut self,
        paths: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.rejected_symlinks
            .extend(paths.into_iter().map(Into::into));
        self
    }

    /// Leaves members at or under `paths` out of every check.
    ///
    /// The reference's `skip_rel_paths`, for content the archive carries but the backend will not
    /// extract.
    #[must_use]
    pub fn skipping(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.skipped.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Also matches member names that start with the workspace directory's own name.
    ///
    /// The reference's `root_name`, for archives a runtime wrote with the root as their first
    /// component.
    #[must_use]
    pub fn with_root_name(mut self, root_name: impl Into<String>) -> Self {
        self.root_name = Some(root_name.into());
        self
    }

    /// Whether a symlink may point outside the archive.
    ///
    /// The reference's `allow_external_symlink_targets`, on by default. Off, an absolute target or
    /// one that climbs above the archive root is refused.
    #[must_use]
    pub const fn with_external_symlink_targets(mut self, allowed: bool) -> Self {
        self.external_symlink_targets = allowed;
        self
    }
}

/// Checks raw workspace tar bytes against the shared policy.
///
/// The archive may be gzip, bzip2 or xz compressed, and every compressed stream is read, as the
/// reference's `r:*` reader reads them.
///
/// # Errors
///
/// Returns the first member refused, or a member named `<tar>` with the reason `invalid tar
/// stream` when the bytes are not a readable archive.
pub fn validate_tar_bytes(raw: &[u8], validation: &TarValidation) -> Result<(), UnsafeTarMember> {
    let invalid = |_| UnsafeTarMember::new("<tar>", "invalid tar stream");
    let tar = decompress_all_streams(raw).map_err(invalid)?;
    let members = read_members(&tar).map_err(invalid)?;
    validate_members(&members, validation)
}

/// Whether a member belongs under one of `skip_rel_paths` and should be left out.
///
/// `member_name` is the raw name from the archive, which may start with `./` or with the workspace
/// root's own name depending on what wrote it; both spellings are matched. A skip path that is
/// empty once normalized matches everything, as it does in the reference.
pub fn should_skip_tar_member<S: AsRef<str>>(
    member_name: &str,
    skip_rel_paths: impl IntoIterator<Item = S>,
    root_name: Option<&str>,
) -> bool {
    let prefixes: Vec<_> = skip_rel_paths
        .into_iter()
        .map(|path| normalize_rel(path.as_ref()))
        .collect();
    skips(member_name.as_bytes(), &prefixes, root_name)
}

/// [`should_skip_tar_member`] over a name's original bytes and already normalized prefixes.
fn skips(member_name: &[u8], prefixes: &[Vec<u8>], root_name: Option<&str>) -> bool {
    member_rel_variants(member_name, root_name)
        .iter()
        .any(|variant| {
            prefixes
                .iter()
                .any(|prefix| is_within(variant, &parts_of(prefix)))
        })
}

/// Why a prefix could not be stripped.
#[derive(Debug)]
#[non_exhaustive]
pub enum StripPrefixError {
    /// The prefix names nothing once normalized.
    EmptyPrefix,
    /// A member the policy refuses, or one outside the prefix.
    Unsafe(UnsafeTarMember),
    /// The archive could not be read or rewritten.
    Io(std::io::Error),
}

impl fmt::Display for StripPrefixError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPrefix => formatter.write_str("tar member prefix must not be empty"),
            Self::Unsafe(member) => member.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StripPrefixError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyPrefix => None,
            Self::Unsafe(member) => Some(member),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<UnsafeTarMember> for StripPrefixError {
    fn from(member: UnsafeTarMember) -> Self {
        Self::Unsafe(member)
    }
}

impl From<std::io::Error> for StripPrefixError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Rewrites an archive so members under `prefix` are named from the workspace root instead.
///
/// A container runtime archives a workspace copied to `/tmp/stage/workspace` as `workspace/...`; a
/// portable snapshot stores the same files as `.` and `...`, whatever the source backend called its
/// root. The member at the prefix itself, and the archive's own root, become `.`; any member outside
/// the prefix is refused rather than dropped.
///
/// Read as the reference reads it, with the streaming `r|*` reader — so only the first compressed
/// stream — and written as it writes it: an uncompressed PAX archive, directories named with a
/// trailing `/`, a `path` record for a name longer than a header holds or not in ASCII, and every
/// other PAX record the member carried. The result is checked against the default policy before it
/// is returned.
///
/// # Errors
///
/// Returns [`StripPrefixError::EmptyPrefix`] for a prefix that names nothing, a refused member —
/// including `member does not start with prefix: …` — or the failure to read or write the archive.
pub fn strip_tar_member_prefix(data: &[u8], prefix: &str) -> Result<Vec<u8>, StripPrefixError> {
    let prefix = parts_of(&normalize_rel(prefix))
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if prefix.is_empty() {
        return Err(StripPrefixError::EmptyPrefix);
    }

    let tar = decompress_first_stream(data)?;
    let mut builder = tar::Builder::new(Vec::new());
    for member in read_members(&tar)? {
        let stripped = match safe_member_rel_path(&member, true)? {
            None => b".".to_vec(),
            Some(relative) => {
                let parts = parts_of(&relative);
                if parts.len() >= prefix.len()
                    && parts[..prefix.len()]
                        .iter()
                        .zip(&prefix)
                        .all(|(part, wanted)| *part == wanted.as_slice())
                {
                    if parts.len() == prefix.len() {
                        b".".to_vec()
                    } else {
                        parts[prefix.len()..].join(&b'/')
                    }
                } else {
                    let prefix = String::from_utf8_lossy(&prefix.join(&b'/')).into_owned();
                    return Err(UnsafeTarMember::new(
                        &member.name,
                        format!("member does not start with prefix: {prefix}"),
                    )
                    .into());
                }
            }
        };
        append_rewritten(&mut builder, &member, stripped)?;
    }
    let rewritten = builder.into_inner()?;

    validate_members(&read_members(&rewritten)?, &TarValidation::default())?;
    Ok(rewritten)
}

/// `--exclude` arguments that leave `skip_rel_paths` out of a shell `tar` of the workspace.
///
/// Sorted, each path shell-quoted and given twice — as written and with `./` in front — because
/// which spelling a `tar` matches depends on how it was invoked. Empty and root paths are dropped:
/// excluding the root would archive nothing. The reference's `shell_tar_exclude_args`.
pub fn shell_tar_exclude_args<S: AsRef<str>>(
    skip_rel_paths: impl IntoIterator<Item = S>,
) -> Vec<String> {
    let mut paths: Vec<String> = skip_rel_paths
        .into_iter()
        .map(|path| posix_rendering(path.as_ref()))
        .collect();
    paths.sort();

    let mut excludes = Vec::new();
    for path in paths {
        let relative = path.trim_start_matches('/');
        if relative.is_empty() || relative == "." {
            continue;
        }
        excludes.push(format!("--exclude={}", quote(relative)));
        excludes.push(format!("--exclude={}", quote(&format!("./{relative}"))));
    }
    excludes
}

/// A path as `Path.as_posix()` renders it: repeated separators collapsed, `.` components dropped,
/// a trailing separator removed, and a path with nothing left rendered as `.`.
fn posix_rendering(path: &str) -> String {
    let absolute = path.starts_with('/');
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    match (absolute, parts.is_empty()) {
        (true, _) => format!("/{}", parts.join("/")),
        (false, true) => ".".to_owned(),
        (false, false) => parts.join("/"),
    }
}

// --- members ---------------------------------------------------------------------------------

/// What a member is, in the terms the rules are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TarMemberKind {
    Directory,
    File,
    Symlink,
    HardLink,
    Other,
}

/// One archive member, read out of the stream.
pub(crate) struct TarMember {
    /// Diagnostic text only; everything else uses the original bytes in `path`.
    pub(crate) name: String,
    /// The name as the archive holds it, with a directory's trailing `/` removed.
    pub(crate) path: Vec<u8>,
    pub(crate) kind: TarMemberKind,
    pub(crate) link_name: Vec<u8>,
    pub(crate) mode: u32,
    pub(crate) bytes: Vec<u8>,
    /// The member's own header, with its effective extended attributes applied.
    header: tar::Header,
    /// The PAX records in force for this member, global ones included.
    pax: BTreeMap<String, Vec<u8>>,
}

/// The tar inside `data`, decompressed when it is a gzip, bzip2 or xz stream.
///
/// Every stream in the input is read, not only the first: the reference opens a restored
/// workspace in `r:*` mode, whose readers continue into a following gzip member, bzip2 stream or xz
/// stream (checked with Python 3.11). That is the opposite of the streaming reader.
pub(crate) fn decompress_all_streams(data: &[u8]) -> std::io::Result<Cow<'_, [u8]>> {
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

/// The tar inside `data`, reading only the first compressed stream, as the reference's streaming
/// `r|*` reader does.
fn decompress_first_stream(data: &[u8]) -> std::io::Result<Cow<'_, [u8]>> {
    let mut decoder: Box<dyn Read + '_> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(data))
    } else if data.starts_with(b"BZh") {
        Box::new(bzip2::read::BzDecoder::new(data))
    } else if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Box::new(lzma_rust2::XzReader::new(data, false))
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
pub(crate) fn read_members(tar_bytes: &[u8]) -> std::io::Result<Vec<TarMember>> {
    if tar_bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "empty tar stream",
        ));
    }
    let mut input = Cursor::new(tar_bytes);
    let mut global = BTreeMap::<String, Vec<u8>>::new();
    let mut pending = BTreeMap::<String, Vec<u8>>::new();
    let mut pending_gnu = BTreeMap::<String, Vec<u8>>::new();
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
            pending_gnu.entry(key.to_owned()).or_insert(value);
        } else {
            needs_member = false;
            drop(entry);
            input.set_position(start);
            let mut pax = global.clone();
            pax.append(&mut pending);
            // An earlier extension wraps a later one in Python's recursive reader, so a local PAX
            // name wins over a GNU one that follows it.
            let mut effective = pax.clone();
            for (key, value) in std::mem::take(&mut pending_gnu) {
                effective.entry(key).or_insert(value);
            }
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
            members.push(read_member(&mut entry, pax)?);
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
fn read_member<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    pax: BTreeMap<String, Vec<u8>>,
) -> std::io::Result<TarMember> {
    let header = entry.header().clone();
    let entry_type = header.entry_type();
    let old_style = header.as_old().linkflag[0] == 0;
    let mode = header.mode().unwrap_or(0o644);
    let mut raw_name = entry.path_bytes().into_owned();
    let link_name = entry.link_name_bytes().unwrap_or_default().into_owned();
    let mut kind = match entry_type {
        tar::EntryType::Directory => TarMemberKind::Directory,
        tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
            TarMemberKind::File
        }
        tar::EntryType::Symlink => TarMemberKind::Symlink,
        tar::EntryType::Link => TarMemberKind::HardLink,
        _ => TarMemberKind::Other,
    };
    if kind == TarMemberKind::File && old_style && raw_name.ends_with(b"/") {
        kind = TarMemberKind::Directory;
    }
    if kind == TarMemberKind::Directory {
        while raw_name.last() == Some(&b'/') {
            raw_name.pop();
        }
    }
    let mut bytes = Vec::new();
    if kind == TarMemberKind::File {
        entry.read_to_end(&mut bytes)?;
    }
    Ok(TarMember {
        name: String::from_utf8_lossy(&raw_name).into_owned(),
        path: raw_name,
        kind,
        link_name,
        mode,
        bytes,
        header,
        pax,
    })
}

/// Writes `member` under `name`, as the reference's PAX writer would.
fn append_rewritten(
    builder: &mut tar::Builder<Vec<u8>>,
    member: &TarMember,
    mut name: Vec<u8>,
) -> std::io::Result<()> {
    const NAME_LENGTH: usize = 100;
    const LINK_LENGTH: usize = 100;

    // Python names a directory with a trailing separator when it writes one, and removes it again
    // when it reads one back.
    if member.kind == TarMemberKind::Directory && !name.ends_with(b"/") {
        name.push(b'/');
    }

    let mut pax = member.pax.clone();
    pax.remove("path");
    if !name.is_ascii() || name.len() > NAME_LENGTH {
        pax.insert("path".to_owned(), name.clone());
    }
    if !pax.contains_key("linkpath")
        && (!member.link_name.is_ascii() || member.link_name.len() > LINK_LENGTH)
    {
        pax.insert("linkpath".to_owned(), member.link_name.clone());
    }
    // GNU base-256 times are signed; the tar crate's accessor reads them as u64.
    // Preserve the signed value in PAX when it cannot fit the unsigned octal field,
    // as Python's PAX writer does, retaining an existing PAX value's precedence.
    let raw_mtime = &member.header.as_old().mtime;
    let mtime = if matches!(raw_mtime[0], 0x80 | 0xff) {
        raw_mtime[1..].iter().fold(
            if raw_mtime[0] == 0xff { -1_i128 } else { 0 },
            |value, byte| (value << 8) | i128::from(*byte),
        )
    } else {
        i128::from(member.header.mtime()?)
    };
    let header_mtime =
        if let Some(value) = u64::try_from(mtime).ok().filter(|value| *value < (1 << 33)) {
            value
        } else {
            pax.entry("mtime".to_owned())
                .or_insert_with(|| mtime.to_string().into_bytes());
            0
        };
    if !pax.is_empty() {
        builder.append_pax_extensions(
            pax.iter()
                .map(|(key, value)| (key.as_str(), value.as_slice())),
        )?;
    }

    let source = &member.header;
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(source.entry_type());
    header.set_mode(member.mode);
    header.set_uid(source.uid().unwrap_or(0));
    header.set_gid(source.gid().unwrap_or(0));
    header.set_mtime(header_mtime);
    if let Some(fields) = header.as_ustar_mut() {
        copy_truncated(
            &mut fields.uname,
            source.username_bytes().unwrap_or_default(),
        );
        copy_truncated(
            &mut fields.gname,
            source.groupname_bytes().unwrap_or_default(),
        );
    }
    // Written as the reference writes a header field: the leading bytes that fit, with the whole
    // value in the PAX record when they are not all of it.
    copy_truncated(&mut header.as_old_mut().name, &name);
    copy_truncated(&mut header.as_old_mut().linkname, &member.link_name);
    header.set_size(member.bytes.len() as u64);
    header.set_cksum();
    builder.append(&header, member.bytes.as_slice())
}

/// Copies as much of `value` as fits into a NUL-padded header field.
fn copy_truncated(field: &mut [u8], value: &[u8]) {
    field.fill(0);
    let length = value.len().min(field.len());
    field[..length].copy_from_slice(&value[..length]);
}

// --- rules -----------------------------------------------------------------------------------

/// A POSIX path's components, without the empty and `.` ones.
fn parts_of(path: &[u8]) -> Vec<&[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect()
}

/// A workspace-relative path with `.` components and any leading root removed.
fn normalize_rel(path: &str) -> Vec<u8> {
    parts_of(path.as_bytes()).join(&b'/')
}

/// The workspace-relative paths a raw member name can stand for.
///
/// The name as written, and, when it starts with the workspace root's own name, the name without
/// it. A name with no components at all is the root, spelled as the empty path.
fn member_rel_variants(name: &[u8], root_name: Option<&str>) -> Vec<Vec<u8>> {
    let parts = parts_of(name);
    if parts.is_empty() {
        return vec![Vec::new()];
    }
    let mut variants = vec![parts.join(&b'/')];
    if let Some(root_name) = root_name
        && !root_name.is_empty()
        && parts[0] == root_name.as_bytes()
    {
        variants.push(parts[1..].join(&b'/'));
    }
    variants
}

/// Whether `path` is `prefix` or lies under it; everything lies under the empty path.
fn is_within(path: &[u8], prefix: &[&[u8]]) -> bool {
    let parts = parts_of(path);
    parts.len() >= prefix.len() && parts[..prefix.len()] == *prefix
}

/// Whether a member name starts with a Windows drive, as `PureWindowsPath(name).drive` decides.
///
/// A letter and a colon, or a UNC `\\server\share` with either separator. Python 3.11's rules,
/// checked against it: two separators, then a non-empty server, a separator and a share that does
/// not start with another separator; an extended `\\?\` prefix is itself a drive.
pub(crate) fn windows_drive(name: &str) -> bool {
    let normalized: Vec<u8> = name
        .bytes()
        .map(|byte| if byte == b'/' { b'\\' } else { byte })
        .collect();
    if normalized.starts_with(br"\\?\") {
        return true;
    }
    if normalized.starts_with(br"\\") && normalized.get(2) != Some(&b'\\') {
        if let Some(index) = normalized[2..].iter().position(|byte| *byte == b'\\') {
            let index = index + 2;
            // A UNC path cannot have two separators in a row after the first two.
            if normalized.get(index + 1) != Some(&b'\\') {
                return true;
            }
        }
        return false;
    }
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.next() == Some(':')
}

/// Validates one member's path and kind, or answers `None` for the archive's own root entry.
///
/// The reference's `safe_tar_member_rel_path`. The relative path comes back as its components
/// joined with `/`.
pub(crate) fn safe_member_rel_path(
    member: &TarMember,
    allow_symlinks: bool,
) -> Result<Option<Vec<u8>>, UnsafeTarMember> {
    let name = member.path.as_slice();
    let refuse = |reason: &str| Err(UnsafeTarMember::new(&member.name, reason));
    if matches!(name, b"" | b"." | b"./") {
        // Skipped rather than written, but only when it really is a directory: anything else
        // there is a claim about the destination itself.
        return match member.kind {
            TarMemberKind::Directory => Ok(None),
            TarMemberKind::Symlink => refuse("archive root symlink"),
            TarMemberKind::HardLink => refuse("archive root hardlink"),
            TarMemberKind::File | TarMemberKind::Other => {
                refuse("archive root member must be directory")
            }
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
    let parts = parts_of(name);
    if parts.contains(&b"..".as_slice()) {
        return refuse("parent traversal");
    }
    match member.kind {
        TarMemberKind::Symlink if !allow_symlinks => refuse("symlink member not allowed"),
        TarMemberKind::HardLink => refuse("hardlink member not allowed"),
        TarMemberKind::Other => refuse("unsupported member type"),
        TarMemberKind::Directory | TarMemberKind::File | TarMemberKind::Symlink => {
            Ok(Some(parts.join(&b'/')))
        }
    }
}

/// Renders a relative path for a reason, lossily.
fn shown(path: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(path)
}

/// Checks an archive as a whole before anything is written.
///
/// The reference's `validate_tarfile` with symlink members allowed: every member's path and kind,
/// the protected and skipped paths, no path twice unless both are directories, symlink targets as
/// the validation says, and no member beneath a symlink or beneath something that is not a
/// directory.
pub(crate) fn validate_members(
    members: &[TarMember],
    validation: &TarValidation,
) -> Result<(), UnsafeTarMember> {
    let rejected: BTreeSet<Vec<u8>> = validation
        .rejected
        .iter()
        .map(|path| normalize_rel(path))
        .collect();
    let rejected_symlinks: BTreeSet<Vec<u8>> = validation
        .rejected_symlinks
        .iter()
        .map(|path| normalize_rel(path))
        .collect();
    let skipped: Vec<Vec<u8>> = validation
        .skipped
        .iter()
        .map(|path| normalize_rel(path))
        .collect();
    let mut kinds: BTreeMap<Vec<u8>, TarMemberKind> = BTreeMap::new();
    let mut symlinks: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut checked: Vec<(&TarMember, Vec<u8>)> = Vec::new();

    for member in members {
        if skips(&member.path, &skipped, validation.root_name.as_deref()) {
            continue;
        }
        let Some(relative) = safe_member_rel_path(member, true)? else {
            continue;
        };

        let variants = member_rel_variants(&member.path, validation.root_name.as_deref());
        for protected in &rejected {
            let protected_parts = parts_of(protected);
            let overlaps = variants.iter().any(|variant| {
                is_within(variant, &protected_parts)
                    || (member.kind != TarMemberKind::Directory
                        && is_within(protected, &parts_of(variant)))
            });
            if overlaps {
                return Err(UnsafeTarMember::new(
                    &member.name,
                    format!(
                        "archive member overlaps protected path: {}",
                        shown(protected)
                    ),
                ));
            }
        }

        if let Some(previous) = kinds.get(&relative)
            && !(*previous == TarMemberKind::Directory && member.kind == TarMemberKind::Directory)
        {
            return Err(UnsafeTarMember::new(
                &member.name,
                format!("duplicate archive path: {}", shown(&relative)),
            ));
        }
        kinds.insert(relative.clone(), member.kind);

        if member.kind == TarMemberKind::Symlink {
            if !validation.external_symlink_targets {
                validate_symlink_target(member, &relative)?;
            }
            if rejected_symlinks.contains(&relative) {
                return Err(UnsafeTarMember::new(
                    &member.name,
                    format!("symlink member not allowed: {}", shown(&relative)),
                ));
            }
            symlinks.insert(relative.clone());
        }
        checked.push((member, relative));
    }

    for (member, relative) in checked {
        let parts = parts_of(&relative);
        for depth in (1..parts.len()).rev() {
            let parent = parts[..depth].join(&b'/');
            if symlinks.contains(&parent) {
                return Err(UnsafeTarMember::new(
                    &member.name,
                    format!("archive path descends through symlink: {}", shown(&parent)),
                ));
            }
            if kinds
                .get(&parent)
                .is_some_and(|kind| *kind != TarMemberKind::Directory)
            {
                return Err(UnsafeTarMember::new(
                    &member.name,
                    format!(
                        "archive path descends through non-directory: {}",
                        shown(&parent)
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Refuses a symlink member whose target leaves the archive.
fn validate_symlink_target(member: &TarMember, relative: &[u8]) -> Result<(), UnsafeTarMember> {
    let target = member.link_name.as_slice();
    if target.starts_with(b"/") {
        return Err(UnsafeTarMember::new(
            &member.name,
            format!("absolute symlink target not allowed: {}", shown(target)),
        ));
    }
    // Normalized from the link's own directory, with `..` allowed as long as it never climbs above
    // the archive root on the way.
    let parts = parts_of(relative);
    let parent = &parts[..parts.len().saturating_sub(1)];
    let mut depth: usize = 0;
    for part in parent.iter().copied().chain(parts_of(target)) {
        if part == b".." {
            let Some(shallower) = depth.checked_sub(1) else {
                return Err(UnsafeTarMember::new(
                    &member.name,
                    format!("symlink target escapes archive root: {}", shown(target)),
                ));
            };
            depth = shallower;
        } else {
            depth += 1;
        }
    }
    Ok(())
}

// --- extraction ------------------------------------------------------------------------------

#[cfg(unix)]
pub use extract::{SafeExtractError, safe_extract_tarfile};

#[cfg(unix)]
mod extract {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use ra_core::sandbox::SandboxError;

    use super::{
        TarMember, TarMemberKind, TarValidation, UnsafeTarMember, decompress_all_streams,
        read_members, safe_member_rel_path, validate_members,
    };
    use crate::host_paths::resolve_without_strictness;

    /// Why an extraction stopped.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum SafeExtractError {
        /// A member the archive may not carry, or may not write where it asked to.
        Unsafe(UnsafeTarMember),
        /// The archive could not be read, or a member could not be written.
        Io(std::io::Error),
        /// A destination's symlinks could not be resolved.
        Resolve(SandboxError),
    }

    impl std::fmt::Display for SafeExtractError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Unsafe(member) => member.fmt(formatter),
                Self::Io(error) => error.fmt(formatter),
                Self::Resolve(error) => error.fmt(formatter),
            }
        }
    }

    impl std::error::Error for SafeExtractError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Unsafe(member) => Some(member),
                Self::Io(error) => Some(error),
                Self::Resolve(error) => Some(error),
            }
        }
    }

    impl From<std::io::Error> for SafeExtractError {
        fn from(error: std::io::Error) -> Self {
            Self::Io(error)
        }
    }

    impl From<UnsafeTarMember> for SafeExtractError {
        fn from(member: UnsafeTarMember) -> Self {
            Self::Unsafe(member)
        }
    }

    /// Extracts a workspace archive into `root`, which is created if it is missing.
    ///
    /// The reference's `safe_extract_tarfile`. The whole archive is checked before the first member
    /// is written, so a refusal leaves `root` as it was; directories and files are written next,
    /// and symlinks last, so nothing the archive carries is ever written through a link the same
    /// archive created. No destination is written through a symlink already on disk, and a file is
    /// created exclusively, without following a link, private until its content is complete. A
    /// gzip, bzip2 or xz archive is decompressed first, every stream of it, as the reference's
    /// `r:*` reader does.
    ///
    /// `allow_external_symlink_targets` is the reference's option of the same name, on by default
    /// there; the local backend's restore turns it off.
    ///
    /// # Errors
    ///
    /// Returns the member refused, the failure to read the archive or write a member, or the
    /// failure to resolve a destination.
    pub fn safe_extract_tarfile(
        data: &[u8],
        root: &Path,
        allow_external_symlink_targets: bool,
    ) -> Result<(), SafeExtractError> {
        std::fs::create_dir_all(root)?;
        let root = resolve_without_strictness(root).map_err(SafeExtractError::Resolve)?;

        let members = read_members(&decompress_all_streams(data)?)?;
        validate_members(
            &members,
            &TarValidation::new().with_external_symlink_targets(allow_external_symlink_targets),
        )?;

        for member in &members {
            let Some(relative) = relative_path(member)? else {
                continue;
            };
            let destination = root.join(&relative);
            match member.kind {
                TarMemberKind::Directory => {
                    prepare_directory_leaf(&root, &destination)?;
                    std::fs::create_dir_all(&destination)?;
                }
                TarMemberKind::File => write_file(&root, &destination, &relative, member)?,
                // Symlinks come after everything else; the other two kinds were refused above.
                TarMemberKind::Symlink | TarMemberKind::HardLink | TarMemberKind::Other => {}
            }
        }
        for member in &members {
            if member.kind != TarMemberKind::Symlink {
                continue;
            }
            let Some(relative) = relative_path(member)? else {
                continue;
            };
            let destination = root.join(&relative);
            prepare_replaceable_leaf(&root, &destination, &relative, &member.name)?;
            std::os::unix::fs::symlink(OsStr::from_bytes(&member.link_name), &destination)?;
        }
        Ok(())
    }

    /// A member's workspace-relative path, under its original bytes.
    fn relative_path(member: &TarMember) -> Result<Option<PathBuf>, UnsafeTarMember> {
        Ok(safe_member_rel_path(member, true)?
            .map(|relative| PathBuf::from(OsStr::from_bytes(&relative))))
    }

    /// Renders a relative path with forward slashes, as the reference's reasons do.
    fn as_posix(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// Refuses a destination whose parent resolves outside the root, or passes through a symlink
    /// that is already on disk.
    ///
    /// The reference's `_ensure_no_symlink_parents` with the leaf left out: the leaf is about to be
    /// replaced, and whether it is a link is the caller's question.
    fn ensure_no_symlink_parents(root: &Path, destination: &Path) -> Result<(), SafeExtractError> {
        let parent = destination.parent().unwrap_or(root);
        let resolved = resolve_without_strictness(parent).map_err(SafeExtractError::Resolve)?;
        if !resolved.starts_with(root) {
            return Err(UnsafeTarMember::new(
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
                return Err(
                    UnsafeTarMember::new(&as_posix(relative), "symlink in parent path").into(),
                );
            }
        }
        Ok(())
    }

    /// Makes room for a directory: a link or a file in its place is removed, a directory is kept.
    fn prepare_directory_leaf(root: &Path, destination: &Path) -> Result<(), SafeExtractError> {
        ensure_no_symlink_parents(root, destination)?;
        if destination.is_symlink() || (destination.exists() && !destination.is_dir()) {
            std::fs::remove_file(destination)?;
        }
        Ok(())
    }

    /// Makes room for a file or a link, refusing to replace a directory.
    ///
    /// A link to a directory is replaced — it is the link that is removed, not what it names — but
    /// a real directory is refused rather than emptied: whatever it holds was not in the archive.
    fn prepare_replaceable_leaf(
        root: &Path,
        destination: &Path,
        relative: &Path,
        name: &str,
    ) -> Result<(), SafeExtractError> {
        ensure_no_symlink_parents(root, destination)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if destination.is_dir() && !destination.is_symlink() {
            return Err(UnsafeTarMember::new(
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
    /// Created exclusively and without following a link, so a link that appeared since the leaf
    /// was cleared is not written through. The file stays private while it holds partial content
    /// and gets its restored mode once the content is complete, which is the reference's order.
    fn write_file(
        root: &Path,
        destination: &Path,
        relative: &Path,
        member: &TarMember,
    ) -> Result<(), SafeExtractError> {
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
    /// The reference's policy, and the standard library filter's before it: setuid, setgid, the
    /// sticky bit and group or other write access are dropped, execute survives only where the
    /// owner had it, and the owner can always read and write what was extracted.
    const fn restored_mode(mode: u32) -> u32 {
        let mut restored = mode & 0o755;
        if restored & 0o100 == 0 {
            restored &= !0o111;
        }
        restored | 0o600
    }
}
