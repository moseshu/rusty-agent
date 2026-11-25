// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Session-ID operations used by Codex's V1 collaboration surface.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use ra_core::{
    agent::{
        AgentId,
        control::{AgentControlError, AgentPath, AgentStatus, InterAgentCommunication, LiveAgent},
    },
    cancel::{CancelReason, ScopeKind},
    error::{Error, Result, SessionErrorKind},
    item::{Message, ModelInputItem},
    session::{SessionId, rollout::ThreadSpawnEdgeStatus},
};

use super::{
    AgentHandle, AgentNode, ChildEnvironment, ChildRuntime, Commit, DEPTH_LIMIT_REACHED, ParentRun,
    SpawnGuard, Tree, closed_error, lock, spawned_declaration,
};

pub(super) struct ResidentLimits {
    pub(super) max_threads: usize,
    pub(super) max_depth: i32,
    enabled: Box<dyn Fn() -> bool + Send + Sync>,
}

pub(super) type LiveAgentLookup = dyn Fn(&SessionId) -> Option<LiveAgent> + Send + Sync;

impl ResidentLimits {
    pub(super) fn new(
        max_threads: usize,
        max_depth: i32,
        enabled: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            max_threads,
            max_depth,
            enabled: Box::new(enabled),
        }
    }
}

/// Storeless trees retain the model history needed to reopen an explicitly named thread.
#[derive(Clone)]
pub(super) struct ClosedAgent {
    pub(super) path: AgentPath,
    pub(super) agent_type: Option<AgentId>,
    pub(super) history: Vec<ModelInputItem>,
    pub(super) explicitly_closed: bool,
}

impl Tree {
    pub(super) fn resident_limits(&self) -> Option<&ResidentLimits> {
        self.resident_limits
            .get()
            .filter(|limits| (limits.enabled)())
    }

    pub(super) fn notify_legacy_completion(&self, child: &AgentNode, status: &AgentStatus) {
        let Some(parent) = child.path.parent().and_then(|path| self.node(&path)) else {
            return;
        };
        let Some(notification) = child.legacy_completion(&parent.path, status) else {
            return;
        };
        if parent.path.is_root() {
            if let Some(thread) = self
                .root_thread
                .get()
                .and_then(crate::thread::WeakAgentThread::upgrade)
            {
                thread.inject_context_background(notification.to_message());
            } else {
                parent.mailbox.push(notification);
            }
        } else {
            parent.inject_context(vec![notification.to_message()]);
        }
    }

    pub(super) fn legacy_agent(&self, id: &SessionId) -> Option<Arc<AgentNode>> {
        if self.root.session_id.get() == Some(id) {
            return Some(Arc::clone(&self.root));
        }
        self.node_of_session(id)
    }

    fn lookup_live_agent(&self, id: &SessionId) -> Option<LiveAgent> {
        self.live_agent_lookup.get().and_then(|lookup| lookup(id))
    }
}

fn missing(id: &SessionId) -> AgentControlError {
    AgentControlError::Unsupported(format!("agent with id {id} not found"))
}

fn final_status(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

impl AgentNode {
    pub(super) fn legacy_completion(
        &self,
        parent: &AgentPath,
        status: &AgentStatus,
    ) -> Option<InterAgentCommunication> {
        let id = self.session_id.get()?;
        if !final_status(status) || self.legacy_completion_sent.swap(true, Ordering::AcqRel) {
            return None;
        }
        let body = serde_json::json!({"agent_path": id, "status": status});
        Some(InterAgentCommunication::new(
            self.path.clone(),
            parent.clone(),
            format!("<subagent_notification>\n{body}\n</subagent_notification>"),
            false,
        ))
    }
}

impl AgentHandle {
    pub(super) async fn send_legacy_input(
        &self,
        target: &SessionId,
        input: Message,
        interrupt: bool,
    ) -> Result<(String, AgentPath), AgentControlError> {
        let tree = self.tree()?;
        self.restore_legacy_children(&tree).await?;
        let node = tree.legacy_agent(target).ok_or_else(|| missing(target))?;
        let parent = ParentRun::current();
        let parent_run_id = parent.as_ref().map(|parent| parent.run_id().clone());
        let origin = parent.as_ref().and_then(|parent| tree.origin_of(parent));
        let queue = || {
            if node.is_closed() || tree.shut_down.load(Ordering::Acquire) {
                return Err(missing(target));
            }
            node.mailbox.push_from(
                InterAgentCommunication::from_user_input(
                    self.node.path.clone(),
                    node.path.clone(),
                    input,
                ),
                parent_run_id,
                origin,
            );
            Ok(())
        };
        if let Some(child) = &node.child {
            let state = lock(&child.state);
            if interrupt && Arc::ptr_eq(&self.node, &node) {
                return Err(AgentControlError::Unsupported(
                    "an agent cannot interrupt itself".to_owned(),
                ));
            }
            if interrupt && let Some(run) = &state.current_run {
                run.cancel(CancelReason::UserInterrupt);
            }
            queue()?;
        } else {
            if interrupt {
                return Err(AgentControlError::Unsupported(
                    "root is not a spawned agent".to_owned(),
                ));
            }
            queue()?;
        }
        tree.ensure_running(&node);
        Ok((uuid::Uuid::now_v7().to_string(), node.path.clone()))
    }

    pub(super) async fn wait_for_legacy_agents(
        &self,
        targets: &[SessionId],
        timeout: Duration,
    ) -> Result<BTreeMap<String, AgentStatus>, AgentControlError> {
        let tree = self.tree()?;
        self.restore_legacy_children(&tree).await?;
        let mut pending = FuturesUnordered::new();
        let mut statuses = BTreeMap::new();
        for target in targets {
            let Some(node) = tree.legacy_agent(target) else {
                statuses.insert(target.to_string(), AgentStatus::NotFound);
                continue;
            };
            let mut receiver = node.status.subscribe();
            let initial = receiver.borrow_and_update().clone();
            if final_status(&initial) {
                statuses.insert(target.to_string(), initial);
                continue;
            }
            pending.push(async move {
                let status = receiver
                    .wait_for(final_status)
                    .await
                    .map_or(AgentStatus::NotFound, |status| status.clone());
                (target.to_string(), status)
            });
        }
        if !statuses.is_empty() {
            return Ok(statuses);
        }
        if let Ok(Some((target, status))) = tokio::time::timeout(timeout, pending.next()).await {
            statuses.insert(target, status);
            while let Some(Some((target, status))) = pending.next().now_or_never() {
                statuses.insert(target, status);
            }
        }
        Ok(statuses)
    }

    pub(super) async fn close_legacy_agent(
        &self,
        target: &SessionId,
    ) -> Result<LiveAgent, AgentControlError> {
        let tree = self.tree()?;
        self.restore_legacy_children(&tree).await?;
        let node = tree
            .node_of_session(target)
            .ok_or_else(|| missing(target))?;
        if Arc::ptr_eq(&self.node, &node) {
            return Err(AgentControlError::Unsupported(
                "an agent cannot close itself".to_owned(),
            ));
        }
        let previous = node.snapshot();
        node.explicitly_closed.store(true, Ordering::Release);
        // As Codex's `close_agent`, the edge is marked closed in the store before the shutdown,
        // apart from the agent's own rollout, so a close already in progress cannot lose it.
        if let Some(store) = tree.rollout_store.get()
            && let Err(error) = store
                .set_thread_spawn_edge_status(target, ThreadSpawnEdgeStatus::Closed)
                .await
        {
            tracing::warn!(%target, %error, "failed to persist thread-spawn edge status");
        }
        let report = tree.close_subtree(&node, true).finished().await;
        if let Some((_, error)) = report.failed().first() {
            return Err(AgentControlError::Unsupported(error.clone()));
        }
        Ok(previous)
    }

    pub(super) async fn resume_legacy_agent(
        &self,
        target: &SessionId,
    ) -> Result<LiveAgent, AgentControlError> {
        let tree = self.tree()?;
        self.restore_legacy_children(&tree).await?;
        if tree.spawn_depth_exceeded(&self.node.path) {
            return Err(AgentControlError::Unsupported(
                DEPTH_LIMIT_REACHED.to_owned(),
            ));
        }
        if let Some(node) = tree.legacy_agent(target) {
            return if node.is_closed() {
                Err(closed_error(&node.path))
            } else {
                Ok(node.snapshot())
            };
        }
        if let Some(agent) = tree.lookup_live_agent(target) {
            return Ok(agent);
        }
        let parent = ParentRun::current().ok_or(AgentControlError::Unavailable)?;
        let node = match tree
            .reopen_legacy_agent(&self.node, target, &parent, true)
            .await
        {
            Ok(node) => node,
            Err(error) => return tree.lookup_live_agent(target).ok_or(error),
        };
        tree.restore_descendants(Arc::clone(&node), &parent).await;
        Ok(node.snapshot())
    }

    pub(super) async fn restore_legacy_children(
        &self,
        tree: &Arc<Tree>,
    ) -> Result<(), AgentControlError> {
        if !self.node.path.is_root()
            || tree.resident_limits().is_none()
            || tree.rollout_store.get().is_none()
        {
            return Ok(());
        }
        let parent = ParentRun::current().ok_or(AgentControlError::Unavailable)?;
        tree.legacy_children_loaded
            .get_or_init(|| async {
                tree.restore_descendants(Arc::clone(&self.node), &parent)
                    .await;
            })
            .await;
        Ok(())
    }
}

impl Tree {
    async fn restore_descendants(self: &Arc<Self>, node: Arc<AgentNode>, parent: &ParentRun) {
        let mut queue = VecDeque::from([node]);
        while let Some(caller) = queue.pop_front() {
            if caller.is_closed() {
                continue;
            }
            let Some(id) = caller.session_id.get() else {
                continue;
            };
            let children = match self.open_legacy_children(id, &caller.path).await {
                Ok(children) => children,
                Err(error) => {
                    tracing::warn!(%id, %error, "could not load open descendants");
                    continue;
                }
            };
            for child in children {
                let result = match self.legacy_agent(&child) {
                    Some(node) => Ok(node),
                    None => {
                        self.reopen_legacy_agent(&caller, &child, parent, false)
                            .await
                    }
                };
                match result {
                    Ok(child) if child.path.parent().as_ref() == Some(&caller.path) => {
                        queue.push_back(child);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(id = %child, %error, "could not reopen a descendant");
                    }
                }
            }
        }
    }

    async fn open_legacy_children(
        &self,
        id: &SessionId,
        path: &AgentPath,
    ) -> Result<Vec<SessionId>> {
        if let Some(store) = self.rollout_store.get() {
            store
                .list_thread_spawn_children(id, Some(ThreadSpawnEdgeStatus::Open))
                .await
        } else {
            Ok(lock(&self.closed_history)
                .iter()
                .filter(|(_, child)| {
                    !child.explicitly_closed && child.path.parent().as_ref() == Some(path)
                })
                .map(|(id, _)| id.clone())
                .collect())
        }
    }

    async fn reopen_recipe(
        &self,
        target: &SessionId,
    ) -> Result<(String, Option<AgentId>, Vec<ModelInputItem>), AgentControlError> {
        Ok(if let Some(store) = self.rollout_store.get() {
            let lineage =
                store
                    .spawned_thread_lineage(target)
                    .await
                    .map_err(|error| match error {
                        Error::Session {
                            kind: SessionErrorKind::NotFound,
                            ..
                        } => missing(target),
                        other => AgentControlError::Unsupported(other.to_string()),
                    })?;
            // A standalone thread has no agent path or role. Allocate an agent name for it,
            // as a fresh V1 spawn does, and keep its existing session ID and history.
            let (name, role) = lineage.map_or_else(
                || (uuid::Uuid::now_v7().simple().to_string(), None),
                |lineage| {
                    (
                        lineage.agent_path().name().to_owned(),
                        lineage.agent_type().cloned(),
                    )
                },
            );
            (name, role, Vec::new())
        } else {
            let closed = lock(&self.closed_history)
                .get(target)
                .cloned()
                .ok_or_else(|| missing(target))?;
            (
                closed.path.name().to_owned(),
                closed.agent_type,
                closed.history,
            )
        })
    }

    async fn reopen_legacy_agent(
        self: &Arc<Self>,
        caller: &Arc<AgentNode>,
        target: &SessionId,
        parent: &ParentRun,
        explicitly_resumed: bool,
    ) -> Result<Arc<AgentNode>, AgentControlError> {
        let (name, agent_type, mut history) = self.reopen_recipe(target).await?;
        let path = caller
            .path
            .join(&name)
            .map_err(AgentControlError::Unsupported)?;
        let agent = spawned_declaration(self, parent, agent_type.as_ref())?;
        let reservation = self.reserve(caller, &path, target)?;
        let mut guard = SpawnGuard::new(self, Arc::clone(&reservation));
        let prepared = self
            .prepare_spawned(
                &path,
                agent,
                agent_type.as_ref(),
                target,
                caller.session_id.get(),
                false,
            )
            .await?;
        let config = prepared.configure_run(parent.config().clone(), parent.initial_agent());
        let (agent, resources) = prepared.into_parts();
        guard.resources.clone_from(&resources);

        let (rollout, recorded) = if let Some(store) = self.rollout_store.get() {
            let (writer, rebuilt) = store
                .reopen_spawned_thread(target)
                .await
                .map_err(|error| AgentControlError::Unsupported(error.to_string()))?;
            history = rebuilt;
            let recorded = history.len();
            let rollout = parent.child_rollout(Some(writer));
            (rollout, recorded)
        } else {
            (None, 0)
        };
        guard.reopened_recorder.clone_from(&rollout);
        if let (Some(resources), Some(rollout)) = (&resources, &rollout) {
            resources.rollout_created(rollout);
        }
        let environment = ChildEnvironment::from_parent(
            parent,
            Arc::clone(&agent),
            agent_type,
            config,
            rollout,
            resources,
        );
        let child = ChildRuntime::new(
            environment,
            self.cancel.child(ScopeKind::custom("agent")),
            history,
            None,
        );
        lock(&child.state).recorded = recorded;
        let node = Arc::new(AgentNode::with_instance(
            path,
            Some(agent.id().clone()),
            Some(child),
            reservation.instance,
        ));
        let _ = node.session_id.set(target.clone());
        match self.commit(&reservation, &node) {
            Commit::Admitted => {
                guard.disarm();
                if explicitly_resumed {
                    self.persist_spawn_edge(caller, target).await;
                }
                Ok(node)
            }
            Commit::AdmittedClosed(error) => {
                guard.disarm();
                Err(error)
            }
            Commit::Refused(error) => Err(error),
        }
    }
}

impl Tree {
    /// Records `caller` as the open parent of the resumed `child`, as Codex's
    /// `persist_thread_spawn_edge_for_source` does after a resume: a child resumed under another
    /// agent than the one that spawned it is then restored below that agent. A failure leaves the
    /// resumed agent running and is only logged, as in Codex.
    async fn persist_spawn_edge(&self, caller: &AgentNode, child: &SessionId) {
        let (Some(store), Some(parent)) = (self.rollout_store.get(), caller.session_id.get())
        else {
            return;
        };
        if let Err(error) = store
            .upsert_thread_spawn_edge(parent, child, ThreadSpawnEdgeStatus::Open)
            .await
        {
            tracing::warn!(%child, %error, "failed to persist thread-spawn edge");
        }
    }
}
