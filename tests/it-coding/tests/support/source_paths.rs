// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_paths::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

/// Creates a platform-absolute [`PathBuf`] from a Unix-style absolute test path.
///
/// On Windows, `/tmp/example` maps to `C:\tmp\example`.
pub fn test_path_buf(unix_path: &str) -> PathBuf {
    if cfg!(windows) {
        let mut path = PathBuf::from(r"C:\");
        path.extend(
            unix_path
                .trim_start_matches('/')
                .split('/')
                .filter(|segment| !segment.is_empty()),
        );
        path
    } else {
        PathBuf::from(unix_path)
    }
}

/// Extension methods for converting paths into [`AbsolutePathBuf`] values in tests.
pub trait PathExt {
    /// Converts an already absolute path into an [`AbsolutePathBuf`].
    fn abs(&self) -> AbsolutePathBuf;
}

impl PathExt for Path {
    #[expect(clippy::expect_used)]
    fn abs(&self) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(self).expect("path should already be absolute")
    }
}

/// Extension methods for converting path buffers into [`AbsolutePathBuf`] values in tests.
pub trait PathBufExt {
    /// Converts an already absolute path buffer into an [`AbsolutePathBuf`].
    fn abs(&self) -> AbsolutePathBuf;
}

impl PathBufExt for PathBuf {
    fn abs(&self) -> AbsolutePathBuf {
        self.as_path().abs()
    }
}
