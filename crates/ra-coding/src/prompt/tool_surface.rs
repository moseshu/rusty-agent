//! Stable, reviewable inventory of the tools advertised with a coding-agent prompt.
//!
//! One tool list produces two artifacts, and keeping them apart is the point.
//!
//! The **prompt section** carries the advertised names and nothing else. It is model-visible and
//! sits in the cached prefix, so a 64-character digest there would spend prefix tokens on
//! something the model cannot act on, and would invalidate the instructions span every time a
//! schema was tweaked — even though the instructions themselves did not change. What changed is
//! the provider's tool table, which is a different part of the request and breaks its own cache.
//!
//! The **surface digest** carries the revision and a fingerprint over every model-facing field. It
//! lives only in the committed snapshot, where a description- or schema-only edit — the kind the
//! names list alone would miss — still cannot land unnoticed.
//!
//! The provider wire snapshot remains the authority for the full schema.

use std::collections::BTreeMap;
use std::sync::Arc;

use ra_core::{
    error::{Error, Result},
    model::ModelToolDefinition,
    prompt::{
        ContentHash, PromptSection, PromptSectionName, PromptSource, SectionPosition,
        SectionStability,
    },
    tool::Tool,
};

/// Product-owned revision of the advertised coding-tool schemas.
///
/// Raise this value whenever any model-facing name, description, strictness flag, or input schema
/// changes. The tool-surface snapshot rejects a changed fingerprint with an unchanged revision,
/// so a reviewer can distinguish a deliberately invalidated tool table from an accidental edit.
pub(crate) const TOOL_SCHEMA_REVISION: u32 = 1;

/// Builds the prompt section and the reviewable digest for one advertised tool surface.
pub(crate) struct ToolSurfacePromptBuilder;

impl ToolSurfacePromptBuilder {
    /// Builds the model-visible inventory: the advertised names, sorted, and nothing else.
    ///
    /// Registration order is host startup detail, and letting it reach this text would invalidate
    /// an otherwise reusable prefix.
    pub(crate) fn build_tool_surface_section(
        tools: &[Arc<dyn Tool>],
    ) -> Result<Option<PromptSection>> {
        let entries = advertised_entries(tools)?;
        if entries.is_empty() {
            return Ok(None);
        }

        let content = format!(
            "Available tool schemas:\n{}\n\nUse only these tools. Their provider-supplied \
             schemas define the accepted arguments.",
            render_names(&entries)
        );

        PromptSection::new(
            PromptSectionName::TOOL_SURFACE,
            "Stable advertised tool inventory",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
        .map(Some)
    }

    /// Builds the committed record: the revision, the surface fingerprint, and the same names.
    ///
    /// This is what review diffs, and it is not model-visible, so it can afford to carry a digest
    /// that moves on any advertised-field edit.
    pub(crate) fn build_surface_digest(tools: &[Arc<dyn Tool>]) -> Result<Option<String>> {
        let entries = advertised_entries(tools)?;
        if entries.is_empty() {
            return Ok(None);
        }

        let material = entries
            .iter()
            .map(|(name, fingerprint)| format!("{name}:{fingerprint}"))
            .collect::<Vec<_>>()
            .join("\n");
        let surface_hash = ContentHash::compute(material.as_bytes());

        Ok(Some(format!(
            "revision {TOOL_SCHEMA_REVISION}; surface SHA-256: {surface_hash}\n{}\n",
            render_names(&entries)
        )))
    }
}

/// Collects the advertised tools, keyed and therefore sorted by model-facing name.
fn advertised_entries(tools: &[Arc<dyn Tool>]) -> Result<BTreeMap<String, ContentHash>> {
    let mut entries = BTreeMap::new();
    for tool in tools {
        if !tool.options().can_reach_model_surface() || !tool.options().is_advertised() {
            continue;
        }

        let definition = tool.model_definition();
        let name = definition.name().to_owned();
        let fingerprint = definition_fingerprint(&definition)?;
        if entries.insert(name.clone(), fingerprint).is_some() {
            return Err(Error::config(format!(
                "tool prompt surface advertises model-facing name `{name}` more than once"
            )));
        }
    }
    Ok(entries)
}

fn render_names(entries: &BTreeMap<String, ContentHash>) -> String {
    entries
        .keys()
        .map(|name| format!("- `{name}`"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn definition_fingerprint(definition: &ModelToolDefinition) -> Result<ContentHash> {
    let schema = serde_json::to_string(definition.input_schema()).map_err(|error| {
        Error::config("failed to render advertised tool schema fingerprint").with_source(error)
    })?;
    let mut material = String::new();
    append_field(&mut material, definition.name());
    append_field(&mut material, definition.description().unwrap_or_default());
    append_field(
        &mut material,
        if definition.strict() { "true" } else { "false" },
    );
    append_field(&mut material, &schema);
    Ok(ContentHash::compute(material.as_bytes()))
}

/// Appends one length-prefixed field, so concatenated fields cannot impersonate each other.
///
/// Built from infallible `String` pushes rather than `write!`: the latter returns a `Result` that
/// only an allocator failure could produce, and the crate denies unwrapping it away.
fn append_field(material: &mut String, value: &str) {
    material.push_str(&value.len().to_string());
    material.push(':');
    material.push_str(value);
}
