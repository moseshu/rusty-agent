# R9-14：Responses compaction session

状态：已完成（`4226e50`）。

`OpenAiResponsesCompactionSession` 在显式选择时装饰本地 `Session`，调用
`POST /responses/compact` 并替换其历史。普通 Session 无需实现压缩，通用 R5
压缩继续由 `ra-context` 提供；Responses 的模式、模型限制、wire 格式和鉴权留在
`ra-model::openai::compaction`。

## 上游核对

框架契约按本地 `openai-agents-python` 的
`08e5c431eb85d243b62d904f21bc57b6db1682a1` 核对：

- [compaction session 实现](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/src/agents/memory/openai_responses_compaction_session.py)：构造参数、候选项、模式解析、延后标记、自动可见性、替换与恢复。
- [Session port](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/src/agents/memory/session.py) 与 [SQLite 原子快照](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/src/agents/memory/sqlite_session.py)。
- [runner persistence](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/src/agents/run_internal/session_persistence.py)：实际模型交换的指纹、generation、append 后压缩与 pending write 对账。
- [session 测试](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/tests/memory/test_openai_responses_compaction_session.py)、[visibility 测试](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/tests/memory/test_compaction_model_visibility.py)、[suffix 测试](https://github.com/openai/openai-agents-python/blob/08e5c431eb85d243b62d904f21bc57b6db1682a1/tests/memory/test_compaction_suffix.py)。

同时读取 Codex `44fe510ce3ee61c8ef623adcbf89b901c73ddd61` 的
[remote compaction 实现](https://github.com/openai/codex/blob/44fe510ce3ee61c8ef623adcbf89b901c73ddd61/codex-rs/core/src/compact_remote_v2_attempt.rs)
及 [history 测试](https://github.com/openai/codex/blob/44fe510ce3ee61c8ef623adcbf89b901c73ddd61/codex-rs/core/src/compact_remote_history_tests.rs)。
Codex 的 remote v2 属于编码产品的协议与历史管理机制；这里采用框架的
`responses.compact` 契约，没有把 Codex harness 元数据、产品开关或账号机制变成前置条件。

## 入口与默认值

入口位于 `ra-model` 的 `openai` feature 下。构造时传本地 Session 与显式
`OpenAiAuth`；拒绝装饰服务端托管历史的 `OpenAiConversationsSession`。
默认模型 `gpt-4.1`，默认模式 `Auto`，默认阈值为十个非用户消息、非 compaction
的候选项。模型名验证沿用上游 GPT、o 系列及 fine-tuned 名称规则。

```rust
use std::sync::Arc;
use ra_model::openai::{
    auth::OpenAiAuth,
    compaction::{OpenAiResponsesCompactionArgs, OpenAiResponsesCompactionSession},
};
use ra_session::SqliteSession;

async fn example() -> ra_core::error::Result<()> {
let backend = Arc::new(SqliteSession::open("conversation", "history.db")?);
let session = Arc::new(OpenAiResponsesCompactionSession::new(
    "conversation",
    backend,
    OpenAiAuth::new("explicit-api-key"),
)?);

// Pass session.clone() to RunRequest::with_session for automatic post-turn compaction.
let outcome = session.run_compaction_with_usage(Some(OpenAiResponsesCompactionArgs {
    force: true,
    ..Default::default()
})).await;
let (usage, result) = outcome.into_parts();
// Settle usage in the caller's ledger even when replacement failed.
result?;
let _ = usage;
Ok(())
}
```

手动 `run_compaction(Option<Args>)` 返回操作结果；需要结算计费用量的调用方使用
`run_compaction_with_usage`。参数保持上游的四个字段：`response_id`、
`compaction_mode`、`store`、`force`。builder 提供模型、模式、decision hook
及 `max_rollback_items` 的设置，预算默认无限制，显式预算须为正数。

| 模式 | 行为 |
| --- | --- |
| `Auto` | 有可用 stored response id 时使用 `previous_response_id`；缺 id、`store=false` 或记得该 id 未存储时使用 `input`。 |
| `Input` | 发送本地选中的 history。 |
| `PreviousResponseId` | 必须有 response id；缺失时即使未达到阈值也报 caller error。 |

**规划文字的契约澄清**：上游允许显式 `PreviousResponseId` 覆盖 `store=false`，
测试也要求此行为。因此实现禁止的是 **Auto 错误推断 unstored id 可用**，没有额外
禁止调用方的显式覆盖。上一轮已知 unstored 的 id 会被记住，后续未传 `store` 也不会
误用；同一 id 显式声明 `store=true` 可解除该记忆。

当前 Responses 适配器在无 continuation 时默认 `store=false`，自动压缩复用这个
实际请求默认值，因此默认走 `input`。手动模式解析则保持上游参数语义。

## 生命周期、可见性与恢复

runner 在模型输入处理与过滤之后，通过可选 `SessionCompaction` 端口记录成功
request 的有序指纹，再追加成功 response 的指纹、id 和 retention。
Responses 实现直接复用自己的 wire 输入归一化与 lowering，未配对或去重后被裁剪
的调用不会被当成模型已看见的记录。重复出现次数及顺序均参与匹配。

取证是尽力而为的，与上游 `digest_input_item` 返回 `None` 时省略该项一致：请求或响应项无法指纹化
时只省略证据并记录 warning，被省略的项无法匹配。旧 prefix 无法发送不再阻止 native 后端选择
可见 suffix；legacy 后端仍须完整覆盖。候选与 defer 判定只规范化历史，并保留不可发送项供 hook
判断；严格 Responses lowering 在最终 suffix 获批后执行。显式手动压缩若要发送不可转换的历史
仍返回错误。

本地 tool output / handoff output 写入 Session 后只设置 defer 标记，不立刻调用
compact。后续工具轮保留标记，直到最新成功模型交换没有新的本地工具结果时执行
force；只有成功替换才清除标记。force 绕过最初阈值，但不绕过 generation、可见性
匹配、部分历史边界和实际 snapshot 的 decision hook 复核。

自动压缩只替换最新成功模型交换覆盖的历史。无原子 snapshot 的后端须完整匹配
全部历史；有 snapshot 的后端可替换匹配 suffix，保留旧 prefix。部分替换使用
`input`，显式 `PreviousResponseId` 则跳过；suffix 从用户消息开始，不能拆散工具
call/output 组。重读后内容或模式发生变化时，hook 须再次批准，force 也遵守。

SQLite snapshot 不受 session 默认读取 limit 限制，保留 raw row id 与数据。
替换在 `BEGIN IMMEDIATE` 中重新比较精确尾部，再删除及插入；外部追加、相同内容
的删除重插、原地变更都会撤销旧 suffix 的替换资格。不可读行使 snapshot 能力拒绝
接管；零长度快照请求返回 `None`。原子 suffix 不需要完整 rollback 快照，不受
`max_rollback_items` 的全历史预算限制。

手动压缩沿用上游全历史替换语义，候选/input 可遵守后端默认读取 limit，恢复快照
必须包含全量历史。没有 native snapshot 的自动路径也使用 clear/add + rollback。
rollback 在 API 返回后重新读取，防止把请求等待期间已过期的记录恢复回来。
clear 或 add 失败，包括已提交但确认丢失，都会恢复旧历史；恢复再失败时记录 warning
并保持原始错误。

wrapper 的锁覆盖快照读取、API 等待、替换与恢复。generation 在成功/不确定 mutation
后失效，interleaved run 不能沿用旧读取的所有权。开始替换后独立 task 持有锁到结算；
每个已启动的后端 mutation 都等待结束，再检查取消和执行 rollback，避免后台迟到
的 clear/add 覆盖恢复结果。取消可能因此等待后端 mutation 完成。
native replacement 同样等待原子事务结算，runner 先清除已结算 pending write，
再传播取消；旧式替换在取消后完成 rollback。

`RunState` schema 升至 8，保存 compaction exchange 与 pending append acknowledgement。
wrapper 的 generation 是实例内、进程内计数器，不写入 checkpoint（上游同样只把交换证据放进
`pending_session_write`）：反序列化恢复的 run 只能经对账读取重新取得所有权，避免不同 wrapper
实例的计数器碰巧相等而误认所有权；内存中续跑保留它。
post-write 压缩失败不会丢掉已确认追加的批次，恢复只重试压缩，不重新执行工具或
重复追加。沿用现有 terminal-unrecoverable 边界：最终答复后写入/压缩失败的终局
checkpoint 不允许作为新一段 run 继续执行。

压缩 usage 在响应输出归一化或替换失败时仍结算到 `RunState`、当前 segment 的
`RunResult::usage`、共享 spend 和 rollout `ModelUsage`。它不伪装成普通模型响应。
开始真正的 compact API 调用时记录结构化 INFO 事件，包含 `compaction.force`、
`automatic`、mode 与 item count；结算事件记录 `compaction.replaced`，不记录历史正文。

## 必要的 Rust 适配与剩余差异

- Python 使用动态方法和 live context 属性；Rust 使用带默认实现的可选 trait 端口、
  typed context/outcome 与序列化 checkpoint。普通 Session 与第三方实现继续使用默认方法。
  Responses 参数没有进入 provider-neutral core。
- Python 可以捕获取消并等待恢复；Rust future 可以直接被 drop，故 wrapper 把 mutation
  所有权交给独立 task，显式取消在结算后传播。若调用方直接 drop 整个 runner 而不使用
  `CancelScope`，历史仍会结算，但无法把丢弃的返回值写入该调用方的内存账本。
- 上游直接存储任意 Responses dict；本项目 Session 存 typed `RunItem`。复用现有
  Conversations lifter，给无 provider id 的项分配本地 UUID identity，raw provider item
  保存原响应。当前没有中立对应物的 hosted-tool 项仍报错并保留旧历史，不能宣称已支持。
- hook 的可转换记录继续使用 Responses wire；无法转换的记录保留 typed 字段，展开 input 的
  `data`、保留 `type` 与消息 `role`，移除顶层 `schema_version` / `created_by`；其他 provider
  的 compaction 则保留其 payload。这是 typed Session 对上游任意 dict 规范化的适配：候选数量、
  顺序与内容不会因为发送能力而丢失。这些回退记录仅用于 hook，不能作为 API input；最终 suffix
  变化时仍需 hook 复核，即使是 defer 后的 force。
- 用户文件复用已有中立 `FileBlock`，补充消息 `ContentBlock::File`；Responses 与 Chat
  使用已有文件 codec。Responses-only 的 URL/id filename 和 detail 留在 file 的 `openai`
  扩展信封，由适配器白名单回放，没有增加中立 provider 参数。Anthropic 尚未实现消息文件。
- 输出按上游清洗 image/file 的 source priority，移除内部 `created_by`；无 reasoning 时
  清除孤立 assistant raw id。typed message 的正常 replay 沿用已有 Responses codec，
  不将 raw assistant id 变成新的 wire 字段。
- `RunConfig` 目前没有 reasoning-item-id 策略；当前 Preserve 语义对齐，不能宣称覆盖
  上游显式省略旧 reasoning id 的配置分支。没有移植不存在的 encrypted/TTL wrapper；
  已有 store 的 snapshot/rollback 合约已覆盖，后续 wrapper 必须提供自己的快照投影。
- OpenAI 默认 client、默认 key 与账号身份机制按仓库约定不移植；使用现有显式
  `OpenAiAuth`、传输头与 provider 错误映射。`with_provider` 仅为本项目已注册别名归属的
  适配器扩展，默认仍为 `openai`。

## 验收

新增 50 项测试：compaction session 43 项、独立 trace 1 项、SQLite snapshot 5 项、
RunState checkpoint 1 项。覆盖模式默认值和显式覆盖、阈值、defer/force、内容归一化、
有序可见性、有限 suffix、hook 复核、并发 generation、native stale snapshot、
失败回滚、取消结算、工具续跑去重、acknowledged append、计费用量，以及无法转换的历史不让
run 失败。2026-10-05 补充旧 thinking prefix 被 filter 隐藏后的 native suffix 回归：普通 turn
与工具 defer/force 各覆盖 stored / unstored，断言 API 只发送获批 suffix、prefix 原样保留，
并覆盖初始 hook 拒绝、suffix hook 拒绝、force 的 hook 复核与默认 suffix 阈值。

- `cargo test --manifest-path tests/Cargo.toml -p it-core -p it-model -p it-runtime -p it-session -p it-session-sqlite --quiet`：1,997 项通过，13 项原有 ignored。
- 审核修正（generation 不入 checkpoint、取证尽力而为、不可发送旧 prefix 保留）后的定向回归：compaction session 43、
  trace 1、conversations session 44、`it-core` run_state 42、`it-session-sqlite` 26、
  `it-runtime` session_persistence 65 与 agent_as_tool 45，全部通过。
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`：通过。
- `cargo clippy -p ra-core -p ra-model -p ra-runtime -p ra-session --all-features -- -D warnings`：通过。
- `cargo check --workspace --all-features`：通过。
- 四个相关 crate 的 `--no-default-features` check：通过；`ra-model/custom_tools.rs` 在该组合仍有已有的 dead-code warnings。
- 主/测试 workspace formatting、`git diff --check`、layering、no-inline-tests、public-api：通过。
  public API 增量逐项核对后接受本机忽略目录中的基线；没有放宽全局门禁，唯一公开字段
  例外限定为上游四字段 compaction args。
