#!/usr/bin/env python3
"""Extract pinned Codex windows and execute its original model-name matching functions."""

import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

REVISION = "815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052"
upstream = Path(sys.argv[1]).resolve()
revision = subprocess.check_output(
    ["git", "-C", str(upstream), "rev-parse", "HEAD"], text=True
).strip()
assert revision == REVISION, f"expected {REVISION}, found {revision}"
source = "codex-rs/models-manager/models.json"


def upstream_text(path):
    return subprocess.check_output(
        ["git", "-C", str(upstream), "show", f"{REVISION}:{path}"], text=True
    )


def rust_function(text, name):
    start = text.index(f"fn {name}(")
    opening = text.index("{", start)
    depth, end = 1, opening + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


models = json.loads(upstream_text(source))["models"]
windows = {model["slug"]: model["context_window"] for model in models}
assert all(isinstance(window, int) and window > 0 for window in windows.values())
assert len(windows) == len(models), "duplicate model slugs"
manager_source = "codex-rs/models-manager/src/manager.rs"
manager = upstream_text(manager_source)
fallback_source = "codex-rs/models-manager/src/model_info.rs"
fallback_function = rust_function(upstream_text(fallback_source), "model_info_from_slug")
fallback = int(re.search(
    r"^\s+context_window:\s+Some\(([\d_]+)\)", fallback_function, re.MULTILINE
).group(1).replace("_", ""))
lookup_names = [
    "gpt-5.5-2026-04-23", "gpt-5.5-pro", "custom/gpt-5.5-pro",
    "openai-codex/gpt-5.5-2026-04-23",
    "gpt-daybreak-red-latest-pro", "gpt-daybreak-red-latest-2026-01-01",
    "custom/gpt-daybreak-red-latest-pro", "openai-codex/gpt-daybreak-red-latest",
    "provider_123/gpt-daybreak-red-latest-pro", "openai/gpt-daybreak-red-latest",
    "ns1/ns2/gpt-daybreak-red-latest", "bad.ns/gpt-daybreak-red-latest",
    "bad namespace/gpt-daybreak-red-latest", "/gpt-daybreak-red-latest",
    "供应商/gpt-daybreak-red-latest", "gpt-daybreak-red-latest/extra",
    "GPT-DAYBREAK-RED-LATEST", " gpt-daybreak-red-latest",
    "private-model", "gpt-4.1", "gpt-4o", "",
]
synthetic_candidates = [("gpt-test", 111_000), ("gpt-test-pro", 222_000),
                        ("gpt-test-pro", 999_000), ("custom/gpt-test", 333_000)]
synthetic_names = ["gpt-test-pro-2026", "provider/gpt-test-pro-2026",
                   "custom/gpt-test-pro-2026", "ns1/ns2/gpt-test-pro", "unknown"]


def rust_models(candidates):
    return "vec![" + ",".join(
        f"ModelInfo {{ slug: {json.dumps(slug)}.into(), context_window: Some({window}) }}"
        for slug, window in candidates
    ) + "]"


harness = "\n".join([
    "#[derive(Clone)] struct ModelInfo { slug: String, context_window: Option<u64> }",
    rust_function(manager, "find_model_by_longest_prefix"),
    rust_function(manager, "find_model_by_namespaced_suffix"),
    "fn lookup(model: &str, candidates: &[ModelInfo]) {",
    "let matched = find_model_by_longest_prefix(model, candidates)"
    ".or_else(|| find_model_by_namespaced_suffix(model, candidates));",
    f'println!("{{}}", matched.and_then(|model| model.context_window).unwrap_or({fallback}));',
    "}",
    "fn main() {",
    f"let bundled = {rust_models(windows.items())};",
    *[f"lookup({json.dumps(name, ensure_ascii=False)}, &bundled);" for name in lookup_names],
    f"let synthetic = {rust_models(synthetic_candidates)};",
    *[f"lookup({json.dumps(name)}, &synthetic);" for name in synthetic_names],
    "}",
])
with tempfile.TemporaryDirectory(prefix="codex-model-lookup-") as directory:
    root = Path(directory)
    (root / "lookup.rs").write_text(harness)
    subprocess.run(["rustc", "--edition=2024", str(root / "lookup.rs"),
                    "-o", str(root / "lookup")], check=True)
    results = [int(line) for line in subprocess.check_output(
        [str(root / "lookup")], text=True
    ).splitlines()]
assert len(results) == len(lookup_names) + len(synthetic_names)
output = {
    "revision": REVISION, "source": source, "context_windows": windows,
    "matcher_source": manager_source, "fallback_source": fallback_source,
    "fallback_context_window": fallback,
    "lookups": [{"model": name, "context_window": window}
                for name, window in zip(lookup_names, results)],
    "synthetic_candidates": synthetic_candidates,
    "synthetic_lookups": [{"model": name, "context_window": window}
                          for name, window in zip(synthetic_names, results[len(lookup_names):])],
}
Path(__file__).with_name("upstream-model-windows.json").write_text(
    json.dumps(output, indent=2) + "\n"
)
