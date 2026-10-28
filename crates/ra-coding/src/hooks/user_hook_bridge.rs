// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product hooks behind the runtime's host hook dispatcher.
//!
//! The source core builds each hook request from its session and turn state (`hook_runtime.rs`)
//! and applies the outcome itself. Here the runtime owns dispatch, so each supported event is
//! registered as a [`UserHook`] that builds the same request and returns the control effect the
//! dispatcher can apply: a `PreToolUse` block becomes a denial with the source's message, a
//! `PermissionRequest` decision becomes a grant or denial, and a Stop or `SubagentStop` block becomes
//! a continuation prompt.
//!
//! The runtime decision set has no channel for the other source effects: additional model
//! context, rewritten tool input, stop-processing requests and `PostToolUse` feedback. They are not
//! applied. Each one is kept as an [`UnappliedHookEffect`] and logged, so a host can see what a
//! hook asked for; this bridge does not claim them as delivered.
//!
//! Completed hook runs, including background handlers that finish later, are published on
//! [`ProductHookRuntime::subscribe_completed`]. Refreshing the configuration keeps running
//! background handlers, as the source's reconfiguration does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use ra_config::AskForApproval;
use ra_core::error::Result;
use ra_core::hook::{
    HookDecision, HookEvent, HookEventName, ToolHookData, UserHook, UserHookContext,
};
use ra_core::item::{MessageRole, ModelInputItem};
use ra_core::session::SessionId;
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::HookCompletedEvent;
use ra_runtime::hook::{UserHookRegistration, UserHooks};
use serde_json::Value;
use tokio::sync::broadcast;

use super::events::permission_request::{PermissionRequestDecision, PermissionRequestRequest};
use super::events::post_tool_use::PostToolUseRequest;
use super::events::pre_tool_use::PreToolUseRequest;
use super::events::session_end::SessionEndRequest;
use super::events::session_start::{SessionStartRequest, SessionStartSource, StartHookTarget};
use super::events::stop::{StopHookTarget, StopRequest};
use super::events::user_prompt_submit::UserPromptSubmitRequest;
use super::{Hooks, HooksConfig};

/// Session-level values the source copies into every hook request.
#[derive(Debug, Clone)]
pub struct ProductHookSession {
    /// Thread identity serialized as `session_id`.
    pub session_id: SessionId,
    /// Host-side working directory; hooks always run on the host.
    pub cwd: AbsolutePathBuf,
    /// Model slug serialized as `model`.
    pub model: String,
    /// Approval policy, mapped to the source's `permission_mode` labels.
    pub approval_policy: AskForApproval,
    /// Transcript file shown to hooks, when the host keeps one.
    pub transcript_path: Option<PathBuf>,
}

/// A hook result the runtime dispatcher cannot apply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnappliedHookEffect {
    /// Text the source would add to the model context.
    AdditionalContext {
        /// Event that produced it.
        event: HookEventName,
        /// The context fragments, in handler order.
        contexts: Vec<String>,
    },
    /// Replacement arguments for a `PreToolUse` call.
    UpdatedInput {
        /// Call the hook rewrote.
        tool_use_id: String,
        /// The rewritten arguments.
        input: Value,
    },
    /// A request to stop processing the turn or session.
    StopProcessing {
        /// Event that produced it.
        event: HookEventName,
        /// The hook's reason, when given.
        reason: Option<String>,
    },
    /// Model-facing feedback or a block after a tool already ran.
    PostToolUseFeedback {
        /// Call the feedback concerns.
        tool_use_id: String,
        /// The feedback message, when given.
        message: Option<String>,
    },
}

/// One thread's product hooks, adapted to the runtime's host hook dispatcher.
pub struct ProductHookRuntime {
    hooks: RwLock<Arc<Hooks>>,
    session: ProductHookSession,
    completed: broadcast::Sender<HookCompletedEvent>,
    unapplied: Mutex<Vec<UnappliedHookEffect>>,
    forwarder: tokio::task::JoinHandle<()>,
}

impl ProductHookRuntime {
    /// Builds hooks from `config` for one thread, as the source does when a session starts.
    ///
    /// # Errors
    ///
    /// When a required managed hook fails to load.
    pub fn new(
        config: HooksConfig,
        session: ProductHookSession,
        mcp_executor: Arc<dyn super::HookMcpExecutor>,
    ) -> anyhow::Result<Arc<Self>> {
        let (hooks, background) = Hooks::new(config, session.session_id.clone(), mcp_executor)?;
        let (completed, _) = broadcast::channel(256);
        let forward = completed.clone();
        let forwarder = tokio::spawn(async move {
            while let Ok(event) = background.recv().await {
                let _ = forward.send(event);
            }
        });
        Ok(Arc::new(Self {
            hooks: RwLock::new(Arc::new(hooks)),
            session,
            completed,
            unapplied: Mutex::new(Vec::new()),
            forwarder,
        }))
    }

    /// Applies a refreshed configuration while background handlers keep running.
    pub fn refresh(&self, config: HooksConfig) {
        let next = Arc::new(self.current().reconfigured(config));
        *self.hooks.write().unwrap_or_else(PoisonError::into_inner) = next;
    }

    /// The hooks used by the next dispatch.
    #[must_use]
    pub fn current(&self) -> Arc<Hooks> {
        self.hooks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Receives completed hook runs, including background handlers.
    #[must_use]
    pub fn subscribe_completed(&self) -> broadcast::Receiver<HookCompletedEvent> {
        self.completed.subscribe()
    }

    /// Removes and returns the effects recorded since the last call.
    pub fn take_unapplied_effects(&self) -> Vec<UnappliedHookEffect> {
        std::mem::take(
            &mut *self
                .unapplied
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Registrations for every event the product hooks handle.
    #[must_use]
    pub fn user_hooks(self: &Arc<Self>) -> UserHooks {
        [
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::UserPromptSubmit,
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
            HookEventName::Stop,
            HookEventName::SubagentStop,
        ]
        .into_iter()
        .fold(UserHooks::default(), |hooks, event| {
            hooks.with_hook(UserHookRegistration::new(
                event,
                Arc::new(ProductHook {
                    runtime: Arc::clone(self),
                }),
            ))
        })
    }

    fn publish(&self, events: Vec<HookCompletedEvent>) {
        for event in events {
            let _ = self.completed.send(event);
        }
    }

    fn record(&self, effect: UnappliedHookEffect) {
        tracing::warn!(
            ?effect,
            "hook effect is not applied by the runtime dispatcher"
        );
        self.unapplied
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(effect);
    }

    fn record_contexts(&self, event: HookEventName, contexts: Vec<String>) {
        if !contexts.is_empty() {
            self.record(UnappliedHookEffect::AdditionalContext { event, contexts });
        }
    }

    fn permission_mode(&self) -> String {
        hook_permission_mode(self.session.approval_policy)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One arm per event, as the source hook runtime has one function per event."
    )]
    async fn dispatch(&self, context: &UserHookContext<'_>, event: &HookEvent<'_>) -> HookDecision {
        let hooks = self.current();
        let session = &self.session;
        let turn_id = context.run().run_id().to_string();
        match *event {
            HookEvent::SessionStart { resumed, .. } => {
                let request = SessionStartRequest {
                    session_id: session.session_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: session.transcript_path.clone(),
                    model: session.model.clone(),
                    permission_mode: self.permission_mode(),
                    target: StartHookTarget::SessionStart {
                        source: if resumed {
                            SessionStartSource::Resume
                        } else {
                            SessionStartSource::Startup
                        },
                    },
                };
                let outcome = hooks.run_session_start(request, None).await;
                self.publish(outcome.hook_events);
                self.record_contexts(HookEventName::SessionStart, outcome.additional_contexts);
                if outcome.should_stop {
                    self.record(UnappliedHookEffect::StopProcessing {
                        event: HookEventName::SessionStart,
                        reason: outcome.stop_reason,
                    });
                }
                HookDecision::Continue
            }
            HookEvent::SessionEnd { .. } => {
                let outcome = hooks
                    .run_session_end(SessionEndRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                    })
                    .await;
                self.publish(outcome.hook_events);
                HookDecision::Continue
            }
            HookEvent::UserPromptSubmit { input } => {
                let outcome = hooks
                    .run_user_prompt_submit(UserPromptSubmitRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        prompt: user_prompt(input),
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(HookEventName::UserPromptSubmit, outcome.additional_contexts);
                if outcome.should_stop {
                    self.record(UnappliedHookEffect::StopProcessing {
                        event: HookEventName::UserPromptSubmit,
                        reason: outcome.stop_reason,
                    });
                }
                HookDecision::Continue
            }
            HookEvent::PreToolUse(call) => {
                let tool = HookToolName::for_call(&call);
                let tool_use_id = call.call_id().to_string();
                let outcome = hooks
                    .run_pre_tool_use(PreToolUseRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name.clone(),
                        matcher_aliases: tool.matcher_aliases.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool_input: call.arguments().clone(),
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(HookEventName::PreToolUse, outcome.additional_contexts);
                if !outcome.should_block {
                    if let Some(input) = outcome.updated_input {
                        self.record(UnappliedHookEffect::UpdatedInput { tool_use_id, input });
                    }
                    return HookDecision::Continue;
                }
                let Some(reason) = outcome.block_reason else {
                    return HookDecision::Continue;
                };
                HookDecision::Deny {
                    message: blocked_tool_message(&tool, call.arguments(), &reason),
                }
            }
            HookEvent::PermissionRequest(call) => {
                let tool = HookToolName::for_call(&call);
                let outcome = hooks
                    .run_permission_request(PermissionRequestRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.to_path_buf(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name,
                        matcher_aliases: tool.matcher_aliases,
                        run_id_suffix: call.call_id().to_string(),
                        tool_input: call.arguments().clone(),
                    })
                    .await;
                self.publish(outcome.hook_events);
                match outcome.decision {
                    Some(PermissionRequestDecision::Allow) => HookDecision::Allow,
                    Some(PermissionRequestDecision::Deny { message }) => {
                        HookDecision::Deny { message }
                    }
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PostToolUse { call, output } => {
                let tool = HookToolName::for_call(&call);
                let tool_use_id = call.call_id().to_string();
                let tool_response = output.as_text().map_or_else(
                    || serde_json::to_value(output).unwrap_or(Value::Null),
                    |text| Value::String(text.to_owned()),
                );
                let outcome = hooks
                    .run_post_tool_use(PostToolUseRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name,
                        matcher_aliases: tool.matcher_aliases,
                        tool_use_id: tool_use_id.clone(),
                        tool_input: call.arguments().clone(),
                        tool_response,
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(HookEventName::PostToolUse, outcome.additional_contexts);
                if outcome.should_block || outcome.feedback_message.is_some() {
                    self.record(UnappliedHookEffect::PostToolUseFeedback {
                        tool_use_id,
                        message: outcome.feedback_message,
                    });
                }
                HookDecision::Continue
            }
            HookEvent::Stop(delivery) | HookEvent::SubagentStop { delivery, .. } => {
                let event_name = event.name();
                let target = if event_name == HookEventName::SubagentStop {
                    StopHookTarget::SubagentStop {
                        agent_id: context.run().run_id().to_string(),
                        agent_type: context.run().agent().name().to_owned(),
                        agent_transcript_path: session.transcript_path.clone(),
                    }
                } else {
                    StopHookTarget::Stop
                };
                let mut outcome = hooks
                    .run_stop(StopRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        request_metadata: None,
                        stop_hook_active: delivery.stop_hook_active(),
                        last_assistant_message: delivery
                            .message()
                            .map(ra_core::item::Message::text_content),
                        target,
                    })
                    .await;
                self.publish(std::mem::take(&mut outcome.hook_events));
                if outcome.should_stop {
                    self.record(UnappliedHookEffect::StopProcessing {
                        event: event_name,
                        reason: outcome.stop_reason.clone(),
                    });
                }
                if !outcome.should_block {
                    return HookDecision::Continue;
                }
                // The source records each fragment as a hook-prompt item keyed by its run; the
                // dispatcher takes one plain continuation prompt, so the texts are joined in order.
                let prompt = outcome
                    .continuation_fragments
                    .iter()
                    .filter(|fragment| !fragment.hook_run_id.trim().is_empty())
                    .map(|fragment| fragment.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                HookDecision::Block { prompt }
            }
            _ => HookDecision::Continue,
        }
    }
}

impl Drop for ProductHookRuntime {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

struct ProductHook {
    runtime: Arc<ProductHookRuntime>,
}

#[async_trait]
impl UserHook for ProductHook {
    fn name(&self) -> &'static str {
        "product hooks"
    }

    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        Ok(self.runtime.dispatch(context, event).await)
    }
}

/// The source's hook-facing tool identity: the serialized name and extra matcher inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HookToolName {
    name: String,
    matcher_aliases: Vec<String>,
}

impl HookToolName {
    fn for_call(call: &ToolHookData<'_>) -> Self {
        let origin = call.origin();
        match origin.name() {
            // Shell-like tools keep the historical hook name.
            "exec_command" | "shell" | "shell_command" | "local_shell"
                if origin.namespace().is_none() =>
            {
                Self {
                    name: "Bash".to_owned(),
                    matcher_aliases: Vec::new(),
                }
            }
            // `Write` and `Edit` select the same handlers for edit-oriented hook configurations.
            "apply_patch" if origin.namespace().is_none() => Self {
                name: "apply_patch".to_owned(),
                matcher_aliases: vec!["Write".to_owned(), "Edit".to_owned()],
            },
            "spawn_agent" if origin.namespace().is_none() => Self {
                name: "spawn_agent".to_owned(),
                matcher_aliases: vec!["Agent".to_owned()],
            },
            _ => Self {
                name: origin.qualified_name().to_owned(),
                matcher_aliases: Vec::new(),
            },
        }
    }
}

fn blocked_tool_message(tool: &HookToolName, input: &Value, reason: &str) -> String {
    if (tool.name == "Bash" || tool.name == "apply_patch")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        format!("Command blocked by PreToolUse hook: {reason}. Command: {command}")
    } else {
        format!(
            "Tool call blocked by PreToolUse hook: {reason}. Tool: {}",
            tool.name
        )
    }
}

fn hook_permission_mode(approval_policy: AskForApproval) -> String {
    // Every policy that may still ask the user maps to the source's `default` label.
    if matches!(approval_policy, AskForApproval::Never) {
        "bypassPermissions"
    } else {
        "default"
    }
    .to_string()
}

fn user_prompt(input: &[ModelInputItem]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) if message.role() == MessageRole::User => {
                Some(message.text_content())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
