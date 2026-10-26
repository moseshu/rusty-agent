// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host command-hook parsing and identity primitives following the upstream product contract.
//!
//! These supplied-value operations do not discover, trust or execute installed plugin handlers.

pub mod matcher;
pub mod output_parser;
mod output_wire;
pub mod timing;
pub mod trust;

/// Access to private wire schemas for fixed upstream fixture verification.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::output_wire::{
        InterruptCommandOutputWire, PermissionRequestCommandOutputWire,
        PostCompactCommandOutputWire, PostToolUseCommandOutputWire, PreCompactCommandOutputWire,
        PreToolUseCommandOutputWire, SessionStartCommandOutputWire, StopCommandOutputWire,
        SubagentStartCommandOutputWire, SubagentStopCommandOutputWire,
        UserPromptSubmitCommandOutputWire,
    };
    use schemars::{JsonSchema, r#gen::SchemaSettings};
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn schema<T: JsonSchema>() -> Result<Value, serde_json::Error> {
        serde_json::to_value(
            SchemaSettings::draft07()
                .with(|settings| {
                    settings.option_add_null_type = false;
                })
                .into_generator()
                .into_root_schema_for::<T>(),
        )
    }

    /// Generates the original output schemas without exposing internal wire records.
    pub fn output_schemas() -> Result<BTreeMap<&'static str, Value>, serde_json::Error> {
        Ok(BTreeMap::from([
            (
                "pre-tool-use.command.output.schema.json",
                schema::<PreToolUseCommandOutputWire>()?,
            ),
            (
                "permission-request.command.output.schema.json",
                schema::<PermissionRequestCommandOutputWire>()?,
            ),
            (
                "post-tool-use.command.output.schema.json",
                schema::<PostToolUseCommandOutputWire>()?,
            ),
            (
                "pre-compact.command.output.schema.json",
                schema::<PreCompactCommandOutputWire>()?,
            ),
            (
                "post-compact.command.output.schema.json",
                schema::<PostCompactCommandOutputWire>()?,
            ),
            (
                "session-start.command.output.schema.json",
                schema::<SessionStartCommandOutputWire>()?,
            ),
            (
                "subagent-start.command.output.schema.json",
                schema::<SubagentStartCommandOutputWire>()?,
            ),
            (
                "user-prompt-submit.command.output.schema.json",
                schema::<UserPromptSubmitCommandOutputWire>()?,
            ),
            (
                "stop.command.output.schema.json",
                schema::<StopCommandOutputWire>()?,
            ),
            (
                "subagent-stop.command.output.schema.json",
                schema::<SubagentStopCommandOutputWire>()?,
            ),
            (
                "interrupt.command.output.schema.json",
                schema::<InterruptCommandOutputWire>()?,
            ),
        ]))
    }
}
