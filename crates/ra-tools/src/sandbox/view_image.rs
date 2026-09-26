//! `view_image` against a sandbox session.
//!
//! A port of the reference's `capabilities/tools/view_image.py`. The model names an image in the
//! workspace or under a grant; the tool reads it through the session, decides what it is from its
//! bytes, and returns it as an image. Everything that goes wrong with the file — missing, too large,
//! not an image — comes back as a sentence the model can act on rather than as a failure, and names
//! the file as the model named it, never where the backend keeps it.
//!
//! What a file is comes from its leading bytes: PNG, JPEG, GIF, WebP, BMP and TIFF by signature,
//! SVG by its opening markup. An extension decides nothing, except that a file named `.svg` or
//! `.svgz` whose bytes are not recognised is still sent as SVG, which is what the reference does
//! for compressed and oddly encoded vector files.
//!
//! # Deviations from the reference
//!
//! - **An unreadable file names the failure's code.** The reference reports the Python exception's
//!   class name (`unable to read image at … : PermissionError`); a session failure here has no
//!   class, and its code is its stable name.
//! - **A path outside the workspace and its grants is an error, as in the reference**, carried as
//!   a tool failure whose source is the session's refusal.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    item::{ImageBlock, ImageSource},
    sandbox::{ErrorCode, PosixPath, SandboxSession, SandboxWorkspaceScope, User},
    tool::{
        FuncSchema, Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolOutputBlock,
        ToolSchema,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{NeedsApproval, decode, sandbox_tool_options, session_failure};

/// The name `view_image` is advertised under.
pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";

/// The largest image the tool returns, in bytes.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGE_SIZE_LABEL: &str = "10MB";
const SVG_SNIFF_BYTES: usize = 2048;

/// The arguments `view_image` takes.
///
/// Unknown fields are ignored, as the reference's model ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToolInput)]
#[tool_input(
    strict = false,
    description = "Loads an image from the sandbox workspace or an explicitly granted sandbox path \
                   and returns it as a structured image output."
)]
pub struct ViewImageArgs {
    /// Path to the image file. Workspace paths and explicitly granted sandbox paths are supported.
    #[schemars(length(min = 1))]
    path: String,
}

impl ViewImageArgs {
    /// Looks at `path`.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }

    /// The image's path, as the model wrote it.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Enforces the bound the schema states and a decoder does not.
    fn validate(&self) -> Result<()> {
        if self.path.is_empty() {
            return Err(Error::tool(
                ToolErrorKind::InvalidInput,
                VIEW_IMAGE_TOOL_NAME,
                "`path` should have at least 1 character",
            ));
        }
        Ok(())
    }
}

/// What a file's bytes say it is, or `None` for a file that is not a supported image.
///
/// `path` is consulted only for a file whose bytes are not recognised: a `.svg` or `.svgz` name
/// still makes it SVG.
#[must_use]
pub fn detect_image_mime_type(path: &str, payload: &[u8]) -> Option<&'static str> {
    if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if payload.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if payload.starts_with(b"GIF87a") || payload.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if payload.starts_with(b"RIFF") && payload.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if payload.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if payload.starts_with(b"II*\x00") || payload.starts_with(b"MM\x00*") {
        return Some("image/tiff");
    }

    let head = &payload[..payload.len().min(SVG_SNIFF_BYTES)];
    let snippet = trim_leading_ascii_space(head).to_ascii_lowercase();
    if snippet.starts_with(b"<svg")
        || (snippet.starts_with(b"<?xml") && contains(&snippet, b"<svg"))
    {
        return Some("image/svg+xml");
    }

    matches!(suffix(path).to_lowercase().as_str(), ".svg" | ".svgz").then_some("image/svg+xml")
}

/// Strips what Python's `bytes.lstrip` strips: space, tab, newline, carriage return, vertical tab
/// and form feed.
fn trim_leading_ascii_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The final extension of a path's last component, as a host path's `suffix` answers: empty for a
/// name without a dot, one that only starts with a dot, or one that ends with one.
fn suffix(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

/// Runs `view_image` against a sandbox session.
#[derive(Clone)]
pub struct ViewImageTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    session: Arc<dyn SandboxSession>,
    user: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
    needs_approval: NeedsApproval,
}

impl fmt::Debug for ViewImageTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewImageTool")
            .field("backend", &self.session.backend_id())
            .field("user", &self.user)
            .field("workspace_scope", &self.workspace_scope)
            .field("needs_approval", &self.needs_approval)
            .finish_non_exhaustive()
    }
}

impl ViewImageTool {
    /// A tool reading images from `session` as its own user, from its workspace root, without
    /// approval.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the schema cannot be built, which is a defect here rather
    /// than a condition a caller can cause.
    pub fn new(session: Arc<dyn SandboxSession>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(VIEW_IMAGE_TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ViewImageArgs>(VIEW_IMAGE_TOOL_NAME)?,
            session,
            user: None,
            workspace_scope: SandboxWorkspaceScope::root(),
            needs_approval: NeedsApproval::Never,
        })
    }

    /// Reads images as `user`.
    #[must_use]
    pub fn with_user(mut self, user: Option<User>) -> Self {
        self.user = user;
        self
    }

    /// Measures relative paths from `workspace_scope`.
    #[must_use]
    pub fn with_workspace_scope(mut self, workspace_scope: SandboxWorkspaceScope) -> Self {
        self.workspace_scope = workspace_scope;
        self
    }

    /// Makes calls wait for approval as `needs_approval` says.
    #[must_use]
    pub fn with_needs_approval(mut self, needs_approval: impl Into<NeedsApproval>) -> Self {
        self.needs_approval = needs_approval.into();
        self
    }

    /// Changes whether calls wait for approval, as a tool-set configurator does.
    pub fn set_needs_approval(&mut self, needs_approval: impl Into<NeedsApproval>) {
        self.needs_approval = needs_approval.into();
    }

    /// The session images are read from.
    #[must_use]
    pub fn session(&self) -> &Arc<dyn SandboxSession> {
        &self.session
    }

    /// The user images are read as, or `None` for the session's own.
    #[must_use]
    pub const fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    /// Where relative paths are measured from.
    #[must_use]
    pub const fn workspace_scope(&self) -> &SandboxWorkspaceScope {
        &self.workspace_scope
    }

    /// Whether calls wait for approval.
    #[must_use]
    pub const fn needs_approval_policy(&self) -> &NeedsApproval {
        &self.needs_approval
    }

    /// Runs one call.
    ///
    /// Returns the image, or a sentence saying why there is none: the file is missing, cannot be
    /// read, is over [`MAX_IMAGE_BYTES`], or is not a supported image.
    ///
    /// # Errors
    ///
    /// Returns invalid-input for an empty path, and a failure for a path outside the workspace and
    /// its grants, carrying the session's refusal as its source.
    pub async fn run(&self, args: &ViewImageArgs) -> Result<ToolOutput> {
        args.validate()?;
        let scoped_path = self
            .workspace_scope
            .anchor(PosixPath::coerce(&args.path).as_str());
        let path_policy = self
            .session
            .workspace_path_policy()
            .map_err(|error| session_failure(VIEW_IMAGE_TOOL_NAME, error))?;
        let resolved_path = path_policy
            .normalize_sandbox_path(&scoped_path, false)
            .map_err(|error| session_failure(VIEW_IMAGE_TOOL_NAME, error))?;
        // A granted path outside the workspace has no workspace-relative form, so it is shown as
        // the absolute path the model already knows.
        let display_path = path_policy
            .relative_path(&scoped_path)
            .ok()
            .and_then(|relative| {
                self.workspace_scope
                    .display_path(&scoped_path, relative.as_str())
                    .ok()
            })
            .unwrap_or_else(|| resolved_path.clone())
            .to_string();

        // One byte past the ceiling, as the reference reads from its file handle: enough to tell an
        // oversized file from one exactly at the limit, and never the whole of it.
        let payload = match self
            .session
            .read_up_to(
                resolved_path.as_str(),
                self.user.clone(),
                u64::try_from(MAX_IMAGE_BYTES + 1).unwrap_or(u64::MAX),
            )
            .await
        {
            Ok(payload) => payload,
            Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {
                return Ok(ToolOutput::text(format!(
                    "image path `{display_path}` was not found"
                )));
            }
            Err(error) => {
                return Ok(ToolOutput::text(format!(
                    "unable to read image at `{display_path}`: {}",
                    error.error_code()
                )));
            }
        };

        if payload.len() > MAX_IMAGE_BYTES {
            return Ok(ToolOutput::text(format!(
                "image path `{display_path}` exceeded the allowed size of {MAX_IMAGE_SIZE_LABEL}; \
                 resize or compress the image and try again"
            )));
        }

        let Some(mime_type) = detect_image_mime_type(resolved_path.as_str(), &payload) else {
            return Ok(ToolOutput::text(format!(
                "image path `{display_path}` is not a supported image file"
            )));
        };

        Ok(ToolOutput::block(ToolOutputBlock::Image(ImageBlock::new(
            ImageSource::base64(mime_type, BASE64.encode(&payload)),
        ))))
    }
}

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

    fn options(&self) -> ToolOptions {
        sandbox_tool_options(&self.needs_approval)
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.needs_approval.evaluate(context).await
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let args: ViewImageArgs = decode(&mut context, &self.func_schema)?;
        self.run(&args).await
    }
}
