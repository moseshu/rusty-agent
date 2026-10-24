# Migration guide

Each crate documents whether its surface is `Stable`, `Evolving`, or `Internal`. A `Stable`
surface changes incompatibly only in a new `0.x` release, and every such change is listed here
with what to do about it. Upgrade all crates together: crates from two tags are two copies of
`ra-core`, and their types do not interoperate.

## Unreleased

### Inherited role tools and explicit restrictions

`ReadOnlySpecialist` and `Planner` now inherit the same tools as `Main` for the selected host and
profile. Shell, patch, web, and collaboration tools follow the ordinary host/profile selection.
Roles do not replace the inherited sandbox or approval policy. `OneOffAnswer` remains tool-free.

Hosts can explicitly select `CodingHost::with_read_only_role_restrictions(true)`. This product
extension, disabled by default, withholds `apply_patch` and the entire collaboration tool group
from specialist/planner roles, including messaging, follow-up tasks, interruption, waiting and
listing agents. It retains shell and web. It removes dedicated tool
entries; shell commands remain subject to the inherited sandbox and role instructions, so this
setting is not a filesystem read-only sandbox. Tool counts, profile floors and prompt fragments
are narrowed by exactly the withheld entries. Role guidance uses the advertised tool inventory.

`Planner` ports three parts of Codex Plan mode: instructions, `update_plan` refusal, and
`request_user_input` behavior. Repository mutations are prohibited by its instructions, builds
and tests may write caches/artifacts, `update_plan` remains declared but returns Codex's Plan-mode
refusal, and root-thread questions carry `isBlocking=true` and await the response. Child-thread
questions retain the root-only refusal. Spawned Planner guidance adapts questions to the parent
agent and returns the plan and unresolved questions to that parent. The embedded upstream template
is retained; emitted guidance adapts unavailable tools, the root/child audience, and plain-text
`<proposed_plan>` markup. It does not promise special client rendering.
Required adaptation text and section boundaries are checked during prompt assembly. Missing or
ambiguous markers, reversed boundaries, or unadapted references to unavailable control tools return
a configuration error instead of silently emitting incomplete guidance. The current template's
rendered text is unchanged by these checks.

The Plan runtime's proposed-plan stream parser and separate plan items, deferred mailbox delivery,
and restrictions on automatic turns entering/leaving Plan mode are not ported. Plans remain plain
assistant text. These are recorded gaps, not a claim of complete Plan-mode parity.

ReadOnlySpecialist reuses the upstream Plan template's allowed/non-allowed operation lists,
including cache-writing builds/tests and prohibitions on formatters or code generation that change
repository files. Its role allowance is 640 estimated tokens, with a 3,360-token prefix ceiling.
Planner's allowance remains 3,072 and its ceiling 5,792; other roles retain 192 and 2,912.
Prompt hashes and the assembly baselines change; tool schemas and
`TOOL_SCHEMA_REVISION` do not.

Mode selection is bound at agent construction, rather than through Codex's mutable turn settings.
Reassemble with another role to change it. Standalone framework tool users can select
`with_mode(ModeKind::Plan)` on `UpdatePlanTool`, `TodoCapability`, or `RequestUserInputTool`;
constructors preserve Default mode. Existing `AgentControl::with_max_depth` still limits spawning
and hides collaboration tools at the limit. Without that opt-in the pinned Codex v2 path, and
Rusty's existing control, impose no depth limit; this change does not add a new depth default.

### Explicit Host skill discovery

Host disk discovery is disabled by default. Enable it with
`CodingHost::with_skill_discovery(SkillDiscoveryConfig::new())`, using the types in
`ra_coding::skills`. This discovers the workspace's `.rusty-agent/skills/` and any explicitly
selected `PluginSkillRoot` directories. Each plugin supplies its owning root and namespace;
recursive discovery is the default, with `DirectChildren` available for Agent Plugin layouts.

`with_config_rules(SkillConfigRules { entries })` applies Codex's ordered enablement rules after
product filtering, plugin qualification, and canonical-path deduplication. Each `SkillConfigRule`
contains a `SkillConfigRuleSelector::Name(full_name)` or `::Path(canonical_document_path)` and
an `enabled` boolean. Every matching rule is applied in order; later rules override earlier ones,
including across name and path selectors. Name selectors affect every matching document. Disabled
entries remain in the catalog with `enabled = false`, and are omitted from the model's catalog.
Reenabling a skill preserves `policy.allow_implicit_invocation = false` and product restrictions.

The default rules are empty. Build rules from configuration entries with
`SkillConfigRules::from_configs([SkillConfig { path, name, enabled }, ...])`, passing entries in
increasing precedence order. As Codex's `[[skills.config]]` loader does, it canonicalizes existing
paths, trims names, ignores entries with an empty name or with both or neither selectors, and lets
a repeated selector replace its earlier rule. `SkillConfig` implements serde for host configuration
files; this API does not read Codex's user/session TOML layers. Relative paths are ignored with a
warning, because there is no configuration base directory to resolve them against. Directly
constructed `SkillConfigRules` are used as given: names match exactly, and path selectors are
native `PathBuf` values in place of Codex's `PathUri`.
Rules only affect discovered Host entries; Executor and Cloud providers keep their own enablement.

`with_allowlist(["release", "acme:review"])` is an explicit product extension applied after these
rules. It selects complete, case-sensitive names after plugin qualification. No allowlist retains
all discovered Host entries, including disabled ones; an empty allowlist selects none. Allowlisting
a disabled skill retains `enabled = false`; enabling a skill cannot bypass the allowlist.
**The allowlist is a context filter, not a sandbox.** Excluded skill files remain on disk and
can still be read by `read_file` or shell commands under their existing permissions. Discovery
adds no filesystem grants and does not authorize plugin scripts, MCP servers, or hooks. External
plugin files still require an ordinary file/command tool with appropriate access.

Only metadata enters the run catalog. Host entries use file locators and ordinary file tools;
Executor and Cloud entries keep `skills.list` / `skills.read`. Allocation follows Codex's
`render_catalogs` entry point: Cloud is rendered first with a cap of `total - total / 4`, then
Executor and Host share the budget left after Cloud's actual metadata cost. If filesystem entries
are omitted and a joint probe can retain every source's entries, the Cloud cap is lowered and
both buckets are rendered again. Description truncation alone does not trigger rebalancing.
Cloud preference for matching qualified names applies only to Executor; matching Host entries
remain in the Host catalog. Source ordering, aliases and Host full descriptions are preserved.
Core surfaces retain Host catalogs while omitting the namespace tools.

Each run uses the upstream first-step allocation with no previous world-state snapshot. Rusty
does not retain the Cloud cap or its catalog fingerprint across steps/resume. Host and Executor
inventories refresh on each new run and remain fixed within that run; this is the existing Rust
run-based adaptation of Codex's step-based lifecycle.

`agents/openai.yaml` policy preserves `allow_implicit_invocation` and `products` restrictions.
Choose a restricted product with `with_restriction_product(SkillProduct::Codex)` when needed;
without a product, product-restricted entries are excluded. Invalid optional metadata is ignored.
Presentation metadata and dependency declarations are validated for parsing but do not install
UI assets or dependencies. The legacy merged `with_skill_catalog` extension remains exclusive
with Host discovery and available namespace providers.

`SkillListQuery::executor_roots` now supplies selected authority ids and canonical local paths to
providers. Unselected executor authorities are removed before prompting, listing, or package
resolution, so a direct read now reports `skill package is not available` for them. The default
tool schema, default static prompt, and schema revision are unchanged.


### Per-turn coding search fallback

`CodingHost::with_web_access` preserves the released local-function behavior on every protocol.
`with_web_access_fallback(access)` instead keeps native Responses search and selects local
`web_search`/`web_fetch` on Chat and Anthropic turns. It preserves existing `with_web_search`
settings; calling `with_web_search` afterward updates settings without removing the fallback.
Selecting legacy `with_web_access` removes both native configuration and the fallback. Selecting
fallback afterward restores default native settings if needed.

Cached remains the default. A plain `WebAccess` fallback can satisfy only Live mode without
additional search options. It does not automatically turn Cached into Live; the existing
permission-profile mode resolution still applies. Use `with_web_access_fallback_adapter` and
`WebSearchFallbackAdapter` for a backend that explicitly supports Cached, Indexed, domain filters,
location or context settings. The adapter receives `WebSearchPolicy` and returns an implementation
bound to the complete policy. Cached does not expose `web_fetch` or recommend it in result guidance.
`WebSearchTool::with_fetch_guidance(false)` supports search-only result guidance while preserving
the default web pair. Fetch domain restrictions are a
Rusty product policy, including adapter enforcement during redirects. Unsupported policies omit
the local candidates while retaining available native Responses search.

`CodingWebSearch::enabled` is the common capability gate (default true); `supported` continues to
mean hosted endpoint support. `mode_for_turn` now returns only the effective permission-resolved
mode, independent of either support gate. `prefer_standalone` explicitly selects an available
fallback even on Responses turns. An unrelated same-name dynamic/MCP function does not suppress
hosted search. Hosted history replay into Chat remains unsupported.

`ToolAvailability::DynamicForProtocol` and `Tool::is_enabled_for_protocol` are opt-in extensions.
Ordinary callbacks retain their existing order before model resolution. The new callback runs
once after model resolution, before final collision handling, discovery caching and budgeting.
Agents without protocol-dependent candidates retain the original early collision/discovery/budget
error order. For an agent with any such candidate, the entire frozen inventory is finalized after
model resolution, including errors involving only ordinary tools. A model-resolution failure can
therefore precede an ordinary-tool collision or budget error in that explicitly selected path.
Approval recovery resolves an already-issued local candidate by its original lookup key and
recipient binding without reselecting its protocol shape. Protocol-dependent candidates do not
participate in the reconstructed discovery surface, so they cannot shadow an approved MCP tool.
The coding fallback candidates carry the adapter-owned binding
`{"coding_web_fallback":"web_search"}` or `{"coding_web_fallback":"web_fetch"}`. This distinguishes
their approvals from same-name resolver functions that have no recipient binding. The marker stays
in approval checkpoints; it does not change model tool definitions or replay input.
Fallback recovery also checks the current common capability gate, effective mode and domain scope
before binding the backend. If current access constraints no longer permit the issued policy, the
call fails closed. Otherwise it retains the issued policy, including its location/context guidance;
loosening current settings does not widen an approved call. This is a coding-product policy.
`HostedToolResolver::resolve_with_tools` defaults to the existing
resolver and optionally exposes the actual enabled local handlers to product adapters.

Protocol-selected search candidates are omitted from the stable inventory. Tool Use now permits
the tools actually declared in the request, changing the stable prefix hash. Schema revision is 11.

### Native Responses web search declarations

Use `ra_model::openai::responses::web_search::WebSearchTool::to_definition()` with
`AgentSpecBuilder::hosted_tool` to declare native search. The framework constructor preserves
`search_context_size="medium"`, optional filters and location, explicit `external_web_access=false`,
content kinds and image settings. Image search automatically includes `web_search_call.results`.
Hosted declarations participate in the real request tool table, budgets and tool-choice
reconciliation. A hosted-only agent retains `auto`/`required` before its first request;
`ToolChoice::Tool("web_search".into())` produces the native built-in selector, as in the SDK.
That selector is reserved even when an ordinary function has the same name.

`AgentSpec::tools()` remains the local-handler projection for compatibility;
`declared_tools()` exposes the ordered local/hosted union. `clear_tools()` clears both kinds,
while `clear_hosted_tools()` removes only hosted declarations. Runtime handler replacement
preserves interleaving. `ToolSurface::tools()`/`into_tools()` likewise project local handlers;
use `hosted_tools()` and `model_definitions()` when inspecting the complete advertised table.

Coding hosts now assemble native Responses search by default. `with_web_search(CodingWebSearch)`
selects disabled, cached (default), live or indexed mode, configured context/domains/location and
explicit model/endpoint capabilities. Codex's per-turn permission-profile resolution is preserved:
unrestricted execution prefers permitted live search, and endpoints without external access fall
back to cached search or disable it. This does not add network authorization policy.
`with_web_access` explicitly selects the existing local `web_search`/`web_fetch` extension.
Native assembly does not require that backend or advertise a separate `web_fetch` function.
Each turn resolves native search against its selected protocol: coding hosts omit it for Chat
and Anthropic. Explicit framework Responses declarations still fail on those adapters.
Use the WebAccess extension to select local web functions for those protocols.
The coding prompt's stable "Available tools" inventory lists only local entries, because whether
a turn declares hosted search depends on its protocol. Its Tool Use rule permits tools actually declared in the request, which changes the stable prefix hash.

These changes are implemented and awaiting review. No release version has been selected.

### Hosted tool records and tool-choice resets

Responses now retains file search, web search, image generation, code interpreter, and
server-executed tool search items in history. Replays preserve their provider payloads except
for output-only `created_by`, which the upstream input projection removes. Unknown output
types warn and are skipped; known unsupported actions, approval requests, and program items
still fail. Input-only computer, MCP approval, local shell, and apply-patch output items also
warn and are skipped when returned as model output, matching the framework contract.
Provider-executed tool activity without a message now requests another model turn, including
a response containing only `mcp_call` (previously it ended with empty final output).

`AgentSpecBuilder::reset_tool_choice(bool)` defaults to `true`, matching the framework contract.
Reset policy belongs to the public agent and uses its cumulative run usage, including hosted
calls but excluding server tool search records. Set it to `false` to retain an explicit choice.
A reset clears every resolved choice to `None`, including `auto` and `none`; layers are unchanged.
Tool-surface reconciliation preserves declared hosted entries as well as local tools. With no
formal declarations, the existing dynamic-availability downgrade remains unchanged across
providers. Hosted tools supplied only through legacy `extra_body` remain invisible to this
neutral inventory; use the formal WebSearch declaration to avoid losing `auto`/`required`.

The generic context estimator still counts image-generation base64 results as text. Large image
results can therefore inflate token estimates; porting the adapter's fixed image estimate remains
follow-up work.

These changes are implemented and awaiting review; no release version has been selected.

## 0.3.0

This section lists the incompatible changes from 0.2.x.

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

`CodingHost::build_run_config()` explicitly selects unlimited turns, following Codex's coding
loop, and keeps function-tool concurrency at 8. To retain the previous product turn ceiling,
apply `with_max_turns(32)` to that config. Other hosts keep the framework defaults unless they
select their own limits.

RunState schema 14 saves the turn limit, including explicitly unlimited runs. A resumed run
uses its saved limit and spent turns even if the new request supplies a different limit.
Tokens and deadlines remain configuration supplied on each continuation.

Checkpoints written before schema 14 carry no turn limit, so they resume under the limit the
request supplies, which is then saved. With the new default of 10, an older paused run that
has already used 10 or more turns fails with the turn-limit error as soon as it resumes. To
resume such a run under its original default, configure `with_max_turns(32)` for that
continuation. In coding products, a checkpoint that predates schema 14 instead adopts the
host's new unlimited policy. Checkpoints that already save a finite limit retain it on resume;
the new host policy does not rewrite an in-progress run's saved limit.

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

### Contextual input contributions and skill mentions

`Capability::contribute_turn_input` is an optional runtime extension for contextual messages;
SDK dynamic/system instructions keep their existing behavior. The runner supplies only newly
accepted input, excluding session history and input dropped by a session callback. Blocking input
guardrails run first. Returned messages enter authoritative history atomically after all installed
contributors succeed; they reach Session persistence and rollout recording as ordinary items.

RunState schema 16 adds `pending_input_contributions`, defaulting to an empty list for old states.
It retains accepted input across a failed or cancelled contribution and is cleared after commit.
Errors at this new preparation boundary carry a checkpoint. Older runtimes preserve the field as
unknown data but cannot finish the pending contribution before inference. Existing model-error
checkpoint rules are unchanged.

Explicitly configured Codex skill catalogs now load `$name` or `[$name](path)` selections before
inference. Extension main prompts are bounded to 8,000 UTF-8 bytes, names to 256 and locators to
1,024; full resource metadata and `skills.read` contents remain intact. Core Host skills retain
complete documents and locators, while Agent Plugin documents use the 8,000-byte bound. Hidden
skills remain explicitly invocable; disabled skills do not. The merged `skill` compatibility
extension and sandbox `load_skill` retain their separate contracts.

Selected `<skill>` user-role fragments remain in model history but are hidden from client thread
items and excluded from stored/user-fork turn boundaries. Marker recognition also handles older
records without annotations. Core Host text links expand `~` and require absolute native paths;
`file://` and relative text links are rejected. Only structured Skill inputs resolve relative paths
against the process cwd.

`SkillCatalogPromptConfig::with_required_skills(environment_id, names)` opts into source-scoped
prerequisites. It checks enabled discovery entries bound to that environment before each model
call, without reading their bodies. Core surfaces retain these checks even when namespace tools
are withheld. Reconfigure the selected requirements on the next run or checkpoint resume to
remove an environment requirement; no environment manager or OpenAI account backend is added.

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

The `ApplyPatchResult::output` constructor deprecated in 0.2.2 is removed. The `output_text`
reader is renamed `output`, matching the field name in the OpenAI Agents SDK. The optional
status and output values, their defaults, and the `with_output` / `with_status` builders are
unchanged.

| 0.2.x | 0.3.0 |
| --- | --- |
| `ApplyPatchResult::output(text)` | `ApplyPatchResult::default().with_output(text)` |
| `result.output_text()` | `result.output()` |

Use `with_output` to construct a result; it is also available in 0.2.2.

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
