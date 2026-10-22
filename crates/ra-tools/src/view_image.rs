//! Putting one workspace image in front of the model.
//!
//! # Why this exists beside `read_file`, which already returns images
//!
//! Two reasons, and neither is that the mechanism differs — the bytes travel as the same
//! [`ImageBlock`] either way.
//!
//! The first is that the entries mean different things to a model choosing between them.
//! `read_file` is where you go when you want a file's contents and the format is incidental; this is
//! where you go when the point *is* to look at something — a screenshot of the failure, a diagram,
//! a rendering that has to be compared against what it was supposed to be. Codex advertises both for
//! that reason, and it is the cheapest way to make "show me" expressible at all.
//!
//! The second is what each does with invalid image data. This tool decodes the bytes as Codex's
//! `core/src/tools/handlers/view_image.rs` does before returning an image block. Invalid data becomes
//! the same model-visible error rather than a malformed image in the next provider request.
//!
//! # What it does not do
//!
//! It does not fetch, resize, crop, compare, or convert. Those are image *pipeline* operations,
//! deferred to the tool search surface, and putting any of them here would turn a 500-byte schema
//! into a media library.

use std::{fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    item::{ImageBlock, ImageSource},
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, ResourceClaim, Tool,
        ToolArgumentDecodeError, ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions,
        ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema,
    },
};
use ra_exec::fs::{RootedFileSystem, RootedOpenError, Workspace};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;

use crate::rooted::{RootedRefusal, relative_in_root};

const INVALID_IMAGE: &str = "unable to process image: invalid or unsupported image data";

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "view_image";

/// Allocation hint for one read, kept well under the ceiling so a large ceiling costs nothing up
/// front.
const INITIAL_READ_BUFFER_CAPACITY: usize = 64 * 1024;

// The model-facing description is copied verbatim from Codex. Keep it on one doc-comment line:
// multiple lines are joined with newline characters and would change the upstream text.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk.
struct ViewImageInput {
    /// Image to look at. A relative path resolves against the workspace root.
    path: String,
}

/// The ceiling this tool applies to one image.
///
/// Configuration rather than schema, for the reason `read_file` keeps its own out: the model cannot
/// usefully choose it, because it does not know the context budget it is spending against, and a
/// `max_bytes` argument would only add a way to ask for more than the host can afford.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewImageLimits {
    max_bytes: usize,
}

impl Default for ViewImageLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewImageLimits {
    /// Creates the default ceiling.
    ///
    /// The same 5 MiB `read_file` applies to an image, and deliberately the same number: two
    /// entries that hand a provider the same block for the same file should not disagree about
    /// which files are too large, or the way to get around one is to call the other.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_bytes: 5 * 1024 * 1024,
        }
    }

    /// Sets the ceiling on one image.
    #[must_use]
    pub const fn with_max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = if bytes == 0 { 1 } else { bytes };
        self
    }

    /// Ceiling on one image.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

/// The `view_image` tool.
pub struct ViewImageTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    root: PathBuf,
    filesystem: Arc<RootedFileSystem>,
    limits: ViewImageLimits,
}

impl fmt::Debug for ViewImageTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewImageTool")
            .field("origin", &self.origin)
            .field("root", &self.root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ViewImageTool {
    /// Creates the entry confined to one workspace.
    ///
    /// There is no unrooted constructor. `read_file` has one because a host may legitimately want a
    /// reader that resolves against the process's working directory; an image handed to a model is
    /// worth less than the boundary, and every host that installs this one installs it through a
    /// capability that already holds a workspace.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ViewImageInput>(TOOL_NAME)?,
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_action_classes([crate::action_classes::READ])
                .with_concurrency(ToolConcurrency::Parallel)
                .with_resource_claim(ResourceClaim::shared(workspace.resource_id().clone())),
            root: workspace.root().to_path_buf(),
            filesystem: Arc::clone(workspace.filesystem()),
            limits: ViewImageLimits::new(),
        })
    }

    /// Replaces the size ceiling.
    #[must_use]
    pub const fn with_limits(mut self, limits: ViewImageLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceiling in force.
    #[must_use]
    pub const fn limits(&self) -> &ViewImageLimits {
        &self.limits
    }

    /// The workspace root this tool is confined to.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    async fn view(&self, input: &ViewImageInput) -> ViewResult<ToolOutput> {
        let relative =
            relative_in_root(&self.root, &input.path).map_err(|refusal| match refusal {
                RootedRefusal::OutsideRoot => ViewImageFailure::OutsideRoot(input.path.clone()),
                RootedRefusal::AmbiguousParent => {
                    ViewImageFailure::AmbiguousParent(input.path.clone())
                }
            })?;

        let mut file = self.open(&input.path, relative).await?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| ViewImageFailure::from_io(&input.path, &error))?;
        if !metadata.is_file() {
            return Err(ViewImageFailure::NotAFile(input.path.clone()));
        }
        let bytes = self
            .read_whole(&input.path, &mut file, metadata.len())
            .await?;
        // Decode before attaching, retaining the original bytes rather than re-encoding them.
        // Unlike Codex's data URL wrapper, ImageBlock also needs the actual media type.
        let (bytes, media_type) = tokio::task::spawn_blocking(move || {
            image::load_from_memory(&bytes).map_err(|_| ViewImageFailure::InvalidImage)?;
            let format = image::guess_format(&bytes).map_err(|_| ViewImageFailure::InvalidImage)?;
            Ok::<_, ViewImageFailure>((bytes, format.to_mime_type()))
        })
        .await
        .map_err(|_| ViewImageFailure::InvalidImage)??;

        Ok(ToolOutput::block(ToolOutputBlock::Image(ImageBlock::new(
            ImageSource::base64(media_type, BASE64.encode(bytes)),
        )))
        .with_metadata(
            ObservationMetadata::new().with_guidance(format!("`{}` is attached.", input.path)),
        ))
    }

    /// Opens through the workspace descriptor, which is what keeps a symbolic link from leaving the
    /// root between the check above and this open.
    async fn open(&self, requested: &str, relative: PathBuf) -> ViewResult<tokio::fs::File> {
        let filesystem = Arc::clone(&self.filesystem);
        let opened = tokio::task::spawn_blocking(move || filesystem.open_read(&relative))
            .await
            .map_err(|_| {
                ViewImageFailure::Unreadable(requested.to_owned(), std::io::ErrorKind::Other)
            })?;
        match opened {
            Ok(file) => Ok(tokio::fs::File::from_std(file)),
            Err(RootedOpenError::OutsideRoot) => {
                Err(ViewImageFailure::OutsideRoot(requested.to_owned()))
            }
            Err(RootedOpenError::Io(error)) => Err(ViewImageFailure::from_io(requested, &error)),
            // A refusal this crate cannot name yet is still a refusal. `RootedOpenError` is
            // `#[non_exhaustive]`, so the wildcard is mandatory; what matters is that it lands on a
            // failure rather than on anything that could read as an image.
            Err(_) => Err(ViewImageFailure::Unreadable(
                requested.to_owned(),
                std::io::ErrorKind::Other,
            )),
        }
    }

    /// Reads the whole image, refusing one over the ceiling.
    ///
    /// The size from `metadata` is only a preflight — the file can grow after it was queried — so
    /// the read itself takes one byte beyond the ceiling and checks what it got. That is the same
    /// arrangement `read_file` uses, and it is what makes the ceiling a property of the bytes
    /// delivered rather than of a number that was true a moment earlier.
    async fn read_whole(
        &self,
        requested: &str,
        file: &mut tokio::fs::File,
        size: u64,
    ) -> ViewResult<Vec<u8>> {
        let ceiling = u64::try_from(self.limits.max_bytes).unwrap_or(u64::MAX);
        if size > ceiling {
            return Err(ViewImageFailure::TooLarge {
                path: requested.to_owned(),
                bytes: size,
                ceiling,
            });
        }
        let capacity = usize::try_from(size.min(ceiling))
            .unwrap_or(INITIAL_READ_BUFFER_CAPACITY)
            .min(INITIAL_READ_BUFFER_CAPACITY)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(capacity);
        file.take(ceiling.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| ViewImageFailure::from_io(requested, &error))?;
        if bytes.len() > self.limits.max_bytes {
            return Err(ViewImageFailure::TooLarge {
                path: requested.to_owned(),
                bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                ceiling,
            });
        }
        Ok(bytes)
    }
}

type ViewResult<T> = std::result::Result<T, ViewImageFailure>;

/// Why an image could not be attached.
///
/// It travels in an [`Error`]'s source rather than in its message, so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum ViewImageFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
    /// Nothing at that path.
    NotFound(String),
    /// A directory, a device, or a socket.
    NotAFile(String),
    /// Outside the workspace root.
    OutsideRoot(String),
    /// Spelled with `..`, which this tool refuses to resolve on the model's behalf.
    AmbiguousParent(String),
    /// It exists and could not be opened.
    Unreadable(String, std::io::ErrorKind),
    /// Codex's model-correctable invalid or unsupported image data error.
    InvalidImage,
    /// Larger than the ceiling.
    TooLarge {
        path: String,
        bytes: u64,
        ceiling: u64,
    },
}

impl ViewImageFailure {
    fn from_io(path: &str, error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(path.to_owned()),
            kind => Self::Unreadable(path.to_owned(), kind),
        }
    }

    /// Which failure class the host records. Everything the model could have avoided by sending a
    /// different path is `InvalidInput`; the rest is the tool failing to do its job.
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::NotFound(_)
            | Self::NotAFile(_)
            | Self::OutsideRoot(_)
            | Self::AmbiguousParent(_)
            | Self::InvalidImage => ToolErrorKind::InvalidInput,
            Self::Unreadable(..) | Self::TooLarge { .. } => ToolErrorKind::ExecutionFailed,
        }
    }

    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// An optional next step, kept out of [`fmt::Display`] so a log line gets the fact without an
    /// instruction addressed to a model. Invalid images retain Codex's plain error response.
    const fn next_step(&self) -> Option<&'static str> {
        Some(match self {
            Self::BadArguments(_) => "Send `path` and nothing else.",
            Self::NotFound(_) | Self::OutsideRoot(_) => {
                "Check the path, or search for the file before opening it."
            }
            Self::AmbiguousParent(_) => "Send the path without `..`.",
            Self::NotAFile(_) => "Name a file rather than a directory.",
            Self::Unreadable(..) => "Look at something else, or ask the user to grant access.",
            Self::InvalidImage => return None,
            Self::TooLarge { .. } => "Look at a smaller image.",
        })
    }
}

impl fmt::Display for ViewImageFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field, which is the whole of
            // what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
            Self::NotFound(path) => write!(formatter, "No such file: `{path}`."),
            Self::NotAFile(path) => write!(formatter, "`{path}` is not a regular file."),
            Self::OutsideRoot(path) => write!(formatter, "`{path}` is outside the workspace."),
            Self::AmbiguousParent(path) => {
                write!(
                    formatter,
                    "`{path}` uses `..`, which this tool does not resolve."
                )
            }
            Self::Unreadable(path, kind) => write!(formatter, "`{path}` cannot be read: {kind}."),
            Self::InvalidImage => formatter.write_str(INVALID_IMAGE),
            Self::TooLarge {
                path,
                bytes,
                ceiling,
            } => write!(
                formatter,
                "`{path}` is {bytes} bytes, over the {ceiling} byte limit."
            ),
        }
    }
}

impl std::error::Error for ViewImageFailure {}

#[async_trait]
impl Tool for ViewImageTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| ViewImageFailure::BadArguments(error).into_error())
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = match context.take_decoded_input::<ViewImageInput>()? {
            Some(input) => input,
            None => serde_json::from_value(context.arguments().clone()).map_err(|error| {
                ViewImageFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                    input_type: self.func_schema.input_type_name(),
                    message: error.to_string(),
                })
                .into_error()
            })?,
        };
        self.view(&input)
            .await
            .map_err(ViewImageFailure::into_error)
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Control signals must reach the runner rather than becoming an observation: they stop or
        // close out work instead of describing an attachment that did not happen.
        if error.is_control_signal() {
            return Ok(None);
        }
        Ok(ViewImageFailure::of(error).map(|failure| {
            let output = ToolOutput::text(failure.to_string());
            match failure.next_step() {
                Some(guidance) => {
                    output.with_metadata(ObservationMetadata::new().with_guidance(guidance))
                }
                None => output,
            }
        }))
    }
}
