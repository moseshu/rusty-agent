//! Host hook execution, independent of SDK lifecycle observers and tool guardrails.
//!
//! Callbacks for one event run concurrently, as Codex hook handlers do. Permission denials win
//! over grants; stop prompts are joined in registration order. Each callback has a timeout, and
//! errors or unsupported control effects are reported and ignored. Cancellation of the enclosing
//! operation still propagates. Reports use the existing host event sink and sequence allocator.
//!
//! A session owner binds this dispatcher to emit session-start/end and user-prompt events. The
//! runner cannot infer those events from a new run or a resumed model-input projection. The same
//! bound port reaches context processors for actual compaction events.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::future::join_all;
use ra_core::{
    cancel::{CancelReason, CancelScope, ScopeKind},
    context::RunContext,
    error::Result,
    event::HostEventEmitter,
    hook::{
        HookDecision, HookEvent, HookEventName, HookReport, HookRunStatus, UserHook,
        UserHookContext, UserHookDispatcher,
    },
    tool::ToolServices,
};
use tracing::Instrument;

/// One callback registered for one event. A callback may be registered for several events.
#[derive(Clone)]
pub struct UserHookRegistration {
    event: HookEventName,
    hook: Arc<dyn UserHook>,
    timeout: Duration,
}

impl UserHookRegistration {
    /// Registers a callback with Codex's default timeout: one second for session-end, otherwise
    /// 600 seconds. The enclosing run's cancellation and deadline always take precedence.
    #[must_use]
    pub fn new(event: HookEventName, hook: Arc<dyn UserHook>) -> Self {
        Self {
            event,
            hook,
            timeout: Duration::from_secs(if event == HookEventName::SessionEnd {
                1
            } else {
                600
            }),
        }
    }
    /// Overrides the callback timeout. Zero expires immediately.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// Event this registration handles.
    #[must_use]
    pub const fn event(&self) -> HookEventName {
        self.event
    }
}

impl std::fmt::Debug for UserHookRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserHookRegistration")
            .field("event", &self.event)
            .field("hook", &self.hook.name())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Immutable, shareable host hook registrations. Empty by default.
#[derive(Debug, Clone, Default)]
pub struct UserHooks {
    registrations: Arc<Vec<UserHookRegistration>>,
}

impl UserHooks {
    /// Appends a registration. Display names may repeat; no lookup by name takes place.
    #[must_use]
    pub fn with_hook(mut self, registration: UserHookRegistration) -> Self {
        Arc::make_mut(&mut self.registrations).push(registration);
        self
    }
    /// Whether this event has any callbacks installed.
    #[must_use]
    pub fn has_event(&self, event: HookEventName) -> bool {
        self.registrations
            .iter()
            .any(|registration| registration.event == event)
    }
    /// Binds execution to one run's context, cancellation and host event channel.
    #[must_use]
    pub fn bind(
        &self,
        run: Arc<RunContext>,
        cancel: CancelScope,
        services: ToolServices,
    ) -> BoundUserHooks {
        BoundUserHooks {
            hooks: self.clone(),
            run,
            cancel,
            services,
        }
    }
}

/// A run-bound dispatcher, also usable by hosts for lifecycle boundaries they own.
#[derive(Clone)]
pub struct BoundUserHooks {
    hooks: UserHooks,
    run: Arc<RunContext>,
    cancel: CancelScope,
    services: ToolServices,
}

impl BoundUserHooks {
    /// Executes all matching hooks and returns only event-valid control effects.
    /// Failed hooks are visible through `HookReport` events and tracing warnings.
    pub async fn dispatch(&self, event: HookEvent<'_>) -> Result<HookDecision> {
        self.cancel.ensure_not_cancelled()?;
        let outcomes = join_all(
            self.hooks
                .registrations
                .iter()
                .filter(|registration| registration.event == event.name())
                .map(|registration| self.run_one(registration, &event)),
        )
        .await;
        let mut allow = false;
        let mut denial = None;
        let mut prompts = Vec::new();
        for decision in outcomes {
            match decision? {
                HookDecision::Allow => allow = true,
                HookDecision::Deny { message } if denial.is_none() => denial = Some(message),
                HookDecision::Block { prompt } => prompts.push(prompt),
                _ => {}
            }
        }
        self.cancel.ensure_not_cancelled()?;
        Ok(if let Some(message) = denial {
            HookDecision::Deny { message }
        } else if !prompts.is_empty() {
            HookDecision::Block {
                prompt: prompts.join("\n\n"),
            }
        } else if allow {
            HookDecision::Allow
        } else {
            HookDecision::Continue
        })
    }

    async fn run_one(
        &self,
        registration: &UserHookRegistration,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        let cancel = self.cancel.child(ScopeKind::custom("user_hook"));
        let context = UserHookContext::new(&self.run, &cancel, &self.services);
        let span = tracing::debug_span!(
            "user_hook",
            hook.name = registration.hook.name(),
            hook.event = event.name().as_str()
        );
        let result = if registration.timeout.is_zero() {
            None
        } else {
            tokio::time::timeout(
                registration.timeout,
                cancel
                    .run(registration.hook.call(&context, event))
                    .instrument(span),
            )
            .await
            .ok()
        };
        let (status, decision, warning) = match result {
            None => {
                cancel.cancel(CancelReason::Timeout);
                (
                    HookRunStatus::TimedOut,
                    HookDecision::Continue,
                    Some("hook timed out".to_owned()),
                )
            }
            Some(Err(error)) => {
                self.report(
                    registration,
                    event,
                    HookRunStatus::Cancelled,
                    HookDecision::Continue,
                    None,
                )?;
                return Err(error);
            }
            Some(Ok(Err(error))) => (
                HookRunStatus::Failed,
                HookDecision::Continue,
                Some(error.to_string()),
            ),
            Some(Ok(Ok(decision))) => match validate_decision(event.name(), &decision) {
                Some(warning) => (
                    HookRunStatus::Ignored,
                    HookDecision::Continue,
                    Some(warning.to_owned()),
                ),
                None => (HookRunStatus::Completed, decision, None),
            },
        };
        self.report(registration, event, status, decision.clone(), warning)?;
        Ok(decision)
    }

    fn report(
        &self,
        registration: &UserHookRegistration,
        event: &HookEvent<'_>,
        status: HookRunStatus,
        decision: HookDecision,
        warning: Option<String>,
    ) -> Result<()> {
        if let Some(warning) = &warning {
            tracing::warn!(hook.name = registration.hook.name(), hook.event = event.name().as_str(), %warning, "user hook produced no control effect");
        }
        tracing::debug!(hook.name = registration.hook.name(), hook.event = event.name().as_str(), hook.status = ?status, "user hook finished");
        if let (Some(sink), Some(allocator)) =
            (self.services.event_sink(), self.run.event_seq_allocator())
        {
            let mut report =
                HookReport::new(registration.hook.name(), event.name(), status, decision)
                    .with_call_id(event.call_id().cloned());
            if let Some(warning) = warning {
                report = report.with_warning(warning);
            }
            HostEventEmitter::new(
                self.run.agent_id().clone(),
                allocator.clone(),
                Arc::clone(sink),
            )
            .emit(report)?;
        }
        Ok(())
    }
}

#[async_trait]
impl UserHookDispatcher for BoundUserHooks {
    async fn dispatch(&self, event: HookEvent<'_>) -> Result<HookDecision> {
        BoundUserHooks::dispatch(self, event).await
    }
}

fn validate_decision(event: HookEventName, decision: &HookDecision) -> Option<&'static str> {
    match (event, decision) {
        (_, HookDecision::Continue) | (HookEventName::PermissionRequest, HookDecision::Allow) => {
            None
        }
        (
            HookEventName::PreToolUse | HookEventName::PermissionRequest,
            HookDecision::Deny { message },
        ) if !message.trim().is_empty() => None,
        (HookEventName::Stop | HookEventName::SubagentStop, HookDecision::Block { prompt }) => {
            prompt
                .trim()
                .is_empty()
                .then_some("stop hook requested continuation without a prompt; ignoring the block")
        }
        _ => Some(
            "hook returned an unsupported or empty control decision for this event; ignoring it",
        ),
    }
}
