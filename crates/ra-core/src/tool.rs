//! Provider-neutral tool contracts and stable runtime identity.
//!
//! A tool has two deliberately separate names:
//!
//! - [`ToolOrigin::qualified_name`] is for diagnostics and traces;
//! - [`ToolOrigin::lookup_key`] is the collision-free, serializable routing identity.
//!
//! Dispatch and persistence must use the lookup key. Reconstructing identity by splitting a
//! dotted display name is ambiguous as soon as namespaces or tool names themselves contain dots.

use async_trait::async_trait;

use crate::{context::RunContext, error::Result, model::ModelToolDefinition};
use serde_json::Value;

pub mod context;
pub mod namespace;
pub mod options;
pub mod origin;
pub mod output;
pub mod resource;
pub mod schema;
pub mod services;
mod strict;

pub use context::{ToolCaller, ToolContext};
pub use namespace::ToolNamespace;
pub use options::{
    DEFAULT_MAX_NO_PROGRESS_STREAK, DEFAULT_MAX_REPEAT_STREAK, TOOL_OPTIONS_SCHEMA_VERSION,
    ToolApprovalPolicy, ToolAvailability, ToolConcurrency, ToolExposure, ToolFailureHandling,
    ToolGuardrailId, ToolOptions, ToolTimeoutBehavior,
};
pub use origin::{
    TOOL_LOOKUP_KEY_SCHEMA_VERSION, TOOL_ORIGIN_SCHEMA_VERSION, ToolLookupKey, ToolLookupKind,
    ToolOrigin,
};
pub use output::{
    OBSERVATION_METADATA_SCHEMA_VERSION, ObservationMetadata, TOOL_OUTPUT_SCHEMA_VERSION,
    TRUNCATION_SCHEMA_VERSION, ToolOutput, ToolOutputBlock, Truncation, TruncationStage,
};
pub use resource::{ResourceAccess, ResourceClaim, ResourceId, ResourceKind};
pub use schema::{
    ArgumentShapeViolation, DecodedToolInput, FUNC_SCHEMA_VERSION, FuncSchema, TOOL_SCHEMA_VERSION,
    ToolArgumentDecodeError, ToolInput, ToolSchema,
};
pub use services::ToolServices;

/// An executable tool exposed by an application or integration.
///
/// Only identity, schema, and invocation are required. Runtime policies have safe defaults so a
/// future framework release can add another optional hook without breaking third-party tools.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Stable source and routing identity.
    fn origin(&self) -> &ToolOrigin;

    /// Provider-neutral model-facing schema.
    fn schema(&self) -> &ToolSchema;

    /// Optional typed function schema from which the default [`Tool::decode_input`] is derived.
    ///
    /// A tool that returns one has its parsed model arguments schema-validated and decoded before
    /// [`Tool::call`] runs. The resulting value is available through
    /// [`ToolContext::decoded_input`]. Implementations that override [`Tool::decode_input`] must
    /// decode against this same schema; the runtime calls `decode_input`, not this method,
    /// directly. Tools without a Rust input type, such as a remote MCP proxy, keep receiving the
    /// parsed JSON value unchanged.
    fn func_schema(&self) -> Option<&FuncSchema> {
        None
    }

    /// Decodes one normalized argument object at the common invocation boundary.
    ///
    /// Typed tools may override this to preserve a domain-specific error source for their custom
    /// failure formatter. Such implementations must not block: decoding runs synchronously before
    /// the cancellation scope can poll or interrupt it. Returning `None` declares that the tool
    /// accepts parsed JSON directly.
    fn decode_input(&self, arguments: &Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema()
            .map(|schema| schema.decode_value(arguments.clone()))
            .transpose()
    }

    /// Executes one already-resolved call.
    ///
    /// The context carries the call and the run it belongs to; see [`ToolContext`] for why those
    /// are one value rather than an invocation beside a separately threaded host object.
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput>;

    /// Declarative execution and exposure policies.
    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    /// Evaluates dynamic availability. Static policies are handled by the default implementation.
    ///
    /// Asked before the turn advertises anything, so there is no call yet — only the run.
    async fn is_enabled(&self, _context: &RunContext) -> Result<bool> {
        match self.options().availability() {
            ToolAvailability::Enabled => Ok(true),
            ToolAvailability::Disabled => Ok(false),
            ToolAvailability::Dynamic => Err(crate::error::Error::caller(format!(
                "tool `{}` declares dynamic availability but does not implement Tool::is_enabled",
                self.origin().qualified_name()
            ))),
        }
    }

    /// Evaluates whether this call requires host approval.
    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        match self.options().approval() {
            ToolApprovalPolicy::Never => Ok(false),
            ToolApprovalPolicy::Always => Ok(true),
            ToolApprovalPolicy::Dynamic => Err(crate::error::Error::caller(format!(
                "tool `{}` declares dynamic approval but does not implement Tool::needs_approval",
                self.origin().qualified_name()
            ))),
        }
    }

    /// Optionally turns a call failure into a model-visible result.
    ///
    /// The common executor calls this only when [`ToolFailureHandling::Custom`] is selected.
    /// Returning `None` means the failure remains unhandled and must be propagated.
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        _error: &crate::error::Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(None)
    }

    /// Evaluates resource claims for this invocation.
    ///
    /// Defaults to the static claims declared in [`ToolOptions::resource_claims`].
    async fn resource_claims(&self, _context: &ToolContext<'_>) -> Result<Vec<ResourceClaim>> {
        Ok(self.options().resource_claims().to_vec())
    }

    /// Checks invariants shared by registries and runners.
    fn validate(&self) -> Result<()> {
        self.origin().validate()?;
        self.schema().validate()?;
        self.options().validate()?;
        if self.origin().name() != self.schema().name() {
            return Err(crate::error::Error::caller(format!(
                "tool origin name `{}` does not match schema name `{}`",
                self.origin().name(),
                self.schema().name()
            )));
        }
        Ok(())
    }

    /// Produces the model-boundary projection without leaking runtime policies.
    fn model_definition(&self) -> ModelToolDefinition {
        self.schema().to_model_definition()
    }
}
