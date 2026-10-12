//! The tools a client provides to a thread it starts: Codex's `DynamicToolSpec` and its legacy
//! shape (`protocol/src/dynamic_tools.rs`).
//!
//! A client sends `thread/start.dynamicTools` either in the canonical shape — functions and
//! namespaces of functions, each tagged with its `type` — or in the flat shape older clients and
//! older rollouts use, where each function names its namespace and may carry `exposeToContext`.
//! The flat shape is read into the canonical one, grouping functions by namespace in the order
//! they first appear; a list mixing the two shapes is refused, as Codex refuses it.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;

/// One entry of a thread's dynamic tools: Codex's `DynamicToolSpec`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolSpec {
    /// A function the model calls by its name.
    Function(DynamicToolFunctionSpec),
    /// Functions the model calls within a namespace.
    Namespace(DynamicToolNamespaceSpec),
}

/// A function a client provides: Codex's `DynamicToolFunctionSpec`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolFunctionSpec {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    defer_loading: bool,
}

impl DynamicToolFunctionSpec {
    /// The function `name`, described by `description`, taking input `input_schema` describes.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            defer_loading: false,
        }
    }

    /// Whether the model is shown the function only once it searches for it.
    #[must_use]
    pub const fn with_defer_loading(mut self, defer_loading: bool) -> Self {
        self.defer_loading = defer_loading;
        self
    }

    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it does, for the model.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema of its input, as the client sent it.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Whether the model is shown it only once it searches for it.
    #[must_use]
    pub const fn defer_loading(&self) -> bool {
        self.defer_loading
    }
}

/// Functions a client provides under one namespace: Codex's `DynamicToolNamespaceSpec`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolNamespaceSpec {
    name: String,
    description: String,
    tools: Vec<DynamicToolNamespaceTool>,
}

impl DynamicToolNamespaceSpec {
    /// The namespace `name`, described by `description`, holding `tools`.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        tools: impl IntoIterator<Item = DynamicToolNamespaceTool>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            tools: tools.into_iter().collect(),
        }
    }

    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it holds, for the model; empty for a namespace read from the legacy shape.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Its tools.
    #[must_use]
    pub fn tools(&self) -> &[DynamicToolNamespaceTool] {
        &self.tools
    }
}

/// One tool of a namespace: Codex's `DynamicToolNamespaceTool`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolNamespaceTool {
    /// A function.
    Function(DynamicToolFunctionSpec),
}

impl DynamicToolNamespaceTool {
    /// The function this tool is.
    #[must_use]
    pub const fn function(&self) -> &DynamicToolFunctionSpec {
        match self {
            Self::Function(function) => function,
        }
    }
}

/// The flat shape older clients send and older rollouts keep, including the old
/// `exposeToContext` flag: Codex's `LegacyDynamicToolSpec`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyDynamicToolSpec {
    namespace: Option<String>,
    name: String,
    description: String,
    input_schema: Value,
    defer_loading: Option<bool>,
    expose_to_context: Option<bool>,
}

/// Reads dynamic tools in either shape into the canonical one: Codex's
/// `normalize_dynamic_tool_specs`.
///
/// # Errors
///
/// Returns Codex's error for a list that mixes the two shapes, or the error reading an entry.
pub fn normalize_dynamic_tool_specs(
    values: Vec<Value>,
) -> Result<Vec<DynamicToolSpec>, serde_json::Error> {
    let has_legacy_fields = |value: &Value| {
        value.get("namespace").is_some()
            || value.get("exposeToContext").is_some()
            || value.get("type").is_none()
    };
    let has_legacy_format = values.iter().any(|value| {
        has_legacy_fields(value)
            || value
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| tools.iter().any(has_legacy_fields))
    });
    let has_canonical_format = values.iter().any(|value| value.get("type").is_some());
    if has_legacy_format && has_canonical_format {
        return Err(serde_json::Error::custom(
            "dynamic tools must use either canonical or legacy format consistently",
        ));
    }
    if !has_legacy_format {
        return values.into_iter().map(serde_json::from_value).collect();
    }

    let tools = values
        .into_iter()
        .map(|value| {
            let tool: LegacyDynamicToolSpec = serde_json::from_value(value)?;
            let function = DynamicToolFunctionSpec {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
                defer_loading: tool
                    .defer_loading
                    .unwrap_or_else(|| tool.expose_to_context.is_some_and(|visible| !visible)),
            };
            Ok((tool.namespace, function))
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    Ok(group_dynamic_tools_by_namespace(tools))
}

/// Groups functions by the namespace each names, in the order the namespaces first appear, with
/// an empty description: Codex's `group_dynamic_tools_by_namespace`.
#[must_use]
pub fn group_dynamic_tools_by_namespace(
    tools: Vec<(Option<String>, DynamicToolFunctionSpec)>,
) -> Vec<DynamicToolSpec> {
    let mut grouped_tools = Vec::with_capacity(tools.len());
    let mut namespace_indices = HashMap::<String, usize>::new();
    for (namespace, function) in tools {
        let Some(namespace) = namespace else {
            grouped_tools.push(DynamicToolSpec::Function(function));
            continue;
        };
        let function = DynamicToolNamespaceTool::Function(function);
        if let Some(index) = namespace_indices.get(&namespace).copied() {
            if let Some(DynamicToolSpec::Namespace(namespace)) = grouped_tools.get_mut(index) {
                namespace.tools.push(function);
            }
            continue;
        }
        namespace_indices.insert(namespace.clone(), grouped_tools.len());
        grouped_tools.push(DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: namespace,
            description: String::new(),
            tools: vec![function],
        }));
    }
    grouped_tools
}

/// Reads an optional list of dynamic tools in either shape: Codex's
/// `deserialize_dynamic_tool_specs`.
pub(crate) fn deserialize_dynamic_tool_specs<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<DynamicToolSpec>>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(values) = Option::<Vec<Value>>::deserialize(deserializer)? else {
        return Ok(None);
    };
    normalize_dynamic_tool_specs(values)
        .map(Some)
        .map_err(D::Error::custom)
}
