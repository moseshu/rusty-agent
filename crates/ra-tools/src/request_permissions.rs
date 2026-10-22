//! Asking the user for file system or network permissions beyond the sandbox, and waiting for
//! what the client grants.
//!
//! Ported from Codex's `request_permissions` tool (`core/src/tools/handlers/request_permissions.rs`,
//! with its schema and description from `shell_spec.rs`): the same name, schema and description.
//! What a call asks for, whom it asks and what a grant covers are the product's: Codex reads them
//! from the turn's environment, approval policy and permission profile, which a host supplies here
//! as a [`RequestPermissionsPolicy`]. The tool only carries the call to it and the answer back.
//!
//! Codex registers the tool only behind its `request_permissions_tool` feature, which is off by
//! default; a host installs it here when it wants it.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    tool::{
        Tool, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use serde_json::{Value, json};

/// The advertised name.
pub const REQUEST_PERMISSIONS_TOOL_NAME: &str = "request_permissions";

/// The model-facing description: Codex's `request_permissions_tool_description`.
pub const REQUEST_PERMISSIONS_TOOL_DESCRIPTION: &str = "Request additional filesystem or network permissions from the user and wait for the client to grant a subset of the requested permission profile. Use environment_id to target a specific attached environment; omit it to use the primary environment. Relative filesystem paths resolve against the selected environment cwd. Granted permissions apply automatically to later shell-like commands in the current turn, or for the rest of the session if the client approves them at session scope.";

/// What a host does with a `request_permissions` call: the body of Codex's handler and the session
/// call it makes.
#[async_trait]
pub trait RequestPermissionsPolicy: Send + Sync {
    /// Asks for what the call's arguments request and waits for the grant.
    ///
    /// # Errors
    ///
    /// Returns the text the model is shown when the call is refused, as Codex's `RespondToModel`.
    async fn request_permissions(
        &self,
        context: &ToolContext<'_>,
    ) -> std::result::Result<String, String>;
}

/// The input schema: Codex's `create_request_permissions_tool`.
fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "reason": {
                "type": "string",
                "description": "Optional short explanation for why additional permissions are needed.",
            },
            "environment_id": {
                "type": "string",
                "description": "Environment id from <environment_context>. Omit to use the primary environment.",
            },
            "permissions": {
                "type": "object",
                "description": "Filesystem or network access request.",
                "properties": {
                    "network": {
                        "type": "object",
                        "description": "Network access request.",
                        "properties": {
                            "enabled": {
                                "type": "boolean",
                                "description": "True requests network access; false or omitted requests none.",
                            },
                        },
                        "additionalProperties": false,
                    },
                    "file_system": {
                        "type": "object",
                        "description": "Filesystem access request.",
                        "properties": {
                            "read": {
                                "type": "array",
                                "description": "Absolute paths to grant read access; omit when none are needed.",
                                "items": {"type": "string"},
                            },
                            "write": {
                                "type": "array",
                                "description": "Absolute paths to grant write access; omit when none are needed.",
                                "items": {"type": "string"},
                            },
                        },
                        "additionalProperties": false,
                    },
                },
                "additionalProperties": false,
            },
        },
        "required": ["permissions"],
        "additionalProperties": false,
    })
}

/// The `request_permissions` tool: Codex's `RequestPermissionsHandler`.
pub struct RequestPermissionsTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    policy: Arc<dyn RequestPermissionsPolicy>,
}

impl fmt::Debug for RequestPermissionsTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestPermissionsTool")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl RequestPermissionsTool {
    /// The tool, handing its calls to `policy`.
    ///
    /// It runs alone, as Codex's tools do unless they declare parallel calls. Asking grants nothing
    /// by itself; what is granted applies to later calls through the host's own policies.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(policy: Arc<dyn RequestPermissionsPolicy>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(REQUEST_PERMISSIONS_TOOL_NAME)?,
            schema: ToolSchema::loose(REQUEST_PERMISSIONS_TOOL_NAME, parameters())?
                .with_description(REQUEST_PERMISSIONS_TOOL_DESCRIPTION),
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_action_classes([crate::action_classes::READ]),
            policy,
        })
    }
}

/// A refusal whose text is what the model is shown, as Codex's `RespondToModel`.
#[derive(Debug)]
struct RequestPermissionsFailure(String);

impl fmt::Display for RequestPermissionsFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RequestPermissionsFailure {}

#[async_trait]
impl Tool for RequestPermissionsTool {
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
        self.policy
            .request_permissions(&context)
            .await
            .map(ToolOutput::text)
            .map_err(|message| {
                Error::tool(
                    ToolErrorKind::InvalidInput,
                    REQUEST_PERMISSIONS_TOOL_NAME,
                    message.clone(),
                )
                .with_source(RequestPermissionsFailure(message))
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
            .and_then(<dyn std::error::Error + 'static>::downcast_ref::<RequestPermissionsFailure>)
            .map(|failure| ToolOutput::text(failure.0.clone())))
    }
}
