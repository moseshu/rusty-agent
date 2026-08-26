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
    sync::Arc,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    item::{Base64FileSource, FileBlock, FileSource, ImageBlock, ImageSource},
    permission::PermissionScope,
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, ResourceClaim, Tool,
        ToolArgumentDecodeError, ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions,
        ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema, Truncation, TruncationStage,
    },
};
use ra_exec::fs::{RootedFileSystem, RootedOpenError};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "read_file";
const INITIAL_READ_BUFFER_CAPACITY: usize = 8 * 1024;

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
pub struct ReadFileTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    root: Option<PathBuf>,
    rooted_filesystem: Option<Arc<RootedFileSystem>>,
    limits: ReadFileLimits,
}

impl fmt::Debug for ReadFileTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadFileTool")
            .field("origin", &self.origin)
            .field("root", &self.root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ReadFileTool {
    /// Creates a tool that reads anywhere the process can, resolving relative paths against the
    /// working directory.
    pub fn new() -> Result<Self> {
        let options = ToolOptions::new()
            .with_failure_handling(ToolFailureHandling::Custom)
            .with_permission_scope(PermissionScope::Read)
            .with_concurrency(ToolConcurrency::Parallel);
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ReadFileInput>(TOOL_NAME)?,
            options,
            root: None,
            rooted_filesystem: None,
            limits: ReadFileLimits::new(),
        })
    }

    /// Creates a tool confined to `root`, which must exist.
    ///
    /// This is the minimal form of R8-5's workspace boundary. It converts the root into a stable
    /// directory capability and resolves every later component from that handle, following a
    /// symbolic link only as far as the link stays below the root. The broader shared path policy
    /// — per-path denies and mount policy — remains R8-5, but a check-then-open escape is not left
    /// for that milestone.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            Error::config(format!(
                "read_file workspace root `{}` cannot be resolved",
                root.display()
            ))
            .with_source(error)
        })?;
        let rooted_filesystem = RootedFileSystem::open(&canonical).map_err(|error| {
            Error::config(format!(
                "read_file workspace root `{}` cannot be opened",
                root.display()
            ))
            .with_source(error)
        })?;
        let resource_id = ra_exec::fs::workspace_resource_id(
            canonical.to_string_lossy().to_string(),
        )
        .map_err(|error| {
            Error::config(format!(
                "read_file workspace root `{}` produces invalid resource identity",
                canonical.display()
            ))
            .with_source(error)
        })?;
        let options = ToolOptions::new()
            .with_failure_handling(ToolFailureHandling::Custom)
            .with_permission_scope(PermissionScope::Read)
            .with_concurrency(ToolConcurrency::Parallel)
            .with_resource_claim(ResourceClaim::shared(resource_id));
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ReadFileInput>(TOOL_NAME)?,
            options,
            root: Some(canonical),
            rooted_filesystem: Some(Arc::new(rooted_filesystem)),
            limits: ReadFileLimits::new(),
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

    /// Resolves a model-supplied path into one this tool will open, or into the reason it will
    /// not.
    ///
    /// Two different reasons, deliberately: a path that **left the workspace** and a path spelled
    /// with **`..`** are refused by different rules and produce different sentences. Only the
    /// first is a boundary violation; the second is a spelling this tool declines to interpret,
    /// and it is often a file that is perfectly well inside the root.
    ///
    /// **An absolute path inside the workspace is accepted.** It is the same request written
    /// another way, and it is the way a model writes it after reading one out of a search result;
    /// refusing it would buy nothing, because what survives the strip is resolved through the
    /// capability exactly like any other relative path. The comparison is lexical against the
    /// canonical root, so a host alias for the same directory — `/tmp` for `/private/tmp` — is not
    /// recognized. Recognizing it would mean canonicalizing model input before the open, which is
    /// the first half of the race this tool is built to avoid.
    fn resolve(&self, requested: &str) -> ReadResult<ResolvedPath> {
        let requested_path = Path::new(requested);
        let Some(root) = &self.root else {
            // Let the OS resolve ambient paths exactly as it normally would.  In particular,
            // normalizing `link/../file` before opening changes POSIX's symbolic-link semantics.
            return Ok(ResolvedPath::Ambient(requested_path.to_path_buf()));
        };
        let relative = if requested_path.is_absolute() {
            match requested_path.strip_prefix(root) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => return Err(ReadFileFailure::OutsideRoot(requested.to_owned())),
            }
        } else {
            requested_path.to_path_buf()
        };
        for component in relative.components() {
            match component {
                // `..` is refused rather than normalized away, because normalizing it changes
                // which file was asked for: POSIX pops a symbolic link's *target*, so `a/b/../c`
                // and `a/c` name different files whenever `a/b` is a link.
                //
                // It gets its own refusal instead of sharing one with a path that left the
                // workspace, because the two are not the same news. `src/../README.md` may well
                // be inside the root, and telling the model it is outside sends it looking for a
                // boundary problem it does not have — while the thing it can actually do, spell
                // the path without `..`, goes unsaid.
                Component::ParentDir => {
                    return Err(ReadFileFailure::AmbiguousParent(requested.to_owned()));
                }
                // Reachable on Windows, where `C:file` is relative and still carries a prefix.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ReadFileFailure::OutsideRoot(requested.to_owned()));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }
        Ok(ResolvedPath::Rooted(relative))
    }

    async fn read(&self, input: &ReadFileInput) -> ReadResult<ToolOutput> {
        let path = self.resolve(&input.path)?;
        let mut file = self.open_file(&input.path, &path).await?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| ReadFileFailure::from_io(&input.path, &error))?;
        if !metadata.is_file() {
            return Err(ReadFileFailure::NotAFile(input.path.clone()));
        }

        match Media::of(path.as_path()) {
            Media::Image(media_type) => {
                let bytes = self
                    .read_whole(&input.path, &mut file, metadata.len())
                    .await?;
                Ok(ToolOutput::block(ToolOutputBlock::Image(ImageBlock::new(
                    ImageSource::base64(media_type, BASE64.encode(bytes)),
                ))))
            }
            Media::Pdf => {
                let bytes = self
                    .read_whole(&input.path, &mut file, metadata.len())
                    .await?;
                let mut source = Base64FileSource::new(BASE64.encode(bytes));
                if let Some(name) = path.as_path().file_name().and_then(|name| name.to_str()) {
                    source = source.with_filename(name);
                }
                Ok(ToolOutput::block(ToolOutputBlock::File(FileBlock::new(
                    FileSource::Base64(source),
                ))))
            }
            Media::Text => self.render_text_stream(input, &mut file).await,
        }
    }

    /// Opens once and returns the descriptor every later operation must use.
    async fn open_file(&self, requested: &str, path: &ResolvedPath) -> ReadResult<tokio::fs::File> {
        match path {
            ResolvedPath::Ambient(path) => tokio::fs::File::open(path)
                .await
                .map_err(|error| ReadFileFailure::from_io(requested, &error)),
            ResolvedPath::Rooted(path) => {
                let filesystem = Arc::clone(self.rooted_filesystem.as_ref().ok_or_else(|| {
                    ReadFileFailure::Unreadable(requested.to_owned(), std::io::ErrorKind::Other)
                })?);
                let path = path.clone();
                let result = tokio::task::spawn_blocking(move || filesystem.open_read(&path))
                    .await
                    .map_err(|_| {
                        ReadFileFailure::Unreadable(requested.to_owned(), std::io::ErrorKind::Other)
                    })?;
                match result {
                    Ok(file) => Ok(tokio::fs::File::from_std(file)),
                    Err(RootedOpenError::OutsideRoot) => {
                        Err(ReadFileFailure::OutsideRoot(requested.to_owned()))
                    }
                    Err(RootedOpenError::Io(error)) => {
                        Err(ReadFileFailure::from_io(requested, &error))
                    }
                    // A refusal this crate cannot name yet is still a refusal. `RootedOpenError`
                    // is `#[non_exhaustive]`, so the wildcard is mandatory; what matters is that
                    // it lands on a failure rather than on anything that could read as a file.
                    Err(_) => Err(ReadFileFailure::Unreadable(
                        requested.to_owned(),
                        std::io::ErrorKind::Other,
                    )),
                }
            }
        }
    }

    /// Reads a whole file that will be sent as one block, refusing one over the ceiling.
    async fn read_whole(
        &self,
        requested: &str,
        file: &mut tokio::fs::File,
        size: u64,
    ) -> ReadResult<Vec<u8>> {
        let ceiling = as_u64(self.limits.max_binary_bytes);
        if size > ceiling {
            return Err(ReadFileFailure::TooLarge {
                path: requested.to_owned(),
                bytes: size,
                ceiling,
            });
        }
        // Metadata is only a preflight: the file can grow after it was queried.  Reading through
        // this same descriptor and taking one byte beyond the cap preserves the descriptor-level
        // TOCTOU guarantee while keeping the allocation bounded.
        // This is only an allocation hint.  A public configuration may deliberately have a large
        // ceiling, but opening a tiny media file must not reserve that entire ceiling up front.
        let capacity = usize::try_from(size.min(ceiling))
            .unwrap_or(INITIAL_READ_BUFFER_CAPACITY)
            .min(INITIAL_READ_BUFFER_CAPACITY)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(capacity);
        let mut reader = file.take(ceiling.saturating_add(1));
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| ReadFileFailure::from_io(requested, &error))?;
        if bytes.len() > self.limits.max_binary_bytes {
            return Err(ReadFileFailure::TooLarge {
                path: requested.to_owned(),
                bytes: as_u64(bytes.len()),
                ceiling,
            });
        }
        Ok(bytes)
    }

    /// Streams text in bounded chunks, retaining only the current line's display prefix.
    async fn render_text_stream(
        &self,
        input: &ReadFileInput,
        file: &mut tokio::fs::File,
    ) -> ReadResult<ToolOutput> {
        const CHUNK_BYTES: usize = 8 * 1024;
        let requested_start = to_usize(input.offset.unwrap_or(1)).max(1) - 1;
        let requested_limit =
            to_usize(input.limit.unwrap_or(self.limits.default_line_limit)).max(1);
        let requested_end = requested_start.saturating_add(requested_limit);
        // Do not reserve the configured maximum before a byte has been read.  The builder still
        // retains at most `max_line_bytes + 4` bytes, but grows only as the source actually proves
        // it needs them; a hostile or mistaken `usize::MAX` configuration cannot OOM on open.
        let prefix_capacity = self.limits.max_line_bytes.saturating_add(4);
        let mut renderer = StreamRenderer::new(&self.limits);
        let mut line = SourceLineBuilder::new(requested_start == 0, prefix_capacity);
        let mut buffer = [0_u8; CHUNK_BYTES];
        let mut total = 0_usize;
        let mut bytes_seen = 0_usize;
        let mut bytes_from_window_start = 0_usize;
        let mut utf8 = Utf8LossyDetector::new();

        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| ReadFileFailure::from_io(&input.path, &error))?;
            if read == 0 {
                break;
            }
            for byte in &buffer[..read] {
                utf8.push(*byte);
                if bytes_seen < 8 * 1024 && *byte == 0 {
                    return Err(ReadFileFailure::Binary(input.path.clone()));
                }
                bytes_seen = bytes_seen.saturating_add(1);
                if *byte == b'\n' {
                    let source = line.finish(true);
                    observe_line(
                        &source,
                        total,
                        requested_start,
                        requested_end,
                        &mut bytes_from_window_start,
                        &mut renderer,
                    );
                    total = total.saturating_add(1);
                    line = SourceLineBuilder::new(
                        total >= requested_start && total < requested_end,
                        prefix_capacity,
                    );
                } else {
                    line.push(*byte);
                }
            }
        }

        if bytes_seen == 0 {
            return Ok(ToolOutput::text("The file is empty (0 bytes)."));
        }
        if line.has_content() {
            let source = line.finish(false);
            observe_line(
                &source,
                total,
                requested_start,
                requested_end,
                &mut bytes_from_window_start,
                &mut renderer,
            );
            total = total.saturating_add(1);
        }

        let window = Window {
            start: requested_start.min(total),
            end: requested_end.min(total),
        };
        if window.is_empty() {
            // The offset the model sent, not the clamped one it turned into: told "no lines at
            // offset 3" after asking for 9, the model has to work out which of the two numbers is
            // its own.
            return Ok(ToolOutput::text(format!(
                "No lines at offset {}; the file has {total} lines.",
                input.offset.unwrap_or(1)
            ))
            .with_metadata(
                ObservationMetadata::new()
                    .with_guidance(format!("Read again with an offset between 1 and {total}.")),
            ));
        }

        let (rendering, rendered_lossy) = renderer.finish();
        Ok(self.finish_text_output(
            input,
            total,
            &window,
            bytes_from_window_start,
            rendering,
            rendered_lossy || utf8.finish(),
        ))
    }

    /// Attaches raw-source truncation facts after the streaming pass knows the file's total size.
    fn finish_text_output(
        &self,
        input: &ReadFileInput,
        total: usize,
        window: &Window,
        bytes_from_window_start: usize,
        rendering: RenderedLines,
        lossy: bool,
    ) -> ToolOutput {
        let mut metadata = ObservationMetadata::new();

        // A window the *model* chose is not a truncation: it asked for those lines and got them.
        // A window this tool chose because no limit was given is one — the model was never told
        // how much it did not receive, and R5-1 needs the same fact as a number.
        if window.end < total && input.limit.is_none() {
            metadata = metadata.with_truncation(Truncation::new(
                TruncationStage::Tool,
                as_u64(bytes_from_window_start),
                as_u64(rendering.window_bytes),
            ));
        }
        // The ceiling and the long-line cut are one truncation, not two: both are this tool
        // declining to render bytes it had already selected, and both mean the same thing to the
        // model. Keeping them apart would only make the rendered note longer.
        if rendering.hit_ceiling || rendering.long_lines > 0 {
            metadata = metadata.with_truncation(Truncation::new(
                TruncationStage::Tool,
                as_u64(rendering.window_bytes),
                as_u64(rendering.emitted_bytes),
            ));
        }

        let shown_end = window.start + rendering.emitted_lines;
        if shown_end < total {
            metadata = metadata.with_guidance(format!(
                "Showing lines {}-{shown_end} of {total}; continue from offset {}.",
                window.start.saturating_add(1),
                shown_end.saturating_add(1)
            ));
        }
        if rendering.long_lines > 0 {
            metadata = metadata.with_guidance(format!(
                "{} line(s) over {} bytes were cut to fit.",
                rendering.long_lines, self.limits.max_line_bytes
            ));
        }
        if rendering.hit_ceiling {
            metadata = metadata.with_guidance(format!(
                "Output stopped at the {} byte ceiling; narrow the range with offset and limit.",
                self.limits.max_output_bytes
            ));
        }
        if lossy {
            metadata = metadata
                .with_guidance("The file is not valid UTF-8; undecodable bytes were replaced.");
        }

        ToolOutput::text(rendering.body).with_metadata(metadata)
    }
}

#[async_trait]
impl Tool for ReadFileTool {
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
            .map_err(|error| ReadFileFailure::BadArguments(error).into_error())
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        // Runtime dispatch puts the schema-bound value here. The fallback keeps direct, isolated
        // tool tests possible; provider calls never take it because the common entry validates
        // and decodes before invoking this method.
        let input = context.take_decoded_input::<ReadFileInput>()?.map_or_else(
            || {
                serde_json::from_value(context.arguments().clone()).map_err(|error| {
                    ReadFileFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                        input_type: self.func_schema.input_type_name(),
                        message: error.to_string(),
                    })
                    .into_error()
                })
            },
            Ok,
        )?;
        self.read(&input).await.map_err(ReadFileFailure::into_error)
    }

    fn options(&self) -> ToolOptions {
        // `Custom` is what buys the model a sentence instead of a bare error code. Approval stays
        // at the default: whether a given path needs one is host policy (R7), not this tool's.
        //
        // `Parallel` is the whole point of the batch shape (R3-4b): three reads of three files
        // are one wall-clock read, and a read cannot observe another call's writes because it
        // holds no lock on anything. The measured slice this imitates is Codex issuing three
        // `sed` calls off one reasoning block.
        //
        // The repeat breaker stays off, and that is a decision rather than an omission. It compares
        // arguments, and for a read of mutable state identical arguments do not mean identical
        // evidence: re-reading a file after editing it is how the edit gets verified, and it is
        // the answer that changed, not the question. The streak also survives the calls in
        // between, so `read -> edit -> read -> edit -> read` reaches three without anything having
        // gone wrong. A different request resets the streak, but requiring the model to change the
        // request merely to verify a write is still the wrong policy. A breaker that can tell a
        // repeat carrying new evidence from one carrying none is what this tool needs before it
        // opts in.
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
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

/// A normalized path whose access policy is already selected.
enum ResolvedPath {
    Ambient(PathBuf),
    Rooted(PathBuf),
}

impl ResolvedPath {
    fn as_path(&self) -> &Path {
        match self {
            Self::Ambient(path) | Self::Rooted(path) => path,
        }
    }
}

/// Why a read could not happen at all.
///
/// It travels in an [`Error`]'s source rather than in its message so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum ReadFileFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
    /// Nothing at that path.
    NotFound(String),
    /// A directory, a device, or a socket.
    NotAFile(String),
    /// Outside the configured workspace root.
    OutsideRoot(String),
    /// Spelled with `..`, which this tool refuses to resolve on the model's behalf.
    AmbiguousParent(String),
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
            | Self::AmbiguousParent(_)
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
            Self::AmbiguousParent(_) => "Send the path without `..`.",
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
            Self::AmbiguousParent(path) => {
                write!(f, "`{path}` uses `..`, which this tool does not resolve.")
            }
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

/// The tiny, bounded state retained for the line currently crossing a stream chunk boundary.
struct SourceLineBuilder {
    capture: bool,
    prefix_capacity: usize,
    prefix: Vec<u8>,
    content_bytes: usize,
    last_byte: Option<u8>,
}

impl SourceLineBuilder {
    fn new(capture: bool, prefix_capacity: usize) -> Self {
        Self {
            capture,
            prefix_capacity,
            prefix: Vec::with_capacity(prefix_capacity.min(8 * 1024)),
            content_bytes: 0,
            last_byte: None,
        }
    }

    fn push(&mut self, byte: u8) {
        self.content_bytes = self.content_bytes.saturating_add(1);
        self.last_byte = Some(byte);
        if self.capture && self.prefix.len() < self.prefix_capacity {
            self.prefix.push(byte);
        }
    }

    const fn has_content(&self) -> bool {
        self.content_bytes > 0
    }

    fn finish(self, has_newline: bool) -> SourceLine {
        let crlf = has_newline && self.last_byte == Some(b'\r');
        SourceLine {
            raw_bytes: self.content_bytes.saturating_add(usize::from(has_newline)),
            content_bytes: self.content_bytes.saturating_sub(usize::from(crlf)),
            prefix: self.prefix,
        }
    }
}

/// One physical source line, with byte counts kept separately from the lossy display string.
struct SourceLine {
    /// Original source bytes for this line, including its actual LF/CRLF delimiter when present.
    raw_bytes: usize,
    /// Source bytes available as visible content after stripping the CR in a CRLF delimiter.
    content_bytes: usize,
    /// At most `max_line_bytes + 4` initial content bytes, enough to cut at a UTF-8 boundary.
    prefix: Vec<u8>,
}

/// Constant-space UTF-8 validator used for metadata about bytes outside the selected window.
///
/// Rendering uses `from_utf8_lossy` for the shown prefix; this detector preserves the previous
/// whole-file guidance without ever materializing the whole file.  `pending` holds only an
/// incomplete scalar (at most three bytes) between stream chunks.
struct Utf8LossyDetector {
    pending: Vec<u8>,
    lossy: bool,
}

impl Utf8LossyDetector {
    const fn new() -> Self {
        Self {
            pending: Vec::new(),
            lossy: false,
        }
    }

    fn push(&mut self, byte: u8) {
        self.pending.push(byte);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(_) => {
                    self.pending.clear();
                    return;
                }
                Err(error) => match error.error_len() {
                    Some(invalid_bytes) => {
                        self.lossy = true;
                        let consumed = error.valid_up_to().saturating_add(invalid_bytes);
                        self.pending.drain(..consumed);
                        if self.pending.is_empty() {
                            return;
                        }
                    }
                    None => return,
                },
            }
        }
    }

    fn finish(self) -> bool {
        self.lossy || !self.pending.is_empty()
    }
}

/// Incremental renderer for a selected stream window.
struct StreamRenderer<'a> {
    limits: &'a ReadFileLimits,
    rendered: RenderedLines,
    lossy: bool,
}

impl<'a> StreamRenderer<'a> {
    fn new(limits: &'a ReadFileLimits) -> Self {
        Self {
            limits,
            rendered: RenderedLines {
                body: String::new(),
                window_bytes: 0,
                emitted_bytes: 0,
                emitted_lines: 0,
                long_lines: 0,
                hit_ceiling: false,
            },
            lossy: false,
        }
    }

    fn push(&mut self, line: &SourceLine, number: usize) {
        // Counted even after the ceiling stops emission: a truncation has to describe all source
        // bytes selected by the window, not only the prefix that happened to fit.
        self.rendered.window_bytes = self.rendered.window_bytes.saturating_add(line.raw_bytes);
        if self.rendered.hit_ceiling {
            return;
        }

        let cut = line.content_bytes > self.limits.max_line_bytes;
        let shown_bytes = if cut {
            cut_raw_to_utf8_boundary(&line.prefix, self.limits.max_line_bytes)
        } else {
            line.content_bytes
        };
        // A complete short line always fits in the prefix.  For a long line, the prefix includes
        // four look-ahead bytes, so the boundary search above never needs the rest of the file.
        let shown = String::from_utf8_lossy(&line.prefix[..shown_bytes.min(line.prefix.len())]);
        self.lossy |= matches!(shown, Cow::Owned(_));
        let entry = format!("{number:>6}\t{shown}\n");
        // The first line is emitted whatever it costs: a result whose body is empty says less
        // than one over budget, and the truncation records the overrun either way.
        if !self.rendered.body.is_empty()
            && self.rendered.body.len().saturating_add(entry.len()) > self.limits.max_output_bytes
        {
            self.rendered.hit_ceiling = true;
            return;
        }
        self.rendered.body.push_str(&entry);
        // Retained bytes are source bytes, never the normalized display newline.  A cut line has
        // not retained its original delimiter; an uncut CRLF line has retained both delimiter
        // bytes even though its display projection uses one LF.
        self.rendered.emitted_bytes = self.rendered.emitted_bytes.saturating_add(if cut {
            shown_bytes
        } else {
            line.raw_bytes
        });
        self.rendered.emitted_lines = self.rendered.emitted_lines.saturating_add(1);
        if cut {
            self.rendered.long_lines = self.rendered.long_lines.saturating_add(1);
        }
    }

    fn finish(self) -> (RenderedLines, bool) {
        (self.rendered, self.lossy)
    }
}

/// Feeds one complete source line into accounting and, when selected, rendering.
fn observe_line(
    line: &SourceLine,
    index: usize,
    start: usize,
    end: usize,
    bytes_from_window_start: &mut usize,
    renderer: &mut StreamRenderer<'_>,
) {
    if index >= start {
        *bytes_from_window_start = bytes_from_window_start.saturating_add(line.raw_bytes);
    }
    if index >= start && index < end {
        renderer.push(line, index.saturating_add(1));
    }
}

/// Returns a raw-byte cutoff that never splits a valid UTF-8 scalar.
fn cut_raw_to_utf8_boundary(bytes: &[u8], max: usize) -> usize {
    let mut end = bytes.len().min(max);
    while end > 0 && !is_utf8_boundary(bytes, end) {
        end -= 1;
    }
    end
}

fn is_utf8_boundary(bytes: &[u8], end: usize) -> bool {
    if end == 0 || end == bytes.len() {
        return true;
    }
    let mut start = end;
    while start > 0 && is_utf8_continuation(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return utf8_width(bytes[end - 1]).is_none_or(|width| width == 1);
    }
    if start == 0 {
        return true;
    }
    utf8_width(bytes[start - 1]).is_none_or(|width| end - (start - 1) >= width)
}

const fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

const fn utf8_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
