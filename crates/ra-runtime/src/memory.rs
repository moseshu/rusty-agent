//! Final-citation feedback over persisted tool evidence. Retrieval never waits for this sink.

use crate::runner::RunResult;
use ra_core::{
    item::RunItemKind,
    memory::{MemoryExposure, MemoryUsage, MemoryUsageSink},
    state::RunId,
    tool::ToolOutput,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

pub(crate) async fn report_final_citations(
    result: &RunResult,
    sink: &Arc<dyn MemoryUsageSink>,
    run_id: &RunId,
) {
    let Some(message) = result.final_message() else {
        return;
    };
    // Budget closeout messages that are not persisted model deliveries have no citation event.
    let Some(final_item) = result.new_items().iter().rev().find(
        |item| matches!(item.kind(), RunItemKind::Message(candidate) if candidate == message),
    ) else {
        return;
    };
    let exposed = result
        .state()
        .memory_exposures()
        .iter()
        .map(|evidence| (evidence.token().to_owned(), evidence.clone()))
        .collect::<BTreeMap<_, _>>();
    let text = message.text_content();
    let mut seen = BTreeSet::new();
    let citations = text
        .split("[[memory:")
        .skip(1)
        .filter_map(|rest| {
            let (token, _) = rest.split_once("]]")?;
            let evidence = exposed.get(token)?;
            seen.insert(token.to_owned()).then(|| evidence.clone())
        })
        .collect::<Vec<_>>();
    if citations.is_empty() {
        return;
    }
    let usage = MemoryUsage::new(
        run_id.clone(),
        result.last_agent().id().clone(),
        final_item.id().clone(),
        citations,
    );
    // A remote accounting outage cannot withhold the final answer. Hosts needing durable delivery
    // implement this port as a local outbox and deduplicate by run, final item, and citation token.
    match tokio::time::timeout(Duration::from_millis(100), sink.record_usage(usage)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "memory citation feedback delivery failed"),
        Err(_) => tracing::warn!("memory citation feedback delivery timed out"),
    }
}

/// Reads evidence from the final model-input projection, restricted to authoritative local outputs.
/// A trimmer that replaces a tool body with an excerpt does not authorize new memory evidence.
pub(crate) fn request_exposures(
    request: &ra_core::model::ModelRequest,
    state: &ra_core::state::RunState,
) -> Vec<MemoryExposure> {
    let trusted = state
        .generated_items()
        .iter()
        .filter_map(|item| {
            let RunItemKind::ToolCallOutput(output) = item.kind() else {
                return None;
            };
            (!output.is_error()).then_some((output.call_id(), output))
        })
        .collect::<BTreeMap<_, _>>();
    request
        .input()
        .iter()
        .filter_map(|item| {
            let ra_core::item::ModelInputItem::ToolCallOutput(output) = item else {
                return None;
            };
            if output.is_error() || trusted.get(output.call_id()).copied() != Some(output) {
                return None;
            }
            let output = ToolOutput::from_stored(output.output()).ok()??;
            if output.model_excerpt().is_some() {
                return None;
            }
            Some(output.metadata().memory_exposures().to_vec())
        })
        .flatten()
        .collect()
}
