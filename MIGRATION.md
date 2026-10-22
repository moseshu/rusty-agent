# Migration guide

Each crate documents whether its surface is `Stable`, `Evolving`, or `Internal`. A `Stable`
surface changes incompatibly only in a new `0.x` release, and every such change is listed here
with what to do about it. Upgrade all crates together: crates from two tags are two copies of
`ra-core`, and their types do not interoperate.

## 0.3.0 (unreleased)

0.3.0 is in development on `main`. This section lists the incompatible changes that have
landed so far; further changes planned for this release are added here as they land.

### Model settings resolve by named layer

Model settings now resolve in provider → model → agent → run order. For scalar settings, the last
layer that sets a value wins. Model registration defaults fill fields the agent and run leave
unset; this is a deliberate registry extension and differs from the OpenAI Agents SDK, which
discards all implicit model-specific defaults when the agent has any explicit model setting.
`retry` continues to merge across layers.

`metadata`, `extra_headers`, and `extra_query` replace the whole map. Use
`with_metadata_map`, `with_extra_headers_map`, or `with_extra_query_map` with an empty map to clear
an inherited value. Provider-bucketed `extra_body` uses a local shallow merge of top-level keys:
later layers replace the complete value for a matching key, including nested objects. This is not
the SDK's `extra_body` behavior (which replaces the map); the SDK's `extra_args` uses a shallow
top-level merge, but it is a separate API.

`timeout` follows the same later-layer-wins rule; the run deadline remains controlled by
`BudgetLimit`. `max_tokens` in `ModelRegistration::with_defaults` is only a default. Declare a hard
model limit with `ModelRegistration::with_max_output_tokens`; requests above it now fail with a
configuration error before calling the model instead of being silently truncated. Provider
requirements such as Anthropic's fallback `max_tokens` remain in provider defaults.

Custom `ModelResolver` implementations that know a model's hard output limit must attach it to the
resolved model. After constructing `ResolvedModel`, call `with_output_token_limit(limit)`:

```rust,ignore
ResolvedModel::new(selector, model, provider_defaults, model_defaults)
    .with_output_token_limit(4_096)
```

Without this declaration, the runtime cannot enforce that model's hard limit.

`ModelSettings::resolve` has been removed because its four same-typed positional layers were easy
to pass in the wrong order. Build the layers by name; any layer you omit is treated as empty:

```rust,ignore
SettingsLayers::new()
    .with_provider(&provider_defaults)
    .with_model(&model_defaults)
    .with_agent(&agent_settings)
    .with_run(&run_overrides)
    .resolve(&provider_key)
```

The `ModelSettings` serialized schema is now version 2 so an explicitly empty map remains distinct
from an unset map. When reading schema version 1 settings, empty `metadata`, `extra_headers`, and
`extra_query` maps are treated as unset to preserve the old format's meaning, and the in-memory
settings are upgraded to version 2 for subsequent writes. Schema version 2 empty maps continue to
clear inherited values. Resolved trace settings retain their own schema version because their
shape did not change.

### Run limits follow the framework defaults

`ra_runtime::runner::DEFAULT_MAX_TURNS` is now 10 (previously 32). An exhausted turn
limit returns `Error::Budget { kind: BudgetKind::MaxTurns, .. }` before the next model
request. The existing `Error::Run` checkpoint wrapper retains that classification and exposes
the generated history through `error.run_state()`, adapting the SDK error's `run_data`.
A final answer on the last allowed turn still succeeds. Install a `RunErrorHandler`
to produce a final delivery on exhaustion; returning `None` from the handler preserves the error.
`AgentTool` defaults to 10 nested turns and applies its failure policy to exhaustion.

`RunConfig::with_max_turns` accepts either a number or `None` for unlimited turns.
`AgentToolBuilder::max_turns` accepts a number; `None` selects the default 10 turns,
matching the SDK's `as_tool(max_turns=None)`. Omitting the call also keeps 10.
Both builders accept zero, which exhausts before any model request, and are no longer
`const fn`. `RunConfig::max_turns()` now returns `Option<u32>`; callers must handle `None`.

`RunConfig::with_budget` keeps the configured turn limit when the supplied `BudgetLimit` has
none, so setting only a token ceiling no longer drops the turn limit. A turn limit in the
supplied budget still replaces the configured one; use `with_max_turns(None)` for unlimited
turns.

Function-tool concurrency is unlimited by default. `with_max_function_tool_concurrency`
accepts a positive number or `None`; its getter now returns `Option<usize>`, and its builder
is no longer `const fn`. This changes the global dispatch cap only: bare `ToolOptions`
remain `Exclusive`, while macro-generated function tools remain `Parallel`.

`CodingHost::build_run_config()` explicitly selects the product's 32 turns and concurrency 8.
Other hosts can select those limits with `with_max_turns(32)` and
`with_max_function_tool_concurrency(8)`.

RunState schema 14 saves the turn limit, including explicitly unlimited runs. A resumed run
uses its saved limit and spent turns even if the new request supplies a different limit.
Tokens and deadlines remain configuration supplied on each continuation.

Checkpoints written before schema 14 carry no turn limit, so they resume under the limit the
request supplies, which is then saved. With the new default of 10, an older paused run that
has already used 10 or more turns fails with the turn-limit error as soon as it resumes. To
resume such a run under its original default, configure `with_max_turns(32)` for that
continuation.

### Coding permission modes move out of core

Replace `ra_core::permission::PermissionMode` with `ra_coding::CodingPermissionMode` in
coding products. Its labels, Display output and camelCase serde spellings are unchanged.
Replace `with_permission_mode(mode)` with `with_permission_policy(mode.policy())`.
Provider-neutral hosts can omit the policy entirely to retain normal SDK approval behavior.

`PermissionScope` and `ToolOptions::with_permission_scope` are replaced by the open
`ActionClass` identifier and `ToolOptions::with_action_classes([class, ...])`. The plural
builder sorts and deduplicates classes. An empty list is unclassified; it does not inherit
Execute. ToolOptions schema 2 reads legacy schema 1 `permission_scope`, including the old
implicit Execute default. Built-in adapter constants are in `ra_tools::action_classes`:
`READ`, `EDIT`, and `EXECUTE`. Generic adapters below ra-tools declare their local effects
without depending upward on the coding product's vocabulary module.

`ActionClassPolicy` is an optional local runtime extension, separate from the framework
approval contract. `allow_only` checks every declared class before rules; matching rules
precede `auto_allow` and `auto_allow_all`. Restricted and class-based automatic approvals
require a nonempty class list. Disabling prompts changes only `prompts`: it preserves the
restriction, automatic approvals and rules. Coding settings apply this overlay only for
`AskForApproval::Never`; other policies preserve the base engine, including an absent policy
and DontAsk. Framework automatic approval does not bypass tool-internal approval or sandbox checks.

The unused callback types `ToolApprovalHandler`, `ToolApprovalDecision`, `ToolApprovalAllow`,
`ToolApprovalDeny`, `ToolPermissionContext` and `PermissionUpdate*` have been removed without replacements. Use the existing tool approval
contract and pending interruption APIs.

### Sticky decisions belong to their agent

An `always` answer now binds the exact tool identity and its owning agent, across handoffs and
JSON checkpoints. `matches_origin_for_agent` evaluates these rules. Name-only `matches` and
`matches_origin` return false for agent-owned rules, because their inputs cannot prove ownership.
Configured rules without an owner retain their wildcard behavior.

Rust uses the registry-unique `AgentId` as durable ownership rather than Python object identity.
Hosts must preserve those IDs when rebuilding an agent registry; assigning an existing ID to a
new capability intentionally reuses that logical identity. This is the necessary adaptation for
an owned, serializable Rust agent graph. Sticky answers without agent provenance return a caller
error before recording any answer; the host may still answer that call once.

RunState schema 15 records the new rule field. Reading schemas through 14 drops ownerless
session rules, so future calls require fresh approval. Exact pending-call answers remain intact.
The migration runs only for old schemas. A current-schema rule is not silently discarded.

### Dynamic instructions are system instructions

Static, synchronous, and asynchronous instruction callbacks now all supply the model's system
instructions, as `Agent.get_system_prompt` does in the OpenAI Agents SDK. A callback runs once per
prepared model turn, and its text no longer enters conversation input or the session. Each
provider adapter places it where its protocol expects system text: Responses `instructions`, a
leading Chat Completions system message, or the Anthropic `system` block.

| Change | Who is affected | What to do |
| --- | --- | --- |
| Generated instructions are sent as system instructions instead of a trailing user message | Agents built with `AgentInstructions::dynamic` / `dynamic_fn`, or with `AgentSpecBuilder::dynamic_instructions` / `dynamic_instructions_fn` | Nothing, if system placement is what you want. To keep the old trailing-user placement, use `AgentInstructions::dynamic_tail` or `dynamic_tail_fn` |
| `ResolvedInstructions` gains a `Tail` variant; `Generated` now means system text | Code that matches `ResolvedInstructions` exhaustively | Add a `Tail` arm. Handle `Generated` as system text |
| Capability assembly no longer trims static instructions, and no longer rejects generated instructions | Hosts that relied on surrounding whitespace being removed, or on the rejection | Trim the text yourself if you need it trimmed. Only a tail source is still refused |
| Empty instructions follow each provider's wire contract | Hosts that inspect requests | Responses sends an empty string, Chat Completions omits the system message, and Anthropic omits the `system` field |

New in 0.3.0: `AgentInstructions::dynamic_sync_fn` and `dynamic_async_fn`, and the builder methods `dynamic_instructions_sync_fn` and `dynamic_instructions_async_fn`.

### `WorkStateHandle` is removed

`ra_core::state::WorkStateHandle`, `ToolServices::with_work_state`, and
`ToolServices::work_state` are removed. They were the mount point reserved for a graph
orchestration engine that is no longer planned, and nothing in the framework read them.

Keep cross-run task state in your application and reach it from tools through
`RunContext::app_context`. A fixed workflow is ordinary Rust control flow around
`Runner::run` calls. `RunState` keeps its `work_state_ref` and `graph_cursor` fields, so
checkpoints written by earlier versions still load.

### `ApplyPatchResult::output` becomes the reader (announced in 0.2.2)

0.2.2 deprecated the `ApplyPatchResult::output` constructor. Before 0.3.0 is released, the
constructor is removed and the `output_text` reader is renamed `output`, matching the field name
in the OpenAI Agents SDK.

| 0.2.x | 0.3.0 |
| --- | --- |
| `ApplyPatchResult::output(text)` | `ApplyPatchResult::default().with_output(text)` |
| `result.output_text()` | `result.output()` |

Move to `with_output` now; it already exists in 0.2.2.

## 0.2.0

Session I/O takes `SessionPath` in place of `&str`, several sandbox request types are built
through constructors and read through accessors instead of public fields, and a few sandbox
defaults and error codes follow the OpenAI Agents SDK.

| Change | Who is affected | What to do |
| --- | --- | --- |
| Session I/O takes `SessionPath<'_>` (eleven path-taking methods), and `validate_path_access` returns `PosixPath` | Third-party backends that implement `SandboxSession` | Take `SessionPath<'_>` and read its text with `as_str()`; callers pass `path.into()` |
| `ra_sandbox::listing::parse_ls_la` is deprecated in favour of `try_parse_ls_la`, which returns a `Result` and fails the whole listing when a permission field is invalid | Hosts that parse `ls -la` output themselves | Switch to `try_parse_ls_la`. The old function keeps its signature and lenient behaviour and only adds a deprecation warning |
| An `ls` whose output cannot be parsed reports `ListingUnreadable`, which is not retryable | Hosts that route on error codes | Do not retry it as a transport error |
| `PermissionsParseError` gains `ReadFlag`, `WriteFlag`, and `ExecFlag` | Code that matches the enum | The enum is `#[non_exhaustive]` and already needs a wildcard arm; distinguish the three flags if you need to |
| `ConcurrencyLimitError` messages use the SDK's configuration field names | Code that compares error strings | Match on the variant instead |
| `SandboxSessionEventBase::with_ts` is no longer a `const fn`, and timestamps are truncated to whole microseconds | Callers in a const context, or code that relies on nanosecond precision | Drop the const use; read timestamps at microsecond precision |
| Numeric timestamps are parsed as the SDK parses them: a millisecond threshold, negative values, and years 0001 to 9999 | External event sources that write numeric timestamps | Write values above 2e10 as milliseconds, or use RFC 3339 |
| A run-level capability and a sandbox agent capability of the same family are refused | Hosts that install the same family in both places | Install each family in one place |
| `SandboxAgentConfig::new()` and `default()` mean "the default capability set", no longer "no capabilities" | Runs that build a sandbox agent with `new()` and inject no default set: they used to run without capabilities and are now refused with a configuration error before a session is created | For the default set, call `SandboxRunConfig::with_default_capabilities(ra_tools::sandbox::default_capabilities)` on the run or build the agent with `ra_tools::sandbox::sandbox_agent_config()`. For no capabilities, use `SandboxAgentConfig::empty()`. `new().with_capability(..)` is unaffected |
| Capability sampling parameters deep-merge `extra_body` | Hosts that relied on a later capability replacing an earlier one's object wholesale | Have the later capability write different keys |
| `ExecRequest`, `PtyStartRequest`, `PtyWriteRequest`, and `CreateRequest` have private fields | Hosts and backends that build them as literals or read their fields | Build them with `new` plus `with_*` / `as_user` (new: `PtyStartRequest::with_timeout_s`, `CreateRequest::with_snapshot_source`, `ExecRequest::without_user`), and read through the methods of the same name, such as `request.command()` |
| The integer inside `PtyProcessId` is private | Code that writes `PtyProcessId(n)` or reads `.0` | Use `PtyProcessId::new(n)` and `.get()` |
| `UnsupportedScheme` and `EphemeralMountRemoval` have private fields | Code that reads them | Use `scheme()` on the former; build the latter only through `new` and its two constructors |
| `SandboxErrorDetails` is `#[non_exhaustive]` | Code that matches it exhaustively | Add a wildcard arm |
