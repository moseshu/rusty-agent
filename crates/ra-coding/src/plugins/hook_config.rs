// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Typed plugin hook declarations only; parsing never executes a handler.

use ra_core::hook::HookEventName;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// A plugin hook file or inline declaration.
pub struct HooksFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional presentation text.
    pub description: Option<String>,
    #[serde(default)]
    /// The declared handlers or event groups.
    pub hooks: HookEventsToml,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// Hook matcher groups, preserving upstream JSON event names and defaults.
pub struct HookEventsToml {
    #[serde(rename = "PreToolUse", default)]
    /// Matcher groups for the `pre_tool_use` event.
    pub pre_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PermissionRequest", default)]
    /// Matcher groups for the `permission_request` event.
    pub permission_request: Vec<MatcherGroup>,
    #[serde(rename = "PostToolUse", default)]
    /// Matcher groups for the `post_tool_use` event.
    pub post_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PreCompact", default)]
    /// Matcher groups for the `pre_compact` event.
    pub pre_compact: Vec<MatcherGroup>,
    #[serde(rename = "PostCompact", default)]
    /// Matcher groups for the `post_compact` event.
    pub post_compact: Vec<MatcherGroup>,
    #[serde(rename = "SessionStart", default)]
    /// Matcher groups for the `session_start` event.
    pub session_start: Vec<MatcherGroup>,
    #[serde(rename = "SessionEnd", default)]
    /// Matcher groups for the `session_end` event.
    pub session_end: Vec<MatcherGroup>,
    #[serde(rename = "UserPromptSubmit", default)]
    /// Matcher groups for the `user_prompt_submit` event.
    pub user_prompt_submit: Vec<MatcherGroup>,
    #[serde(rename = "SubagentStart", default)]
    /// Matcher groups for the `subagent_start` event.
    pub subagent_start: Vec<MatcherGroup>,
    #[serde(rename = "SubagentStop", default)]
    /// Matcher groups for the `subagent_stop` event.
    pub subagent_stop: Vec<MatcherGroup>,
    #[serde(rename = "Stop", default)]
    /// Matcher groups for the `stop` event.
    pub stop: Vec<MatcherGroup>,
    #[serde(rename = "Interrupt", default)]
    /// Matcher groups for the `interrupt` event.
    pub interrupt: Vec<MatcherGroup>,
}

impl HookEventsToml {
    /// Whether every event has no matcher groups.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let Self {
            pre_tool_use,
            permission_request,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        } = self;
        pre_tool_use.is_empty()
            && permission_request.is_empty()
            && post_tool_use.is_empty()
            && pre_compact.is_empty()
            && post_compact.is_empty()
            && session_start.is_empty()
            && session_end.is_empty()
            && user_prompt_submit.is_empty()
            && subagent_start.is_empty()
            && subagent_stop.is_empty()
            && stop.is_empty()
            && interrupt.is_empty()
    }

    /// The total number of declared handlers.
    #[must_use]
    pub fn handler_count(&self) -> usize {
        let Self {
            pre_tool_use,
            permission_request,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        } = self;
        [
            pre_tool_use,
            permission_request,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        ]
        .into_iter()
        .flatten()
        .map(|group| group.hooks.len())
        .sum()
    }

    /// Consumes all matcher groups in upstream event order.
    #[must_use]
    pub fn into_matcher_groups(mut self) -> [(HookEventName, Vec<MatcherGroup>); 12] {
        self.matcher_groups_mut()
            .map(|(event, groups)| (event, std::mem::take(groups)))
    }

    /// Borrows matcher groups in upstream event order.
    pub fn matcher_groups_mut(&mut self) -> [(HookEventName, &mut Vec<MatcherGroup>); 12] {
        use HookEventName as Event;

        [
            (Event::PreToolUse, &mut self.pre_tool_use),
            (Event::PermissionRequest, &mut self.permission_request),
            (Event::PostToolUse, &mut self.post_tool_use),
            (Event::PreCompact, &mut self.pre_compact),
            (Event::PostCompact, &mut self.post_compact),
            (Event::SessionStart, &mut self.session_start),
            (Event::SessionEnd, &mut self.session_end),
            (Event::UserPromptSubmit, &mut self.user_prompt_submit),
            (Event::SubagentStart, &mut self.subagent_start),
            (Event::SubagentStop, &mut self.subagent_stop),
            (Event::Stop, &mut self.stop),
            (Event::Interrupt, &mut self.interrupt),
        ]
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// Handlers sharing an optional event matcher.
pub struct MatcherGroup {
    #[serde(default)]
    /// The optional event matcher.
    pub matcher: Option<String>,
    #[serde(default)]
    /// The declared handlers or event groups.
    pub hooks: Vec<HookHandlerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
/// A declared handler, without an installed runtime or execution authority.
pub enum HookHandlerConfig {
    #[serde(rename = "command")]
    /// A shell command declaration.
    Command {
        /// The command text.
        command: String,
        #[serde(default, rename = "commandWindows", alias = "command_windows")]
        /// An optional Windows-specific command.
        command_windows: Option<String>,
        #[serde(default, rename = "timeout")]
        /// The optional handler timeout in seconds.
        timeout_sec: Option<u64>,
        #[serde(default)]
        /// Whether the command is declared asynchronous.
        r#async: bool,
        #[serde(default, rename = "statusMessage")]
        /// Optional progress text.
        status_message: Option<String>,
        /// Approximate token threshold for spilling this hook's `additionalContext` to disk.
        /// Unset uses 2,500 tokens; `0` disables spilling for this hook. The threshold is
        /// evaluated against the original context; a spilled preview also includes recovery
        /// metadata.
        #[serde(
            default,
            rename = "additionalContextLimit",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context_limit: Option<usize>,
    },
    #[serde(rename = "mcp_tool")]
    /// An MCP tool declaration.
    McpTool {
        /// The MCP server name.
        server: String,
        /// The original MCP tool name.
        tool: String,
        #[serde(default, deserialize_with = "deserialize_mcp_tool_input")]
        /// TOML-representable JSON arguments.
        input: serde_json::Map<String, serde_json::Value>,
        #[serde(default, rename = "timeout")]
        /// The optional handler timeout in seconds.
        timeout_sec: Option<u64>,
        #[serde(default, rename = "statusMessage")]
        /// Optional progress text.
        status_message: Option<String>,
    },
    #[serde(rename = "prompt")]
    /// A prompt handler declaration.
    Prompt {},
    #[serde(rename = "agent")]
    /// An agent handler declaration.
    Agent {},
}

// Reject values such as null that cannot be represented in TOML for trust hashing.
fn deserialize_mcp_tool_input<'de, D>(
    deserializer: D,
) -> Result<serde_json::Map<String, serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let input = serde_json::Map::deserialize(deserializer)?;
    toml::Value::try_from(&input).map_err(|error| {
        serde::de::Error::custom(format!(
            "MCP hook input must be representable as TOML: {error}"
        ))
    })?;
    Ok(input)
}
