// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Metadata limits shared with the upstream catalog renderer.
use std::borrow::Cow;

pub(super) const MAX_SKILL_NAME_BYTES: usize = 256;
const MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1024;
const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";

pub(crate) fn truncate_catalog_skill_description(description: &str) -> Cow<'_, str> {
    if description
        .char_indices()
        .nth(MAX_CATALOG_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return Cow::Borrowed(description);
    }

    let prefix_chars = MAX_CATALOG_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let mut truncated = description[..prefix_end].to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    Cow::Owned(truncated)
}

pub(super) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let mut end = contents.len().min(max_bytes);
    while !contents.is_char_boundary(end) {
        end -= 1;
    }
    (contents[..end].to_string(), end < contents.len())
}
