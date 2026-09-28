//! The tools a sandbox agent works through, bound to one sandbox session.
//!
//! A port of the reference's `sandbox/capabilities/` tool half. Everything here talks to the
//! session only through the protocol in [`ra_core::sandbox`], so it runs unchanged against any
//! backend, and it depends on no backend crate — the backends depend on nothing here either.
//!
//! # Not the coding tools of the same names
//!
//! [`crate::exec_command`] and [`crate::write_stdin`] are the coding product's tools: they run on
//! this host through `ra-exec`, under that product's policies, with arguments of their own
//! (`timeout_ms`, `until`, `control`). The tools here carry the reference's schema and output
//! format and run wherever the session is. Both pairs advertise `exec_command` and `write_stdin`,
//! so an agent is given one pair or the other, never both — a sandbox agent gets these through
//! [`shell::Shell`], and a coding agent keeps its own.
//!
//! The same holds for [`crate::view_image`] and [`view_image`], and for [`crate::apply_patch`] and
//! [`apply_patch_tool`]: the coding versions read and write the host through a confined
//! filesystem, and these go through the session. A sandbox agent gets them through
//! [`filesystem::Filesystem`]; [`filesystem::default_capabilities`] is the reference's default set,
//! which adds [`compaction::Compaction`].
//!
//! The capabilities share names with the coding ones too. [`shell::Shell`],
//! [`filesystem::Filesystem`], [`compaction::Compaction`], [`memory::Memory`] and [`skills::Skills`]
//! claim the families `shell`, `filesystem`, `compaction`, `memory` and `skills`, which
//! [`crate::capability`] and `ra-context` also claim for host-side capabilities of different
//! meaning. Both halves of that are enforced rather than left to the host: an agent cannot
//! advertise one tool name twice, and a run that installs a family for every agent cannot run a
//! sandbox agent that installs the same family itself.

pub mod apply_patch;
pub mod apply_patch_tool;
pub mod compaction;
pub mod filesystem;
pub mod memory;
pub mod shell;
pub mod shell_tool;
pub mod skills;
pub mod view_image;

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    sandbox::SandboxError,
    tool::{
        FuncSchema, ToolApprovalPolicy, ToolArgumentDecodeError, ToolConcurrency, ToolContext,
        ToolInput, ToolOptions,
    },
};

/// Carries a session's failure out of a tool, keeping it as the source.
pub(crate) fn session_failure(tool: &str, error: SandboxError) -> Error {
    Error::tool(ToolErrorKind::ExecutionFailed, tool, error.to_string()).with_source(error)
}

/// Decodes a call's arguments, whether or not the runtime decoded them first.
pub(crate) fn decode<T: ToolInput>(
    context: &mut ToolContext<'_>,
    schema: &FuncSchema,
) -> Result<T> {
    if let Some(input) = context.take_decoded_input::<T>()? {
        return Ok(input);
    }
    serde_json::from_value(context.arguments().clone()).map_err(|error| {
        Error::tool(
            ToolErrorKind::InvalidInput,
            schema.tool_schema().name(),
            ToolArgumentDecodeError::Deserialize {
                input_type: schema.input_type_name(),
                message: error.to_string(),
            }
            .to_string(),
        )
    })
}

/// The options every sandbox function tool declares.
///
/// Parallel, because the reference runs every function call of a turn concurrently; the session is
/// what serializes anything that has to be.
pub(crate) fn sandbox_tool_options(needs_approval: &NeedsApproval) -> ToolOptions {
    ToolOptions::new()
        .with_approval(needs_approval.policy())
        .with_concurrency(ToolConcurrency::Parallel)
}

/// Decides, per call, whether a sandbox tool's call waits for the host's approval.
///
/// The reference's `needs_approval` callable, which receives the run, the call's parameters and
/// its id; here those are all on the [`ToolContext`]. A plain closure over the context is a check,
/// so a predicate on the arguments needs no type of its own.
#[async_trait]
pub trait ApprovalCheck: Send + Sync {
    /// Whether this call needs approval.
    ///
    /// # Errors
    ///
    /// Returns whatever deciding failed with; the call is not run.
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool>;
}

#[async_trait]
impl<F> ApprovalCheck for F
where
    F: Fn(&ToolContext<'_>) -> Result<bool> + Send + Sync,
{
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self(context)
    }
}

/// Whether a sandbox tool's calls wait for the host's approval.
///
/// The reference's `needs_approval: bool | Callable`, defaulting to no approval.
#[non_exhaustive]
#[derive(Clone, Default)]
pub enum NeedsApproval {
    /// Never.
    #[default]
    Never,
    /// Always.
    Always,
    /// Decided per call.
    Check(Arc<dyn ApprovalCheck>),
}

impl NeedsApproval {
    /// Decides per call with `check`.
    #[must_use]
    pub fn check(check: impl ApprovalCheck + 'static) -> Self {
        Self::Check(Arc::new(check))
    }

    /// The static policy the tool declares; a per-call check is declared dynamic.
    #[must_use]
    pub const fn policy(&self) -> ToolApprovalPolicy {
        match self {
            Self::Never => ToolApprovalPolicy::Never,
            Self::Always => ToolApprovalPolicy::Always,
            Self::Check(_) => ToolApprovalPolicy::Dynamic,
        }
    }

    /// Answers for one call.
    ///
    /// # Errors
    ///
    /// Returns the per-call check's failure.
    pub async fn evaluate(&self, context: &ToolContext<'_>) -> Result<bool> {
        match self {
            Self::Never => Ok(false),
            Self::Always => Ok(true),
            Self::Check(check) => check.needs_approval(context).await,
        }
    }
}

impl From<bool> for NeedsApproval {
    fn from(value: bool) -> Self {
        if value { Self::Always } else { Self::Never }
    }
}

impl std::fmt::Debug for NeedsApproval {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Never => formatter.write_str("Never"),
            Self::Always => formatter.write_str("Always"),
            Self::Check(_) => formatter.write_str("Check(..)"),
        }
    }
}
