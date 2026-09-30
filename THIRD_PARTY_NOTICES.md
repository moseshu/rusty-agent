# Third-party notices

rusty-agent is licensed under Apache-2.0 (see [LICENSE](LICENSE)). Parts of it are copied or
ported from the project below, which is licensed under the MIT License. Its copyright and
permission notice follow the list of what was taken from it.

## OpenAI Agents SDK for Python

Source: <https://github.com/openai/openai-agents-python>

### Text carried verbatim

These files are copied unchanged from the upstream repository and compiled into the crates with
`include_str!`. They are what the model reads, so they are kept byte for byte and carry no header
of their own.

| File in this repository | Upstream file (under `src/agents/`) |
| --- | --- |
| `crates/ra-runtime/src/sandbox/prompt.md` | `sandbox/instructions/prompt.md` |
| `crates/ra-runtime/src/sandbox/memory/prompts/memory_consolidation_prompt.md` | `sandbox/memory/prompts/memory_consolidation_prompt.md` |
| `crates/ra-runtime/src/sandbox/memory/prompts/rollout_extraction_prompt.md` | `sandbox/memory/prompts/rollout_extraction_prompt.md` |
| `crates/ra-runtime/src/sandbox/memory/prompts/rollout_extraction_user_message.md` | `sandbox/memory/prompts/rollout_extraction_user_message.md` |
| `crates/ra-tools/src/sandbox/memory_read_prompt.md` | `sandbox/memory/prompts/memory_read_prompt.md` |

Shorter model-facing strings are also carried verbatim inside Rust sources, each marked as such in
its doc comment:

- the `apply_patch` tool description and Lark grammar (`crates/ra-tools/src/sandbox/apply_patch_tool.rs`);
- the `load_skill` tool description (`crates/ra-tools/src/sandbox/skills.rs`);
- the live-update paragraphs of the memory read prompt (`crates/ra-tools/src/sandbox/memory.rs`);
- the remote mount policy wording (`crates/ra-core/src/sandbox/remote_mount_policy.rs`);
- the shell helper scripts (`crates/ra-sandbox/src/runtime_helpers.rs`,
  `crates/ra-sandbox/src/docker/session/pty.rs`).

The test fixture `tests/it-runtime/tests/fixtures/sandbox_memory/consolidation.md` is the
consolidation prompt above, rendered.

### Code ported to Rust

These modules are translations of upstream Python modules. Each names its upstream source in its
module documentation.

- **Sandbox contracts** (`crates/ra-core/src/sandbox/`): events, sinks, memory configuration,
  remote mount policy, skills metadata and token truncation.
- **Sandbox backends** (`crates/ra-sandbox/`): the Docker sandbox, session instrumentation, PTY
  output and PTY execution, lazy skill sources and the runtime helper scripts.
- **Sandbox capabilities** (`crates/ra-tools/src/sandbox/`): filesystem, shell, `apply_patch`,
  `view_image`, skills, memory and compaction.
- **Sandbox runtime** (`crates/ra-runtime/src/sandbox/`): the sandbox run configuration, the
  per-run runtime, the session manager, agent preparation and the memory pipeline (rollouts,
  storage, phases one and two, and the manager); also run grouping
  (`crates/ra-runtime/src/runner/grouping.rs`).
- **V4A applier** (`crates/ra-patch/src/apply_diff.rs`, `crates/ra-patch/src/editor.rs`): the
  reference's `apply_diff.py` and its editor protocol.

The provider-neutral contracts in `ra-core` (model settings, input and output items, tools,
usage) follow the upstream SDK's public API shape and semantics.

### License

```text
MIT License

Copyright (c) 2025 OpenAI

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Dependencies

Crate dependencies are not vendored: each is fetched from crates.io with its own license, and
`cargo deny check licenses` limits them to the licenses allowed in [deny.toml](deny.toml).
