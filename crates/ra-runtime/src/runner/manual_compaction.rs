// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use std::sync::Arc;

use ra_core::{
    cancel::CancelScope,
    capability::{CapabilityFamily, ContextProcessorRequest},
    context::RunContext,
    error::{Error, Result},
    hook::{CompactTrigger, HookEvent, UserHookDispatcher},
    item::{Compaction, ItemId, ModelInputItem, RunItem, RunItemKind},
    model::ModelResolver,
    session::rollout::RolloutCompacted,
    state::RunId,
    tool::ToolServices,
};

use super::{
    CompactionHookDispatcher, RunConfig, RunOwner, RunStreamEvent, RunnerContextSummarizer,
    hook_context_metadata, hook_model_name,
};
use crate::{
    agent::AgentBinding,
    runner::stream::ModelStreamSink,
    thread::{ThreadRecorder, TurnAbort},
    turn::prepare::prepare_compaction,
};

/// A standalone compaction turn, with the thread retaining ownership of turn settlement.
pub(crate) struct ManualCompaction<'a> {
    pub(crate) agent: &'a AgentBinding,
    pub(crate) models: &'a dyn ModelResolver,
    pub(crate) config: &'a RunConfig,
    pub(crate) services: &'a ToolServices,
    pub(crate) cancel: &'a CancelScope,
    pub(crate) recorder: &'a ThreadRecorder,
    pub(crate) owner: Arc<TurnAbort>,
    pub(crate) run_id: RunId,
}

impl ManualCompaction<'_> {
    pub(crate) async fn run(self, history: &mut Vec<ModelInputItem>) -> Result<()> {
        let processor = self
            .config
            .capabilities()
            .iter()
            .find(|capability| capability.kind() == CapabilityFamily::COMPACTION)
            .and_then(|capability| capability.context_processor())
            .ok_or_else(|| Error::config("this thread has no compaction capability"))?;
        let run = hook_context_metadata(
            RunContext::new(self.run_id.clone(), self.agent.public()),
            hook_model_name(self.models, self.config, self.agent).as_deref(),
            self.config.app_context(),
        );
        if let Some(observer) = &self.config.step_observer {
            self.cancel
                .run(observer.capture(&run, self.cancel))
                .await??;
        }
        let (model, template) = prepare_compaction(
            self.agent,
            self.models,
            &run,
            self.cancel,
            self.config,
            history.clone(),
        )
        .await?;
        let template = template.with_tracing(self.config.tracing);
        let summarizer = RunnerContextSummarizer {
            model: model.model(),
            template: &template,
            cancel: self.cancel,
            record_usage: Some((self.recorder, &self.run_id)),
        };
        let hooks = self.bind_hooks(run);
        let item_id = ItemId::new(uuid::Uuid::now_v7().to_string());
        let trigger = CompactTrigger::Manual;
        hooks
            .dispatch(HookEvent::PreCompact {
                record_id: &item_id,
                trigger,
            })
            .await?;
        self.cancel.ensure_not_cancelled()?;
        let started = RunItem::new(
            item_id.clone(),
            RunItemKind::Compaction(Compaction::new("", Vec::new())),
        );
        self.recorder.emit(&RunStreamEvent::ItemStarted(started));

        let source = history
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| RunItem::new(ItemId::new(format!("source-{index}")), item.into()))
            .collect();
        let request = ContextProcessorRequest::new(
            self.run_id.clone(),
            1,
            item_id.clone(),
            model.selector().model().map(str::to_owned),
            Vec::new(),
            source,
            template.input()[history.len()..].to_vec(),
            template.input().to_vec(),
        )
        .with_force_compaction(true);
        let result = processor.process_context(request, &summarizer).await?;
        let item = result
            .generated_items()
            .iter()
            .find(|item| item.id() == &item_id)
            .ok_or_else(|| Error::caller("compaction did not produce a summary record"))?;
        let RunItemKind::Compaction(compaction) = item.kind() else {
            return Err(Error::caller("compaction produced an unexpected record"));
        };
        self.cancel.ensure_not_cancelled()?;
        let replacement = result.input().to_vec();
        self.recorder
            .commit_compaction(RolloutCompacted::new(
                self.run_id.clone(),
                item_id.clone(),
                replacement.clone(),
            ))
            .await?;
        *history = replacement;
        self.owner.install_compaction(history);
        self.recorder.compaction_completed(item.clone());
        self.recorder.warning(
            "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_owned(),
        );
        hooks
            .dispatch(HookEvent::PostCompact {
                record_id: &item_id,
                trigger,
                compaction,
            })
            .await?;
        self.cancel.ensure_not_cancelled()
    }

    fn bind_hooks(&self, run: RunContext) -> CompactionHookDispatcher {
        CompactionHookDispatcher {
            hooks: self.config.user_hooks().bind(
                Arc::new(run),
                self.cancel.clone(),
                self.services.clone(),
            ),
            compacted: false.into(),
            owner: Some(RunOwner::Thread(Arc::clone(&self.owner))),
            run_id: self.run_id.clone(),
            run_cancel: self.cancel.clone(),
            turn_scope: self.cancel.clone(),
            stopped_after_compaction: false.into(),
            session_start_on_stop: None,
        }
    }
}
