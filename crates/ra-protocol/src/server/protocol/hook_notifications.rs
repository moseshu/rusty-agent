// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The `hook/started` and `hook/completed` notifications: Codex's `HookStartedNotification` and
//! `HookCompletedNotification` (`app-server-protocol/src/protocol/v2/hook.rs`), with the hook run
//! summary in its client-facing camelCase form.
//!
//! The product reports its hook runs with the `snake_case` records in [`super::hooks`]; the server
//! converts them here when it tells a thread's clients, as Codex's app server converts its core
//! events.

#![allow(missing_docs, reason = "Retain source hook notification wire records.")]

use ra_paths::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::hooks as core;

/// The method of the notification that a hook run started.
pub const HOOK_STARTED_METHOD: &str = "hook/started";

/// The method of the notification that a hook run completed.
pub const HOOK_COMPLETED_METHOD: &str = "hook/completed";

macro_rules! notification_enum_from_core {
    (
        $(#[$enum_meta:meta])*
        pub enum $Name:ident from $Src:path {
            $( $Variant:ident ),+ $(,)?
        }
    ) => {
        #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
        $(#[$enum_meta])*
        #[serde(rename_all = "camelCase")]
        pub enum $Name {
            $( $Variant ),+
        }

        impl From<$Src> for $Name {
            fn from(value: $Src) -> Self {
                match value { $( <$Src>::$Variant => Self::$Variant ),+ }
            }
        }
    };
}

/// A hook event, as the client names it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HookEventName {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
    Interrupt,
}

impl HookEventName {
    /// The client name of `event`, for the events Codex reports.
    #[must_use]
    pub fn from_core(event: ra_core::hook::HookEventName) -> Option<Self> {
        use ra_core::hook::HookEventName as Core;
        Some(match event {
            Core::PreToolUse => Self::PreToolUse,
            Core::PermissionRequest => Self::PermissionRequest,
            Core::PostToolUse => Self::PostToolUse,
            Core::PreCompact => Self::PreCompact,
            Core::PostCompact => Self::PostCompact,
            Core::SessionStart => Self::SessionStart,
            Core::SessionEnd => Self::SessionEnd,
            Core::UserPromptSubmit => Self::UserPromptSubmit,
            Core::SubagentStart => Self::SubagentStart,
            Core::SubagentStop => Self::SubagentStop,
            Core::Stop => Self::Stop,
            Core::Interrupt => Self::Interrupt,
            _ => return None,
        })
    }
}

notification_enum_from_core!(
    pub enum HookHandlerType from core::HookHandlerType {
        Command, McpTool, Prompt, Agent
    }
);

notification_enum_from_core!(
    pub enum HookExecutionMode from core::HookExecutionMode {
        Sync, Async
    }
);

notification_enum_from_core!(
    pub enum HookScope from core::HookScope {
        Thread, Turn
    }
);

notification_enum_from_core!(
    pub enum HookSource from core::HookSource {
        System,
        User,
        Project,
        Mdm,
        SessionFlags,
        Plugin,
        CloudRequirements,
        CloudManagedConfig,
        LegacyManagedConfigFile,
        LegacyManagedConfigMdm,
        Unknown,
    }
);

notification_enum_from_core!(
    pub enum HookRunStatus from core::HookRunStatus {
        Running, Completed, Failed, Blocked, Stopped
    }
);

notification_enum_from_core!(
    pub enum HookOutputEntryKind from core::HookOutputEntryKind {
        Warning, Stop, Feedback, Context, Error
    }
);

fn default_hook_source() -> HookSource {
    HookSource::Unknown
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookOutputEntry {
    pub kind: HookOutputEntryKind,
    pub text: String,
}

impl From<core::HookOutputEntry> for HookOutputEntry {
    fn from(value: core::HookOutputEntry) -> Self {
        Self {
            kind: value.kind.into(),
            text: value.text,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookRunSummary {
    pub id: String,
    pub event_name: HookEventName,
    pub handler_type: HookHandlerType,
    pub execution_mode: HookExecutionMode,
    pub scope: HookScope,
    pub source_path: AbsolutePathBuf,
    #[serde(default = "default_hook_source")]
    pub source: HookSource,
    pub display_order: i64,
    pub status: HookRunStatus,
    pub status_message: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub entries: Vec<HookOutputEntry>,
}

impl HookRunSummary {
    /// The client form of `run`; `None` for an event Codex does not report.
    #[must_use]
    pub fn from_core(run: core::HookRunSummary) -> Option<Self> {
        Some(Self {
            id: run.id,
            event_name: HookEventName::from_core(run.event_name)?,
            handler_type: run.handler_type.into(),
            execution_mode: run.execution_mode.into(),
            scope: run.scope.into(),
            source_path: run.source_path,
            source: run.source.into(),
            display_order: run.display_order,
            status: run.status.into(),
            status_message: run.status_message,
            started_at: run.started_at,
            completed_at: run.completed_at,
            duration_ms: run.duration_ms,
            entries: run.entries.into_iter().map(Into::into).collect(),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookStartedNotification {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookCompletedNotification {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}
