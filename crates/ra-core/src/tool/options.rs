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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOptions {
    schema_version: SchemaVersion,
    #[serde(default)]
    availability: ToolAvailability,
    #[serde(default)]
    approval: ToolApprovalPolicy,
    #[serde(default)]
    defer_loading: bool,
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

impl Default for ToolOptions {
    fn default() -> Self {
        Self {
            schema_version: TOOL_OPTIONS_SCHEMA_VERSION,
            availability: ToolAvailability::Enabled,
            approval: ToolApprovalPolicy::Never,
            defer_loading: false,
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

    /// Hides the schema from the default advertise set while keeping the tool registered.
    #[must_use]
    pub const fn with_defer_loading(mut self, defer_loading: bool) -> Self {
        self.defer_loading = defer_loading;
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

    /// Whether this tool is discoverable only on demand.
    #[must_use]
    pub const fn defer_loading(&self) -> bool {
        self.defer_loading
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
