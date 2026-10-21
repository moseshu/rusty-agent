//! The approval requests of a turn whose run stopped on approvals: the approval arms of Codex's
//! `apply_bespoke_event_handling` (`ExecApprovalRequest`, `ApplyPatchApprovalRequest`) and their
//! response handlers (`on_command_execution_request_approval_response`,
//! `on_file_change_request_approval_response`).
//!
//! # The batch
//!
//! Codex's tools each wait in their own invocation, so it asks each approval as a tool raises it.
//! A run here stops once the calls it was making have settled, with every question it reached in
//! its checkpoint, and continues once all of them are answered, as the agents-python interruption
//! contract has it. The questions are therefore asked together, when the turn reports it is
//! waiting (see [`ThreadEvent::TurnAwaitingApproval`]). Each answer is recorded and applied as it
//! arrives, but the checkpoint resumes only once every answer is in. A task of its own lets the
//! thread's listener go on handling events meanwhile. A follow-up question a resumed call raises
//! is a new stop, and a new batch.
//!
//! Each request keeps the thread waiting on approval while it is unanswered. Its answer is read as
//! Codex reads it (see [`super::answers`]); its [`ApprovalEvent`] is recorded for live items and
//! rollout replay, the checkpoint is answered, and the product applies effects such as session
//! caching. The checkpoint resumes once the remaining questions have answers too.
//!
//! A cancellation aborts the turn as soon as it arrives, as Codex's aborts its turn: it is
//! recorded, the turn is interrupted, and the turn's end withdraws the requests still waiting. A
//! request withdrawn because its turn moved on passes nothing on, and answers that arrive for a
//! checkpoint that is no longer the one waiting are dropped, so they cannot settle a later stop.
//!
//! Spawned agents use the same batch handling. Their session owns each pending request, while
//! the parent thread's connections carry it; parent turn transitions leave these requests alive.
//! The control tree's pause notifications withdraw a child's requests when its own run ends.
//!
//! A call the product asks nothing for — or any call of a thread loaded without a product
//! adapter — is refused as Codex refuses a call whose approval could not be obtained.

use std::sync::Arc;

mod target;
use target::ApprovalTarget;

use futures::{StreamExt, stream::FuturesUnordered};
use ra_core::{
    event::{ApprovalEvent, ApprovalResolution},
    item::{RunItem, RunItemKind},
    state::RunState,
};
use ra_runtime::thread::ThreadEvent;

use super::{
    answers::{ApprovalAnswer, command_execution_approval_answer, file_change_approval_answer},
    approvals::{ReviewDecision, RunAnswer},
    outgoing::{ServerNotification, ThreadScopedOutgoingMessageSender, now_unix_timestamp_ms},
    protocol::{ServerRequestPayload, WARNING_METHOD, WarningNotification},
    settings_adapter::PendingApproval,
    thread_manager::LoadedThread,
    thread_status::ThreadWatchManager,
};

/// The method an approval's answer is read for.
#[derive(Debug, Clone, Copy)]
enum ApprovalMethod {
    CommandExecution,
    FileChange,
}

/// Asks the client about the calls the run of `turn_id` stopped on, and settles them once
/// answered, in a task of its own.
pub(crate) fn start_approval_batch(
    loaded: LoadedThread,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_watch: Arc<ThreadWatchManager>,
    turn_id: String,
) {
    tokio::spawn(async move {
        run_approval_batch(
            &loaded,
            &outgoing,
            &thread_watch,
            &turn_id,
            &ApprovalTarget::Parent,
        )
        .await;
    });
}

/// Child requests retain the child's session ownership while going to the parent's connections.
pub(crate) fn start_spawned_approval_batch(
    loaded: LoadedThread,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_watch: Arc<ThreadWatchManager>,
    control: ra_runtime::agent::control::AgentControl,
    paused: ra_runtime::agent::control::PausedAgentRun,
) {
    let outgoing = outgoing.with_request_owner(paused.session_id().clone());
    tokio::spawn(async move {
        let turn_id = paused.state().run_id().as_str().to_owned();
        let target = ApprovalTarget::Spawned {
            control,
            paused: Box::new(paused),
        };
        run_approval_batch(&loaded, &outgoing, &thread_watch, &turn_id, &target).await;
    });
}

async fn run_approval_batch(
    loaded: &LoadedThread,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_watch: &Arc<ThreadWatchManager>,
    turn_id: &str,
    target: &ApprovalTarget,
) {
    let Some((mut state, config)) = target.checkpoint(loaded, turn_id) else {
        return;
    };
    let batch: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    let thread_id = match target {
        ApprovalTarget::Parent => loaded.thread_id().clone(),
        ApprovalTarget::Spawned { paused, .. } => paused.session_id().clone(),
    };

    let mut answers = Vec::with_capacity(batch.len());
    let mut asked = Vec::with_capacity(batch.len());
    let mut waiting = FuturesUnordered::new();
    for item in &batch {
        let Some((method, request)) =
            approval_request(loaded, &config, thread_id.as_str(), turn_id, item)
        else {
            answers.push((item.clone(), ApprovalAnswer::failed()));
            continue;
        };
        let guard = match target {
            ApprovalTarget::Parent => Some(
                thread_watch
                    .note_permission_requested(loaded.thread_id())
                    .await,
            ),
            ApprovalTarget::Spawned { .. } => None,
        };
        let (request_id, answer) = outgoing.send_request(request).await;
        asked.push(request_id.clone());
        let item = item.clone();
        waiting.push(async move {
            let answer = outgoing.wait_for_answer(request_id, answer).await;
            drop(guard);
            let answer = match method {
                ApprovalMethod::CommandExecution => command_execution_approval_answer(answer),
                ApprovalMethod::FileChange => file_change_approval_answer(answer),
            };
            (item, answer)
        });
    }

    let mut ended = false;
    let mut effects = FuturesUnordered::new();
    for (item, answer) in answers {
        if !apply_answer(loaded, turn_id, &batch, &mut state, &item, &answer, target).await {
            ended = true;
            break;
        }
        effects.push(apply_product_decision(
            loaded,
            outgoing,
            &config,
            item,
            answer.decision,
        ));
    }

    // Once the turn has moved on, the requests still waiting are only waited for, so each is
    // resolved as its turn's end withdraws it, as Codex's handler of each request resolves it.
    // The parent's listener withdraws them at its turn transition. A spawned agent's run has no
    // such transition, so its batch withdraws exactly the requests it asked, once.
    let mut withdrawn = false;
    while !waiting.is_empty() || !effects.is_empty() {
        if ended && !withdrawn {
            withdrawn = true;
            if target.withdraws_own_requests() {
                outgoing.abort_server_requests(&asked);
            }
        }
        tokio::select! {
            () = target.changed(loaded, turn_id, &batch), if !ended => ended = true,
            Some((item, answer)) = waiting.next(), if !waiting.is_empty() => {
                if ended {
                    continue;
                }
                match answer {
                    // Withdrawn because the turn moved on: nothing is passed on.
                    None => ended = true,
                    Some(answer) => {
                        ended = !apply_answer(loaded, turn_id, &batch, &mut state, &item, &answer, target).await;
                        if !ended {
                            effects.push(apply_product_decision(loaded, outgoing, &config, item, answer.decision));
                        }
                    }
                }
            }
            Some(()) = effects.next(), if !effects.is_empty() => {}
        }
    }
    if !ended && target.current_batch(loaded, turn_id, &batch) && !target.resume(loaded, state) {
        // A refused continuation that leaves the same batch waiting did not lose a race: the
        // thread could not continue the checkpoint, such as one whose current agent its registry
        // no longer resolves. The pause is kept, so only an interrupt ends that turn.
        if target.current_batch(loaded, turn_id, &batch) {
            tracing::warn!(
                turn_id,
                "the run answered for approval could not be continued and stays paused until \
                 interrupted; its checkpoint's current agent may not be resolvable"
            );
        } else {
            tracing::warn!(
                turn_id,
                "the run answered for approval was no longer waiting"
            );
        }
    }
}

fn approval_request(
    loaded: &LoadedThread,
    config: &ra_runtime::runner::RunConfig,
    thread_id: &str,
    turn_id: &str,
    item: &RunItem,
) -> Option<(ApprovalMethod, ServerRequestPayload)> {
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        return None;
    };
    let pending = PendingApproval::new(thread_id, turn_id, now_unix_timestamp_ms(), approval);
    let request = loaded.adapter.as_ref()?.approval_request(config, pending)?;
    match request {
        request @ ServerRequestPayload::CommandExecutionRequestApproval(_) => {
            Some((ApprovalMethod::CommandExecution, request))
        }
        request @ ServerRequestPayload::FileChangeRequestApproval(_) => {
            Some((ApprovalMethod::FileChange, request))
        }
        _ => None,
    }
}

/// Settles one answer immediately, preserving its event and product effects even if another
/// request later cancels the batch. Returns whether the remaining answers may still be applied.
async fn apply_answer(
    loaded: &LoadedThread,
    turn_id: &str,
    batch: &[RunItem],
    state: &mut RunState,
    item: &RunItem,
    answer: &ApprovalAnswer,
    target: &ApprovalTarget,
) -> bool {
    if !target.current_batch(loaded, turn_id, batch) {
        return false;
    }
    if !record(loaded, state, item, answer.resolution, target) {
        return false;
    }
    let settled = match answer.decision.run_answer() {
        RunAnswer::Approve => state.approve(item, false),
        RunAnswer::Reject(message) => state.reject_with_message(item, false, message),
        RunAnswer::Abort => {
            target.interrupt(loaded).await;
            return false;
        }
    };
    if let Err(error) = settled {
        tracing::error!(%error, "an approval answer could not be applied to its run");
        if let Err(error) =
            state.reject_with_message(item, false, super::answers::APPROVAL_REQUEST_FAILED)
        {
            tracing::error!(%error, "an approval could not be refused on its run");
            return false;
        }
    }
    true
}

/// An amendment can await storage. Keep polling answers meanwhile so a cancellation still aborts
/// the turn immediately, and finish accepted effects before resuming or releasing the batch.
///
/// What could not be applied is sent to the thread's connections as Codex's `warning`.
async fn apply_product_decision(
    loaded: &LoadedThread,
    outgoing: &ThreadScopedOutgoingMessageSender,
    config: &ra_runtime::runner::RunConfig,
    item: RunItem,
    decision: ReviewDecision,
) {
    let (Some(adapter), RunItemKind::ToolApproval(approval)) =
        (loaded.adapter.as_ref(), item.kind())
    else {
        return;
    };
    let Some(message) = adapter
        .apply_review_decision(config, approval, &decision)
        .await
    else {
        return;
    };
    let warning = WarningNotification::new(Some(loaded.thread_id().as_str().to_owned()), message);
    match serde_json::to_value(warning) {
        Ok(params) => {
            outgoing
                .send_server_notification_to_current_connections(ServerNotification::new(
                    WARNING_METHOD,
                    params,
                ))
                .await;
        }
        // The notification is a plain record, so this does not happen.
        Err(error) => tracing::error!("failed to serialize {WARNING_METHOD}: {error}"),
    }
}

/// Records how the approval of `item`'s call was settled.
fn record(
    loaded: &LoadedThread,
    state: &mut RunState,
    item: &RunItem,
    resolution: ApprovalResolution,
    target: &ApprovalTarget,
) -> bool {
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        return false;
    };
    let event = ApprovalEvent::resolved(approval.call_id().clone(), resolution);
    if let Err(error) = target.record(loaded, state, event) {
        tracing::warn!(%error, "an approval answer could not be recorded");
        return false;
    }
    true
}

/// Whether `event` is a turn's run stopping on approvals, and which turn.
pub(crate) fn awaiting_approval(event: &ThreadEvent) -> Option<&str> {
    match event {
        ThreadEvent::TurnAwaitingApproval { turn_id } => Some(turn_id),
        _ => None,
    }
}
