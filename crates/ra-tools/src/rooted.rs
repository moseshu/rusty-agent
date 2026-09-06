//! The rule that turns a model-supplied path into one a workspace-confined tool will open.
//!
//! Three tools open a file the model named — `read_file`, `view_image`, and whatever comes next —
//! and the decision about which paths are openable is a security boundary rather than a
//! convenience. One copy per tool is one drift per tool, and the drift is silent: a tool that
//! accepted a spelling the others refuse would not fail anywhere, it would simply open a file the
//! rule was written to keep closed.
//!
//! # What the rule is
//!
//! Three checks, in this order:
//!
//! 1. An **absolute path inside the root** is accepted and stripped. It is the same request written
//!    another way, and it is the way a model writes it after reading a path out of a search result.
//!    The comparison is lexical against the canonical root, so a host alias for one directory —
//!    `/tmp` for `/private/tmp` — is not recognized. Recognizing it would mean canonicalizing model
//!    input before the open, which is the first half of the race the descriptor-based open exists to
//!    avoid.
//! 2. An absolute path **outside** the root is refused as [`RootedRefusal::OutsideRoot`].
//! 3. A path spelled with **`..`** is refused as [`RootedRefusal::AmbiguousParent`] rather than
//!    normalized, because normalizing changes which file was asked for: POSIX pops a symbolic link's
//!    *target*, so `a/b/../c` and `a/c` name different files whenever `a/b` is a link.
//!
//! The two refusals stay apart because they are not the same news. `src/../README.md` is very often
//! inside the root, and telling the model it is outside sends it looking for a boundary problem it
//! does not have — while the thing it can actually do, spell the path without `..`, goes unsaid.
//!
//! # What this module deliberately does not decide
//!
//! It returns a relative path and nothing else: opening it is
//! [`RootedFileSystem`](ra_exec::fs::RootedFileSystem)'s job, and each tool renders its own refusal
//! sentences, because what a reader should do next depends on what the tool was for.
//!
//! `grep` and `glob` resolve a *directory base for a walk* rather than a file to open, and keep
//! their own copy in [`crate::search`]: their refusal vocabulary is one value where this is two, and
//! they normalize what they accept because a walk has no descriptor to inherit. The rule they apply
//! is the same one, and a change here that they should follow is a change to make in both.

use std::path::{Component, Path, PathBuf};

/// Why a workspace-confined tool will not open the path it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootedRefusal {
    /// The path names something the workspace root does not contain.
    OutsideRoot,
    /// The path is spelled with `..`, which no tool here resolves on the model's behalf.
    AmbiguousParent,
}

/// Resolves `requested` against `root`, or reports why it will not be opened.
///
/// The returned path is relative to the root and free of `..`, which is what makes it safe to walk
/// component by component from a directory capability.
pub(crate) fn relative_in_root(root: &Path, requested: &str) -> Result<PathBuf, RootedRefusal> {
    let requested_path = Path::new(requested);
    let relative = if requested_path.is_absolute() {
        requested_path
            .strip_prefix(root)
            .map_err(|_| RootedRefusal::OutsideRoot)?
            .to_path_buf()
    } else {
        requested_path.to_path_buf()
    };
    for component in relative.components() {
        match component {
            Component::ParentDir => return Err(RootedRefusal::AmbiguousParent),
            // Reachable on Windows, where `C:file` is relative and still carries a prefix.
            Component::RootDir | Component::Prefix(_) => return Err(RootedRefusal::OutsideRoot),
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(relative)
}

/// What a path's extension says the bytes are.
///
/// Extension only, deliberately: sniffing content would let a `.rs` file that happens to start with
/// a magic number come back as an image, and the model named the path it wanted.
pub(crate) enum Media {
    /// An image, carrying the media type a provider is told.
    Image(&'static str),
    /// A PDF, which travels as a file block rather than an image.
    Pdf,
    /// Anything else, which is read as text until it proves otherwise.
    Text,
}

impl Media {
    /// Classifies one path by its extension.
    pub(crate) fn of(path: &Path) -> Self {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("png") => Self::Image("image/png"),
            Some("jpg" | "jpeg") => Self::Image("image/jpeg"),
            Some("gif") => Self::Image("image/gif"),
            Some("webp") => Self::Image("image/webp"),
            Some("bmp") => Self::Image("image/bmp"),
            Some("pdf") => Self::Pdf,
            _ => Self::Text,
        }
    }
}
