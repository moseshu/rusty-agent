//! Metadata resolution and hosted catalog collection ported from the Python SDK helpers.

use ra_mcp::{catalog::McpToolInfo, metadata::McpToolMetadata};
use serde_json::json;

#[test]
fn typed_tools_preserve_metadata_precedence_without_trimming() {
    for (description, title, annotation, expected_description, expected_title, expected_model) in [
        (
            Some("Long form"),
            Some("Explicit"),
            Some("Annotated"),
            Some("Long form"),
            Some("Explicit"),
            "Long form",
        ),
        (
            None,
            Some("Explicit"),
            Some("Annotated"),
            None,
            Some("Explicit"),
            "Explicit",
        ),
        (
            Some(""),
            Some(""),
            Some("Annotated"),
            None,
            Some("Annotated"),
            "Annotated",
        ),
        (None, None, None, None, None, ""),
        (Some(""), Some(""), Some(""), None, None, ""),
        (
            Some(" \n"),
            Some("Title"),
            None,
            Some(" \n"),
            Some("Title"),
            " \n",
        ),
        (
            None,
            Some(" \t"),
            Some("Annotated"),
            None,
            Some(" \t"),
            " \t",
        ),
        (
            None,
            None,
            Some("显示名称"),
            None,
            Some("显示名称"),
            "显示名称",
        ),
    ] {
        let raw = json!({"name":"search", "inputSchema":{}, "description":description,
            "title":title, "annotations":{"title":annotation}});
        let tool: rmcp::model::Tool = serde_json::from_value(raw).unwrap();
        let metadata = McpToolMetadata::from(&tool);
        assert_eq!(metadata.title(), expected_title);
        assert_eq!(metadata.description(), expected_description);
        assert_eq!(metadata.description_for_model(), expected_model);
        let info = McpToolInfo::new("docs", tool);
        assert_eq!(info.metadata(), &metadata);
        assert_eq!(info.title(), expected_title);
        assert_eq!(info.description_for_ui(), expected_description);
        assert_eq!(info.description_for_model(), expected_model);
    }
}

#[test]
fn metadata_defaults_have_no_display_fields() {
    let metadata = McpToolMetadata::default();
    assert_eq!(metadata.title(), None);
    assert_eq!(metadata.description(), None);
    assert_eq!(metadata.description_for_model(), "");
}
