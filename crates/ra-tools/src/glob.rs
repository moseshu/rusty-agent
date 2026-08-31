//! Rust-native file-pattern lookup with deterministic output.

use std::{fmt, path::Path, sync::Arc};

use async_trait::async_trait;
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
use schemars::JsonSchema;
use serde::Deserialize;

use crate::search::{MAX_WALKED_FILES, SearchPathError, SearchReport, SearchRoot, compile_glob};

const TOOL_NAME: &str = "glob";

/// Finds files whose workspace-relative paths match a glob pattern.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    /// Glob matched against workspace-relative paths. `*` stops at `/`; use `**/` to recurse.
    pattern: String,
    /// Directory below the workspace to search. Defaults to the workspace root.
    path: Option<String>,
    /// Paths to return. The host may enforce a lower ceiling.
    max_results: Option<u32>,
}

/// Resource ceilings applied to one glob call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobLimits {
    max_results: usize,
    max_output_bytes: usize,
}
impl Default for GlobLimits {
    fn default() -> Self {
        Self::new()
    }
}
impl GlobLimits {
    /// Creates the default ceilings.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_results: 200,
            max_output_bytes: 32 * 1024,
        }
    }
    /// Sets the maximum number of paths returned.
    #[must_use]
    pub const fn with_max_results(mut self, value: usize) -> Self {
        self.max_results = value;
        self
    }
    /// Sets the maximum number of result bytes retained.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, value: usize) -> Self {
        self.max_output_bytes = value;
        self
    }
    /// Returns the maximum number of paths returned.
    #[must_use]
    pub const fn max_results(&self) -> usize {
        self.max_results
    }
    /// Returns the maximum number of result bytes retained.
    #[must_use]
    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

/// The native `glob` tool.
pub struct GlobTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    root: SearchRoot,
    limits: GlobLimits,
}
impl fmt::Debug for GlobTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GlobTool")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl GlobTool {
    /// Creates a tool rooted in the current directory.
    pub fn new() -> Result<Self> {
        let root = std::env::current_dir().map_err(|error| {
            Error::config("glob could not determine the current directory").with_source(error)
        })?;
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<GlobInput>(TOOL_NAME)?,
            options: base_options(),
            root: SearchRoot::Ambient(root),
            limits: GlobLimits::new(),
        })
    }
    /// Creates a tool confined to an existing workspace.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let workspace = Workspace::open(root).map_err(|error| {
            Error::config("glob workspace root cannot be opened as a workspace").with_source(error)
        })?;
        Self::for_workspace(&workspace)
    }
    /// Creates a tool bound to an already-open workspace capability.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<GlobInput>(TOOL_NAME)?,
            options: base_options()
                .with_resource_claim(ResourceClaim::shared(workspace.resource_id().clone())),
            root: SearchRoot::Rooted {
                root: workspace.root().to_path_buf(),
                filesystem: Arc::clone(workspace.filesystem()),
            },
            limits: GlobLimits::new(),
        })
    }
    /// Replaces the search ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: GlobLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Returns the search ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &GlobLimits {
        &self.limits
    }
}

fn search(
    root: &SearchRoot,
    limits: GlobLimits,
    input: &GlobInput,
) -> std::result::Result<ToolOutput, GlobFailure> {
    let matcher = compile_glob(&input.pattern)
        .map_err(|error| GlobFailure::InvalidPattern(error.to_string()))?;
    let base = root
        .resolve(input.path.as_deref())
        .map_err(GlobFailure::Path)?;
    let walk = root.files_below(&base).map_err(GlobFailure::Path)?;
    let scanned = walk.files.len();
    let limit = input
        .max_results
        .map_or(limits.max_results, |value| {
            usize::try_from(value).unwrap_or(usize::MAX)
        })
        .min(limits.max_results);

    let mut report = SearchReport::new(limit, limits.max_output_bytes);
    let mut hit_files = 0_usize;
    for path in walk.files {
        if !matcher.is_match(&path) {
            continue;
        }
        hit_files = hit_files.saturating_add(1);
        let line = format!("{}\n", path.display());
        report.push(&line, line.len());
    }

    let returned = report.returned();
    let summary = format!(
        "{hit_files} files matched (returned {returned}); scanned {scanned} files; skipped {} unreadable entries.\n",
        walk.unreadable
    );
    let (body, truncation) = report.finish(&summary);
    let mut metadata = ObservationMetadata::new();
    if let Some(truncation) = truncation {
        metadata = metadata.with_truncation(truncation).with_guidance(format!(
            "glob kept only the first {returned} of {hit_files} paths; narrow `path` or `pattern`."
        ));
    }
    if walk.stopped_at_limit {
        metadata = metadata.with_guidance(format!(
            "The walk stopped at {MAX_WALKED_FILES} files; narrow `path` to reach the rest."
        ));
    }
    Ok(ToolOutput::text(body).with_metadata(metadata))
}

#[async_trait]
impl Tool for GlobTool {
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
            .map_err(|error| GlobFailure::BadArguments(error).into_error())
    }
    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = context.take_decoded_input::<GlobInput>()?.map_or_else(
            || {
                serde_json::from_value(context.arguments().clone()).map_err(|error| {
                    GlobFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                        input_type: self.func_schema.input_type_name(),
                        message: error.to_string(),
                    })
                    .into_error()
                })
            },
            Ok,
        )?;
        // Synchronous walking belongs on the blocking pool, for the reason `grep` states: run
        // inline it holds an executor thread and leaves the dispatcher unable to cancel the call.
        let root = self.root.clone();
        let limits = self.limits;
        tokio::task::spawn_blocking(move || search(&root, limits, &input))
            .await
            .map_err(|_| GlobFailure::Interrupted.into_error())?
            .map_err(GlobFailure::into_error)
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(GlobFailure::of(error).map(|failure| {
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
#[derive(Debug)]
enum GlobFailure {
    BadArguments(ToolArgumentDecodeError),
    InvalidPattern(String),
    Path(SearchPathError),
    /// The blocking walk did not run to completion.
    Interrupted,
}
impl GlobFailure {
    fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::InvalidPattern(_)
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
                "Send a glob `pattern`, with optional `path` and `max_results`."
            }
            Self::InvalidPattern(_) => "Use a valid glob such as `src/**/*.rs`.",
            Self::Path(SearchPathError::OutsideRoot(_) | SearchPathError::InvalidPath(_)) => {
                "Search with a workspace-relative path that does not use `..`."
            }
            Self::Path(SearchPathError::NotFound(_)) => {
                "Search from the workspace root, or check the directory name first."
            }
            Self::Path(SearchPathError::Io(_)) => {
                "Check that the search directory exists and can be read."
            }
            Self::Interrupted => "Run the lookup again.",
        }
    }
}
impl fmt::Display for GlobFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArguments(error) => write!(formatter, "Invalid arguments: {error}."),
            Self::InvalidPattern(error) => write!(formatter, "Invalid glob: {error}."),
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
            Self::Interrupted => formatter.write_str("The lookup did not finish."),
        }
    }
}
impl std::error::Error for GlobFailure {}
