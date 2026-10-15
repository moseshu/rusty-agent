//! Unified entry point for the provider-neutral agent framework.
//!
//! **Boundary**: an additive facade over the kernel, provider adapters, macros, and sessions.
//! The kernel never depends on this crate. Provider credentials and configuration remain explicit;
//! there is no global client or account machinery. Lower-level crates remain independently usable.
//!
//! **Stability**: `Evolving`. Re-exported contracts retain their owning crate's stability grade.
//!
//! Configure a [`ProviderRegistry`] once, put it in an `Arc`, and pass it to [`AgentRunner::new`].
//! Then call `runner.run(agent, "hello").await`. Use [`AgentRunner::request`] when attaching
//! session, context, or configuration. [`Runner`] still accepts explicit [`RunRequest`] values.
//!
//! The kernel's contract modules are re-exported under their `ra-core` names (for example
//! [`session`], [`finish`], [`hook`], [`trace`], and [`sandbox`]), so advanced contracts do not
//! need a second, version-matched `ra-core` dependency. Two are left out: the `Internal` `step`
//! intermediates, and `ra-core`'s `compat` module, whose name belongs to the `compat` provider
//! module here. From the other crates, [`runner`] and [`provider`] are re-exported; their
//! remaining modules are reached through `ra-runtime`, `ra-model`, and `ra-session`.

// Macro expansions need a stable path to the owning contracts even when only this facade is a
// direct dependency, including when the facade is renamed in a downstream Cargo manifest.
#[doc(hidden)]
pub use ra_core as core;

// Every public kernel contract module except two. `step` holds `Internal` settlement
// intermediates that downstream code must not name. `compat` (schema versions and unknown-field
// retention) would collide with the `compat` provider module; depend on `ra-core` to name it.
pub use ra_core::{
    agent, budget, cancel, capability, config, context, error, event, filter, finish, guardrail,
    hook, item, lifecycle, memory, model, output, permission, prompt, sandbox, session, skill,
    state, tool, trace, usage, user_input, web,
};
pub use ra_core::{
    agent::{
        AgentId, AgentInstructions, AgentSpec, AgentSpecBuilder, HandoffSpec, ToolUseBehavior,
    },
    cancel::{CancelReason, CancelScope},
    context::RunContext,
    error::{Error, Result},
    finish::FinishReason,
    item::{Message, ModelInputItem, RunItem, RunItemKind},
    lifecycle::{LifecycleHook, LifecycleScope},
    model::{ApiProtocol, Model, ModelProvider, ModelResolver, ModelSettings, ProviderKey},
    output::{OutputSchema, OutputValue},
    prompt::{PromptSource, ResolvedPrompt},
    session::{Session, SessionId, SessionInputCallback, SessionSettings},
    state::{RunId, RunState},
    tool::{Tool, ToolContext, ToolInput, ToolOutput},
    usage::Usage,
};
pub use ra_macros::{ToolInput, tool};
pub use ra_model::provider::{self, ProviderRegistration, ProviderRegistry};
pub use ra_runtime::{
    agent::{
        AgentBinding, AgentRegistry,
        tool::{AgentAsTool, AgentTool, AgentToolBuilder},
    },
    runner,
    runner::{
        AgentRunner, RunConfig, RunInput, RunOutcome, RunRequest, RunResult, RunStream,
        RunStreamEvent, Runner,
    },
};
pub use ra_session::InMemorySession;
#[cfg(feature = "sqlite")]
pub use ra_session::SqliteSession;

#[cfg(feature = "anthropic")]
pub use ra_model::anthropic;
#[cfg(feature = "compat")]
pub use ra_model::compat;
#[cfg(feature = "openai")]
pub use ra_model::openai;

/// Common declarations, execution methods, tool macros, and conversation sessions.
pub mod prelude {
    pub use crate::{
        AgentAsTool, AgentBinding, AgentId, AgentInstructions, AgentRegistry, AgentRunner,
        AgentSpec, ApiProtocol, CancelReason, CancelScope, Error, FinishReason, HandoffSpec,
        InMemorySession, LifecycleHook, LifecycleScope, Message, ModelInputItem, ModelSettings,
        OutputSchema, OutputValue, PromptSource, ProviderKey, ProviderRegistration,
        ProviderRegistry, ResolvedPrompt, Result, RunConfig, RunContext, RunId, RunInput, RunItem,
        RunItemKind, RunOutcome, RunRequest, RunResult, RunState, RunStream, RunStreamEvent,
        Runner, Session, SessionId, SessionInputCallback, SessionSettings, Tool, ToolContext,
        ToolInput, ToolOutput, ToolUseBehavior, Usage, tool,
    };
}
