//! Final-output validation and explicit recovery, shared by ordinary and streamed runs.

use super::{
    AgentBinding, CancelScope, Error, FinishReason, Message, MessageRole, NextStep, OutputPhase,
    OutputSchema, OutputValue, ProviderErrorKind, Result, RunErrorData, RunErrorHandlerInput,
    RunErrorHandlerResult, RunItem, RunState, TurnLoopContext, TurnLoopProgress,
    find_final_message, live_context,
};

pub(super) fn schema<'a>(
    context: &TurnLoopContext<'a>,
    agent: &'a AgentBinding,
) -> &'a OutputSchema {
    context
        .output_schema
        .unwrap_or_else(|| agent.public().output_schema())
}

/// Projects final text under a run-level schema without validating it.
///
/// A run-level declaration is the Codex turn setting, which Codex only forwards to the provider;
/// the reference has no run-level output type to port a validation contract from. Parseable JSON
/// becomes the JSON projection (unwrapped when the declaration wraps), and anything else stays
/// text. Such a run never fails or continues because of its final output's shape.
fn project_unvalidated(schema: &OutputSchema, text: &str) -> OutputValue {
    if schema.is_plain_text() {
        return OutputValue::text(text);
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut value) => {
            if schema.is_wrapped()
                && let Some(inner) = value
                    .as_object_mut()
                    .and_then(|object| object.remove(ra_core::output::OUTPUT_WRAPPER_KEY))
            {
                value = inner;
            }
            OutputValue::json(value)
        }
        Err(_) => OutputValue::text(text),
    }
}

/// Decodes final text under the effective declaration: validated for an agent's declaration,
/// projected as is for a run-level one.
fn decode_final(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    text: &str,
) -> Result<OutputValue> {
    match context.output_schema {
        Some(run_schema) => Ok(project_unvalidated(run_schema, text)),
        None => agent.public().output_schema().validate_json(text),
    }
}

pub(super) fn validate_handler_message(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    message: &Message,
) -> Result<OutputValue> {
    if message.role() != MessageRole::Assistant || message.phase() != Some(OutputPhase::Final) {
        return Err(Error::caller(
            "a run error handler must return an assistant message with final output phase",
        ));
    }
    decode_final(context, agent, &message.text_content()).map_err(|error| {
        Error::caller("Invalid run error handler final_output for structured output")
            .with_source(error)
    })
}

/// Calls the run's error handler on `scope`.
///
/// A budget closeout passes the sibling closeout scope: a deadline cancels the run scope, and an
/// exhausted budget must still get its chance to close out, while an interrupt on the caller's
/// scope cancels both. Final-output recovery happens inside the run and uses the run scope, so a
/// deadline stops it like any other turn work.
pub(super) async fn call_handler(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    progress: &TurnLoopProgress,
    state: &RunState,
    error: &Error,
    extra_items: &[RunItem],
    scope: &CancelScope,
) -> Result<Option<RunErrorHandlerResult>> {
    let Some(handler) = context.config.error_handler.as_ref() else {
        return Ok(None);
    };
    let (base, carried) = if state.handoff_projection().is_some() {
        state.model_input_base()?
    } else {
        (context.input_base, progress.segment_items(state))
    };
    let mut items = carried.to_vec();
    items.extend_from_slice(extra_items);
    let data = RunErrorData::new(
        agent.public(),
        base,
        &items,
        progress.segment_responses(state),
        progress.turns,
        state.budget().clone(),
        context.spend.run_usage(),
    );
    let run = live_context(context, agent, state);
    let result = scope
        .run(handler.handle(RunErrorHandlerInput::new(error, data, &run)))
        .await??;
    scope.ensure_not_cancelled()?;
    Ok(result)
}

/// Produces the final value before stop hooks and output checks; tool promotion bypasses the
/// model-output decoder, as the reference does. Empty structured output continues when recovery
/// declines. Invalid nonempty output fails without a retry or replaying completed tools.
pub(super) async fn resolve(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    progress: &mut TurnLoopProgress,
    state: &RunState,
    settled: ra_core::step::SingleStepResult,
) -> Result<(ra_core::step::SingleStepResult, Option<(Message, bool)>)> {
    let NextStep::FinalOutput { reason } = settled.next_step() else {
        return Ok((settled, None));
    };
    let schema = schema(context, agent);
    if *reason == FinishReason::ToolStop {
        let output = settled.final_output().cloned().ok_or_else(|| {
            Error::caller("tool-stop settlement did not retain its selected output")
        })?;
        progress.final_output = Some(if schema.is_plain_text() {
            OutputValue::text(output.text_content())
        } else {
            output
        });
        return Ok((settled, None));
    }
    let message = find_final_message(settled.session_step_items());
    let refusal = message.and_then(Message::refusal_content);
    let text = message.map(Message::text_content).unwrap_or_default();
    // Only an agent's declaration owes an answer; a run-level schema is forwarded, not enforced.
    let missing = context.output_schema.is_none() && !schema.is_plain_text() && text.is_empty();
    let parsed = if let Some(refusal) = refusal {
        Err(Error::provider(ProviderErrorKind::Refusal, refusal))
    } else if missing {
        Err(Error::invalid_final_output(
            "Model returned no final output for the structured output type",
        ))
    } else {
        decode_final(context, agent, &text)
    };
    match parsed {
        Ok(output) => {
            progress.final_output = Some(output);
            Ok((settled, None))
        }
        Err(error)
            if matches!(
                &error,
                Error::InvalidFinalOutput { .. }
                    | Error::Provider {
                        kind: ProviderErrorKind::Refusal,
                        ..
                    }
            ) =>
        {
            let recovered = call_handler(
                context,
                agent,
                progress,
                state,
                &error,
                settled.session_step_items(),
                context.cancel,
            )
            .await?;
            let Some(recovered) = recovered else {
                return if missing && matches!(&error, Error::InvalidFinalOutput { .. }) {
                    Ok((settled.continue_empty_output()?, None))
                } else {
                    Err(error)
                };
            };
            let output = validate_handler_message(context, agent, recovered.message())?;
            progress.final_output = Some(output);
            progress.final_message = Some(recovered.message().clone());
            progress.final_message_in_history = recovered.write_to_history();
            Ok((
                settled,
                Some((recovered.message().clone(), recovered.write_to_history())),
            ))
        }
        Err(error) => Err(error),
    }
}
