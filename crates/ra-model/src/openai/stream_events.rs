//! Lift the text subset shared by Responses and the synthesized Chat event stream.

use ra_core::{
    item::{
        ContentBlock, ItemId, Message, MessageRole, OutputPhase, Reasoning, RunItem, RunItemKind,
    },
    model::{ModelStreamDelta, ModelStreamDeltaKind, ModelStreamEvent, RunItemStreamEvent},
};
use serde_json::Value;

const ASSISTANT_ROLE: &str = "assistant"; // layering-allow: assistant = OpenAI wire message role

pub(super) fn text_delta(
    event_type: &str,
    frame: &Value,
    item_id: ItemId,
) -> Option<ModelStreamDelta> {
    let kind = match event_type {
        "response.output_text.delta" => ModelStreamDeltaKind::OutputText {
            delta: frame.get("delta")?.as_str()?.to_owned(),
        },
        "response.reasoning_summary_text.delta" => ModelStreamDeltaKind::ReasoningSummaryText {
            delta: frame.get("delta")?.as_str()?.to_owned(),
            summary_index: frame.get("summary_index")?.as_i64()?,
        },
        "response.reasoning_summary_part.added" => {
            ModelStreamDeltaKind::ReasoningSummaryPartAdded {
                summary_index: frame.get("summary_index")?.as_i64()?,
            }
        }
        "response.reasoning_text.delta" => ModelStreamDeltaKind::ReasoningText {
            delta: frame.get("delta")?.as_str()?.to_owned(),
            content_index: frame.get("content_index")?.as_i64()?,
        },
        _ => return None,
    };
    Some(ModelStreamDelta::new(item_id, kind))
}

/// Incomplete items carry only displayable content; replay material belongs to their final form.
pub(super) fn text_item_added(item: &Value, item_id: ItemId) -> Option<ModelStreamEvent> {
    let kind = match item.get("type")?.as_str()? {
        "message" => {
            if item.get("role").and_then(Value::as_str) != Some(ASSISTANT_ROLE) {
                return None;
            }
            let blocks = texts(item.get("content"))
                .into_iter()
                .map(ContentBlock::text)
                .collect();
            let mut message = Message::new(MessageRole::Assistant, blocks);
            if let Some(phase) = item.get("phase").and_then(Value::as_str) {
                message = match phase {
                    "commentary" => message.with_phase(OutputPhase::Commentary),
                    "final_answer" => message.with_phase(OutputPhase::Final),
                    _ => message,
                };
            }
            RunItemKind::Message(message)
        }
        "reasoning" => RunItemKind::Reasoning(
            Reasoning::new()
                .with_summary(texts(item.get("summary")))
                .with_content(texts(item.get("content"))),
        ),
        _ => return None,
    };
    Some(ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new(
        kind.label(),
        RunItem::new(item_id, kind),
    )))
}

fn texts(parts: Option<&Value>) -> Vec<String> {
    parts
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}
