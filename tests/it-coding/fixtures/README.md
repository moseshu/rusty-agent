# Permission decision baseline

`permission_decision_table_legacy_core.json` preserves the original six-tool capture unchanged.
The expanded table covers the complete configured coding surface, built-in memory and MCP
resource tools, sandbox adapters, an agent tool, and explicit unclassified/multiple-class probes.
Each mode and approval result is evaluated after settings initialize/prepare for both OnRequest
and Never. No class is synthesized when the declared array is empty.

The expanded baseline was reconciled against the legacy evaluator and adapter declarations at
the `baseline_revision` recorded in the JSON. The old six-tool rows were checked individually.
This is an expanded replay of the old implementation, not a claim that all adapters had been
captured before the refactor. The `legacy_differences` list records each changed decision and
its named exception. `exceptions` also identifies the agent ownership correction whose end-to-end
coverage is in `agent_handoff/approval_ownership.rs`. The open-class probes specify the local
extension boundary and have no released-tool counterpart.

The test compares installed behavior with this frozen table. Updates require inspecting the
legacy decision and recording an explicit exception; tests do not regenerate the baseline.
