// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's `skills.list` and `skills.read` contracts, selected explicitly by the host.
//!
//! Executor metadata is supplied by a provider; automatic disk discovery is a separate integration.
//! Host providers contribute file locators to the same bounded run catalog; they add no tools or
//! filesystem grants. The capability renders a bounded catalog at run assembly. Each selected skill root is a grant,
//! keyed by the executor authority id and unrelated to the coding working directory. Per-turn
//! sandbox permission changes are not interpreted by these tools. Cloud schema and routing are
//! retained, without an `OpenAI` account backend. The merged [`crate::skill`] tool remains a separate
//! compatibility extension and cannot share the skills capability family on one agent.
//!
//! A tool pair owns one executor read snapshot. Unlike Codex's thread-owned slot, its lifetime is
//! the pair's lifetime; keys include authority, package, resource binding and filesystem identity.
//! Cache hits deliberately retain old file contents, including after the last page is read.

use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result as CoreResult, ToolErrorKind},
    model::ModelToolDefinition,
    state::RunId,
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolFailureHandling, ToolNamespace, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_exec::fs::{RootedFileSystem, Workspace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use catalog::{SkillAuthority, SkillCatalog, SkillPackageId, SkillReadResult, SkillSourceKind};
use provider::SkillListQuery;

mod aliases;
mod allocation;
pub mod catalog;
mod catalog_prompt;
mod host_aliases;
mod host_selection;
mod host_snapshot;
mod invocation;
mod list;
mod mentions;
mod prompt;
pub mod provider;
mod read;
mod render;
mod required;
mod schema;
mod selection;
mod warnings;

pub use host_selection::collect_host_skill_mentions;
pub use host_snapshot::HostSkillsSnapshot;
pub(crate) use prompt::SkillCatalogPrompt;
pub use provider::{ExecutorSkillProvider, SkillProvider, SkillProviders};
pub use ra_core::session::contextual::is_skill_prompt_fragment;
#[doc(hidden)]
pub use required::set_required_skills;
pub use required::{EnvironmentSkillRequirements, validate_required_skills};
pub use selection::collect_explicit_skill_mentions;

/// Product input used for exact skill selection, without adding Codex UI fields to model items.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SkillInput {
    /// Text that may contain plain or linked dollar mentions.
    Text {
        /// Complete input text.
        text: String,
    },
    /// A structured skill selection; its name blocks plain-name fallback in the same batch.
    Skill {
        /// The selected display name.
        name: String,
        /// Exact native instruction path.
        path: std::path::PathBuf,
    },
    /// A structured resource mention, resolved only when its path identifies a skill.
    Mention {
        /// The mentioned display name.
        name: String,
        /// Exact resource locator.
        path: String,
    },
}

/// The namespace advertised on the Responses wire.
pub const SKILLS_NAMESPACE: &str = "skills";
const MAX_HANDLE_BYTES: usize = 2048;
/// Maximum serialized response bytes for any skill call, including programmatic calls.
pub const MAX_SKILL_RESPONSE_BYTES: usize = 512 * 1024;

/// Builds the independent list/read pair over providers and authority-keyed local skill roots.
/// Executor discovery requires both a provider and at least one granted root. Cloud providers
/// require no executor roots. Returns no tools when neither source is available.
///
/// `response_byte_budget` must be no larger than the host projector's byte ceiling and its token
/// ceiling converted to bytes. No serialization allowance is added: the projector measures the
/// actual returned text. All caller classes use this same budget because all results pass through
/// the host projector. Each root id must match the catalog entry's `authority.id`. Executor paths
/// must use that root's spelling, as returned by `Workspace::root`.
/// Each invocation is parallel-safe. Recoverable errors return the upstream plain text, while
/// provider identity violations abort.
pub fn skill_tools(
    providers: SkillProviders,
    executor_roots: BTreeMap<String, Workspace>,
    response_byte_budget: usize,
) -> CoreResult<Vec<Arc<dyn Tool>>> {
    Ok(skill_tools_with_prompt(providers, executor_roots, response_byte_budget)?.tools)
}

pub(crate) struct SkillToolSet {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) prompt: Option<SkillCatalogPrompt>,
}

pub(crate) fn skill_tools_with_prompt(
    providers: SkillProviders,
    executor_roots: BTreeMap<String, Workspace>,
    response_byte_budget: usize,
) -> CoreResult<SkillToolSet> {
    let executor_available = providers.has_executor_provider() && !executor_roots.is_empty();
    let tools_available = executor_available || providers.has_cloud_provider();
    if !tools_available && !providers.has_host_provider() {
        return Ok(SkillToolSet {
            tools: Vec::new(),
            prompt: None,
        });
    }
    let context = SkillToolContext {
        providers,
        executor_roots: Arc::new(executor_roots),
        response_byte_budget,
        executor_catalog: Arc::new(Mutex::new(None)),
        host_catalog: Arc::new(Mutex::new(None)),
        executor_read_snapshot: Arc::new(Mutex::new(None)),
    };
    let prompt = SkillCatalogPrompt::new(context.clone());
    Ok(SkillToolSet {
        tools: if tools_available {
            vec![
                Arc::new(SkillTool::new::<list::ListArgs>(
                    list::TOOL_NAME,
                    list::DESCRIPTION,
                    context.clone(),
                )?),
                Arc::new(SkillTool::new::<read::ReadArgs>(
                    read::TOOL_NAME,
                    read::DESCRIPTION,
                    context,
                )?),
            ]
        } else {
            Vec::new()
        },
        prompt: Some(prompt),
    })
}

#[derive(Clone)]
struct SkillToolContext {
    providers: SkillProviders,
    executor_roots: Arc<BTreeMap<String, Workspace>>,
    response_byte_budget: usize,
    executor_catalog: Arc<Mutex<Option<ExecutorCatalog>>>,
    host_catalog: Arc<Mutex<Option<ExecutorCatalog>>>,
    executor_read_snapshot: Arc<Mutex<Option<ExecutorReadSnapshot>>>,
}

struct ExecutorCatalog {
    run_id: RunId,
    catalog: Arc<OnceCell<SkillCatalog>>,
}

impl SkillToolContext {
    async fn catalog(
        &self,
        call: &ToolContext<'_>,
        authority: SkillToolAuthoritySelector,
    ) -> SkillCatalog {
        self.catalog_for_run(call.run().run_id(), authority).await
    }

    async fn catalog_for_run(
        &self,
        run_id: &RunId,
        authority: SkillToolAuthoritySelector,
    ) -> SkillCatalog {
        let query = SkillListQuery {
            turn_id: run_id.as_str().to_string(),
            executor_roots: self
                .executor_roots
                .iter()
                .map(|(id, root)| (id.clone(), root.root().to_path_buf()))
                .collect(),
        };
        match authority {
            // There is no account-backed thread snapshot here; optional cloud adapters own their
            // catalog lifecycle. The default cloud slot has no provider and returns an empty catalog.
            SkillToolAuthoritySelector::Cloud => {
                self.providers
                    .list(
                        &SkillSourceKind::Cloud,
                        SkillListQuery {
                            executor_roots: BTreeMap::new(),
                            ..query
                        },
                    )
                    .await
            }
            SkillToolAuthoritySelector::Executor => {
                if self.executor_roots.is_empty() {
                    return SkillCatalog::default();
                }
                // Codex rebuilds this cache per model step. This framework has no step identity,
                // so share concurrent loads within a run and replace the slot when the run changes.
                // Calls already in flight retain their own cell without holding the lock over IO.
                let catalog = {
                    let mut slot = self
                        .executor_catalog
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if slot.as_ref().is_some_and(|cached| cached.run_id != *run_id) {
                        *slot = None;
                    }
                    let cached = slot.get_or_insert_with(|| ExecutorCatalog {
                        run_id: run_id.clone(),
                        catalog: Arc::new(OnceCell::new()),
                    });
                    Arc::clone(&cached.catalog)
                };
                catalog
                    .get_or_init(|| self.providers.list(&SkillSourceKind::Executor, query))
                    .await
                    .clone()
            }
        }
    }

    async fn host_catalog_for_run(&self, run_id: &RunId) -> SkillCatalog {
        let cell = {
            let mut slot = self
                .host_catalog
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.as_ref().is_some_and(|cached| cached.run_id != *run_id) {
                *slot = None;
            }
            Arc::clone(
                &slot
                    .get_or_insert_with(|| ExecutorCatalog {
                        run_id: run_id.clone(),
                        catalog: Arc::new(OnceCell::new()),
                    })
                    .catalog,
            )
        };
        cell.get_or_init(|| {
            self.providers.list(
                &SkillSourceKind::Host,
                SkillListQuery {
                    turn_id: run_id.as_str().to_string(),
                    executor_roots: BTreeMap::new(),
                },
            )
        })
        .await
        .clone()
    }

    fn response_byte_budget(&self) -> usize {
        self.response_byte_budget.min(MAX_SKILL_RESPONSE_BYTES)
    }
}

struct ExecutorReadSnapshot {
    authority: SkillAuthority,
    package: SkillPackageId,
    access: Arc<RootedFileSystem>,
    result: Arc<SkillReadResult>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SkillToolAuthoritySelector {
    Cloud,
    Executor,
}

impl SkillToolAuthoritySelector {
    fn matches(self, authority: &SkillAuthority) -> bool {
        match self {
            Self::Cloud => authority.kind == SkillSourceKind::Cloud,
            Self::Executor => authority.kind == SkillSourceKind::Executor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SkillToolAuthority {
    Cloud,
    Executor { id: String },
}

impl SkillToolAuthority {
    const fn selector(&self) -> SkillToolAuthoritySelector {
        match self {
            Self::Cloud => SkillToolAuthoritySelector::Cloud,
            Self::Executor { .. } => SkillToolAuthoritySelector::Executor,
        }
    }
    fn from_authority(authority: &SkillAuthority) -> Option<Self> {
        match &authority.kind {
            SkillSourceKind::Cloud if authority.id == provider::CLOUD_AUTHORITY_ID => {
                Some(Self::Cloud)
            }
            SkillSourceKind::Executor => Some(Self::Executor {
                id: authority.id.clone(),
            }),
            _ => None,
        }
    }
}

struct SkillTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    context: SkillToolContext,
}

impl SkillTool {
    fn new<I: JsonSchema>(
        name: &str,
        description: &str,
        context: SkillToolContext,
    ) -> CoreResult<Self> {
        Ok(Self {
            origin: ToolOrigin::namespaced(ToolNamespace::new(SKILLS_NAMESPACE)?, name)?,
            schema: ToolSchema::loose(name, schema::input_schema_for::<I>())?
                .with_description(description)
                .with_namespace_description("Tools in the skills namespace."),
            context,
        })
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_failure_handling(ToolFailureHandling::Custom)
            .with_action_classes([crate::action_classes::READ])
            .with_concurrency(ToolConcurrency::Parallel)
    }
    fn model_definition(&self) -> ModelToolDefinition {
        self.schema.to_model_definition().with_namespace(
            SKILLS_NAMESPACE,
            Some("Tools in the skills namespace.".to_string()),
        )
    }
    async fn call(&self, context: ToolContext<'_>) -> CoreResult<ToolOutput> {
        let result = match self.origin.name() {
            "list" => list::handle(&self.context, &context).await,
            _ => read::handle(&self.context, &context).await,
        };
        result.map_err(|error| {
            Error::tool(
                ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                error.to_string(),
            )
            .with_source(error)
        })
    }
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> CoreResult<Option<ToolOutput>> {
        let failure = std::error::Error::source(error)
            .and_then(|source| source.downcast_ref::<FunctionCallError>());
        Ok(match failure {
            Some(FunctionCallError::RespondToModel(message)) => Some(ToolOutput::text(message)),
            _ => None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
enum FunctionCallError {
    #[error("{0}")]
    RespondToModel(String),
    #[error("{0}")]
    Fatal(String),
}

fn parse_args<T: for<'de> Deserialize<'de>>(
    call: &ToolContext<'_>,
) -> Result<T, FunctionCallError> {
    // Function arguments arrive as parsed JSON in the framework. Its model codecs already turn
    // empty function-argument strings into an empty object, just as upstream parse_args does.
    serde_json::from_value(call.arguments().clone())
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

fn validate_handle(name: &str, value: &str, max_bytes: usize) -> Result<(), FunctionCallError> {
    if is_bounded_handle(value, max_bytes) {
        return Ok(());
    }
    Err(FunctionCallError::RespondToModel(format!(
        "{name} must be non-empty, contain no control characters, and be at most {max_bytes} bytes"
    )))
}
fn is_bounded_handle(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}
fn value_fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
fn pagination_cursor(value: &(impl Hash + ?Sized), offset: usize) -> String {
    format!("{:016x}:{offset}", value_fingerprint(value))
}
fn parse_pagination_cursor(
    cursor: Option<&str>,
    value: &(impl Hash + ?Sized),
    tool: &str,
) -> Result<usize, FunctionCallError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let invalid = || FunctionCallError::RespondToModel(format!("{tool} cursor is invalid"));
    let (fingerprint, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    if u64::from_str_radix(fingerprint, 16).ok() != Some(value_fingerprint(value)) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool} cursor is stale; restart from the first page"
        )));
    }
    offset.parse::<usize>().map_err(|_| invalid())
}
fn serialized_len(value: &impl Serialize) -> Result<usize, FunctionCallError> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))
}
fn skill_json_output<T: Serialize>(
    value: &T,
    _authority: SkillToolAuthoritySelector,
) -> Result<ToolOutput, FunctionCallError> {
    serde_json::to_string(value)
        .map(ToolOutput::text)
        .map_err(|err| FunctionCallError::Fatal(format!("failed to serialize tool output: {err}")))
}
fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}
fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}
