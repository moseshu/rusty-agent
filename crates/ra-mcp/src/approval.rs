//! Server-owned approval settings following Python's local MCP approval contract.

use futures::future::BoxFuture;
use ra_core::{
    context::{RunAgent, RunContext},
    error::{Error, Result},
    tool::ToolApprovalPolicy,
};
use rmcp::model::Tool;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// An approval predicate receiving the current run, the discovering public agent, and the
/// original MCP declaration. Arguments and model-visible renamed identities are not supplied.
pub type LocalMcpApprovalCallable = Arc<
    dyn for<'a> Fn(&'a RunContext, &'a RunAgent, Tool) -> BoxFuture<'a, Result<bool>> + Send + Sync,
>;

/// Python's normalized `require_approval`: a server default, a tool-name map, or a callable.
/// Missing names in a map never require approval. The default is [`Self::Never`].
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum RequireApprovalSetting {
    /// Calls do not need approval unless the host's permission policy asks for it.
    #[default]
    Never,
    /// Every call requires approval.
    Always,
    /// Original MCP tool names mapped to whether approval is required.
    ToolMapping(BTreeMap<String, bool>),
    /// Evaluated at invocation, after any existing approval decision has been checked.
    Dynamic(LocalMcpApprovalCallable),
}

impl std::fmt::Debug for RequireApprovalSetting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Never => formatter.write_str("Never"),
            Self::Always => formatter.write_str("Always"),
            Self::ToolMapping(mapping) => {
                formatter.debug_tuple("ToolMapping").field(mapping).finish()
            }
            Self::Dynamic(_) => formatter.write_str("Dynamic(<callback>)"),
        }
    }
}

impl From<bool> for RequireApprovalSetting {
    fn from(required: bool) -> Self {
        if required { Self::Always } else { Self::Never }
    }
}

impl RequireApprovalSetting {
    /// Wraps an asynchronous predicate whose future may borrow the run and agent.
    #[must_use]
    pub fn dynamic<F>(policy: F) -> Self
    where
        F: for<'a> Fn(&'a RunContext, &'a RunAgent, Tool) -> BoxFuture<'a, Result<bool>>
            + Send
            + Sync
            + 'static,
    {
        Self::Dynamic(Arc::new(policy))
    }

    /// Wraps a synchronous fallible predicate.
    #[must_use]
    pub fn dynamic_sync<F>(policy: F) -> Self
    where
        F: Fn(&RunContext, &RunAgent, Tool) -> Result<bool> + Send + Sync + 'static,
    {
        Self::dynamic(move |run, agent, tool| {
            Box::pin(std::future::ready(policy(run, agent, tool)))
        })
    }

    pub(crate) fn for_tool(&self, name: &str, has_agent: bool) -> ToolApprovalPolicy {
        let required = match self {
            Self::Never => false,
            Self::ToolMapping(mapping) => mapping.get(name).copied().unwrap_or(false),
            Self::Dynamic(_) if has_agent => return ToolApprovalPolicy::Dynamic,
            // Python's legacy conversion without an agent must not bypass a callable policy.
            Self::Always | Self::Dynamic(_) => true,
        };
        if required {
            ToolApprovalPolicy::Always
        } else {
            ToolApprovalPolicy::Never
        }
    }
}

impl TryFrom<Value> for RequireApprovalSetting {
    type Error = Error;

    /// Normalizes Python's null, bool, "always"/"never", tool-name mappings and
    /// `{"always":{"tool_names":[...]},"never":{"tool_names":[...]}}` forms.
    /// Conflicting lists and malformed settings are configuration errors.
    fn try_from(value: Value) -> Result<Self> {
        match value {
            Value::Null => Ok(Self::Never),
            Value::Object(object) => {
                let is_tool_list = ["always", "never"].iter().any(|key| {
                    object
                        .get(*key)
                        .and_then(Value::as_object)
                        .is_some_and(|entry| entry.contains_key("tool_names"))
                });
                let mut mapping = BTreeMap::new();
                if is_tool_list {
                    if object.keys().any(|key| key != "always" && key != "never") {
                        return Err(Error::config(
                            "Invalid require_approval tool list policy: expected only 'always' and 'never'",
                        ));
                    }
                    for (policy, required) in [("always", true), ("never", false)] {
                        let Some(entry) = object.get(policy) else {
                            continue;
                        };
                        let entry = entry.as_object().ok_or_else(|| Error::config(format!(
                            "Invalid require_approval.{policy}: expected an object with tool_names"
                        )))?;
                        let Some(names) = entry.get("tool_names") else {
                            continue;
                        };
                        let names = names.as_array().ok_or_else(|| Error::config(format!(
                            "Invalid require_approval.{policy}.tool_names: expected a list of strings"
                        )))?;
                        for (index, name) in names.iter().enumerate() {
                            let name = name.as_str().ok_or_else(|| Error::config(format!(
                                "Invalid require_approval tool name at require_approval.{policy}.tool_names[{index}]: expected a string"
                            )))?;
                            if mapping
                                .insert(name.to_owned(), required)
                                .is_some_and(|old| old != required)
                            {
                                return Err(Error::config(format!(
                                    "Invalid require_approval tool list policy: tool name {name:?} appears in both always and never"
                                )));
                            }
                        }
                    }
                } else {
                    for (name, value) in object {
                        let required = policy_bool(&value, &format!("require_approval[{name:?}]"))?;
                        mapping.insert(name, required);
                    }
                }
                Ok(Self::ToolMapping(mapping))
            }
            value => policy_bool(&value, "require_approval").map(Self::from),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RequireApprovalSetting {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Self::try_from(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

fn policy_bool(value: &Value, location: &str) -> Result<bool> {
    match value {
        Value::Bool(required) => Ok(*required),
        Value::String(policy) if policy == "always" => Ok(true),
        Value::String(policy) if policy == "never" => Ok(false),
        _ => Err(Error::config(format!(
            "Invalid require_approval value at {location}: expected 'always' or 'never'"
        ))),
    }
}
