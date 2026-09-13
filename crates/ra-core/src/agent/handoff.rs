//! Declaring a transfer of control, and projecting the history that travels with it.
//!
//! A handoff is not a function tool: when a model calls one, the receiving agent becomes the
//! active agent and continues the run. [`HandoffSpec`] freezes the declaration; [`HandoffInputData`]
//! is what the transfer hands over, split into the three parts a projection acts on separately.
//!
//! # Two narrowings, in one order
//!
//! [`HistoryProjection`] is the **declaration's** ceiling: it is written down beside the target and
//! says how much of the predecessor's history that target may ever see. [`HandoffInputFilter`] is
//! the host's own transform, and it runs on what the projection already produced — so a filter can
//! narrow further, reshape, or summarize, but it cannot hand back context the declaration did not
//! grant, because it is never given it.
//!
//! # What a projection can never reach
//!
//! The authoritative session history. Both narrowings produce the receiving agent's **model input**;
//! the records this turn generated are stored complete either way. That split is the reason
//! [`SingleStepResult`](crate::step::SingleStepResult) carries `new_step_items` and
//! `session_step_items` separately, and it is what keeps a filter from silently deleting history
//! that a later replay, audit, or resume still needs.

use std::{collections::BTreeSet, fmt, future::Future, sync::Arc};

use async_trait::async_trait;

use super::validate_required_text;
use crate::{
    context::RunContext,
    error::{Error, Result},
    item::{AgentId, CallId, ModelInputItem, RunItem},
    model::ModelHandoffDefinition,
    tool::ToolSchema,
};

/// Controls how much predecessor history a handoff gives its target as model input.
///
/// This declares a model-input projection only. It never changes the authoritative session
/// history, which retains the complete records that led to the transfer. The default is
/// deliberately [`None`](Self::None): silently granting another agent the caller's complete
/// transcript is both a context-cost surprise and an authority expansion.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum HistoryProjection {
    /// Do not provide predecessor history to the receiving agent.
    #[default]
    None,
    /// Provide the complete predecessor history.
    Full,
    /// Provide the most recent number of input items.
    LastItems(usize),
    /// Provide a separately generated summary.
    Summary,
}

impl HistoryProjection {
    /// Validates parameterized projection forms.
    pub fn validate(&self) -> Result<()> {
        if matches!(self, Self::LastItems(0)) {
            return Err(Error::config(
                "a handoff history projection must retain at least one item",
            ));
        }
        Ok(())
    }

    /// Whether applying this projection needs a model call that settlement cannot make on its own.
    ///
    /// Only [`Summary`](Self::Summary) does. It is a single predicate with two readers — the stage
    /// that refuses to advertise such a handoff, and [`HandoffInputData::project`], which refuses to
    /// invent one — so the two cannot come to disagree about which forms are executable.
    #[must_use]
    pub const fn requires_summarizer(&self) -> bool {
        matches!(self, Self::Summary)
    }
}

/// Evaluates whether a handoff is available during a particular turn.
///
/// The handler receives the same live context as dynamic instructions and tools. It cannot
/// modify model input or session history; it only decides whether this transfer reaches the
/// current turn's action surface.
#[async_trait]
pub trait HandoffAvailabilityHandler: Send + Sync + 'static {
    /// Returns whether the handoff may be offered during this turn.
    async fn is_enabled(&self, context: &RunContext) -> Result<bool>;
}

struct HandoffAvailabilityFn<F>(F);

#[async_trait]
impl<F, Fut> HandoffAvailabilityHandler for HandoffAvailabilityFn<F>
where
    F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<bool>> + Send + 'static,
{
    async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        (self.0)(context).await
    }
}

/// Reshapes what a transfer hands the receiving agent, after the declaration's projection.
///
/// # What a filter may and may not do
///
/// It may **drop** anything and may rewrite [`HandoffInputData::input_history`] freely — that half
/// is model input the session does not own. It may **not** rewrite the records in
/// [`HandoffInputData::new_items`]: those are the records this very turn is storing, and a rewritten
/// copy carried forward under the same identity would leave the run holding two versions of one
/// record. Settlement refuses such a result rather than storing one version and sending the other.
///
/// # Cancellation
///
/// This is third-party `async` code, so the runtime awaits it inside the turn's cancellation scope
/// rather than bare. On cancellation the returned future is dropped, which is safe for a pure
/// future: an implementation that spawns a task or a child process owns draining it.
#[async_trait]
pub trait HandoffInputFilter: Send + Sync + 'static {
    /// Returns what the receiving agent is handed.
    async fn filter(
        &self,
        context: &RunContext,
        data: HandoffInputData,
    ) -> Result<HandoffInputData>;
}

struct HandoffInputFilterFn<F>(F);

#[async_trait]
impl<F, Fut> HandoffInputFilter for HandoffInputFilterFn<F>
where
    F: Fn(&RunContext, HandoffInputData) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandoffInputData>> + Send + 'static,
{
    async fn filter(
        &self,
        context: &RunContext,
        data: HandoffInputData,
    ) -> Result<HandoffInputData> {
        (self.0)(context, data).await
    }
}

/// Declarative, provider-neutral transfer of control to another agent.
///
/// The declaration carries a stable target identity rather than the target's own declaration.
/// Resolving that identity is the runtime's job, which is what lets two agents hand control to each
/// other: a declaration that embedded its target could only ever describe a tree.
#[non_exhaustive]
#[derive(Clone)]
pub struct HandoffSpec {
    target_agent: AgentId,
    schema: ToolSchema,
    history_projection: HistoryProjection,
    availability: Option<Arc<dyn HandoffAvailabilityHandler>>,
    input_filter: Option<Arc<dyn HandoffInputFilter>>,
}

impl HandoffSpec {
    /// Creates an always-enabled handoff with no predecessor history by default.
    #[must_use]
    pub fn new(target_agent: AgentId, schema: ToolSchema) -> Self {
        Self {
            target_agent,
            schema,
            history_projection: HistoryProjection::None,
            availability: None,
            input_filter: None,
        }
    }

    /// Sets the model-input history projection for the receiving agent.
    #[must_use]
    pub fn with_history_projection(mut self, history_projection: HistoryProjection) -> Self {
        self.history_projection = history_projection;
        self
    }

    /// Installs an asynchronous per-turn availability handler.
    #[must_use]
    pub fn with_availability(mut self, availability: Arc<dyn HandoffAvailabilityHandler>) -> Self {
        self.availability = Some(availability);
        self
    }

    /// Installs an asynchronous per-turn availability function.
    #[must_use]
    pub fn with_availability_fn<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<bool>> + Send + 'static,
    {
        self.availability = Some(Arc::new(HandoffAvailabilityFn(f)));
        self
    }

    /// Installs the transform applied after this declaration's history projection.
    #[must_use]
    pub fn with_input_filter(mut self, input_filter: Arc<dyn HandoffInputFilter>) -> Self {
        self.input_filter = Some(input_filter);
        self
    }

    /// Installs the transform applied after this declaration's history projection, as a function.
    #[must_use]
    pub fn with_input_filter_fn<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&RunContext, HandoffInputData) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandoffInputData>> + Send + 'static,
    {
        self.input_filter = Some(Arc::new(HandoffInputFilterFn(f)));
        self
    }

    /// Stable identity of the agent that receives control.
    #[must_use]
    pub const fn target_agent(&self) -> &AgentId {
        &self.target_agent
    }

    /// Model-facing function declaration used to invoke the transfer.
    #[must_use]
    pub const fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    /// Model-input history the receiving agent may see.
    #[must_use]
    pub const fn history_projection(&self) -> &HistoryProjection {
        &self.history_projection
    }

    /// Transform applied to what the projection produced, when this declaration installs one.
    #[must_use]
    pub const fn input_filter(&self) -> Option<&Arc<dyn HandoffInputFilter>> {
        self.input_filter.as_ref()
    }

    /// Revalidates the declaration before a registry accepts it.
    pub fn validate(&self) -> Result<()> {
        validate_required_text("handoff target agent id", self.target_agent.as_str())?;
        self.schema.validate()?;
        self.history_projection.validate()
    }

    /// Resolves the provider-neutral model declaration.
    #[must_use]
    pub fn model_definition(&self) -> ModelHandoffDefinition {
        let definition = ModelHandoffDefinition::new(
            self.target_agent.clone(),
            self.schema.name(),
            self.schema.input_schema().clone(),
        )
        .with_strict(self.schema.strict_json_schema());
        match self.schema.description() {
            Some(description) => definition.with_description(description.to_owned()),
            None => definition,
        }
    }

    /// Evaluates dynamic availability, or returns `true` for a static declaration.
    pub async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        match &self.availability {
            Some(handler) => handler.is_enabled(context).await,
            None => Ok(true),
        }
    }
}

impl fmt::Debug for HandoffSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HandoffSpec")
            .field("target_agent", &self.target_agent)
            .field("schema", &self.schema)
            .field("history_projection", &self.history_projection)
            .field("dynamic_availability", &self.availability.is_some())
            .field("input_filter", &self.input_filter.is_some())
            .finish()
    }
}

/// Everything a transfer could hand the receiving agent, in the three parts it is narrowed in.
///
/// The split is not cosmetic. `input_history` is what the caller asked for, `pre_handoff_items` is
/// what earlier turns produced, and `new_items` is what the turn that transferred control produced —
/// including the transfer records themselves. A projection or a filter that could only see one
/// flattened list would have no way to say "keep the brief, drop the deliberation", which is the
/// distinction every non-trivial handoff policy is built on.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct HandoffInputData {
    input_history: Vec<ModelInputItem>,
    pre_handoff_items: Vec<RunItem>,
    new_items: Vec<RunItem>,
}

impl HandoffInputData {
    /// Creates the complete, unprojected view of what a transfer could hand over.
    #[must_use]
    pub fn new(
        input_history: Vec<ModelInputItem>,
        pre_handoff_items: Vec<RunItem>,
        new_items: Vec<RunItem>,
    ) -> Self {
        Self {
            input_history,
            pre_handoff_items,
            new_items,
        }
    }

    /// The model input the run was started from.
    #[must_use]
    pub fn input_history(&self) -> &[ModelInputItem] {
        &self.input_history
    }

    /// Records earlier turns generated.
    #[must_use]
    pub fn pre_handoff_items(&self) -> &[RunItem] {
        &self.pre_handoff_items
    }

    /// Records the transferring turn generated, including the transfer itself.
    #[must_use]
    pub fn new_items(&self) -> &[RunItem] {
        &self.new_items
    }

    /// Replaces the run's opening input.
    #[must_use]
    pub fn with_input_history(mut self, input_history: Vec<ModelInputItem>) -> Self {
        self.input_history = input_history;
        self
    }

    /// Replaces what earlier turns contribute.
    #[must_use]
    pub fn with_pre_handoff_items(mut self, pre_handoff_items: Vec<RunItem>) -> Self {
        self.pre_handoff_items = pre_handoff_items;
        self
    }

    /// Replaces what the transferring turn contributes.
    #[must_use]
    pub fn with_new_items(mut self, new_items: Vec<RunItem>) -> Self {
        self.new_items = new_items;
        self
    }

    /// Takes the three parts apart, in the order they are read in.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ModelInputItem>, Vec<RunItem>, Vec<RunItem>) {
        (self.input_history, self.pre_handoff_items, self.new_items)
    }

    /// Applies the declared ceiling, keeping the transfer itself whatever the ceiling says.
    ///
    /// `transfer` names the handoff call that is taking control. Its records — the call and the
    /// output answering it — survive every projection, for two independent reasons: they are the
    /// brief the receiving agent was handed, and a call left in the model's input without its answer
    /// makes the next request malformed. They are also not counted against
    /// [`HistoryProjection::LastItems`], which measures predecessor history rather than the transfer.
    ///
    /// # Errors
    ///
    /// [`HistoryProjection::Summary`] is declared but cannot be applied here: producing a summary is
    /// a model call, and nothing at this seam is in a position to make one. It is refused rather
    /// than silently downgraded to `Full` or `None`, either of which would be a projection the
    /// declaration did not ask for.
    pub fn project(&self, projection: &HistoryProjection, transfer: &CallId) -> Result<Self> {
        let is_transfer = |item: &RunItem| item.call_id() == Some(transfer);
        match projection {
            HistoryProjection::Full => Ok(self.clone()),
            HistoryProjection::None => Ok(Self {
                input_history: Vec::new(),
                pre_handoff_items: Vec::new(),
                new_items: self
                    .new_items
                    .iter()
                    .filter(|item| is_transfer(item))
                    .cloned()
                    .collect(),
            }),
            HistoryProjection::LastItems(keep) => {
                let mut remaining = *keep;
                // Counted from the end, which is what "most recent" means once the three parts are
                // read in order. The transfer records are taken first and for free, so a small
                // window never costs the receiving agent the brief it is answering.
                let new_items = take_last_by(&self.new_items, &mut remaining, is_transfer);
                let pre_handoff_items =
                    take_last_by(&self.pre_handoff_items, &mut remaining, |_| false);
                let start = self.input_history.len().saturating_sub(remaining);
                Ok(Self {
                    input_history: self.input_history[start..].to_vec(),
                    pre_handoff_items,
                    new_items,
                })
            }
            HistoryProjection::Summary => Err(Error::config(
                "a `Summary` history projection needs a summarizer, which turn settlement does not \
                 have; declare `Full`, `LastItems`, or `None`",
            )),
        }
    }

    /// Flattens the three parts into one model-input list the receiving agent can be sent.
    ///
    /// Records that are not model input — a pending approval above all — drop out here, the same as
    /// they do when an ordinary turn's input is built.
    ///
    /// Pairing is repaired last, and repairing it is not optional: every narrowing above operates on
    /// whole records, so a window or a filter that keeps an answer without its question, or a
    /// question whose answer it dropped, produces a history no provider accepts. Dropping the
    /// unpaired half is the only repair that cannot invent content.
    #[must_use]
    pub fn into_model_input(self) -> Vec<ModelInputItem> {
        let mut items = self.input_history;
        items.extend(
            self.pre_handoff_items
                .iter()
                .chain(&self.new_items)
                .filter_map(RunItem::to_model_input),
        );
        repair_pairing(items)
    }
}

/// Keeps the last `remaining` items, plus every item `always` claims, in original order.
///
/// `remaining` is decremented by what the window consumed so the caller can spend the rest of the
/// allowance on the part before this one.
fn take_last_by(
    items: &[RunItem],
    remaining: &mut usize,
    always: impl Fn(&RunItem) -> bool,
) -> Vec<RunItem> {
    let mut taken = Vec::new();
    for item in items.iter().rev() {
        if always(item) {
            taken.push(item.clone());
            continue;
        }
        if *remaining == 0 {
            continue;
        }
        *remaining -= 1;
        taken.push(item.clone());
    }
    taken.reverse();
    taken
}

/// The identity two records pair on, when they are two halves of one exchange.
fn pairing_id(item: &ModelInputItem) -> Option<(bool, &str)> {
    match item {
        ModelInputItem::ToolCall(call) => Some((true, call.call_id().as_str())),
        ModelInputItem::HandoffCall(call) => Some((true, call.call_id().as_str())),
        ModelInputItem::McpApprovalRequest(request) => Some((true, request.request_id())),
        ModelInputItem::ToolCallOutput(output) => Some((false, output.call_id().as_str())),
        ModelInputItem::HandoffOutput(output) => Some((false, output.call_id().as_str())),
        ModelInputItem::McpApprovalResponse(response) => Some((false, response.request_id())),
        _ => None,
    }
}

/// Drops every half of an exchange whose other half the narrowing above removed.
fn repair_pairing(items: Vec<ModelInputItem>) -> Vec<ModelInputItem> {
    let mut asked = BTreeSet::new();
    let mut answered = BTreeSet::new();
    for item in &items {
        match pairing_id(item) {
            Some((true, id)) => {
                asked.insert(id.to_owned());
            }
            Some((false, id)) => {
                answered.insert(id.to_owned());
            }
            None => {}
        }
    }
    items
        .into_iter()
        .filter(|item| match pairing_id(item) {
            Some((true, id)) => answered.contains(id),
            Some((false, id)) => asked.contains(id),
            None => true,
        })
        .collect()
}
