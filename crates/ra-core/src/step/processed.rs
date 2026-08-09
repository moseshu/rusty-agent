//! R3-2: one model response classified into typed, already-bound actions.
//!
//! A raw provider call says only "the model named `write_file`". Every later stage needs a
//! different answer — which executable object runs this, is it a control transfer, does a human
//! decide first — and re-deriving that answer at each call site is how a runner ends up re-typing
//! provider payloads in four places. Binding happens once, here, and settlement reads actions.
//!
//! # Why there is no `shell_calls` / `apply_patch_calls` / `computer_actions`
//!
//! The reference implementation splits those out because each is a distinct provider payload with
//! its own execution path. This framework has exactly one local execution contract —
//! [`Tool`] plus [`ToolLookupKey`] dispatch — and a shell or patch tool is an ordinary
//! implementation of it living in a product crate. A category per product tool would put product
//! vocabulary in the kernel and put `if name == "shell"` back in the runner, which is the
//! text-driven control flow R7-10 forbids. The categories below are the ones that make settlement
//! *behave* differently: run it, transfer control, ask a human, or answer a call that bound to
//! nothing.
//!
//! # Two things this type deliberately derives instead of storing
//!
//! [`ProcessedResponse::interruptions`] and [`ProcessedResponse::tools_used`] are projections over
//! the stored categories, not fields. A stored copy is a second source of truth that can disagree
//! with the first — the same reason `Recoverability` is derived from the error variant rather than
//! stored beside it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use crate::{
    error::{Error, Result},
    item::{
        AgentId, CallId, HandoffCall, ItemId, McpApprovalRequest, RunItem, RunItemKind, ToolCall,
    },
    tool::{Tool, ToolLookupKey},
};

/// A model call bound to the executable tool that will run it.
///
/// The binding is the point. Holding the resolved [`Tool`] means the execution stage cannot pick a
/// different implementation than the one the turn advertised — which is what a second name lookup
/// against the agent's *declared* tools would allow, quietly running a tool that dynamic
/// availability turned off for this turn.
#[non_exhaustive]
#[derive(Clone)]
pub struct ToolRunFunction {
    item_id: ItemId,
    call: ToolCall,
    tool: Arc<dyn Tool>,
}

impl ToolRunFunction {
    /// ID of the record in [`ProcessedResponse::new_items`] this action came from.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// The decoded call.
    #[must_use]
    pub const fn call(&self) -> &ToolCall {
        &self.call
    }

    /// ID pairing this call with the output it owes.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        self.call.call_id()
    }

    /// The resolved executable.
    #[must_use]
    pub const fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }
}

impl fmt::Debug for ToolRunFunction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRunFunction")
            .field("item_id", &self.item_id)
            .field("call_id", self.call.call_id())
            .field("tool", &self.tool.origin().qualified_name())
            .finish_non_exhaustive()
    }
}

/// A model call bound to the handoff target it named.
///
/// It carries an [`AgentId`] rather than an `Arc<AgentSpec>` because resolving an identity to a
/// declaration needs the agent registry R17 owns. [`NextStep::Handoff`](super::NextStep::Handoff)
/// is where the resolved declaration belongs; this is the evidence that produces it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ToolRunHandoff {
    item_id: ItemId,
    call: HandoffCall,
}

impl ToolRunHandoff {
    /// ID of the record in [`ProcessedResponse::new_items`] this action came from.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// The decoded handoff call.
    #[must_use]
    pub const fn call(&self) -> &HandoffCall {
        &self.call
    }

    /// ID pairing this call with the output it owes. A handoff reaches the wire as an ordinary
    /// function call, so it owes one exactly like any other tool.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        self.call.call_id()
    }

    /// Stable identity of the agent taking over.
    #[must_use]
    pub const fn target_agent(&self) -> &AgentId {
        self.call.target_agent()
    }
}

/// A hosted-tool call the host has to approve before the server may run it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ToolRunApproval {
    item_id: ItemId,
    request: McpApprovalRequest,
}

impl ToolRunApproval {
    /// ID of the record in [`ProcessedResponse::new_items`] this action came from.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// The decoded approval request.
    #[must_use]
    pub const fn request(&self) -> &McpApprovalRequest {
        &self.request
    }

    /// ID the host's decision has to quote.
    #[must_use]
    pub fn request_id(&self) -> &str {
        self.request.request_id()
    }
}

/// A call naming something the turn did not advertise.
///
/// This is not a framework error and must not end the run. The usual answer is a model-visible
/// failure observation paired to [`Self::call_id`], which is also why the call is kept whole: the
/// next request is invalid without an output for it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ToolNotFound {
    item_id: ItemId,
    call: ToolCall,
}

impl ToolNotFound {
    /// ID of the record in [`ProcessedResponse::new_items`] this action came from.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// The decoded call.
    #[must_use]
    pub const fn call(&self) -> &ToolCall {
        &self.call
    }

    /// ID pairing this call with the failure output it owes.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        self.call.call_id()
    }

    /// The name the model used.
    #[must_use]
    pub fn name(&self) -> &str {
        self.call.name()
    }
}

/// One action identity the model invoked during a turn.
///
/// R3-6b tracks repeat calls on these values, so a plain name would not do: a namespaced tool and
/// a bare tool can legitimately share a model-facing name across agents, and counting them as one
/// is exactly the "统计按可重名的 tool name" mistake that milestone names.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolUse {
    /// A local tool, identified by its collision-free routing key.
    Tool(ToolLookupKey),
    /// A control transfer to another agent.
    Handoff(AgentId),
    /// A hosted MCP tool on a named server.
    Mcp {
        /// Registered server name.
        server: String,
        /// Tool name on that server.
        tool_name: String,
    },
    /// A name the turn did not advertise. It still counts as an attempt, which is what
    /// `reset_tool_choice` (R3-6) reacts to.
    Unresolved(String),
}

/// One model response, classified.
///
/// Construction goes through [`ProcessedResponseBuilder`] so an action and the record it came from
/// are always added together. Handing out `Vec` fields would let the two drift, and a typed action
/// whose record is missing from `new_items` is a call the session never learns about.
///
/// This type is deliberately **not** serializable. It holds `Arc<dyn Tool>`, and a resolved tool
/// object is not state — R6-6 persists [`ToolLookupKey`]s and rebinds them against the live
/// registry on resume, which is the only way a restored run can refuse to call a tool that no
/// longer exists instead of silently calling a different one.
#[non_exhaustive]
#[derive(Clone)]
pub struct ProcessedResponse {
    new_items: Vec<RunItem>,
    handoffs: Vec<ToolRunHandoff>,
    functions: Vec<ToolRunFunction>,
    mcp_approval_requests: Vec<ToolRunApproval>,
    tools_not_found: Vec<ToolNotFound>,
}

impl ProcessedResponse {
    /// Starts an empty classification.
    pub fn builder() -> ProcessedResponseBuilder {
        ProcessedResponseBuilder::new()
    }

    /// Every record the response produced, in response order.
    #[must_use]
    pub fn new_items(&self) -> &[RunItem] {
        &self.new_items
    }

    /// Control transfers requested this turn.
    #[must_use]
    pub fn handoffs(&self) -> &[ToolRunHandoff] {
        &self.handoffs
    }

    /// Local tool calls bound to their executables.
    #[must_use]
    pub fn functions(&self) -> &[ToolRunFunction] {
        &self.functions
    }

    /// Hosted-tool calls waiting on a host decision.
    #[must_use]
    pub fn mcp_approval_requests(&self) -> &[ToolRunApproval] {
        &self.mcp_approval_requests
    }

    /// Calls that bound to nothing the turn advertised.
    #[must_use]
    pub fn tools_not_found(&self) -> &[ToolNotFound] {
        &self.tools_not_found
    }

    /// Records the host has to answer before the run continues.
    ///
    /// Derived from [`RunItemKind::is_interruption`] rather than stored, so this list and
    /// [`NextStep::interruption`](super::NextStep::interruption)'s validation can never disagree
    /// about what counts as a pending decision.
    pub fn interruptions(&self) -> impl Iterator<Item = &RunItem> {
        self.new_items
            .iter()
            .filter(|item| item.kind().is_interruption())
    }

    /// Whether the run has to stop and ask.
    #[must_use]
    pub fn has_interruptions(&self) -> bool {
        self.interruptions().next().is_some()
    }

    /// Whether settlement owes work before the next model call.
    ///
    /// **Unresolved calls count**, and that is the one place this differs from the reference
    /// implementation. A call that bound to nothing is not *run*, but it is still owed an output:
    /// leave it unanswered and the next request carries a tool call with no result, which every
    /// provider rejects. Excluding it would make `false` mean "nothing left to do" in a case where
    /// there very much is — the silent direction.
    #[must_use]
    pub fn has_tools_or_approvals_to_run(&self) -> bool {
        !self.functions.is_empty()
            || !self.handoffs.is_empty()
            || !self.mcp_approval_requests.is_empty()
            || !self.tools_not_found.is_empty()
    }

    /// Action identities invoked this turn, in response order, without repeats.
    #[must_use]
    pub fn tools_used(&self) -> Vec<ToolUse> {
        let mut by_item: BTreeMap<&ItemId, ToolUse> = BTreeMap::new();
        for action in &self.functions {
            by_item.insert(
                &action.item_id,
                ToolUse::Tool(action.tool.origin().lookup_key().clone()),
            );
        }
        for action in &self.handoffs {
            by_item.insert(
                &action.item_id,
                ToolUse::Handoff(action.target_agent().clone()),
            );
        }
        for action in &self.mcp_approval_requests {
            by_item.insert(
                &action.item_id,
                ToolUse::Mcp {
                    server: action.request.server().to_owned(),
                    tool_name: action.request.tool_name().to_owned(),
                },
            );
        }
        for action in &self.tools_not_found {
            by_item.insert(
                &action.item_id,
                ToolUse::Unresolved(action.name().to_owned()),
            );
        }

        let mut seen = BTreeSet::new();
        let mut used = Vec::new();
        for item in &self.new_items {
            if let Some(use_) = by_item.get(item.id())
                && seen.insert(use_.clone())
            {
                used.push(use_.clone());
            }
        }
        used
    }
}

impl fmt::Debug for ProcessedResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessedResponse")
            .field("new_items", &self.new_items.len())
            .field("handoffs", &self.handoffs.len())
            .field("functions", &self.functions.len())
            .field("mcp_approval_requests", &self.mcp_approval_requests.len())
            .field("tools_not_found", &self.tools_not_found.len())
            .field("interruptions", &self.interruptions().count())
            .finish_non_exhaustive()
    }
}

/// Builds a [`ProcessedResponse`] one record at a time.
///
/// Every method takes the whole [`RunItem`] and reads the call out of it rather than accepting a
/// separately supplied call. A signature that took both would allow a bound action whose `call_id`
/// does not match its own record, and that mismatch surfaces much later as an output paired to the
/// wrong call.
#[must_use]
#[derive(Default)]
pub struct ProcessedResponseBuilder {
    new_items: Vec<RunItem>,
    handoffs: Vec<ToolRunHandoff>,
    functions: Vec<ToolRunFunction>,
    mcp_approval_requests: Vec<ToolRunApproval>,
    tools_not_found: Vec<ToolNotFound>,
}

impl fmt::Debug for ProcessedResponseBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessedResponseBuilder")
            .field("new_items", &self.new_items.len())
            .field("handoffs", &self.handoffs.len())
            .field("functions", &self.functions.len())
            .field("mcp_approval_requests", &self.mcp_approval_requests.len())
            .field("tools_not_found", &self.tools_not_found.len())
            .finish_non_exhaustive()
    }
}

impl ProcessedResponseBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an item that carries no action: a message, reasoning, or a control-plane record.
    pub fn item(mut self, item: RunItem) -> Self {
        self.new_items.push(item);
        self
    }

    /// Records a call bound to the tool that will run it.
    ///
    /// The tool's advertised name has to match the name the model used. A classifier that bound
    /// the wrong implementation would otherwise run it under the model's name and report success.
    pub fn function(mut self, item: RunItem, tool: Arc<dyn Tool>) -> Result<Self> {
        let call = tool_call(&item, "a bound function call")?.clone();
        if call.name() != tool.origin().name() {
            return Err(Error::caller(format!(
                "call `{}` was bound to tool `{}`, which advertises a different name",
                call.name(),
                tool.origin().qualified_name()
            )));
        }
        self.functions.push(ToolRunFunction {
            item_id: item.id().clone(),
            call,
            tool,
        });
        self.new_items.push(item);
        Ok(self)
    }

    /// Records a call bound to the agent it transfers control to.
    ///
    /// The item may be either wire form: an ordinary [`RunItemKind::ToolCall`], which is how a
    /// handoff actually reaches a provider, or an adapter-typed [`RunItemKind::HandoffCall`]. In
    /// the second case the item's own target has to agree with `target_agent`, because a
    /// disagreement means the adapter and the turn's advertised surface resolved the same name to
    /// two different agents.
    pub fn handoff(mut self, item: RunItem, target_agent: AgentId) -> Result<Self> {
        let call = match item.kind() {
            RunItemKind::ToolCall(call) => HandoffCall::new(
                call.call_id().clone(),
                target_agent,
                call.arguments().clone(),
            )
            .with_tool_name(call.name()),
            RunItemKind::HandoffCall(call) => {
                if call.target_agent() != &target_agent {
                    return Err(Error::caller(format!(
                        "handoff item targets agent `{}` but the turn resolved the call to `{}`",
                        call.target_agent(),
                        target_agent
                    )));
                }
                call.clone()
            }
            kind => {
                return Err(Error::caller(format!(
                    "run item kind `{}` cannot be classified as a handoff",
                    kind.label()
                )));
            }
        };
        self.handoffs.push(ToolRunHandoff {
            item_id: item.id().clone(),
            call,
        });
        self.new_items.push(item);
        Ok(self)
    }

    /// Records a hosted-tool call waiting on a host decision.
    pub fn mcp_approval(mut self, item: RunItem) -> Result<Self> {
        let RunItemKind::McpApprovalRequest(request) = item.kind() else {
            return Err(Error::caller(format!(
                "run item kind `{}` is not an MCP approval request",
                item.kind().label()
            )));
        };
        self.mcp_approval_requests.push(ToolRunApproval {
            item_id: item.id().clone(),
            request: request.clone(),
        });
        self.new_items.push(item);
        Ok(self)
    }

    /// Records a call that bound to nothing this turn advertised.
    pub fn tool_not_found(mut self, item: RunItem) -> Result<Self> {
        let call = tool_call(&item, "an unresolved call")?.clone();
        self.tools_not_found.push(ToolNotFound {
            item_id: item.id().clone(),
            call,
        });
        self.new_items.push(item);
        Ok(self)
    }

    /// Validates cross-category identity and returns the classification.
    pub fn build(self) -> Result<ProcessedResponse> {
        let mut item_ids = BTreeSet::new();
        for item in &self.new_items {
            if !item_ids.insert(item.id()) {
                return Err(Error::caller(format!(
                    "response item id `{}` appears more than once; session reconciliation \
                     identifies records by id",
                    item.id()
                )));
            }
        }

        // A call filed through `item()` instead of a classifier is the failure this whole type
        // exists to prevent: `has_tools_or_approvals_to_run()` would answer "nothing to do" while
        // the response still holds a call nobody will answer, and the symptom lands one request
        // later as a malformed history.
        let claimed: BTreeSet<&ItemId> = self
            .functions
            .iter()
            .map(|action| &action.item_id)
            .chain(self.handoffs.iter().map(|action| &action.item_id))
            .chain(
                self.mcp_approval_requests
                    .iter()
                    .map(|action| &action.item_id),
            )
            .chain(self.tools_not_found.iter().map(|action| &action.item_id))
            .collect();
        for item in &self.new_items {
            if item.kind().requires_action_binding() && !claimed.contains(item.id()) {
                return Err(Error::caller(format!(
                    "response item `{}` of kind `{}` was recorded without being classified; \
                     an unanswered call cannot be filed as an inert record",
                    item.id(),
                    item.kind().label()
                )));
            }
        }

        // One call gets one output. Two actions on one `call_id` produce two, and a provider that
        // sees a duplicated output either rejects the request or keeps the wrong one.
        let mut call_ids = BTreeSet::new();
        let call_id_sources = self
            .functions
            .iter()
            .map(ToolRunFunction::call_id)
            .chain(self.handoffs.iter().map(ToolRunHandoff::call_id))
            .chain(self.tools_not_found.iter().map(ToolNotFound::call_id));
        for call_id in call_id_sources {
            if !call_ids.insert(call_id) {
                return Err(Error::caller(format!(
                    "call id `{call_id}` is claimed by more than one action in the same response"
                )));
            }
        }

        let mut request_ids = BTreeSet::new();
        for approval in &self.mcp_approval_requests {
            if !request_ids.insert(approval.request_id()) {
                return Err(Error::caller(format!(
                    "MCP approval request id `{}` appears more than once in the same response",
                    approval.request_id()
                )));
            }
        }

        Ok(ProcessedResponse {
            new_items: self.new_items,
            handoffs: self.handoffs,
            functions: self.functions,
            mcp_approval_requests: self.mcp_approval_requests,
            tools_not_found: self.tools_not_found,
        })
    }
}

fn tool_call<'a>(item: &'a RunItem, expected: &str) -> Result<&'a ToolCall> {
    match item.kind() {
        RunItemKind::ToolCall(call) => Ok(call),
        kind => Err(Error::caller(format!(
            "run item kind `{}` cannot be classified as {expected}",
            kind.label()
        ))),
    }
}
