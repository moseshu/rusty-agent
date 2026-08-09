//! Model response -> `ProcessedResponse` classification (R3-2).
//!
//! This stage does exactly one thing: bind every call the model made to the object that will
//! answer it. It executes nothing, approves nothing, and decides no control flow — those are R3-4's
//! and R3-4b's jobs, and keeping them out is what stops the runner from becoming a place where
//! provider decoding, approval, execution, and output parsing all happen in one function.
//!
//! Classification is also **non-destructive**: `new_items` carries the adapter's records unchanged,
//! including a handoff that arrived in its wire form as an ordinary tool call. The typed view lives
//! on the action, so the session keeps what the provider actually sent and replay stays exact.

use std::sync::Arc;

use ra_core::{
    error::{Error, Result},
    item::{ModelResponse, RunItemKind},
    step::ProcessedResponse,
};

use super::prepare::TurnActionSurface;

/// Classifies one model response against the surface the turn advertised.
///
/// Name resolution order is handoff, then tool, then unresolved. The order is not a tie-break:
/// [`TurnActionSurface::new`] already rejected a surface where one name means two things, so at
/// most one branch can match.
pub fn process_model_response(
    response: &ModelResponse,
    surface: &TurnActionSurface,
) -> Result<ProcessedResponse> {
    let mut builder = ProcessedResponse::builder();

    for item in response.output() {
        builder = match item.kind() {
            RunItemKind::ToolCall(call) => {
                if let Some(handoff) = surface.find_handoff(call.name()) {
                    builder.handoff(item.clone(), handoff.target_agent().clone())?
                } else if let Some(tool) = surface.find_tool(call.name()) {
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
                if !surface.advertises_handoff_to(call.target_agent()) {
                    return Err(Error::caller(format!(
                        "the response hands off to agent `{}`, which this turn did not advertise",
                        call.target_agent()
                    )));
                }
                builder.handoff(item.clone(), call.target_agent().clone())?
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
            _ => builder.item(item.clone()),
        };
    }

    builder.build()
}
