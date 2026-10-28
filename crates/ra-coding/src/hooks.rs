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
    pub use super::engine::discovery::{DiscoveryResult, discover_handlers};

    pub use super::output_spill::{AdditionalContext, AdditionalContextLimit, HookOutputSpiller};

    pub use super::engine::command_runner::{
        MAX_CONCURRENT_ASYNC_HOOKS, build_command, default_shell_program, run_command,
    };
    pub use super::engine::{
        ClaudeHooksEngine, CommandHookRuntime, CommandShell, ConfiguredHandler,
        ConfiguredHandlerKind, HandlerRunResult, HandlerSourcePath,
    };

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

mod declarations;
pub use declarations::{PluginHookDeclaration, plugin_hook_declarations};

/// Stable persisted key for one hook handler in its declared source group.
#[must_use]
pub fn hook_key(
    key_source: &str,
    event_name: ra_core::hook::HookEventName,
    group_index: usize,
    handler_index: usize,
) -> String {
    format!(
        "{key_source}:{}:{group_index}:{handler_index}",
        event_name.as_str()
    )
}

mod config_rules;
mod engine;
mod events;
mod legacy_notify;
mod mcp;
mod output_spill;
mod registry;
mod types;
mod user_hook_bridge;
pub use config_rules::hook_states_from_stack;
pub use engine::dispatcher::{hook_execution_mode_label, hook_handler_type_label};
pub use engine::{HookListEntry, HookListEntryHandler};
pub use events::common::SubagentHookContext;
pub use events::compact::{
    PostCompactRequest, PreCompactOutcome, PreCompactRequest, StatelessHookOutcome,
};
pub use events::interrupt::{InterruptOutcome, InterruptRequest};
pub use events::permission_request::{
    PermissionRequestDecision, PermissionRequestOutcome, PermissionRequestRequest,
};
pub use events::post_tool_use::{PostToolUseOutcome, PostToolUseRequest};
pub use events::pre_tool_use::{PreToolUseOutcome, PreToolUseRequest};
pub use events::session_end::{SessionEndOutcome, SessionEndRequest};
pub use events::session_start::{
    SessionStartOutcome, SessionStartRequest, SessionStartSource, StartHookTarget,
};
pub use events::stop::{StopHookTarget, StopOutcome, StopRequest};
pub use events::user_prompt_submit::{UserPromptSubmitOutcome, UserPromptSubmitRequest};
pub use mcp::{HookMcpCall, HookMcpExecutor};
pub use registry::{HookListOutcome, Hooks, HooksConfig, list_hooks};
pub use types::{
    Hook, HookEvent, HookEventAfterAgent, HookFn, HookPayload, HookResponse, HookResult,
};
pub use user_hook_bridge::{ProductHookRuntime, ProductHookSession, UnappliedHookEffect};
/// Stable event label used in persisted handler state.
#[must_use]
pub fn hook_event_key_label(event: ra_core::hook::HookEventName) -> &'static str {
    event.as_str()
}
/// Model-facing text attached to one completed hook run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookPromptFragment {
    /// The continuation text.
    pub text: String,
    /// The hook run that produced it.
    pub hook_run_id: String,
}

/// Hook event names as they appear in hooks JSON and config files.
pub const HOOK_EVENT_NAMES: [&str; 12] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "Interrupt",
];

/// Hook event names whose matcher fields are meaningful during dispatch.
///
/// Other events can appear in hooks JSON, but Codex ignores their matcher
/// fields because those events do not dispatch against a tool, compaction
/// trigger, session-start source, or session-end reason.
pub const HOOK_EVENT_NAMES_WITH_MATCHERS: [&str; 9] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
];

mod schema;
pub use schema::write_schema_fixtures;

impl HookPromptFragment {
    /// Associates model-facing text with the hook run that produced it.
    pub fn from_single_hook(text: impl Into<String>, hook_run_id: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            hook_run_id: hook_run_id.into(),
        }
    }
}
