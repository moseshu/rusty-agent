//! The methods that load threads and read them: the parts of Codex's `ThreadRequestProcessor`
//! (`request_processors/thread_processor.rs`, `request_processors/thread_lifecycle.rs`) for
//! `thread/start`, `thread/resume`, `thread/fork`, `thread/read` and `thread/loaded/list`.
//!
//! # Loading
//!
//! A thread is prepared by the host's [`ThreadFactory`], given the settings the request carries,
//! given its history and the recorder of its rollout, built and loaded (see
//! [`super::thread_factory`]). The calling connection is subscribed to it and its listener
//! started, as Codex attaches the creating client.
//!
//! - `thread/start` creates the thread's rollout in the server's store — unless it is ephemeral —
//!   and every connection is sent `thread/started`.
//! - `thread/resume` of a thread that is not loaded reopens its rollout, rebuilds its history and
//!   loads it on that history, restoring the model and effort the store recorded and the approval
//!   policy, reviewer and named permissions its last settings snapshot recorded, unless the
//!   request sets them — a thread loaded with a settings adapter records its settings when it is
//!   loaded, written with its next turn; every connection is sent `thread/status/changed`. `thread/resume` of a
//!   loaded thread attaches the connection to it instead: the answer is composed by the thread's
//!   listener, between two of its events (see [`super::thread_listener`]). Overrides that differ
//!   from a loaded thread's settings reload it first when nobody else is subscribed to it and it is
//!   idle, and are otherwise ignored, as Codex ignores them.
//! - `thread/fork` copies the source's records — cut after `lastTurnId` or before `beforeTurnId`,
//!   and with a turn still running ended as interrupted — into a new thread, restoring the source's
//!   approval policy, reviewer and named permissions unless the request sets them, and every
//!   connection is sent `thread/started`. An ephemeral fork keeps the copy in memory only.
//!
//! # Reading
//!
//! `thread/read` describes a thread from what the store holds of it, overlaid with the live
//! thread's settings when it is loaded, and from the live thread alone when the store holds none
//! of it yet. Its turns are rebuilt from its rollout, as `thread/turns/list` rebuilds them; a
//! loaded thread's rollout is flushed first. `thread/loaded/list` pages the ids of the loaded
//! threads in order.
//!
//! # Differences from Codex
//!
//! - Codex's own configuration, its project trust, instruction files, MCP servers, dynamic tools,
//!   environments, goals and token usage replay are not ported; the members that set them are
//!   refused (see [`super::protocol::ThreadStartParams::unsupported_field`]).
//! - Threads are named by `UUIDv7` ids, as Codex names them; `thread/loaded/list` takes any loaded
//!   id as a cursor rather than refusing one that is not a UUID, since a host may load threads
//!   under ids of its own.
//! - A new thread's rollout path is not reported until a read finds the rollout: the recorder a
//!   store hands back does not say where it will write.
//! - Resuming by `path` and with `history` are refused; Codex reserves both for its cloud.
//! - An archived thread is refused with a pointer to `thread/unarchive` rather than to Codex's
//!   command-line `codex unarchive`.

use std::{sync::Arc, time::Duration};

use ra_core::{
    error::SessionErrorKind,
    event::{EventTimestamp, HostEventBody},
    session::{SessionId, rollout::RolloutRecorder},
};
use ra_runtime::thread::{AgentThread, AgentThreadBuilder};
use ra_session::{
    ForkSnapshot, ForkThreadParams, ForkedThread, ResumedThread, RolloutSessionMeta,
    rollout::{RolloutPayload, RolloutRecord, reconstruct_history},
    store::{
        CreateThreadParams, LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams,
        StoredThread, StoredThreadHistory, ThreadMetadataPatch, ThreadStore,
        UpdateThreadMetadataParams,
    },
};
use serde_json::{Map, Value, json};

use super::{
    dynamic_tools::validate_dynamic_tools,
    outgoing::{ConnectionRequestId, ServerNotification, now_unix_timestamp_ms},
    protocol::{
        ApprovalsReviewer, AskForApproval, THREAD_STARTED_METHOD, Thread, ThreadForkParams,
        ThreadForkResponse, ThreadHistoryMode, ThreadLoadedListParams, ThreadLoadedListResponse,
        ThreadPolicyFields, ThreadPolicyOverrides, ThreadReadParams, ThreadReadResponse,
        ThreadResumeParams, ThreadResumeResponse, ThreadSettingsRecord, ThreadStartParams,
        ThreadStartResponse, ThreadStartedNotification, ThreadStatus, Turn, TurnStatus,
    },
    settings_adapter::{ThreadSettingsAdapter, settings_snapshot},
    thread_factory::{ThreadFactory, ThreadFactoryRequest, ThreadOrigin, check_approval_requests},
    thread_history::{HistorySources, ThreadHistoryBuilder},
    thread_listener::{ListenerTaskContext, ensure_listener_task_running},
    thread_manager::{LoadedThread, LoadedThreadInfo},
    thread_processor::{
        ThreadRequestProcessor, normalize_thread_turns_status, response,
        set_thread_status_and_interrupt_stale_turns, turns_page,
    },
    thread_state::{PendingThreadResumeRequest, ThreadListenerCommand},
    thread_status::resolve_thread_status,
    tool_items::ThreadToolItems,
    turn_processor::{
        SettingsTarget, TurnMethodResult, check_approvals_reviewer, prepare_overrides,
        thread_settings_overrides, unsupported,
    },
};
use crate::frame::JsonRpcErrorObject;

/// The version of the program that creates a thread, recorded as Codex records its CLI version.
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long an idle loaded thread is given to shut down before `thread/resume` reloads it with
/// new settings: Codex's `wait_for_thread_shutdown`.
const RELOAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The host event family a thread's settings snapshots are recorded under (see
/// [`AgentThread::persist_settings_snapshot`]).
const THREAD_SETTINGS_APPLIED_FAMILY: &str = "threadSettingsApplied";

/// What the loading methods work with besides the processor.
pub(crate) struct ThreadMethodContext<'a> {
    pub(crate) history: &'a HistorySources,
    pub(crate) factory: Option<Arc<dyn ThreadFactory>>,
    pub(crate) listener: ListenerTaskContext,
}

impl ThreadRequestProcessor {
    /// `thread/start`: Codex's `thread_start`.
    pub(crate) async fn thread_start(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadStartParams,
        context: &ThreadMethodContext<'_>,
    ) -> TurnMethodResult {
        const METHOD: &str = "thread/start";
        let _thread_list_state = self.thread_list_state.lock().await;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported(METHOD, field));
        }
        check_approvals_reviewer(METHOD, params.policy.approvals_reviewer)?;
        if let Some(tools) = params.dynamic_tools().filter(|tools| !tools.is_empty()) {
            validate_dynamic_tools(tools).map_err(JsonRpcErrorObject::invalid_request)?;
            return Err(unsupported(METHOD, "dynamicTools"));
        }
        if params.history_mode == Some(ThreadHistoryMode::Paginated) {
            return Err(JsonRpcErrorObject::invalid_request(
                "paginated threads require thread/turns/list and thread/items/list support",
            ));
        }
        check_sandbox_and_permissions(&params.policy)?;
        let ephemeral = params.ephemeral.unwrap_or(false);
        let factory = require_factory(context, METHOD)?;
        let store = if ephemeral {
            None
        } else {
            Some(
                context
                    .history
                    .store()
                    .ok_or_else(|| unsupported_yet(METHOD))?,
            )
        };

        let thread_id = new_thread_id();
        let prepared = factory
            .prepare_thread(&ThreadFactoryRequest::new(
                thread_id.clone(),
                ThreadOrigin::Start,
            ))
            .await
            .map_err(|error| {
                JsonRpcErrorObject::internal_error(format!("error creating thread: {error}"))
            })?;
        let (builder, adapter, terminals) = prepared.into_parts();
        let builder = apply_settings(
            METHOD,
            builder,
            adapter.as_deref(),
            params.model,
            None,
            &to_policy_overrides(&params.policy),
            params.service_tier,
        )?;
        let builder = match &store {
            Some(store) => {
                let meta = new_session_meta(
                    &thread_id,
                    &builder,
                    adapter.as_deref(),
                    params.thread_source.as_deref(),
                );
                let recorder = store
                    .create_thread_with(&CreateThreadParams::new(meta))
                    .await
                    .map_err(|error| {
                        JsonRpcErrorObject::internal_error(format!(
                            "error creating thread: {error}"
                        ))
                    })?;
                builder.with_rollout_recorder(recorder)
            }
            None => builder,
        };
        let info = LoadedThreadInfo {
            ephemeral,
            created_at: now_unix_seconds(),
            thread_source: params.thread_source,
            ..LoadedThreadInfo::default()
        };
        let loaded = self.load(builder.build(), info, adapter, terminals);
        self.attach(&request_id, &loaded, &context.listener).await;
        self.thread_watch.upsert_thread_silently(loaded.thread_id());

        let mut thread = build_thread_from_loaded(&loaded);
        thread.status = resolve_thread_status(
            self.thread_watch
                .loaded_status_for_thread(loaded.thread_id()),
            false,
        );
        let settings = thread_settings_record(&loaded, false);
        self.outgoing
            .send_serialized_response(
                request_id,
                ThreadStartResponse::new(thread.clone(), settings),
            )
            .await;
        self.send_thread_started(thread).await;
        Ok(None)
    }

    /// `thread/resume`: Codex's `thread_resume`.
    #[allow(clippy::too_many_lines)] // Codex's `thread_resume_inner`, step for step.
    pub(crate) async fn thread_resume(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadResumeParams,
        context: &ThreadMethodContext<'_>,
    ) -> TurnMethodResult {
        const METHOD: &str = "thread/resume";
        let _thread_list_state = self.thread_list_state.lock().await;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported(METHOD, field));
        }
        check_approvals_reviewer(METHOD, params.policy.approvals_reviewer)?;
        let thread_id = SessionId::new(&params.thread_id);
        self.refuse_closing(&thread_id).await?;
        check_sandbox_and_permissions(&params.policy)?;

        if let Some(loaded) = self.thread_manager.get_thread(&thread_id) {
            let stored = read_stored_thread_for_resume(context.history.store(), &thread_id).await?;
            if !self.reload_for_overrides(&loaded, &params).await {
                return self
                    .resume_running_thread(request_id, &params, loaded, &stored, context)
                    .await;
            }
        }

        let factory = require_factory(context, METHOD)?;
        let store = context
            .history
            .store()
            .ok_or_else(|| no_rollout_found(&thread_id))?;
        let stored = read_stored_thread_for_resume(Some(Arc::clone(&store)), &thread_id).await?;
        let resumed =
            ResumedThread::resume(store.as_ref(), &ResumeThreadParams::new(thread_id.clone()))
                .await
                .map_err(|error| resume_error(&thread_id, &error))?;
        let (recorder, history, reconstruction) = resumed.into_parts();
        let records = history.into_records();
        if let Err(error) = self.refuse_closing(&thread_id).await {
            discard(&recorder).await;
            return Err(error);
        }

        let prepared = match factory
            .prepare_thread(&ThreadFactoryRequest::new(
                thread_id.clone(),
                ThreadOrigin::Resume,
            ))
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                discard(&recorder).await;
                return Err(JsonRpcErrorObject::internal_error(format!(
                    "error resuming thread: {error}"
                )));
            }
        };
        let (builder, adapter, terminals) = prepared.into_parts();
        let (model, effort) = if params.model.is_some() {
            (params.model.clone(), None)
        } else {
            persisted_model_and_effort(&builder, &stored)
        };
        let policy = restore_persisted_policy(
            to_policy_overrides(&params.policy),
            &records,
            adapter.is_some(),
        );
        let builder = match apply_settings(
            METHOD,
            builder,
            adapter.as_deref(),
            model,
            effort,
            &policy,
            params.service_tier.clone(),
        ) {
            Ok(builder) => builder,
            Err(error) => {
                discard(&recorder).await;
                return Err(error);
            }
        };
        let builder = builder
            .with_history(reconstruction.history().to_vec())
            .with_rollout_recorder(recorder);
        let info = LoadedThreadInfo {
            ephemeral: false,
            created_at: stored.created_at().map_or_else(now_unix_seconds, seconds),
            forked_from_id: stored.forked_from_id().cloned(),
            thread_source: stored.thread_source().map(str::to_owned),
            preview: stored.preview().to_owned(),
        };
        let loaded = self.load(builder.build(), info, adapter, terminals);
        self.attach(&request_id, &loaded, &context.listener).await;
        self.thread_watch.upsert_thread(loaded.thread_id()).await;

        let mut thread = thread_from_stored(&stored);
        apply_live_thread_settings(&mut thread, &loaded);
        let tool_items = context.history.tool_items();
        let turns = turns_from_records(&records, tool_items.as_deref());
        let status = self
            .thread_watch
            .loaded_status_for_thread(loaded.thread_id());
        let page = match &params.initial_turns_page {
            Some(page_params) => {
                let mut page_turns = turns.clone();
                normalize_thread_turns_status(&mut page_turns, status.clone(), false);
                Some(turns_page(page_turns, page_params)?)
            }
            None => None,
        };
        if !params.exclude_turns {
            thread.turns = turns;
        }
        set_thread_status_and_interrupt_stale_turns(&mut thread, status, false);
        let settings = thread_settings_record(&loaded, true);
        response(ThreadResumeResponse::new(thread, page, settings))
    }

    /// `thread/fork`: Codex's `thread_fork`.
    #[allow(clippy::too_many_lines)] // Codex's `thread_fork_inner`, step for step.
    pub(crate) async fn thread_fork(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadForkParams,
        context: &ThreadMethodContext<'_>,
    ) -> TurnMethodResult {
        const METHOD: &str = "thread/fork";
        let _thread_list_state = self.thread_list_state.lock().await;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported(METHOD, field));
        }
        check_approvals_reviewer(METHOD, params.policy.approvals_reviewer)?;
        check_sandbox_and_permissions(&params.policy)?;
        let source_id = SessionId::new(&params.thread_id);
        if let Some(source) = self.thread_manager.get_thread(&source_id)
            && let Err(error) = source.flush_rollout().await
        {
            tracing::warn!(%source_id, %error, "failed to flush a thread before forking it");
        }
        let store = context
            .history
            .store()
            .ok_or_else(|| no_rollout_found(&source_id))?;
        let source = read_stored_thread_for_resume(Some(Arc::clone(&store)), &source_id).await?;
        if params.last_turn_id.is_some() && params.before_turn_id.is_some() {
            return Err(JsonRpcErrorObject::invalid_request(
                "`beforeTurnId` cannot be combined with `lastTurnId`",
            ));
        }
        let factory = require_factory(context, METHOD)?;
        let records = store
            .load_history(&LoadThreadHistoryParams::new(source_id.clone()).including_archived())
            .await
            .map_err(|error| store_read_error(&source_id, &error))?
            .into_records();
        let tool_items = context.history.tool_items();
        // Settings belong to the source thread, independently of the history cutoff.
        let persisted_policy = latest_settings_snapshot(&records);
        let records = match (&params.last_turn_id, &params.before_turn_id) {
            (Some(last_turn_id), None) => {
                truncate_records_after_turn_id(records, last_turn_id, tool_items.as_deref())?
            }
            (None, Some(before_turn_id)) => {
                truncate_records_before_turn_id(records, before_turn_id, tool_items.as_deref())?
            }
            _ => records,
        };

        let thread_id = new_thread_id();
        let prepared = factory
            .prepare_thread(&ThreadFactoryRequest::new(
                thread_id.clone(),
                ThreadOrigin::Fork {
                    source: source_id.clone(),
                },
            ))
            .await
            .map_err(|error| {
                JsonRpcErrorObject::internal_error(format!("error forking thread: {error}"))
            })?;
        let (builder, adapter, terminals) = prepared.into_parts();
        let policy = restore_policy_from_snapshot(
            to_policy_overrides(&params.policy),
            persisted_policy.as_ref(),
            adapter.is_some(),
        );
        let builder = apply_settings(
            METHOD,
            builder,
            adapter.as_deref(),
            params.model.clone(),
            None,
            &policy,
            params.service_tier.clone(),
        )?;
        let snapshot = ForkThreadParams::new(ForkSnapshot::Interrupted);
        let (builder, fork_records) = if params.ephemeral {
            let fork_records = snapshot
                .snapshot_records(records)
                .map_err(|error| fork_error(&source_id, &error))?;
            let reconstruction = reconstruct_history(&fork_records)
                .map_err(|error| fork_error(&source_id, &error))?;
            (
                builder.with_history(reconstruction.history().to_vec()),
                fork_records,
            )
        } else {
            let meta = new_session_meta(
                &thread_id,
                &builder,
                adapter.as_deref(),
                params.thread_source.as_deref(),
            );
            let forked = ForkedThread::fork_from_history(
                store.as_ref(),
                StoredThreadHistory::new(source_id.clone(), records),
                &snapshot.with_session_meta(meta),
            )
            .await
            .map_err(|error| fork_error(&source_id, &error))?;
            let (recorder, reconstruction) = forked.into_parts();
            (
                builder
                    .with_history(reconstruction.history().to_vec())
                    .with_rollout_recorder(recorder),
                Vec::new(),
            )
        };
        let source_name = source
            .name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        if !params.ephemeral
            && let Some(name) = &source_name
            && let Err(error) = store
                .update_thread_metadata(&UpdateThreadMetadataParams::new(
                    thread_id.clone(),
                    ThreadMetadataPatch::new().with_name(Some(name.clone())),
                ))
                .await
        {
            return Err(JsonRpcErrorObject::internal_error(format!(
                "failed to inherit source thread name: {error}"
            )));
        }

        let info = LoadedThreadInfo {
            ephemeral: params.ephemeral,
            created_at: now_unix_seconds(),
            forked_from_id: Some(source_id.clone()),
            thread_source: params.thread_source.clone(),
            preview: if params.ephemeral {
                preview_from_records(&fork_records)
            } else {
                String::new()
            },
        };
        let loaded = self.load(builder.build(), info, adapter, terminals);
        self.attach(&request_id, &loaded, &context.listener).await;

        let mut thread = if params.ephemeral {
            let mut thread = build_thread_from_loaded(&loaded);
            if !params.exclude_turns {
                thread.turns = turns_from_records(&fork_records, tool_items.as_deref());
            }
            thread
        } else {
            let stored = store
                .read_thread(&ReadThreadParams::new(thread_id.clone()).including_archived())
                .await
                .map_err(|error| store_read_error(&thread_id, &error))?;
            let mut thread = thread_from_stored(&stored);
            if !params.exclude_turns {
                let records = store
                    .load_history(&LoadThreadHistoryParams::new(thread_id.clone()))
                    .await
                    .map_err(|error| store_read_error(&thread_id, &error))?
                    .into_records();
                thread.turns = turns_from_records(&records, tool_items.as_deref());
            }
            thread
        };
        if let Some(name) = source_name {
            set_thread_name_from_title(&mut thread, name);
        }
        apply_live_thread_settings(&mut thread, &loaded);
        thread.thread_source.clone_from(&params.thread_source);
        self.thread_watch.upsert_thread_silently(loaded.thread_id());
        set_thread_status_and_interrupt_stale_turns(
            &mut thread,
            self.thread_watch
                .loaded_status_for_thread(loaded.thread_id()),
            false,
        );
        let settings = thread_settings_record(&loaded, false);
        self.outgoing
            .send_serialized_response(
                request_id,
                ThreadForkResponse::new(thread.clone(), settings),
            )
            .await;
        self.send_thread_started(thread).await;
        Ok(None)
    }

    /// `thread/read`: Codex's `thread_read`.
    pub(crate) async fn thread_read(
        &self,
        params: ThreadReadParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let thread_id = SessionId::new(&params.thread_id);
        let loaded = self.thread_manager.get_thread(&thread_id);
        if params.include_turns
            && let Some(loaded) = &loaded
        {
            if loaded.info.ephemeral {
                return Err(JsonRpcErrorObject::invalid_request(
                    "ephemeral threads do not support includeTurns",
                ));
            }
            if let Err(error) = loaded.flush_rollout().await {
                tracing::warn!(%thread_id, %error, "failed to flush a thread before reading it");
            }
        }
        let store = history.store();
        let persisted = match &store {
            Some(store) => match store
                .read_thread(&ReadThreadParams::new(thread_id.clone()).including_archived())
                .await
            {
                Ok(stored) => Some(stored),
                Err(error) if is_not_found(&error) => None,
                Err(error) => {
                    return Err(JsonRpcErrorObject::internal_error(format!(
                        "failed to read thread: {error}"
                    )));
                }
            },
            None => None,
        };
        let mut thread = match (&persisted, &loaded) {
            (Some(stored), loaded) => {
                let mut thread = thread_from_stored(stored);
                if let Some(loaded) = loaded {
                    apply_live_thread_settings(&mut thread, loaded);
                }
                thread
            }
            (None, Some(loaded)) if !params.include_turns => build_thread_from_loaded(loaded),
            (None, Some(_)) => {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "thread {thread_id} is not materialized yet; includeTurns is unavailable before first user message"
                )));
            }
            (None, None) => {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "thread not loaded: {thread_id}"
                )));
            }
        };
        if params.include_turns
            && let Some(store) = &store
        {
            let records = store
                .load_history(&LoadThreadHistoryParams::new(thread_id.clone()).including_archived())
                .await
                .map_err(|error| {
                    JsonRpcErrorObject::internal_error(format!(
                        "failed to load thread history for thread {thread_id}: {error}"
                    ))
                })?
                .into_records();
            let tool_items = history.tool_items();
            thread.turns = turns_from_records(&records, tool_items.as_deref());
        }
        let has_live_in_progress_turn = loaded.as_ref().is_some_and(|loaded| loaded.is_running());
        set_thread_status_and_interrupt_stale_turns(
            &mut thread,
            self.thread_watch.loaded_status_for_thread(&thread_id),
            has_live_in_progress_turn,
        );
        response(ThreadReadResponse::new(thread))
    }

    /// `thread/loaded/list`: Codex's `thread_loaded_list`.
    pub(crate) fn thread_loaded_list(&self, params: &ThreadLoadedListParams) -> TurnMethodResult {
        let mut data: Vec<String> = self
            .thread_manager
            .list_thread_ids()
            .into_iter()
            .map(|thread_id| thread_id.as_str().to_owned())
            .collect();
        if data.is_empty() {
            return response(ThreadLoadedListResponse::new(data, None));
        }
        data.sort();
        let total = data.len();
        let start = params
            .cursor
            .as_ref()
            .map_or(0, |cursor| match data.binary_search(cursor) {
                Ok(index) => index + 1,
                Err(index) => index,
            });
        let effective_limit = params
            .limit
            .map_or(total, |limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .max(1);
        let end = start.saturating_add(effective_limit).min(total);
        let page = data[start.min(end)..end].to_vec();
        let next_cursor = page.last().filter(|_| end < total).cloned();
        response(ThreadLoadedListResponse::new(page, next_cursor))
    }

    /// Answers `thread/resume` of a loaded thread through its listener: Codex's
    /// `resume_running_thread`.
    async fn resume_running_thread(
        &self,
        request_id: ConnectionRequestId,
        params: &ThreadResumeParams,
        loaded: LoadedThread,
        stored: &StoredThread,
        context: &ThreadMethodContext<'_>,
    ) -> TurnMethodResult {
        let thread_id = loaded.thread_id().clone();
        let needs_history = !params.exclude_turns || params.initial_turns_page.is_some();
        let history_turns = if needs_history {
            let store = context
                .history
                .store()
                .ok_or_else(|| no_rollout_found(&thread_id))?;
            let records = store
                .load_history(&LoadThreadHistoryParams::new(thread_id.clone()).including_archived())
                .await
                .map_err(|error| store_read_error(&thread_id, &error))?
                .into_records();
            let tool_items = context.history.tool_items();
            turns_from_records(&records, tool_items.as_deref())
        } else {
            Vec::new()
        };
        ensure_listener_task_running(&context.listener, &loaded).await?;
        let mut thread_summary = thread_from_stored(stored);
        apply_live_thread_settings(&mut thread_summary, &loaded);
        let command_tx = self
            .outgoing
            .thread_state()
            .thread_state(&thread_id)
            .lock()
            .await
            .listener_command_tx();
        let Some(command_tx) = command_tx else {
            return Err(JsonRpcErrorObject::internal_error(format!(
                "failed to enqueue running thread resume for thread {thread_id}: thread listener is not running"
            )));
        };
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let command = ThreadListenerCommand::SendThreadResumeResponse {
            request: Box::new(PendingThreadResumeRequest {
                request_id,
                thread_summary,
                history_turns,
                include_turns: !params.exclude_turns,
                initial_turns_page: params.initial_turns_page.clone(),
                settings: thread_settings_record(&loaded, true),
            }),
            completion_tx,
        };
        if command_tx.send(command).is_err() {
            return Err(JsonRpcErrorObject::internal_error(format!(
                "failed to enqueue running thread resume for thread {thread_id}: thread listener command channel is closed"
            )));
        }
        let _ = completion_rx.await;
        Ok(None)
    }

    /// Unloads a loaded thread whose settings differ from those `thread/resume` asks for, so it
    /// can be loaded again with them, when nobody is subscribed to it and it is idle: the reload
    /// path of Codex's `resume_running_thread`. Returns whether it was unloaded; otherwise the
    /// differences are logged and ignored.
    async fn reload_for_overrides(
        &self,
        loaded: &LoadedThread,
        params: &ThreadResumeParams,
    ) -> bool {
        let mismatches = collect_resume_override_mismatches(params, loaded);
        if mismatches.is_empty() {
            return false;
        }
        let thread_id = loaded.thread_id();
        let has_subscribers = !self
            .outgoing
            .thread_state()
            .subscribed_connection_ids(thread_id)
            .is_empty();
        let idle = self.thread_watch.loaded_status_for_thread(thread_id) == ThreadStatus::Idle;
        if !has_subscribers && idle && !loaded.is_running() {
            if tokio::time::timeout(RELOAD_SHUTDOWN_TIMEOUT, async {
                loaded.shutdown().await;
                loaded.close_rollout().await
            })
            .await
            .is_ok_and(|result| result.is_ok())
            {
                self.thread_manager.remove_thread_if_matches(loaded);
                self.finalize_thread_teardown(thread_id).await;
                return true;
            }
            tracing::warn!(%thread_id, "thread shutdown failed or timed out");
        }
        tracing::warn!(
            "thread/resume overrides ignored for loaded thread {thread_id}: {}",
            mismatches.join("; ")
        );
        false
    }

    /// Refuses a thread being unloaded: Codex's check of its pending unloads.
    async fn refuse_closing(&self, thread_id: &SessionId) -> Result<(), JsonRpcErrorObject> {
        if self.pending_thread_unloads.lock().await.contains(thread_id) {
            return Err(JsonRpcErrorObject::invalid_request(format!(
                "thread {thread_id} is closing; retry thread/resume after the thread is closed"
            )));
        }
        Ok(())
    }

    fn load(
        &self,
        thread: AgentThread,
        info: LoadedThreadInfo,
        adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
        terminals: Option<Arc<dyn super::ThreadBackgroundTerminals>>,
    ) -> LoadedThread {
        let loaded = self
            .thread_manager
            .add_thread(thread, info, adapter, terminals);
        // The product settings the thread was loaded with, for a later resume or fork to restore,
        // as Codex records the settings each turn runs with.
        if loaded.adapter.is_some() {
            let snapshot = settings_snapshot(&loaded.thread, loaded.adapter.as_deref());
            if let Err(error) = loaded.record_settings_snapshot(snapshot) {
                tracing::warn!(thread_id = %loaded.thread_id(), %error, "failed to record thread settings");
            }
        }
        loaded
    }

    /// Subscribes the calling connection to a thread just loaded and starts its listener: Codex's
    /// `ensure_conversation_listener`, whose failures are logged rather than failing the request.
    async fn attach(
        &self,
        request_id: &ConnectionRequestId,
        loaded: &LoadedThread,
        listener: &ListenerTaskContext,
    ) {
        let thread_state = self.outgoing.thread_state();
        // The thread's state exists before its listener watches it, subscribers or not.
        let _ = thread_state.thread_state(loaded.thread_id());
        if !thread_state.try_add_connection_to_thread(loaded.thread_id(), request_id.connection_id)
        {
            tracing::debug!(
                thread_id = %loaded.thread_id(),
                "skipping the subscription of a closed connection"
            );
        }
        if let Err(error) = ensure_listener_task_running(listener, loaded).await {
            tracing::warn!(
                thread_id = %loaded.thread_id(),
                "failed to attach listener for thread: {}",
                error.message()
            );
        }
    }

    async fn send_thread_started(&self, thread: Thread) {
        match serde_json::to_value(ThreadStartedNotification::new(thread)) {
            Ok(params) => {
                self.outgoing
                    .send_server_notification(ServerNotification::new(
                        THREAD_STARTED_METHOD,
                        params,
                    ))
                    .await;
            }
            // The notification is a plain record, so this does not happen.
            Err(error) => tracing::error!("failed to serialize {THREAD_STARTED_METHOD}: {error}"),
        }
    }
}

/// A new thread's id: a `UUIDv7`, as Codex's thread ids are.
fn new_thread_id() -> SessionId {
    SessionId::new(uuid::Uuid::now_v7().to_string())
}

fn now_unix_seconds() -> i64 {
    now_unix_timestamp_ms() / 1000
}

fn seconds(timestamp: EventTimestamp) -> i64 {
    i64::try_from(timestamp.as_millis() / 1000).unwrap_or(i64::MAX)
}

/// Codex's `unsupported_thread_store_operation`.
fn unsupported_yet(operation: &str) -> JsonRpcErrorObject {
    JsonRpcErrorObject::method_not_found(format!("{operation} is not supported yet"))
}

fn require_factory(
    context: &ThreadMethodContext<'_>,
    method: &str,
) -> Result<Arc<dyn ThreadFactory>, JsonRpcErrorObject> {
    context
        .factory
        .clone()
        .ok_or_else(|| unsupported_yet(method))
}

fn check_sandbox_and_permissions(policy: &ThreadPolicyFields) -> Result<(), JsonRpcErrorObject> {
    if policy.sandbox.is_some() && policy.permissions.is_some() {
        return Err(JsonRpcErrorObject::invalid_request(
            "`permissions` cannot be combined with `sandbox`",
        ));
    }
    Ok(())
}

fn to_policy_overrides(policy: &ThreadPolicyFields) -> ThreadPolicyOverrides {
    ThreadPolicyOverrides {
        approval_policy: policy.approval_policy,
        approvals_reviewer: policy.approvals_reviewer,
        sandbox_policy: policy.sandbox.map(super::protocol::SandboxMode::to_policy),
        permissions: policy.permissions.clone(),
    }
}

/// Lowers the settings a request carries onto the thread's configuration before it is built, as
/// Codex builds its configuration with the request's overrides.
#[allow(
    clippy::option_option,
    reason = "Codex distinguishes omitted, null and supplied service tiers"
)]
fn apply_settings(
    method: &str,
    builder: AgentThreadBuilder,
    adapter: Option<&dyn ThreadSettingsAdapter>,
    model: Option<String>,
    effort: Option<String>,
    policy: &ThreadPolicyOverrides,
    service_tier: Option<Option<String>>,
) -> Result<AgentThreadBuilder, JsonRpcErrorObject> {
    let overrides = prepare_overrides(
        method,
        &SettingsTarget {
            models: &builder,
            adapter,
        },
        builder.config().clone(),
        model,
        effort,
        policy,
        None,
        service_tier,
    )?;
    let builder = builder.with_settings(&overrides);
    check_approval_requests(&builder, adapter).map_err(|error| {
        let operation = match method {
            "thread/start" => "creating",
            "thread/resume" => "resuming",
            "thread/fork" => "forking",
            _ => "loading",
        };
        JsonRpcErrorObject::internal_error(format!("error {operation} thread: {error}"))
    })?;
    Ok(builder)
}

/// The model and effort a stored thread last ran with, for a resume that sets no model: Codex's
/// `merge_persisted_resume_metadata`. A recorded model the host no longer resolves, or an effort
/// a run here does not take, is left to the host's default.
fn persisted_model_and_effort(
    builder: &AgentThreadBuilder,
    stored: &StoredThread,
) -> (Option<String>, Option<String>) {
    let model = stored
        .model()
        .filter(|model| builder.resolve_model(Some(model)).is_ok())
        .map(str::to_owned);
    let effort = stored
        .effort()
        .filter(|effort| {
            thread_settings_overrides("thread/resume", None, Some((*effort).to_owned())).is_ok()
        })
        .map(str::to_owned);
    (model, effort)
}

/// Fills in what the request leaves unset of the approval policy, reviewer and named permissions
/// from the thread's last recorded settings snapshot: Codex's `latest_persisted_resume_settings`
/// as its resume and fork apply it. Only a thread loaded with a settings adapter has them.
fn restore_persisted_policy(
    policy: ThreadPolicyOverrides,
    records: &[RolloutRecord],
    has_adapter: bool,
) -> ThreadPolicyOverrides {
    restore_policy_from_snapshot(
        policy,
        latest_settings_snapshot(records).as_ref(),
        has_adapter,
    )
}

fn restore_policy_from_snapshot(
    mut policy: ThreadPolicyOverrides,
    snapshot: Option<&Map<String, Value>>,
    has_adapter: bool,
) -> ThreadPolicyOverrides {
    if !has_adapter {
        return policy;
    }
    let Some(snapshot) = snapshot else {
        return policy;
    };
    if policy.approval_policy.is_none() {
        policy.approval_policy = snapshot
            .get("approvalPolicy")
            .and_then(|value| serde_json::from_value::<AskForApproval>(value.clone()).ok());
    }
    if policy.approvals_reviewer.is_none() {
        // A thread recorded under `auto_review` before it was refused resumes with its client
        // reviewing, as Codex's configuration falls back from a reviewer its requirements
        // disallow rather than failing to load.
        policy.approvals_reviewer = snapshot
            .get("approvalsReviewer")
            .and_then(|value| serde_json::from_value::<ApprovalsReviewer>(value.clone()).ok())
            .filter(|reviewer| *reviewer != ApprovalsReviewer::AutoReview);
    }
    if policy.sandbox_policy.is_none() && policy.permissions.is_none() {
        policy.permissions = snapshot
            .get("activePermissionProfile")
            .and_then(|profile| profile.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    policy
}

/// The last settings snapshot a thread recorded.
fn latest_settings_snapshot(records: &[RolloutRecord]) -> Option<Map<String, Value>> {
    records
        .iter()
        .rev()
        .find_map(|record| match record.payload() {
            Ok(RolloutPayload::Event(event)) => match event.body() {
                HostEventBody::Unknown { family, data }
                    if family == THREAD_SETTINGS_APPLIED_FAMILY =>
                {
                    data.as_object().cloned()
                }
                _ => None,
            },
            _ => None,
        })
}

/// The differences between a loaded thread's settings and those `thread/resume` asks for:
/// Codex's `collect_resume_override_mismatches`, for the settings a thread here has.
fn collect_resume_override_mismatches(
    params: &ThreadResumeParams,
    loaded: &LoadedThread,
) -> Vec<String> {
    let snapshot = settings_snapshot(&loaded.thread, loaded.adapter.as_deref());
    let mut mismatches = Vec::new();
    let mut compare = |name: &str, key: &str, requested: Option<Value>| {
        if let Some(requested) = requested {
            let active = snapshot.get(key).cloned().unwrap_or(Value::Null);
            if requested != active {
                mismatches.push(format!("{name} requested={requested} active={active}"));
            }
        }
    };
    compare("model", "model", params.model.clone().map(Value::String));
    compare(
        "service_tier",
        "serviceTier",
        params
            .service_tier
            .clone()
            .map(|tier| tier.map_or(Value::Null, Value::String)),
    );
    compare(
        "approval_policy",
        "approvalPolicy",
        params
            .policy
            .approval_policy
            .and_then(|policy| serde_json::to_value(policy).ok()),
    );
    compare(
        "approvals_reviewer",
        "approvalsReviewer",
        params
            .policy
            .approvals_reviewer
            .and_then(|reviewer| serde_json::to_value(reviewer).ok()),
    );
    compare(
        "sandbox",
        "sandboxPolicy",
        params
            .policy
            .sandbox
            .and_then(|mode| serde_json::to_value(mode.to_policy()).ok()),
    );
    if let Some(permissions) = &params.policy.permissions {
        let active = snapshot
            .get("activePermissionProfile")
            .and_then(|profile| profile.get("id"))
            .and_then(Value::as_str);
        if active != Some(permissions.as_str()) {
            mismatches.push(format!(
                "permissions requested={permissions} active={}",
                active.unwrap_or("none")
            ));
        }
    }
    mismatches
}

/// A stored thread for resuming or forking, refused if the store holds none or it is archived:
/// Codex's `read_stored_thread_for_resume`.
async fn read_stored_thread_for_resume(
    store: Option<Arc<dyn ThreadStore>>,
    thread_id: &SessionId,
) -> Result<StoredThread, JsonRpcErrorObject> {
    let store = store.ok_or_else(|| no_rollout_found(thread_id))?;
    let stored = store
        .read_thread(&ReadThreadParams::new(thread_id.clone()).including_archived())
        .await
        .map_err(|error| store_read_error(thread_id, &error))?;
    if stored.archived_at().is_some() {
        return Err(JsonRpcErrorObject::invalid_request(format!(
            "session {thread_id} is archived. Unarchive it with thread/unarchive first."
        )));
    }
    Ok(stored)
}

pub(super) fn is_not_found(error: &ra_core::error::Error) -> bool {
    matches!(
        error,
        ra_core::error::Error::Session {
            kind: SessionErrorKind::NotFound,
            ..
        }
    )
}

fn no_rollout_found(thread_id: &SessionId) -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_request(format!("no rollout found for thread id {thread_id}"))
}

/// Codex's `thread_store_resume_read_error`.
fn store_read_error(thread_id: &SessionId, error: &ra_core::error::Error) -> JsonRpcErrorObject {
    if is_not_found(error) {
        no_rollout_found(thread_id)
    } else {
        JsonRpcErrorObject::internal_error(format!("failed to read thread: {error}"))
    }
}

fn resume_error(thread_id: &SessionId, error: &ra_core::error::Error) -> JsonRpcErrorObject {
    if is_not_found(error) {
        no_rollout_found(thread_id)
    } else {
        JsonRpcErrorObject::internal_error(format!("error resuming thread: {error}"))
    }
}

fn fork_error(source_id: &SessionId, error: &ra_core::error::Error) -> JsonRpcErrorObject {
    if is_not_found(error) {
        no_rollout_found(source_id)
    } else {
        JsonRpcErrorObject::internal_error(format!("error forking thread: {error}"))
    }
}

/// Lets a reopened writer go after a resume failed, as Codex discards a session that did not
/// start.
async fn discard(recorder: &Arc<dyn RolloutRecorder>) {
    if let Err(error) = recorder.discard().await {
        tracing::warn!(%error, "failed to discard a thread after resume failed");
    }
}

/// The session metadata a thread created by the server records.
fn new_session_meta(
    thread_id: &SessionId,
    builder: &AgentThreadBuilder,
    adapter: Option<&dyn ThreadSettingsAdapter>,
    thread_source: Option<&str>,
) -> RolloutSessionMeta {
    let mut meta = RolloutSessionMeta::new(thread_id.clone()).with_cli_version(CLI_VERSION);
    if let Ok(resolved) = builder.resolve_model(builder.config().model()) {
        meta = meta.with_model_provider(resolved.selector().provider().as_str());
    }
    if let Some(cwd) = adapter
        .map(|adapter| adapter.describe(builder.config()))
        .and_then(|described| {
            described
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    {
        meta = meta.with_cwd(cwd);
    }
    if let Some(source) = thread_source {
        meta = meta.with_thread_source(source);
    }
    meta
}

/// The turns `records` hold, with all their items.
fn turns_from_records(
    records: &[RolloutRecord],
    tool_items: Option<&dyn ThreadToolItems>,
) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    for record in records {
        builder.handle_record(record, tool_items);
    }
    builder.finish()
}

/// The run a record starts, if it starts one.
fn run_started(record: &RolloutRecord) -> Option<String> {
    match record.payload() {
        Ok(RolloutPayload::RunStarted(started)) => Some(started.run_id().as_str().to_owned()),
        _ => None,
    }
}

/// The records up to the end of the turn `last_turn_id`: Codex's `truncate_rollout_after_turn_id`.
///
/// A turn is a run here, and a run continued after approval starts again under its id, so the cut
/// falls before the first start of another run after it, rather than before the next start of any
/// turn.
fn truncate_records_after_turn_id(
    mut records: Vec<RolloutRecord>,
    last_turn_id: &str,
    tool_items: Option<&dyn ThreadToolItems>,
) -> Result<Vec<RolloutRecord>, JsonRpcErrorObject> {
    let turns = turns_from_records(&records, tool_items);
    let turn = turns
        .iter()
        .find(|turn| turn.id() == last_turn_id)
        .ok_or_else(|| {
            JsonRpcErrorObject::invalid_request(format!(
                "lastTurnId '{last_turn_id}' was not found in the source thread"
            ))
        })?;
    let target_start_index = records
        .iter()
        .position(|record| run_started(record).as_deref() == Some(last_turn_id))
        .ok_or_else(|| {
            JsonRpcErrorObject::invalid_request(format!(
                "lastTurnId '{last_turn_id}' is not a persisted canonical turn in the source thread"
            ))
        })?;
    if turn.status() == TurnStatus::InProgress {
        return Err(JsonRpcErrorObject::invalid_request(format!(
            "lastTurnId '{last_turn_id}' identifies an in-progress turn"
        )));
    }
    let cut_index = records
        .iter()
        .enumerate()
        .skip(target_start_index.saturating_add(1))
        .find_map(|(index, record)| {
            run_started(record)
                .is_some_and(|run_id| run_id != last_turn_id)
                .then_some(index)
        })
        .unwrap_or(records.len());
    records.truncate(cut_index);
    Ok(records)
}

/// The records before the turn `before_turn_id`: Codex's `truncate_rollout_before_turn_id`.
fn truncate_records_before_turn_id(
    mut records: Vec<RolloutRecord>,
    before_turn_id: &str,
    tool_items: Option<&dyn ThreadToolItems>,
) -> Result<Vec<RolloutRecord>, JsonRpcErrorObject> {
    let Some(cut_index) = records
        .iter()
        .position(|record| run_started(record).as_deref() == Some(before_turn_id))
    else {
        if turns_from_records(&records, tool_items)
            .iter()
            .any(|turn| turn.id() == before_turn_id)
        {
            return Err(JsonRpcErrorObject::invalid_request(format!(
                "beforeTurnId '{before_turn_id}' is not a persisted canonical turn in the source thread"
            )));
        }
        return Err(JsonRpcErrorObject::invalid_request(format!(
            "beforeTurnId '{before_turn_id}' was not found in the source thread"
        )));
    };
    records.truncate(cut_index);
    Ok(records)
}

/// The first user message `records` hold: Codex's `preview_from_rollout_items`.
fn preview_from_records(records: &[RolloutRecord]) -> String {
    turns_from_records(records, None)
        .iter()
        .flat_map(Turn::items)
        .find_map(|item| match item {
            super::protocol::ThreadItem::UserMessage { content, .. } => {
                content.iter().find_map(|input| match input {
                    super::protocol::UserInput::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Names the thread by `title` unless it is empty or only repeats the preview: Codex's
/// `set_thread_name_from_title`.
fn set_thread_name_from_title(thread: &mut Thread, title: String) {
    if title.trim().is_empty() || thread.preview.trim() == title.trim() {
        return;
    }
    thread.name = Some(title);
}

/// Where a stored thread's session came from: Codex's `SessionSource`, for what is recorded here.
fn session_source(stored: &StoredThread) -> Value {
    match stored.thread_spawn() {
        Some(spawn) => json!({"subAgent": {"thread_spawn": {
            "parent_thread_id": spawn.parent_session_id().as_str(),
            "depth": spawn.depth(),
            "agent_path": spawn.agent_path(),
            "agent_nickname": null,
            "agent_role": spawn.agent_type().map(ra_core::agent::AgentId::as_str),
        }}}),
        None => Value::String("unknown".to_owned()),
    }
}

/// The thread as the store describes it: Codex's `thread_from_stored_thread`.
pub(super) fn thread_from_stored(stored: &StoredThread) -> Thread {
    let id = stored.session_id().as_str().to_owned();
    let created_at = stored.created_at().map_or(0, seconds);
    let mut thread = Thread {
        session_id: stored.thread_spawn().map_or_else(
            || id.clone(),
            |spawn| spawn.root_session_id().as_str().to_owned(),
        ),
        id,
        environments: None,
        extra: None,
        forked_from_id: stored.forked_from_id().map(|id| id.as_str().to_owned()),
        parent_thread_id: stored.parent_session_id().map(|id| id.as_str().to_owned()),
        preview: stored.preview().to_owned(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: ThreadHistoryMode::Legacy,
        model_provider: stored.model_provider().unwrap_or_default().to_owned(),
        model: stored.model().map(str::to_owned),
        reasoning_effort: stored.effort().map(str::to_owned),
        created_at,
        updated_at: stored.updated_at().map_or(created_at, seconds),
        recency_at: None,
        status: ThreadStatus::NotLoaded,
        path: stored.rollout_path().map(std::path::Path::to_path_buf),
        cwd: stored.cwd().map(std::path::PathBuf::from),
        cli_version: stored.cli_version().unwrap_or_default().to_owned(),
        originator: stored.originator().map(str::to_owned),
        source: session_source(stored),
        can_accept_direct_input: None,
        thread_source: stored.thread_source().map(str::to_owned),
        agent_nickname: None,
        agent_role: stored
            .thread_spawn()
            .and_then(|spawn| spawn.agent_type())
            .map(|role| role.as_str().to_owned()),
        git_info: stored.git_info().map(|git| {
            json!({
                "sha": git.commit_hash, "branch": git.branch, "originUrl": git.repository_url,
            })
        }),
        name: None,
        daybreak_enabled: None,
        turns: Vec::new(),
    };
    if let Some(name) = stored.name() {
        set_thread_name_from_title(&mut thread, name.to_owned());
    }
    thread
}

/// The model, provider and effort a loaded thread runs with.
fn live_model(loaded: &LoadedThread) -> (Option<String>, Option<String>, Option<String>) {
    let config = loaded.run_config();
    let resolved = loaded.resolve_model(config.model()).ok();
    (
        resolved
            .as_ref()
            .and_then(|resolved| resolved.selector().model().map(str::to_owned)),
        resolved
            .as_ref()
            .map(|resolved| resolved.selector().provider().as_str().to_owned()),
        config
            .model_settings()
            .effort()
            .map(|effort| effort.label().to_owned()),
    )
}

/// Overlays a loaded thread's live settings: Codex's `apply_live_thread_settings`, with the
/// direct-input capability and, for a thread the store recorded no provider of, the provider.
pub(super) fn apply_live_thread_settings(thread: &mut Thread, loaded: &LoadedThread) {
    let (model, provider, effort) = live_model(loaded);
    thread.model = model;
    thread.reasoning_effort = effort;
    if thread.model_provider.is_empty()
        && let Some(provider) = provider
    {
        thread.model_provider = provider;
    }
    thread.ephemeral = loaded.info.ephemeral;
    thread.can_accept_direct_input = Some(true);
}

/// A loaded thread the store holds nothing of yet: Codex's `build_thread_from_snapshot`.
pub(super) fn build_thread_from_loaded(loaded: &LoadedThread) -> Thread {
    let (model, provider, effort) = live_model(loaded);
    let cwd = loaded
        .adapter
        .as_ref()
        .map(|adapter| adapter.describe(&loaded.run_config()))
        .and_then(|described| {
            described
                .get("cwd")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from)
        });
    let id = loaded.thread_id().as_str().to_owned();
    Thread {
        session_id: id.clone(),
        id,
        environments: None,
        extra: None,
        forked_from_id: loaded
            .info
            .forked_from_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        parent_thread_id: None,
        preview: loaded.info.preview.clone(),
        ephemeral: loaded.info.ephemeral,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: ThreadHistoryMode::Legacy,
        model_provider: provider.unwrap_or_default(),
        model,
        reasoning_effort: effort,
        created_at: loaded.info.created_at,
        updated_at: loaded.info.created_at,
        recency_at: None,
        status: ThreadStatus::NotLoaded,
        path: None,
        cwd,
        cli_version: CLI_VERSION.to_owned(),
        originator: None,
        source: Value::String("unknown".to_owned()),
        can_accept_direct_input: Some(true),
        thread_source: loaded.info.thread_source.clone(),
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        daybreak_enabled: None,
        turns: Vec::new(),
    }
}

/// A loaded thread's effective settings, as `thread/start`, `thread/resume` and `thread/fork`
/// answer with them: the thread's model settings and its host adapter's product fields, under
/// Codex's response names (see [`super::protocol::ThreadStartResponse`]).
fn thread_settings_record(
    loaded: &LoadedThread,
    with_collaboration_mode: bool,
) -> ThreadSettingsRecord {
    let snapshot = settings_snapshot(&loaded.thread, loaded.adapter.as_deref());
    let get = |key: &str| snapshot.get(key).cloned();
    let mut record = Map::new();
    record.insert("model".into(), get("model").unwrap_or(Value::Null));
    record.insert(
        "modelProvider".into(),
        get("modelProvider").unwrap_or(Value::Null),
    );
    record.insert(
        "serviceTier".into(),
        get("serviceTier").unwrap_or(Value::Null),
    );
    record.insert(
        "disabledPluginIds".into(),
        get("disabledPluginIds").unwrap_or_else(|| json!([])),
    );
    for key in ["cwd", "approvalPolicy", "approvalsReviewer"] {
        if let Some(value) = get(key) {
            record.insert(key.into(), value);
        }
    }
    record.insert("runtimeWorkspaceRoots".into(), json!([]));
    record.insert("instructionSources".into(), json!([]));
    if let Some(sandbox) = get("sandboxPolicy") {
        record.insert("sandbox".into(), sandbox);
    }
    if let Some(profile) = get("activePermissionProfile") {
        record.insert("activePermissionProfile".into(), profile);
    }
    record.insert(
        "reasoningEffort".into(),
        get("effort").unwrap_or(Value::Null),
    );
    if with_collaboration_mode && let Some(mode) = get("collaborationMode") {
        record.insert("collaborationMode".into(), mode);
    }
    record.insert(
        "multiAgentMode".into(),
        get("multiAgentMode").unwrap_or_else(|| json!("explicitRequestOnly")),
    );
    record
}
