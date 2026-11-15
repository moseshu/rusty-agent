//! Which threads are being closed, and by what: Codex's pending unloads, extended for removal.
//!
//! Codex keeps a set of thread ids being unloaded and refuses to resume them. Here two operations
//! close threads: unloading an idle thread, and archiving or deleting a stored subtree. Each holds
//! the ids it is closing and lets go of its own hold only, so an unload that fails does not reopen
//! a thread a removal is still closing.
//!
//! A removal's hold says that one instance of a thread — the loaded thread, or the spawned agent,
//! that holds its writer — has not been confirmed closed yet. It does not belong to the request
//! that made it: whichever removal later confirms that instance closed lets go of it, so a retry
//! takes over what an earlier request left. A confirmation about one instance never lets go of a
//! hold on another, so a late result about a thread closed before cannot reopen the same thread
//! resumed and being closed again. A hold also names the removal targets whose subtree froze it,
//! which is how a retry finds a descendant that neither the store nor any tree can show any more.

use std::collections::{BTreeSet, HashMap, HashSet};

use ra_core::session::SessionId;
use ra_runtime::agent::control::AgentInstance;

/// The instance of a thread a removal is closing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeldInstance {
    /// A thread loaded on the server, by its load generation.
    Loaded(u64),
    /// A spawned agent in a loaded thread's tree.
    Spawned(AgentInstance),
    /// A thread nothing is running: held only while the request runs, so it is not resumed under
    /// it.
    Stored,
}

#[derive(Debug, Default)]
struct Holds {
    idle_unload: bool,
    removal: Option<RemovalHold>,
}

#[derive(Debug)]
struct RemovalHold {
    instance: HeldInstance,
    targets: BTreeSet<SessionId>,
}

/// The threads being closed.
#[derive(Debug, Default)]
pub(crate) struct UnloadLedger {
    holds: HashMap<SessionId, Holds>,
}

impl UnloadLedger {
    /// Whether anything is closing `id`, so it must not be resumed.
    pub(crate) fn is_held(&self, id: &SessionId) -> bool {
        self.holds.contains_key(id)
    }

    /// Holds `id` for an idle unload, unless something already holds it.
    pub(crate) fn hold_idle_unload(&mut self, id: &SessionId) -> bool {
        if self.holds.contains_key(id) {
            return false;
        }
        self.holds.entry(id.clone()).or_default().idle_unload = true;
        true
    }

    /// Lets go of an idle unload's hold on `id`, leaving a removal's.
    pub(crate) fn release_idle_unload(&mut self, id: &SessionId) {
        if let Some(holds) = self.holds.get_mut(id) {
            holds.idle_unload = false;
        }
        self.forget_if_free(id);
    }

    /// Holds `instance` of `id` for the removal of `target`'s subtree. A hold on the same instance
    /// is taken over, with `target` added to the subtrees that froze it; one on another instance is
    /// replaced.
    pub(crate) fn hold_removal(
        &mut self,
        id: &SessionId,
        instance: HeldInstance,
        target: &SessionId,
    ) {
        let holds = self.holds.entry(id.clone()).or_default();
        match &mut holds.removal {
            Some(hold) if hold.instance == instance => {
                hold.targets.insert(target.clone());
            }
            removal => {
                *removal = Some(RemovalHold {
                    instance,
                    targets: BTreeSet::from([target.clone()]),
                });
            }
        }
    }

    /// Lets go of the removal hold on `id` if it is on `instance`: that instance was confirmed
    /// closed.
    pub(crate) fn release_removal(&mut self, id: &SessionId, instance: &HeldInstance) {
        if let Some(holds) = self.holds.get_mut(id)
            && holds
                .removal
                .as_ref()
                .is_some_and(|hold| hold.instance == *instance)
        {
            holds.removal = None;
        }
        self.forget_if_free(id);
    }

    /// The removal holds frozen by a removal of any of `targets`, with their instances.
    pub(crate) fn removal_holds_for(
        &self,
        targets: &HashSet<SessionId>,
    ) -> Vec<(SessionId, HeldInstance)> {
        self.holds
            .iter()
            .filter_map(|(id, holds)| {
                let hold = holds.removal.as_ref()?;
                hold.targets
                    .iter()
                    .any(|target| targets.contains(target))
                    .then(|| (id.clone(), hold.instance.clone()))
            })
            .collect()
    }

    /// The instance a removal holds `id` on, if any.
    pub(crate) fn removal_instance(&self, id: &SessionId) -> Option<HeldInstance> {
        self.holds
            .get(id)
            .and_then(|holds| holds.removal.as_ref())
            .map(|hold| hold.instance.clone())
    }

    fn forget_if_free(&mut self, id: &SessionId) {
        if self
            .holds
            .get(id)
            .is_some_and(|holds| !holds.idle_unload && holds.removal.is_none())
        {
            self.holds.remove(id);
        }
    }
}
