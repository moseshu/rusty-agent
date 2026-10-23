# Skills catalog prompt reference

`upstream-render.json` is the executed output of openai/codex commit
`815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052` (Apache-2.0). Regenerate with:

```sh
python3 generate_reference.py /path/to/codex
```

The script verifies the checkout revision and builds an isolated Rust harness. It compiles the
original `render.rs` and `render_dedup.rs`, `catalog_prompt.rs`, `aliases.rs`, the original
`AvailableSkillsInstructions` implementation and fragment `render` method, and the upstream byte
based token estimator. Catalog/environment records and the fragment trait boundary are substituted;
no discovery, executor, OpenAI account, model or Rusty implementation supplies expected text. The
Host alias discovery stub is never used: these cases cover Executor/Cloud single-catalog rendering.
This does not run the complete Codex workspace or claim the multi-catalog allocator was tested.

The 28 cases preserve complete marked prompts, including optional usage instructions. They cover
empty and hidden inventories, provider order, short descriptions, Executor/Cloud and mixed locators,
Unicode limits, character versus token budgets, omission notices, no fragment fitting, alias costs
and selection, alias order, literal backslashes, context percentage and fallback, and the explicit
10,000 token override cap. Repeated large descriptions make budget decisions observable in the
rendered text, rather than only checking the budget helper's return value.

`it-tools/skills_catalog_prompt` compares real run-bound capability output byte for byte against
these results. Its separate lifecycle tests exercise shared discovery and reading a listed alias
without calling `skills.list`. `it-coding/skills_catalog_prompt` uses production assembly and the
Runner to verify model-visible instructions and direct reads from an external skill root.

The Rust capability also declares a paragraph token ceiling independently of measured output. Tests
compare that ceiling to the actual estimate, then subtract this fixture's upstream metadata budget
to verify a constant fixed-wording allowance per usage setting. This allowance is Rust host
bookkeeping, not additional upstream prompt text; no expected prompt was regenerated for it.
