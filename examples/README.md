# Framework examples

Start with the framework quickstart and
[basic_agent](basic_agent/README.md). Each directory below is an independently compiled
workspace binary. Run it with `cargo run -p NAME`, not `--example`.

| Example | What it demonstrates | Offline mode |
| --- | --- | --- |
| [basic_agent](basic_agent/README.md) | Configured provider, text input, async/sync calls and settled stream views | Scripted provider |
| [custom_tools](custom_tools/README.md) | Sync/async `#[tool]`, parameter defaults, context and fallible output | Scripted model/tool/result round trip |
| [structured_output](structured_output/README.md) | Type-generated schemas, validated values and invalid output errors | Ordinary and streamed scripted checks |
| [sessions](sessions/README.md) | Two turns, automatic history and SQLite reopen | Local HTTP, in-memory and SQLite |
| [streaming](streaming/README.md) | Raw text deltas, settled tool/message items and `finish` | Local HTTP/SSE |
| [app_context](app_context/README.md) | Typed local context, borrowed async instructions and lifecycle callbacks | Local HTTP plus context and callback assertions |
| [handoffs](handoffs/README.md) | Full history, typed transfer callback and explicit filtering | Local HTTP and authoritative session checks |
| [agents_as_tools](agents_as_tools/README.md) | Generated specialist input, returned tool output and manager synthesis | Three local HTTP requests |
| [approvals](approvals/README.md) | Serialized checkpoint, approve/reject and ordinary/streamed recovery | Local HTTP plus execution and history counts |
| [guardrails](guardrails/README.md) | Passing input/output checks, blocking input and output tripwires | Local HTTP plus exact error assertions |
| [minimal_agent](minimal_agent/README.md) | Third-party `Tool`, `Model` and `ModelResolver` using only kernel crates | Fully offline extension gate |

Each README lists dependencies, features, commands, environment variables and what its
check proves. All provider-backed examples default to live mode, requiring explicit
`OPENAI_API_KEY` and `RA_MODEL`; `OPENAI_BASE_URL` optionally selects another Responses
endpoint. Select `--check` explicitly for offline fixtures. Credentials are not needed
for checks, and missing live credentials produce an error rather than a successful skip.
The shared [support.rs](support.rs) configures and reuses the shipped provider for the seven
HTTP-backed tutorials. Its loopback server and wire payloads are example fixtures.

Run all documented offline variants and verify source excerpts with Python 3.11 or later:

```bash
python3 scripts/check_examples.py
```

[checks.json](checks.json) is the CI run matrix. The checker requires every example crate
to appear, verifies its README commands, compiles all binaries, and executes every variant.
It removes provider environment overrides before executing checks. The seven HTTP tutorials
use only loopback traffic; the remaining checks are entirely in process. No paid/live
endpoint is included in this command.

`minimal_agent` remains the kernel extension gate: its framework dependencies are exactly
`ra-core` and `ra-runtime`, enforced by `cargo xtask layering`. Its custom model and resolver
demonstrate extension points; applications using a shipped provider start with `basic_agent`.

MCP integration is available in the framework but a dedicated MCP tutorial remains future
work outside this quickstart scope; there is no `mcp_integration` example crate yet.

## Future examples

After the framework is complete, optional examples may demonstrate:

- Read-only research using retrieval tools and reports with source references.
- Plan-and-execute workflows using the orchestration APIs.
- Parallel specialist agents that return results for synthesis.
- A host-supplied memory store connected through the public memory contracts.

These are candidate tutorials, with no implementation or example crates added yet. Keep each
example focused on public API usage and reuse the framework's tools and runtime. Data sources,
memory backends, prompts, and report policies belong to the example or its host application.
A general assistant product and comparisons between product lines are outside the framework's
development and release requirements. New product lines can be built separately on the
completed framework.
