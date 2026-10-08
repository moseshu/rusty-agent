//! What the server makes of the client's answers to its requests: the decoding in Codex's handlers
//! of each request (`app-server/src/bespoke_event_handling.rs`, `app-server/src/dynamic_tools.rs`).
//!
//! A request withdrawn because its turn moved on needs nothing passed on, so it reads as `None`.
//! Any other failure — the client answered with an error, the answer does not fit the method, or
//! the request was withdrawn without one — reads as the refusal Codex falls back to, never as an
//! approval.
//!
//! An approval becomes Codex's [`ReviewDecision`], preserving session scope, command-prefix rules
//! and network rules for the coding runtime. It is not reduced to the framework's whole-tool
//! approval. The other answers are passed on as the method's own response.
//!
//! Codex's handlers also complete a declined command's item and update the thread's status while
//! a request waits; both come with the thread and item notifications.

use super::{
    approvals::ReviewDecision,
    image_url::{REMOTE_IMAGE_URL_ERROR, is_remote_image_url},
    outgoing::{ClientAnswer, is_turn_transition_server_request_error},
    protocol::{
        CommandExecutionApprovalDecision, CommandExecutionRequestApprovalResponse,
        DynamicToolCallOutputContentItem, DynamicToolCallResponse, FileChangeApprovalDecision,
        FileChangeRequestApprovalResponse, GrantedPermissionProfile, PermissionGrantScope,
        PermissionsRequestApprovalResponse, ToolRequestUserInputResponse,
    },
};
use crate::frame::JsonRpcErrorObject;

/// What the model is told of an approval the user refused: Codex's rejection text.
pub(crate) const REJECTED_BY_USER: &str = "rejected by user";
/// What the model is told of an approval that could not be obtained: Codex's rejection text.
pub(crate) const APPROVAL_REQUEST_FAILED: &str = "approval request failed";
/// Codex's `INVALID_AUDIO_URL_ERROR`.
pub(crate) const INVALID_AUDIO_URL_ERROR: &str = "audio URLs must use an inline data URL";

/// How a request ended, before its answer is read.
enum Outcome {
    /// The client answered with this value.
    Answered(serde_json::Value),
    /// The server withdrew the request because its turn moved on.
    TurnTransition,
    /// The client answered with an error, or the request was withdrawn without an answer.
    Failed,
}

impl From<ClientAnswer> for Outcome {
    fn from(answer: ClientAnswer) -> Self {
        match answer {
            Ok(Ok(value)) => Self::Answered(value),
            Ok(Err(error)) if is_turn_transition_server_request_error(&error) => {
                Self::TurnTransition
            }
            Ok(Err(error)) => {
                log_client_error(&error);
                Self::Failed
            }
            Err(error) => {
                tracing::error!("request failed: {error:?}");
                Self::Failed
            }
        }
    }
}

fn log_client_error(error: &JsonRpcErrorObject) {
    // Codex logs the whole error here; its message and data come from the client.
    tracing::error!(code = error.code(), "request failed with client error");
}

fn deny(message: &str) -> ReviewDecision {
    ReviewDecision::denied(message)
}

/// The decision a command approval's answer carries: Codex's
/// `on_command_execution_request_approval_response`.
pub(crate) fn command_execution_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(deny(APPROVAL_REQUEST_FAILED)),
    };
    let response = match serde_json::from_value::<CommandExecutionRequestApprovalResponse>(value) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                "failed to deserialize CommandExecutionRequestApprovalResponse: {error}"
            );
            return Some(deny(APPROVAL_REQUEST_FAILED));
        }
    };
    Some(match response.into_decision() {
        CommandExecutionApprovalDecision::Accept => ReviewDecision::Approved,
        CommandExecutionApprovalDecision::AcceptForSession => ReviewDecision::ApprovedForSession,
        CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
            execpolicy_amendment,
        } => ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment: execpolicy_amendment,
        },
        CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
            network_policy_amendment,
        } => ReviewDecision::NetworkPolicyAmendment {
            network_policy_amendment,
        },
        CommandExecutionApprovalDecision::Decline => deny(REJECTED_BY_USER),
        CommandExecutionApprovalDecision::Cancel => ReviewDecision::Abort,
    })
}

/// The decision a file change approval's answer carries: Codex's
/// `on_file_change_request_approval_response` and `map_file_change_approval_decision`.
pub(crate) fn file_change_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(deny(APPROVAL_REQUEST_FAILED)),
    };
    let response = match serde_json::from_value::<FileChangeRequestApprovalResponse>(value) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!("failed to deserialize FileChangeRequestApprovalResponse: {error}");
            return Some(deny(APPROVAL_REQUEST_FAILED));
        }
    };
    Some(match response.decision() {
        FileChangeApprovalDecision::Accept => ReviewDecision::Approved,
        FileChangeApprovalDecision::AcceptForSession => ReviewDecision::ApprovedForSession,
        FileChangeApprovalDecision::Decline => deny(REJECTED_BY_USER),
        FileChangeApprovalDecision::Cancel => ReviewDecision::Abort,
    })
}

/// The user's answers: Codex's `on_request_user_input_response`. Without a usable answer the
/// questions are answered with nothing.
pub(crate) fn request_user_input_response(
    answer: ClientAnswer,
) -> Option<ToolRequestUserInputResponse> {
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(ToolRequestUserInputResponse::new()),
    };
    Some(
        serde_json::from_value::<ToolRequestUserInputResponse>(value).unwrap_or_else(|error| {
            tracing::error!("failed to deserialize ToolRequestUserInputResponse: {error}");
            ToolRequestUserInputResponse::new()
        }),
    )
}

/// The permissions granted: Codex's `request_permissions_response_from_client_result`. Without a
/// usable answer nothing is granted, for the turn.
///
/// Codex also turns the granted paths into its sandbox's own, and interrupts the turn if one
/// cannot be; the runtime has no permission grants yet to turn them into.
pub(crate) fn request_permissions_response(
    answer: ClientAnswer,
) -> Option<PermissionsRequestApprovalResponse> {
    let nothing = || PermissionsRequestApprovalResponse::new(GrantedPermissionProfile::new());
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(nothing()),
    };
    let response = serde_json::from_value::<PermissionsRequestApprovalResponse>(value)
        .unwrap_or_else(|error| {
            tracing::error!("failed to deserialize PermissionsRequestApprovalResponse: {error}");
            nothing()
        });
    if response.strict_auto_review() == Some(true)
        && response.scope() == PermissionGrantScope::Session
    {
        tracing::error!("strict auto review is only supported for turn-scoped permission grants");
        return Some(nothing());
    }
    Some(response)
}

/// What the dynamic tool returned: Codex's `dynamic_tools::on_call_response`. Without a usable
/// answer the call failed, and the model is told why.
pub(crate) fn dynamic_tool_call_response(answer: ClientAnswer) -> Option<DynamicToolCallResponse> {
    match Outcome::from(answer) {
        Outcome::Answered(value) => Some(decode_dynamic_tool_call_response(value)),
        Outcome::TurnTransition => None,
        Outcome::Failed => Some(dynamic_tool_fallback("dynamic tool request failed")),
    }
}

/// Codex's `dynamic_tools::decode_response`.
fn decode_dynamic_tool_call_response(value: serde_json::Value) -> DynamicToolCallResponse {
    match serde_json::from_value::<DynamicToolCallResponse>(value) {
        Ok(response)
            if response.content_items().iter().any(|item| {
                matches!(
                    item,
                    DynamicToolCallOutputContentItem::InputImage { image_url, .. }
                        if is_remote_image_url(image_url)
                )
            }) =>
        {
            tracing::error!(
                message = REMOTE_IMAGE_URL_ERROR,
                "dynamic tool response was invalid"
            );
            dynamic_tool_fallback(REMOTE_IMAGE_URL_ERROR)
        }
        Ok(response)
            if response.content_items().iter().any(|item| {
                matches!(
                    item,
                    DynamicToolCallOutputContentItem::InputAudio { audio_url, .. }
                        if !audio_url
                            .get(.."data:".len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
                )
            }) =>
        {
            tracing::error!(
                message = INVALID_AUDIO_URL_ERROR,
                "dynamic tool response was invalid"
            );
            dynamic_tool_fallback(INVALID_AUDIO_URL_ERROR)
        }
        Ok(response) => response,
        Err(error) => {
            tracing::error!("failed to deserialize DynamicToolCallResponse: {error}");
            dynamic_tool_fallback("dynamic tool response was invalid")
        }
    }
}

/// Codex's `dynamic_tools::fallback_response`.
fn dynamic_tool_fallback(message: &str) -> DynamicToolCallResponse {
    DynamicToolCallResponse::new(
        [DynamicToolCallOutputContentItem::input_text(message)],
        false,
    )
}
