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
