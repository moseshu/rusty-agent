# Skill catalog model windows

`upstream-model-windows.json` contains every `slug` and `context_window` from openai/codex
commit `815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052`, extracted from the committed
`codex-rs/models-manager/models.json`. Regenerate with:

```sh
python3 generate_model_windows.py /path/to/codex
```

The script verifies the upstream revision and reads its committed JSON directly; Rusty code does
not supply the expected capacities. These are the windows used by `ModelInfo::resolved_context_window`,
not `max_context_window` or the Agents SDK compaction capacity table.

Lookup expectations execute the original `find_model_by_longest_prefix` and
`find_model_by_namespaced_suffix` functions from the pinned `models-manager/src/manager.rs`,
compiled with `rustc` in an isolated harness. Only the `ModelInfo` record is substituted with its
slug and window fields. The fallback window is extracted from `model_info_from_slug` in the
committed `model_info.rs`; the script does not reproduce the matching algorithm in Python.
Cases cover dates, variants, simple provider namespaces, invalid and multiple namespace segments,
unknown and empty names, case sensitivity, overlapping prefixes, first-wins ties and full-name
matching ahead of namespace retry.

The integration test binds the product-selected skills capability for every model, both as a bare
slug and with a provider namespace. It verifies the declared paragraph ceiling
against two percent of the upstream window plus the fixed wording allowance, with usage instructions
both disabled and enabled. It also checks every executed lookup case through the product capability.
Named models without a match, including SDK-only models, use the 272,000-token fallback; only a
binding context with no model name uses 8,000 characters. Synthetic descriptor cases exercise the
same matcher via the `test-api` boundary. The production Runner test separately checks model
switching, unnamed defaults, explicit token limits and validated host overrides taking precedence
over prefix matches, namespace retry and fallback.
