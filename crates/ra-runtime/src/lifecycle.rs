//! What a run's lifecycle narration is, at both scopes, and how a transfer of control moves it.
//!
//! The contract is [`ra_core::lifecycle`]; [`dispatch`] is the execution. What is here is the one
//! thing between them: which hooks hear about a moment, and which scope each of them speaks as.
//!
//! # Two sets, and only one of them moves
//!
//! The run-scoped set is installed on the run and stays for its whole length. The agent-scoped set
//! belongs to whichever agent is currently running, so it is **replaced** when control transfers —
//! see [`LifecycleHooks::rebound`]. Keeping the departing agent's hooks would be the more forgiving
//! behaviour and the wrong one: an agent-scoped hook exists to describe what its own agent did, and
//! one that kept reporting after the handoff would attribute another agent's model calls and tool
//! runs to it.
//!
//! # Order is the order they were declared in
//!
//! Run scope first, then agent scope, each in declaration order — the same rule
//! [`UserHooks`](crate::hook::UserHooks) follows, and for the same reason: a hook's name is a
//! display label that two hooks may share, so there is no identity to sort by and no registration
//! to refuse. What the order buys is that when two hooks fail at one moment, which failure the run
//! reports does not depend on scheduling.
//!
//! # A host installs this one, and it decides nothing
//!
//! Both scopes are supplied from outside — an agent's through its declaration, a run's through
//! [`RunConfig`](crate::runner::RunConfig) — and neither can refuse anything. That is why this set
//! needs no matcher, no verdict, and no reduction rule, which is the whole difference between it
//! and the hook registry next door.

/// Telling one moment's hooks about it.
///
/// `Internal` but technically reachable, the same deliberate trade [`turn`](crate::turn) carries
/// and for the same reason: tests live in a separate workspace, so a module with no `pub` path has
/// no way to be tested at all. Every moment is reachable through the runner, which is where they
/// are covered. No compatibility promise.
#[doc(hidden)]
pub mod dispatch;

use core::fmt;
use std::sync::Arc;

use ra_core::lifecycle::{LifecycleHook, LifecycleScope};

/// What a run narrates to, at both scopes.
///
/// Cloning shares everything: it is built once per run and handed to every turn, model call and
/// tool call under it, and a per-call copy of two vectors would be paid for on every dispatch to
/// answer a question whose answer does not change between them.
#[must_use]
#[non_exhaustive]
#[derive(Clone, Default)]
pub struct LifecycleHooks {
    run: Arc<Vec<Arc<dyn LifecycleHook>>>,
    agent: Arc<Vec<Arc<dyn LifecycleHook>>>,
}

impl LifecycleHooks {
    /// Nothing installed at either scope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes what a run and its starting agent installed.
    pub fn installed(run: &[Arc<dyn LifecycleHook>], agent: &[Arc<dyn LifecycleHook>]) -> Self {
        Self {
            run: Arc::new(run.to_vec()),
            agent: Arc::new(agent.to_vec()),
        }
    }

    /// Moves the agent-scoped half onto the agent that now speaks, keeping the run-scoped half.
    ///
    /// This is what a handoff does to narration, and it is a value rather than a mutation so the
    /// set a call already holds cannot change under it mid-turn.
    pub fn rebound(&self, agent: &[Arc<dyn LifecycleHook>]) -> Self {
        Self {
            run: Arc::clone(&self.run),
            agent: Arc::new(agent.to_vec()),
        }
    }

    /// Whether nothing is installed at either scope.
    ///
    /// The ordinary case, and what every dispatch point checks first: a run with no narration should
    /// not pay for a span, a future, or a borrow of the run context to discover that.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.run.is_empty() && self.agent.is_empty()
    }

    /// Every hook told about a moment, run scope first and each in declaration order.
    pub(crate) fn all_scoped(&self) -> impl Iterator<Item = (LifecycleScope, &dyn LifecycleHook)> {
        let run = self
            .run
            .iter()
            .map(|hook| (LifecycleScope::Run, &**hook as &dyn LifecycleHook));
        let agent = self
            .agent
            .iter()
            .map(|hook| (LifecycleScope::Agent, &**hook as &dyn LifecycleHook));
        run.chain(agent)
    }
}

impl fmt::Debug for LifecycleHooks {
    /// Prints who is installed at each scope. The hooks themselves are trait objects with nothing
    /// to show.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let run: Vec<&str> = self.run.iter().map(|hook| hook.name()).collect();
        let agent: Vec<&str> = self.agent.iter().map(|hook| hook.name()).collect();
        formatter
            .debug_struct("LifecycleHooks")
            .field(LifecycleScope::Run.code(), &run)
            .field(LifecycleScope::Agent.code(), &agent)
            .finish()
    }
}
