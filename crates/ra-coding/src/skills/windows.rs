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
    find_model_by_longest_prefix(model, candidates)
        .or_else(|| find_model_by_namespaced_suffix(model, candidates))
        .unwrap_or(FALLBACK_CONTEXT_WINDOW)
}

fn find_model_by_longest_prefix(model: &str, candidates: &[(&str, u64)]) -> Option<u64> {
    let mut best: Option<&(&str, u64)> = None;
    for candidate in candidates {
        if !model.starts_with(candidate.0) {
            continue;
        }
        let is_better_match = if let Some(current) = best {
            candidate.0.len() > current.0.len()
        } else {
            true
        };
        if is_better_match {
            best = Some(candidate);
        }
    }
    best.map(|candidate| candidate.1)
}

fn find_model_by_namespaced_suffix(model: &str, candidates: &[(&str, u64)]) -> Option<u64> {
    // Retry metadata lookup for a single namespaced slug like `namespace/model-name`.
    // This only strips one leading namespace segment and only when the namespace looks
    // like a simple provider id to avoid broadly matching arbitrary aliases.
    let (namespace, suffix) = model.split_once('/')?;
    if suffix.contains('/') {
        return None;
    }
    if namespace.is_empty()
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    find_model_by_longest_prefix(suffix, candidates)
}
