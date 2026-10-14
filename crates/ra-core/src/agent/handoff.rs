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
//! # What each narrowing may store
//!
//! A [`HistoryProjection`] only narrows the receiving agent's **model input**; the records the turn
//! generated are stored complete whatever it says. A filter follows the reference instead: the
//! `new_items` it returns are what the turn stores, so a filter that removes tool records keeps
//! them out of the session too, while [`HandoffInputData::with_input_items`] narrows the model input
//! alone. A filter only governs records the session has not received yet, and records a projection
//! withheld from it are stored as they were. That split is why
//! [`SingleStepResult`](crate::step::SingleStepResult) carries `new_step_items` and
//! `session_step_items` separately.

use std::{collections::BTreeSet, fmt, future::Future, marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentSpec, validate_required_text};
use crate::{
    context::RunContext,
    error::{Error, ProviderErrorKind, Result},
    item::{AgentId, CallId, ModelInputItem, RunItem},
    model::ModelHandoffDefinition,
    tool::{ToolInput, ToolSchema, schema::ToolInputRequirements},
};

/// Controls how much predecessor history a handoff gives its target as model input.
///
/// This declares a model-input projection only. It never changes the authoritative session
/// history, which retains the complete records that led to the transfer. The default is
/// [`Full`](Self::Full), matching the framework's conversation-transfer contract. Restrictions
/// are explicit host extensions.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum HistoryProjection {
    /// Do not provide predecessor history to the receiving agent.
    None,
    /// Provide the complete predecessor history.
    #[default]
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
/// A filter may drop, rewrite or synthesize model-input items in any of the three source lists.
///
/// As in the reference, the returned [`HandoffInputData::new_items`] are what the transferring turn
/// stores, and the receiving agent sees them too unless [`HandoffInputData::input_items`] is set,
/// which narrows the model input alone. The filter governs only records it was handed that are
/// model input and that the session has not received yet: records an earlier write delivered, and
/// records a declared projection withheld, are stored unchanged. Calls and their outputs stay
/// paired in what is stored.
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

/// Invokes a handoff's callback before control transfers or history is filtered.
///
/// Typed declarations validate the model payload before invoking user code. The payload is
/// callback data; it does not replace the receiving agent's conversation input.
#[async_trait]
pub trait HandoffInvocationHandler: Send + Sync + 'static {
    /// Validates the payload and runs the callback, propagating failures to the run.
    async fn on_handoff(&self, context: &RunContext, arguments: &Value) -> Result<()>;
}

struct HandoffInvocationFn<F>(F);

#[async_trait]
impl<F, Fut> HandoffInvocationHandler for HandoffInvocationFn<F>
where
    F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    async fn on_handoff(&self, context: &RunContext, _arguments: &Value) -> Result<()> {
        (self.0)(context).await
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
struct HandoffArguments<T>(T);

impl<T: ToolInputRequirements> ToolInput for HandoffArguments<T> {}

struct TypedHandoffInvocation<T, F> {
    callback: F,
    input: PhantomData<T>,
}

#[async_trait]
impl<T, F, Fut> HandoffInvocationHandler for TypedHandoffInvocation<T, F>
where
    T: ToolInputRequirements,
    F: Fn(&RunContext, T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    async fn on_handoff(&self, context: &RunContext, arguments: &Value) -> Result<()> {
        // The input type is the validation authority, as TypeAdapter is upstream. Strict schema
        // normalization describes model generation; it must not turn serde defaults into runtime
        // required fields or override the input type's policy for unknown fields.
        let input = serde_json::from_value::<T>(arguments.clone()).map_err(|error| {
            Error::provider(ProviderErrorKind::Behavior, "invalid handoff input").with_source(error)
        })?;
        (self.callback)(context, input).await
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
    invocation: Option<Arc<dyn HandoffInvocationHandler>>,
}

impl HandoffSpec {
    /// Creates an always-enabled handoff inheriting the complete conversation by default.
    #[must_use]
    pub fn new(target_agent: AgentId, schema: ToolSchema) -> Self {
        Self {
            target_agent,
            schema,
            history_projection: HistoryProjection::Full,
            availability: None,
            input_filter: None,
            invocation: None,
        }
    }

    /// Derives the name, description and empty strict parameter schema from a target agent.
    ///
    /// Only the stable target identity is retained; the runtime still resolves and validates it
    /// through its agent registry, avoiding ownership cycles between mutually handing-off agents.
    pub fn from_agent(agent: &AgentSpec) -> Result<Self> {
        Ok(Self::new(
            agent.id().clone(),
            ToolSchema::new(Self::default_tool_name(agent), json!({
                "type": "object", "properties": {}, "required": [], "additionalProperties": false
            }))?.with_description(Self::default_tool_description(agent)),
        ))
    }

    /// Derives the upstream function-style name for a target.
    #[must_use]
    pub fn default_tool_name(agent: &AgentSpec) -> String {
        format!("transfer_to_{}", agent.name())
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Derives the upstream description, including the target's handoff description.
    #[must_use]
    pub fn default_tool_description(agent: &AgentSpec) -> String {
        format!(
            "Handoff to the {} agent to handle the request. {}",
            agent.name(),
            agent.handoff_description().unwrap_or_default()
        )
    }

    /// Overrides the model-facing tool name without changing routing identity.
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Result<Self> {
        let mut schema = if self.schema.strict_json_schema() {
            ToolSchema::new(name, self.schema.input_schema().clone())?
        } else {
            ToolSchema::loose(name, self.schema.input_schema().clone())?
        };
        if let Some(description) = self.schema.description() {
            schema = schema.with_description(description);
        }
        self.schema = schema;
        Ok(self)
    }

    /// Overrides the description shown to the model.
    #[must_use]
    pub fn with_tool_description(mut self, description: impl Into<String>) -> Self {
        self.schema = self.schema.with_description(description);
        self
    }

    /// Installs a custom payload-validation and invocation handler.
    #[must_use]
    pub fn with_on_handoff(mut self, handler: Arc<dyn HandoffInvocationHandler>) -> Self {
        self.invocation = Some(handler);
        self
    }

    /// Installs a callback taking only live context, ignoring the model payload as upstream does.
    #[must_use]
    pub fn with_on_handoff_fn<F, Fut>(self, callback: F) -> Self
    where
        F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.with_on_handoff(Arc::new(HandoffInvocationFn(callback)))
    }

    /// Replaces the parameter schema with a strict schema for `T` and its mandatory callback.
    ///
    /// Rust proves the callback arity and input type at construction. `Deserialize` and
    /// `JsonSchema` supply the upstream type-adapter roles without requiring a tool declaration.
    pub fn with_input_type<T, F, Fut>(mut self, callback: F) -> Result<Self>
    where
        T: ToolInputRequirements,
        F: Fn(&RunContext, T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut declaration = HandoffArguments::<T>::tool_schema(self.schema.name())?;
        if let Some(description) = self.schema.description() {
            declaration = declaration.with_description(description);
        }
        self.schema = declaration;
        self.invocation = Some(Arc::new(TypedHandoffInvocation::<T, F> {
            callback,
            input: PhantomData,
        }));
        Ok(self)
    }

    /// Runs the payload callback, or succeeds without inspecting arguments when none is set.
    pub async fn invoke(&self, context: &RunContext, arguments: &Value) -> Result<()> {
        match &self.invocation {
            Some(handler) => handler.on_handoff(context, arguments).await,
            None => Ok(()),
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

    /// Sets static enablement, replacing any dynamic availability callback.
    #[must_use]
    pub fn with_is_enabled(self, enabled: bool) -> Self {
        self.with_availability_fn(move |_| std::future::ready(Ok(enabled)))
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
            .field("on_handoff", &self.invocation.is_some())
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
    input_items: Option<Vec<RunItem>>,
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
            input_items: None,
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

    /// Optional model-input override; the turn still stores `new_items`.
    #[must_use]
    pub fn input_items(&self) -> Option<&[RunItem]> {
        self.input_items.as_deref()
    }

    /// Selects the items forwarded to the model instead of `new_items`, which the turn still stores.
    #[must_use]
    pub fn with_input_items(mut self, items: Vec<RunItem>) -> Self {
        self.input_items = Some(items);
        self
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

    /// Replaces what the transferring turn contributes and, from a filter, what it stores.
    #[must_use]
    pub fn with_new_items(mut self, new_items: Vec<RunItem>) -> Self {
        self.new_items = new_items;
        self
    }

    /// Takes the three model-input contributions apart, selecting `input_items` when set.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ModelInputItem>, Vec<RunItem>, Vec<RunItem>) {
        (
            self.input_history,
            self.pre_handoff_items,
            self.input_items.unwrap_or(self.new_items),
        )
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
                input_items: None,
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
                    input_items: None,
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
                .chain(self.input_items.as_ref().unwrap_or(&self.new_items))
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
