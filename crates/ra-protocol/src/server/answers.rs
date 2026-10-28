// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
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
//! Codex's handlers also complete the item of a call that was not approved, with a status their
//! answer gives. Here an answer also carries how it settled the approval
//! ([`ApprovalResolution`]), which the thread records for the product to complete the call's item
//! from (see [`super::approval_requests`]).

use super::{
    approvals::ReviewDecision,
    image_url::{REMOTE_IMAGE_URL_ERROR, is_remote_image_url},
    outgoing::{ClientAnswer, is_turn_transition_server_request_error},
    protocol::{
        CommandExecutionApprovalDecision, CommandExecutionRequestApprovalResponse,
        DynamicToolCallOutputContentItem, DynamicToolCallResponse, FileChangeApprovalDecision,
        FileChangeRequestApprovalResponse, GrantedPermissionProfile, NetworkPolicyRuleAction,
        PermissionGrantScope, PermissionsRequestApprovalResponse, ToolRequestUserInputResponse,
    },
};
use crate::frame::JsonRpcErrorObject;
use ra_core::event::ApprovalResolution;

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

/// An approval's decision, and how it settled the approval: the completion status Codex's
/// handlers give the item of a call that was not approved, as a resolution the thread records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalAnswer {
    pub(crate) decision: ReviewDecision,
    pub(crate) resolution: ApprovalResolution,
    mcp_tool_approval: bool,
}

impl ApprovalAnswer {
    const fn new(decision: ReviewDecision, resolution: ApprovalResolution) -> Self {
        Self {
            decision,
            resolution,
            mcp_tool_approval: false,
        }
    }

    /// No answer could be obtained: Codex's refusal with `approval request failed`.
    pub(crate) fn failed() -> Self {
        Self::new(deny(APPROVAL_REQUEST_FAILED), ApprovalResolution::Failed)
    }
    pub(crate) fn run_answer(&self) -> super::approvals::RunAnswer {
        if self.mcp_tool_approval {
            match self.decision {
                ReviewDecision::ApprovedMcpPolicyAmendment => {
                    return super::approvals::RunAnswer::Approve;
                }
                // Codex skips a cancelled MCP call and returns its result to the model.
                ReviewDecision::Abort => {
                    return super::approvals::RunAnswer::Reject(
                        "user cancelled MCP tool call".into(),
                    );
                }
                _ => {}
            }
        }
        self.decision.run_answer()
    }
}

fn parse_mcp_tool_approval_response(
    response: Option<&ToolRequestUserInputResponse>,
    question_id: &str,
) -> ReviewDecision {
    let answers = response
        .and_then(|response| response.answers().get(question_id))
        .map(super::protocol::ToolRequestUserInputAnswer::answers);
    match answers {
        Some(answers)
            if answers
                .iter()
                .any(|answer| answer == "Allow for this session") =>
        {
            ReviewDecision::ApprovedForSession
        }
        Some(answers)
            if answers
                .iter()
                .any(|answer| answer == "Allow and don't ask me again") =>
        {
            ReviewDecision::ApprovedMcpPolicyAmendment
        }
        Some(answers) if answers.iter().any(|answer| answer == "Allow") => ReviewDecision::Approved,
        _ => ReviewDecision::Abort,
    }
}

/// Source MCP tool-approval response parsing. Prompt/Writes never inherit a remember answer.
pub(crate) fn mcp_tool_approval_answer(
    answer: ClientAnswer,
    question_id: &str,
    elicitation: bool,
    remember: bool,
) -> Option<ApprovalAnswer> {
    use super::protocol::{McpServerElicitationAction, McpServerElicitationRequestResponse};
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => Some(value),
        Outcome::TurnTransition => return None,
        Outcome::Failed => None,
    };
    let mut decision = if elicitation {
        match value.and_then(|value| {
            serde_json::from_value::<McpServerElicitationRequestResponse>(value).ok()
        }) {
            Some(response) => match response.action {
                McpServerElicitationAction::Accept => match response
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("persist"))
                    .and_then(serde_json::Value::as_str)
                {
                    Some("session") => ReviewDecision::ApprovedForSession,
                    Some("always") => ReviewDecision::ApprovedMcpPolicyAmendment,
                    _ => {
                        let converted = response
                            .content
                            .as_ref()
                            .and_then(serde_json::Value::as_object)
                            .map(|content| {
                                content
                                    .iter()
                                    .filter_map(|(id, value)| {
                                        let answers = match value {
                                            serde_json::Value::String(answer) => {
                                                vec![answer.clone()]
                                            }
                                            serde_json::Value::Array(answers) => answers
                                                .iter()
                                                .filter_map(|answer| {
                                                    answer.as_str().map(str::to_owned)
                                                })
                                                .collect(),
                                            _ => return None,
                                        };
                                        Some((id.clone(), serde_json::json!({"answers":answers})))
                                    })
                                    .collect::<serde_json::Map<_, _>>()
                            });
                        let parsed = parse_mcp_tool_approval_response(
                            converted
                                .and_then(|answers| {
                                    serde_json::from_value::<ToolRequestUserInputResponse>(
                                        serde_json::json!({"answers":answers}),
                                    )
                                    .ok()
                                })
                                .as_ref(),
                            question_id,
                        );
                        if matches!(parsed, ReviewDecision::Abort) {
                            ReviewDecision::Approved
                        } else {
                            parsed
                        }
                    }
                },
                McpServerElicitationAction::Decline => {
                    ReviewDecision::denied("user rejected MCP tool call")
                }
                McpServerElicitationAction::Cancel => ReviewDecision::Abort,
            },
            // The source app-server maps malformed/failed elicitation responses to Decline.
            None => ReviewDecision::denied("user rejected MCP tool call"),
        }
    } else {
        parse_mcp_tool_approval_response(
            value
                .and_then(|value| {
                    serde_json::from_value::<ToolRequestUserInputResponse>(value).ok()
                })
                .as_ref(),
            question_id,
        )
    };
    if !remember
        && matches!(
            decision,
            ReviewDecision::ApprovedForSession | ReviewDecision::ApprovedMcpPolicyAmendment
        )
    {
        decision = ReviewDecision::Approved;
    }
    let resolution = match decision {
        ReviewDecision::Abort => ApprovalResolution::Cancelled,
        ReviewDecision::Denied { .. } => ApprovalResolution::Declined,
        _ => ApprovalResolution::Approved,
    };
    let mut answer = ApprovalAnswer::new(decision, resolution);
    answer.mcp_tool_approval = true;
    Some(answer)
}

/// The decision a command approval's answer carries: Codex's
/// `on_command_execution_request_approval_response`.
pub(crate) fn command_execution_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
    command_execution_approval_answer(answer).map(|answer| answer.decision)
}

/// The decision a command approval's answer carries, and how it settled the approval: Codex's
/// `on_command_execution_request_approval_response`, whose completion status is `declined` for a
/// refusal, a cancellation or a denied network rule, and `failed` when no answer was obtained.
pub(crate) fn command_execution_approval_answer(answer: ClientAnswer) -> Option<ApprovalAnswer> {
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(ApprovalAnswer::failed()),
    };
    let response = match serde_json::from_value::<CommandExecutionRequestApprovalResponse>(value) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                "failed to deserialize CommandExecutionRequestApprovalResponse: {error}"
            );
            return Some(ApprovalAnswer::failed());
        }
    };
    let approved = |decision| ApprovalAnswer::new(decision, ApprovalResolution::Approved);
    Some(match response.into_decision() {
        CommandExecutionApprovalDecision::Accept => approved(ReviewDecision::Approved),
        CommandExecutionApprovalDecision::AcceptForSession => {
            approved(ReviewDecision::ApprovedForSession)
        }
        CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
            execpolicy_amendment,
        } => approved(ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment: execpolicy_amendment,
        }),
        CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
            network_policy_amendment,
        } => {
            let resolution = match network_policy_amendment.action() {
                NetworkPolicyRuleAction::Allow => ApprovalResolution::Approved,
                NetworkPolicyRuleAction::Deny => ApprovalResolution::Declined,
            };
            ApprovalAnswer::new(
                ReviewDecision::NetworkPolicyAmendment {
                    network_policy_amendment,
                },
                resolution,
            )
        }
        CommandExecutionApprovalDecision::Decline => {
            ApprovalAnswer::new(deny(REJECTED_BY_USER), ApprovalResolution::Declined)
        }
        CommandExecutionApprovalDecision::Cancel => {
            ApprovalAnswer::new(ReviewDecision::Abort, ApprovalResolution::Cancelled)
        }
    })
}

/// The decision a file change approval's answer carries: Codex's
/// `on_file_change_request_approval_response` and `map_file_change_approval_decision`.
pub(crate) fn file_change_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
    file_change_approval_answer(answer).map(|answer| answer.decision)
}

/// The decision a file change approval's answer carries, and how it settled the approval: Codex's
/// `on_file_change_request_approval_response`.
pub(crate) fn file_change_approval_answer(answer: ClientAnswer) -> Option<ApprovalAnswer> {
    let value = match Outcome::from(answer) {
        Outcome::Answered(value) => value,
        Outcome::TurnTransition => return None,
        Outcome::Failed => return Some(ApprovalAnswer::failed()),
    };
    let response = match serde_json::from_value::<FileChangeRequestApprovalResponse>(value) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!("failed to deserialize FileChangeRequestApprovalResponse: {error}");
            return Some(ApprovalAnswer::failed());
        }
    };
    Some(match response.decision() {
        FileChangeApprovalDecision::Accept => {
            ApprovalAnswer::new(ReviewDecision::Approved, ApprovalResolution::Approved)
        }
        FileChangeApprovalDecision::AcceptForSession => ApprovalAnswer::new(
            ReviewDecision::ApprovedForSession,
            ApprovalResolution::Approved,
        ),
        FileChangeApprovalDecision::Decline => {
            ApprovalAnswer::new(deny(REJECTED_BY_USER), ApprovalResolution::Declined)
        }
        FileChangeApprovalDecision::Cancel => {
            ApprovalAnswer::new(ReviewDecision::Abort, ApprovalResolution::Cancelled)
        }
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
