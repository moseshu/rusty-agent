//! MCP output retains embedded media bytes and serializes unexpanded references as JSON text.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ra_core::{
    item::{FileSource, ImageSource},
    tool::ToolOutputBlock,
};
use ra_mcp::{
    rmcp::model::{CallToolResult, ContentBlock},
    tool::map_tool_result,
};
use serde_json::json;

#[test]
fn text_image_audio_and_resources_keep_order_and_bytes() {
    let result:CallToolResult=serde_json::from_value(json!({"content":[
        {"type":"text","text":"caption"},
        {"type":"image","data":"aW1hZ2U=","mimeType":"image/png"},
        {"type":"audio","data":"YXVkaW8=","mimeType":"audio/wav"},
        {"type":"resource","resource":{"uri":"file:///doc.txt","mimeType":"text/plain","text":"中文 fixture"}},
        {"type":"resource","resource":{"uri":"file:///doc.pdf","mimeType":"application/pdf","blob":"cGRm"}},
        {"type":"resource_link","uri":"https://example.test/report.pdf","name":"Report"}
    ],"_meta":{"ui_only":"must not reach model"}})).unwrap();
    let expected_link = serde_json::to_value(&result.content[5]).unwrap();
    let output = map_tool_result(result).unwrap();
    assert_eq!(output.blocks().len(), 6);
    assert_eq!(output.blocks()[0].as_text(), Some("caption"));
    let ToolOutputBlock::Image(image) = &output.blocks()[1] else {
        panic!("image")
    };
    let ImageSource::Base64(image) = image.source() else {
        panic!("inline image")
    };
    assert_eq!(image.media_type(), "image/png");
    assert_eq!(image.data(), "aW1hZ2U=");
    assert_eq!(
        output.blocks()[2],
        ToolOutputBlock::audio("data:audio/wav;base64,YXVkaW8=")
    );
    for (index, bytes) in [(3, "中文 fixture".as_bytes()), (4, b"pdf".as_slice())] {
        let ToolOutputBlock::File(file) = &output.blocks()[index] else {
            panic!("file")
        };
        let FileSource::Base64(file) = file.source() else {
            panic!("inline file")
        };
        assert_eq!(STANDARD.decode(file.data()).unwrap(), bytes);
        assert!(file.filename().unwrap().starts_with("file:///"));
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output.blocks()[5].as_text().unwrap()).unwrap(),
        expected_link
    );
    assert!(!serde_json::to_string(&output).unwrap().contains("ui_only"));
}
#[test]
fn server_error_content_remains_actionable_even_with_structured_content() {
    let mut result = CallToolResult::error(vec![ContentBlock::text("Please fix the query")]);
    result.structured_content = Some(json!({"answer":"not the error"}));
    assert_eq!(
        map_tool_result(result).unwrap().as_text(),
        Some("Please fix the query")
    );
}
#[test]
fn default_content_precedence_matches_python_and_empty_results_answer_the_call() {
    let mut result = CallToolResult::success(vec![ContentBlock::text("ordinary content")]);
    result.structured_content = Some(json!({"answer":42}));
    assert_eq!(
        map_tool_result(result).unwrap().as_text(),
        Some("ordinary content")
    );
    assert_eq!(
        map_tool_result(CallToolResult::success(vec![]))
            .unwrap()
            .as_text(),
        Some("")
    );
}
