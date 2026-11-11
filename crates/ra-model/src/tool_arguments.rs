//! Function-call argument text, shared by the provider codecs.

/// A function call's argument text as JSON: blank text is `{}`, as an absent value; `None` when the
/// text is not JSON.
///
/// Text that is not JSON does not fail the response that carried it. The call keeps the text and
/// is reported to the model as a failure when it runs, as the reference parses arguments only when
/// the tool is invoked and reports what it cannot parse there.
pub(crate) fn function_arguments(text: &str) -> Option<serde_json::Value> {
    if text.trim().is_empty() {
        return Some(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(text).ok()
}
