//! What an archive handed to a session is, and what unpacking it is allowed to cost.
//!
//! A caller can hand a session an archive and ask for it to be unpacked into the workspace. The
//! archive arrives from wherever the caller got it, which makes it the one workspace input that is
//! both attacker-shaped and cheap to produce: a few kilobytes of members can declare gigabytes of
//! files, and a few bytes of path can name somewhere outside the workspace entirely.
//!
//! The path rules are the extractor's, in the service crate. What lives here is the declaration a
//! caller configures: which format the archive is in, and the three ceilings that stop a small
//! archive from becoming a large workspace.

use serde::{Deserialize, Serialize};

/// How many bytes of archive a session accepts, when a caller asks for the built-in limits.
pub const DEFAULT_MAX_ARCHIVE_INPUT_BYTES: u64 = 1024 * 1024 * 1024;

/// How many bytes an archive may declare it will write, under those same limits.
pub const DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How many members an archive may have, under those same limits.
pub const DEFAULT_MAX_ARCHIVE_MEMBERS: usize = 100_000;

/// The formats an archive can be unpacked from.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionScheme {
    /// A tar archive, plain or compressed with gzip, bzip2 or xz; the compression is read from the
    /// archive's own bytes, not from its name.
    Tar,
    /// A zip archive.
    Zip,
}

impl CompressionScheme {
    /// The scheme's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::Zip => "zip",
        }
    }

    /// Reads the scheme a caller wrote, or `None` for anything else.
    #[must_use]
    pub fn parse(scheme: &str) -> Option<Self> {
        match scheme {
            "tar" => Some(Self::Tar),
            "zip" => Some(Self::Zip),
            _ => None,
        }
    }

    /// Guesses the scheme from the archive's own name.
    ///
    /// The last extension and nothing cleverer, which is the reference's rule: `archive.tar` is a
    /// tar, `archive.tar.gz` is **not** — its last extension is `gz`, and a caller who meant a
    /// compressed tar has to say so rather than have it inferred.
    #[must_use]
    pub fn from_file_name(file_name: &str) -> Option<Self> {
        Self::parse(file_name_suffix(file_name)?)
    }
}

/// The extension a file name ends with, without its dot.
///
/// A name that is all extension has none: `.tar` is a file called `.tar`, not a tar file, and the
/// reference reads it the same way. So is a name ending in a dot, which announces an extension and
/// then does not give one.
#[must_use]
pub fn file_name_suffix(file_name: &str) -> Option<&str> {
    let dot = file_name.rfind('.')?;
    if dot == 0 || dot + 1 == file_name.len() {
        return None;
    }
    Some(&file_name[dot + 1..])
}

/// What unpacking one archive may cost.
///
/// Three ceilings rather than one, because an archive can be too large in three unrelated ways: the
/// bytes that arrive, the bytes the members say they will become, and how many members there are.
/// A zip bomb is small on all but one of those. `None` on a field means that ceiling does not
/// apply.
///
/// **A caller who supplies none of this gets no ceilings at all**, which is the reference's
/// default: `extract` takes these as an option, and the option being absent means the SDK imposes
/// nothing. [`Self::default`] is the opt-in set of built-in values.
// The shared `max_` prefix is the reference's field naming, and these are the names a caller
// reads in its documentation.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxArchiveLimits {
    max_input_bytes: Option<u64>,
    max_extracted_bytes: Option<u64>,
    max_members: Option<usize>,
}

impl Default for SandboxArchiveLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: Some(DEFAULT_MAX_ARCHIVE_INPUT_BYTES),
            max_extracted_bytes: Some(DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES),
            max_members: Some(DEFAULT_MAX_ARCHIVE_MEMBERS),
        }
    }
}

impl SandboxArchiveLimits {
    /// The built-in ceilings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts at most `limit` bytes of archive, or any size when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveLimitError::InputBytes`] for a limit of zero, which would refuse every
    /// archive including an empty one, and reads as "unlimited" to anyone skimming.
    pub fn with_max_input_bytes(mut self, limit: Option<u64>) -> Result<Self, ArchiveLimitError> {
        if limit == Some(0) {
            return Err(ArchiveLimitError::InputBytes);
        }
        self.max_input_bytes = limit;
        Ok(self)
    }

    /// Unpacks at most `limit` declared bytes, or any number when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveLimitError::ExtractedBytes`] for a limit of zero.
    pub fn with_max_extracted_bytes(
        mut self,
        limit: Option<u64>,
    ) -> Result<Self, ArchiveLimitError> {
        if limit == Some(0) {
            return Err(ArchiveLimitError::ExtractedBytes);
        }
        self.max_extracted_bytes = limit;
        Ok(self)
    }

    /// Unpacks at most `limit` members, or any number when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveLimitError::Members`] for a limit of zero.
    pub fn with_max_members(mut self, limit: Option<usize>) -> Result<Self, ArchiveLimitError> {
        if limit == Some(0) {
            return Err(ArchiveLimitError::Members);
        }
        self.max_members = limit;
        Ok(self)
    }

    /// How many bytes of archive are accepted.
    #[must_use]
    pub const fn max_input_bytes(self) -> Option<u64> {
        self.max_input_bytes
    }

    /// How many declared bytes may be unpacked.
    #[must_use]
    pub const fn max_extracted_bytes(self) -> Option<u64> {
        self.max_extracted_bytes
    }

    /// How many members may be unpacked.
    #[must_use]
    pub const fn max_members(self) -> Option<usize> {
        self.max_members
    }
}

/// An archive ceiling that would refuse everything rather than bound anything.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveLimitError {
    /// The accepted-input ceiling was zero.
    #[error("archive_limits.max_input_bytes must be at least 1")]
    InputBytes,
    /// The unpacked-size ceiling was zero.
    #[error("archive_limits.max_extracted_bytes must be at least 1")]
    ExtractedBytes,
    /// The member-count ceiling was zero.
    #[error("archive_limits.max_members must be at least 1")]
    Members,
}
