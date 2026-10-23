// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bounded discovery warnings.

use super::render::truncate_utf8_to_bytes;

const MAX_WARNINGS: usize = 4;
const MAX_WARNING_BYTES: usize = 256;

pub(crate) fn bounded_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .take(MAX_WARNINGS)
        .map(|warning| truncate_utf8_to_bytes(warning, MAX_WARNING_BYTES).0)
        .collect()
}
