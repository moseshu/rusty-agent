// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Asking the user one to three short questions and waiting for the answers.
//!
//! Ported from Codex's `request_user_input` tool (`core/src/tools/handlers/request_user_input.rs`
//! and `request_user_input_spec.rs`, with `request_user_input_available_modes` from
//! `tools/src/tool_config.rs`): the same name, schema, description, argument normalization,
//! model-facing refusals and output. The questions go to the [`UserInputPort`] the run was given
//! through [`ToolServices`](ra_core::tool::ToolServices), and the call waits until the answers come
//! back; the model is given them as Codex gives them, the response serialized as JSON.
//!
//! # Modes
//!
//! Codex offers the tool by collaboration mode: only Plan mode may use it, unless its
//! `default_mode_request_user_input` feature also lets Default mode; a call in any other mode is
//! refused, and a call in Plan mode tells the host the user's work waits on the answer. The tool
//! here is built with the modes it is available in (see [`request_user_input_available_modes`]).
//!
//! # Differences from Codex
//!
//! - The host binds the collaboration mode when constructing the tool, rather than reading a
//!   Codex turn context. The default is Default mode; Plan requests are marked blocking.
//! - A run whose host installed no port is told the tool is not enabled, with a text of Rusty's
//!   own; a Codex session can always raise the request.
//! - The root-thread check reads [`ra_core::context::RunContext::parent_run_id`]: an agent-tool call's nested run and
//!   a spawned agent's run are the counterparts of Codex's sub-agent sessions.
//! - Codex's Guardian reviewer keeps the answers as verified context; Guardian is not ported.

use std::fmt;

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    tool::{
        Tool, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
    user_input::{RequestUserInputArgs, RequestUserInputQuestion, UserInputPort},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The advertised name: Codex's `REQUEST_USER_INPUT_TOOL_NAME`.
pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

/// What a run whose host can ask nobody is told.
const NOT_ENABLED: &str = "request_user_input is not enabled for this run";

/// A collaboration mode: Codex's `ModeKind`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModeKind {
    /// Planning before the work: the mode Codex offers this tool in.
    Plan,
    /// Doing the work.
    #[default]
    Default,
}

impl ModeKind {
    /// The name the model is told: Codex's `display_name`.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Default => "Default",
        }
    }

    /// Whether the mode offers this tool without a feature enabling it: Codex's
    /// `allows_request_user_input`.
    #[must_use]
    pub const fn allows_request_user_input(self) -> bool {
        matches!(self, Self::Plan)
    }
}

/// The modes a user can pick: Codex's `TUI_VISIBLE_COLLABORATION_MODES`.
pub const TUI_VISIBLE_COLLABORATION_MODES: [ModeKind; 2] = [ModeKind::Default, ModeKind::Plan];

/// The modes the tool is available in: Codex's `request_user_input_available_modes`, with its
/// `default_mode_request_user_input` feature as `default_mode_enabled`.
#[must_use]
pub fn request_user_input_available_modes(default_mode_enabled: bool) -> Vec<ModeKind> {
    TUI_VISIBLE_COLLABORATION_MODES
        .into_iter()
        .filter(|mode| {
            mode.allows_request_user_input() || (default_mode_enabled && *mode == ModeKind::Default)
        })
        .collect()
}

/// The model-facing description: Codex's `request_user_input_tool_description`.
#[must_use]
pub fn request_user_input_tool_description(available_modes: &[ModeKind]) -> String {
    let allowed_modes = format_allowed_modes(available_modes);
    format!(
        "Request user input for one to three short questions and wait for the response. This tool is only available in {allowed_modes}."
    )
}

fn format_allowed_modes(available_modes: &[ModeKind]) -> String {
    let mode_names: Vec<&str> = available_modes
        .iter()
        .map(|mode| mode.display_name())
        .collect();
    match mode_names.as_slice() {
        [] => "no modes".to_owned(),
        [mode] => format!("{mode} mode"),
        [first, second] => format!("{first} or {second} mode"),
        [..] => format!("modes: {}", mode_names.join(",")),
    }
}

/// Why a call in `mode` is refused, if it is: Codex's `request_user_input_unavailable_message`.
#[must_use]
pub fn request_user_input_unavailable_message(
    mode: ModeKind,
    available_modes: &[ModeKind],
) -> Option<String> {
    if available_modes.contains(&mode) {
        None
    } else {
        Some(format!(
            "request_user_input is unavailable in {} mode",
            mode.display_name()
        ))
    }
}

/// The arguments: Codex's `RequestUserInputToolArgs`.
#[derive(Debug, Deserialize)]
struct RequestUserInputToolArgs {
    questions: Vec<RequestUserInputQuestion>,
}

/// Every question needs options, and the user may always answer otherwise: Codex's
/// `normalize_request_user_input_tool_args`.
fn normalize_request_user_input_tool_args(
    args: RequestUserInputToolArgs,
) -> std::result::Result<Vec<RequestUserInputQuestion>, String> {
    let missing_options = args
        .questions
        .iter()
        .any(|question| question.options().is_none_or(<[_]>::is_empty));
    if missing_options {
        return Err("request_user_input requires non-empty options for every question".to_owned());
    }
    Ok(args
        .questions
        .into_iter()
        .map(|question| question.with_is_other(true))
        .collect())
}

/// The input schema: Codex's `create_request_user_input_tool`.
fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Stable identifier for mapping answers (snake_case).",
                        },
                        "header": {
                            "type": "string",
                            "description": "Short header label shown in the UI (12 or fewer chars).",
                        },
                        "question": {
                            "type": "string",
                            "description": "Single-sentence prompt shown to the user.",
                        },
                        "options": {
                            "type": "array",
                            "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "description": "User-facing label (1-5 words).",
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "One short sentence explaining impact/tradeoff if selected.",
                                    },
                                },
                                "required": ["label", "description"],
                                "additionalProperties": false,
                            },
                        },
                    },
                    "required": ["id", "header", "question", "options"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["questions"],
        "additionalProperties": false,
    })
}

/// The `request_user_input` tool: Codex's `RequestUserInputHandler`.
#[derive(Debug)]
pub struct RequestUserInputTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    available_modes: Vec<ModeKind>,
    mode: ModeKind,
}

impl RequestUserInputTool {
    /// The tool, available in `available_modes` (see [`request_user_input_available_modes`]).
    ///
    /// It runs alone, as Codex's tools do unless they declare parallel calls, and reaches nothing
    /// but the user.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(available_modes: Vec<ModeKind>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(REQUEST_USER_INPUT_TOOL_NAME)?,
            schema: ToolSchema::loose(REQUEST_USER_INPUT_TOOL_NAME, parameters())?
                .with_description(request_user_input_tool_description(&available_modes)),
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_action_classes([crate::action_classes::READ]),
            available_modes,
            mode: ModeKind::Default,
        })
    }

    /// The modes the tool is available in.
    #[must_use]
    pub fn available_modes(&self) -> &[ModeKind] {
        &self.available_modes
    }

    /// Binds the effective collaboration mode for this tool's agent.
    /// Mode selection does not change sandbox authority or root-thread restrictions.
    #[must_use]
    pub const fn with_mode(mut self, mode: ModeKind) -> Self {
        self.mode = mode;
        self
    }

    async fn ask(&self, context: &ToolContext<'_>) -> std::result::Result<ToolOutput, String> {
        if context.run().parent_run_id().is_some() {
            return Err("request_user_input can only be used by the root thread".to_owned());
        }
        let mode = self.mode;
        if let Some(message) = request_user_input_unavailable_message(mode, &self.available_modes) {
            return Err(message);
        }
        let args: RequestUserInputToolArgs = serde_json::from_value(context.arguments().clone())
            .map_err(|error| format!("failed to parse function arguments: {error}"))?;
        let questions = normalize_request_user_input_tool_args(args)?;
        let args = RequestUserInputArgs::new(questions, mode == ModeKind::Plan);
        let port: &dyn UserInputPort = context
            .services()
            .user_input()
            .ok_or_else(|| NOT_ENABLED.to_owned())?
            .as_ref();
        let response = port
            .request_user_input(context.run(), context.call_id(), args)
            .await
            .ok_or_else(|| {
                format!("{REQUEST_USER_INPUT_TOOL_NAME} was cancelled before receiving a response")
            })?;
        serde_json::to_string(&response)
            .map(ToolOutput::text)
            .map_err(|error| {
                format!("failed to serialize {REQUEST_USER_INPUT_TOOL_NAME} response: {error}")
            })
    }
}

/// A refusal whose text is what the model is shown, as Codex's `RespondToModel`.
#[derive(Debug)]
struct RequestUserInputFailure(String);

impl fmt::Display for RequestUserInputFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RequestUserInputFailure {}

#[async_trait]
impl Tool for RequestUserInputTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.ask(&context).await.map_err(|message| {
            Error::tool(
                ToolErrorKind::InvalidInput,
                REQUEST_USER_INPUT_TOOL_NAME,
                message.clone(),
            )
            .with_source(RequestUserInputFailure(message))
        })
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Control signals stop or close out the run rather than becoming an observation.
        if error.is_control_signal() {
            return Ok(None);
        }
        Ok(std::error::Error::source(error)
            .and_then(<dyn std::error::Error + 'static>::downcast_ref::<RequestUserInputFailure>)
            .map(|failure| ToolOutput::text(failure.0.clone())))
    }
}
