//! What a manifest application actually put in the workspace.
//!
//! A receipt, not a plan. The manifest says what should be there; this says what was written and
//! what it hashed to, which is what lets a caller tell a fresh materialization from a reused
//! workspace without reading the files back out of the sandbox.
//!
//! Not everything can produce one. A git checkout copies its files with a command inside the
//! sandbox, so computing checksums would mean reading every file back out again; the reference
//! leaves those out rather than pay for it, and an empty receipt therefore does not mean nothing was
//! written.

use super::workspace_paths::PosixPath;

/// One file a manifest application wrote, and what it hashed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedFile {
    path: PosixPath,
    sha256: String,
}

impl MaterializedFile {
    /// Records one written file.
    #[must_use]
    pub fn new(path: PosixPath, sha256: impl Into<String>) -> Self {
        Self {
            path,
            sha256: sha256.into(),
        }
    }

    /// Where the file was written.
    #[must_use]
    pub const fn path(&self) -> &PosixPath {
        &self.path
    }

    /// The file's SHA-256, lowercase hexadecimal.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// What one manifest application wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializationResult {
    files: Vec<MaterializedFile>,
}

impl MaterializationResult {
    /// An application that wrote nothing it can account for.
    #[must_use]
    pub const fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Records the files one application wrote.
    #[must_use]
    pub fn with_files(files: Vec<MaterializedFile>) -> Self {
        Self { files }
    }

    /// The files written, in the order they were reported.
    #[must_use]
    pub fn files(&self) -> &[MaterializedFile] {
        &self.files
    }

    /// Whether this application accounted for any file.
    ///
    /// **Not the same as "nothing was written."** An entry that materializes with a command inside
    /// the sandbox writes files it cannot hash without reading them back, and reports none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl FromIterator<MaterializedFile> for MaterializationResult {
    fn from_iter<T: IntoIterator<Item = MaterializedFile>>(files: T) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }
}

/// How many manifest entries are materialized at once unless a host says otherwise.
pub const DEFAULT_MAX_MANIFEST_ENTRY_CONCURRENCY: usize = 4;

/// How many files one copied host directory contributes at once unless a host says otherwise.
pub const DEFAULT_MAX_LOCAL_DIR_FILE_CONCURRENCY: usize = 4;

/// How much of a manifest application may be in flight at once.
///
/// Two limits rather than one, because they bound different things: a manifest with four large
/// directory entries and a directory holding four thousand files are not the same amount of work,
/// and a single number would have to be wrong for one of them. `None` means unbounded — every unit
/// of that kind starts at once — which is what a caller asks for when the backend, not this, is the
/// thing doing the rationing.
///
/// **The reference declares this on its run configuration** (`run_config.py`) and applies it to a
/// session after the session exists. It lives next to the materialization it governs here because
/// the run configuration has not been carried over yet; when it is, it passes one of these rather
/// than growing its own pair of numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxConcurrencyLimits {
    manifest_entries: Option<usize>,
    local_dir_files: Option<usize>,
}

impl Default for SandboxConcurrencyLimits {
    fn default() -> Self {
        Self {
            manifest_entries: Some(DEFAULT_MAX_MANIFEST_ENTRY_CONCURRENCY),
            local_dir_files: Some(DEFAULT_MAX_LOCAL_DIR_FILE_CONCURRENCY),
        }
    }
}

impl SandboxConcurrencyLimits {
    /// The limits a host gets when it does not choose.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Materializes at most `limit` manifest entries at once, or all of them when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ConcurrencyLimitError::ManifestEntries`] for a limit of zero, which asks for work
    /// to proceed with nothing in flight and would otherwise read as "unbounded" to whatever
    /// divided by it.
    pub fn with_manifest_entries(
        mut self,
        limit: Option<usize>,
    ) -> Result<Self, ConcurrencyLimitError> {
        if limit == Some(0) {
            return Err(ConcurrencyLimitError::ManifestEntries);
        }
        self.manifest_entries = limit;
        Ok(self)
    }

    /// Copies at most `limit` files of one host directory at once, or all of them when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ConcurrencyLimitError::LocalDirFiles`] for a limit of zero, as
    /// [`Self::with_manifest_entries`] does.
    pub fn with_local_dir_files(
        mut self,
        limit: Option<usize>,
    ) -> Result<Self, ConcurrencyLimitError> {
        if limit == Some(0) {
            return Err(ConcurrencyLimitError::LocalDirFiles);
        }
        self.local_dir_files = limit;
        Ok(self)
    }

    /// How many manifest entries may be materialized at once.
    #[must_use]
    pub const fn manifest_entries(self) -> Option<usize> {
        self.manifest_entries
    }

    /// How many files of one copied host directory may be in flight at once.
    #[must_use]
    pub const fn local_dir_files(self) -> Option<usize> {
        self.local_dir_files
    }
}

/// A concurrency limit that would stop work rather than pace it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConcurrencyLimitError {
    /// The manifest entry limit was zero.
    #[error("max_entry_concurrency must be at least 1")]
    ManifestEntries,
    /// The copied-directory file limit was zero.
    #[error("max_concurrency must be at least 1")]
    LocalDirFiles,
}
