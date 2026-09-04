//! Capability assembly and dependency-topology validation.
//!
//! [`Capability`] declares what one installable unit contributes. This module is where a set of
//! them becomes one agent, and it owns the two decisions no single capability can make: whether the
//! installed set is coherent, and what order it is folded in.
//!
//! # Order is not a presentation choice
//!
//! Three of the four contributions are order-sensitive. The sampling fold hands each capability
//! what the ones before it produced, prompt fragments reach the cached prefix in the order they are
//! assembled, and context processors run as a chain over the same request. A set with no defined
//! order is therefore a set whose prompt text and model settings are decided by whichever sequence
//! the host happened to write down — and two capabilities that disagree resolve by luck.
//!
//! A declared dependency is consequently also an ordering edge: a `memory` capability that requires
//! `shell` is assembled after it, so it folds onto the shell settings and its fragment follows the
//! shell fragment. Everything a dependency does not constrain keeps installation order.
//!
//! # Deviation from the reference contract
//!
//! The reference implementation checks that every required type is present — a set difference with
//! no topology in it — and then assembles in declaration order. Two things are added here, both
//! because ordering carries meaning in this framework that it does not carry there:
//!
//! - **A dependency orders as well as requires.** Declaration order alone accepts installing
//!   `memory` before the `shell` it depends on and silently folds the dependent first, which is the
//!   one arrangement its own declaration says is wrong.
//! - **A cycle is rejected rather than resolved by iteration accident.** Two capabilities that
//!   require each other have no assembly order at all, and picking one by list position would make
//!   the resulting prompt and settings an artifact of how the host typed them.
//!
//! One capability per family is likewise enforced here rather than upstream: a dependency names a
//! family, so a family that names two objects cannot say which one it means.
//!
//! # Deferred prompt text is resolved here and delivered later
//!
//! [`Capability::deferred_instructions`] is read during assembly like every other contribution, so
//! a capability resolves its text once per run rather than on the turn its signal happens to fire —
//! a slow source would otherwise pay its cost in the middle of a turn, and a failing one would end
//! a run that had been going fine. What waits is the *delivery*: the turn loop asks
//! [`Capability::wants_deferred_instructions`] before each turn and, the first time one answers
//! yes, writes that fragment into the run's history as a tail message.
//!
//! **Nothing here records which fragments have been delivered.** A delivered fragment is in the
//! history, under an ID derived from its family, and that is the answer — a `delivered` flag beside
//! it would be a second copy of the same fact, and the two would first disagree on the resume path,
//! where the history survives a checkpoint and a flag on an in-memory assembly does not.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer, LoadSignal,
    },
    context::RunContext,
    error::{Error, Result},
    item::{ItemId, Message, RunItem, RunItemKind},
    prompt::{PromptSection, PromptSectionName, PromptSource, SectionPosition},
    tool::Tool,
};

/// The capabilities installed for one run, validated and frozen in assembly order.
///
/// Resolving is separate from assembling because the two answer to different lifetimes: the plan
/// is a property of the configuration and can be checked before a run exists, while assembly binds
/// each capability to the run it is about to serve. A host that wants its configuration errors at
/// startup rather than at the first run calls [`CapabilityPlan::resolve`] itself.
#[must_use]
#[non_exhaustive]
#[derive(Clone, Default)]
pub struct CapabilityPlan {
    ordered: Vec<Arc<dyn Capability>>,
}

impl CapabilityPlan {
    /// Validates the installed set and freezes the order assembly will use.
    ///
    /// The order is installation order wherever dependencies leave it free, and dependency order
    /// wherever they do not: each capability is placed after every family it requires and otherwise
    /// as early as it can go, so a declared dependency costs one swap rather than a rearrangement.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when two capabilities claim one family, when a declared
    /// dependency names a family nothing installs, when a capability declares its own family, or
    /// when a group of them depends in a circle. Every message names the capabilities involved,
    /// because the fix is always to install or remove one of them.
    pub fn resolve(installed: impl IntoIterator<Item = Arc<dyn Capability>>) -> Result<Self> {
        let installed: Vec<Arc<dyn Capability>> = installed.into_iter().collect();
        let families: Vec<CapabilityFamily> = installed
            .iter()
            .map(|capability| capability.kind())
            .collect();
        let requirements: Vec<BTreeSet<CapabilityFamily>> = installed
            .iter()
            .map(|capability| capability.required_capabilities())
            .collect();

        let mut provider_of: BTreeMap<CapabilityFamily, usize> = BTreeMap::new();
        for (index, family) in families.iter().enumerate() {
            if let Some(first) = provider_of.insert(family.clone(), index) {
                return Err(Error::config(format!(
                    "capability family `{family}` is installed twice, in positions {first} and \
                     {index}; a dependency names a family rather than an object, so two \
                     capabilities sharing one family leave `required_capabilities` no way to say \
                     which of them it means. Give one of them a family of its own, such as \
                     `plugin.{family}`, or install only one"
                )));
            }
        }

        // Reported for the whole set at once. A configuration error should say everything that has
        // to be fixed; naming one missing dependency per attempt turns a two-line correction into
        // two runs.
        let unsatisfied: Vec<String> = families
            .iter()
            .zip(&requirements)
            .filter_map(|(family, required)| {
                let absent: Vec<String> = required
                    .iter()
                    .filter(|family| !provider_of.contains_key(family))
                    .map(|family| format!("`{family}`"))
                    .collect();
                (!absent.is_empty())
                    .then(|| format!("`{family}` requires {}", absent.join(" and ")))
            })
            .collect();
        if !unsatisfied.is_empty() {
            return Err(Error::config(format!(
                "these capabilities declare dependencies that nothing installs: {}. Install a \
                 capability for each named family, or remove the capability that needs it",
                unsatisfied.join("; ")
            )));
        }

        // A capability that names its own family is a one-node cycle, and it is worth its own
        // message: the cycle report below would name one capability twice and read like a bug in
        // the report.
        for (family, required) in families.iter().zip(&requirements) {
            if required.contains(family) {
                return Err(Error::config(format!(
                    "capability `{family}` declares its own family as a dependency; nothing can be \
                     assembled after itself"
                )));
            }
        }

        Ok(Self {
            ordered: order_by_dependency(installed, &families, &requirements, &provider_of)?,
        })
    }

    /// The installed capabilities, in the order assembly folds them.
    #[must_use]
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.ordered
    }

    /// The installed families, in assembly order.
    #[must_use]
    pub fn families(&self) -> Vec<CapabilityFamily> {
        self.ordered
            .iter()
            .map(|capability| capability.kind())
            .collect()
    }

    /// Whether a capability of this family is installed.
    #[must_use]
    pub fn contains(&self, family: &CapabilityFamily) -> bool {
        self.ordered
            .iter()
            .any(|capability| &capability.kind() == family)
    }

    /// Number of installed capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    /// Whether nothing is installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// The static prompt fragments the installed capabilities contribute, in assembly order.
    ///
    /// **Read from the installed capabilities rather than from bound ones, and that is the point
    /// rather than a shortcut.** These fragments go into a cached prefix, which is one span shared
    /// by every run an agent serves — so a fragment that only exists once a run has been bound is a
    /// fragment that cannot be in it. A caller that assembles a prefix before any run exists (an
    /// agent builder, a prompt dump) reads them here, which is also the only place it can: binding
    /// takes a [`RunContext`], and at that point there is no run to hand it.
    ///
    /// This is deliberately a separate contribution from the per-run fragments
    /// [`Self::assemble`] reads through [`Capability::instructions`]. A bound capability may
    /// legitimately produce different per-run text; reading that method here would both violate
    /// its binding lifecycle and resolve it twice.
    ///
    /// # Errors
    ///
    /// Returns whatever resolving a fragment produced, a section not named and attributed to its
    /// own family, a section placed outside the cached prefix, and two capabilities claiming one
    /// section name.
    pub async fn static_prompt_sections(&self) -> Result<Vec<PromptSection>> {
        collect_static_prompt_sections(&self.ordered).await
    }

    /// Binds every capability to this run and collects what the bound forms contribute.
    ///
    /// Binding happens first and for the whole set, before any contribution is read, because
    /// [`Capability::bind`] is what gives a capability the run it serves — a fragment or a tool
    /// read from the unbound value would describe a different run than the one about to start.
    ///
    /// # Errors
    ///
    /// Propagates a binding failure, a failure to resolve a prompt fragment, a bound form that
    /// changed its family, and a fragment that asks for a placement outside the cached prefix.
    pub async fn assemble(&self, context: &RunContext) -> Result<AssembledCapabilities> {
        let mut capabilities: Vec<Arc<dyn Capability>> = Vec::with_capacity(self.ordered.len());
        for installed in &self.ordered {
            let family = installed.kind();
            let bound = installed
                .bind(context)
                .map_err(|error| error.with_context(format!("binding capability `{family}`")))?;
            match bound {
                Some(bound) if bound.kind() != family => {
                    return Err(Error::caller(format!(
                        "capability `{family}` bound as `{}`; a bound form stands in for the \
                         capability it came from, and both the dependency check and the assembly \
                         order have already been decided against the installed family",
                        bound.kind()
                    )));
                }
                Some(bound) => capabilities.push(bound),
                None => capabilities.push(Arc::clone(installed)),
            }
        }

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut context_processors: Vec<Arc<dyn ContextProcessor>> = Vec::new();

        for capability in &capabilities {
            tools.extend(capability.tools());
            if capability.context_processor().is_some() {
                context_processors.push(Arc::new(CapabilityContextProcessor {
                    capability: Arc::clone(capability),
                }));
            }
        }

        let prompt_sections = collect_run_prompt_sections(&capabilities).await?;
        let deferred_prompts = collect_deferred_prompts(&capabilities).await?;

        Ok(AssembledCapabilities {
            capabilities,
            tools,
            prompt_sections,
            deferred_prompts,
            context_processors,
        })
    }
}

/// Resolves one ordered capability list's installed, static prompt fragments.
///
/// This is intentionally not shared with the per-run reading: the two methods are different
/// lifecycle points. They share validation below, so the structural prefix rules stay identical.
async fn collect_static_prompt_sections(
    capabilities: &[Arc<dyn Capability>],
) -> Result<Vec<PromptSection>> {
    let mut sections: Vec<PromptSection> = Vec::new();
    let mut owners: BTreeMap<PromptSectionName, CapabilityFamily> = BTreeMap::new();

    for capability in capabilities {
        let family = capability.kind();
        let section = capability.static_instructions().await.map_err(|error| {
            error.with_context(format!(
                "resolving the static prompt fragment of capability `{family}`"
            ))
        })?;
        record_prompt_section(
            &mut sections,
            &mut owners,
            &family,
            section,
            PromptChannel::Prefix,
        )?;
    }

    Ok(sections)
}

/// Resolves one ordered capability list's per-run prompt fragments.
async fn collect_run_prompt_sections(
    capabilities: &[Arc<dyn Capability>],
) -> Result<Vec<PromptSection>> {
    let mut sections: Vec<PromptSection> = Vec::new();
    let mut owners: BTreeMap<PromptSectionName, CapabilityFamily> = BTreeMap::new();

    for capability in capabilities {
        let family = capability.kind();
        let section = capability.instructions().await.map_err(|error| {
            error.with_context(format!(
                "resolving the prompt fragment of capability `{family}`"
            ))
        })?;
        record_prompt_section(
            &mut sections,
            &mut owners,
            &family,
            section,
            PromptChannel::Prefix,
        )?;
    }

    Ok(sections)
}

/// Resolves one ordered capability list's deferred prompt fragments.
///
/// Read from the bound capabilities and in assembly order, like the per-run fragments: a deferred
/// fragment never reaches the cached prefix, so per-run text is legitimate here in a way it is not
/// in the other two channels.
async fn collect_deferred_prompts(
    capabilities: &[Arc<dyn Capability>],
) -> Result<Vec<DeferredPrompt>> {
    let mut prompts: Vec<DeferredPrompt> = Vec::new();
    let mut sections: Vec<PromptSection> = Vec::new();
    let mut owners: BTreeMap<PromptSectionName, CapabilityFamily> = BTreeMap::new();

    for capability in capabilities {
        let family = capability.kind();
        let section = capability.deferred_instructions().await.map_err(|error| {
            error.with_context(format!(
                "resolving the deferred prompt fragment of capability `{family}`"
            ))
        })?;
        let before = sections.len();
        record_prompt_section(
            &mut sections,
            &mut owners,
            &family,
            section,
            PromptChannel::Deferred,
        )?;
        if sections.len() != before {
            prompts.push(DeferredPrompt {
                capability: Arc::clone(capability),
                record_id: deferred_record_id(&family),
                section: sections[before].clone(),
            });
        }
    }

    Ok(prompts)
}

/// The history record ID one family's deferred fragment is delivered under.
///
/// Derived from the family rather than from the turn that delivers it, because it is what
/// "delivered already" is answered with: a per-turn ID would make the same fragment arrive again on
/// every turn after the signal, which is the per-turn charge deferring exists to avoid.
fn deferred_record_id(family: &CapabilityFamily) -> ItemId {
    ItemId::new(format!("capability-prompt.{family}"))
}

/// Which of the two capability prompt channels a fragment was offered to.
///
/// The channels differ in exactly two things — the section name a fragment must claim and the
/// position it must ask for — so they are one function with a parameter rather than two functions
/// that would drift on the checks they share.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptChannel {
    /// Text that joins the agent's cached prefix.
    Prefix,
    /// Text delivered into run history once a signal fires.
    Deferred,
}

impl PromptChannel {
    /// The section name a fragment in this channel must claim.
    fn expected_name(self, family: &CapabilityFamily) -> PromptSectionName {
        match self {
            Self::Prefix => family.prompt_section_name(),
            Self::Deferred => family.deferred_prompt_section_name(),
        }
    }

    /// Whether a fragment in this channel may ask for this position.
    fn accepts(self, position: SectionPosition) -> bool {
        match self {
            Self::Prefix => position.is_prefix(),
            Self::Deferred => position.is_tail_message(),
        }
    }
}

/// Validates and records one capability-owned fragment in one channel.
fn record_prompt_section(
    sections: &mut Vec<PromptSection>,
    owners: &mut BTreeMap<PromptSectionName, CapabilityFamily>,
    family: &CapabilityFamily,
    section: Option<PromptSection>,
    channel: PromptChannel,
) -> Result<()> {
    let Some(section) = section else {
        return Ok(());
    };

    let expected_source = family.prompt_source();
    if section.source() != &expected_source {
        return Err(Error::config(format!(
            "capability `{family}` contributes prompt section `{}` with source `{}`; \
             capability prompt text must use source `{expected_source}` so prompt dumps and \
             assembly errors attribute it to the capability that wrote it",
            section.name(),
            section.source()
        )));
    }
    let expected_name = channel.expected_name(family);
    if section.name() != &expected_name {
        return Err(Error::config(format!(
            "capability `{family}` contributes prompt section `{}`; capability prompt text must \
             claim its own section `{expected_name}` so it cannot take another topic's prefix slot",
            section.name()
        )));
    }
    if !channel.accepts(section.position()) {
        return Err(Error::config(match channel {
            PromptChannel::Prefix => format!(
                "capability `{family}` places its prompt section `{}` at `{}`; a fragment resolved \
                 once per run belongs in the cached prefix, and text that varies per turn belongs \
                 in a context processor, which runs against the live request and writes into the \
                 tail",
                section.name(),
                section.position()
            ),
            PromptChannel::Deferred => format!(
                "capability `{family}` places its deferred prompt section `{}` at `{}`; deferred \
                 text is delivered into run history as a tail message, and asking for the prefix \
                 makes it resident text with a delay on it — charged on every turn of every run, \
                 including the ones that never fire its signal",
                section.name(),
                section.position()
            ),
        }));
    }
    if let Some(owner) = owners.insert(section.name().clone(), family.clone()) {
        return Err(Error::config(format!(
            "capabilities `{owner}` and `{family}` both contribute the prompt section `{}`; one \
             of the two would be gone from the prefix with nothing said about it",
            section.name()
        )));
    }
    sections.push(section);

    Ok(())
}

/// One capability's deferred fragment, resolved and waiting for the signal that delivers it.
///
/// It holds the capability rather than a copy of its predicate: the decision is the capability's
/// own, and asking it directly is what lets an implementation arm on something the assembly layer
/// has no way to know about.
#[derive(Clone)]
pub struct DeferredPrompt {
    capability: Arc<dyn Capability>,
    record_id: ItemId,
    section: PromptSection,
}

impl DeferredPrompt {
    /// The family whose text this is.
    #[must_use]
    pub fn family(&self) -> CapabilityFamily {
        self.capability.kind()
    }

    /// The resolved fragment.
    #[must_use]
    pub const fn section(&self) -> &PromptSection {
        &self.section
    }

    /// The history record ID this fragment is delivered under.
    ///
    /// Stable for the family, so a history that already holds it is the record of the delivery.
    #[must_use]
    pub const fn record_id(&self) -> &ItemId {
        &self.record_id
    }

    /// Whether this turn's facts have earned the fragment.
    #[must_use]
    pub fn is_signalled(&self, signal: &LoadSignal<'_>) -> bool {
        self.capability.wants_deferred_instructions(signal)
    }

    /// The authoritative record that delivers this fragment into a run's history.
    ///
    /// A user tail message, matching the cross-protocol lowering of dynamic prompts and reminders.
    /// A system message in input history is not portable: for example, Anthropic accepts system
    /// text only in its top-level instruction field. It is a record rather than an ephemeral tail
    /// item because it is delivered once: an item that vanished after the turn that delivered it
    /// would leave the model with a mechanism it read exactly one time, and re-sending it to fix
    /// that is the per-turn charge deferring exists to avoid.
    #[must_use]
    pub fn to_run_item(&self) -> RunItem {
        RunItem::new(
            self.record_id.clone(),
            RunItemKind::Message(Message::user(self.section.content().to_owned())),
        )
    }
}

impl fmt::Debug for DeferredPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredPrompt")
            .field("family", &self.family())
            .field("record_id", &self.record_id)
            .field("section", self.section.name())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for CapabilityPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityPlan")
            .field("families", &self.families())
            .finish()
    }
}

/// Places every capability after the families it requires, keeping installation order otherwise.
///
/// The ready set is drained by smallest installed position, so each capability lands as early as
/// its dependencies allow. That makes the result the topological order closest to what the host
/// wrote down — a dependency edge moves the two capabilities it joins past each other and leaves
/// everything else where it was, rather than sending the dependent behind every unrelated
/// capability installed after it.
fn order_by_dependency(
    installed: Vec<Arc<dyn Capability>>,
    families: &[CapabilityFamily],
    requirements: &[BTreeSet<CapabilityFamily>],
    provider_of: &BTreeMap<CapabilityFamily, usize>,
) -> Result<Vec<Arc<dyn Capability>>> {
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); installed.len()];
    let mut pending: Vec<usize> = vec![0; installed.len()];
    for (index, required) in requirements.iter().enumerate() {
        pending[index] = required.len();
        for family in required {
            // Every required family has a provider: an absent one was reported above.
            if let Some(&provider) = provider_of.get(family) {
                dependents[provider].push(index);
            }
        }
    }

    let mut ready: BTreeSet<usize> = (0..installed.len())
        .filter(|index| pending[*index] == 0)
        .collect();
    let mut order: Vec<usize> = Vec::with_capacity(installed.len());
    while let Some(&next) = ready.iter().next() {
        ready.remove(&next);
        order.push(next);
        for &dependent in &dependents[next] {
            pending[dependent] -= 1;
            if pending[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }

    if order.len() != installed.len() {
        let cycle = find_cycle(&pending, requirements, provider_of);
        let rendered: Vec<String> = cycle
            .iter()
            .chain(cycle.first())
            .map(|index| format!("`{}`", families[*index]))
            .collect();
        return Err(Error::config(format!(
            "these capabilities require one another in a circle: {}. Assembly order is what makes \
             the sampling fold, the prompt order, and the context-processor chain deterministic, \
             and a circle has no order to pick",
            rendered.join(" -> ")
        )));
    }

    // Rebuilt by index rather than by removing from the source, so the reordering is one move of
    // each element and the source list is never left with holes to reason about.
    let mut ordered: Vec<Option<Arc<dyn Capability>>> = installed.into_iter().map(Some).collect();
    Ok(order
        .into_iter()
        .filter_map(|index| ordered[index].take())
        .collect())
}

/// Finds one cycle among the capabilities that never became ready.
///
/// A capability with unmet requirements after the topological drain has at least one required
/// family whose provider is also still unmet, so following that edge can never dead-end — which
/// makes revisiting a node the only way the walk can end, and the revisited node the entry into a
/// cycle.
fn find_cycle(
    pending: &[usize],
    requirements: &[BTreeSet<CapabilityFamily>],
    provider_of: &BTreeMap<CapabilityFamily, usize>,
) -> Vec<usize> {
    let blocked = |index: usize| pending[index] > 0;
    let mut path: Vec<usize> = Vec::new();
    let mut visited: BTreeMap<usize, usize> = BTreeMap::new();
    let Some(mut current) = (0..pending.len()).find(|index| blocked(*index)) else {
        return Vec::new();
    };
    loop {
        if let Some(&position) = visited.get(&current) {
            return path.split_off(position);
        }
        visited.insert(current, path.len());
        path.push(current);
        let next = requirements[current]
            .iter()
            .filter_map(|family| provider_of.get(family).copied())
            .find(|index| blocked(*index));
        match next {
            Some(next) => current = next,
            None => return path,
        }
    }
}

/// What one run's capabilities contributed, in assembly order.
///
/// The contributions are kept apart rather than merged into one prepared object, because they are
/// applied in three different places: tools, prompt text, and sampling settings become the agent
/// instance that executes, context processors are installed on the run, and deferred fragments are
/// held by the turn loop until one of them is earned. Merging them would mean inventing a home for
/// the group that has no other reason to exist.
#[non_exhaustive]
pub struct AssembledCapabilities {
    capabilities: Vec<Arc<dyn Capability>>,
    tools: Vec<Arc<dyn Tool>>,
    prompt_sections: Vec<PromptSection>,
    deferred_prompts: Vec<DeferredPrompt>,
    context_processors: Vec<Arc<dyn ContextProcessor>>,
}

impl AssembledCapabilities {
    /// The bound capabilities serving this run, in assembly order.
    #[must_use]
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.capabilities
    }

    /// The assembled families, in assembly order.
    #[must_use]
    pub fn families(&self) -> Vec<CapabilityFamily> {
        self.capabilities
            .iter()
            .map(|capability| capability.kind())
            .collect()
    }

    /// Tools the capabilities contribute, in assembly order.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Prompt fragments the capabilities contribute, in assembly order.
    #[must_use]
    pub fn prompt_sections(&self) -> &[PromptSection] {
        &self.prompt_sections
    }

    /// Fragments waiting for a signal, in assembly order.
    ///
    /// Resolved but undelivered: none of these is in the agent's prefix, and a run whose signals
    /// never fire never pays for one.
    #[must_use]
    pub fn deferred_prompts(&self) -> &[DeferredPrompt] {
        &self.deferred_prompts
    }

    /// Context transformations the capabilities contribute, in assembly order.
    #[must_use]
    pub fn context_processors(&self) -> &[Arc<dyn ContextProcessor>] {
        &self.context_processors
    }

    /// Whether no capability was installed at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Produces the agent instance that runs with these capabilities installed.
    ///
    /// The result is the execution instance, never the identity a run is attributed to: pair it
    /// with the agent the user configured through
    /// [`AgentBinding::prepared`](crate::agent::AgentBinding::prepared).
    ///
    /// Three of the four contributions land here. Tools are appended to the agent's own, the
    /// sampling fold starts from the agent's settings layer and replaces it, and prompt fragments
    /// follow the agent's static instructions in assembly order. The fourth — context processing —
    /// is not the agent's to hold and is installed on the run configuration instead.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a capability's tool collides with one the agent already
    /// declares, and when a capability contributes prefix text to an agent whose own instructions
    /// are generated per turn: a generated prompt may only reach the volatile tail, so the two
    /// cannot share the one instruction slot an agent has.
    pub fn prepare_agent(&self, agent: &AgentSpec) -> Result<Arc<AgentSpec>> {
        let settings = self
            .capabilities
            .iter()
            .fold(agent.model_settings().clone(), |settings, capability| {
                capability.sampling_params(settings)
            });

        let mut builder = agent
            .to_builder()
            .tools(self.tools.iter().map(Arc::clone))
            .model_settings(settings);

        if let Some(fragments) = self.prompt_text() {
            let instructions = match agent.instructions() {
                None => fragments,
                Some(instructions) => match instructions.as_static() {
                    Some(text) => format!("{}\n\n{fragments}", text.trim_end()),
                    None => {
                        return Err(Error::config(format!(
                            "agent `{}` generates its instructions per turn, and capabilities {} \
                             contribute prompt text for the cached prefix; a generated prompt \
                             reaches the volatile tail only, so the two cannot share the one \
                             instruction slot an agent has. Make the agent's own instructions \
                             static, or move the per-turn text into a reminder",
                            agent.id(),
                            self.section_owners()
                        )));
                    }
                },
            };
            builder = builder.instructions(instructions);
        }

        builder.build().map_err(|error| {
            error.with_context(format!(
                "assembling capabilities {} onto agent `{}`",
                self.rendered_families(),
                agent.id()
            ))
        })
    }

    /// The prompt text these capabilities contribute, or `None` when they contribute none.
    fn prompt_text(&self) -> Option<String> {
        let fragments: Vec<&str> = self
            .prompt_sections
            .iter()
            .map(|section| section.content().trim())
            .filter(|content| !content.is_empty())
            .collect();
        (!fragments.is_empty()).then(|| fragments.join("\n\n"))
    }

    /// The families that contributed prompt text, for an error that has to name them.
    ///
    /// Read back from each section's own provenance rather than from the assembled family list:
    /// the error is about the capabilities that wrote text, and naming one that contributed none
    /// would send the reader to the wrong capability.
    fn section_owners(&self) -> String {
        let owners: Vec<String> = self
            .prompt_sections
            .iter()
            .map(|section| match section.source() {
                PromptSource::Capability(family) => format!("`{family}`"),
                source => format!("`{source}`"),
            })
            .collect();
        owners.join(", ")
    }

    /// Every assembled family, for an error that has to name them.
    fn rendered_families(&self) -> String {
        let families: Vec<String> = self
            .families()
            .iter()
            .map(|family| format!("`{family}`"))
            .collect();
        families.join(", ")
    }
}

impl fmt::Debug for AssembledCapabilities {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools: Vec<&str> = self
            .tools
            .iter()
            .map(|tool| tool.origin().qualified_name())
            .collect();
        let sections: Vec<&PromptSectionName> = self
            .prompt_sections
            .iter()
            .map(PromptSection::name)
            .collect();
        formatter
            .debug_struct("AssembledCapabilities")
            .field("families", &self.families())
            .field("tools", &tools)
            .field("prompt_sections", &sections)
            .field("deferred_prompts", &self.deferred_prompts)
            .field("context_processor_count", &self.context_processors.len())
            .finish_non_exhaustive()
    }
}

/// Presents a capability's own context transform as an installable processor.
///
/// [`Capability::context_processor`] lends a reference for the duration of a borrow, while a run
/// holds its processors as owned values for the duration of the run. This adapter is the join: it
/// owns the bound capability and forwards each call to the processor that capability returns, so
/// the transform stays the capability's own implementation rather than a copy assembly made of it.
struct CapabilityContextProcessor {
    capability: Arc<dyn Capability>,
}

#[async_trait]
impl ContextProcessor for CapabilityContextProcessor {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        // Assembly installed this adapter because the capability offered a processor. Passing the
        // input through unchanged instead would make a compaction that stopped answering look like
        // a run that had nothing to compact.
        let Some(processor) = self.capability.context_processor() else {
            return Err(Error::caller(format!(
                "capability `{}` offered a context processor when the run was assembled and none \
                 when the turn ran; its transform would silently not happen",
                self.capability.kind()
            )));
        };
        processor.process_context(request, summarizer).await
    }
}
