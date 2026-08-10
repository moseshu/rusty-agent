//! R3-12: the two agent identities a turn runs under, and which one every consumer reads.
//!
//! A run has one agent from the user's point of view and, once R10 assembles capabilities and R11
//! prepares a sandbox, potentially a different one that actually executes. Keeping a single
//! `Arc<AgentSpec>` in a variable called `agent` is what quietly merges them: the prepared clone
//! flows onward and from then on the tool-use tracker keys on it, records are attributed to it, and
//! hooks report it — so a user who configured `coder` is shown events belonging to something they
//! never wrote down. The clone did not announce anything; it just became the identity.
//!
//! [`AgentBinding`] removes the variable that could be either. There are two accessors with
//! different names, and every call site has to say which one it wants:
//!
//! | Question | Accessor |
//! | --- | --- |
//! | What runs? | [`AgentBinding::execution`] |
//! | Who is this attributed to? | [`AgentBinding::public_id`] / [`AgentBinding::public`] |
//!
//! There is deliberately no `execution_id()`. Reaching an id for attribution has exactly one short
//! path, and getting at the execution instance's own id means writing `execution().id()` — which
//! reads as what it is, a look inside the thing that ran, rather than as an identity to file
//! records under.
//!
//! # What this type does not check
//!
//! [`AgentBinding::prepared`] does not verify that the execution instance was derived from the
//! public one. Nothing in an [`AgentSpec`] records that, and inventing a naming convention to infer
//! it would make identity depend on string shape — the mistake
//! [`ToolOrigin`](ra_core::tool::ToolOrigin) keeps its qualified name out of dispatch to avoid.
//! What the type does guarantee is that a prepared instance cannot reach the framework *without* a
//! public agent named beside it.
//!
//! `as_tool` — the sub-agent shape, where a whole run becomes one tool call — is R12-2's and lands
//! in this module beside the binding, since a nested run needs both identities too.

use std::{fmt, sync::Arc};

use ra_core::{agent::AgentSpec, item::AgentId};

/// The public agent a turn is attributed to, bound to the instance that executes it.
///
/// Cloning is cheap and does not fork either identity: both sides are shared [`Arc`]s, so a clone
/// of the binding is the same two agents, not a third one.
#[non_exhaustive]
#[derive(Clone)]
pub struct AgentBinding {
    public: Arc<AgentSpec>,
    execution: Arc<AgentSpec>,
}

impl AgentBinding {
    /// The user's agent runs exactly as they configured it.
    ///
    /// Both sides are the same object, so there is no identity to get wrong. This is the shape
    /// every run has until capability assembly (R10) or sandbox preparation (R11) produces a
    /// different executable instance.
    #[must_use]
    pub fn direct(agent: Arc<AgentSpec>) -> Self {
        Self {
            execution: Arc::clone(&agent),
            public: agent,
        }
    }

    /// A prepared instance stands in for the user's agent.
    ///
    /// Taking both by value is the whole contract: a preparation step cannot hand its output
    /// onward as *the* agent, because the only way to produce a runnable binding from it is to name
    /// the public agent it stands in for.
    #[must_use]
    pub fn prepared(public: Arc<AgentSpec>, execution: Arc<AgentSpec>) -> Self {
        Self { public, execution }
    }

    /// The agent as the user configured it. Use it for display and for reading declared
    /// configuration; it is **not** what runs.
    #[must_use]
    pub const fn public(&self) -> &Arc<AgentSpec> {
        &self.public
    }

    /// The identity every record, count, hook, and span is filed under.
    ///
    /// Attribution follows the public agent even when something else executed, which is the rule
    /// the whole type exists for: an execution-time clone must not overwrite the identity the user
    /// can see.
    #[must_use]
    pub fn public_id(&self) -> &AgentId {
        self.public.id()
    }

    /// The instance that actually runs: its tools, its model, its settings.
    ///
    /// Turn preparation reads this and nothing else. Resolving the advertised surface from the
    /// public agent instead would offer tools the prepared instance cannot run, and the mismatch
    /// would only surface when the model called one of them.
    #[must_use]
    pub const fn execution(&self) -> &Arc<AgentSpec> {
        &self.execution
    }

    /// Whether a preparation step replaced the executable instance.
    ///
    /// Compares object identity rather than agent IDs on purpose. A prepared clone is free to keep
    /// the public agent's ID — it is still the same agent, differently assembled — and asking
    /// "are these two IDs different?" would answer `false` for exactly that case while the tools
    /// being run had in fact been swapped.
    #[must_use]
    pub fn is_prepared(&self) -> bool {
        !Arc::ptr_eq(&self.public, &self.execution)
    }
}

impl fmt::Debug for AgentBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBinding")
            .field("public_id", self.public_id())
            .field("execution_id", self.execution.id())
            .field("prepared", &self.is_prepared())
            .finish_non_exhaustive()
    }
}
