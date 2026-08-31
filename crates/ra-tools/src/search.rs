//! Shared workspace traversal and output budget for the native search tools.

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use globset::{GlobBuilder, GlobMatcher};
use ra_core::tool::{Truncation, TruncationStage};
use ra_exec::fs::{RootedFileSystem, RootedOpenError};

/// Directory names a workspace search does not descend into on its own.
///
/// Every one of them holds generated, vendored, or metadata content that no one greps for on
/// purpose, and each is routinely larger than the source it sits beside — this workspace carries
/// 444 tracked files and 135,695 under `target`. Skipping them is what makes an unscoped search
/// cost the size of the project rather than the size of the disk.
///
/// A name is only refused as a directory this walk *descends into*. A search that explicitly names
/// one as its `path` still searches it, because then the model has asked for it.
const UNSEARCHED_DIRECTORIES: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

/// The ceiling on how many files one search visits.
///
/// The skip list above is a policy about the trees that are known to be uninteresting; this is the
/// backstop for the one it does not name. A search that reaches the ceiling reports it rather than
/// presenting a partial walk as a complete one.
pub(super) const MAX_WALKED_FILES: usize = 50_000;

/// Compiles one model-supplied glob, treating `/` as a component boundary.
///
/// globset's default lets `*` and `?` match a separator, which is not what either tool's callers
/// mean: it makes `src/*.rs` answer with everything under `src`, and leaves `**` denoting nothing
/// that a single star does not already denote — so a model has no way to ask for one directory's
/// own files. Both tools compile through here, so a pattern selects the same files whichever one
/// the model reaches for.
pub(super) fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
}

/// Where a search reads from, and by what authority.
#[derive(Clone)]
pub(super) enum SearchRoot {
    Ambient(PathBuf),
    Rooted {
        root: PathBuf,
        filesystem: Arc<RootedFileSystem>,
    },
}

impl SearchRoot {
    /// Turns the model's `path` into one relative to this root, or into the reason it will not be
    /// searched.
    ///
    /// An absolute path inside the root is accepted and stripped, exactly as `read_file` accepts
    /// one: it is the same request written the way a model writes it after reading a path out of a
    /// previous result. A path spelled with `..` is refused rather than normalized, because
    /// normalizing it changes which directory was asked for whenever a component is a link.
    pub(super) fn resolve(&self, requested: Option<&str>) -> Result<PathBuf, SearchPathError> {
        let requested = requested.unwrap_or(".");
        let requested_path = Path::new(requested);
        let root = match self {
            Self::Ambient(root) | Self::Rooted { root, .. } => root,
        };
        let relative = if requested_path.is_absolute() {
            requested_path
                .strip_prefix(root)
                .map_err(|_| SearchPathError::OutsideRoot(requested.to_owned()))?
                .to_path_buf()
        } else {
            requested_path.to_path_buf()
        };
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(SearchPathError::InvalidPath(requested.to_owned()));
        }
        Ok(normalize_relative(&relative))
    }

    /// Walks the files at or below `base`, which must exist.
    pub(super) fn files_below(&self, base: &Path) -> Result<SearchWalk, SearchPathError> {
        match self {
            Self::Ambient(root) => {
                let directory = root.join(base);
                let mut walk = SearchWalk::default();
                if directory.is_file() {
                    walk.files.push(base.to_path_buf());
                    return Ok(walk);
                }
                walk_ambient(&directory, base, &mut walk)
                    .map_err(|error| SearchPathError::from_io(error, base))?;
                walk.files.sort();
                Ok(walk)
            }
            Self::Rooted { filesystem, .. } => {
                let walked = filesystem
                    .walk_files_below(base, MAX_WALKED_FILES, &|directory| {
                        !is_unsearched(directory)
                    })
                    .map_err(|error| SearchPathError::from_rooted(error, base))?;
                Ok(SearchWalk {
                    unreadable: walked.unreadable_entries(),
                    stopped_at_limit: walked.stopped_at_limit(),
                    files: walked.into_files(),
                })
            }
        }
    }

    /// Reads a file through the selected capability, retaining one byte beyond the ceiling so a
    /// file over it is recognized from the read itself rather than from a separate stat that the
    /// file could have outgrown in between.
    pub(super) fn read_up_to(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, SearchPathError> {
        use std::io::Read as _;

        let mut file = match self {
            Self::Ambient(root) => std::fs::File::open(root.join(path))
                .map_err(|error| SearchPathError::from_io(error, path))?,
            Self::Rooted { filesystem, .. } => filesystem
                .open_read(path)
                .map_err(|error| SearchPathError::from_rooted(error, path))?,
        };
        let mut bytes = Vec::new();
        file.by_ref()
            .take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| SearchPathError::from_io(error, path))?;
        Ok((bytes.len() <= max_bytes).then_some(bytes))
    }
}

/// The files one walk offered a search, and what it could not look at.
#[derive(Default)]
pub(super) struct SearchWalk {
    pub(super) files: Vec<PathBuf>,
    pub(super) unreadable: usize,
    pub(super) stopped_at_limit: bool,
}

fn is_unsearched(directory: &Path) -> bool {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| UNSEARCHED_DIRECTORIES.contains(&name))
}

fn normalize_relative(path: &Path) -> PathBuf {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => None,
        })
        .collect()
}

/// Walks an ambient directory tree, keeping only the failure of `directory` itself fatal.
fn walk_ambient(
    directory: &Path,
    prefix: &Path,
    walk: &mut SearchWalk,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(directory)? {
        if walk.files.len() >= MAX_WALKED_FILES {
            walk.stopped_at_limit = true;
            return Ok(());
        }
        let Ok(entry) = entry else {
            walk.unreadable = walk.unreadable.saturating_add(1);
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            walk.unreadable = walk.unreadable.saturating_add(1);
            continue;
        };
        let relative = prefix.join(entry.file_name());
        if file_type.is_file() {
            walk.files.push(relative);
        } else if file_type.is_dir() {
            if is_unsearched(&relative) {
                continue;
            }
            if walk_ambient(&entry.path(), &relative, walk).is_err() {
                walk.unreadable = walk.unreadable.saturating_add(1);
            }
            if walk.stopped_at_limit {
                return Ok(());
            }
        } else if file_type.is_symlink()
            && std::fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_file())
        {
            walk.files.push(relative);
        }
    }
    Ok(())
}

/// The output budget the search tools share: how many result lines fit, how many bytes, and what
/// the pair means for the truncation the host records.
///
/// One type for both tools because the accounting is where they agree — the sentences they write
/// around it are their own.
pub(super) struct SearchReport {
    limit: usize,
    byte_limit: usize,
    body: String,
    original_bytes: usize,
    returned: usize,
    full: bool,
}

impl SearchReport {
    pub(super) const fn new(limit: usize, byte_limit: usize) -> Self {
        Self {
            limit,
            byte_limit,
            body: String::new(),
            original_bytes: 0,
            returned: 0,
            full: false,
        }
    }

    /// Offers one result line, which cost `source_bytes` before the caller shortened it.
    ///
    /// Passing the pre-cut cost is what lets a line the caller abbreviated show up in the
    /// truncation as bytes the model did not receive.
    ///
    /// **The first line that does not fit closes the report.** Skipping it and taking a shorter one
    /// further down would leave the model holding a sifted subset while both tools tell it that it
    /// holds the first `returned` results — and a subset assembled by length is not something a
    /// narrower `path` or pattern lets it page through. Latching here is what makes that sentence
    /// true, and it is why `original_bytes` keeps accruing afterwards: the totals the summary
    /// reports are still counted from every result, only the body stops growing.
    pub(super) fn push(&mut self, line: &str, source_bytes: usize) {
        self.original_bytes = self.original_bytes.saturating_add(source_bytes);
        if self.full {
            return;
        }
        if self.returned >= self.limit
            || self.body.len().saturating_add(line.len()) > self.byte_limit
        {
            self.full = true;
            return;
        }
        self.body.push_str(line);
        self.returned = self.returned.saturating_add(1);
    }

    pub(super) const fn returned(&self) -> usize {
        self.returned
    }

    /// Appends `summary` and reports the truncation the two byte counts imply.
    ///
    /// The summary is counted on **both** sides. It is part of what a complete result would have
    /// cost and part of what this one did, so leaving it out of the original would let a heavily
    /// truncated result claim to have retained more bytes than it started with — which is what the
    /// model reads back as `[truncated by tool: 72 of 18 bytes kept]`.
    pub(super) fn finish(mut self, summary: &str) -> (String, Option<Truncation>) {
        let original = self.original_bytes.saturating_add(summary.len());
        self.body.push_str(summary);
        let retained = self.body.len();
        let truncation = (retained < original)
            .then(|| Truncation::new(TruncationStage::Tool, as_u64(original), as_u64(retained)));
        (self.body, truncation)
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Why a search could not read where it was pointed.
#[derive(Debug)]
pub(super) enum SearchPathError {
    OutsideRoot(String),
    InvalidPath(String),
    NotFound(String),
    Io(std::io::Error),
}

impl SearchPathError {
    fn from_rooted(error: RootedOpenError, path: &Path) -> Self {
        match error {
            RootedOpenError::OutsideRoot => Self::OutsideRoot(path.display().to_string()),
            RootedOpenError::Io(error) => Self::from_io(error, path),
            // A refusal this crate cannot name yet is still a refusal, and it carries the path so
            // the sentence the model reads names something it can act on.
            _ => Self::Io(std::io::Error::other(format!(
                "`{}` was refused by the workspace",
                path.display()
            ))),
        }
    }

    fn from_io(error: std::io::Error, path: &Path) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound(path.display().to_string())
        } else {
            Self::Io(error)
        }
    }
}
