# Skills catalog prompt reference

`upstream-render.json` is the executed output of openai/codex commit
`815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052` (Apache-2.0). Regenerate with:

```sh
python3 generate_reference.py /path/to/codex
```

The script verifies the checkout revision and builds an isolated Rust harness. It compiles the
original `render.rs`, `render_dedup.rs`, `host_aliases.rs`, `catalog_prompt.rs`, `aliases.rs`, the
original fragment rendering and byte-based token estimator. It also extracts and compiles the
**unmodified bodies** of `world_state_catalogs.rs::CatalogContext::render_catalogs`,
`render_with_cloud_limit`, and `rebalance_to_retain_all_skills`, together with their original
contribution/allocation records and attributes. The runtime context is substituted with
`include_instructions=true` and `previous_world_state=None`. The original entry point computes
its own Cloud fingerprint and allocation; neither source preference nor the budgeting control
flow is supplied by Rusty or rewritten in the harness.

Catalog/environment records and the fragment trait boundary are substituted. No discovery,
executor, OpenAI account, model or Rusty implementation supplies expected text. This does not run
the full Codex workspace, filesystem discovery, or persistent world-state diff/recovery.

The 66 cases preserve each source's marked fragment separately, in the original entry point's
contribution order. There is no synthetic expected prompt assembled with Rusty's newline join.
The production test splits Rusty's marked run prompt and compares its fragments byte for byte
with those independently returned by the original entry point. The fixture also records each
source's budget, metadata cost, report and accepted Cloud cap.

Coverage includes the earlier inventories, aliases, Unicode, token/character budgets, omission
notices and explicit override limits. The six upstream pressure cases come directly from
`catalog_rebalances_only_to_avoid_omissions_and_retains_the_allocation`: description-only pressure
(17 Executor / 82 Cloud), rescuable omissions (40 / 5), and unavoidable omissions (40 / 160), each
with token and fallback character accounting. Four more cases exercise Host and mixed-filesystem
rescues. Every case also runs the original entry point with only its Cloud inventory, so tests can
compare initial and accepted Cloud text/caps without reproducing that policy. These are fresh
allocations for each call; persisted cross-step allocations are outside this Rust adaptation.

Additional cases verify empty/disabled/hidden Cloud inventories release their unused allowance,
Cloud-only capping, odd-budget rounding, and qualified Cloud preference that removes only the
Executor copy while retaining Host. `it-tools/skills_catalog_prompt` also verifies shared run
loading and reading aliases without `skills.list`; `it-coding/skills_catalog_prompt` exercises
production assembly and the Runner.

The Rust capability also declares a paragraph token ceiling independently of measured output. Tests
compare that ceiling to the actual estimate, then subtract this fixture's upstream metadata budget
to verify a constant fixed-wording allowance per usage setting and configured source count. This allowance is Rust host
bookkeeping, not additional upstream prompt text; no expected prompt was regenerated for it.
