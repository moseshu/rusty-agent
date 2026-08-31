//! Rust-native text search with scan statistics and narrowing hints.

use std::{fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use globset::GlobMatcher;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    permission::PermissionScope,
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, ResourceClaim, Tool,
        ToolArgumentDecodeError, ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_exec::fs::Workspace;
use ra_macros::ToolInput;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::search::{MAX_WALKED_FILES, SearchPathError, SearchReport, SearchRoot, compile_glob};

const TOOL_NAME: &str = "grep";

/// Searches workspace text files with a regular expression. Results include `path:line` prefixes.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    /// Regular expression to search for.
    pattern: String,
    /// Directory below the workspace to search. Defaults to the workspace root.
    path: Option<String>,
    /// Optional glob selecting files by workspace-relative path. `*` stops at `/`; `**/` recurses.
    glob: Option<String>,
    /// Match lines to return. The host may enforce a lower ceiling.
    max_results: Option<u32>,
    /// Match without regard to case.
    case_insensitive: Option<bool>,
}

/// Resource ceilings applied to one grep call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrepLimits {
    results: usize,
    output_bytes: usize,
    file_bytes: usize,
}

impl Default for GrepLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepLimits {
    /// Creates limits that retain enough context for a targeted edit without letting a generated
    /// file dominate a turn.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            results: 100,
            output_bytes: 48 * 1024,
            file_bytes: 2 * 1024 * 1024,
        }
    }
    /// Sets the maximum number of matching lines returned.
    #[must_use]
    pub const fn with_max_results(mut self, value: usize) -> Self {
        self.results = value;
        self
    }
    /// Sets the maximum number of result bytes retained.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, value: usize) -> Self {
        self.output_bytes = value;
        self
    }
    /// Sets the largest file the tool will search.
    #[must_use]
    pub const fn with_max_file_bytes(mut self, value: usize) -> Self {
        self.file_bytes = value;
        self
    }
    /// Returns the maximum number of matching lines returned.
    #[must_use]
    pub const fn max_results(&self) -> usize {
        self.results
    }
    /// Returns the maximum number of result bytes retained.
    #[must_use]
    pub const fn max_output_bytes(&self) -> usize {
        self.output_bytes
    }
    /// Returns the largest file the tool will search.
    #[must_use]
    pub const fn max_file_bytes(&self) -> usize {
        self.file_bytes
    }
}

/// The native `grep` tool.
pub struct GrepTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    root: SearchRoot,
    limits: GrepLimits,
}

impl fmt::Debug for GrepTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrepTool")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl GrepTool {
    /// Creates a tool rooted in the current directory.
    pub fn new() -> Result<Self> {
        let root = std::env::current_dir().map_err(|error| {
            Error::config("grep could not determine the current directory").with_source(error)
        })?;
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<GrepInput>(TOOL_NAME)?,
            options: base_options(),
            root: SearchRoot::Ambient(root),
            limits: GrepLimits::new(),
        })
    }
    /// Creates a tool confined to an existing workspace.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let workspace = Workspace::open(root).map_err(|error| {
            Error::config("grep workspace root cannot be opened as a workspace").with_source(error)
        })?;
        Self::for_workspace(&workspace)
    }
    /// Creates a tool bound to an already-open workspace capability.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<GrepInput>(TOOL_NAME)?,
            options: base_options()
                .with_resource_claim(ResourceClaim::shared(workspace.resource_id().clone())),
            root: SearchRoot::Rooted {
                root: workspace.root().to_path_buf(),
                filesystem: Arc::clone(workspace.filesystem()),
            },
            limits: GrepLimits::new(),
        })
    }
    /// Replaces the search ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: GrepLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Returns the search ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &GrepLimits {
        &self.limits
    }
}

#[async_trait]
impl Tool for GrepTool {
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
            .map_err(|error| GrepFailure::BadArguments(error).into_error())
    }
    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = context.take_decoded_input::<GrepInput>()?.map_or_else(
            || {
                serde_json::from_value(context.arguments().clone()).map_err(|error| {
                    GrepFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                        input_type: self.func_schema.input_type_name(),
                        message: error.to_string(),
                    })
                    .into_error()
                })
            },
            Ok,
        )?;
        // The walk and the reads are synchronous filesystem work over an unknown number of files.
        // Run inline they would hold an executor thread for the whole search, and a future that
        // never yields cannot be cancelled or timed out by the dispatcher — it awaits `call`
        // directly. Handing the work to the blocking pool is what leaves both able to fire.
        let root = self.root.clone();
        let limits = self.limits;
        tokio::task::spawn_blocking(move || search(&root, limits, &input))
            .await
            .map_err(|_| GrepFailure::Interrupted.into_error())?
            .map_err(GrepFailure::into_error)
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(GrepFailure::of(error).map(|failure| {
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.guidance()))
        }))
    }
}

fn base_options() -> ToolOptions {
    ToolOptions::new()
        .with_failure_handling(ToolFailureHandling::Custom)
        .with_permission_scope(PermissionScope::Read)
        .with_concurrency(ToolConcurrency::Parallel)
}
/// Names a bad file glob in this tool's own vocabulary.
///
/// The separator semantics live in [`compile_glob`] so both search tools select the same files; the
/// only thing left here is which failure the model is told about, which is each tool's own.
fn compile_file_glob(pattern: &str) -> std::result::Result<GlobMatcher, GrepFailure> {
    compile_glob(pattern).map_err(|error| GrepFailure::InvalidGlob(error.to_string()))
}

/// The longest match line rendered in full.
const MAX_LINE_BYTES: usize = 500;

/// Shortens one match line, returning `None` when it already fits.
///
/// Returning the cut rather than the finished line is what lets the caller record how many bytes
/// the model did not receive: a line silently ending in `…` is a truncation the host never counted.
fn abbreviate(line: &str) -> Option<String> {
    if line.len() <= MAX_LINE_BYTES {
        return None;
    }
    let end = line
        .char_indices()
        .take_while(|(index, _)| *index < MAX_LINE_BYTES)
        .last()
        .map_or(0, |(index, _)| index);
    Some(format!("{}…", &line[..end]))
}

/// Files whose content this search declined to look at, and why.
#[derive(Default)]
struct Skips {
    binary: usize,
    large: usize,
    unreadable: usize,
}

fn search(
    root: &SearchRoot,
    limits: GrepLimits,
    input: &GrepInput,
) -> std::result::Result<ToolOutput, GrepFailure> {
    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|error| GrepFailure::InvalidPattern(error.to_string()))?;
    let matcher = input.glob.as_deref().map(compile_file_glob).transpose()?;
    let base = root
        .resolve(input.path.as_deref())
        .map_err(GrepFailure::Path)?;
    let walk = root.files_below(&base).map_err(GrepFailure::Path)?;
    let limit = input
        .max_results
        .map_or(limits.results, |value| {
            usize::try_from(value).unwrap_or(usize::MAX)
        })
        .min(limits.results);

    let mut report = SearchReport::new(limit, limits.output_bytes);
    let mut skips = Skips {
        unreadable: walk.unreadable,
        ..Skips::default()
    };
    let mut hit_lines = 0_usize;
    let mut scanned = 0_usize;
    let mut cut_lines = 0_usize;
    for path in walk.files {
        if matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.is_match(&path))
        {
            continue;
        }
        // One read, not a stat and then a read: reading one byte past the ceiling already
        // distinguishes an over-size file, and the second open bought nothing but a syscall.
        let bytes = match root.read_up_to(&path, limits.file_bytes) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                skips.large = skips.large.saturating_add(1);
                continue;
            }
            Err(_) => {
                skips.unreadable = skips.unreadable.saturating_add(1);
                continue;
            }
        };
        if bytes.contains(&0) {
            skips.binary = skips.binary.saturating_add(1);
            continue;
        }
        scanned = scanned.saturating_add(1);
        for (line_index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            hit_lines = hit_lines.saturating_add(1);
            let cut = abbreviate(line);
            let rendered = format!(
                "{}:{}:{}\n",
                path.display(),
                line_index + 1,
                cut.as_deref().unwrap_or(line)
            );
            let dropped = cut
                .as_ref()
                .map_or(0, |cut| line.len().saturating_sub(cut.len()));
            if cut.is_some() {
                cut_lines = cut_lines.saturating_add(1);
            }
            report.push(&rendered, rendered.len().saturating_add(dropped));
        }
    }

    let returned = report.returned();
    let summary = format!(
        "{hit_lines} matches (returned {returned}); scanned {scanned} files; skipped {} binary, {} over-size, and {} unreadable files.\n",
        skips.binary, skips.large, skips.unreadable
    );
    let (body, truncation) = report.finish(&summary);
    let mut metadata = ObservationMetadata::new();
    if let Some(truncation) = truncation {
        metadata = metadata.with_truncation(truncation);
        if returned < hit_lines {
            metadata = metadata.with_guidance(format!(
                "grep kept only the first {returned} of {hit_lines} match lines; narrow `path`, `glob`, or the pattern."
            ));
        }
        if cut_lines > 0 {
            metadata = metadata.with_guidance(format!(
                "{cut_lines} match line(s) over {MAX_LINE_BYTES} bytes were cut to fit."
            ));
        }
    }
    // Binaries are deliberately not searched and saying so on every call would make the sentence
    // permanent noise in any repository that holds an image. These two are the skips that can
    // actually be hiding a match the model asked for.
    if skips.large > 0 || skips.unreadable > 0 {
        metadata = metadata.with_guidance(
            "Some files were over-size or unreadable; a match inside one is not in these results.",
        );
    }
    if walk.stopped_at_limit {
        metadata = metadata.with_guidance(format!(
            "The walk stopped at {MAX_WALKED_FILES} files; narrow `path` to reach the rest."
        ));
    }
    Ok(ToolOutput::text(body).with_metadata(metadata))
}

#[derive(Debug)]
enum GrepFailure {
    BadArguments(ToolArgumentDecodeError),
    InvalidPattern(String),
    InvalidGlob(String),
    Path(SearchPathError),
    /// The blocking search task did not run to completion.
    Interrupted,
}
impl GrepFailure {
    fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::InvalidPattern(_)
            | Self::InvalidGlob(_)
            | Self::Path(
                SearchPathError::OutsideRoot(_)
                | SearchPathError::InvalidPath(_)
                | SearchPathError::NotFound(_),
            ) => ToolErrorKind::InvalidInput,
            Self::Path(SearchPathError::Io(_)) | Self::Interrupted => {
                ToolErrorKind::ExecutionFailed
            }
        }
    }
    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }
    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }
    fn guidance(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => {
                "Send a string `pattern`; optional filters are `path`, `glob`, `max_results`, and `case_insensitive`."
            }
            Self::InvalidPattern(_) => "Use a valid Rust regular expression.",
            Self::InvalidGlob(_) => "Use a valid glob such as `**/*.rs`.",
            Self::Path(SearchPathError::OutsideRoot(_) | SearchPathError::InvalidPath(_)) => {
                "Search with a workspace-relative path that does not use `..`."
            }
            Self::Path(SearchPathError::NotFound(_)) => {
                "Search from the workspace root, or check the directory name first."
            }
            Self::Path(SearchPathError::Io(_)) => {
                "Check that the search directory exists and can be read."
            }
            Self::Interrupted => "Run the search again.",
        }
    }
}
impl fmt::Display for GrepFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArguments(error) => write!(formatter, "Invalid arguments: {error}."),
            Self::InvalidPattern(error) => {
                write!(formatter, "Invalid regular expression: {error}.")
            }
            Self::InvalidGlob(error) => write!(formatter, "Invalid file glob: {error}."),
            Self::Path(SearchPathError::OutsideRoot(path)) => {
                write!(formatter, "`{path}` is outside the workspace.")
            }
            Self::Path(SearchPathError::InvalidPath(path)) => write!(
                formatter,
                "`{path}` uses a path spelling this tool does not resolve."
            ),
            Self::Path(SearchPathError::NotFound(path)) => {
                write!(formatter, "No such directory or file: `{path}`.")
            }
            Self::Path(SearchPathError::Io(error)) => {
                write!(formatter, "The search path cannot be read: {error}.")
            }
            Self::Interrupted => formatter.write_str("The search did not finish."),
        }
    }
}
impl std::error::Error for GrepFailure {}
