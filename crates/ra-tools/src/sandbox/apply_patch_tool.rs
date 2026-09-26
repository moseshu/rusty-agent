//! `apply_patch` against a sandbox session: a custom tool taking the patch as raw text.
//!
//! A port of the reference's `capabilities/tools/apply_patch_tool.py`. The model writes the whole
//! patch — `*** Begin Patch` to `*** End Patch` — as the tool's raw input, constrained by the
//! reference's Lark grammar, and the tool splits it into one [`ApplyPatchOperation`] per file and
//! applies them through the [`WorkspaceEditor`]. A JSON payload of operations is accepted too, as
//! the reference accepts one.
//!
//! Every operation's paths are checked before any operation runs, so a patch with one bad path
//! changes nothing. The operations themselves still run one after another: a diff that fails to
//! apply leaves the files before it changed, as the reference leaves them.
//!
//! Anything that goes wrong comes back to the model as the failure's own text — a parse error, a
//! refused path, a diff that does not apply. That is the runtime's default for every custom tool,
//! as it is the reference's custom-tool runner's; this tool only has to put the text on its error.
//!
//! # Approval
//!
//! A per-operation check ([`PatchApproval`]) is shown each operation with its paths in canonical
//! form, so it judges the files that will actually be touched. A patch that does not parse, or
//! names a path outside the workspace, needs no approval: it is left to fail as an ordinary result
//! the model can correct, rather than stopping the run to ask about something that will not run.
//!
//! # Deviations from the reference
//!
//! - **An approval that resolves mid-check is not consulted.** The reference re-reads the call's
//!   approval status after each operation's check, so a host that answers while a slow check runs
//!   stops the remaining checks. Here a call's approval is decided before the call pauses and
//!   answered only after the turn has stopped, so there is no status to read; the checks stop at the
//!   first operation that needs approval, which is the same answer.
//! - **Invalid JSON is reported in serde's words.** The reference passes Python's `json` error
//!   through; the two parsers phrase the same fault differently.
//! - **The reference's `ApplyPatchEditor` protocol is not a trait here.** Its only implementor in
//!   scope is [`SandboxApplyPatchEditor`]; the protocol serves the reference's hosted `ApplyPatchTool`,
//!   which is not ported.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    model::{CustomToolFormat, CustomToolGrammarSyntax},
    sandbox::{ErrorCode, SandboxError, SandboxSession, SandboxWorkspaceScope, User},
    tool::{
        Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_patch::{ApplyPatchOperation, ApplyPatchOperationType, ApplyPatchResult};
use serde_json::Value;

use super::apply_patch::WorkspaceEditor;

/// The name `apply_patch` is advertised under.
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

/// The Lark grammar the patch is constrained to, verbatim from the reference.
pub const APPLY_PATCH_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF"#;

/// The tool's description, verbatim from the reference.
pub const APPLY_PATCH_DESCRIPTION: &str = r#"Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.
Your patch language is a stripped-down, file-oriented diff format designed to be easy to
parse and safe to apply. You can think of it as a high-level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).

May be immediately followed by *** Move to: <new path> if you want to rename the file.
Then one or more hunks, each introduced by @@ (optionally followed by a hunk header).
Within a hunk, each line starts with a space, -, or +.

For context lines:
- By default, show 3 lines of code immediately above and 3 lines immediately below each
change. If a change is within 3 lines of a previous change, do NOT duplicate the first
change's post-context lines in the second change's pre-context lines.
- If 3 lines of context is insufficient to uniquely identify the snippet of code within the
file, use the @@ operator to indicate the class or function to which the snippet belongs.
For instance:
@@ class BaseClass
[3 lines of pre-context]
-[old_code]
+[new_code]
[3 lines of post-context]

- If a code block is repeated so many times in a class or function that a single @@ statement
and 3 lines of context cannot uniquely identify the snippet, use multiple @@ statements to
jump to the right context. For instance:

@@ class BaseClass
@@ def method():
[3 lines of pre-context]
-[old_code]
+[new_code]
[3 lines of post-context]

The full grammar definition is below:
Patch := Begin { FileOp } End
Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE
FileOp := AddFile | DeleteFile | UpdateFile
AddFile := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile := "*** Delete File: " path NEWLINE
UpdateFile := "*** Update File: " path NEWLINE [ MoveTo ] Hunk { Hunk }
MoveTo := "*** Move to: " newPath NEWLINE
Hunk := "@@" [ header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine := (" " | "-" | "+") text NEWLINE

A full patch can combine several operations:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

Important:
- You must include a header with your intended action (Add/Delete/Update).
- You must prefix new lines with + even when creating a new file.
- File references can only be relative, NEVER ABSOLUTE."#;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";

/// Why a patch could not be read, in the reference's words.
///
/// The reference raises `ValueError` with the same message; the model reads it as the tool's
/// result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchParseError {
    message: String,
}

impl PatchParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// What was wrong.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PatchParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PatchParseError {}

type ParseResult<T> = std::result::Result<T, PatchParseError>;

/// Splits the tool's raw input into operations.
///
/// Input that starts, after leading whitespace, with `{` or `[` is read as JSON: an object with an
/// `operations` array or an `operation` object, a bare operation object, or an array of them.
/// Anything else is read as a patch envelope.
///
/// # Errors
///
/// Returns the reference's message for input that is neither.
pub fn parse_apply_patch_input(raw_input: &str) -> ParseResult<Vec<ApplyPatchOperation>> {
    let stripped = raw_input.trim_start_matches(is_py_space);
    if stripped.starts_with('{') || stripped.starts_with('[') {
        return parse_json_input(raw_input);
    }
    parse_patch_input(raw_input)
}

fn parse_json_input(raw_input: &str) -> ParseResult<Vec<ApplyPatchOperation>> {
    let payload: Value =
        serde_json::from_str(raw_input).map_err(|error| PatchParseError::new(error.to_string()))?;
    match &payload {
        Value::Object(object) => {
            if let Some(Value::Array(operations)) = object.get("operations") {
                return operations.iter().map(parse_json_operation).collect();
            }
            if let Some(operation) = object.get("operation").filter(|value| !value.is_null()) {
                return Ok(vec![parse_json_operation(operation)?]);
            }
            Ok(vec![parse_json_operation(&payload)?])
        }
        Value::Array(operations) => operations.iter().map(parse_json_operation).collect(),
        _ => Err(PatchParseError::new(
            "apply_patch JSON input must be an object or array",
        )),
    }
}

fn parse_json_operation(operation: &Value) -> ParseResult<ApplyPatchOperation> {
    let Value::Object(object) = operation else {
        return Err(PatchParseError::new(
            "apply_patch operation must be an object",
        ));
    };
    let raw_type = object.get("type");
    let kind = match raw_type.and_then(Value::as_str) {
        Some("create_file") => ApplyPatchOperationType::CreateFile,
        Some("update_file") => ApplyPatchOperationType::UpdateFile,
        Some("delete_file") => ApplyPatchOperationType::DeleteFile,
        _ => {
            return Err(PatchParseError::new(format!(
                "Invalid apply_patch operation type: {}",
                python_str(raw_type)
            )));
        }
    };
    let path = match object.get("path") {
        Some(Value::String(path)) if !path.is_empty() => path,
        _ => {
            return Err(PatchParseError::new(
                "apply_patch operation is missing a path",
            ));
        }
    };
    let mut parsed = ApplyPatchOperation::new(kind, path.as_str());
    if kind != ApplyPatchOperationType::DeleteFile {
        let Some(Value::String(diff)) = object.get("diff") else {
            return Err(PatchParseError::new(format!(
                "apply_patch operation {} is missing a diff",
                kind.as_str()
            )));
        };
        parsed = parsed.with_diff(diff.as_str());
    }
    match object.get("move_to") {
        None | Some(Value::Null) => {}
        Some(Value::String(move_to)) => parsed = parsed.with_move_to(move_to.as_str()),
        Some(_) => {
            return Err(PatchParseError::new(
                "apply_patch operation move_to must be a string",
            ));
        }
    }
    Ok(parsed)
}

/// How Python's `str()` renders a JSON value, which is how the reference names a bad `type`.
fn python_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::Bool(false)) => "False".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn parse_patch_input(raw_input: &str) -> ParseResult<Vec<ApplyPatchOperation>> {
    let lines = py_splitlines(raw_input);
    if lines.first() != Some(&BEGIN_PATCH) {
        return Err(PatchParseError::new(
            "apply_patch input must start with '*** Begin Patch'",
        ));
    }
    if lines.len() < 2 || lines.last() != Some(&END_PATCH) {
        return Err(PatchParseError::new(
            "apply_patch input must end with '*** End Patch'",
        ));
    }

    let mut operations = Vec::new();
    let mut index = 1;
    while index < lines.len() - 1 {
        let line = lines[index];
        let (parsed, next) = if line.starts_with(ADD_FILE) {
            parse_add_file(&lines, index)?
        } else if line.starts_with(DELETE_FILE) {
            parse_delete_file(&lines, index)?
        } else if line.starts_with(UPDATE_FILE) {
            parse_update_file(&lines, index)?
        } else {
            return Err(PatchParseError::new(format!(
                "Invalid apply_patch file operation header: {line}"
            )));
        };
        operations.push(parsed);
        index = next;
    }

    if operations.is_empty() {
        return Err(PatchParseError::new(
            "apply_patch input must include at least one file operation",
        ));
    }
    Ok(operations)
}

fn parse_add_file(lines: &[&str], index: usize) -> ParseResult<(ApplyPatchOperation, usize)> {
    let path = parse_path_header(lines[index], ADD_FILE)?;
    let mut index = index + 1;
    let mut diff_lines = Vec::new();
    while index < lines.len() - 1 && !is_file_operation_header(lines[index]) {
        let line = lines[index];
        if !line.starts_with('+') {
            return Err(PatchParseError::new(format!(
                "Invalid Add File line: {line}"
            )));
        }
        diff_lines.push(line);
        index += 1;
    }
    if diff_lines.is_empty() {
        return Err(PatchParseError::new(format!(
            "Add File patch for {path} must include at least one + line"
        )));
    }
    Ok((
        ApplyPatchOperation::create_file(path, join_diff(&diff_lines)),
        index,
    ))
}

fn parse_delete_file(lines: &[&str], index: usize) -> ParseResult<(ApplyPatchOperation, usize)> {
    let path = parse_path_header(lines[index], DELETE_FILE)?;
    let index = index + 1;
    if index < lines.len() - 1 && !is_file_operation_header(lines[index]) {
        return Err(PatchParseError::new(format!(
            "Delete File patch for {path} must not include a diff"
        )));
    }
    Ok((ApplyPatchOperation::delete_file(path), index))
}

fn parse_update_file(lines: &[&str], index: usize) -> ParseResult<(ApplyPatchOperation, usize)> {
    let path = parse_path_header(lines[index], UPDATE_FILE)?;
    let mut index = index + 1;
    let mut move_to = None;
    if index < lines.len() - 1 && lines[index].starts_with(MOVE_TO) {
        move_to = Some(parse_path_header(lines[index], MOVE_TO)?);
        index += 1;
    }

    let mut diff_lines = Vec::new();
    while index < lines.len() - 1 && !is_file_operation_header(lines[index]) {
        diff_lines.push(lines[index]);
        index += 1;
    }
    if diff_lines.is_empty() {
        return Err(PatchParseError::new(format!(
            "Update File patch for {path} must include a hunk"
        )));
    }
    let mut operation = ApplyPatchOperation::update_file(path, join_diff(&diff_lines));
    if let Some(move_to) = move_to {
        operation = operation.with_move_to(move_to);
    }
    Ok((operation, index))
}

fn parse_path_header<'a>(line: &'a str, prefix: &str) -> ParseResult<&'a str> {
    let path = line
        .strip_prefix(prefix)
        .unwrap_or(line)
        .trim_matches(is_py_space);
    if path.is_empty() {
        return Err(PatchParseError::new(format!(
            "Missing path in apply_patch header: {line}"
        )));
    }
    Ok(path)
}

fn is_file_operation_header(line: &str) -> bool {
    line.starts_with(ADD_FILE) || line.starts_with(DELETE_FILE) || line.starts_with(UPDATE_FILE)
}

fn join_diff(lines: &[&str]) -> String {
    let mut diff = lines.join("\n");
    diff.push('\n');
    diff
}

/// Whether Python's `str.isspace` holds for `character`.
fn is_py_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// Python's `str.splitlines`: every line boundary it recognises, `\r\n` counted once, and no final
/// empty line after a trailing boundary.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let is_boundary = matches!(
            character,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_boundary {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + character.len_utf8();
        if character == '\r'
            && let Some(&(next_index, '\n')) = chars.peek()
        {
            chars.next();
            end = next_index + 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Decides, per operation, whether a patch waits for the host's approval.
///
/// The reference's operation-typed `needs_approval` callable. A plain closure over the context and
/// the operation is a check.
#[async_trait]
pub trait PatchApprovalCheck: Send + Sync {
    /// Whether this operation needs approval.
    ///
    /// # Errors
    ///
    /// Returns whatever deciding failed with; the call is not run.
    async fn needs_approval(
        &self,
        context: &ToolContext<'_>,
        operation: &ApplyPatchOperation,
    ) -> Result<bool>;
}

#[async_trait]
impl<F> PatchApprovalCheck for F
where
    F: Fn(&ToolContext<'_>, &ApplyPatchOperation) -> Result<bool> + Send + Sync,
{
    async fn needs_approval(
        &self,
        context: &ToolContext<'_>,
        operation: &ApplyPatchOperation,
    ) -> Result<bool> {
        self(context, operation)
    }
}

/// Whether a patch's operations wait for the host's approval.
///
/// The reference's `needs_approval: bool | ApplyPatchApprovalFunction`, defaulting to no approval.
#[non_exhaustive]
#[derive(Clone, Default)]
pub enum PatchApproval {
    /// Never.
    #[default]
    Never,
    /// For every patch that parses and stays inside the workspace.
    Always,
    /// Decided per operation.
    Check(Arc<dyn PatchApprovalCheck>),
}

impl PatchApproval {
    /// Decides per operation with `check`.
    #[must_use]
    pub fn check(check: impl PatchApprovalCheck + 'static) -> Self {
        Self::Check(Arc::new(check))
    }

    /// The static policy the tool declares.
    ///
    /// Anything but [`Self::Never`] is dynamic: even an unconditional approval first reads the
    /// patch, so a malformed one fails as a result instead of waiting for a host.
    #[must_use]
    pub const fn policy(&self) -> ToolApprovalPolicy {
        match self {
            Self::Never => ToolApprovalPolicy::Never,
            Self::Always | Self::Check(_) => ToolApprovalPolicy::Dynamic,
        }
    }

    async fn evaluate(
        &self,
        context: &ToolContext<'_>,
        operation: &ApplyPatchOperation,
    ) -> Result<bool> {
        match self {
            Self::Never => Ok(false),
            Self::Always => Ok(true),
            Self::Check(check) => check.needs_approval(context, operation).await,
        }
    }
}

impl From<bool> for PatchApproval {
    fn from(value: bool) -> Self {
        if value { Self::Always } else { Self::Never }
    }
}

impl fmt::Debug for PatchApproval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Never => formatter.write_str("Never"),
            Self::Always => formatter.write_str("Always"),
            Self::Check(_) => formatter.write_str("Check(..)"),
        }
    }
}

/// Applies one operation at a time on a sandbox session, as its bound user and from its bound
/// working directory.
#[derive(Clone)]
pub struct SandboxApplyPatchEditor {
    session: Arc<dyn SandboxSession>,
    user: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
}

impl fmt::Debug for SandboxApplyPatchEditor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxApplyPatchEditor")
            .field("backend", &self.session.backend_id())
            .field("user", &self.user)
            .field("workspace_scope", &self.workspace_scope)
            .finish()
    }
}

impl SandboxApplyPatchEditor {
    /// An editor over `session`.
    #[must_use]
    pub const fn new(
        session: Arc<dyn SandboxSession>,
        user: Option<User>,
        workspace_scope: SandboxWorkspaceScope,
    ) -> Self {
        Self {
            session,
            user,
            workspace_scope,
        }
    }

    /// The user operations run as.
    #[must_use]
    pub const fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    fn workspace_editor(&self) -> WorkspaceEditor<'_> {
        WorkspaceEditor::new(self.session.as_ref())
            .with_user(self.user.clone())
            .with_workspace_scope(self.workspace_scope.clone())
    }

    /// Creates a file.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceEditor::apply_operation`].
    pub async fn create_file(
        &self,
        operation: &ApplyPatchOperation,
    ) -> std::result::Result<ApplyPatchResult, SandboxError> {
        self.workspace_editor().apply_operation(operation).await
    }

    /// Updates, and possibly moves, a file.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceEditor::apply_operation`].
    pub async fn update_file(
        &self,
        operation: &ApplyPatchOperation,
    ) -> std::result::Result<ApplyPatchResult, SandboxError> {
        self.workspace_editor().apply_operation(operation).await
    }

    /// Deletes a file.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceEditor::apply_operation`].
    pub async fn delete_file(
        &self,
        operation: &ApplyPatchOperation,
    ) -> std::result::Result<ApplyPatchResult, SandboxError> {
        self.workspace_editor().apply_operation(operation).await
    }
}

/// Runs `apply_patch` against a sandbox session.
#[derive(Clone)]
pub struct SandboxApplyPatchTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    session: Arc<dyn SandboxSession>,
    workspace_scope: SandboxWorkspaceScope,
    editor: SandboxApplyPatchEditor,
    needs_approval: PatchApproval,
}

impl fmt::Debug for SandboxApplyPatchTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxApplyPatchTool")
            .field("editor", &self.editor)
            .field("needs_approval", &self.needs_approval)
            .finish_non_exhaustive()
    }
}

impl SandboxApplyPatchTool {
    /// A tool patching `session`'s workspace as its own user, from its root, without approval.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the tool's identity cannot be built, which is a defect here
    /// rather than a condition a caller can cause.
    pub fn new(session: Arc<dyn SandboxSession>) -> Result<Self> {
        let format = CustomToolFormat::Grammar {
            syntax: CustomToolGrammarSyntax::Lark,
            definition: APPLY_PATCH_GRAMMAR.to_owned(),
        };
        Ok(Self {
            origin: ToolOrigin::new(APPLY_PATCH_TOOL_NAME)?,
            schema: ToolSchema::custom(APPLY_PATCH_TOOL_NAME, Some(format))?
                .with_description(APPLY_PATCH_DESCRIPTION),
            editor: SandboxApplyPatchEditor::new(
                Arc::clone(&session),
                None,
                SandboxWorkspaceScope::root(),
            ),
            session,
            workspace_scope: SandboxWorkspaceScope::root(),
            needs_approval: PatchApproval::Never,
        })
    }

    /// Applies operations as `user`.
    #[must_use]
    pub fn with_user(mut self, user: Option<User>) -> Self {
        self.editor.user = user;
        self
    }

    /// Measures relative paths from `workspace_scope`.
    #[must_use]
    pub fn with_workspace_scope(mut self, workspace_scope: SandboxWorkspaceScope) -> Self {
        self.editor.workspace_scope = workspace_scope.clone();
        self.workspace_scope = workspace_scope;
        self
    }

    /// Makes patches wait for approval as `needs_approval` says.
    #[must_use]
    pub fn with_needs_approval(mut self, needs_approval: impl Into<PatchApproval>) -> Self {
        self.needs_approval = needs_approval.into();
        self
    }

    /// Changes whether patches wait for approval, as a tool-set configurator does.
    pub fn set_needs_approval(&mut self, needs_approval: impl Into<PatchApproval>) {
        self.needs_approval = needs_approval.into();
    }

    /// Whether patches wait for approval.
    #[must_use]
    pub const fn needs_approval_policy(&self) -> &PatchApproval {
        &self.needs_approval
    }

    /// The session patches are applied to.
    #[must_use]
    pub fn session(&self) -> &Arc<dyn SandboxSession> {
        &self.session
    }

    /// Where relative paths are measured from.
    #[must_use]
    pub const fn workspace_scope(&self) -> &SandboxWorkspaceScope {
        &self.workspace_scope
    }

    /// The editor operations are applied with.
    #[must_use]
    pub const fn editor(&self) -> &SandboxApplyPatchEditor {
        &self.editor
    }

    /// Splits raw input into operations.
    ///
    /// # Errors
    ///
    /// As [`parse_apply_patch_input`].
    pub fn parse_custom_input(&self, raw_input: &str) -> ParseResult<Vec<ApplyPatchOperation>> {
        parse_apply_patch_input(raw_input)
    }

    fn normalize_operation(
        &self,
        operation: &ApplyPatchOperation,
    ) -> std::result::Result<ApplyPatchOperation, SandboxError> {
        WorkspaceEditor::new(self.session.as_ref())
            .with_workspace_scope(self.workspace_scope.clone())
            .normalize_operation(operation)
    }

    /// Applies a patch and returns what each operation reported, one line per operation.
    ///
    /// Every operation's paths are checked before any operation runs.
    ///
    /// # Errors
    ///
    /// Returns a failure carrying the parse error or the session's refusal; the tool reports its
    /// text to the model.
    pub async fn run(&self, raw_input: &str) -> Result<String> {
        let operations = self.parse_custom_input(raw_input).map_err(parse_failure)?;
        for operation in &operations {
            self.normalize_operation(operation).map_err(patch_failure)?;
        }

        let mut outputs = Vec::new();
        for operation in &operations {
            let result = match operation.kind() {
                ApplyPatchOperationType::CreateFile => self.editor.create_file(operation).await,
                ApplyPatchOperationType::UpdateFile => self.editor.update_file(operation).await,
                ApplyPatchOperationType::DeleteFile => self.editor.delete_file(operation).await,
                other => {
                    return Err(parse_failure(PatchParseError::new(format!(
                        "Unsupported apply_patch operation: {other}"
                    ))));
                }
            }
            .map_err(patch_failure)?;
            if let Some(output) = result.output_text().filter(|output| !output.is_empty()) {
                outputs.push(output.to_owned());
            }
        }
        Ok(outputs.join("\n"))
    }
}

fn parse_failure(error: PatchParseError) -> Error {
    Error::tool(
        ToolErrorKind::InvalidInput,
        APPLY_PATCH_TOOL_NAME,
        error.message().to_owned(),
    )
    .with_source(error)
}

fn patch_failure(error: SandboxError) -> Error {
    let kind = match error.error_code() {
        ErrorCode::ApplyPatchInvalidPath | ErrorCode::ApplyPatchInvalidDiff => {
            ToolErrorKind::InvalidInput
        }
        _ => ToolErrorKind::ExecutionFailed,
    };
    Error::tool(kind, APPLY_PATCH_TOOL_NAME, error.message().to_owned()).with_source(error)
}

#[async_trait]
impl Tool for SandboxApplyPatchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_approval(self.needs_approval.policy())
            .with_concurrency(ToolConcurrency::Parallel)
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        if matches!(self.needs_approval, PatchApproval::Never) {
            return Ok(false);
        }
        let Some(raw_input) = context.arguments().as_str() else {
            return Ok(false);
        };
        // A patch that does not parse, or leaves the workspace, runs straight into its failure so
        // the model can correct it; stopping the run to ask about it would ask about nothing.
        let Ok(operations) = self.parse_custom_input(raw_input) else {
            return Ok(false);
        };
        let mut normalized = Vec::with_capacity(operations.len());
        for operation in &operations {
            match self.normalize_operation(operation) {
                Ok(operation) => normalized.push(operation),
                Err(error) if error.error_code() == ErrorCode::ApplyPatchInvalidPath => {
                    return Ok(false);
                }
                Err(error) => return Err(patch_failure(error)),
            }
        }
        for operation in &normalized {
            if self.needs_approval.evaluate(context, operation).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let raw_input = context.arguments().as_str().ok_or_else(|| {
            parse_failure(PatchParseError::new("apply_patch input must be a string"))
        })?;
        self.run(raw_input).await.map(ToolOutput::text)
    }
}
