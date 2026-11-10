//! Tool calls, outputs, and approval control items.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CallId;
use crate::{
    compat::{SchemaVersion, Unknown},
    model::ModelToolKind,
    tool::{ToolLookupKey, ToolOrigin},
};

/// Current tool-item schema version.
pub const TOOL_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Whether a call and its output belong to a function, a custom tool, or client discovery.
///
/// A custom call carries one raw string where a function call carries a JSON object; its arguments
/// are that string as a JSON string. Only a provider that speaks the custom form natively — `OpenAI`
/// Responses, where the two travel as different item types — needs to tell them apart when it
/// replays history, and it needs it on the output as much as on the call: a continuation that sends
/// only the new output has nothing else to go on.
///
/// Omitted from the record for a function call, which is every record written before custom tools
/// existed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    /// A call to a function tool.
    #[default]
    Function,
    /// A call to a custom tool.
    Custom,
    /// A client-executed tool discovery call and its definition output.
    ToolSearch,
}

impl ToolCallKind {
    /// Whether this is a function call.
    #[must_use]
    #[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` passes a reference
    pub const fn is_function(&self) -> bool {
        matches!(self, Self::Function)
    }
}

impl From<&ModelToolKind> for ToolCallKind {
    fn from(kind: &ModelToolKind) -> Self {
        if kind.is_tool_search() {
            Self::ToolSearch
        } else if kind.is_custom() {
            Self::Custom
        } else {
            Self::Function
        }
    }
}

/// A provider-neutral tool call.
///
/// A call of a tool advertised in a namespace names the namespace too, as the reference's
/// `ResponseFunctionToolCall.namespace` does: the name alone is ambiguous once two namespaces
/// offer the same one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    schema_version: SchemaVersion,
    call_id: CallId,
    name: String,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(default, skip_serializing_if = "ToolCallKind::is_function")]
    kind: ToolCallKind,
    // The provider's argument text when it is not JSON; `arguments` is then `null`. Kept apart so
    // a build that does not know this field reads the call as one with `null` arguments rather
    // than taking the text for a valid string argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unparsed_arguments: Option<Box<str>>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolCall {
    /// Creates a function call whose arguments the provider gave as text that is not JSON.
    ///
    /// Its [`arguments`](Self::arguments) are `null`; the text is
    /// [`unparsed_arguments`](Self::unparsed_arguments). The call is reported to the model as a
    /// failure when it runs, as the reference reports arguments it cannot parse, rather than
    /// failing the response that carried it.
    #[must_use]
    pub fn unparsed(call_id: CallId, name: impl Into<String>, text: impl Into<String>) -> Self {
        let mut call = Self::new(call_id, name, Value::Null);
        call.unparsed_arguments = Some(text.into().into_boxed_str());
        call
    }

    /// The provider's argument text, when it was not JSON.
    #[must_use]
    pub fn unparsed_arguments(&self) -> Option<&str> {
        self.unparsed_arguments.as_deref()
    }

    /// Creates a call to a custom tool, whose arguments are its raw input as a JSON string.
    #[must_use]
    pub fn custom(call_id: CallId, name: impl Into<String>, input: impl Into<String>) -> Self {
        Self::new(call_id, name, Value::String(input.into())).with_kind(ToolCallKind::Custom)
    }

    /// Creates a tool call.
    #[must_use]
    pub fn new(call_id: CallId, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            name: name.into(),
            arguments,
            namespace: None,
            kind: ToolCallKind::Function,
            unparsed_arguments: None,
            unknown: Unknown::new(),
        }
    }

    /// Records whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn with_kind(mut self, kind: ToolCallKind) -> Self {
        self.kind = kind;
        self
    }

    /// Records the namespace the called tool was advertised in.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn kind(&self) -> ToolCallKind {
        self.kind
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Call ID.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Tool name, within its namespace when it has one.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The namespace the called tool was advertised in, if any.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Parsed argument JSON; `null` for a call whose arguments were not JSON (see
    /// [`unparsed_arguments`](Self::unparsed_arguments)).
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A provider-neutral tool-call output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    schema_version: SchemaVersion,
    call_id: CallId,
    output: Value,
    #[serde(default)]
    is_error: bool,
    #[serde(default, skip_serializing_if = "ToolCallKind::is_function")]
    kind: ToolCallKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom_data: Option<serde_json::Map<String, Value>>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolCallOutput {
    /// Creates a successful output.
    #[must_use]
    pub fn new(call_id: CallId, output: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            output,
            is_error: false,
            kind: ToolCallKind::Function,
            custom_data: None,
            unknown: Unknown::new(),
        }
    }

    /// Records whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn with_kind(mut self, kind: ToolCallKind) -> Self {
        self.kind = kind;
        self
    }

    /// Whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn kind(&self) -> ToolCallKind {
        self.kind
    }

    /// Marks this as a tool failure observation that can be replayed to the model, rather than a
    /// framework error.
    #[must_use]
    pub const fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Replaces the model-facing output while preserving this record's identity and metadata.
    ///
    /// Context projections use this on a cloned model-input view. The authoritative session item
    /// remains unchanged, while the `call_id`, error flag, and unknown fields still travel with
    /// the result sent to the provider.
    #[must_use]
    pub fn with_output(mut self, output: Value) -> Self {
        self.output = output;
        self
    }

    /// Attaches SDK-only JSON data; empty mappings are treated as absent.
    #[must_use]
    pub fn with_custom_data(mut self, data: serde_json::Map<String, Value>) -> Self {
        self.custom_data = (!data.is_empty()).then_some(data);
        self
    }

    /// SDK-only data retained in authoritative output records.
    #[must_use]
    pub const fn custom_data(&self) -> Option<&serde_json::Map<String, Value>> {
        self.custom_data.as_ref()
    }

    /// Creates the model-facing view without SDK-only data.
    #[must_use]
    pub fn without_custom_data(mut self) -> Self {
        self.custom_data = None;
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// ID that pairs this output with its call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Output JSON. A future execution layer will add a typed multimodal `ToolOutput`.
    #[must_use]
    pub const fn output(&self) -> &Value {
        &self.output
    }

    /// Whether this is a tool failure observation.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.is_error
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Where an approved call resumes: before it started, or inside the invocation that asked.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResumePoint {
    /// The approval was asked before the tool was invoked. The approved call is dispatched again
    /// through every stage, as any approved call is.
    BeforeCall,
    /// The tool was invoked and asked from inside the invocation. The approved call continues that
    /// invocation: it was admitted, checked and announced when it started, so it is not again.
    DuringCall,
}

/// What a tool asked to keep with its approval, handed back to it when the approved call resumes.
///
/// The state is the tool's own and opaque to the runtime, which stores it with the approval and
/// returns it through [`ToolContext::resume_state`](crate::tool::ToolContext::resume_state). It
/// is how a tool keeps what it decided, or how far it got, across an approval that may be answered
/// in another process: an interrupted run cannot hold a tool's stack frame while it waits.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResume {
    point: ToolResumePoint,
    state: Value,
}

impl ToolResume {
    /// State a tool keeps with an approval asked before it is invoked.
    #[must_use]
    pub const fn before_call(state: Value) -> Self {
        Self {
            point: ToolResumePoint::BeforeCall,
            state,
        }
    }

    /// State a tool keeps with an approval it asks from inside its invocation.
    #[must_use]
    pub const fn during_call(state: Value) -> Self {
        Self {
            point: ToolResumePoint::DuringCall,
            state,
        }
    }

    /// Where the approved call resumes.
    #[must_use]
    pub const fn point(&self) -> ToolResumePoint {
        self.point
    }

    /// The tool's state.
    #[must_use]
    pub const fn state(&self) -> &Value {
        &self.state
    }
}

/// A tool call awaiting host approval.
///
/// This is a control-plane record. It may be stored in a session or carried by an interruption,
/// but it must never be sent to the model.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApproval {
    schema_version: SchemaVersion,
    call_id: CallId,
    tool_name: String,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    // Boxed because only this one variant of `RunItemKind` carries it. Inline, an approval-only
    // routing key widens every message and reasoning record the session stores by the same amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lookup_key: Option<Box<ToolLookupKey>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipient_binding: Option<Box<Value>>,
    #[serde(default, skip_serializing_if = "ToolCallKind::is_function")]
    kind: ToolCallKind,
    // Boxed for the reason `lookup_key` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<Box<ToolResume>>,
    // The call's argument text when it was not JSON, as on `ToolCall`; `arguments` is then `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unparsed_arguments: Option<Box<str>>,
    // The routing key of an approval whose call's arguments were not JSON, held here instead of in
    // `lookup_key`. A build that does not know unparsed arguments refuses to approve or resume an
    // approval without `lookup_key`, so it stops rather than running the call with `null`
    // arguments; it keeps this field as unknown and writes it back unchanged. At most one of the
    // two is set, chosen by whether `unparsed_arguments` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unparsed_call_lookup_key: Option<Box<ToolLookupKey>>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl std::fmt::Debug for ToolApproval {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolApproval")
            .field("schema_version", &self.schema_version)
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .field("arguments", &self.arguments)
            .field("namespace", &self.namespace)
            .field("lookup_key", &self.lookup_key())
            .field("has_recipient_binding", &self.recipient_binding.is_some())
            .field("kind", &self.kind)
            .field("resume", &self.resume)
            .field("unparsed_arguments", &self.unparsed_arguments)
            .field(
                "routing_key_withheld_from_older_builds",
                &self.unparsed_call_lookup_key.is_some(),
            )
            .field("unknown", &self.unknown)
            .finish()
    }
}

impl ToolApproval {
    /// Creates an approval item.
    #[must_use]
    pub fn new(call_id: CallId, tool_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            tool_name: tool_name.into(),
            arguments,
            namespace: None,
            lookup_key: None,
            recipient_binding: None,
            kind: ToolCallKind::Function,
            resume: None,
            unparsed_arguments: None,
            unparsed_call_lookup_key: None,
            unknown: Unknown::new(),
        }
    }

    /// Records that the call asked about had argument text that was not JSON, and sets its
    /// [`arguments`](Self::arguments) to `null`, so the approved call resumes as that call.
    #[must_use]
    pub fn with_unparsed_arguments(mut self, text: impl Into<String>) -> Self {
        self.arguments = Value::Null;
        self.unparsed_arguments = Some(text.into().into_boxed_str());
        if let Some(key) = self.lookup_key.take() {
            self.unparsed_call_lookup_key = Some(key);
        }
        self
    }

    /// The asked-about call's argument text, when it was not JSON.
    #[must_use]
    pub fn unparsed_arguments(&self) -> Option<&str> {
        self.unparsed_arguments.as_deref()
    }

    /// Keeps the tool's state with this approval, to hand back when the approved call resumes.
    #[must_use]
    pub fn with_resume(mut self, resume: ToolResume) -> Self {
        self.resume = Some(Box::new(resume));
        self
    }

    /// The tool's state kept with this approval, if it asked to keep any.
    #[must_use]
    pub fn resume(&self) -> Option<&ToolResume> {
        self.resume.as_deref()
    }

    /// Records whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn with_kind(mut self, kind: ToolCallKind) -> Self {
        self.kind = kind;
        self
    }

    /// Whether this belongs to a function tool or a custom one.
    #[must_use]
    pub const fn kind(&self) -> ToolCallKind {
        self.kind
    }

    /// Sets the tool namespace.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Records the exact executable identity selected for this approval.
    ///
    /// The model-facing name is not a routing key: two namespaces may advertise the same name,
    /// and a resumed run must not select whichever implementation happens to be present later.
    ///
    /// Only the lookup key is stored. The qualified name is a pure function of it — see
    /// [`ToolOrigin::from_lookup_key`], which derives it, and [`ToolOrigin::validate`], which
    /// enforces the derivation — so a second copy on this record could only ever be redundant or
    /// wrong, and checking one against the other proves nothing a reader would not already know.
    #[must_use]
    pub fn with_tool_origin(mut self, origin: &ToolOrigin) -> Self {
        let key = Some(Box::new(origin.lookup_key().clone()));
        if self.unparsed_arguments.is_some() {
            self.unparsed_call_lookup_key = key;
        } else {
            self.lookup_key = key;
        }
        self
    }
    /// Retains the selected adapter recipient independently of its allocated model name.
    #[must_use]
    pub fn with_recipient_binding(mut self, binding: Option<Value>) -> Self {
        self.recipient_binding = binding.map(Box::new);
        self
    }
    /// Opaque recipient identity for validating a resumed call against rediscovered tools.
    #[must_use]
    pub fn recipient_binding(&self) -> Option<&Value> {
        self.recipient_binding.as_deref()
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Call ID.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Tool name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Call arguments.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// Tool namespace.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Exact routing key needed to rebuild the pending action after a restart.
    #[must_use]
    pub fn lookup_key(&self) -> Option<&ToolLookupKey> {
        self.lookup_key
            .as_deref()
            .or(self.unparsed_call_lookup_key.as_deref())
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
