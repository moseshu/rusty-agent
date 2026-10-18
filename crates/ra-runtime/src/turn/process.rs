//! Model response -> `ProcessedResponse` classification (R3-2).
//!
//! This stage does exactly one thing: bind every call the model made to the object that will
//! answer it. It executes nothing, approves nothing, and decides no control flow — those are R3-4's
//! and R3-4b's jobs, and keeping them out is what stops the runner from becoming a place where
//! provider decoding, approval, execution, and output parsing all happen in one function.
//!
//! Classification preserves each provider payload while attaching the display metadata known at
//! item creation. A handoff that arrived as an ordinary tool call retains that payload; its typed
//! view lives on the action. Display metadata stays outside the model-input projection.

use std::sync::Arc;

use ra_core::{
    error::{Error, Result},
    item::{ModelResponse, RunItem, RunItemKind, ToolCallKind},
    step::ProcessedResponse,
};

use super::prepare::TurnActionSurface;

/// Classifies one model response against the surface the turn advertised.
///
/// Name resolution order is handoff, then tool, then unresolved. Preparation retains at most one
/// action for each name under its collision policy, so at most one branch can match.
pub fn process_model_response(
    response: &ModelResponse,
    surface: &TurnActionSurface,
) -> Result<ProcessedResponse> {
    process_model_response_with_history(response, surface, &[])
}

pub(crate) fn process_model_response_with_history(
    response: &ModelResponse,
    surface: &TurnActionSurface,
    existing_items: &[RunItem],
) -> Result<ProcessedResponse> {
    let metadata = super::mcp_metadata::collect(existing_items, response.output());
    let mut builder = ProcessedResponse::builder();

    for item in response.output() {
        builder = match item.kind() {
            // A call within a namespace can only be a namespace's tool; handoffs are top-level.
            RunItemKind::ToolCall(call) => {
                if call.kind() == ToolCallKind::ToolSearch {
                    if let Some(tool) = surface.find_tool_search() {
                        builder.function(item.clone(), Arc::clone(tool))?
                    } else {
                        builder.tool_not_found(item.clone())?
                    }
                } else if let Some(handoff) = call
                    .namespace()
                    .is_none()
                    .then(|| surface.find_handoff(call.name()))
                    .flatten()
                {
                    builder.handoff(
                        item.clone(),
                        handoff.spec().clone(),
                        Arc::clone(handoff.target()),
                    )?
                } else if let Some(tool) = surface.find_tool_in(call.namespace(), call.name()) {
                    builder.function(item.clone(), Arc::clone(tool))?
                } else {
                    // Not an error. The model naming a tool that is gone this turn is ordinary,
                    // and the answer is a failure observation it can read, not a dead run.
                    builder.tool_not_found(item.clone())?
                }
            }
            // Already-typed handoffs come from an adapter that knew the handoff table, or from a
            // restored state. Either way the turn's own surface is the authority on what was on
            // offer: a transfer to an agent this turn never advertised is a control transfer
            // nobody authorised, and running it would be worse than refusing it.
            RunItemKind::HandoffCall(call) => {
                let resolved = if let Some(name) = call.tool_name() {
                    surface.find_handoff(name)
                } else {
                    let mut candidates = surface
                        .handoffs()
                        .iter()
                        .filter(|handoff| handoff.target_agent() == call.target_agent());
                    let first = candidates.next();
                    if candidates.next().is_some() {
                        return Err(Error::caller(format!(
                            "the handoff to agent `{}` is ambiguous without a tool name",
                            call.target_agent()
                        )));
                    }
                    first
                };
                let Some(handoff) = resolved else {
                    return Err(Error::caller(format!(
                        "the response hands off to agent `{}`, which this turn did not advertise",
                        call.target_agent()
                    )));
                };
                builder.handoff(
                    item.clone(),
                    handoff.spec().clone(),
                    Arc::clone(handoff.target()),
                )?
            }
            RunItemKind::McpApprovalRequest(_) => builder.mcp_approval(item.clone())?,
            // Messages, reasoning, outputs, control-plane records, and — because `RunItemKind` is
            // `#[non_exhaustive]` — any kind a newer `ra-core` adds. Recording without binding is
            // the right default for all of them: inventing an action for a kind this build does
            // not understand is a guess, while keeping the record costs nothing and keeps the
            // session complete.
            //
            // A `ToolApproval` landing here is not being ignored. It reaches
            // `ProcessedResponse::interruptions` through `RunItemKind::is_interruption`, whose
            // match in `ra-core` **is** exhaustive — that is where a new kind has to declare
            // whether it is a pending decision, and it is a compile error there rather than a
            // silent pass here.
            _ => builder.item(super::mcp_metadata::enrich(item.clone(), &metadata)),
        };
    }

    builder.build()
}
