//! Declarative tool policies consumed by the common executor.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::ToolCaller;
use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
};

/// Current tool-options schema version.
pub const TOOL_OPTIONS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Whether a tool is available in the current run.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAvailability {
    /// Always advertise and dispatch the tool, subject to caller policy.
    #[default]
    Enabled,
    /// Keep the tool registered but do not advertise or dispatch it.
    Disabled,
    /// Ask [`Tool::is_enabled`](super::Tool::is_enabled) for each relevant context.
    Dynamic,
}

/// How much of the model's tool surface a registered tool occupies.
///
/// **Registered and advertised are different questions**, which is why this is not a boolean.
/// Every variant here is registered and dispatchable; they differ only in what the model can see,
/// and each one has a consumer that has to tell them apart:
///
/// | Variant | In the turn's tool list | Found by `tool_search` |
/// | --- | --- | --- |
/// | [`Advertised`](Self::Advertised) | yes | — |
/// | [`Deferred`](Self::Deferred) | no | yes |
/// | [`Hidden`](Self::Hidden) | no | no |
///
/// [`Deferred`](Self::Deferred) is the mechanism behind a 15-entry tool surface with 40-plus
/// reachable capabilities (R2-5c): the schema costs nothing until the model asks for it.
///
/// # Why three states and not Codex's six
///
/// Codex crosses two axes into one enum — visibility (direct / deferred / hidden) times surface
/// (model / nested code-mode) — giving `DirectModelOnly`, `CodeModeOnly`, and so on. This project
/// already carries the second axis as [`ToolOptions::allowed_callers`], so crossing it in again
/// would create pairs that can contradict each other: a tool exposed only to a programmatic
/// caller whose allowlist admits only [`ToolCaller::Direct`] is a state with no meaning and no
/// error. One axis per field, and the two are combined where they are read.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    /// In every turn's tool list, and paying for its schema every turn.
    #[default]
    Advertised,
    /// Withheld from the tool list until discovery surfaces it (R2-5c).
    Deferred,
    /// Never shown to the model; reachable only when something else dispatches it.
    Hidden,
}

/// Whether a tool may run while other tools from the same response are running.
///
/// **The default is [`Exclusive`](Self::Exclusive)**: a tool that has not said it tolerates
/// company gets none. The opposite default would make every tool written before this field
/// existed silently eligible for concurrent execution.
///
/// Measured, not assumed: Codex's `exec_command` and `shell_command` both declare parallel, and
/// `apply_patch` declares nothing and therefore serializes — one writer against every reader is
/// the whole rule. The batch executor (R3-4b) reads this to pick a read or a write lock, which is
/// why a tool cannot express "parallel with these, not with those": that would need a resource
/// identity, and a resource identity is what a later variant here would carry.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    /// Runs alone: nothing else from the same response overlaps it.
    #[default]
    Exclusive,
    /// Runs alongside other parallel-declared calls from the same response.
    Parallel,
}

/// Whether host approval is needed before invocation.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalPolicy {
    /// Never interrupt for approval.
    #[default]
    Never,
    /// Every call requires approval.
    Always,
    /// Ask [`Tool::needs_approval`](super::Tool::needs_approval) for each call.
    Dynamic,
}

/// How executor-generated invocation failures become run outcomes.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureHandling {
    /// Return a structured, model-visible error observation.
    #[default]
    ModelVisible,
    /// Propagate the framework error and stop the turn.
    Propagate,
    /// Delegate formatting to a tool-specific handler supplied by the runtime adapter.
    Custom,
}

/// Behavior when a tool exceeds its invocation timeout.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTimeoutBehavior {
    /// Return a model-visible timeout observation.
    #[default]
    ModelVisible,
    /// Propagate a timeout error and stop the turn.
    Propagate,
}

/// Stable registry identity for a tool input/output guardrail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolGuardrailId(String);

impl ToolGuardrailId {
    /// Creates a non-empty guardrail identity.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            return Err(Error::caller(
                "tool guardrail ID must be non-empty, trimmed, and contain no control characters",
            ));
        }
        Ok(Self(value))
    }

    /// Stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ToolGuardrailId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolGuardrailId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Unified metadata controlling exposure and execution.
///
/// Callback objects are intentionally absent: they are runtime resources and cannot safely enter
/// a persisted config. `Dynamic` and `Custom` values announce which default `Tool` method the
/// executor must call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOptions {
    schema_version: SchemaVersion,
    #[serde(default)]
    availability: ToolAvailability,
    #[serde(default)]
    approval: ToolApprovalPolicy,
    #[serde(default)]
    exposure: ToolExposure,
    #[serde(default)]
    concurrency: ToolConcurrency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_callers: Option<Vec<ToolCaller>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_duration",
        deserialize_with = "deserialize_optional_duration"
    )]
    timeout: Option<Duration>,
    #[serde(default)]
    timeout_behavior: ToolTimeoutBehavior,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    input_guardrails: Vec<ToolGuardrailId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    output_guardrails: Vec<ToolGuardrailId>,
    #[serde(default)]
    failure_handling: ToolFailureHandling,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[derive(Deserialize)]
struct ToolOptionsWire {
    schema_version: SchemaVersion,
    #[serde(default)]
    availability: ToolAvailability,
    #[serde(default)]
    approval: ToolApprovalPolicy,
    #[serde(default)]
    exposure: ToolExposure,
    #[serde(default)]
    concurrency: ToolConcurrency,
    #[serde(default)]
    allowed_callers: Option<Vec<ToolCaller>>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    timeout: Option<Duration>,
    #[serde(default)]
    timeout_behavior: ToolTimeoutBehavior,
    #[serde(default)]
    input_guardrails: Vec<ToolGuardrailId>,
    #[serde(default)]
    output_guardrails: Vec<ToolGuardrailId>,
    #[serde(default)]
    failure_handling: ToolFailureHandling,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for ToolOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolOptionsWire::deserialize(deserializer)?;
        // `defer_loading: bool` was this field before `exposure` split "not advertised" from
        // "not discoverable". Rejecting it is the point: an unknown key round-trips into
        // `unknown` and the tool would read as `Advertised`, which is the one wrong answer that
        // costs schema budget every turn without anyone noticing.
        if wire.unknown.get("defer_loading").is_some() {
            return Err(D::Error::custom(
                "`defer_loading` was replaced by `exposure`: use \"deferred\" or \"hidden\"",
            ));
        }
        let mut allowed_callers = wire.allowed_callers;
        if let Some(callers) = &mut allowed_callers {
            callers.sort_unstable();
            callers.dedup();
        }
        Ok(Self {
            schema_version: wire.schema_version,
            availability: wire.availability,
            approval: wire.approval,
            exposure: wire.exposure,
            concurrency: wire.concurrency,
            allowed_callers,
            timeout: wire.timeout,
            timeout_behavior: wire.timeout_behavior,
            input_guardrails: deduplicate_guardrails(wire.input_guardrails),
            output_guardrails: deduplicate_guardrails(wire.output_guardrails),
            failure_handling: wire.failure_handling,
            unknown: wire.unknown,
        })
    }
}

impl Default for ToolOptions {
    fn default() -> Self {
        Self {
            schema_version: TOOL_OPTIONS_SCHEMA_VERSION,
            availability: ToolAvailability::Enabled,
            approval: ToolApprovalPolicy::Never,
            exposure: ToolExposure::Advertised,
            concurrency: ToolConcurrency::Exclusive,
            allowed_callers: None,
            timeout: None,
            timeout_behavior: ToolTimeoutBehavior::ModelVisible,
            input_guardrails: Vec::new(),
            output_guardrails: Vec::new(),
            failure_handling: ToolFailureHandling::ModelVisible,
            unknown: Unknown::new(),
        }
    }
}

impl ToolOptions {
    /// Creates the default policy set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets static or dynamic availability.
    #[must_use]
    pub const fn with_availability(mut self, availability: ToolAvailability) -> Self {
        self.availability = availability;
        self
    }

    /// Sets approval behavior.
    #[must_use]
    pub const fn with_approval(mut self, approval: ToolApprovalPolicy) -> Self {
        self.approval = approval;
        self
    }

    /// Sets how much of the model's tool surface this tool occupies.
    #[must_use]
    pub const fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// Declares whether this tool tolerates running beside other calls from the same response.
    #[must_use]
    pub const fn with_concurrency(mut self, concurrency: ToolConcurrency) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Restricts allowed caller classes. An empty list denies every caller.
    #[must_use]
    pub fn with_allowed_callers(mut self, callers: impl IntoIterator<Item = ToolCaller>) -> Self {
        let mut callers = callers.into_iter().collect::<Vec<_>>();
        callers.sort_unstable();
        callers.dedup();
        self.allowed_callers = Some(callers);
        self
    }

    /// Sets the per-invocation timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets timeout behavior.
    #[must_use]
    pub const fn with_timeout_behavior(mut self, behavior: ToolTimeoutBehavior) -> Self {
        self.timeout_behavior = behavior;
        self
    }

    /// Adds an input guardrail by stable registry ID.
    #[must_use]
    pub fn with_input_guardrail(mut self, guardrail: ToolGuardrailId) -> Self {
        push_unique(&mut self.input_guardrails, guardrail);
        self
    }

    /// Adds an output guardrail by stable registry ID.
    #[must_use]
    pub fn with_output_guardrail(mut self, guardrail: ToolGuardrailId) -> Self {
        push_unique(&mut self.output_guardrails, guardrail);
        self
    }

    /// Sets invocation failure behavior.
    #[must_use]
    pub const fn with_failure_handling(mut self, handling: ToolFailureHandling) -> Self {
        self.failure_handling = handling;
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Availability policy.
    #[must_use]
    pub const fn availability(&self) -> ToolAvailability {
        self.availability
    }

    /// Approval policy.
    #[must_use]
    pub const fn approval(&self) -> ToolApprovalPolicy {
        self.approval
    }

    /// Model-surface exposure.
    #[must_use]
    pub const fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    /// Concurrency declaration read by the batch executor (R3-4b).
    #[must_use]
    pub const fn concurrency(&self) -> ToolConcurrency {
        self.concurrency
    }

    /// Whether this tool belongs in the turn's advertised tool list.
    ///
    /// The consumer is turn preparation (R3-0 stage 1). Named rather than left as a `match` at
    /// the call site because the same question is asked by the R2-10 schema budget, and two
    /// spellings of it would eventually disagree about `Hidden`.
    #[must_use]
    pub const fn is_advertised(&self) -> bool {
        matches!(self.exposure, ToolExposure::Advertised)
    }

    /// Whether discovery may surface this tool to the model.
    ///
    /// The consumer is `tool_search` (R2-5c). Deliberately **not** `!is_advertised()`:
    /// [`Hidden`](ToolExposure::Hidden) is neither, and a negation would quietly index it.
    #[must_use]
    pub const fn is_discoverable(&self) -> bool {
        matches!(self.exposure, ToolExposure::Deferred)
    }

    /// Explicit caller allowlist, or `None` for unrestricted.
    #[must_use]
    pub fn allowed_callers(&self) -> Option<&[ToolCaller]> {
        self.allowed_callers.as_deref()
    }

    /// Whether one caller class is admitted.
    #[must_use]
    pub fn allows_caller(&self, caller: ToolCaller) -> bool {
        self.allowed_callers
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&caller))
    }

    /// Per-invocation timeout.
    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Timeout behavior.
    #[must_use]
    pub const fn timeout_behavior(&self) -> ToolTimeoutBehavior {
        self.timeout_behavior
    }

    /// Ordered, deduplicated input guardrails.
    #[must_use]
    pub fn input_guardrails(&self) -> &[ToolGuardrailId] {
        &self.input_guardrails
    }

    /// Ordered, deduplicated output guardrails.
    #[must_use]
    pub fn output_guardrails(&self) -> &[ToolGuardrailId] {
        &self.output_guardrails
    }

    /// Invocation failure behavior.
    #[must_use]
    pub const fn failure_handling(&self) -> ToolFailureHandling {
        self.failure_handling
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

fn push_unique(values: &mut Vec<ToolGuardrailId>, value: ToolGuardrailId) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn deduplicate_guardrails(values: Vec<ToolGuardrailId>) -> Vec<ToolGuardrailId> {
    let mut deduplicated = Vec::with_capacity(values.len());
    for value in values {
        push_unique(&mut deduplicated, value);
    }
    deduplicated
}

// `#[serde(serialize_with)]` fixes this signature to `&Option<T>`.
#[allow(clippy::ref_option)]
fn serialize_optional_duration<S>(
    value: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    value
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .serialize(serializer)
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
}
