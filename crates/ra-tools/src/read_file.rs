//! One multimodal read entry plus window and budget truncation metadata.
//!
//! # Why this is a tool when `exec_command` can already run `sed`
//!
//! Codex has no read tool: it reads with `cat` and `sed -n '1,50p'`, and that works. What it does
//! not produce is a *fact* — the model is handed 50 lines, and neither it nor the host learns that
//! the file has 4,000 more, that a line was cut, or that the bytes were never text. Claude Code's
//! `Read` returns the same content plus that accounting, which is why R2-8 takes the observation
//! surface from Claude Code and the execution surface from Codex.
//!
//! This is the first real consumer of R2-3's
//! [`ObservationMetadata`](ra_core::tool::ObservationMetadata): the window and the ceiling are
//! recorded as typed truncations for the host, and rendered into a leading sentence for the model
//! only at the provider boundary.
//!
//! # The body is content; everything else is metadata
//!
//! The text block holds file content and nothing else. Where the window sits, what was cut, and
//! what to do next all live in the metadata, so an untruncated read of a small file costs exactly
//! the file and no framing. It also keeps framing out of the bytes a model copies when it edits.
//!
//! The one exception is a read with no content to show — an empty file, or an offset past the end
//! — where a single sentence takes the body's place. A result must carry at least one block
//! (R2-3), and an empty text block is a block a provider may drop.
//!
//! # Line numbers
//!
//! Numbers are prefixed the way Claude Code prefixes them, because `grep` reports line numbers and
//! a model that cannot see them cannot act on them. They are display, not content, and the schema
//! description says so — the failure otherwise is a patch whose context lines carry `    12\t`.
//!
//! # Two ways to answer a read that did not produce a file
//!
//! An empty file, or an offset past the end, is a **successful read with something to say**: the
//! question was well formed and the answer is true. A missing path, a directory, or binary content
//! is a **failure**, and it leaves as an [`Error`] carrying a typed cause in its source. R3-4's
//! dispatcher renders framework errors to the model as a bare code with no prose, deliberately;
//! [`ToolFailureHandling::Custom`] is the one seam where a tool writes its own model-facing
//! sentence, and this tool takes it rather than disguising a failure as an observation.

use std::{
    borrow::Cow,
    fmt,
    path::{Component, Path, PathBuf},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    item::{Base64FileSource, FileBlock, FileSource, ImageBlock, ImageSource},
    tool::{
        ObservationMetadata, Tool, ToolFailureHandling, ToolInput as _, ToolInvocation,
        ToolOptions, ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema, Truncation,
        TruncationStage,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "read_file";

// The doc comment below is the model-facing description, so it says what the model needs and
// stops. R2-11's measurement: the tools that do the work carry very short descriptions (Codex's
// `exec_command` is one sentence), and long behavior contracts belong to the dangerous and the
// ambiguous. A read is neither — except for the line-number sentence, which is there because a
// model that mistakes the prefix for content produces patches that cannot apply.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Reads one file: text comes back with line numbers, images and PDFs come back as content. The
/// line numbers are display only and are not part of the file.
struct ReadFileInput {
    /// File to read. A relative path resolves against the workspace root.
    path: String,
    /// First line to return, 1-based. Text only.
    offset: Option<u32>,
    /// How many lines to return. Text only.
    limit: Option<u32>,
}

/// Ceilings this tool applies to its own output.
///
/// Configuration rather than schema, because the model cannot usefully choose them: it does not
/// know the context budget it is spending against, and a `max_bytes` argument would only add a way
/// to ask for more than the host can afford.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadFileLimits {
    max_output_bytes: usize,
    default_line_limit: u32,
    max_line_bytes: usize,
    max_binary_bytes: usize,
}

impl Default for ReadFileLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadFileLimits {
    /// Creates the defaults.
    ///
    /// 2,000 lines and 2,000 bytes per line are Claude Code's numbers, taken for the reason it
    /// has them: past that point another line stops being worth the tokens it costs on this turn
    /// and on every turn after it. The 64 KiB ceiling is for the file that defeats both — one
    /// minified line, or 100,000 short ones.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_output_bytes: 64 * 1024,
            default_line_limit: 2_000,
            max_line_bytes: 2_000,
            max_binary_bytes: 5 * 1024 * 1024,
        }
    }

    /// Sets the ceiling on the rendered text body.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets how many lines a read returns when the model names no limit.
    #[must_use]
    pub const fn with_default_line_limit(mut self, lines: u32) -> Self {
        self.default_line_limit = lines;
        self
    }

    /// Sets the ceiling on one line.
    #[must_use]
    pub const fn with_max_line_bytes(mut self, bytes: usize) -> Self {
        self.max_line_bytes = bytes;
        self
    }

    /// Sets the ceiling on an image or PDF read.
    #[must_use]
    pub const fn with_max_binary_bytes(mut self, bytes: usize) -> Self {
        self.max_binary_bytes = bytes;
        self
    }

    /// Ceiling on the rendered text body.
    #[must_use]
    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Lines returned when the model names no limit.
    #[must_use]
    pub const fn default_line_limit(&self) -> u32 {
        self.default_line_limit
    }

    /// Ceiling on one line.
    #[must_use]
    pub const fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    /// Ceiling on an image or PDF read.
    #[must_use]
    pub const fn max_binary_bytes(&self) -> usize {
        self.max_binary_bytes
    }
}

/// The `read_file` tool.
#[derive(Debug)]
pub struct ReadFileTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    root: Option<PathBuf>,
    limits: ReadFileLimits,
}

impl ReadFileTool {
    /// Creates a tool that reads anywhere the process can, resolving relative paths against the
    /// working directory.
    pub fn new() -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ReadFileInput::tool_schema(TOOL_NAME)?,
            root: None,
            limits: ReadFileLimits::new(),
        })
    }

    /// Creates a tool confined to `root`, which must exist.
    ///
    /// This is the minimal form of R8-5's workspace boundary and not the whole of it: `..` is
    /// resolved lexically before the check and symlinks are re-checked after canonicalization, but
    /// the real path policy — the shared normalization every file API uses, per-path denies, mount
    /// points — is R8-5's. It is here now because a tool that reads `~/.ssh/id_rsa` on request is
    /// not something to leave for a later milestone.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            Error::config(format!(
                "read_file workspace root `{}` cannot be resolved",
                root.display()
            ))
            .with_source(error)
        })?;
        Ok(Self {
            root: Some(canonical),
            ..Self::new()?
        })
    }

    /// Replaces the output ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: ReadFileLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &ReadFileLimits {
        &self.limits
    }

    /// The workspace root, when this tool is confined to one.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Resolves a model-supplied path, refusing one that leaves the root.
    fn resolve(&self, requested: &str) -> ReadResult<PathBuf> {
        let requested_path = Path::new(requested);
        let joined = match (&self.root, requested_path.is_absolute()) {
            (Some(root), false) => root.join(requested_path),
            _ => requested_path.to_path_buf(),
        };
        let normalized = normalize_lexically(&joined);
        match &self.root {
            Some(root) if !normalized.starts_with(root) => {
                Err(ReadFileFailure::OutsideRoot(requested.to_owned()))
            }
            _ => Ok(normalized),
        }
    }

    /// Resolves, canonicalizes, and re-checks the boundary against the real path.
    async fn locate(&self, requested: &str) -> ReadResult<PathBuf> {
        let normalized = self.resolve(requested)?;
        // Canonicalizing is what turns a symlink that sits inside the root and points outside it
        // into a path the boundary check can see. It answers "does this exist" in the same call.
        let real = tokio::fs::canonicalize(&normalized)
            .await
            .map_err(|error| ReadFileFailure::from_io(requested, &error))?;
        match &self.root {
            Some(root) if !real.starts_with(root) => {
                Err(ReadFileFailure::OutsideRoot(requested.to_owned()))
            }
            _ => Ok(real),
        }
    }

    async fn read(&self, input: &ReadFileInput) -> ReadResult<ToolOutput> {
        let path = self.locate(&input.path).await?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ReadFileFailure::from_io(&input.path, &error))?;
        if !metadata.is_file() {
            return Err(ReadFileFailure::NotAFile(input.path.clone()));
        }

        match Media::of(&path) {
            Media::Image(media_type) => {
                let bytes = self.read_whole(&input.path, &path, metadata.len()).await?;
                Ok(ToolOutput::block(ToolOutputBlock::Image(ImageBlock::new(
                    ImageSource::base64(media_type, BASE64.encode(bytes)),
                ))))
            }
            Media::Pdf => {
                let bytes = self.read_whole(&input.path, &path, metadata.len()).await?;
                let mut source = Base64FileSource::new(BASE64.encode(bytes));
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    source = source.with_filename(name);
                }
                Ok(ToolOutput::block(ToolOutputBlock::File(FileBlock::new(
                    FileSource::Base64(source),
                ))))
            }
            Media::Text => {
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|error| ReadFileFailure::from_io(&input.path, &error))?;
                if is_binary(&bytes) {
                    return Err(ReadFileFailure::Binary(input.path.clone()));
                }
                Ok(self.render_text(input, &bytes))
            }
        }
    }

    /// Reads a whole file that will be sent as one block, refusing one over the ceiling.
    async fn read_whole(&self, requested: &str, path: &Path, size: u64) -> ReadResult<Vec<u8>> {
        let ceiling = as_u64(self.limits.max_binary_bytes);
        if size > ceiling {
            return Err(ReadFileFailure::TooLarge {
                path: requested.to_owned(),
                bytes: size,
                ceiling,
            });
        }
        tokio::fs::read(path)
            .await
            .map_err(|error| ReadFileFailure::from_io(requested, &error))
    }

    /// Renders the selected window and records what the rendering had to leave out.
    fn render_text(&self, input: &ReadFileInput, bytes: &[u8]) -> ToolOutput {
        if bytes.is_empty() {
            return ToolOutput::text("The file is empty (0 bytes).");
        }

        // Lossy rather than refused: a Latin-1 source file is still a source file, and a line
        // saying which bytes were replaced is a better observation than no read at all.
        let text = String::from_utf8_lossy(bytes);
        let lossy = matches!(text, Cow::Owned(_));
        // `lines` folds CRLF and does not invent a trailing empty line for a file that ends in a
        // newline, which is the count every other tool reports for the same file.
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let window = Window::select(
            total,
            input.offset,
            input.limit,
            self.limits.default_line_limit,
        );
        if window.is_empty() {
            // The offset the model sent, not the clamped one it turned into: told "no lines at
            // offset 3" after asking for 9, the model has to work out which of the two numbers is
            // its own.
            return ToolOutput::text(format!(
                "No lines at offset {}; the file has {total} lines.",
                input.offset.unwrap_or(1)
            ))
            .with_metadata(
                ObservationMetadata::new()
                    .with_guidance(format!("Read again with an offset between 1 and {total}.")),
            );
        }

        let selected = &lines[window.start..window.end];
        let rendered = render_lines(selected, window.start + 1, &self.limits);
        let mut metadata = ObservationMetadata::new();

        // A window the *model* chose is not a truncation: it asked for those lines and got them.
        // A window this tool chose because no limit was given is one — the model was never told
        // how much it did not receive, and R5-1 needs the same fact as a number.
        if window.end < total && input.limit.is_none() {
            metadata = metadata.with_truncation(Truncation::new(
                TruncationStage::Tool,
                as_u64(content_bytes(&lines[window.start..])),
                as_u64(rendered.window_bytes),
            ));
        }
        // The ceiling and the long-line cut are one truncation, not two: both are this tool
        // declining to render bytes it had already selected, and both mean the same thing to the
        // model. Keeping them apart would only make the rendered note longer.
        if rendered.hit_ceiling || rendered.long_lines > 0 {
            metadata = metadata.with_truncation(Truncation::new(
                TruncationStage::Tool,
                as_u64(rendered.window_bytes),
                as_u64(rendered.emitted_bytes),
            ));
        }

        let shown_end = window.start + rendered.emitted_lines;
        if shown_end < total {
            metadata = metadata.with_guidance(format!(
                "Showing lines {}-{shown_end} of {total}; continue from offset {}.",
                window.start.saturating_add(1),
                shown_end.saturating_add(1)
            ));
        }
        if rendered.long_lines > 0 {
            metadata = metadata.with_guidance(format!(
                "{} line(s) over {} bytes were cut to fit.",
                rendered.long_lines, self.limits.max_line_bytes
            ));
        }
        if rendered.hit_ceiling {
            metadata = metadata.with_guidance(format!(
                "Output stopped at the {} byte ceiling; narrow the range with offset and limit.",
                self.limits.max_output_bytes
            ));
        }
        if lossy {
            metadata = metadata
                .with_guidance("The file is not valid UTF-8; undecodable bytes were replaced.");
        }

        ToolOutput::text(rendered.body).with_metadata(metadata)
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        // `FuncSchema`'s schema-bound decoder takes the raw argument string a provider sent, while
        // an invocation carries the parsed value. Until R2-6's registry owns that seam, the typed
        // decode happens here; `deny_unknown_fields` keeps it as strict as the schema is.
        let input: ReadFileInput = serde_json::from_value(invocation.arguments().clone())
            .map_err(|error| ReadFileFailure::BadArguments(error.to_string()).into_error())?;
        self.read(&input).await.map_err(ReadFileFailure::into_error)
    }

    fn options(&self) -> ToolOptions {
        // `Custom` is what buys the model a sentence instead of a bare error code. Approval stays
        // at the default: whether a given path needs one is host policy (R7), not this tool's.
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom)
    }

    async fn handle_failure(
        &self,
        _invocation: &ToolInvocation<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Matched on the typed cause, never on the message. The sentence the model reads is
        // written here, once, from a value — not scraped back out of an error's `Display`.
        //
        // **Every failure this tool produces carries one**, including a rejected argument object.
        // Under `Custom`, returning `None` means the failure propagates and the turn stops, so a
        // gap here would turn a mistyped argument — the most correctable mistake a model makes —
        // into a stopped run.
        Ok(ReadFileFailure::of(error).map(|failure| {
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.next_step()))
        }))
    }
}

type ReadResult<T> = std::result::Result<T, ReadFileFailure>;

/// Why a read could not happen at all.
///
/// It travels in an [`Error`]'s source rather than in its message so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum ReadFileFailure {
    /// The argument object does not match the schema.
    BadArguments(String),
    /// Nothing at that path.
    NotFound(String),
    /// A directory, a device, or a socket.
    NotAFile(String),
    /// Outside the configured workspace root.
    OutsideRoot(String),
    /// It exists and could not be opened.
    Unreadable(String, std::io::ErrorKind),
    /// Not text, and not a media type a model can be handed.
    Binary(String),
    /// Larger than the image and PDF ceiling.
    TooLarge {
        path: String,
        bytes: u64,
        ceiling: u64,
    },
}

impl ReadFileFailure {
    fn from_io(path: &str, error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(path.to_owned()),
            kind => Self::Unreadable(path.to_owned(), kind),
        }
    }

    /// Which failure class the host records. Everything the model could have avoided by sending
    /// different arguments is `InvalidInput`; the rest is the tool failing to do its job.
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::NotFound(_)
            | Self::NotAFile(_)
            | Self::OutsideRoot(_)
            | Self::Binary(_) => ToolErrorKind::InvalidInput,
            Self::Unreadable(..) | Self::TooLarge { .. } => ToolErrorKind::ExecutionFailed,
        }
    }

    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// The next step that follows the diagnosis, kept out of [`fmt::Display`] so a log line gets
    /// the fact without an instruction addressed to a model.
    const fn next_step(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => "Send `path`, and `offset` and `limit` only for text.",
            Self::NotFound(_) | Self::OutsideRoot(_) => {
                "Check the path, or search for the file before reading it."
            }
            Self::NotAFile(_) => "List its entries instead of reading it.",
            Self::Unreadable(..) => "Read something else, or ask the user to grant access.",
            Self::Binary(_) => "read_file returns text, images, and PDFs only.",
            Self::TooLarge { .. } => "Read a smaller file.",
        }
    }
}

impl fmt::Display for ReadFileFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field — which is the whole
            // of what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(f, "Invalid arguments: {reason}."),
            Self::NotFound(path) => write!(f, "No such file: `{path}`."),
            Self::NotAFile(path) => write!(f, "`{path}` is not a regular file."),
            Self::OutsideRoot(path) => write!(f, "`{path}` is outside the workspace."),
            Self::Unreadable(path, kind) => write!(f, "`{path}` cannot be read: {kind}."),
            Self::Binary(path) => write!(f, "`{path}` is binary, not text."),
            Self::TooLarge {
                path,
                bytes,
                ceiling,
            } => write!(
                f,
                "`{path}` is {bytes} bytes, over the {ceiling} byte limit."
            ),
        }
    }
}

impl std::error::Error for ReadFileFailure {}

/// What a path's extension says the bytes are.
///
/// Extension only, deliberately: sniffing content would let a `.rs` file that happens to start
/// with a magic number come back as an image, and the model named the path it wanted.
enum Media {
    Image(&'static str),
    Pdf,
    Text,
}

impl Media {
    fn of(path: &Path) -> Self {
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

/// The half-open line range a read returns.
struct Window {
    start: usize,
    end: usize,
}

impl Window {
    fn select(total: usize, offset: Option<u32>, limit: Option<u32>, default_limit: u32) -> Self {
        // A 0 offset and a 0 limit are clamped rather than refused. The schema says 1-based, and
        // spending a turn to say "you sent 0" buys nothing that clamping does not.
        let start = (to_usize(offset.unwrap_or(1)).max(1) - 1).min(total);
        let limit = to_usize(limit.unwrap_or(default_limit)).max(1);
        Self {
            start,
            end: start.saturating_add(limit).min(total),
        }
    }

    const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// What rendering one window produced, and what it left out.
struct RenderedLines {
    body: String,
    /// Content bytes in the whole window, before any cut.
    window_bytes: usize,
    /// Content bytes actually emitted. Line-number prefixes are excluded from both, so the pair
    /// measures the file rather than the framing this tool added to it.
    emitted_bytes: usize,
    emitted_lines: usize,
    long_lines: usize,
    hit_ceiling: bool,
}

fn render_lines(lines: &[&str], first_number: usize, limits: &ReadFileLimits) -> RenderedLines {
    let mut rendered = RenderedLines {
        body: String::new(),
        window_bytes: 0,
        emitted_bytes: 0,
        emitted_lines: 0,
        long_lines: 0,
        hit_ceiling: false,
    };

    for (index, line) in lines.iter().enumerate() {
        // Counted even after the ceiling stops emission: the truncation records how much was
        // there, and stopping the count reports a smaller loss than the one that happened.
        rendered.window_bytes += line.len() + 1;
        if rendered.hit_ceiling {
            continue;
        }
        let (shown, cut) = cut_to(line, limits.max_line_bytes);
        let entry = format!("{:>6}\t{shown}\n", first_number + index);
        // The first line is emitted whatever it costs: a result whose body is empty says less
        // than one over budget, and the truncation reports the overrun either way.
        if !rendered.body.is_empty() && rendered.body.len() + entry.len() > limits.max_output_bytes
        {
            rendered.hit_ceiling = true;
            continue;
        }
        rendered.body.push_str(&entry);
        rendered.emitted_bytes += shown.len() + 1;
        rendered.emitted_lines += 1;
        if cut {
            rendered.long_lines += 1;
        }
    }

    rendered
}

/// Cuts to at most `max` bytes, on a character boundary.
fn cut_to(line: &str, max: usize) -> (&str, bool) {
    if line.len() <= max {
        return (line, false);
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    (&line[..end], true)
}

fn content_bytes(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.len() + 1).sum()
}

/// A NUL byte in the first block is the test `git` uses to call a file binary.
fn is_binary(bytes: &[u8]) -> bool {
    const SNIFF: usize = 8 * 1024;
    bytes[..bytes.len().min(SNIFF)].contains(&0)
}

/// Resolves `.` and `..` without touching the filesystem.
///
/// Done before the boundary check so `../../etc/passwd` is refused rather than followed, and
/// before canonicalization so a path that does not exist is still refused for the right reason.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(Component::ParentDir);
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
