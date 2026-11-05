// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Catalog windows and model lookup from the bundled model descriptors and manager.

const FALLBACK_CONTEXT_WINDOW: u64 = 272_000;

// These are the resolved catalog windows, not max_context_window or API compaction capacities.
const MODEL_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("gpt-6-astra", 272_000),
    ("gpt-6.1-sol", 272_000),
    ("gpt-6-sol", 272_000),
    ("gpt-6-luna", 272_000),
    ("gpt-5.6-sol", 272_000),
    ("gpt-5.6-terra", 272_000),
    ("gpt-5.6-luna", 272_000),
    ("gpt-daybreak-blue-latest", 272_000),
    ("gpt-daybreak-red-latest", 372_000),
    ("gpt-5.5", 272_000),
    ("codex-auto-review", 272_000),
];

pub(super) fn context_window(model: &str) -> u64 {
    context_window_from_candidates(model, MODEL_CONTEXT_WINDOWS)
}

pub(super) fn context_window_from_candidates(model: &str, candidates: &[(&str, u64)]) -> u64 {
    crate::config::catalog::find_model_by_slug(model, candidates).unwrap_or(FALLBACK_CONTEXT_WINDOW)
}
