// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

//! Compaction of a thread's whole history.
//!
//! Codex compacts a session in three places: a standalone turn the user asks for
//! (`run_compact_task`), before a turn records its prompt (`run_pre_sampling_compact`), and between
//! two steps of a turn that needs a follow-up (`run_turn`'s `token_limit_reached` branch). All three
//! run the same body, `run_compact_task_inner`: `PreCompact`, the `ContextCompaction` item starting,
//! the summary request, `replace_compacted_history`, the item completing, a warning, and
//! `PostCompact`. [`HistoryCompaction`] is that body. The thread runs the first two, the turn loop
//! the third.
//!
//! Which history a thread holds is decided by the compaction capability's policy, which is why the
//! automatic checks ask the capability rather than measuring here: the request marks the history as
//! the session's ([`ContextProcessorRequest::with_session_history`]), and a capability that finds
//! nothing to do answers with no record.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    cancel::CancelScope,
    capability::{
        CapabilityFamily, ContextProcessorRequest, ContextSummarizer, ContextSummaryRequest,
        ContextSummaryResponse,
    },
    context::RunContext,
    error::{Error, Result},
    hook::{CompactTrigger, HookEvent, SessionStartSource, UserHookDispatcher},
    item::{Compaction, ItemId, ModelInputItem, RunItem, RunItemKind},
    model::ModelResolver,
    session::rollout::{RolloutCompacted, RolloutRecorder},
    state::{RunId, RunState},
    tool::ToolServices,
    usage::Usage,
};

use super::{
    AttemptStart, CompactionHookDispatcher, RunConfig, RunOutcome, RunOwner, RunStreamEvent,
    RunnerContextSummarizer, SummaryUsage, TurnInput, TurnLoopContext, TurnLoopProgress, emit,
    record_spend, reestablish_compact_context, turn_hook_context,
};
use crate::{
    agent::AgentBinding, runner::stream::ModelStreamSink, turn::prepare::prepare_compaction,
};

/// Codex's warning after every compaction.
const COMPACTION_WARNING: &str = "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.";

/// One run of Codex's `run_compact_task_inner` over a thread's history.
pub(crate) struct HistoryCompaction<'a> {
    pub(crate) agent: &'a AgentBinding,
    pub(crate) models: &'a dyn ModelResolver,
    pub(crate) config: &'a RunConfig,
    pub(crate) services: &'a ToolServices,
    /// The scope the compaction runs in, which a stopping hook cancels.
    pub(crate) cancel: &'a CancelScope,
    /// The scope of the run the compaction belongs to, cancelled by a stop with no owner above.
    pub(crate) run_cancel: &'a CancelScope,
    pub(crate) rollout: Option<&'a dyn RolloutRecorder>,
    pub(crate) sink: Option<&'a dyn ModelStreamSink>,
    pub(crate) owner: Option<RunOwner>,
    pub(crate) run_id: RunId,
    /// The context compaction hooks and dynamic instructions see.
    pub(crate) run: Arc<RunContext>,
    pub(crate) trigger: CompactTrigger,
    /// Whole-run ordinal of the turn the compaction belongs to.
    pub(crate) turn: u64,
    /// What the provider reported for the latest model call in the history, when it is known.
    pub(crate) reported_tokens: Option<u64>,
    /// What re-establishes context after a `PostCompact` stop, when the caller would otherwise
    /// have done it at once.
    pub(crate) session_start_on_stop: Option<SessionStartSource>,
    /// Where a run that pays for the summary collects its usage, to record it with its own spend.
    /// Without one, the usage goes straight into the rollout.
    pub(crate) spent: Option<&'a std::sync::Mutex<Usage>>,
    /// What records the turn's start once an attempt begins, before `PreCompact`: Codex persists
    /// `TurnStarted` before it compacts ahead of a turn's prompt, which is the boundary a cut at
    /// that turn falls on.
    pub(crate) turn_start: Option<AttemptStart>,
}

impl HistoryCompaction<'_> {
    /// Compacts `history` and returns its replacement, or `None` when an automatic check found
    /// nothing to compact.
    ///
    /// `install` receives the replacement and the compaction's item ID once the replacement is
    /// committed and before the item completes, which is where Codex's `replace_compacted_history`
    /// sits. A `PostCompact` stop still keeps what was installed, and ends with the stop's
    /// cancellation.
    #[allow(
        clippy::too_many_lines,
        reason = "Keep Codex's compaction order — hooks, item events, commit and install — together."
    )]
    pub(crate) async fn run(
        mut self,
        history: &[ModelInputItem],
        install: impl FnOnce(&[ModelInputItem], &ItemId),
    ) -> Result<Option<Vec<ModelInputItem>>> {
        let manual = self.trigger == CompactTrigger::Manual;
        let processor = self
            .config
            .capabilities()
            .iter()
            .find(|capability| capability.kind() == CapabilityFamily::COMPACTION)
            .and_then(|capability| capability.context_processor());
        let Some(processor) = processor else {
            if manual {
                return Err(Error::config("this thread has no compaction capability"));
            }
            return Ok(None);
        };
        if !manual && history.is_empty() {
            return Ok(None);
        }
        if manual && let Some(observer) = &self.config.step_observer {
            self.cancel
                .run(observer.capture(&self.run, self.cancel))
                .await??;
        }
        let turn_start = self.turn_start.take();
        let hooks = Arc::new(self.bind_hooks(turn_start));
        let item_id = ItemId::new(uuid::Uuid::now_v7().to_string());
        if manual {
            hooks
                .dispatch(HookEvent::PreCompact {
                    record_id: &item_id,
                    trigger: self.trigger,
                })
                .await?;
            self.cancel.ensure_not_cancelled()?;
        }

        let source = history
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| RunItem::new(ItemId::new(format!("source-{index}")), item.into()))
            .collect();
        let mut request = ContextProcessorRequest::new(
            self.run_id.clone(),
            self.turn,
            item_id.clone(),
            super::hook_model_name(self.models, self.config, self.agent),
            Vec::new(),
            source,
            Vec::new(),
            history.to_vec(),
        )
        .with_session_history(true)
        .with_force_compaction(manual)
        .with_user_hook_dispatcher(Arc::clone(&hooks) as Arc<dyn UserHookDispatcher>);
        if let Some(tokens) = self.reported_tokens {
            request = request.with_reported_tokens(tokens);
        }
        let summarizer = LazySummarizer {
            compaction: &self,
            item_id: &item_id,
            started: AtomicBool::new(false),
        };
        let result = processor.process_context(request, &summarizer).await?;
        let Some(item) = result
            .generated_items()
            .iter()
            .find(|item| item.id() == &item_id)
        else {
            if manual {
                return Err(Error::caller("compaction did not produce a summary record"));
            }
            return Ok(None);
        };
        let RunItemKind::Compaction(compaction) = item.kind() else {
            return Err(Error::caller("compaction produced an unexpected record"));
        };
        self.cancel.ensure_not_cancelled()?;
        let replacement = result.input().to_vec();
        if let Some(rollout) = self.rollout {
            rollout
                .commit_compaction(RolloutCompacted::new(
                    self.run_id.clone(),
                    item_id.clone(),
                    replacement.clone(),
                ))
                .await?;
        }
        install(&replacement, &item_id);
        if let Some(sink) = self.sink {
            sink.compaction_completed(item);
            sink.warning(COMPACTION_WARNING.to_owned());
        }
        hooks
            .dispatch(HookEvent::PostCompact {
                record_id: &item_id,
                trigger: self.trigger,
                compaction,
            })
            .await?;
        if hooks.stopped_after_compaction.load(Ordering::SeqCst) {
            return Err(hooks.cancellation());
        }
        self.cancel.ensure_not_cancelled()?;
        Ok(Some(replacement))
    }

    fn bind_hooks(&self, turn_start: Option<AttemptStart>) -> CompactionHookDispatcher {
        CompactionHookDispatcher {
            hooks: self.config.user_hooks().bind(
                Arc::clone(&self.run),
                self.cancel.clone(),
                self.services.clone(),
            ),
            compacted: false.into(),
            owner: self.owner.clone(),
            run_id: self.run_id.clone(),
            run_cancel: self.run_cancel.clone(),
            turn_scope: self.cancel.clone(),
            stopped_after_compaction: false.into(),
            session_start_on_stop: self.session_start_on_stop,
            before_attempt: std::sync::Mutex::new(turn_start),
        }
    }
}

/// Prepares the summary request only once a compaction has actually started.
///
/// An automatic check runs before every turn and between steps, and nearly every time finds
/// nothing to compact; resolving the agent's instructions for a request that is never sent would
/// cost each of those a dynamic-instructions call. The first request is also where Codex's
/// `ContextCompaction` item starts: after `PreCompact` allowed the attempt, before the summary is
/// asked for.
struct LazySummarizer<'a> {
    compaction: &'a HistoryCompaction<'a>,
    item_id: &'a ItemId,
    started: AtomicBool,
}

#[async_trait]
impl ContextSummarizer for LazySummarizer<'_> {
    async fn summarize(&self, request: ContextSummaryRequest) -> Result<ContextSummaryResponse> {
        let compaction = self.compaction;
        if !self.started.swap(true, Ordering::SeqCst)
            && let Some(sink) = compaction.sink
        {
            sink.emit(&RunStreamEvent::ItemStarted(RunItem::new(
                self.item_id.clone(),
                RunItemKind::Compaction(Compaction::new("", Vec::new())),
            )));
        }
        let (model, template) = prepare_compaction(
            compaction.agent,
            compaction.models,
            &compaction.run,
            compaction.cancel,
            compaction.config,
            request.input().to_vec(),
        )
        .await?;
        let template = template.with_tracing(compaction.config.tracing);
        let mut summary =
            ContextSummaryRequest::new(template.input().to_vec(), request.instructions());
        if let Some(schema) = request.output_schema() {
            summary = summary.with_output_schema(schema.clone());
        }
        RunnerContextSummarizer {
            model: model.model(),
            template: &template,
            cancel: compaction.cancel,
            record_usage: match (compaction.spent, compaction.rollout) {
                (Some(spent), _) => SummaryUsage::Spent(spent),
                (None, Some(rollout)) => SummaryUsage::Rollout(rollout, &compaction.run_id),
                (None, None) => SummaryUsage::Returned,
            },
            retry: true,
            retry_sink: compaction.sink,
            fallback_retry: compaction.config.compaction_retry(),
        }
        .summarize(summary)
        .await
    }
}

/// The total a provider reported for the latest request a model call made: Codex's
/// `last_token_usage.total_tokens`. `None` when the provider reported nothing.
pub(crate) fn reported_tokens(usage: &Usage) -> Option<u64> {
    usage
        .request_usage_entries()
        .last()
        .map(ra_core::usage::RequestUsage::total_tokens)
        .or_else(|| (usage.requests() > 0).then(|| usage.total_tokens()))
        .filter(|tokens| *tokens > 0)
}

/// Whether the run's whole history belongs to a thread that compacts it between steps.
///
/// Only a thread's own run, and only while its history is wholly reconstructible: a caller-managed
/// continuation holds a projection the run never recorded, which a replacement could not stand for.
/// A run with no thread above it keeps the capability's run-scoped projection.
pub(super) fn compacts_session(context: &TurnLoopContext<'_>) -> bool {
    matches!(context.owner, Some(RunOwner::Thread(_))) && context.authoritative_history_complete
}

/// Whether this step follows one that needed a follow-up, which is where Codex checks the limit
/// mid-turn. The first step of a turn has been checked before the turn recorded its prompt.
pub(super) fn due_mid_turn(
    context: &TurnLoopContext<'_>,
    state: &RunState,
    progress: &TurnLoopProgress,
) -> bool {
    compacts_session(context)
        && state.parent_run_id().is_none()
        && (progress.turns > 1 || context.continues_turn)
}

/// Compacts the thread's history between two steps of a turn, and continues the turn from the
/// replacement: Codex's mid-turn `run_auto_compact` followed by `run_pending_session_start_hooks`.
///
/// Returns the outcome that ends the run when a `SessionStart` hook re-establishing context stopped
/// it, and `None` when the turn goes on, compacted or not.
pub(super) async fn compact_mid_turn(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    turn_scope: &CancelScope,
) -> Result<Option<RunOutcome>> {
    let turn_input = TurnInput::resolve(context, state, progress)?;
    let mut history = turn_input.base().to_vec();
    history.extend(
        turn_input
            .carried()
            .iter()
            .filter_map(RunItem::to_model_input),
    );
    let spent = std::sync::Mutex::new(Usage::default());
    let compaction = HistoryCompaction {
        agent,
        models: context.model_resolver.as_ref(),
        config: context.config,
        services: context.services,
        cancel: turn_scope,
        run_cancel: context.cancel,
        rollout: context.events.rollout,
        sink: context.events.model_stream_sink,
        owner: context.owner.cloned(),
        run_id: context.run_id.clone(),
        run: Arc::new(turn_hook_context(context, agent, state)),
        trigger: CompactTrigger::Automatic,
        turn: progress.reference_turn(),
        reported_tokens: progress.last_response_tokens,
        // A stop leaves the context this turn would have re-established to the next one.
        session_start_on_stop: Some(SessionStartSource::Compact),
        spent: Some(&spent),
        turn_start: None,
    };
    let sink = context.events.model_stream_sink;
    let compacted = compaction
        .run(&history, |replacement, item_id| {
            let superseded: Vec<ItemId> = state
                .generated_items()
                .iter()
                .map(|item| item.id().clone())
                .collect();
            state.install_compacted_history(replacement.to_vec(), item_id.clone());
            progress.last_response_tokens = None;
            if let Some(sink) = sink {
                sink.history_compacted(replacement, &superseded);
            }
        })
        .await;
    // Paid for whether or not the compaction succeeded, and recorded once, with the run's spend.
    let usage = spent
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if usage.requests() > 0 {
        record_spend(context, state, &usage, None, progress, false);
    }
    if compacted?.is_none() {
        return Ok(None);
    }
    let hooks = context.config.user_hooks().bind(
        Arc::new(turn_hook_context(context, agent, state)),
        turn_scope.clone(),
        context.services.clone(),
    );
    let (items, stopped) = reestablish_compact_context(
        context,
        Some(&hooks),
        &format!("compact-hook-{}-history", progress.reference_turn()),
    )
    .await?;
    if !items.is_empty() {
        for item in &items {
            emit(context.events, RunStreamEvent::Item(item.clone()));
        }
        state.record_generated_items(items);
    }
    if stopped {
        progress.stop_processing = true;
        return Ok(Some(RunOutcome::Completed {
            reason: ra_core::finish::FinishReason::Final,
        }));
    }
    // The summary was paid for after the loop last checked the budget, and the next sampling would
    // be paid for on top of it: a spent token budget ends the run here, once the context Codex
    // re-establishes at once is in and before that request.
    let tokens_spent = context
        .config
        .budget()
        .max_tokens()
        .is_some_and(|maximum| context.spend.budget_tokens() >= maximum)
        || context.spend.rollout_budget_exhausted();
    if tokens_spent {
        progress.budget_stop = Some(ra_core::error::BudgetKind::Tokens);
        return Ok(Some(RunOutcome::Completed {
            reason: ra_core::finish::FinishReason::BudgetExhausted,
        }));
    }
    Ok(None)
}
