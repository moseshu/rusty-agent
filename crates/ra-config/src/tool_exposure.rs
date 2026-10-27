// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Model-facing tool exposure selectors in product configuration.

/// A model-facing surface on which a tool can be exposed.
#[derive(
    Debug, serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposureSurface {
    /// Nested tools available to Code Mode scripts.
    CodeMode,
    /// Tools discovered later through tool search.
    Deferred,
    /// Tools present in the model's initial tool list.
    Direct,
}

impl std::fmt::Display for ToolExposureSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::CodeMode => "code_mode",
            Self::Deferred => "deferred",
            Self::Direct => "direct",
        })
    }
}
