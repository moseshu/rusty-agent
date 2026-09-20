# Rusty Agent Framework Development Plan

本文档追踪 `rusty-agent` 的开发进度。目标是**两件事，不是一件**：

1. **一套通用 agent 框架**——第三方能用它实现 ReAct、graph engineering、plan-and-execute、multi-agent 四类智能体，扩展面与稳定性承诺见[框架契约与扩展面](#框架契约与扩展面)；
2. **用这套框架写出一个 Codex / Claude Code 级的编码 agent**（`ra-coding`）——它是**参考产品**，作用是把框架抽象逼到正确形状，同时也是本项目全部实证基线（附录 A）的来源。

> **两个目标会互相拉扯，判据只有一条**：凡是「换成法律文书 / 数据分析 agent 就要改」的，是产品内容；凡是「不用改但每个产品都要再写一遍」的，是可复用件；两者都不是的，才是内核。工具数 15、schema ≤20 KB、output ≤1.5k token 这类数字**全部属于第一类**——它们是 `ra-coding` 的产品指标，不是框架上限。

架构依据：[`Agent_Framework_Analysis_ClaudeAgentSDK_vs_OpenAIAgents.md`](Agent_Framework_Analysis_ClaudeAgentSDK_vs_OpenAIAgents.md)
经验依据：**两处可读源码**——`/Users/moses/workspace/custom-app/openai-agents-python`（框架契约）与 `/Users/moses/workspace/custom-app/codex`（产品机制）。**2026-09-09 起不再引用 AgentForge 作为设计论据**：R7 的审计证明，以它的治理思路为前提长出过一整套无收益证据的内建纪律家族。凡曾挂在它名下的第三方实测数字（缓存命中率、工具计数等），要用必须先回到上面两处源码或第一手日志重新核实。
配套文档：[项目结构与模块划分](Rusty_Agent_Project_Structure.md) · [代码地图](Code_Map.md)（任务号 ↔ 代码位置 ↔ 实现状态） · [模型协议层设计](Model_Protocol_Design_Responses_and_Chat.md) · [取消契约](Cancellation_Contract.md)

> **2026-08-11 验证状态裁决**：不实现 `VerificationLedger`、`TaskLedgerProjection` 或产品级 `ReadRevisionSnapshot` 表。当前 run 的 `Session` / 工具事件历史已经是「改了什么、跑了什么、结果如何」的唯一事实来源；模型上下文、final 成型和离线 eval 都从这条历史按需投影，不能再复制出一份验证真相。AgentForge 的相关机制未证明默认收益且成本 gate 未过，Codex 可见源码也未出现等价账本。改后验证保留为 prompt 提醒与 final 诚实披露，不成为 runtime 硬 gate 或自动续跑理由；只有未来 A/B 同时证明降低 false completion 且不增加成本时，才可重新立项评估。

---

### 2026-08-11 通用 `ra-core` 契约补全映射

`ra-core` 的目标仍是 ReAct、图编排和各产品 profile 共同消费的协议内核；编码 Agent 只是第一个宿主，不反向定义内核。对照本机 `openai-agents-python` 的 `Agent` / `RunContextWrapper` / `Session` / `AgentOutputSchemaBase` / `Handoff`，以下不是新 crate 或新的顶层架构，而是补入已有里程碑的契约责任：

| 通用缺口 | 落入任务 | 边界 |
| --- | --- | --- |
| `RunContext` / `ToolContext` 与 `RunId` | **R3-9a**，再接最小 R6-6a identity slice，二者均在 R8-0 之前 | 宿主 live context 不进模型也不序列化；`RunState` 是可恢复的框架状态，usage / approval / `RunId` 的所有权在它那边，context 只给读视图。`ToolContext` 是既有 `ToolInvocation` 的**直接改名与演进**，不是新类型或 wrapper，并与 R12-B 的 `ToolServices` 一次改完。`CodingHost` 只能实现这个契约，不能成为它的定义者。 |
| 结构化输出的 schema、解析与验证 | R1-16 | `ModelOutputSchema` 仅是 provider lowering；`OutputSchema` / `OutputValue` 才是跨 runner、图边和产品的语义契约。 |
| 动态 instruction | R4-11 | 依赖 R3-9a 的通用 context，只产生模型可见 prompt 投影。 |
| 预算、权限、guard / hook | R3-8、R6、R7、R3-9 | 值类型与 callback contract 在 core，执行、UI、存储和具体策略留给 runtime / host。 |
| 最小 Session port 与会话身份 | R9-2a，执行顺序在 R5-3 / R6-6 之前 | 本地权威历史身份叫 `SessionId`；exec/PTY 叫 `ExecSessionId`；provider 管理的远端 conversation 另叫 `ProviderConversationId`。port 收发 `RunItem`。`SessionStore`、SQLite、JSONL、archive、fork 和镜像是它的**后端与扩展**，不是它的上位接口。 |
| `HandoffSpec` 声明 | R12-1 | 线性控制转移的声明是通用组合能力；**运行行为已由 R12-8 在 `ra-runtime` 独立完成**（2026-09-09 修正，原文写的「属 R17-8」已解绑）；图里的边表达仍属 R17-8。 |

`Capability`、Memory、MCP、Sandbox、Graph/Node/Edge/Scheduler 不是本轮 `ra-core` 缺口：分别留在 R10、R11、R8、R17，以真实消费者验证后再决定是否上移。特别是不得让 `RunContext` 与既有 `ToolRuntimeContext` 并存为两个竞争接口；R3-9a 必须演进后者为前者的工具视图或取代它。**这条要有机械门禁而不只是一句约定**——`ToolRuntimeContext` 从公开 API 消失；应用上下文只能经 `RunContext::app_context<T>()` 这一处读取；`WorkStateHandle::as_any()` 是另一条任务态端口，不计入也不得冒充应用上下文。这样避免用脆弱的全仓 `as_any` 字符串计数替代真正的 API 约束。同一条对低一层同样成立：`ToolContext` 不得与已落地的 `ToolInvocation` 并列成两个都带 `CallId` + 参数的类型（见 R3-9a ①）。

---

## 状态说明

| 状态 | 含义 |
| --- | --- |
| `TODO` | 尚未开始 |
| `SCAFFOLDED` | 目录、接口或占位测试已建且可编译，但尚未有行为实现或有效断言 |
| `DOING` | 开发中 |
| `BLOCKED` | 被依赖、设计或外部条件阻塞 |
| `DONE` | 已完成并通过基础验证 |
| `DEFERRED` | 暂缓，不影响当前阶段目标 |

---

## 总体架构

```text
              ra-cli                 Desktop / IDE / SDK 宿主
                 |                              |
                 |                        ra-protocol
                 |                 双向控制协议 · 审批 · 事件订阅
                 |                              |
                 └──────────────┬───────────────┘
                                |
   ┌───────────────┬──────────────────┬──────────────────┐
   │ ra-coding     │ ra-assistant     │ 第三方产品 crate   │ ← 产品内容
   │ 编码参考产品    │ 通用助手参考产品   │ 法律 / 客服 / …    │   换业务就换掉
   │ 写为主·单循环   │ 只读为主·图编排    │                  │
   └───────────────┴────────┬─────────┴──────────────────┘
                            |
       ┌────────────────────┴────────────────────────┐
       │  可复用件层（换业务不用改，但每个产品都要用）      │
       │  ra-flow（图 / 计划 / 多 agent 编排）           │
       │  ra-tools（通用工具入口） · ra-patch（V4A）      │
       └────────────────────┬────────────────────────┘
                            |
       ┌────────────────────┴────────────────────────┐
       │  ra-runtime（通用 loop 内核 = ReAct 本体）      │
       │  NextStep 状态机 · turn 结算 · FinishReason    │
       │  批量工具执行 · guard/hook · 中断恢复            │
       └────────────────────┬────────────────────────┘
                            |
       ┌────────────────────┴────────────────────────┐
       │  ra-core（公共类型与契约 · 扩展面的定义处）       │
       │  RunItem · ModelRequest · Tool · Capability   │
       │  Permission · RunState · WorkState · Memory   │
       └─────────────────────────────────────────────┘

       通用服务层：ra-model · ra-prompt · ra-context · ra-session · ra-exec · ra-mcp
       支撑工具：ra-macros · ra-eval · xtask
```

> **三层不是两层。** 中间那层（可复用件）是这次补上的：图引擎、通用工具、补丁解析器都不是业务内容（换业务不用改），但塞进内核会让内核知道「文件」「图」「计划」这些不该知道的概念。判据见[框架契约与扩展面](#框架契约与扩展面)。
>
> **两个参考产品是刻意做成对照组的。** 单靠 `ra-coding` 只能证明「框架跑得动编码 agent」；再加一个**特征相反**的 `ra-assistant`（只读为主 / 图编排 / 依赖记忆检索 / 无编辑纪律），才能证明抽象是通用的。判据很直接：**任何一处需要为其中一个产品开后门，就说明那个抽象错了**。详见 [R18](#r18-通用助手参考产品ra-assistant)。

### Crate 划分

当前主 workspace 已建 **15 个 crate**（`ra-tools` 随 R2-12 落地），目标形态 **17 个**（还缺 `ra-flow` / `ra-assistant`，见 R17 / R18）；`apps/desktop` 后置，不计入 Cargo workspace。

**层**列是硬约束：`内核` 不得依赖 `可复用件` 与 `产品`，`可复用件` 不得依赖 `产品`，由 `cargo tree` 门禁校验（R0-6）。

| crate | 层 | 职责 | 依赖 |
| --- | --- | --- | --- |
| `ra-core` | 内核 | 公共类型与契约：RunItem、ModelRequest、Tool、Capability、Guard、Permission、RunState、**WorkState、MemoryStore** | — |
| `ra-macros` | 内核 | `#[derive(ToolInput)]` / `#[tool]` 过程宏 | proc-macro |
| `ra-model` | 服务 | provider 实现（OpenAI Responses / Chat、Anthropic Messages、OpenAI-compatible）、流式、重试、usage | ra-core |
| `ra-prompt` | 服务 | PromptSection 装配、稳定前缀、缓存计划、delta reminder、prompt dump | ra-core |
| `ra-context` | 服务 | 上下文预算、compaction、老结果淘汰、archive、oversized preflight | ra-core |
| `ra-runtime` | 内核 | 通用 loop 内核：NextStep、turn 结算、工具分派、guard/hook、审批中断、子 agent 机制 | ra-core |
| `ra-exec` | 服务 | 执行隔离：进程、PTY、后台 job、宿主基线 unix_local 与 seatbelt / bwrap+seccomp 围栏、Docker 工作区 session、manifest / snapshot | ra-core |
| `ra-session` | 服务 | SessionStore trait、SQLite/JSONL 本地实现、镜像、摘要、fork、resume、file checkpoint | ra-core |
| `ra-mcp` | 服务 | MCP client（stdio/SSE/HTTP）+ 进程内工具服务器 | ra-core |
| `ra-protocol` | 服务 | 控制协议帧、stdio/WebSocket transport、app-server | ra-core, ra-session |
| `ra-eval` | 服务 | eval / replay / trace 断言 / 回归 fixture / 成本与纪律报告 | ra-core, ra-runtime, **ra-model**, ra-protocol |
| **`ra-tools`**（新增，R2-12） | 可复用件 | 与业务无关的通用工具入口：exec_command / write_stdin / read_file / grep / glob / view_image / web_search / web_fetch / ask_user / update_plan / skill / tool_search / **apply_patch** / `agent.*` / `mcp.*`。**等于 R2-8 的 15 个 advertise 入口全集** | ra-core, ra-exec, ra-mcp, ra-patch |
| **`ra-flow`**（新增，R17） | 可复用件 | 编排与图引擎：WorkState reducer、Node/Edge/Scheduler、图 checkpoint、plan-execute 与 multi-agent 预置拓扑 | ra-core, ra-runtime |
| `ra-patch` | 可复用件 | V4A apply_patch 解析、fuzz 匹配、应用与 diff 渲染 | — |
| `ra-coding` | **参考产品 A** | 用框架写出的编码 agent：提示词内容、编码纪律、危险动作事实、改后验证提醒与 final 诚实披露、coding profile。**不拥有任何工具**——含 `apply_patch` 在内的每个入口都来自 `ra-tools`，这个 crate 只决定装哪些、各自拿到什么 capability、提示词怎么讲 | ra-core, ra-runtime, ra-tools, ra-prompt, ra-patch |
| **`ra-assistant`**（新增，R18） | **参考产品 B** | 通用助手 agent（研究 / 问答 / 分析）：只读 profile、检索与引用纪律、报告成型；**`ra-flow` 与 `MemoryStore` 的第一个真实消费者**。与 A 构成对照组 | ra-core, ra-runtime, ra-tools, ra-flow, ra-prompt |
| `ra-cli` | 二进制 | 命令行入口、`prompt dump`、REPL、doctor | ra-coding, ra-protocol, ra-eval |
| `apps/desktop` | 二进制 | 桌面前端（后置） | ra-protocol |

> **两个参考产品都不是框架的一部分。** 它们存在的理由是「抽象没有真实消费者就是错的」——`Tool` / `Guard` / `Capability` / `PromptSection` / `Node` / `MemoryStore` 的形状，只有被真实产品用过才知道对不对。一个消费者会把抽象拟合到那一个业务上，**两个特征相反的消费者才能拉出正确的边界**。
>
> 配套硬规则：**产品 crate 之间零依赖（`ra-coding` ↮ `ra-assistant`），任何 crate 不得 `use ra_coding::*` / `use ra_assistant::*`；某个类型一旦两个产品都需要，必须先上移到 `ra-tools` / `ra-flow` / `ra-core`。上移是重构，不是复制。** 这条由 `cargo tree` 门禁（R0-6）执行。

### 2026-08-11 Codex 源码复核：结论、边界与六项收口

本节的证据范围是本机 `/Users/moses/workspace/custom-app/codex`、提交 `070a26a1f0` 的**可见客户端源码**。它说明该版本如何组织客户端运行时；不据此推断线上闭源服务、模型训练或所有产品版本的实现。下表刻意分开「源码事实」与「Rusty 的设计裁决」：前者可随版本失效，后者才是本项目要维护的契约。

| 观察维度 | Codex 源码可见事实 | Rusty 裁决 |
| --- | --- | --- |
| 主循环 / 计划 | `core/src/session/turn.rs` 的单 session `run_turn` 是「采样 → 工具 → 结果回写 → 再采样」循环；每步以 `StepContext` 把 request view、可见工具与执行绑定。未见一个必须先生成长计划的独立 Planner | 默认仍是单主 ReAct；计划是产品策略或 `ra-flow` 拓扑，不成为每个 run 的强制前置阶段。先观察、再做 2–3 步近景计划；只有任务确实跨节点时才进入图编排 |
| 反思 / 失败 | 未见独立 Reflector 节点；`apply_patch` 等工具的失败回到下一次模型输入。**但不能误说全部为强类型 JSON**：不少错误仍以面向模型的文本呈现 | 不做「失败后先写长篇自我批评」的必经节点；工具失败应保留机器可读记录和简短模型投影，让下一轮基于新证据修正。R3-6c 防的是无信息重复，不是禁止模型解释错误 |
| 上下文 | `context_manager/history.rs` 负责历史规范化、截断和投影；shell 输出在 `tools/context.rs` 按 token 截断；`compact.rs` 可做摘要压缩 | Session/archive 保存权威材料，模型只看预算内投影。二层输出是「摘要 + 引用」而非复制两份历史；压缩、归档、预检由 R5 收口 |
| 工具并发 | `tools/parallel.rs` 以工具声明的 `supports_parallel` 配合一个 `RwLock<()>`：Parallel 拿读锁，Exclusive 拿写锁 | R3-4b 的 `Exclusive \| Parallel` 是正确 v1；它不是资源级调度。后续增加 effect/resource claim，但不急于做文件级锁或解析 shell 命令语义 |
| 多 Agent | V2 有 root-scoped control plane、registry、execution/residency；默认 `ExplicitRequestOnly`，高推理档才可 proactive。worker 并非只读，源码 prompt 要求写集合清晰且互斥 | 不采纳「一律单 Agent」或「一律只读 worker」这两个极端。默认 ExplicitOnly；只有子任务独立、收益可量化、写集合/lease 可证明互斥、父预算足够时才允许委派 |
| 状态与回滚 | 存在显式运行状态、历史回退与会话 materialize；未见「每一个磁盘副作用自动事务回滚」 | `RunState` / session 的回放、对账和 rollback 与 workspace 副作用分开。文件 checkpoint、Git/worktree snapshot、lease 清理由 `ra-session` / `ra-exec` 提供，不能把它们伪装成 prompt 历史功能 |

**反向核验（已于二次复核修正）**：初版这一段写成"全仓 `cache_key` 只出现在审批缓存""没有 no-progress / 重复失败熔断或等价机制"，**两句都不成立**——初版的 grep 只扫了 `core/src/tools/` 就推广成了"全仓"。准确表述如下：

- **缓存**：在 `core/src` 的生产 Rust 源码中有 8 个模块出现 `cache_key`，包括 `client.rs` 的 provider prompt cache key、`session/mcp_runtime.rs` 的 MCP tool catalog / codex-apps 工具缓存、`connectors.rs`、`guardian/review_session.rs` 与 `tools/approvals.rs` 的审批缓存；测试与快照另有命中。Codex 缓存的是**元数据与协议侧对象**（提示前缀、工具目录、审批决定），**未发现按 `(tool, normalized_params, env)` 复用完整工具输出的通用 memoization**。
- **熔断**：`core/src/guardian/mod.rs` 有 `GuardianRejectionCircuitBreaker`（按 turn 记 `consecutive_denials` + 定长窗口 `recent_denials`，任一超阈值 `InterruptTurn`，`record_non_denial` 清零连续计数）。**未发现面向工具无进展 / 重复失败的通用熔断器**——它熔的是安全评审的连续拒绝。R3-6c 仍是增量；可借鉴的是连续/窗口计数与重置形式，不能直接复用其按 turn/安全拒绝分桶的语义，见该任务。
- **Claude Code**：本次核验没有它的源码基线，凡引用必须另附可核验来源，不得凭印象断言。

文档任何位置都不得把这两项增量表述为「对齐 Codex」，也不得把"Codex 没做"当成"不该做"的论据。

**一处措辞精度**：上表多 Agent 行的"写集合互斥"是 `spawn_agent` **prompt 文本**的要求（`disjoint write set` / `edit files directly in its forked workspace`），**不是运行时锁**。Rusty 把它升级为 lease / worktree 硬约束，这是机制层面的真实超越，不是措辞差异——写清楚才不会在 R12 落地时误以为"抄一段提示词就够了"。

#### 分层裁决：不增加顶层层级或新 crate

现有「`ra-core` / `ra-runtime` + 通用服务 / 可复用件 / 产品」的分层足够，**不应为了“超级智能体”再增加第四个顶层 layer，也不新建 `ra-control`、`ra-artifact` crate**。需要扩充的是已有 `ra-runtime` 内部的控制面子域：先由已规划的 `ra-runtime::agent` 承载；待 registry、lifecycle、admission 三个稳定子概念都落地后，再演进为 `agent.rs + agent/`。这符合本项目“出现 3 个以上稳定子概念才拆目录”的规则，也避免 R12-B 的设计出现两个竞争控制面。

权威边界固定如下：`ra-core` 只持有可序列化值对象和 port trait；`ra-runtime` 持有 live `RunState`、准入和状态迁移；`ra-session` 是可恢复记录/归档的权威实现；`ra-context` 只负责模型投影、压缩与 archive 引用；`ra-exec` 拥有 workspace/process 资源；`ra-flow` 只拥有拓扑、join 与调度；产品 crate 只拥有角色、提示词和策略。`Session`、`RunState`、`WorkState` 各自回答不同问题，禁止互相冒充：会话及其工具事件记录事实（包括编辑和验证结果），运行态支持暂停恢复，任务态跨节点协作。final 或 eval 若需验证摘要，必须由会话事实按需导出，不能持有第二份权威状态。

#### 六项收口动作（接入已有路线图）

| # | 动作 | 落点与任务映射 | 必守边界 |
| ---: | --- | --- | --- |
| 1 | 固定权威边界 | R3-13、R6-6、R9-12、R15-1 | 不用自然语言历史推断运行态；编辑/验证事实只写入 Session 工具事件，不让 `WorkState` 复制进每个 `RunState`，也不新建验证状态 |
| 2 | 结构化失败记录 + 无进展熔断 | R3-6 / **R3-6c** | 只有「重复失败且没有新增证据/进展」才熔断；返回短失败观察，不创建 Reflector 长文 |
| 3 | 工具输出两层化 | R5-1、R5-5 | prompt 只有 `ModelExcerpt`；完整输出进 session/archive，靠稳定 `ArtifactRef` 按需定位，不能反复复制正文。省 token 优先走**批内合并**（需工具显式声明 deterministic + read-only + coalescible）；文件是否需要重读由模型结合上下文与工具结果决定，不建文件版本表；工具结果 memoization 暂缓不否决 |
| 4 | 从工具级并发升级到资源/effect 准入 | R3-4b / **R3-4d**、R8-11a、R12-5 | 先保守默认 Exclusive；资源 claim 是 Rusty 的增强，不误称 Codex 已实现细粒度锁；`ResourceId` 定义在 `ra-core`，`ra-runtime` 依赖面不得扩大 |
| 5 | 多 Agent 显式准入 | R12-A、R12-B、R12-5/6/7 | 默认 ExplicitOnly；允许受 lease 保护的 writer，不允许平级 agent 任意转派或静默共享可写目录。**准入条件分两栏**：预算/深度/权限/lease 不相交/产物 schema/生命周期可恢复是运行时硬判定；"收益是否大于开销""子任务是否够独立"只能进 prompt，由 ROI 指标事后校准 |
| 6 | 把效率指标前移 | **R3-8b**、R14-5 | R3/R4 记录最小数值 trace；R14 再聚合、建基线和设置 CI gate，不记录 prompt 或工具入参。工具耗时拆「等准入 / 真执行」两段 |
| — | 控制面状态在压缩后仍然成立 | **R5-3b** ↔ R3-6c | 压缩只改模型可见投影，不动 runtime 权威计数器；这是六项动作能长期成立的前提，不是第七项动作 |

#### 两条明确否决 + 一条暂缓（记录理由，防止后续重新提出或误当定论）

| 被否决 / 暂缓的设计 | 理由 |
| --- | --- |
| **`ControlDecision{Explore/Act/Verify/Fallback/Parallel/Terminate}` 六态自适应决策枚举** | 拆开看没有一态需要新类型：`Terminate` 已由 `NextStep::Finish` 收口，`Parallel` 由资源准入收口，`Fallback` 由 R3-6c 熔断收口，`Explore/Act/Verify` 没有任何运行时语义。加它等于造出第二个控制流枚举，与 R3「`match next_step` 无 `_ =>`」要守的"控制流状态唯一"直接冲突，也就是本节警告的「两个竞争控制面」。它还违反本计划的硬规则「抽象没有真实消费者就是错的」：调对它需要 R14 才有的 eval 基线，在基线之前它只是用信息更少的策略去覆盖模型判断。**阶段引导属于提示词与 profile，不属于内核类型**——Codex 的做法正是把这段话写进 `spawn_agent` 描述而不是状态机 |
| **只读工具确定性结果 memoization `Hash(ToolID + NormalizedParams + EnvFingerprint)`**<br>**状态：暂缓，不是否决（二次复核撤回原否决）** | 原否决的第一条理由**是错的**：~~"与 R3-6c 互相破坏"~~——Rusty 的调用轨迹在**执行之前**已记录本轮 attempt，缓存命中照样计入 `repeat_streak`；而无进展判定看的是 evidence fingerprint，命中缓存返回同一份证据、同一个指纹，熔断条件**反而恰好成立**。<br>**仍成立的理由**：编码 run 里精确重复的 `(tool, params, env)` **命中率未知**，并且运行环境指纹很难在不建立文件版本表的前提下可靠定义；没有数据就不该承担失效复杂度。<br>**还需写死的桥接契约**：cache hit 通常是成功结果，不会自动产生 `ToolFailureRecord`；若启用 memoization，runtime 必须在熔断入口提交带 input/evidence fingerprint 的 `new_evidence = false` 无进展 observation。<br>**裁决**：只优先做收口动作 3 的批内合并；memoization 待 R3-8b 产出命中率与正确性数据后再定。**若要做，cache hit 必须以 `new_evidence = false` 的无进展 observation 参与 R3-6c 判定**。不写"永不实现"** |
| **把「收益是否大于开销」写进多 Agent 准入代码** | spawn 时不可机器判定。混进准入清单会诱导实现者去写一个假的收益估算器，而它的输入（子任务实际耗时、并行度）要等任务跑完才存在。正确位置是 spawn 工具的描述文本 + R3-8b/R14-5 的 ROI 指标事后校准 |

### 技术选型

| 领域 | 选型 | 说明 |
| --- | --- | --- |
| 异步运行时 | `tokio` (full) | 取消传播用 `CancellationToken` |
| HTTP / SSE | `reqwest` + `eventsource-stream` | 参考 `/Users/moses/workspace/custom-app/rust-llm` 已验证的依赖组合 |
| 序列化 | `serde` + `serde_json`；枚举用 `#[serde(tag="type")]` | RunState 靠它做版本化 |
| JSON Schema | `schemars` + 自研 `#[derive(ToolInput)]` 过程宏 | 对齐 openai `function_schema.py` 的 docstring→description |
| 存储 | `rusqlite`（bundled）与 JSONL transcript | **两个各自完整的可选后端**，外部 store 镜像是第三种；组合与否由产品定（R9-1） |
| 错误 | `thiserror`（库）/ `anyhow`（应用边界） | 沙箱错误蓝本见 openai `sandbox/errors.py` |
| 日志 | `tracing` + `tracing-subscriber` | span 分类对齐 openai `tracing/span_data.py`，**不接 OpenAI 后端** |
| 进程 | `tokio::process` + `portable-pty` | PTY 支持 `write_stdin` / Ctrl-C |
| 测试 | `insta`（快照）+ `wiremock`（provider mock） | prompt 装配用快照测试锁住 |

---

## 框架契约与扩展面

> **本节回答一个问题：第三方拿 rusty-agent 写自己的 agent（法律文书 / 数据分析 / 客服）时，他消费什么、实现什么、什么东西保证不会被改坏。**
> crate 边界防的是内部依赖倒挂；本节防的是**对外 API 一旦发布就改不动**。这两件事都必须在 R0/R1 定死，晚了只能靠 major 版本收拾。

### 分层判据（两条，缺一不可）

| # | 提问 | 答「是」则 | 落点 |
| ---: | --- | --- | --- |
| 1 | 换成法律文书 agent，这段代码要改吗？ | 要改 = **产品内容** | `ra-coding` / `ra-assistant` / 第三方 crate |
| 2 | 不用改，但换个产品要再写一遍吗？ | 要再写 = **可复用件** | `ra-flow` / `ra-tools` / `ra-patch` |
| — | 两条都答「否」 | **内核机制** | `ra-core` / `ra-runtime` / 通用服务层 |

**判据 2 是这一版补的。** 只有判据 1 时，图引擎、通用工具、补丁解析器这类东西无处安放——它们不是内容（换业务不用改），但塞进内核会让内核知道「文件」「图」「计划」这些不该知道的概念。

**判据靠人判断会飘，所以配一个机械判据（R18）**：维护**两个特征相反的参考产品**（`ra-coding` 写为主 / 单循环，`ra-assistant` 只读为主 / 图编排）。凡是需要在框架里按产品名分支才能满足的东西，就是抽象错了——这条由 CI lint 执行，不靠自觉。

### 四种 agent 形态的分层归属

| 形态 | 内核机制（必须在框架内） | 可复用件（独立 crate） | 产品内容 |
| --- | --- | --- | --- |
| **ReAct** | **全部**——`ra-runtime` 的 loop 本身就是 ReAct，无需新模块 | — | 提示词、工具偏好 |
| **Plan-and-Execute** | `FinishReason`（R3-1b）、Runner 可重入、`WorkState` 挂载点（R3-13） | `ra-flow::plan`：计划驱动执行 + replan 回边（R17-6） | 什么算一步、planner 提示词 |
| **Graph engineering** | `WorkState` channel/reducer、`RunState` checkpoint 复用 | `ra-flow`：Node / Edge / Scheduler / GraphCheckpoint（R17-2..5） | 具体的图定义 |
| **Multi-agent** | `as_tool`、嵌套审批冒泡、取消传播、预算继承、transcript subkey（R12） | `ra-flow::topology`：supervisor / fan-out / pipeline / debate（R17-7） | 有哪些子 agent、各干什么 |

> **multi-agent 的那四件为什么进不了产品层**：① 子 agent 的审批中断要序列化进父 `RunState`；② 父 run 取消要传播到子 run 的子进程；③ 父子 usage 要合账并受父上限约束；④ 子 agent transcript 要作为 session subkey 落盘。四件都要触碰内核不变量，放产品层只剩两条路——每个产品重写一遍，或者反向 `use ra_runtime::internal::*` 破坏依赖方向。**这也是 R12 必须前移的唯一理由**（见 R12 的优先级说明）。

### 第三方的扩展面

**他实现这些（extension points）**——全是 trait，方法都带默认实现，框架加方法不破坏下游：

| trait | 所在 | 最小必需方法 | 典型场景 |
| --- | --- | --- | --- |
| `Tool` | `ra-core::tool` | `origin` / `schema` / `call` | 加自己的业务工具 |
| `Guard` | `ra-core::guard` | 至少一个时机方法 | 加自己的纪律 |
| `Capability` | `ra-core::capability` | `kind` | 打包工具 + 提示 + 采样 + 上下文变换 |
| `Model` / `ModelProvider` | `ra-core::model` | `respond` | 自研 / 私有 provider |
| `Session` | `ra-core::session` | `session_id` / `get_items` / `add_items` / `pop_item` / `clear`（五个全必需，无默认实现） | 换对话历史来源（R9-2a 的最小 port；参考内存实现在 `ra-session::memory`，不在 `ra-core`） |
| `SessionStore` | `ra-core::session` | `append` / `load` | 换持久化后端（`Session` 的后端，不是它的上位接口） |
| `SandboxBackend` | `ra-exec::sandbox` | `spawn` / `policy` | 换隔离方案 |
| `MemoryStore` | `ra-core::memory` | `list` / `read` / `search` | 长期记忆的只读检索面（R10-8）。不承诺向量或相关性分数；无 `put`/`forget`。最终引用反馈经独立 `MemoryUsageSink`，不是检索副作用。 |
| `Node` | `ra-flow` | `run` | 图节点（R17-2） |
| `OutputSchema` | `ra-core::output` | derive 即可 | 结构化输出 |
| PromptSection 提供者 | `ra-prompt` | `sections()` | 提示词内容 |

**他消费这些（Stable API）**——签名进入稳定承诺，破坏性变更走 major + 迁移说明：

`Runner::run` / `run_streamed` · `AgentSpec`(builder) · `RunConfig` · `RunResult` / `RunStream` / `RunState` · `FinishReason` · `RunItem` / `ContentBlock` · `ModelRequest` / `ModelResponse` / `StreamEvent` · `ToolRegistry` / `ToolProfile` / `ToolOrigin` · `ToolOutput` · `PermissionMode` / `PermissionDecision` · `WorkState` · `ra-tools` 的工具构造函数 · `ra-flow::Graph` builder

**他看不见这些（Internal，随时可重构）**：`ProcessedResponse` · `ToolExecutionPlan` · `SingleStepResult` · `HistoryReconciler` · `PersistenceCursor` · `InputItemNormalizer` · 各 provider 的 codec。这些是 turn 结算与协议 lowering 的中间态，一旦泄漏成公共 API，R1/R3 就再也重构不动。

### 稳定性分级（写进每个 crate 的 `lib.rs` 模块文档）

| 级别 | 承诺 | 适用 |
| --- | --- | --- |
| `Stable` | 破坏性变更走 major + 迁移说明 | 上面「消费」清单 + 全部 extension trait |
| `Evolving` | minor 可加不可删；改语义要在 CHANGELOG 显式列出 | `RunState` 字段、rollout 事件 payload、profile 名、guard id |
| `Internal` | 随时改；`pub(crate)` 或 `#[doc(hidden)]` | turn 结算中间态、provider codec、宏展开产物 |

### Rust 扩展安全七条（R0 就要定死）

| # | 规则 | 不这么做将来会怎样 |
| ---: | --- | --- |
| 1 | **对外数据枚举一律 `#[non_exhaustive]`**：`FinishReason` / `ToolOutput` / `StreamEvent` / `ContentBlock` / `Error` / `GuardOutcome`。**`NextStep` 是唯一例外，保持穷尽** | 每加一个 `ToolOutput` 变体，所有第三方代码都不能编译 |
| 2 | **公开配置结构体用 builder + `#[non_exhaustive]`**：`AgentSpec` / `RunConfig` / `ModelSettings` / `ToolOrigin` / `RequestUsage` | 字面量构造一旦流行，加字段就是破坏性变更 |
| 3 | **extension trait 的每个方法都要有默认实现**，required 集压到最小 | `Guard` 三个时机全 required → 加第四个时机时所有下游 guard 报错 |
| 4 | **用户不该实现的 trait 要 sealed**：`ApiProtocol`、`Capability` 的内部辅助 trait | 协议能力矩阵（R1-5b）无法演进，因为外部有实现者 |
| 5 | **分类标签用开放集不用闭合枚举**：`CapabilityFamily` / `ToolNamespace` / `PromptRole` 要留 `Custom(Cow<'static, str>)`。**开放集有两种形态，按「这个标签上要不要做相等判定」选**：只被读取和展示的用带 `Custom` 的枚举（`PromptRole` 即此形态）；要拿来对账的用开放 newtype（`ToolNamespace` 与 R10-1 落地的 `CapabilityFamily`）——枚举形态下 `Custom("shell")` 与 `Shell` 是两个不相等的值指同一个能力，第三方按最自然的写法声明依赖就永远匹配不上已装的那个 | 第三方加不了自己的能力族与角色，只能 fork 框架 |
| 6 | **可序列化结构必须 `schema_version` + 新字段 `#[serde(default)]`**，且未知字段策略在 R0 定死（保留并原样回写，不报错） | `RunState` / `WorkState` / rollout 行跨版本读不了，resume 直接失败 |
| 7 | **公共 API diff 进 CI 门禁**（`cargo public-api`）：未标注的破坏性变更直接失败 | 破坏在发版之后才被下游发现 |

> **第 1 条最容易搞反。** R3 验收标准写着「`match next_step` 无 `_ =>` 兜底分支」——那是**对内**的要求，目的是新增控制流状态时编译期报错。**对外数据枚举必须反过来**：框架自己 match 的收口保持穷尽，用户会 match 的数据枚举一律 `non_exhaustive`。两者不是矛盾，是不同方向的约束。

---

## 阶段总览

| 阶段 | 目标 | 状态 | 交付物 |
| --- | --- | --- | --- |
| R0 | 工程骨架与选型 | **DONE** | crate 边界、四条横切基线、CI 门禁、独立测试 workspace 与扩展面契约全部落地；后续能力的行为测试随对应阶段实现，不预建空占位 |
| R1 | 消息模型与 Provider 抽象 | **进行中** | RunItem/ModelResponse、**`ModelSettings` 四层 resolve**、Model trait、**`ApiProtocol` 能力矩阵**、四条协议路径（OpenAI Responses / OpenAI Chat / Anthropic Messages / compat）、流式、usage 明细、归一化重试 |
| R2 | 工具体系 | **进行中** | Tool trait、ToolOrigin 身份键、schema 派生宏与**字节级稳定**、**工具面对齐单次 advertise 口径：Codex 16 / CC 24**、单 exec 收编长尾、多模态结果、**`ra-tools` 通用工具库拆分** |
| R3 | Agent Loop 内核 | **进行中** | **`AgentSpec` 不可变契约**、turn 准备顺序、NextStep 四态状态机、**`FinishReason`**、SingleStepResult、turn 结算、**commentary/final 双通道**、`Runner::run` / `run_streamed`、**`WorkState` 挂载点** |
| R4 | Prompt 装配与缓存治理 | **进行中** | 稳定前缀、尾部增量提醒、prefix hash、缓存断点、prompt dump |
| R5 | 上下文管理 | **DONE** | 工具结果预算、compaction、老结果淘汰、oversized preflight、上下文用量 API —— 批次 F 九条全部完成 |
| R6 | 权限、审批与中断恢复 | **进行中** | PermissionMode、决策类型、Interruption、RunState 序列化 + approve/reject |
| R7 | 护栏与 hook 扩展点 | **DONE** | Guardrail 两层（对 `guardrail.py`）、工具两端护栏（对 `tool_guardrails.py`）、UserHook 事件面（对 codex `hooks/`，四个决定点含 stop 拦交付）三条全部落地，见 R7-1 / R7-3 / R7-4。**框架不带任何内建 guard**——两个参考都没有，原先的内建纪律家族已于 2026-09-09 撤销 |
| R8 | 执行面 | **进行中** | exec_command/write_stdin（管道 stdin，PTY 已裁决不做）、apply_patch(V4A)、文件工具、沙箱后端、后台 job |
| R9 | 会话与持久化 | **进行中** | **双通道 rollout 事件日志**（response_item + event_msg，call_id 配对）、SessionStore trait、本地 SQLite/JSONL、镜像、摘要 fold、fork、resume、file checkpoint/rewind |
| R10 | Capability 装配层 | **进行中** | Capability trait、工具面 profile、按需装配、依赖校验；R10-1 契约、R10-2 依赖校验与装配、R10-4 工具面 profile、R10-4b capability 提示片段、R10-5 懒加载通道、R10-6 压缩 capability、R10-6b filter 链、R10-7 装配快照已落地；**R10-3 内置集 10/10 补齐**（`Todo` / `ViewImage` / `Web` / `Skills` 四族连同各自的工具一起落地，其中 `Web` / `Skills` 要宿主给后端，`ra-coding` 没有因而不装）；R10-8 记忆三层已落地。**整段只剩验收第一条的 1/4**（采样参数与上下文变换尚未在产品路径上折叠） |
| R11 | MCP、Skills 与插件 | TODO | MCP client（stdio + streamable-http，基于官方 `rmcp` crate）、进程内工具服务器、skills 白名单、plugin 发现与风险。**R11-0 是先导切片**：一条最小可用的端到端 MCP 路径，取舍见该节 |
| R12 | 子 Agent 内核机制 | TODO（**前移至 R6 之前**） | `as_tool` 子 agent、嵌套审批冒泡、取消传播、预算继承、outputFile 上下文隔离。**这四件触碰内核不变量，产品层做不了**；工具面是否 advertise 由 profile 决定，与机制无关 |
| R13 | 控制协议与 App Server | TODO | 帧协议、stdio/WS transport、动态控制（set_model/interrupt/rewind/…）、事件订阅 |
| R14 | Eval / Replay 飞轮 | TODO | 确定性 fixture、契约与结果断言、跨版本回归。**机械纪律指标已随 R7-6/7/8 撤销** |
| R15 | 输出成型与收尾披露 | TODO | Final answer 模式、由工具历史派生的验证摘要、改后提醒、bounded continuation；不建验证账本或 closeout gate |
| R16 | CLI / 桌面产品面 | DEFERRED | REPL、doctor、桌面 timeline（在能力主线之后） |
| **R17** | **编排与图引擎（`ra-flow`）** | TODO | `WorkState` 通道与 reducer、Node/Edge/Scheduler、图 checkpoint 与恢复、**plan-and-execute 与 multi-agent 预置拓扑**、handoff 落地为图的边。定位是可复用件，只消费 `Runner` 公开 API <br>**2026-09-09：整段暂缓**，不作为基础 agent / 多 agent / session 的前置条件；handoff 已退回 R12-8 |
| **R18** | **通用助手参考产品（`ra-assistant`）** | TODO | 与 `ra-coding` 特征相反的第二个参考产品：只读工具面、检索纪律、图与 plan-execute 拓扑、`MemoryStore` 消费、报告成型。**核心交付是「对照组回归」**——框架里任何按产品名分支的代码即 CI 失败 |

---

## R0 工程骨架与选型

### R0 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R0-1 | Cargo workspace 与 crate 边界 | **DONE** | 14 个 crate 的 facade 全部写明「负责 / 不负责 / 稳定性」；`ra-runtime` 只保留 `agent` / `runner` / `guard` / `tool::{profile,registry}` 为公开模块，turn 结算、派发、预算执行、熔断、hook 与审批流收为 `pub(crate)`；`ra-model` 的 request / convert / stream / SSE / error codec 和 `ra-patch::fuzz` 同样收口，`ra-coding` 模块树全部为产品内部。直接依赖不再只看层级：`layering` 增加逐 crate 白名单，拦住 `ra-core → ra-model` 这类同属框架但职责倒挂的边；同时修正 `ra-runtime` 只依赖 `ra-core`、`ra-cli` 只依赖装配层、`ra-eval` 补齐四个合法消费者。provider / MCP transport / session store / sandbox backend 均用可叠加 feature，`--no-default-features` 与 `--all-features` 已验证。29 个误公开模块从 API 基线移除；2 条契约测试（含 4 个 rustc 负向夹具）位于 `tests/it-e2e/tests/crate_boundaries.rs`。 |
| R0-2 | 错误与结果基线 | **DONE** | 落地形态：`Error` 按子系统分变体（Config / Caller / Provider / Tool / Sandbox / Session / Protocol / Budget / Guardrail / Cancelled），`Recoverability` 是**推导出来的投影**而非存储字段——因此两维不可能不一致。另有 `code()` 机器可读标识（进 trace 与 eval 归因）、`user_message()`（面向 UI，与面向日志的 `Display` 分开）、`with_source()` 链式附加。18 条断言在 `tests/it-core/tests/error_taxonomy.rs`。原始设计说明：<br>① **按子系统**（`ra-core::Error` 用 `thiserror`）：`Config` / `Provider` / `Tool` / `Sandbox` / `Session` / `Protocol` / `Budget` / `Cancelled`；<br>② **按可恢复性**（对齐 openai `exceptions.py` 的 12 个类）：`MaxTurns` / `ModelBehavior`（模型输出不合协议）/ `ModelRefusal`（拒答，触发 R1-12 回退）/ `UserError`（调用方用错 API，不可重试）/ `ToolTimeout` / `McpCancellation` / 四类 `GuardrailTripwire`。<br>每个变体带 `is_retryable()`、`user_message()`、`recoverability()`；错误对象携带 run 快照引用（对应 `RunErrorDetails`），供 R3-8 的 error handler 生成最终输出。**`#[non_exhaustive]`**（扩展安全第 1 条） |
| R0-3 | 日志与 span 分类 | **DONE** | 落地形态：`ra-core::trace` 只定**词表**——`SpanKind` 八分类（`agent` / `turn` / `generation` / `function` / `handoff` / `guardrail` / `mcp_list_tools` / `custom`，`turn` 是我们自己加的：openai 用 `response` 表示一次模型往返，但一个 turn 含重试、回退与整批工具，没这层就答不了「这一轮花了多少钱」）、`field::*` 字段名常量（全集在 `field::ALL`）、`SpanKind::required_fields()` 把「每类必带哪些字段」从文档表格变成可断言的代码。级别不由 callsite 自己拍：span 级别按「**一次正常 run 在 INFO 下读得完**」分档（骨架 INFO / 高频检查 DEBUG），错误级别由 `level_for(Recoverability)` 从 R0-2 投影（可自愈 WARN、需干预 ERROR、**取消 INFO**——否则重试前的失败全刷红，真错误被淹掉）。`SpanOutcome` 把取消单列成 `cancelled` 而非 `error`，与 R0-2 / R0-4 同一条判据。三条边界写进模块文档：① **不实现任何 subscriber、不接任何外部上报后端**（装配是 `ra-cli` 的事）；② tracing 通道面向开发者与 eval，**不是 UI 事件流**（那是 R9 rollout 的 `event_msg` 双通道）；③ **span 字段只放标识与计数，不放模型输入输出与工具入参**——体量大、含敏感数据，且 rollout 通道已有权威副本，这条让 R14-2 的「关掉敏感数据」不至于把 span 拓扑一起关掉。`usage.cached_input_tokens` 单列（缓存命中率是成本主因，折进 `input_tokens` 就再也算不出来）。23 条断言在 `tests/it-core/tests/span_taxonomy.rs`，含 record helper 的落盘验证与「未用 `Empty` 占位则 `record` 静默无效」这条 `tracing` 语义的钉子 |
| R0-4 | 取消与超时基线 | **DONE** | 落地形态：`CancelScope` 包住 `CancellationToken` 树（run → turn → tool → 子进程，子 agent 是挂在 tool 下的又一个 run），额外携带**根因**与**时限**——裸 token 只有一个 bool，归因无处安放。三条不变量：① 取消只向下传播；② **根因先到先得**且沿父链上溯查询（同样是投影而非副本），因此刻意**不设 `ParentCancelled`**——否则超时率与中断率在传播第一跳就混了；③ 取消不是失败（`Recoverability::Cancelled`）。超时侧：`Deadline` 是绝对时间点、**只能收紧不能放宽**、不可序列化；`ra-core` 不 arm 定时器，但任何检查点观察到过期都就地转成真取消，因此定时器只影响及时性不影响正确性。另有 `CancelOnDrop`（防「忘记取消」导致后代永远等下去）与 `DRAIN_GRACE`（取消后必须 drain 到终态，**直接 drop `JoinHandle` 会在 Rust 里留下正在跑的子进程**）。31 条断言在 `tests/it-core/tests/cancel_contract.rs`，六条硬规则、分层责任与反例清单在 [`Docs/Cancellation_Contract.md`](Cancellation_Contract.md)。**e2e 验收（子进程被杀、无泄漏任务）随 R3 / R12 / R17 落地** |
| R0-5 | 配置模型 | **DONE**（机制） | 落地形态：`ra-core::config` 只给**机制**，不定义任何具体配置项——那些字段依赖各阶段的能力，在各自阶段定义并用 `Layered<T>` 承载。六层优先级 `Builtin < UserFile < ProjectFile < LocalFile < Env < Explicit`（顺序对应「离这次调用有多近」），优先级同样是从变体**投影**出来的 `precedence()` 而非存储字段。<br>两个刻意设计：① **`SourceSelection::default()` 是隔离的**——框架被嵌进别人的进程时偷读 `~/.rusty-agent/config.toml` 不可接受，来源是选出来的不是发现出来的（对齐 `setting_sources`）；`ra-cli` 显式调 `all()`。`Builtin` 与 `Explicit` **关不掉**（一个是兜底，一个是本次调用的意图），因此不存在「什么都没配上」的荒谬状态。② **被排除的来源在诊断里仍然看得见**：`FieldReport` 区分 `Shadowed`（被更高层盖住）与 `Excluded`（来源未启用）——**两种「没生效」的修法完全不同**，只显示生效值会把用户困在「我明明写了配置为什么没用」里。这直接支撑 `ra doctor config` 的验收项。<br>另有 `env_key()`（`model.name` → `RA_MODEL_NAME`）与文件名约定常量。25 条断言在 `tests/it-core/tests/config_layering.rs`。**未做**：文件发现与 TOML 解析（需要 I/O，属 `ra-cli`）、具体 `Config` 字段（随各阶段落地） |
| R0-6 | CI 与质量门 | **DONE**（9/9 真执行） | 落地形态：CI 三个 job（编译期检查 / xtask 九条 / `cargo deny`），互不阻塞。<br>**`cargo xtask all` 全绿不等于 CI 全绿**：`cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 只在第一个 job 里，九条门禁一条都不覆盖它们。实测代价：clippy job 曾在 main 上连红数个提交无人发现，攒下六条错误（`ra-model` 三个函数漏了 `#[cfg(feature = "compat")]`、`ra-core` 两个 serde 谓词、`ra-session` 两个所有权移交构造器），外加四个测试文件的格式漂移。本地提交前应当跑的是这两条加 `cargo xtask all`，不是只跑后者。门禁结果是**三态** `PASS / SKIP / FAIL`——被测对象尚未存在的那条报 SKIP 并带上挡着它的任务号，在汇总里单独计数：占位输出会被当成完成，假装通过更糟（虚假的安全感），**只有 FAIL 让 CI 变红**。<br>**真执行的九条**：`layering`（读 `cargo metadata` 建内部依赖图，查**传递依赖**——只查直接依赖会漏掉 `ra-core → X → ra-coding`；外加框架 crate 的产品引用与按产品名分支扫描。产品 crate 标识符永不允许豁免；`assistant` 这类协议词汇撞名只能用真实行尾注释 `// layering-allow: assistant = <理由>` 逐 alias 豁免，空理由、未知 alias、字符串中的伪标记和未消费标记全部失败，例外数进入每次汇总。六类常规分支、三个例外绕过以及字符字面量导致的词法错位均有永久回归测试；产品清单从层级表里的 `Layer::Product` 推导，不维护第二份来源。层级表是白名单，**新建未登记的 crate 直接失败**）、`no-inline-tests`、`public-api`、`feature-matrix`、`test`，外加落盘对账的 `schema-stability`（R2-9）与 `prompt-dump`（R4-6），以及与入库登记表对账的 `guard-registry`（R7-0）。<br>**`feature-matrix` 为什么必须逐 crate**：`cargo check --workspace --no-default-features` **不穿透 crate 之间的依赖边**——`ra-eval → ra-model` 上仍写着 `default-features = true`，`ra-model` 照样带 `openai` 被编译进来，恰好漏掉要验的那一格。门禁改为逐 crate 跑 `-p <crate> --no-default-features` 与 `--all-features` 两个极端（全排列是 2^n 次编译，不值）。<br>**最后一条 SKIP 已消除**：`guard-registry` 随 R7-0 启用，`pending.rs` 删除；它读的 `guard-registry.md` 特意放在仓库根目录并进版本库——`Docs/` 整目录被忽略，CI 读不到的规格约束不了任何人。`schema-stability` 随 R2-9 启用，`prompt-dump` 随 R4-6 启用，`token-budget` 随 R2-10 启用——它只覆盖工具面这一半，**每段 prompt 的 token 上限不在其中**（R4-7 已把这半边落在别处：额度由每段在定义处声明、`PromptAssembler::assemble` 拒绝超额段，`it-coding/tests/prompt_regression.rs` 守合计 2048 与地板/上限区间，都不经过 xtask），因此它的 PASS 只说明下发的工具表放得下。<br>`cargo deny` 首跑即抓到三个真问题并已修：`time` 0.3.45 的 RUSTSEC-2026-0009（升 0.3.47，**连带把 `rust-version` 抬到 1.88**）、`portable-pty` 0.8 经 `serial` 引入的未维护依赖（升 0.9，改用 `serial2`）、内部 path 依赖是通配版本且无法发布（补 `version = "0.0.1"`）。九条 `xtask` 门禁：schema-stability / prompt-dump / guard-registry / token-budget / **no-inline-tests** / test / **public-api** / **feature-matrix** / **layering**。其中 `public-api` 对比入库基线，`layering` 校验依赖白名单、传递层级、产品引用和现代模块布局。 |
| R0-7 | **独立测试 workspace** | **DONE** | `tests/` 自成 workspace（隔离测试专用依赖，不污染主依赖图）**且进版本库**——断言是 R0 各项“已完成”的唯一证据，放在忽略目录里等于把证据留在一台机器上、CI 永远报 SKIP；现只忽略 `target/` / `Cargo.lock` / 个人实验目录。13 个库 crate 各有一个 path-dependency 宿主，另有 `it-e2e` 承载跨 crate 契约，补齐了此前遗漏的 `it-eval`，个人实验 `test-gemini` 不再混入正式测试成员。删除 60 个零断言占位文件——能力尚未实现时不创建测试文件，避免 Cargo 的“0 passed”制造假绿；当前 9 个测试源包含 136 条有效断言。`cargo xtask test` 在运行前会机械校验宿主映射并拒绝空测试源；宿主清单**从 `crates/` 推导**（有 `src/lib.rs` 即需 `it-<后缀>` 宿主），不写死在任何常量表里——写死的表只会在新建 crate 那天忘记更新。测试属性按**属性名前缀**匹配而非整行比较，否则 `#[tokio::test(flavor = …)]` 与带参 `#[rstest(..)]` 会被误判成空占位（门禁误报比漏报更糟）。`workspace_contract.rs` 用同一套推导反向断言 workspace 隔离、断言进版本库、每库一宿主、`crates/` 零测试代码以及零 `mod.rs`。**明确例外仍保留**：未来 `ra-patch` fuzz 与 `ra-model` chat convert/stream 可通过 `#[cfg(feature = "test-api")] pub` 暴露 test-only 入口，但测试逻辑本身仍只能放在 `tests/`。 |
| R0-8 | **扩展面契约落地** | **DONE**（4 条进 CI，3 条留文档） | 落地形态见下表。**13 个 crate 的 `lib.rs` 全部标注了稳定性分级**，`public-api` 门禁强制这一条——没有分级，下游无从判断能不能依赖。`api/` 下 12 份基线快照入库，`cargo xtask public-api --bless` 更新，**基线变更不是错误而是需要被看见的决定**。<br>**偏离原计划一处**：⑦ 原定用 `cargo public-api`，它要 nightly（rustdoc JSON），而本仓库把工具链钉在 1.97.1 stable，为一条门禁让 CI 装第二套工具链不划算。改用 `xtask` 里基于 `syn` 的源码级快照：`pub use` 再导出与 `#[cfg]` 看不见，但「公开项被删 / 签名变了 / 悄悄多了一个」三件都抓得到。已验证 doc 注释改动**不会**误触发（属性不进快照，否则人会学会无脑 `--bless`，门禁就废了）。<br><br>**七条的落地状态**：<br>① 公开枚举 `#[non_exhaustive]` —— **CI 强制**（`NextStep` 在例外表里，附理由）；<br>② 公开结构体无公开字段 —— **CI 强制**（例外表当前为空）；<br>③ extension trait 默认实现 / ④ sealed / ⑤ 开放标签 —— **留文档与 review**：判据依赖语义而非语法，机器分不出「哪个 trait 是扩展点」「哪个枚举是分类标签」，硬检查只会误报，而**误报会让人开始怀疑门禁本身**。等 R1/R3 有真实 trait 再看能不能收紧；<br>⑥ `schema_version` + `serde(default)` + 未知字段保留 —— **机制已落地**：`ra-core::compat` 的 `SchemaVersion` / `Compatibility` / `Unknown`。关键性质是**降级读取不丢数据**（「新版写 → 旧版读 → 旧版再写」，多出的字段原样带回；默认 serde 行为是静默丢弃，那会让用户看到「resume 之后状态没了」且无从追查），12 条断言在 `tests/it-core/tests/compat_schema.rs`；<br>⑦ 公开 API 基线 —— **CI 强制**（见上）。<br><br>原始要求：把[框架契约与扩展面](#框架契约与扩展面)的「Rust 扩展安全七条」变成代码约定与 CI：① 对外数据枚举加 `#[non_exhaustive]`（`NextStep` 除外）；② 公开配置结构体只留 builder 构造；③ extension trait 的 required 方法集最小化；④ 内部 trait sealed；⑤ 分类标签留 `Custom(Cow<'static, str>)`；⑥ 可序列化结构统一 `schema_version` + `#[serde(default)]` + 未知字段保留回写；⑦ `cargo public-api` 基线快照入库。**每个 crate 的 `lib.rs` 标注 Stable / Evolving / Internal 分级** |

### R0 非目标

| 项目 | 处理 |
| --- | --- |
| 一次性设计完所有 trait | 不做；R0 只定错误/取消/配置/日志四条横切基线，业务 trait 在各自阶段定 |
| 引入 DI 容器 / 插件运行时 | 不做；Rust 用泛型 + trait object 足够 |

### R0 验收标准

| 能力 | 标准 |
| --- | --- |
| 边界清楚 | 每个 crate 的 `lib.rs` 有模块级文档说明职责、不做什么、稳定性分级 |
| 取消可靠 | 有一个 e2e 测试：run 中途取消，子进程被杀、无泄漏任务 |
| 配置可诊断 | `ra-cli doctor config` 能打印每个配置项的最终值与来源层 |
| **扩展面可验证** | `examples/minimal_agent`：**不依赖 `ra-coding`**，只用 `ra-core` + `ra-runtime`（+ 可选 `ra-tools`）定义一个自有 `Tool` 并跑通一次 run。**这个例子进 CI，是「框架是否真的通用」的唯一硬证据**——它编译不过，说明扩展面有洞 |

---

## R1 消息模型与 Provider 抽象

### R1 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R1-1 | 项模型 `RunItem` / `ModelResponse` | **DONE** | 落地为两层强类型：`RunItem` 是 session 权威记录，保存稳定 `ItemId`、`ItemProvenance`、隔离的 `RawProviderItem` 与 `SessionData`；`ModelInputItem` 是显式发送投影，剥离这些元数据并在类型上排除 `ToolApproval`，不靠 adapter 临时记得过滤。覆盖 `Message` / `Reasoning` / `ToolCall` / `ToolCallOutput` / `Handoff*` / `McpListTools` / `McpApproval*` / `Compaction` / `ToolApproval` 共 11 种 payload；call/output 统一用 `CallId` 配对，reasoning 同时保留归一化字段与完整 `provider_data` 回放真相，`ModelResponse` 保存 output / usage / response_id / request_id 并提供 `to_input_items()`。所有持久结构带 schema version、私有字段构造器与未知字段回写；9 条契约测试在 `tests/it-core/tests/item_model.rs`。R1-2 的图片 / thinking / tool-use 等多模态 `ContentBlock` 仍未展开，本项只放了 Message 必需的文本块。 |
| R1-2 | 内容块与多模态 | **DONE** | `ContentBlock::{Text, Thinking, Image, ToolUse, ToolResult, ServerToolUse, ServerToolResult}` 已落地；thinking 保留回放签名，client/server tool block 统一用 `CallId` 配对，server tool 名保持开放字符串，结果内容保留协议中立 JSON。图片通过 `ImageSource::{Base64, LocalPath}` 强类型区分，`ra-core` 只保存路径、不做 I/O。所有 payload 均带 schema version 与未知字段回写，7 条契约测试覆盖七变体往返、两类图片、配对、开放 server tool、文本投影、双层降级读取及三态缺省语义。 |
| R1-2b | **`ModelSettings` 与四层 resolve 语义** | **DONE** | 协议中立字段、`ThinkingConfig` / `Effort` / `ToolChoice`、retry/backoff 配置与 `ResolvedModelSettings` 已落地。`provider_defaults.resolve(provider_key, agent, model, run)` 不修改原层：普通字段后层 `Some` 胜出，`max_tokens` / `timeout` 取全层最严格值，metadata / headers / query / retry 合并，`extra_body` 按 `ProviderKey` 选桶后四层递归合并。trace-safe 投影从类型入口排除 body / headers / query。8 条契约测试覆盖四层快照、显式零值、严格约束、深合并、provider 切换隔离、trace 脱敏、retry falsey 值和兼容回写。 |
| R1-3 | `Model` / `ModelProvider` trait | **DONE** | `Model` 已落地为对象安全的非流式/流式双入口：`get_response(ModelRequest)` 返回归一化 `ModelResponse`，`stream_response(ModelRequest)` 返回 `BoxStream<Result<StreamEvent>>`；`get_retry_advice` 与异步 `close` 都有默认实现，第三方最小 required 集只有两个调用方法。`ModelProvider::get_model(Option<&str>) -> Arc<dyn Model>` 支持 provider 默认模型与实例缓存，异步 `close` 默认 no-op。`ModelRequest` 私有字段覆盖 `system_instructions`、协议中立 input、`ResolvedModelSettings`、模型侧 tool / output-schema / handoff 投影、tracing 与服务端续接；`ConversationContinuation` 把 `previous_response_id` / `conversation_id` 做成互斥枚举而不是两个可同时为真的 Option。请求刻意不可序列化，避免 resolved headers / query / body 误入 trace 或持久态。`StreamEvent` 这里只冻结 raw / run-item / agent-update 信封，delta 聚合与终态 backfill 仍属 R1-7；`RetryAdvice` 这里只冻结 provider evidence 外壳，规范化错误与最终策略仍属 R1-9/R1-9b。7 条契约测试覆盖完整参数面、续接互斥、tracing 三态、trait-object 双入口、默认 retry/close、raw stream 信封的未知字段降级回写，以及**模型事件通道不含 run 级事实**。<br>**事件分层修正**：初版 `StreamEvent` 含 `AgentUpdated`，等于允许 provider adapter 发布「公开 agent 变了」——adapter 既不知道 agent 也不知道 handoff，是类型允许的无效状态；参考实现同样是两条通道（`Model.stream_response` 只发 provider 事件，消费者联合另加 run 级变体）。改名为 `ModelStreamEvent` 并收窄为 `{RawResponse, RunItem}`，`AgentUpdatedStreamEvent` 删除，run 通道由 `ra-runtime` 在 R1-7/R13 包一层。<br>`RunItemStreamEvent::name` 暂留开放字符串并写明理由：词表由 runner 映射步骤项产生（R1-7/R3-1），现在闭合等于照参考实现猜。<br>**两处刻意缺席已写进类型文档**：可复用 prompt 对象属 Responses 专有，走 `extra_body`；`prompt_cache_key` 是会话生命周期的运行期值，`extra_body` 那个静态桶不对，R1-13 落地时应作为 `ModelRequest` 新字段（`#[non_exhaustive]` 保证不破坏）。<br>**2026-08-28 裁定：`stream_response` 改为带默认实现，`get_response` 成为唯一必填方法。** 起因是 R3-4b 之后每次模型调用都走 `stream_response`（`call_model` 写明「so every call streams」，`partial_messages` 只决定旁白是否离开 runtime），而 trait 把两个方法并列为必填，第三方据此以为非流式 run 会走 `get_response`——`examples/minimal_agent` 第一版正是这么写的，跑起来直接报错。<br>**没有删 `get_response`，因为它不是死代码**：`tests/it-model` 有 60+ 处调用它，那是三个 adapter 非流式wire path 的契约测试，删掉等于把那套测试连根拔了。真正死掉的是「运行时会调它」这个预期。<br>**默认方向选择 `stream_response` 兜底而不是反过来**，理由是两个退化默认的代价不对称：由 `get_response` 合成一条单 `Completed` 的流，损失的只是 R3-4b 的重叠执行（性能属性，不是行为），而且 `get_response` 借此重新变成 loop 间接走到的原语；反过来把 `stream_response` 设为必填，则是让每个 mock、测试替身和示例都去写 `BoxStream` 管道，换不到任何东西——真 adapter 两种情况下都会自己实现。仓库里 19 处 `impl Model for` 只有 3 处是真 adapter，其余全是前一类。<br>**连带改的三处**：`examples/minimal_agent` 删掉手写流式实现与 `futures` 依赖（这是这次改动的人体工学证据）；`turn/prepare.rs` 引用 `Model::get_response` 的过期注释改指 `stream_response`；`it-core` 的 `test_model_contract_05` 里那个 `MinimalModel` 原本用 `stream::empty()`，在 R3-4b 之后是一个 loop 根本驱动不了的模型（流不以 `Completed` 结束就没产生 turn），现在删掉该方法走默认。新增 `test_model_contract_default_stream_yields_one_completed` 钉住默认流恰好是一个 `Completed`。 |
| R1-3a | Provider 注册与模型名前缀解析 | **DONE** | `ra-model::provider` 已落地不可变 `ProviderRegistry`：builder 一次性校验 canonical `ProviderKey`、prefix alias、模型 alias、默认 provider、unknown-prefix target 与 `extra_body` 桶身份；`ModelSelector{provider,model,protocol}` 始终返回 canonical provider key 和 provider-facing 模型名。显式注册优先，未知前缀可 fail-fast 或用 `UnknownPrefixPolicy::ForwardTo` 把**完整原字符串**交给 compat；`openai` / `gemini` / `anthropic` / `grok` 没有任何内建分支，是否特殊完全取决于注册项。`ProviderFactory` 在不进入 `Debug` 的闭包/实现里持有 endpoint、凭据、headers 与 client 构造；同一个 `ProviderRegistration` 还持有 protocol、模型别名、provider/model 两层 settings 和静态 `extra_body`，R1-6b 的 `ProviderQuirks` 已在模块文档中明确要求原位加入，不另建 vendor 表。provider 实例按 canonical key 懒加载缓存；统一 `close` 去重共享实例、即使一个失败仍排空其余 provider、幂等并永久封闭 registry。`ResolvedModel::resolve_settings` 把注册层与模型层接回四层 merge，因此 Anthropic 可在注册层补出必填 `max_tokens`、再受模型上限钳制。9 条 `it-model` 契约测试覆盖显式优先/完整转交、零厂商分支、默认 provider、模型别名与 Anthropic 兜底、懒加载缓存、`ModelProvider` trait、close 语义、fail-fast 与注册冲突。 |
| R1-4 | OpenAI Responses provider | **DONE** | `ra-model::openai::responses` 已落地可真实调用的 `OpenAiResponsesProvider` / `OpenAiResponsesModel`：共享 `reqwest` client、默认模型与实例缓存，`OpenAiAuth` 支持 API key / base URL / organization / project / 默认头且 Debug 脱敏。请求 lowering 把 `reasoning.encrypted_content` 合并进 include；稳定前缀走顶层 `instructions`，动态 system message 保持 input 尾部；支持 previous response / conversation 互斥续接、文本与 base64/本地图片、reasoning replay、function tool / handoff、structured output、effort、metadata、transport extras，工具存在时默认 `tool_choice=auto` 与 `parallel_tool_calls=true`。发送前复用 R1-17 normalizer。完成态 lifting 保留 response/request id、usage/cached/reasoning token、message phase、refusal、reasoning 完整 provider data、tool/handoff call 与隔离的 raw provider item；HTTP/transport/非法模型输出映射到通用错误分类，失败响应的 `x-request-id` 进错误文本。`stream_response` 在 R1-7 前明确只做完成态适配，不伪装 token SSE。13 条 mock HTTP 契约测试覆盖请求形状、双 continuation、store 推导、handoff 回放、压缩历史、hosted tool 合并、截断响应、鉴权与脱敏、本地图片、reasoning/tool replay、usage、错误映射及完成态事件。<br>**四处初版设计冲突已修正**：① `store` 原为无条件 `false`，与它同时支持的 `previous_response_id` 互斥——未存储的响应下一轮必然找不到（计划 R9-14 早写明这条禁忌）。改为**由续接模式推导**：无续接 `false`（Codex 式全量回传），有续接 `true`；`extra_body.store` 显式值优先，唯一硬拒的是 `previous_response_id` + `store=false`。② `HandoffCall` 只存 `target_agent`，回放要反查「当前请求是否还广播该 handoff」——而控制权转移后目标 agent 正好不再广播它，整条历史从此发不出去。`HandoffCall` 增加 `tool_name`（lifting 时从 wire 记录），反查降为 fallback。③ `Compaction` 原被判成「无 provider replay data 不可 lowering」，等于 R5 一压缩就发不出去；它的 `summary` 本就是协议中立的，现在落成一条 user 消息，措辞归产出它的压缩步骤。④ extra_body 合并策略自相矛盾（`include` 取并集，`reasoning` / `tools` 整体覆盖）：`effort` 现在按 key 合进 `reasoning`（不再连带清掉只能走 extra_body 的 `summary` / `context` / `mode`），hosted tool 与中立工具合并且跨来源查重，`tool_choice` / `parallel_tool_calls` 只在中立层未设时兜底。<br>**另修**：`status` 之前完全没读，`incomplete` 的半截消息会当成 final answer 交给 runner——现按 `incomplete_details.reason` 映射成 `ContextOverflow` / `Refusal`，`cancelled` 与未完成态一并拒绝（`ModelResponse` 至今没有承载部分结果的字段——原写"等 R1-8"，但 R1-8 的题域是 usage 账本、已按此完成，这条留白目前无人认领，见 R1-8 行末）。<br>**真实端点实测**（`tests/it-model/tests/openai_responses_live.rs`，默认 `#[ignore]`，环境变量驱动，离线 CI 不受影响）：对 bianxie.ai 中转的 `gpt-5.5` 打通两条路径——单轮往返，以及**「reasoning + tool call → 原样回放 → 拿到 final message」**。实测结论：① 该中转**接受并透传** `include: reasoning.encrypted_content`，reasoning 项带 `id` 与 `encrypted_content` 回来，回放不报 `reasoning item without its required following item`；② `phase: final_answer` 是真实回传字段，不是本地臆造；③ `x-request-id` 有值但是中转自生成的 UUID，不是 `OpenAI` 的 `req_` 形态——R1-9 归一化错误时不能假设它能在 `OpenAI` 侧检索；④ 短 prompt 下 `cached_tokens` 恒为 0，缓存透传能力待 ≥1024 token 的长前缀再验（R1-8/R1-13）。<br>**已知留白**：`include: reasoning.encrypted_content` 仍无条件下发——按请求内容推导会打断推理模型的第一轮（replay 材料正是那轮首发），正确的门控是模型能力轴，见 R1-3a / R1-13；hosted tool 的输出项没有中立 item 类型，回来会明确报错而不是静默丢半个 turn。 |
| R1-5 | Anthropic Messages provider | **DONE** | `ra-model::anthropic` 落地可真实调用的 `AnthropicAuth` / `AnthropicMessagesProvider` / `AnthropicMessagesModel`：认证与 `anthropic-version`、beta 与传输头（`x-api-key` / `anthropic-version` / `anthropic-beta` 不可被 `extra_headers` 顶掉）、逐请求超时与 query、请求 lowering、响应 lifting、SSE 重组、错误分类与 `Retry-After` / `x-should-retry` 归一化。R1-11 落在 compat preview 的三条 thinking/effort 耦合整体搬了过来。<br>**`cache_control` 的落点与本条原文不同，理由写在代码里**：原文设想 system 数组 block[0] 放易变头、block[1..] 设断点，但 `ra-core` 的 `system_instructions` 按契约**整串就是稳定前缀**（`validate_cache_plan` 对它整体取哈希），易变内容本来就该走 messages 尾部。于是 system 下发为单块数组并在该块打断点，门槛按 **tools + system 合计** token 估算（`MIN_CACHEABLE_PREFIX_TOKENS`，因为 tools 排在 system 之前、同属一个前缀），不足则不打——省下一个断点而不是买一次必然落空的写入。<br>**三处按当代 API 而不是旧形状下发**：结构化输出走 `output_config.format` 并与 `effort` 合并进同一个对象（顶层 `output_format` 已废弃；两者分键写会互相覆盖），adapter 自动补 `structured-outputs-2025-11-13`（caller 自配任意 `structured-outputs-*` 则不插手）；`ToolChoice::None` 如实下发 `tool_choice:{type:none}`——另一种说法只有撤掉工具表，而那是唯一会让 tools+system 缓存整块重建的改动；`redacted_thinking` 逐字回放（它既无正文也无自己的签名，向它要签名等于让 redacted 之后每一轮都发不出去）。<br>**流式三条硬约束**：`message_start` / `content_block_start` 的 object 形状在入口一次立住（往非 object 的 `Value` 写键会 panic，网关发一帧坏数据就能把进程带走）；首个失败即终态（「没收到 message_stop」在输入耗尽后每次 poll 都成立，drain 一条失败的流会拿到无限个同样的错误）；`error` 帧按 vendor error type 分类，流中 `overloaded_error` 判为可原样重试而不是「改请求再试」。<br>**tool_result 保持在 user 轮开头**，历史里结果与其 `tool_use` 断开时本地拒收而不是拼一个必被 400 的请求——normalizer 默认 `DropCallsWithoutOutputs` 会保留孤儿 output，这条路径没有服务端续接可依托。<br>**刻意没做**：`thinking` 开着仍照常下发 `temperature` / `top_p`（当代模型对采样参数直接 400，但那是模型能力轴 R1-3a 的事，与 R1-11 对 effort 的处理同源，adapter 不猜）；forced `tool_choice` + thinking **不拦**，那条限制是 Bedrock 专有、第一方 API 不要求，真要防属于 `ProviderQuirks` 的 provider 事实轴。<br>验收：`tests/it-model/tests/anthropic_messages.rs` 9 条覆盖请求形状与头、thinking/text/tool 三类 lifting 与 usage 明细、redacted 回放逐字往返、effort+schema 合并进单个 `output_config`（含 beta 头）、`tool_choice:none` 保留工具表、坏帧报错不 panic、流中 overload 判可重试与溢出判 `ContextOverflow`、截断流只失败一次并终止、tool_result 断裂本地拒收且一个请求都没发出。<br>**遗留缺口**：① `anthropic::smoke` 这份 preview 没退休，两份 lowerer 需手工同步，且 `compat_smoke` 的快照钉的是 preview 而非发货代码（见 R1-14）；② messages 层没有缓存断点，agent loop 里增长的那半永远不命中，断点回看窗口只有 20 个 block，归 R1-13 旁边；③ messages 里的 `{"role":"system"}` 算子通道未支持（Opus 5 / 4.8 / Fable 5 已 GA、无需 beta 头），这正是本条原文说的「动态提醒走 messages 尾部」；④ 无工具时仍可能下发 `tool_choice`；⑤ CI 的 clippy 只跑默认 feature，`anthropic` 模块目前无 lint 门禁，现存 5 条 pedantic 告警。 |
| R1-5b | **`ApiProtocol` 能力矩阵** | **DONE** | `ra-core::model::protocol` 已用封闭的 `#[non_exhaustive] ApiProtocol` 和私有字段 `ProtocolCapabilities` 显式冻结三套线协议；公开强类型覆盖 reasoning 载体与 replay 材料、服务端会话、稳定前缀、缓存、工具调用/结果载体和结构化输出位置，并提供 first-class reasoning / `previous_response_id` / `conversation_id` 的能力查询。`compat` 明确定义为 Chat 协议 + provider quirks，不伪造第四套协议；provider 开放性继续由 `ProviderKey` 承担。`ModelRequest` 保持协议中立，5 条契约测试锁住完整矩阵、runtime 禁止 Responses 假设及稳定配置名往返。**两处 provider 事实已从协议轴移除**：`prompt_cache` 改为可并存的 `PromptCacheSupport` 集合（两条 OpenAI 协议在缓存上同构，「某端点认不认 `prompt_cache_key`」归 `Quirks`）；Chat 的 reasoning 归零（`reasoning_content` 是 Qwen / DeepSeek / Kimi 网关约定，第一方 OpenAI 从不回传，GPT / Gemini 思考形状不同），`MessageField` / `ReasoningContent` 两个变体一并移除。`ApiProtocol::ALL` 用 `&'static [Self]` 而非定长数组，避免加第四个协议时破坏公开类型。**遗留缺口见 R1-13**：OpenAI 的显式断点尚未建模，代码里 `PromptCacheSupport` 的文档已写明不得复用 Anthropic 的能力位。 |
| R1-6 | **OpenAI Chat Completions provider（第一等公民）** | **DONE** | `ra-model::openai::chat` 已落地可真实调用的 `OpenAiChatProvider` / `OpenAiChatModel`，与 Responses 共用 auth、错误映射与新写的 `openai/sse.rs` 帧解析。**它不是 Responses 的降级层**：没有一等 reasoning item、没有服务端会话、工具调用载体是 message 里的 `tool_calls`。<br>**`convert.rs`（双向）**：合并状态机把中立的 reasoning / message / 多个 tool call 折成同一条 assistant 消息，四个动作 `flush` / `begin_assistant` / `ensure_assistant` / `attach_pending` 对应参考实现的四个 helper。§2 七条逐条落地——① 空 `tool_calls` 整个删字段；② 没有 tool call 的 assistant 轮结算时清掉 pending `reasoning_content`；③ 未紧跟 assistant 消息的 reasoning 不把带签名块顺延到下一轮；④ 多项合一；⑤ 工具结果只留文本，非文本要端点显式声明，全空时按 `strict` 报错或落 `[tool output omitted]` 占位；⑥ `Reasoning.provider_data` 原样保存 provider thinking 序列作为回放真相来源，`content` / `encrypted_content` 是派生；⑦ **在 Rust 里结构性消失**——输入是封闭的 `ContentBlock` 枚举，不存在「别名 dict」这回事，缓存断点则是 R1-13 明确留白的能力位，两者都写进注释而不是伪造实现。<br>**`stream.rs`**：SSE → 合成 **Responses 形状**事件（`RawResponse` 通道）+ 归一化 `RunItem`（中立通道）。§3 五条：① `BufferedToolCall` 按 index 累积分片，`should_buffer` / `finished_tool_calls` 决定冲刷，可选 `with_buffered_tool_calls` 打开预缓冲；② `OutputLayout` 自己推 `output_index`（reasoning 占 0，message 落在已公开的 call 之后），一旦发出永不重算；③ 单调 `sequence_number` 贯穿所有事件；④ thinking 文本与签名跨 delta 开块/累积/收尾，签名闭合当前块；⑤ `has_passthrough_output` 保证只带 provider 私有字段的 delta 不被缓冲吞掉。<br>**`reasoning.rs`**：`ReasoningReplayContext` / `ReasoningReplayPolicy` + 默认策略。<br>**四处对参考实现的刻意偏离（均已写进代码注释）**：① `strict_feature_validation` **默认开**（参考实现默认关）——静默降级事后表现为「模型无视了一条谁也找不到的指令」，按本文档 §2⑤ 的结论默认报错；② `reasoning_content` 回放的默认策略**不含 `deepseek` 字符串嗅探**，改为「端点声明 + 该 item 的 origin 必须是本模型（或根本没记 origin）」，理由与 R1-3a 的零厂商分支一致；③ `Compaction` 落成一条 user 消息而不是像参考实现那样拒绝，否则会话一被压缩就发不出去（与 R1-4 同一处修正）；④ `finish_reason == "length"` 映射成 `ContextOverflow` 而不是当作正常终态返回，与 Responses 侧拒绝 `incomplete/max_output_tokens` 是同一条理由。另：`Reasoning.id` 对 Chat 保持 `None`（Rust 侧 id 可选，不需要 `FAKE_RESPONSES_ID` 兜底），`FAKE_ITEM_ID` 只用在合成流事件的 `item_id` 上；`ModelResponse.response_id` 保持空，`chatcmpl-` 不是可续接句柄。<br>**能力开关**：`ProviderQuirks` 新增 `store` / `stream_usage` / `parallel_tool_calls` / `multimodal_tool_output` / `reasoning_content` / `thinking_blocks` 六项，全部默认关——其中 `reasoning_content` 正是 R1-5b 明确「推给 provider 轴、但当时还没有落点」的那一条。端点能力归 `ProviderQuirks`，调用方策略归 `ChatLoweringOptions`，参考实现把两者混在 converter 参数里。<br>**顺带的共享收口**：`openai/content.rs` 抽出两协议完全相同的图片源解析（base64 / 本地路径 / URL / 已上传文件）与工具输出字符串化，Responses 侧改为调用它；公开面不变（`api/` 基线仅新增，无删改）。<br>28 条契约测试在 `tests/it-model/tests/openai_chat.rs`，§2 七条与 §3 五条各有一条对应用例，另覆盖请求形状、未声明端点不发可选字段、显式 `parallel_tool_calls` 压过端点声明、缓存 scope 双条件、续接拒绝、hosted MCP tool choice 拒绝、完成态 lifting（reasoning / message / tool call / handoff / usage 明细）、截断分类、内容过滤转 refusal、流式 usage 与流式失败单事件。 |
| R1-6b | OpenAI 兼容端点 compat 层 | **DONE** | `ra-model::compat::CompatEndpoint` 已落地：一个第三方中转或本地模型端点的全部事实——base URL、凭据（或显式声明的「没有凭据」）、能力开关、codec quirk、厂商必需的非标 body 字段——都挂在这一个值上。`into_registration(key)` 把它变成 `ProviderRegistration`（协议固定 `OpenAiChatCompletions`，quirks 与静态 `extra_body` 一并挂上，并在读配置时就校验而不是等第一次 resolve）；`build_provider()` 是不过 registry 的直连入口。**没有新 codec**：compat 不是第四套协议，它配置的就是 R1-6 那套 Chat 适配器。<br>**Quirks 七项已齐**：`supports_store` / `supports_stream_usage` / `supports_multimodal_tool_output` / `supports_parallel_tool_calls` / `accepts_prompt_cache_key` / `returns_reasoning_content` 六项 R1-6 已做进 `ProviderQuirks`，本项补 `sse_done_marker` 并把整套接到端点配置上。**默认保守**照旧：未声明的端点收不到任何可选字段。<br>**`DoneMarker` 三态**（`compat::quirks`）：`Standard` = `[DONE]`；`Literal(String)` 额外认一个自定义标记（`[DONE]` 仍然认——端点两个都发不吃亏，而 `[DONE]` 本来也不是合法 chunk）；`Absent` = 这个端点不发终止符，body 结束就是结束。`Literal` 必须非空且不带首尾空白，两种坏配置都在建端点时就拒：空的会让任何一个空 `data:` 帧终止整条流（静默截断 turn），带首尾空白的因为比较的是 trim 过的 payload 而永远匹配不上（表现为「端点发了坏 JSON」这种误导性报错）。**第三态确实削弱了 R1-6 那条「只凭证据结算」**，代价写进类型文档：对这种端点，被中转掐断的连接和正常收尾是同一串字节。**但空流照样拒收**——一条什么都没送到的流结算成成功，表现为「模型没话说」，那是最贵的一种错。<br>**keyless 端点**：`OpenAiAuth::api_key` 改成 `Option<String>`，新增 `keyless(base_url)`，两条 OpenAI 协议的 `send` 只在有凭据时才加 `Authorization`。Ollama / vLLM / llama.cpp / LM Studio 本来不要 key，逼用户编一个假 key 过校验等于把无意义的值写进配置和日志；**空字符串仍是配置错误**——那是「读了个没设的环境变量」，不是「这个端点不要凭据」。<br>**两处把静默失败改成响亮失败**：① base URL 以 `/chat/completions` 结尾直接拒（把厂商文档里的完整 URL 整个贴进来是接第三方最常见的错，放过去就是一个谁也没打印过的 URL 报 404，读起来像端点挂了）；② `extra_body` 只能经注册项的 settings 层下发，所以 `build_provider` 在有静态 body 字段时拒绝构造，而不是给一个悄悄丢掉这些字段的 provider。<br>**运行期能力只有一个来源**：factory 用 registry 交给它的 quirks，不是端点里那份拷贝——`into_registration(...).with_quirks(...)` 事后覆盖仍然生效，注册表和 provider 不会各说各话。<br>**与设计文档 §4 的偏离**：`Quirks` 没有做成一个结构体。请求字段能力（`ProviderQuirks`，`Copy`，协议无关，Responses / Anthropic 也用）与「回包怎么读」（`DoneMarker`，带 String，只有兼容端点才可能不同）是两类事实，塞进同一个 `Copy` 结构会把 `const fn quirks()` 这类既有公开签名一起改掉。`Quirks` 与 `extra_body` 仍然**同住 provider 注册项**（R1-2b §2），接入一家新厂商只改一处。<br>14 条契约测试在 `tests/it-model/tests/openai_compat.rs`：未声明端点收不到 `store` / `stream_options` / `parallel_tool_calls` / `prompt_cache_key` 且不带 `Authorization`；声明后逐条上线；`stream_options.include_usage` 只发给声明过的端点；lowering 策略随端点走；一条注册项同时带能力与 `extra_body` 且两者都到达 wire；注册项覆盖胜过端点声明；未知前缀整串转交成 wire 上的 `model`；`Literal` 与 `[DONE]` 都能收尾；`Absent` 在 body 结束时结算但空流仍拒；默认端点对无证据的流照样报错；两处响亮失败各一条 |
| R1-7 | 流式与部分消息 | **DONE** | 三件欠账都补上了。**① 终态 backfill 落在模型通道上**：`ModelStreamEvent::Completed(Box<ModelResponse>)` 是一条流的最后一个事件，装的就是非流式入口返回的那份。delta 拼不出它——usage 单独占一帧、request id 在响应头、输出顺序是整通调用的属性；**缺席本身有含义**：没有它的流没有产出 turn，不管它吐了多少旁白。Chat 侧在发布每个 item 时记下它的 output slot（发布顺序 ≠ 输出顺序：夹在两个 tool call 中间的消息最后才收尾），Responses 侧直接把 `response.completed` 里的完整 response 对象过一遍非流式那套 lifting，两条协议因此不可能对同一个 turn 给出两种说法。<br>**② run 级事件通道**：`RunConfig::with_partial_messages` 打开后模型调用走 `stream_response`，provider 事件原样转发成 `RunStreamEvent::RawResponse`。**只转发旁白，不转发 adapter 的归一化 item**——那些记录由结算发布（已归因、已按 R3-10 盖好 output phase），再转发一份等于每条消息出现两次、而且第一次还没被 run 判定过。默认关：给没人看的输出拼 delta，白付一个解码器；且只在流式入口生效（另一个入口没有订阅者）。两条路径终点都是同一个 `ModelResponse`、交给同一套结算——给流式单开一条结算路径，正是流式视图与非流式视图开始各说各话的地方。<br>**③ Responses 真实 SSE**：`openai/responses/stream.rs` 替掉 R1-4 那个「发一次非流式请求再合成完成态」的占位。这一侧**什么都不合成**：帧自带 type、`output_index`、`sequence_number`，output item 在 `response.output_item.done` 整个到达，终帧带完整 response 对象——所以**在这条协议上 raw 通道是真·raw**（Chat 那边是合成的，也只有那边警告拼写不稳定）。它仍要判两件事：`response.completed` 是唯一的完成证据（body 断了不算），以及 `response.failed` / `response.incomplete` / `error` 三种帧必须当场分类，否则会被后面那条「没有终帧」的检查报成掉线。`stream` 标志归入口所有，不接受从 `extra_body` 静态设置。<br>**刻意没做的两件，都写进了代码**：`AgentUpdated`——`TurnStarted` 每轮已经报了 public agent，而结算目前拒绝 handoff，公开一个没有任何代码路径能发出的变体就是照着参考实现猜；枚举是 `#[non_exhaustive]`，R12 落地 handoff 时一行就能加。`RunItemStreamEvent::name` 的语义词表（`message_output_created` / `tool_called` / …）——Rust 侧的词表就是 `RunItemKind` 这个类型本身，再造一套并行字符串是第二份真相。<br>**顺带的共享收口**：`ensure_event_stream` 与 `failed_stream` 从 Chat 挪进共享的 `openai/sse.rs`（两条协议逐字相同），`ConvertedResponse` 包装消掉（去掉两个没人读的字段后只剩一个）。<br>**测试**：it-model 新增 Chat 终态与输出顺序 2 条、Responses 流式 6 条（全量转发 + 边到边 lifting、只有流式入口发 `stream`、缺终帧被拒、两种失败帧各自分类、端点无视 `stream=true` 时响亮失败）；it-runtime 新增 4 条（转发 + 终态结算、不开就不流、非流式入口忽略开关、流没给终态事实就失败）。 |
| R1-8 | Usage 逐请求明细 | **DONE** | `ra-core::usage` 拆成两层：`RequestUsage` 是**一次 provider 请求**报回来的原始事实（input / output / cached / cache_write / reasoning，`total` 派生），`Usage` 是跨请求账本（`requests` 计数 + 五个汇总计数器 + `request_usage_entries` 逐条明细）。**没有"直接给汇总数"的构造入口**——只能 `Usage::from_request` / `From<RequestUsage>` 进、`accumulate` 加，因此账本里每一个 token 都附着在报出它的那次请求上，`requests` 不可能和被加进来的东西对不上。<br>**汇总值存储而非从 entries 推导**：entries 会被裁剪（checkpoint 投影），更老的记录本来就没有；推导出来的总数会在"记录最老、run 最长"的时候少报，而那正是最需要它的时候。因此对账不靠类型内部自洽，而是落在唯一能做的地方——rollout 日志里永不裁剪的 `model_usage` 记录之和必须等于账本声称的总数。<br>**两条 OpenAI 路径**（非流式 + 流式）各把一次调用记成 `requests=1` 的一条 entry；**端点没回 usage 也算一次请求**（零 token）——中转端点省略 usage 块是常态，不记等于让这种 run 看起来根本没调过模型。`ModelResponse::usage` 保持 `Usage` 而不是收窄成 `RequestUsage`：一次响应不一定只有一次请求（R1-12 拒答升级、adapter 内部拆分），参考实现也是同一形状（`request_usage_entries`）。<br>**单一事实来源落地**（R6-6a 留给 R3-8 / R1-8 的那条）：`BudgetSnapshot` 删掉 `tokens_used` / `record_usage` / `remaining_tokens` / `exhausted_kind` 的 token 维度，只留 turn；账本住进 `RunState.usage_totals`（从 `Option<Usage>` 占位槽改成必填带 default 的 `Usage`），`RunState::record_usage` 是唯一入口，`tokens_used` / `remaining_tokens` / `exhausted_budget_kind` 都在**同时持有两份事实的地方**回答。没有把 `exhausted_kind(limit, usage)` 留在 snapshot 上是刻意的：那个 usage 参数一旦传错（传一轮的而不是整个 run 的），表现是循环永不停止，而放在 `RunState` 上这种传参根本不存在。预算相关的旧字段被反序列化时进 `Unknown` 原样回写，**不会**变回花掉的额度。<br>**两个总数刻意不同且都保留**：`RunResult::usage()` 是**本段**（由 `model_responses` 求和投影），`state().usage_totals()` 是**跨段**账本、也是预算度量的对象；续跑时它们本就应该不一样，把前者当 run 总数会漏掉每一次 continuation。两处文档与测试都写明了这条。<br>**顺带的读侧收口**：`RunContext::usage_totals()` 给工具一个只读视图（结算点已把本轮计入），`RunErrorData::usage()` 让 closeout handler 拿到跨段账本（它手上的 responses 只有本段），trace 词表新增 `usage.requests`（同一批 token 是一次大调用还是三次小调用，报表此前无从分辨）。<br>**rollout 侧的分工**：`model_usage` 记录保留 entries，是永不裁剪的对账基线；`RolloutCheckpoint` / `RolloutSidecar` / `RolloutSummary` 只留汇总——checkpoint 每隔若干条就重述一次总数，把 entries 也带上会让日志随会话长度**平方**增长。checkpoint 校验加上 `requests` 维度：token 数对得上但调用次数被改过的 checkpoint 此前能过关，而所有"每请求平均"从它算出来都是错的。<br>**测试**：新增 `tests/it-core/tests/usage.rs` 11 条（明细是子集不是加项、一次请求一条 entry、累加保序、totals-only 投影仍可继续累加、带 entries 的 JSON 往返、无明细的老记录仍报总数且能续加、未知计数器保留且后来者胜、饱和不 panic、零 usage 仍计一次请求、"只有 entries 能定位是哪一次调用没命中缓存"）；it-core budget 7 条改写为对账本度量（含"旧版预算里的 tokens_used 不会复活成额度"）、run_state 增 2 条（账本跨 checkpoint 继续逐条累计、只读投影进 context）；it-runtime 增"逐请求条目跨轮保留且与 state 账本逐条一致"与"续跑的账本覆盖全段而 result 只覆盖本段"，trace 断言 `usage.requests` 的三层口径；it-model 两条协议各加 entry 形状断言、Responses 增"端点不回 usage 仍计一次请求"、两条流式各断言流式与非流式记法一致；it-session 增"checkpoint 只留汇总而 model_usage 记录仍带 entries"与"请求数被篡改的 checkpoint 被扫描抓出"；it-prompt 增逐请求命中率。全仓 847 条断言通过，四道静态门禁绿。<br>**Review 后补三处（2026-08-22，`b90ac2a`）**——都是「数字悄悄少了」而不是「操作失败」的那类：① **旧 checkpoint 的 token 丢失**：`BudgetSnapshot` 拿掉 `tokens_used` 之后，旧键无处落脚、只被 `unknown` 原样留着，而新的上限只读账本——续跑等于白拿第二份额度。现在由具名字段接住，并在**两个门**都折进账本：`RunState` 反序列化（经 `RunStateRecord` 影子结构，字段漏写由既有的九槽往返用例兜住）与 `with_budget`；只堵一个门等于「看你从哪扇门进来决定收不收费」。迁移后归零、不会二次计入，快照单独往返也不丢（**这一条是审核补的**：原实现用 `skip_serializing`，宿主若先单独落盘快照再 attach，spend 在写出那一刻就没了，等到迁移时已无可迁移）。旧值以**无拆分的总数**进 `carried_total_tokens`：计入 `total_tokens()`（预算量的就是它），不进 input/output——旧记录从没说过怎么拆，塞进 input 等于把一个没人测量过的数字放进每一个缓存命中率的分母。`Usage::from_carried_total` 是 `pub(crate)`（**审核收窄**）：迁移专用构造器，公开出去就是一个凭空铸造无归属 spend 的入口，恰是本任务一直在防的事。② **默认 `ModelResponse` 记 0 次请求**：按旧 API 写的适配器与任何第三方 `Model` 会成功完成调用却不计数。`new()` 与 serde 默认都改成「一次、未知 token 的请求」——响应存在就意味着请求发生过；要求每个实现记得声明这件显然的事、忘了还不出声，是两个默认里更差的那个。合成响应（replay / stub）显式 `with_usage(Usage::default())` 说明没有调用。③ **`RequestUsage` 的未知计数器只活在条目上**，`without_entries()` 之后彻底消失——恰好是「未知字段跨版本保留」要防的失败，现在提升到账本层且仍保留在条目上。三条各配用例：钉住旧错误行为的那条改成断言迁移、state 级往返证明 spend 既不丢也不翻倍、条目未知计数器活过投影，以及 trace 侧按新的请求数口径更新。<br>**本条未做、需另立条目**：R1-4 注释里那句"`ModelResponse` 承载部分结果要等 R1-8"没有兑现——本条的题域是 usage 账本，而"部分消息"属 R1-7 且 R1-7 已按"没有终帧就不算 turn"结案。截断响应目前仍映射成分类错误（`ContextOverflow` / `Refusal`），代码里那处过期指向已删。要让半截结果流到 runner，涉及 R3-1b 的终止原因与 R5 的溢出处理，应当单独排期。 |
| R1-9 | 错误归一化与重试 | **DONE** | `ra-core::model::retry` 拆成三层：配置（`ModelRetrySettings` / `RetryBackoffSettings`，R1-2b 已有）、provider 事实（`NormalizedProviderError` / `RetryAdvice`）、算术（`RetryBackoff`）。**事实挂在错误的 source 链上**而不是塞进 message——`into_error()` 进、`from_error()` 出，错误形状不变、上层继续只读 `recoverability()`，要 `retry_after` 的策略不必解析散文。字段：`kind` / `message` / `status_code` / `error_code` / `request_id` / `retry_after` / `should_retry` / `replay_safety`，外加 `stamp_replay_safety()`（只改重放判据、其余事实与 source 原样保留）与 `replay_safety_of()`。`RetryBackoff` 是配置解析后的四个具体值加算术（默认 500ms / 8s / ×2 / 有抖动，对齐参考实现），**随机数由调用方注入**（`JitterSample`）——内核保持确定性，schedule 可以断言而不是近似；`retry_after` 在 60s 窗口内原样胜出且不抖动，超窗退回有界 schedule。`ra-model::retry` 放两件与厂商无关的 HTTP 事实：`RetryHints` 解析 `retry-after-ms` / `retry-after`（秒数与 IMF-fixdate 两种形式）/ `x-should-retry`，以及两条 OpenAI 协议共用的 `retry_advice()`。两个 adapter 的错误构造改走 `ResponseFacts::read()`（body 被消费前一次性读完头部）→ `response_failure()`。<br>**四处与草稿不同，各有理由**：① **`is_abort` 不要**——取消在本框架是 `Error::Cancelled` 自成一档，再给 provider 事实加一个弱版本的同问题字段，早晚会被用来重试用户主动停掉的 run；② **`is_network_error` / `is_timeout` 是投影不是字段**，理由同 `recoverability` 不存储：两个能互相矛盾的字段迟早矛盾；③ **`replay_safe: bool` 保持三态 `ReplaySafety`**——bool 表达不了「provider 不知道」，而那正是不能当成 safe 的那一格；④ 厂商 code 原样保留不映射（`insufficient_quota` 与 `rate_limit_exceeded` 同为 429 而含义相反，「谁愿意付钱」是策略问题）。<br>**重放判据由唯一知道真相的那层盖章**：非流式失败（消费者什么都没拿到）、流未起（refused / 未发出 / 不是事件流）记 `Safe`；**流已发出任何事件记 `Unsafe`**，两个解码器各收敛到一个出口盖章，byte 层不参与判断。<br>**review 后补三处**：① **stateful 请求不再声称 Safe**——`previous_response_id` / `conversation_id` 的请求在没拿到响应时，服务端可能已经受理并把这轮追加进了会话，重放会在客户端看不见的状态里写两遍；现按 `unstarted_replay_safety(continuation)` 返回 `Unknown`（**withheld 不是 denied**，留给 R1-9b 的 policy 显式 approval），`ModelRetryAdviceRequest::continuation` 因此真正参与判定，且 advice 只会沿用不会上调 adapter 盖的章；② **`Retry-After` 的 IMF-fixdate 形式改为解析**（自带 `days_from_civil`，无新依赖；过期日期读作「没有等待」以保住「有值就值得等」这条不变量；两种废弃日期格式明确拒绝——RFC 850 的两位年份要猜世纪，猜错就是一个世纪的延迟）；③ **HTTP 409 新增 `ProviderErrorKind::Conflict`**（`Recoverability::Retryable`）——原判成 `BadRequest` 等于把参考实现会重试的瞬时冲突降级为不可重试，两条协议各一条回归测试。<br>15 条 provider 契约测试在 `tests/it-model/tests/openai_retry.rs`（全部写在调用方真正拿到的错误与 advice 上，断言字段而不是文案——断言文案的话字段没填也照样过），11 条在 `tests/it-core/tests/retry_normalization.rs`。<br>**不在本项**：loop 还不会重试。policy、hard veto、次数/预算判定、decision 进 trace、以及「已吐 token 的流不得透明重放」的**执行**都归 R1-9b；本项只保证判据能跨层传到策略手上。`ModelRetrySettings::max_retries` 仍只是配置，尝试预算归数它的那一层。 |
| R1-9b | Retry policy 与流式重试边界 | **DONE** | `ModelRetryPolicy` / `RetryPolicyContext` / `RetryDecision` 把可执行的宿主策略与可序列化的 retry/backoff 设置分开，`max_retries` 是初始请求之外的硬上限。runner 每次物理请求各开 generation span，成功响应在 usage ledger 前补失败请求的零用量条目；退避等待走 turn cancel scope，取消或 deadline 不会在等待后再发一枪。adapter 的 `Unsafe` replay verdict 是硬拒；client-owned 非流式请求可安全重放，server-managed 的 `Unknown` 只能由策略显式批准。流式只要已向 run subscriber 转发过一个 raw provider event，随后的失败立即标 `Unsafe`，绝不重放并重复用户已见帧。`retry.*` trace 字段记录尝试、上限、等待和原因；6 条 `it-runtime/model_retry` 契约测试覆盖安全非流重试与 usage、adapter veto、公开流事件边界、显式零预算、退避等待取消、无 adapter advice 时错误链 `retry_after` 回落。 |
| R1-10 | Provider 兼容矩阵 smoke | **DONE** | `tests/it-model/tests/compat_smoke.rs` 以**同一份**中性 `ModelRequest` 产生并内联快照四种请求 payload：Responses、Chat、Anthropic Messages、OpenAI-compatible。基线同时钉住稳定 system 前缀的位置、`lookup` 的显式 tool choice、普通工具与 handoff 的表顺序、tool call/result 配对、`effort=high` 与关闭并行调用；compat 走真实 `CompatEndpoint`，不是复制一份 Chat JSON。当时 Anthropic 尚无可运行 adapter，因此只暴露 crate 内归属、`#[doc(hidden)]` 的纯 preview lowering 给该 smoke，用于锁住其 Messages 形状；R1-5 之后真 adapter 已落地，这份 preview 与它是两套 lowering、快照钉的仍是 preview，退休归 R1-14。另有两个真实 wiremock 往返：Responses 输出 → Chat 输入会把同模型 reasoning 摘要降级为 `reasoning_content` 并保住 call/result id；Chat 输出 → Responses 输入保住 reasoning 与同一工具配对。实现过程中补齐 Responses reasoning 的来源 model 标记，流式和非流式 lifting 同用，避免 Chat 的跨模型 replay 安全门把同模型的历史误判为未知来源而丢弃。 |
| R1-11 | Thinking / effort 透传 | **DONE** | `ThinkingConfig::{Adaptive, Enabled{budget_tokens}, Disabled}` + `Effort::{Low,Medium,High,XHigh,Max}` 类型已随 R1-2b 落地；adapter 只透传用户设置，不做自动 router。OpenAI Responses 将 effort 合并进 `reasoning.effort`，Chat Completions 下发为 `reasoning_effort`；两条协议都没有 thinking 开关，三个 `ThinkingConfig` 形态一律拒绝（含 `Disabled`：静默丢掉一个显式「别思考」，等于让推理模型按默认 effort 照想不误，而请求里查不到任何痕迹）。`Effort` 的全部中立档位一律下发，是否被某个模型接受属于模型能力而非协议能力；模型能力轴落地前 adapter 不猜测也不静默丢弃。Anthropic Messages 下发 `thinking={type:adaptive\|enabled,budget_tokens\|disabled}` 与 `output_config={effort}`，并校验三条 Anthropic 专有的字段耦合：`max_tokens > budget_tokens`、`budget_tokens ≥ 1024`、`disabled` 不得与 `xhigh`/`max` effort 同时出现。三条都刻意不放 `ra-core`，因为 OpenAI 的 effort 与 `max_tokens` 没有任何对应关系。**三条已随 R1-5 整体搬进真 adapter**（`anthropic/request.rs`）；`anthropic::smoke` 这份 preview 仍在，退休归 R1-14。已知未覆盖：`enabled{budget_tokens}` 只有 4.6 及更早的模型接受，当代模型只收 `adaptive`、给了预算直接 400，二选一需要 R1-3a / R1-13 的模型能力轴，preview 只下发拿到的形态。`compat_smoke` 契约测试覆盖三形态下发、五档 effort、三条拒绝各自的边界、thinking 与 tools/effort 共存，以及两条 OpenAI 协议对三形态的拒绝与五档 effort 的 wire lowering。 |
| R1-12 | 模型分层与拒答回退 | TODO | `fallback_model` 不只是"主模型不可用时的备胎"：照 CC 的运行时硬机制——**便宜模型打头，拒答/错误自动升级**（实测 `model_refusal_fallback` 38 次，`fable-5` 拒答 → 自动回退 `opus-4-8`）。回退是运行时机制不是提示词请求，且必须在 trace 里可见 |
| R1-13 | `prompt_cache_key` 稳定下发**与显式断点** | TODO | 照 Codex：`prompt_cache_key == thread_id`，**每会话恒定单值**（21/21 会话验证）。这是命中 provider 侧 KV 缓存的显式手段，不能每轮重算。**两条 OpenAI 协议都收这个字段**（`chat/completion_create_params.py` 与 `responses/response_create_params.py`），按协议区分会漏发——某个端点认不认它是 provider 事实，归 R1-6b 的 `Quirks`。<br>**缺口（R1-5b 遗留）**：官方 SDK 稳定面已加入请求级 `prompt_cache_options{mode, ttl}` 与内容块 `prompt_cache_breakpoint`。**不得复用 Anthropic 的 `cache_control_breakpoints` 表达它**——OpenAI 不打断点照样缓存（隐式断点）、TTL 在请求级、且可用 `mode="explicit"` 关掉隐式；Anthropic 不打标记完全不缓存、TTL 每断点各带。要加独立能力位。顺带两点：`automatic_prefix_matching` 对 OpenAI 是**默认模式**而非不变量；该特性文档标注 `gpt-5.6` 及以后，是**模型**门控——矩阵只有协议轴、`Quirks` 是 provider 轴，模型能力这根轴的落点见 R1-3a |
| R1-14 | Anthropic 请求形态对齐 | TODO | 实证 CC 的顶层字段：`max_tokens=64000`、`thinking={type:adaptive}`、`output_config={effort:xhigh}`、`stream=true`、`metadata.user_id`。beta 头：`interleaved-thinking`（思考与工具调用交错）、**`mid-conversation-system`（动态提醒作为 system role 消息插进 messages）**、`effort`。**`cache_control` 只设在 system 段，24 个工具无一带断点**——tools 由 system[2] 之后的整体前缀缓存覆盖 |
| R1-15 | thinking 块与签名 | TODO | assistant 消息含独立 `thinking{type, thinking, signature}` 块，signature 约 1200 字符（加密签名防伪造）。跨轮回传时必须原样保留，不能重写或丢弃 |
| R1-16 | 结构化输出 schema | DOING | 已补第一段声明链：`ra_core::output::OutputSchema` 统一表达 plain text 与 JSON schema，JSON 默认 strict；`AgentSpec` 保存不可变声明、派生 builder 原样继承并可清回 plain text；turn preparation 把它投影到既有 `ModelOutputSchema`，所以已有 provider lowering 现在能被 agent 配置驱动。**尚未完成** schema 生成、最终文本/JSON 的解析与验证、`OutputValue`、错误语义和 closeout 校验；这些仍是本条完成条件。<br>**声明在 build 时校验，不留给 provider 报错**（审核补）：初版从 `OutputSchema::json_schema` 到 wire 之间没有任何一处检查，空名字或非 object schema 能 build 成功、然后**每一轮**栽在 provider 的 400 上——正是 R3-1c 为工具名专门堵掉的那个形态。`OutputSchema::validate()` 由 `AgentSpecBuilder::build` 调用：名字按模型面名字的统一标准（非空、trim、无控制字符，与 `validate_tool_name` 同一条，per-provider 字符集限制两边都不管），schema 必须是 JSON object，**strict 声明才核对 strict 不变量**。<br>**strict 是核对不是改写**：仓库已有的 `ensure_strict_json_schema` / `verify_strict_json_schema` 从 `tool/strict.rs` 提到 crate 级私有 `ra-core::strict`（公开面无变化），错误消息里的名词参数化。分工沿用工具面既定那条——框架从 Rust 类型**生成**的 schema 走 normalize，调用方**手写**的只 verify，因为改写等于偷换它作者选的契约。`OutputSchema::json_schema` 收的是手写 schema，所以归后者；等本条的 schema 生成落地，那条路自然走 `ensure_*`，与工具面对称。upstream `strict_schema.py` 是同一份机制、同样被两个 caller 共用。<br>**输出声明读 public agent，工具/模型/设置继续读 execution**（审核补）：交付承诺不是执行能力。prepared 实例可以改自己跑什么，但不能丢掉、也不能强加一份调用方没配过的输出契约——前者会让按 schema 解析 `final_message` 的调用方在一轮看起来正常的 turn 上炸，后者会让只准备读文本的宿主收到 JSON；且 closeout 校验本来就对 public 声明负责，两边现在同源。`AgentBinding::execution` 的文档已标注这是唯一例外。<br>`ModelOutputSchema::new` 与 `ModelToolDefinition::new` 一样做 `canonicalize_json`：key 顺序取决于依赖图选了哪个 `serde_json` feature，请求体不该跟着漂。<br>**边界补充**：现有 `ModelOutputSchema` 仍只是请求侧 provider 投影；`OutputSchema` 现在拥有 agent 层声明，最终可被 R17 边路由和不同产品共同消费的值契约仍待 `OutputValue` 落地。<br>**本条必须同时接管 closeout 的校验**（2026-08-13 补，借鉴 openai `error_handlers.py::validate_handler_final_output`）：R3-8 的 `RunErrorHandler` 产出的 final message 走的是框架自己的投递路径，今天只能校验 role 与 phase（`ra-runtime::runner::validate_error_handler_message`）。agent 一旦有了声明的输出 schema，closeout 也必须满足它——否则一个契约承诺结构化输出的 run，会以一段自由文本收场，而所有按 schema 解析 `RunResult::final_message` 的调用方都会在这里炸，且这段文本还不是模型给的。落地时把校验加在那个函数里，并补一条「结构化输出 agent 的 closeout 不满足 schema 即被拒」的测试。 |
| R1-17 | 输入项规范化与 reasoning replay | **DONE** | `ra-core::item::InputItemNormalizer` 作为 `#[doc(hidden)]` 内部共享实现落地：只接收强类型 `ModelInputItem` 或从 `RunItem` 做单向投影，session provenance / raw provider payload / host data / approval 控制项不会回灌模型。去重只认 reasoning id、分类型 call id 与 MCP approval id；前驱项保留最早因果锚点但采用最新值，普通重复 message 不按正文误删。默认裁掉无 output 的 call 及其悬空 reasoning，同时保留 output-only continuation；`OrphanPolicy::DropUnpaired` 可对自包含历史做双向严格裁剪。**悬空 reasoning 的判据是「紧随的非 reasoning 项必须存活」**，覆盖两种失败形态：后继项被裁掉，以及 rewind / 压缩截断后**根本没有后继项**——初版只查前者，结尾 reasoning 会原样发出去，正是 Responses 那条 `reasoning item ... without its required following item` 的典型触发形态。`ReasoningIdPolicy` 只控制 id，encrypted content / provider replay data 永远保留。`InputItemDigest` 用确定性 JSON 的 SHA-256，session occurrence 用 `ItemId + digest` 对账，不依赖数组位置。provider conversation id 只可留在隔离的 raw payload，并再次明确：具体服务端 item-id 清洗属于 adapter/session policy，通用 normalizer 不按字符串删除前向兼容 unknown 字段。8 条独立契约测试覆盖投影清洗、孤儿策略、结尾 reasoning、因果去重、reasoning replay、occurrence 坐标与 digest 的键序无关性——最后一条同时是 `serde_json/preserve_order` 的门禁：任何依赖打开它都会让 digest 随字节序漂移，测试会当场失败。 |
| R1-18 | 可选 LiteLLM / any-llm bridge | TODO | 参考 `AnyLLMProvider` / `AnyLLMModel` / `LitellmProvider`，在独立 `ra-model-bridge` crate 或 feature 中把聚合层收敛成 `Model`。它只用于长尾 provider 接入和迁移验证，不能成为 `ra-core` 硬依赖，也不能绕过 rusty-agent 的 normalized event、retry、usage、session 与 replay 契约 |

### R1-2b `ModelSettings` 字段面与 resolve 语义

> 基线：`openai-agents-python/src/agents/model_settings.py`（390 行，22 个字段）。
> **可以大量照抄，但有三处必须改**——它是为「包一层 OpenAI Python SDK」设计的，而 rusty-agent 自己拼 JSON body，且四条协议路径平级。

#### 1. 字段分类

| 处理 | 字段 | 理由 |
| --- | --- | --- |
| **直接照抄** | `temperature` `top_p` `frequency_penalty` `presence_penalty` `max_tokens` `tool_choice` `parallel_tool_calls` `metadata` `extra_headers` `extra_query` `retry` | 协议中立，四条路径都有对应物 |
| **换形态** | `reasoning` + `verbosity` → `ThinkingConfig` + `Effort`（R1-11） | OpenAI 是 `reasoning={effort}`，Anthropic 是 `thinking={type:adaptive}` + `output_config={effort}`。统一抽象，由 adapter lowering |
| **合并成一个** | `extra_args` + `extra_body` → 单一 `extra_body` | 见 §2 |
| **必须挪走** | `truncation` `store` `prompt_cache_retention` `response_include` `top_logprobs` `prompt_cache_options` `context_management` | **全是 OpenAI Responses 专有**。留在协议中立结构里直接违反 [R1 验收标准](#r1-验收标准)第 2 条（「`ra-runtime` 里搜不到协议专有字段」）。语义上也无解：用户设 `store=false` 然后跑 Anthropic，静默忽略和报错都不对——因为字段放错了层。它们进各自 provider 的 `extra_body` 桶（§2），或由 adapter 从中立字段推导 |

> openai 自己也感觉到了这个张力：`to_traceable_dict()` 用 `_TRACEABLE_MODEL_SETTING_FIELDS` 把「provider-specific request extras」从 trace 里滤掉。我们把这条前移到**类型层面**解决，而不是在输出端补救。

#### 2. `extra_body`：厂商私有字段的逃生舱

**先认清它的主要用途。** `extra_body` 不是边缘功能——它绝大多数时候承载的是 **OpenAI 兼容端点的厂商私有字段**：

| 来源 | 常见私有字段 |
| --- | --- |
| vLLM / SGLang 自建服务 | `top_k`、`repetition_penalty`、`min_p`、`guided_json` / `guided_regex` / `guided_choice` |
| OpenRouter | `provider: {order, allow_fallbacks}`、`transforms`、`route` |
| 国内厂商兼容层 | 各家的搜索开关、结果格式等 |
| OpenAI 自己 | 新参数已发布但 SDK 版本没跟上时的临时通道 |

（示例而非核对过的完整清单，具体以各家文档为准。）**这意味着 `extra_body` 的主战场是 compat 路径（R1-6b），不是第一方 OpenAI 路径。**

**先合并两个逃生舱。** Python 那边 `extra_args` 走 SDK 的类型化具名参数（`create(**kwargs)`），`extra_body` 绕过 SDK 类型直接进 body——**这个区别纯粹是「在包 SDK」的产物**。rusty-agent 自己拼 JSON，没有 SDK 中间层，区别不存在；保留两个只会让人每次纠结用哪个。

**再分桶，键是 `ProviderKey` 不是 `ApiProtocol`。** Python 里 `extra_body` 是单个 dict，因为它只服务 OpenAI。我们有四条协议路径 + 任意多个兼容厂商，同一个 `AgentSpec` 换个 provider 就要能跑：

```rust
// 单 dict 的后果：切到 Anthropic 后这些字段原样进 /v1/messages → 400
extra_body: { "store": false, "include": ["reasoning.encrypted_content"] }
```

> ⚠️ **按 `ApiProtocol` 分桶不够**（本节初稿的错误）。`compat` 是**一个协议、一堆厂商**：vLLM / OpenRouter / DashScope / Together 会共用同一个桶，从 vLLM 切到 OpenRouter 时 `guided_json` 照样被发出去——和单 dict 是同一个 bug，只是从协议层挪到了厂商层。

正确的键是 **provider 注册键**（即 `ModelSelector` 中 `provider/model` 的 provider 别名）：它才是「哪个 endpoint + 哪套 quirks + 哪些私有字段合法」的真正身份。

```rust
/// 逃生舱：按 provider 注册键分桶，只有当前 provider 的那一桶会被下发。
/// BTreeMap 不是 HashMap —— 序列化顺序必须确定（同 R2-9 的字节稳定要求）。
extra_body: BTreeMap<ProviderKey, JsonMap>,
```

**不支持协议级共享是刻意的**：要在两个 provider 上用同一段就写两遍。多一个查找维度带来的隐式行为，不值得省那点重复。

配套三条硬约束：

| # | 约束 | 理由 |
| ---: | --- | --- |
| 1 | `ra-runtime` **只写不读，且不解释内容** | 它是 write-through 到 adapter 的不透明载荷；一旦 runtime 开始读它，协议中立就破了 |
| 2 | resolve 时**深合并而非替换** | 见 §3 |
| 3 | **不进 trace、不进 `RunState` 的可移植字段** | 不只是噪音：用户很可能往里塞敏感值，一旦进 rollout 日志就再也拿不出来 |

**与 `Quirks` 是同一问题的两半，必须同住 provider 注册项：**

| | 回答的问题 |
| --- | --- |
| `Quirks{supports_store, supports_stream_usage, …}`（R1-6b） | 这家**不能接受**哪些标准字段 |
| `extra_body` | 这家**额外需要**哪些非标字段 |

两者都是 per-provider 事实。拆到两处（一个在 provider 配置、一个在 `ModelSettings`）会导致接入一家新厂商要改两个地方，且没有任何一处能完整回答「这家到底要什么」。

#### 3. 四层 resolve 的 merge 语义

`extra_body` 有两个生命周期不同的来源，因此 resolve 是**四层不是三层**：

| 层 | 来源 | 例子 |
| ---: | --- | --- |
| 1 | **provider 注册项**（静态，该厂商永远要带） | OpenRouter 的路由偏好、自建 vLLM 的固定采样参数 |
| 2 | agent 隐式默认 | 这个 agent 的 `temperature` |
| 3 | 模型名解析出的默认 | 该模型的 `max_tokens` 上限与能力 |
| 4 | `run_config` 覆盖 | 本次 run 临时调参 |

后层赢。产出 `ResolvedModelSettings`，**不修改任何一层的原对象**。

| 类别 | 字段 | 是否 openai 行为 |
| --- | --- | --- |
| `Override`（后层直接盖） | `temperature` / `top_p` / `frequency_penalty` / `presence_penalty` / `tool_choice` / `parallel_tool_calls` | ✅ 照抄 |
| `Merge`（合并不替换） | `extra_body` / `extra_headers` / `extra_query` / `metadata` / `retry` | ⚠️ **部分是我们的修正**——openai 只合并 `extra_args` 与 `retry`，`extra_body` 是整体替换。两者并成一个之后统一按合并处理，顺手消掉这个不一致 |
| `TakeStricter`（取更严格的） | `max_tokens` 不得超过模型上限、`timeout` 取更短 | ❌ **我们的增强，不是 openai 行为**。他们所有字段都是纯覆盖。理由：模型上限是硬事实，`run_config` 设个超上限的值只会换来 400 |

**最容易错的一条**：必须区分「未设置」与「显式设成默认值」——用 `Option<T>` 而不是拿默认值填充。`temperature: Some(0.0)` 是用户的决定，`None` 才允许下层填。混同之后要么下层永远盖不了，要么用户显式设的 `0.0` 被悄悄改掉，两种都极难排查。

#### 4. 工程约束与验收

- builder + `#[non_exhaustive]`（扩展安全第 2 条）；`ModelSettings` 在 Stable API 清单内。
- `extra_body` 与 `Quirks` 同住 provider 注册项（R1-3a），不拆两处。
- **验收**：四层 fixture 的 resolve 快照测试，外加五条断言——
  1. 未设置字段可被下层填充，显式设置不被覆盖；
  2. `max_tokens` 取更严格值；
  3. `extra_body` 四层深合并（provider 注册项 → agent → model → run_config）；
  4. **切 provider 后，前一个 provider 的 `extra_body` 桶不被下发**——这条直接对应 §2 那个 bug，必须有 vLLM→OpenRouter 之类的用例；
  5. `extra_body` 不出现在 trace 与 `RunState` 的可移植字段中。

---

### R1 实现注释：openai-agents-python 流式 / 非流式参考

可参考 `/Users/moses/workspace/custom-app/openai-agents-python` 的结构，但只借鉴边界拆分，不把 Python SDK 或 OpenAI Responses 的类型泄漏进 `ra-core`：

- 非流式结果：`src/agents/result.py` 里的 `RunResultBase` / `RunResult`。关键字段是 `input`、`new_items`、`raw_responses: list[ModelResponse]`、`final_output`、四类 guardrail 结果、`_last_processed_response`、`_previous_response_id` / `_conversation_id`。rusty-agent 对应为完成态 `RunResult`，用于 resume / approve / replay。
- 流式结果：`src/agents/result.py` 里的 `RunResultStreaming`。它继承同一组结果字段，但额外持有 `_event_queue`、guardrail 队列、后台 `run_loop_task`、`is_complete`、`stream_events()`。rusty-agent 对应为 `RunStream` / `RunResultStreaming`：消费者读事件，后台 loop 持续推进，结束后能物化为同一份完成态结果。
- 模型接口：`src/agents/models/interface.py` 把 `get_response(...) -> ModelResponse` 与 `stream_response(...) -> AsyncIterator[TResponseStreamEvent]` 分开；rusty-agent 也保留双入口，但 `ModelRequest` / `ModelResponse` / `StreamEvent` 必须是协议中立结构，由 provider adapter 负责 lowering / lifting。
- 事件分层：`src/agents/stream_events.py` 只定义三层事件，`src/agents/run_internal/streaming.py` 再把 `new_step_items` 映射成 `message_output_created`、`tool_called`、`tool_output`、`reasoning_item_created`、`mcp_*` 等语义事件。rusty-agent 应保持同样分层：raw provider event 归档，semantic run event 驱动 UI，session item 用于 replay。
- 流式终结：`src/agents/run_internal/run_loop.py` 的流式路径先逐个透传 `RawResponsesStreamEvent`，遇到 `ResponseCompletedEvent` 后再组装终态 `ModelResponse`，并补齐 usage / request_id / response_id。rusty-agent 的流式实现也必须有“终态 backfill”，不能只把 token delta 当作最终响应。

### 跨阶段实现注释：openai-agents-python 可借鉴结构

本节基于 **2026-08-06** 对 `/Users/moses/workspace/custom-app/openai-agents-python/src/agents` 的源码审阅。目标是借鉴它已经验证过的**边界拆分、状态流转和恢复契约**，不是把 Python dataclass、Pydantic 模型或 OpenAI SDK 类型逐个翻译成 Rust。

#### 1. 结构体与架构映射

| Python 源码结构 | 值得借鉴的边界 | rusty-agent 落点 | 不应照抄 |
| --- | --- | --- | --- |
| `AgentBase` / `Agent`、`ToolsToFinalOutputResult` | Agent 配置集中保存 `instructions`、model、settings、tools、handoffs、guardrails、hooks、output schema、tool-use 行为；`clone()` 用于生成轻量配置变体；`tool_use_behavior` 允许“工具结果直接成为最终输出” | `ra-core::AgentSpec`（**R3-1c**）+ `ra-runtime::AgentRunContext`；配置不可变，运行态单独存 `RunState`；R3-5 负责工具结果终止策略 | 不复制 Python 的浅拷贝/动态 callable 字段；Rust 用 builder、`Arc` 和显式 trait object，避免运行中隐式修改 Agent 配置 |
| `Agent.as_tool()` 与 `Handoff` | 两者必须是不同语义：`as_tool` 接收生成 input，子 agent 返回结果后父 agent 继续；handoff 过滤/传递历史并转移控制权 | `ra-runtime::NestedAgentTool` 与 `NextStep::Handoff` 分开；R12-2 优先实现 `as_tool` | 不把 handoff 简化成普通函数工具，否则会丢失控制权、历史和审批边界 |
| `HandoffInputData`、`HandoffInputFilter`、`nest_handoff_history()` | handoff 输入拆成 `input_history`、`pre_handoff_items`、`new_items`、可选 `input_items`；模型输入可以压缩/过滤，但 session history 仍保留完整项；nested history 还要保存 provenance ownership | R12-8（`ra-runtime` 内，不依赖 R17）+ R9 session item ownership；设计 `HandoffInput` / `HistoryProjection` / `OwnedHistoryRef` | 不把“给下一个 agent 的输入”直接覆盖成“整个会话历史”，避免回放重复、丢项和无法归因 |
| `RunItemBase[T]` 与 typed `RunItem` union | 每个 normalized item 同时保留产生它的 agent、provider raw item 和 `to_input_item()`；`ToolCallItem` 统一提供 `tool_name` / `call_id`；`ToolCallOutputItem` 还保存 SDK-only custom data | `ra-core::RunItem`、`RawProviderItem`、`InputItem` 三层；R1-1/R9-0 共同使用；provider raw payload 与 normalized item 分开落盘 | 不把 provider raw JSON 直接当公共事件，也不为了复刻 Python 弱引用而引入复杂生命周期技巧；Rust 用 `Arc`、稳定 id 和显式 ownership |
| `ModelResponse` | 统一保存 output、usage、response_id、request_id，并能转换成下一轮 input items；provider 不支持 response id 时可以为空 | `ra-core::ModelResponse` + `to_input_items()`；R1-3/R1-8/R1-10 | 不在 `ModelResponse` 中暴露 OpenAI SDK 的具体 output item 类型 |
| `Model` / `ModelProvider`、`ModelTracing` | 模型调用有非流式/流式双入口；provider 还能提供 retry advice、run 结束清理、全局 `aclose()`；`ModelTracing` 明确区分 disabled、含数据、只保留拓扑三种模式 | `ra-model::Model` / `ModelProvider`；R1-3/R1-9/R14-2；每次调用接收 `TraceMode`，provider 资源由 run owner 统一释放 | 不把某个 HTTP SDK client 直接暴露给 runtime，也不让“关闭敏感数据”变成“关闭 trace 拓扑” |
| `MultiProviderMap` / `MultiProvider` | 通过 `provider/model` 前缀解析模型；显式注册表优先于内建 fallback；未知前缀可 fail-fast，也可按配置把完整字符串交给 OpenAI-compatible endpoint；provider 缓存和统一关闭由 manager 管理 | `ra-model::ProviderRegistry` + `ModelSelector{provider, model, protocol}`；R1-3a/R1-18；`openai/gemini/anthropic/grok/compat` 都是注册项，不把厂商判断写进 runner | 不用字符串 `match` 散落在各 adapter；不把 `openai/xxx` 永久解释成唯一含义，必须保留 alias/model-id 两种模式 |
| `AnyLLMProvider` / `AnyLLMModel` / `LitellmProvider` | 聚合层只承担“路由到第三方模型”的适配职责，仍然向上收敛成统一 `Model`；它可以是可选扩展，不必污染核心 | `ra-model-bridge`（可选 feature）实现 `AnyLlmBridge` / `LiteLlmBridge`；R1-18 | 不把 LiteLLM/any-llm 作为 `ra-core` 的硬依赖；多运营商的核心语义、流式事件和重试仍由 rusty-agent 自己掌控 |
| `ProcessedResponse`、`ToolRun*`、`ToolExecutionPlan`、`SingleStepResult` | 把“模型响应分类”“工具执行计划”“工具执行”“turn 结算”拆成不同中间结构；工具执行计划按 handoff/function/shell/apply-patch/custom/MCP approval 分类，而不是在 runner 里连续 `if` | `ra-runtime::ProcessedResponse`、`ToolExecutionPlan`、`SingleStepResult`；R3-2/R3-3/R3-4 | 不让 `Runner` 直接同时做 provider 解码、审批、工具执行和最终输出解析 |
| `run_internal.items` 的 normalize / dedupe / orphan pruning | 发送给 provider 前统一清掉内部 metadata、孤儿 tool call 和悬空 reasoning；通过 fingerprint/digest 去重、rewind 和恢复后的 item 对账；拒绝结果也用协议项回灌模型 | `ra-core::InputItemNormalizer` + `ra-runtime::HistoryReconciler`；R1-17/R9-3 | 不把 session 中的任意 JSON 原样重新发送；不靠列表位置猜 call/output 配对，必须使用 `call_id`、类型和 digest |
| `run_internal.session_persistence` | 明确区分“本轮送模型的 prepared input”“本轮应追加到 session 的 input”“模型生成项”和“session_step_items”；callback 即使重排、删除或复制 history，也通过 identity + fingerprint/frequency 识别真正的新项；流式/恢复用 persisted count 幂等追加，retry rewind 只弹出精确匹配的尾部 suffix 并复查清理结果 | `ra-session::SessionInputPlan` / `PersistenceCursor` / `SessionReconciler`；R9-12，和 R1-17/R3-3 共用 digest、occurrence key、reasoning-id policy | 不把 callback 返回的完整数组直接再次持久化；不在 retry 时按“最后 N 条”盲删；provider conversation 的 id 清洗必须留在 adapter/session policy，不能污染通用 item |
| `FunctionTool`、`FunctionToolResult`、`ToolOrigin` | 工具定义不仅是 name/schema/invoke，还包含 strict schema、enabled、approval、timeout、defer loading、caller 限制、tool guardrail、failure handler；执行结果还要带 run item、interruptions、nested result | `ra-core::ToolSpec` / `ToolOrigin` / `ToolExecutionResult`；R2-1/R2-6/R2-7/R7-3/R12-2 | 不把 approval、timeout、失败格式化塞进每个工具实现；这些是统一执行器契约 |
| `FuncSchema` | 保存 callable 的名字、描述、参数 JSON schema、原始签名、context 注入标志、strict 标志和 return annotation；schema 生成与参数调用绑定在一起 | `ra-macros` 生成 `ToolSchema`，`ra-core` 保存 canonical schema/hash，`ra-runtime` 负责参数解码和 `ToolContext` 注入；R2-2 | 不照搬 Python docstring 运行时解析；Rust 优先编译期 derive，运行时只消费已版本化 schema |
| `AgentOutputSchemaBase` / `AgentOutputSchema` | plain text 与 structured JSON 共用一套输出抽象；schema、strictness、validate/parse 集中在 adapter；最终 output 不是随处 `serde_json::from_str` | `ra-core::OutputSchema` + `ra-model` provider lowering + `ra-runtime` validation；R1-16/R13-11 | 不让 provider adapter 自己决定最终业务类型，也不把结构化输出实现成一个额外工具 |
| `RunConfig`、`RunOptions`、`ModelSettings.resolve()`、`Usage` / `RequestUsage` | Agent 默认配置、run 级覆盖、单次模型调用过滤、工具执行并发、错误格式化、tracing 和 session 设置分层；usage 同时有聚合账本和逐请求明细 | `ra-core::RunConfig`、`ModelSettings`（**R1-2b**）、`RequestUsage`；R0-5/R1-8/R3-8/R4；override 用显式 merge，不修改 Agent 原对象 | 不把所有设置塞进 `Agent`，也不把 cached/reasoning tokens 只保留在总数中 |
| `ModelRetryNormalizedError`、`ModelRetryAdvice`、`RetryDecision`、`RetryPolicyContext` | provider 事实、provider 建议、应用层策略和最终 retry decision 分层；decision 可以携带 delay/reason，并有 hard veto 和 replay-safe approval；流式只允许在未发出不可重放事件前重试 | `ra-model::NormalizedProviderError` / `RetryAdvice` / `RetryDecision`；R1-9/R1-9b；usage 对每次失败尝试单独记账 | 不把“HTTP 5xx 就重试”当完整策略，也不在已经吐出 token/状态事件后透明重放 |
| `Prompt`、`GenerateDynamicPromptData`、`PromptUtil` | 静态托管 prompt 与动态 prompt 生成函数共用解析入口；动态生成可以读取 run context 和 agent，但最终 lowering 是 provider 专属字段 | `ra-prompt::PromptRef` / `DynamicPrompt`；R4-11；动态 prompt 进入 volatile 段并带 hash/provenance | 不让动态 prompt 随意改写稳定前缀，也不把 prompt provider 的 SDK 参数泄漏到通用 `ModelRequest` |
| `ToolOutputTrimmer` | 作为 model-call 前的 history projection：保护最近 N 个 user turn，只裁剪更老且超过预算的指定工具输出；按 `call_id` 还原 bare/qualified tool name；文本保留明确 preview，结构化输出只预览可读 text、丢弃 image/file 等 opaque payload 并标注；tool-search schema 只删 description/title/examples 等 prose，保留可调用结构 | `ra-context::ToolOutputTrimmer`，R10-6b 起实现 `ra-core::filter::ContextFilter`；R5-8/R10-6/R10-6b；只修改送模型的 projection，不修改 session 权威记录 | 不对最近编辑/验证结果做统一截断；不把 base64、图片或文件内容硬切成无效片段；不递归删除未知 JSON Schema 关键字 |
| `RunContextWrapper`、`ToolContext`、`AgentHookContext` | context 是给工具、guardrail、hook 和依赖注入使用的运行时对象，**不会发给模型**；`ToolContext` 额外提供 `CallId`、唯一 `ToolOrigin`（经 `lookup_key()` 取得路由/持久化身份）、arguments、public agent 与 run 读视图 | `ra-runtime::RunContext` / `ToolContext`；R3-9a/R6/R7；模型输入只能从显式 `ModelRequest` 构造 | 不把宿主依赖、审批记录或密钥放进 `ModelRequest`，避免上下文泄漏，也不按字符串工具名做控制流 |
| `RunState`、`RunResult.to_state()`、`RunResultStreaming` | `RunState` 是 pause/resume 的持久化边界；完成态与流式态共享结果字段；streaming wrapper 持有 event queue、run task、cancel mode、guardrail task，结束后可物化为同一完成态 | `ra-core::RunState`、`ra-runtime::runner::{RunResult, RunStream}`（**已落地**）；R3-7/R6-6/R9-9；流式与非流式共享 `SingleStepResult` | 不把 `RunStream` 做成只能消费一次的 token 管道；必须可观察终态、interruption、exception 和 persistence 状态 |
| `RunResultBase.to_input_list()`、`AgentToolInvocation`、weakref release | 结果对象同时支持展示、继续运行、转 `RunState` 和识别 nested agent invocation；`preserve_all` 与 `normalized` 两种 history view 要明确，释放 agent graph 不能破坏已持有的 item | `ra-runtime::runner::RunResult` + `ContinuationInput::{PreserveAll, Normalized}`（**已落地**，续跑输入是投影而不是第四个数组）；R3-7/R9-7/R12-2 | 不把“结果展示历史”“session 权威历史”“下一轮模型输入”混成一个数组，也不让资源释放依赖 GC 时机 |
| `agent_tool_state.py` 的 scoped nested-result registry | `as_tool` 的子 run 结果按 tool-call identity + scope 关联，并在 consume/drop/GC 时清理，避免 call_id 重用导致父子结果串线 | `NestedRunRef{scope_id, call_id, signature}` + `RunState` 中的持久化引用；R12-2/R12-3 | 不使用进程级无界全局 map；Rust 侧由父 run owner 管理，必要时使用 bounded registry |
| `AgentToolInput`、`StructuredInputSchemaInfo`、structured input builder | agent-as-tool 不只接受字符串；可以保存输入 schema 摘要、完整 JSON schema 和自定义 builder，并明确把结构化参数作为“数据”传给子 agent | `ra-runtime::NestedAgentInput` + `InputBuilder`；R12-2/R12-2b | 不把父 agent 的内部 context 直接序列化给子 agent；输入必须经过显式 schema/协议中立 item 转换 |
| `AgentToolUseTracker`、`AgentBindings` | 工具使用轨迹按稳定 agent identity 记录并可序列化恢复；public agent 与执行 agent 分离，便于 sandbox-prepared clone、审计和结果归属 | `ra-core::state::ToolUseTracker`（**已落地**；挂载点从 runtime 改到 core——`RunState` 属于 `ra-core`，而 `ra-core` 不能反向依赖 `ra-runtime`）/ `ra-runtime::agent::AgentBinding`（**已落地**）；R3-6b/R3-12/R6-6 | 不只按 agent name 保存状态，也不让执行期 clone 覆盖用户可见 agent identity |
| `GuardrailFunctionOutput`、四类 guardrail result、tool guardrail behavior | guardrail 结果既有可审计 `output_info`，又有明确控制行为；输入 guardrail 可和首个模型调用并行，工具 input guardrail 在 approval 前可预检但批准后还需重跑 | `ra-core::guardrail::GuardrailFunctionOutput` + `InputGuardrailResult` / `OutputGuardrailResult`（**已落地**，R7-1；上游的单一 `GuardrailResult` 拆成两类，两端产出、两处存放，一个带 stage 标签的类型会把「把输出结论记进输入那一栏」从编译错误降级成运行期错误）/ `ToolGuardrailOutcome`；R7-1/R7-3/R14-4 | 不把 guardrail 结果压成 bool，也不把“拒绝内容后继续”误做成异常终止 |
| `RunHooksBase` 与 `AgentHooksBase` | 一套 hook 面向整个 run，另一套只面向某个 agent；两者共享 llm/tool/handoff 生命周期，但作用域和回调参数不同 | `ra-runtime::RunHooks` / `AgentHooks`；R3-9，和 R7-4 的 `UserHook` 保持独立 | 生命周期观察者不参与审批或续跑决定；决定型 UserHook 按 R7-4 的显式契约接入运行循环 |
| `Session` / `SessionABC` / `SessionSettings` | session 最小接口只负责 `get_items(limit)` / `add_items` / `pop_item` / `clear_session`；`limit` 是读取投影参数，不是物理删除；context-aware session 通过显式可选 wrapper opt-in，不破坏已有实现 | `ra-session::SessionStore` + `SessionReadOptions`；R9-2/R9-12 | 不把 session 做成 runner 状态对象，也不要求所有外部 store 理解 `RunContext` |
| `SQLiteSession` | 本地 session store 使用 session/message 两表、递增 id 保序、JSON item 存储、`session_id,id` 索引；文件库用进程内共享 lock + thread-local connection，内存库用共享连接，读 limit 返回最新 N 条但保持时间正序 | `ra-session::SqliteSessionStore`；R9-1/R9-11 | 不把 SQLite 的内部自增 id 当跨 store item id；不让多个 connection 并发写破坏顺序 |
| `OpenAIConversationsSession` | provider 服务端 conversation 是一种 session backend：lazy 创建 conversation id、远端 list/create/delete items、limit 读取、pop 删除远端最后一项；它需要 provider 特定 id 清洗和不可持久项过滤 | `ra-model::openai::OpenAiConversationSession`（模块，不是独立 crate）+ `ra-session::ServerBackedSession`；R9-13/R1-17 | 不把 OpenAI Conversations 当作通用 session 语义；`conversation_id` 不能进入 `ra-core::RunState` 的可移植字段 |
| `OpenAIResponsesCompactionAwareSession` / `OpenAIResponsesCompactionSession` | Responses compaction 是 optional protocol：可按 candidate items、response_id、store 标志和 mode 选择 `previous_response_id` 或本地 `input`；遇到本地 tool output 时要 defer compaction，避免服务端历史缺少本地工具观察 | `ra-session::CompactionSession` + provider-specific compactor；R9-14/R5-3 | 不要求每个 session 都实现 compaction；不把 provider `responses.compact` 当通用压缩算法，也不能用它替代本地 archive |
| `MCPServer` 与 `MCPServerManager` | server 定义 connect/cleanup/tools/prompts/resources；manager 为每个 server 建 worker，使 connect/cleanup 保持在同一任务上下文，独立 timeout、失败记录、并行 connect、failed-only reconnect、反向 cleanup 和 active/failed server 视图 | `ra-mcp::McpServer` / `McpServerManager`；R11-1/R11-6 | 不让一个坏 MCP server 阻塞整个 agent run，也不把 server lifecycle 隐藏在单次 tool call 里 |
| `MCPServerStdio` / `MCPServerSse` / `MCPServerStreamableHttp` 及 Params | 三种 transport 共享 `_MCPServerWithClientSession` 的 session、请求串行化、工具缓存、动态 filter、approval、retry、message handler 和 cleanup；差异只留在 `create_streams()` 与 transport config。HTTP transport 还要处理 auth/client factory、session id、terminate-on-close 和 initialized notification 兼容 | `McpTransportConfig::{Stdio,Sse,StreamableHttp}` + 共享 `ClientSessionServer`；R11-1/R11-2/R11-4；transport error 进入凭据脱敏后的统一错误 | 不为三种 transport 复制 list/call/cache/approval 逻辑；不把具体 HTTP client auth 类型暴露到 `ra-core`；错误日志不能泄露 header、URL credential 或 token |
| `SpanData`、`Trace` / `Span`、`TraceState`、`TracingProcessor` | Agent/Task/Turn/Generation/Response/Function/Handoff/Guardrail/MCP 各有 span data；trace/span 可嵌套、可标记 error、可 flush；processor 可替换且支持敏感数据开关 | `ra-eval` 或独立 tracing 模块；R0-3/R14-2；trace 结构与 R9 rollout 事件互相引用 | 不默认绑定 OpenAI tracing 后端；也不因关闭敏感数据而丢失 span 拓扑 |
| `RunErrorData`、`RunErrorHandlerInput`、`RunErrorHandlerResult` | max turns、model refusal、invalid final output 进入统一错误处理器；handler 能读取 run snapshot、生成 final output，并决定是否写入 history | `ra-runtime::RunError` / `ErrorHandler`；R3-8/R15 | 不用异常 message 做恢复路由，不把所有错误都重试或都转成成功 |
| **`run_internal/tool_execution.py` 前 550 行**（2026-08-06 补充审阅） | **并发工具任务的结算与取消是独立难题**：多个并行工具同时失败要按优先级选一个报告（`_select_function_tool_failure`）；已结算后迟到的失败要合并（`_merge_late_function_tool_failure`）；取消后仍在跑的任务要排空而非直接 drop（`_drain_cancelled_function_tool_tasks`）；清理任务自身抛异常也要被记录（`_background_cleanup_task_exception_message`）；结算与收集分离，避免半结算态 | `ra-runtime::turn::batch`；**R3-4c**（新增），并被 R17-4 的图级并行复用 | 不用 `join_all` 一把梭；不把「第一个失败」当作「要报告的失败」；不在取消时直接 drop `JoinHandle`——Rust 里这会泄漏正在跑的子进程 |
| **`exceptions.py`**（12 个类 + `RunErrorDetails`） | 异常按**可恢复性**分类：`MaxTurnsExceeded` / `ModelBehaviorError` / `ModelRefusalError` / `UserError` / `MCPToolCancellationError` / `ToolTimeoutError` + 四类 guardrail tripwire；错误对象携带 run 快照 | `ra-core::Error` 的**第二维度**；R0-2 + R3-1b。与 R0-2 的子系统维度（Config/Provider/Tool/Sandbox/…）**正交，两套都要** | 不把可恢复性判断塞进 message 字符串；不让 `is_retryable()` 成为唯一的恢复语义 |
| **`run_internal/turn_preparation.py`** | 每轮开始前的解析有**固定顺序且顺序有语义**：`get_all_tools` → `get_handoffs` → `get_output_schema` → `get_model` → `get_model_settings`（agent 隐式 → 名字解析 → run_config 覆盖三层 merge）→ `maybe_filter_model_input` | `ra-runtime::turn::prepare`；**R3-0**（新增） | 不让动态 `is_enabled` 跑在 input filter 之后；不让 `model_settings` 在 tools 之前定型——`tool_choice` 依赖有没有工具 |
| **`run_internal/run_grouping.py`** | 多次 run 归到一个 group（默认取 session id），trace 才能跨 run 聚合 | `ra-eval::RunGroupId`；**R14-2b**（新增）。**R17 直接依赖**：一张图 = N 次 `Runner::run`，没有 group id 就聚合不成一张图 | 不用 `trace_id` 兼做分组键——单次 run 内可以有多个 trace |
| **`extensions/visualization.py`** | agent/handoff 拓扑导出 graphviz DOT，建图后先看一眼 | `ra-flow::export::dot`；**R17-9** | 不引入 graphviz 运行时依赖，只输出 DOT 文本由使用方渲染 |
| **`_mcp_tool_metadata.py`** | MCP 工具的 title/description 有多个来源（`annotations.title` / `title` / `name`）要按优先级解析；**「给模型看的 description」与「给 UI 看的」是两个** | `ra-mcp::metadata`；**R11-12**（新增） | 不把 UI 文案直接塞进工具 schema，那会白占 token 预算 |
| **`extensions/memory/encrypt_session.py`** | 加密是**包在 SessionStore 外面的 wrapper**，不是每个 store 自己实现 | `ra-session::SessionWrapper` 链；R9-2 | 不让每个 store 实现各自的加密/压缩/审计 |

> ⚠️ **命名陷阱：openai-agents-python 的 `memory/` 不是记忆，是会话存储。** `src/agents/memory/` 里是 `session.py` / `sqlite_session.py` / `openai_conversations_session.py`；`extensions/memory/` 里是 redis / mongodb / sqlalchemy / dapr / encrypt 等 **session 后端**（合计 5,246 行；文档旧值 4,151 是过期快照）。它对应 rusty-agent 的 **`ra-session`**，与 RAG / 长期记忆无关。rusty-agent 的记忆语义见 [R10-8](#r10-开发顺序)，两者不要混。

#### 2. 建议的 rusty-agent 分层

把上面的借鉴项落到现有 crate 时，保持下面的依赖方向：

1. `ra-core` 只放协议中立、可序列化的值对象：`RunItem`、`ModelRequest`、`ModelResponse`、`ToolSpec`、`ToolOrigin`、`OutputSchema`、`GuardrailResult`、`RunState`、`RequestUsage`。
2. `ra-model` 负责 provider lowering/lifting：把 `ModelRequest` 转成 Responses / Chat / Anthropic / compat payload，再把 raw response 转回 normalized item 和终态 `ModelResponse`。不能把 provider 字段反向泄漏给 `ra-runtime`。
3. `ra-runtime` 只处理 `ProcessedResponse -> ToolExecutionPlan -> SingleStepResult -> NextStep`，并为 streaming wrapper 提供 event queue、取消和终态 backfill。
4. `ra-session` 负责 session item 与 rollout/event 的持久化；`RunState` 是可恢复运行态，不能用 session transcript 代替。
5. `ra-mcp` 负责 server 生命周期、连接隔离和工具发现；`ra-runtime` 只消费 normalized MCP tool/action。
6. `ra-eval` / tracing 模块负责 Trace/Span/Processor 和 replay 断言；R9 的 raw event、semantic event、session item 三层事件可以引用同一个 `trace_id` / `span_id`。

#### 3. 多运营商 Provider 实现结论

结论不是“全部自己造”或“完全交给一个聚合包”，而是采用**框架契约自有、provider adapter 自有、底层通用库复用、聚合桥可选**的分层方案：

| 层级 | 实现策略 | 设计结论 |
| --- | --- | --- |
| 公共模型契约 | rusty-agent 自己实现 | `ModelRequest` / `ModelResponse` / `StreamEvent` / `RunItem` / `Usage` / `RetryDecision` 必须由 rusty-agent 定义，保证 loop、session、replay 和 UI 不绑定任何厂商 SDK |
| Provider 注册与配置 | rusty-agent 自己实现 | `ProviderRegistry + ModelSelector + ProviderCapabilities + ProviderQuirks` 管理 `provider/model`、凭据、base URL、默认 header、模型别名和协议能力 |
| 核心 provider adapter | rusty-agent 自己维护 | OpenAI、Gemini、Anthropic/Claude、xAI/Grok 和 compat 都作为一等注册项；“自己维护”指掌握 lowering/lifting、SSE 事件、错误、usage 和 retry 语义，不代表每家都复制一套 HTTP 代码 |
| 协议 codec 复用 | 在 rusty-agent 内共享 | OpenAI Responses、OpenAI Chat、Anthropic Messages、Gemini native 各有 codec；Grok 和其他兼容厂商优先复用 Chat/Responses codec，再通过 `ProviderQuirks` 表达字段差异。只有出现不可表达的原生语义时才新增专用 codec |
| 底层 crates / 官方 SDK | 可复用，但封在 adapter 内 | HTTP、TLS、JSON、SSE、backoff 等使用成熟 crate；若某厂商 Rust SDK 足够合适，也只能作为 adapter 私有实现细节。公共 trait、事件和持久化格式不能暴露 SDK 类型，替换 SDK 不应影响 `ra-runtime` |
| LiteLLM / any-llm | 可选 bridge | 用于长尾模型、迁移验证和快速 smoke；默认 feature 不依赖，不能成为 provider 能力判定、重试、事件归一化或 session replay 的权威实现 |

推荐调用链固定为：`ProviderRegistry -> ProviderAdapter -> ProtocolCodec -> HttpTransport`。其中 `ProviderAdapter` 处理身份、凭据、endpoint、模型映射、capability/quirk 和 retry advice；`ProtocolCodec` 只处理协议 payload 与流式事件；`HttpTransport` 只处理连接、超时、取消和字节流。这样新增 OpenAI-compatible 厂商通常只增加一份 provider 配置和 quirks，不改 loop，也不复制 Chat/Responses parser。

#### 4. 本阶段明确不吸收的内容

- **不直接依赖 Python SDK 或 Pydantic**：只吸收字段语义和生命周期，Rust schema 由 `serde` / `schemars` / `ra-macros` 管理。
- **不把 Responses API 当公共类型系统**：`ResponseFunctionToolCall`、`ResponseReasoningItem`、`previous_response_id` 等只能出现在 provider adapter 或兼容层。
- **不复制 voice/realtime、hosted tools、computer、sandbox 全量实现**：这些属于独立产品面，当前 roadmap 只保留必要的扩展点，避免 R1-R9 的 loop 拓扑膨胀。
- **不复制全局 contextvars、弱引用和隐式 GC 清理**：Rust 用 owner、scope、stable id、bounded registry 表达同样的生命周期约束。
- **不把所有 Python 公共类都变成 Rust 公共 API**：优先稳定 `RunItem`、`ModelResponse`、`RunState`、`ToolSpec`、`NextStep`、`StreamEvent` 六个跨 provider/runtime 边界。

### R1 非目标

| 项目 | 处理 |
| --- | --- |
| effort / 模型自动路由 | 不做；用户显式选择 |
| 把 Chat 当 Responses 的降级兼容层 | **不做**；两套协议语义不同，平级实现。把 Chat 做成附属品会导致 `ra-runtime` 悄悄依赖 Responses 语义 |
| 把 LiteLLM / any-llm 作为核心依赖 | 不做；只允许 R1-18 的可选 bridge，默认 provider 路径仍由 rusty-agent 自己维护 |
| 服务端托管工具（FileSearch / CodeInterpreter / ImageGeneration） | 不做；rusty-agent 用自己的本地工具 |

### R1 验收标准

| 能力 | 标准 |
| --- | --- |
| 四种协议可跑 | 同一个 agent 在 Responses / Chat / Anthropic / compat 四条路径上各完成一次工具调用往返 |
| 协议中立 | `ra-runtime` 的代码里搜不到 `previous_response_id` / `tool_calls` 这类协议专有字段 |
| 往返一致 | 同一段历史在 Responses↔Chat 间互转后工具调用配对不丢；reasoning 降级为 `reasoning_content` 有明确记录 |
| 缓存可测 | usage 明细能算出单次 run 的 cache hit rate；Anthropic 侧断点生效可在响应中看到 `cache_read_input_tokens` |
| 重试可信 | 流式中断后不重放已吐 token 的请求；`retry_after` 被尊重 |

---

## R2 工具体系

### R2 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R2-1 | `Tool` trait 与工具身份键 | **DONE** | `ra-core::tool::Tool` 已落地为对象安全扩展点，最小 required 集只有 `origin` / `schema` / `call`；动态启用、动态审批、自定义失败处理均有 fail-fast 默认方法，第三方实现可按需覆盖。`ToolOrigin` 保存 `namespace` / 诊断用 `qualified_name` / 路由用 `ToolLookupKey`，lookup key 用私有字段结构体强制经过校验并区分 bare / namespaced / deferred-top-level 三种形状；provider 的同名 synthetic namespace 可恢复成 deferred key，显式同名 namespace 则拒绝，避免歧义。`ToolNamespace` 是开放 newtype，两个 MCP server 的同名工具可在同一 `BTreeMap<ToolLookupKey, _>` 中独立反查；origin/key/namespace 的反序列化均重验不变量，origin 带 schema version 与未知字段回写。`ToolSchema` 持有 strict JSON schema 并投影为 `ModelToolDefinition`；`ToolOptions` 集中声明 enabled / approval / **exposure** / **concurrency** / caller allowlist / 毫秒 timeout 与行为 / 输入输出 guardrail ID / failure handling，运行期 callback 不进入可持久配置。<br>**后两个字段是对照本机 Codex 源码（`070a26a1f0`）补的，趁 `ra-tools` 里只有 `read_file` 一个真工具时改，成本最低**：<br>· `ToolExposure::{Advertised, Deferred, Hidden}` 取代原来的 `defer_loading: bool`。布尔答不了三个状态里的第三个——「注册且可派发、但既不广播也不该被 `tool_search` 索引」是宿主内部工具的常态，而 `is_discoverable()` 若写成 `!is_advertised()` 就会把它们全都索引出去（两个投影因此各自 `match`，各自点名消费者：R3-0 准备阶段与 R2-5c）。**没有照抄 Codex 的六态**：它把「可见性 × 面（模型 / 嵌套 code-mode）」两条轴交叉进一个枚举，而本项目的第二条轴已经是 `allowed_callers`，再交叉一次会造出「只暴露给 programmatic 面、而 allowlist 只许 Direct」这种既无意义也不报错的组合。一条轴一个字段。<br>· `ToolConcurrency::{Exclusive, Parallel}`，默认 `Exclusive`，由 R3-4b 的批执行读取。**默认必须是保守那一边**：反过来会让这个字段出现之前写的每个工具静默变成可并发。`read_file` 声明 `Parallel`（读不锁任何东西），实证依据是 Codex 的 `exec_command` / `shell_command` 声明并行、`apply_patch` 不声明。<br>**旧键 `defer_loading` 在反序列化时当场拒收**而不是落进 `Unknown`：认不出的键会原样回写、工具则读成 `Advertised`，也就是每轮白付 schema 预算且没有任何东西报错。`ToolInvocation` 隔离 call ID、参数、caller 与可 downcast host context，Debug 不打印参数。9 条 `it-core` 契约测试覆盖同名路由、三类恢复、损坏状态拒绝、未知字段、完整 metadata、trait object、上下文、动态 policy 与 origin/schema 一致性。`ToolOutput::Text` 仅作为稳定 `call` 返回壳提前落位；Image/FileContent 和观察元数据仍归 R2-3。<br>**审查修正**：`ToolSchema` 原本 `new()` 默认 `strict=true` 却从不校验 strict 不变量，手写与 MCP 路径（正是 R11 要走的那条）会静默产出 provider 必拒的请求，`validate()` 也查不出来。现在 `new()` = 严格构造并校验，`loose()` 显式退出 strict，`with_strict_json_schema` 删除——让「声明 strict 但不满足 strict」这个状态无法表示；`validate()` 与反序列化同样重验。另在 OpenAI adapter 边界补上 `^[A-Za-z0-9_-]{1,64}$` 工具名校验（字符集是 provider 事实，`ra-core` 继续保持开放）。11 条 `it-core` 契约测试。 |
| R2-2 | schema 派生宏 | **DONE** | `ra-macros::ToolInput` derive 已落地：只接受 struct，读取类型 doc comment 作为工具 description，并支持 `#[tool_input(strict = false, description = "...")]`；字段 doc 由同一次 `schemars::JsonSchema` derive 进入 property description，泛型输入通过隐藏 prerequisite trait 补齐边界。`ra-core::tool::ToolInput` 把具体 `DeserializeOwned + JsonSchema` 类型同时绑定到 schema 生成与 decoder；`FuncSchema` 保存版本、`ToolSchema`、输入类型、类型化 decoder 函数指针、是否注入 `ToolContext` 及 return type，运行期参数先按同一 schema 校验 required / unknown / nested shape，再做 typed deserialize，避免 strict schema 与 Serde 对缺失 Option、额外字段的默认行为漂移。strict 规范化递归覆盖 definitions / `$defs` / properties / arrays / anyOf / oneOf / allOf / 本地 ref：对象强制 `additionalProperties:false`、所有 property 进入排序后的 required，`Option<T>` 仍保留 nullable union，根必须是非 nullable object，并有 100k node 防病态展开预算。生成后移除根部重复 `$schema` / title / description，递归按 key canonicalize；`ToolSchema` 保存 SHA-256，反序列化验证 hash，旧记录缺 hash 时安全重算。9 条 `it-macros` 契约测试覆盖文档、strict/loose、nullable、嵌套 schema、schema-bound 解码、context/return metadata、hash 防篡改、泛型、连续 100 次字节稳定、自引用类型与元组。<br>**审查修正三处**：① `$ref` 带兄弟键的内联展开没有环检测，自引用输入类型（`Box<Self>` 且字段带 doc）会**撑爆调用栈直接 SIGABRT**——`MAX_SCHEMA_NODES` 拦不住，每层只耗几个 node 却吃掉一串栈帧；现在按 ref 路径检测环并报错。② 元组字段（schemars 生成数组形式 `items`）在规范化与运行期校验里都被当成单个 schema，报一个指不到字段的错；现在按位置逐项处理，`prefixItems` 一并支持。③ 内联后不清理 `definitions`，每个带文档的嵌套字段都让该类型在 schema 里出现两份，白白吃掉 R2-10 的 20KB 预算与缓存前缀；现在从 schema 主体做可达性标记后裁剪。另修：运行期校验命中 `anyOf` 后早返回，会跳过同级 `type`/`properties`/`required`。R2-9 的 schema-stability 门禁已启用，覆盖真实 advertise 集。 |
| R2-3 | 工具结果结构化 | **DONE** | 落地形态：`ra-core::tool::output` —— `ToolOutput{blocks, metadata}` + `ToolOutputBlock::{Text, Image, File}` + `ObservationMetadata{truncations, guidance}` + `Truncation{stage, original_bytes, retained_bytes}`；`ra-core::item::content` 补齐 `ImageSource::{Url, ProviderFile}`、`ImageDetail`、`FileSource` / `FileBlock`，与 `openai-agents` 的 `ToolOutputText/Image/FileContent` 字段对齐。<br>**结构给宿主、散文给模型，这是本条的骨架**：`metadata` 是**类型化的且一直保持类型化**（R5-1 预算、UI、rollout 都要按数据读它），只有 `ToolOutput::model_blocks()` 在交给 provider 的那一刻把它渲染成一个前置文本块。于是「哪些事实值这些 token」变成**渲染决策而不是 schema 决策**，改它不用迁移任何已存记录。两个互相独立的实现给出同一形状：Codex rollout 把 `Wall time 1.0 seconds` 放在正文之前一个单独的 `input_text` 块里；`openai-agents` 的 `ToolOutputTrimmer` 把 `[Trimmed: … 12345 chars → 200 char preview]` 写进它替换掉的文本。**两边的线格式上都没有 metadata 字段。**<br>**截断是列表不是字段**：一条结果可以被截两次——工具撞自己的输出上限，R5-1 又按上下文预算裁一刀。只留一格会让第二次静默抹掉第一次，模型被告知的损失比实际的小。`stage` 分 `Tool` / `ContextBudget` 两种，因为对模型意味着不同的事：前者值得换个更窄的查询，后者不是换查询能避免的。单位用**字节**——它是每个阶段都能测且不必先就编码或 tokenizer 达成一致的那个量。<br>**空结果当场拒收**：没被回答的调用会让历史畸形，而 provider 要到下一次请求才拒。`openai-agents` 从另一头撞过同一格——`all([])` 是 `True`，空的结构化列表通过了转换检查、整条结果被静默丢掉，它现在也显式挡了这一格。<br>**刻意没有自由 map**：跨版本增长已经由 `schema_version` + `Unknown` 覆盖；再加一个开放袋子只会多出「本版本不声明就塞语义」的位置，两个代价这个项目都付过——没有任何东西记录语义变了，而无类型袋子会招来 `meta["truncated"]` 这种读文本的控制流（去词表化那条禁的正是它）。而且**这里的成本不对称与 R3-13 相反**：这些结构是 `#[non_exhaustive]` + 私有字段，以后加类型化字段是免费的。`grep` 的扫描/跳过统计与 `exec_command` 的耗时/退出码同理，跟着各自的工具在 R8-6 / R8-1 以类型化字段到位。<br>**适配器侧一并落地**：`function_call_output.output` 从「一个字符串」改成**内容块数组**（`input_text` / `input_image` / `input_file`），认不出的宿主载荷仍按字符串下发——R2-3 之前存下的裸值在 resume 时照样重放得了。<br>**读一条存下来的载荷是三态而不是两态**：`ToolOutput::from_stored` 返回 `Ok(Some)`（读得了）/ `Ok(None)`（根本不是工具结果，字符串回放）/ `Err`（**声称是但读不了**，例如新版本写了本版本没有的 block kind）。把后两者合并成「没解析成功」，会让新版本写下的记录被静默字符串化成 JSON 塞进模型上下文——正是这个结构要终结的那件事。<br>**认领凭据是「我们自己的版本标记 + `blocks`」，不是 `blocks` 一个键**：`blocks` 是常见英文词也是常见 JSON 键（Slack Block Kit 的返回就是 `{"blocks":[{"type":"section"}]}`），只按它认领会把宿主自己的工具结果抢过来、读失败，而「读不了」已经是硬错误——那条 session 直接重放不了。`schema_version` 是框架自己的记号，本版本写的每一条都带它。有一条测试专门钉住「自己序列化出来的一定认得回来」，因为整条规则压在这个不变量上。<br>**判别是手写的，不是 `#[serde(untagged)]`**：untagged 会把每一种失败都答成 `data did not match any variant of untagged enum ToolOutputWire`——真正的原因（`unknown variant \`bogus\`, expected one of \`text\`, \`image\`, \`file\``）连同产生它的那次尝试一起被丢掉。这些记录是在 resume 与 rollout replay 里读的，问的正是「几千条里哪一条读不了、怎么坏的」。做法与 `AgentToolUse` 手写 `Deserialize` 同源。图片降级抽成 `lower_image` 一处，消息内容与工具结果共用，否则同一张 base64 图会在消息里成功、在工具结果里失败。<br>**`custom_tool_call_output` 仍然没有**：freeform 工具（apply_patch）走的是另一个 payload 类型，而它由**被回答的那个 call** 决定、不该标在结果上；缺口留给 R8-4 与 R1-5b。工具失败的成型（`failure_error_function`）属于 R2-7，本条不碰。<br>**验收**（`tests/it-core/tests/tool_output.rs` 16 条 + `tests/it-model/tests/openai_responses.rs` 4 条）：空结果当场拒收；文本投影只认单块、多块返回 `None` 而不是拼接丢图；无截断无建议时**一个 token 都不占**且 `model_blocks` 与 `blocks` 相等；两次截断按序累加、结算后的阶段能追加；渲染只发生在下发那一刻、排在正文之前、正文那份不变；`stage` 的线值与 `label` 一致；四种图片来源 × 三种文件来源往返、没要精度就不写 `detail`；三层未知字段原样回写。反序列化同样过不了空块这一关（校验只写在构造函数里就等于没写——checkpoint 与 rollout 走的正是反序列化）；R2-1 存的 `{"type":"text"}` 升级成新结构；四种坏法各自点名而不是只说「都不匹配」；三态分类各走一遍；自己序列化出来的一定认得回来。adapter 侧：结构化结果下发成四个内容块且元数据块在最前、图片带 `file_id` + `detail`、文件带 `file_url`；非结构化宿主载荷仍按字符串下发；R2-1 的文本结果续跑时按原文下发；**读不了的记录当场失败且一个请求都没发出去** |
| R2-4 | 工具注册表与 profile | **DONE** | 落地形态：`ra-runtime::tool::registry::{ToolRegistry, ToolRegistryBuilder}` 持有宿主装的**全部**工具（按 `ToolLookupKey` 排序），`ra-runtime::tool::profile::{ToolProfile, ToolProfileId, ToolSelection, ToolSurfaceBudget, ToolSurface}` 是「选哪些 + 允许多大」，`registry.assemble(&profile)` 单向产出 `ToolSurface`，它的 `into_tools()` 就是 `AgentSpec::builder().tools()` 的入参。<br>**数字在产品里，机制在内核里**：`core(6-8)` / `codex_like(14-16, 默认)` / `full(≤24)` 三档连同 20 KB 预算落在 `ra-coding::CodingProfile::to_tool_profile()`，`ra-runtime` 一个都不认识。两条理由各自独立成立：① `ra-runtime` 的 crate 文档写死「公开 API 不含业务词汇」，把 `codex_like` 这个**厂商产品名**放进 loop 内核的公开面比业务词汇更糟；② 本表的口径提醒已经写明这些数字是 coding profile 的策略——R18-1 的只读 profile 与图编排 profile 各有预算，内核把 24 焊死等于挡住它们。内核提供的是「选择 + 预算 + 装配」，产品提供数字。<br>**注册表只保证 lookup key 唯一，模型面名字冲突留到 assemble 才报**。两个 MCP server 各出一个 `search` 是正常安装，R2-1 的三种 key 形状正是为它留的；只有**同时 advertise 两个**才是 provider 必拒的请求。在 assemble 处报还有一个具体好处：错误能同时点出 profile 名与两个 lookup key，而 `AgentSpec::build` 那条只拿得到名字。R2-6 的 `Warn` / 重命名策略因此有**一个**插入点。<br>**名字唯一性只管得着「进得了 tool list 的那些」**（审核补正）：初版两处检查——`assemble` 与 `AgentSpec::build`——都对**全部**声明工具查重名，而准备阶段只投射 enabled ∧ advertised 的那些，于是一个 `Hidden` 的宿主内部工具跟某个 MCP server 都叫 `search` 就建不出来了，而这正是 R2-1 写明的常态配置，且 provider 永远看不到这个冲突。判据收进 `ra-core` 的 `ToolOptions::can_reach_model_surface()`，两处读同一条规则：`Hidden` 不占名字（永远不在任何 tool list 里）；`Deferred` **占**（它是「discovery 可以把它提升进 advertised 集」的承诺，等提升那一刻才发现撞名的话，恰好是没有任何检查在跑的时刻）；`Disabled` 不占（静态关掉，不改配置就上不了台）；`Dynamic` 占（回调怎么答不能决定一个请求合不合法）。两个 `match` 都写全变体而不写否定式，将来加 exposure 变体是这里的编译错误，而不是静默变成「不占名字」。**预算与名字是两个判据**：`Deferred` 占名字但不占本轮 slot，所以计费仍走 `is_advertised() && can_reach_model_surface()`。<br>**预算是必填的**：`ToolProfileBuilder::build()` 拒绝没声明预算的 profile。工具面只会长，每一次增加单独看都合理、没有任何东西失败，代价是每轮账单大一点——AF 的 43-44 个工具面就是这个过程的终点。「不设上限」仍然可表达，但得写成 `usize::MAX`，也就是得有人**特意写下**它。<br>**预算有下限不只有上限**：掉了工具的工具面不会失败，run 照跑，而 prompt 还在讲那个不存在的工具——正是 R10-4 要禁的「砍了工具提示还在说用它」。下限是 profile 说明「哪几个是重点」的方式。<br>**只数会被广播的那些**：`Hidden` / `Deferred` / `Disabled` 的条目照样进 surface、照样可派发，但不计数也不计字节——这正是「advertise ≠ 拥有」能省下钱的地方。`Dynamic` **计入**，因为预算描述的是 profile 声明的工具面，不是某一轮恰好发出去的那个。<br>**字节口径收成一份**：新增 `ra-core::tool::ToolSchema::advertised_bytes()`（input schema + name + description，不含各 provider 的信封——信封按 wire 格式变，算进去同一个工具换个 endpoint 就换个数）。`read_file` 那条 900 B 上限断言改用同一个函数：两份「一条目值多少字节」的定义会漂移，然后 surface 过了而它的条目没过。<br>**`full` 是 `AllRegistered` 而不是列 24 个名字**：MCP server 的导出在 profile 写下来的时候还不知道，只能枚举的 profile 永远包不进它们；预算是让「全都要」仍然是个**有界**陈述的那个东西。<br>**顺序按 lookup key 排，与注册顺序无关**：advertised table 是缓存前缀的一部分，两个宿主启动顺序不同就得出不同字节的话，缓存永远不命中。<br>**三档清单**：`core` 六个（`exec_command` / `write_stdin` / `apply_patch` / `read_file` / `grep` / `glob`）——`grep` / `glob` 不是 `exec_command` 的便利封装，shell 出去拿回的是裸文本，上下文预算与离线 eval 都读不了；`codex_like` 十五个（= R2-8 的 advertise 集）；`full` = 全注册 ≤ 24。**顺带改掉 R0 时写的桩注释**：原来 `Core` 写的是 4 个、`CodexLike` 写的是 7 个，与本表 6-8 / 14-16 的口径对不上。<br>**`agent` / `mcp` 是 bare key 的单条目**，不是 namespace：namespaced key 在模型面投影成子工具的名字，那是 5 个槽不是 1 个。内部怎么路由归 R2-5b，占几个槽不归它。<br>**今天装配不出来，这是刻意的**：交付时 15 个里只有 `read_file` 与 `exec_command` 是真工具，`assemble` 点名 `apply_patch` 失败；R8-4 之后 `apply_patch` 已落地，`core` 还缺 `write_stdin` / `grep` / `glob` 三个，现在点名 `glob`。**清单即规格**——装配「现有的那些」会让产品带着比 prompt 描述更小的工具面上线，且哪里都不报错。因此 `build_agent` 本条不接 profile，等这三个补齐再接（**已补齐并接上**：`write_stdin` / `grep` / `glob` 落地后 `core` 可装配，R10-4 让 `build_agent_with_profile` 走注册表 + profile，`build_agent_with_host` 委托到 `core`）。<br>**复查修正三处**：① 名字与字节两个判据原本都读 `ToolSchema`，而 `AgentSpec::build` 读的是 `Tool::model_definition()`——后者可被覆盖，而 R2-6 的重命名策略恰好会让两处判不同的名字，于是同一个工具面在这里合法、在 `AgentSpec` 非法（或反过来，一对本来合法的同 schema 名工具在这里被拒）。现在 `assemble` 每个条目只取一次 `model_definition()`，占名字、查重名、算字节三件事都读它；`ToolSchema::advertised_bytes()` 改为委托 `ModelToolDefinition::advertised_bytes()`，口径仍是一份，但量的是真发出去的那份投影。`ToolSurface` 顺带改成记下装配当时的 advertised 名单，而不是事后按 schema 名重算——R10-4 的提示片段对账拿到的必须是被计费的那批名字。② `ToolSurfaceBudget` 直接 derive `Deserialize`，绕过了 `new()` 的 floor ≤ ceiling 校验：配置里写反的预算造得出来，然后每次 assemble 都失败在上限或下限，错误指着工具面而不是那两个数。现在经 `DeclaredBudget` 走同一个构造函数，与同文件 `ToolProfileId` 的手写 `Deserialize` 一致。③ **只改 `assemble` 反而打开一个洞**：装配放行了重命名工具，而 `TurnActionSurface::find_tool()` / `advertised_names()`、`ProcessedResponseBuilder::function()` 的绑定校验、`ToolApproval` 的记录仍按 `origin().name()` 走——模型调 `jira_search` 会解析不到、或绑到错的实现、或让人去批一个模型从没用过的名字。现在这六处模型面统一读 `Tool::model_definition()`，`TurnActionSurface` 与 `ToolSurface` 一样在构造时记下投影名单而不是每次重算；`Tool::validate()` 里那处 `schema().name()` 是 origin↔schema 一致性检查，保留。<br>**遗留（未修，非本条引入）**：`dispatch.rs` 的 `failure_output` 把 `origin().qualified_name()` 写进回给模型的 `{"error":{"tool":...}}`，于是命名空间工具被拒时模型收到的是 `mcp.github.search`，而它调的是 `search`——同一类「模型面读了路由身份」的毛病，今天对每个 MCP 工具都成立，与重命名无关。<br>**验收**：`tests/it-runtime/tests/tool_profile.rs` 23 条（显式选择 / 缺件点名 / 全注册 / 同名双 server 可注册但不可同面 / **判名字读 `model_definition` 投影** / **配置来的预算同样过校验** / **`Hidden` 可与 advertised 同名** / **`Deferred` 仍占名字** / **`Disabled` 什么都不占** / 重复 key / 注册即校验 / 三类不广播不计费 / Dynamic 计费 / 上下限 / 字节上限与口径一致 / 注册顺序不改结果 / 四类 profile 构造错误 / surface 直接喂给 `AgentSpec`），`tests/it-core/tests/agent_spec.rs` 补 1 条（`Hidden` 与 advertised 同名可 build），`tests/it-runtime/tests/response_classification.rs` 补 1 条（两个内部都叫 `search`、其一对外 `jira_search`，模型按对外名调用能解析并绑到正确实现，8→9 条），`tests/it-coding/tests/tool_profile.rs` 8 条（默认档是 15 个 / 每档自洽 / 三档实测边界 / core ⊂ codex_like / 六个执行观察入口 / 装配在预算内 / 缺件时响亮失败 / 已实现的工具都注册在声明的 key 上）。**后两条按「今天有哪几个工具」推导，不再写死名单**：原来手写 `read_file \| exec_command` 白名单，`apply_patch` 落地后它就静默漏检最新的那个——正是这条测试存在的理由。现在缺件集从注册表反推、实现集反过来对着声明查，core 补齐那天缺件测试会自己喊「该退休了」 |
| R2-5 | **手法一：单 exec 收编长尾** | **DONE** | `rg` / `sed` / `jq` / `git` / `nl` / `find` 全部经 `exec_command` 跑，**不给每个动作单独 schema**。实证：Codex `exec_command` schema 仅 1635 B、描述一句话，占 26,969 次调用（绝对主力）；CC 的 `Bash` 占 3212/6234。**`exec_command` 同时统一前台与后台**（返回输出 or `session_id`），配 `write_stdin`——两个工具覆盖 AF 的 `shell_command` + 9 个 `background_shell_*`。`write_stdin` 与启动工具共享一个 `ProcessManager`，按每个 session 跨调用存活的投递游标只回传尚未交付给模型的输出（投递过的不重复，两次调用之间产生的也不丢），并在新输出、进程退出或 yield timeout 三者之一发生时返回；未知 session、stdin 已关闭的 session，以及向已结束 session 写入字符，都以模型可纠正的失败结果返回；空 `chars` 是 poll 而非写入，因此对已结束但仍保留的 session 返回退出状态而不是失败。coding host 将这对工具一起装配，稳定提示词枚举同一套 surface；命令入口声明并行，rooted workspace claim 负责冲突准入。配套 prompt 侧同步收窄，禁止用 `cat`/shell 写入直接编辑文件，避免模型为每个命令寻找细分工具。 |
| R2-5b | **手法二：namespace 折叠** | TODO | Codex 的 `collaboration`（4592 B）与 `codex_app`（1303 B）是 **namespace 工具：1 个入口代表一组子工具**，不把子工具全部 advertise。rusty-agent 的 `agent.*`（spawn/output/followup/interrupt/stop）与 `mcp.*`（list/read/templates）按此折叠 |
| R2-5c | **手法三：deferred tool + `tool_search`** | TODO | **Codex 不是"少工具"，是"核心 advertise + 长尾 deferred"**：长尾工具不进 schema，但可被 `tool_search`（BM25 检索工具元数据）发现并按需加载。落库形态见 Codex 的 `thread_dynamic_tools(thread_id, position, name, description, input_schema, defer_loading, namespace)`。**注意 advertise ≠ 拥有**：registry 里工具全在、仍可执行，只是模型看不见 schema 就不会主动调用。<br>**声明面已就位**：`ToolOptions::exposure`（R2-1）的 `Deferred` 就是这一档，`is_discoverable()` 是本条的索引判据；**BM25 直接用 `bm25` crate**（Codex 的 `tool_search` 就是它：`SearchEngine<usize>` + 按 registry 条目身份缓存，见 `core/src/tools/handlers/tool_search.rs`），不自己实现 |
| R2-6 | 动态启用与冲突策略 | **DONE** | `ToolAvailability::{Enabled, Disabled, Dynamic}` 在每轮准备的第一阶段用同一份 `RunContext` 解析；动态回调的 enabled 快照同时决定请求工具表和后续可执行对象。`RunConfig::tool_name_collision_policy` 默认 `Warn`：在模型调用前按既定派发优先级保留一个 action（handoff 优先，其余取最后声明），记录结构化 warning；`Error` 则在调用 provider 前拒绝。bare / namespaced / deferred-top-level lookup key 继续分开路由。函数工具的 schema-bound 解码也收进统一派发入口，工具从 `ToolContext::decoded_input()` 取已校验值；没有 Rust 输入类型的远程工具仍接收解析后的 JSON。输入规范化负责清除历史中的孤儿 tool call/output，避免把已不在本轮 surface 的动作重新送给模型。 |
| R2-7 | 并发、超时与失败成型 | **DONE** | `RunConfig::max_function_tool_concurrency` 默认 8 并拒绝零值；每个工具按 `ToolOptions::timeout` 在自己的取消 scope 内限时；`ToolFailureHandling::Custom` + `Tool::handle_failure` 是可插拔的失败成型入口。批执行返回 `FunctionToolResult{tool, output?, run_item?, interruptions, nested_run?}`，保留成功、模型可见失败与待审批三种结果的归一化 item；停止策略继续只看成功的 `ToolUseResult`，不能把失败或等待审批当成功结束。`nested_run` 采用可持久的 `NestedRunRef` 而非 runtime 的 live result，避免 `ra-core → ra-runtime` 反向依赖；R12-2 将填充并解析它。当前 runner 仍通过既有 `RunItem` / `NextStep::Interruption` 路径传递结果，不把 `TurnExecution::function_results` 收进 `SingleStepResult`；嵌套 agent 是它的第一个 runner 级消费者，到位时一并接出。**只读工具轮内并行是默认形态而非优化项**（见 R3-4b）。 |
| R2-8 | **内置工具集 v1（Codex × Claude Code 融合，15 个）** | **进行中（11/15：`read_file` / `grep` / `glob` / `exec_command` / `write_stdin` / `apply_patch` / `update_plan` / `view_image` / `web_search` / `web_fetch` / `skill` DONE；缺 `ask_user` / `tool_search` / `agent.*` / `mcp.*` 四个入口）** | 见下方[融合工具集](#r2-8-融合工具集codex--claude-code)。原则：**执行面取 Codex（统一 exec + freeform apply_patch），观察面取 Claude Code（结构化元数据的 read/grep/glob）**，编排类一律 namespace 折叠或 defer。<br>**`read_file` 落地形态**（`ra-tools/src/read_file.rs`，615 B schema）：`{path, offset?, limit?}` → 文本带行号 / 图片进 `ImageBlock` / PDF 进 `FileBlock`；正文只放文件内容，窗口位置、截断、下一步全在 `ObservationMetadata` 里，无截断时**一个 token 都不占**。<br>**「谁选的窗口」决定它算不算截断**：模型自己给了 `limit` 就不是——它拿到了它要的；工具因为没给 `limit` 而按默认 2000 行截的才是，因为模型没被告知少拿了多少，而 R5-1 要的正是这个数。窗口截断与渲染截断（行长 + 字节上限）是两条 `Truncation`，不是一条——R2-3 那条「列表不是字段」在这里第一次真正用上。<br>**失败走 `ToolFailureHandling::Custom`**：R3-4 的 dispatcher 只把框架错误渲染成一个裸 code（框架散文不进模型上下文是它的硬规矩），所以「文件不存在 / 是目录 / 是二进制 / 参数不合 schema」由工具自己写句子，类型化的失败经 `Error::source` 传过去、绝不从 `Display` 里刮文本回来。**参数解码失败也必须成型**——`Custom` 下返回 `None` 会让错误继续上抛把整轮停掉，而参数打错是模型最容易改的一类错。<br>**空文件与 offset 越界是成功的观察不是失败**（问题问得好好的，答案是真的）；空结果在 R2-3 那层就会被拒，所以正文放一句话。<br>**`rooted()` 的边界已随 R8-5 定型**：根变成一个 descriptor capability，之后每一段都从那个句柄走，符号链接只跟到不出根为止，`..` 拒绝而不归一化（POSIX 弹的是链接的*目标*，`a/b/../c` 与 `a/c` 在 `a/b` 是链接时不是同一个文件）。宿主装配走 `for_workspace`，与 `apply_patch` / `exec_command` 共用同一个 `Workspace`。完整路径策略（per-path denies、mount policy）见 R8-5 的移交项。<br>**`grep` / `glob` 落地形态**（`ra-tools/src/{grep,glob,search}.rs`）：两者通过同一 workspace descriptor capability 做稳定排序遍历，并以共享 resource claim 与读取并行；阻塞遍历移到 Tokio blocking pool。`grep` 支持正则、可选文件 glob 与大小/二进制跳过统计，`glob` 返回确定排序的相对路径；两者报告结果/字节截断、不可读项和收窄建议。外部链接和 `..` 均不能越过 workspace。宿主把这两个观察入口同时装给主角色，且只读角色同样可用。<br>**两条判据经复审收紧**：① glob 以 `/` 为组件边界编译（`GlobBuilder::literal_separator(true)`，两个工具共用 `search::compile_glob`）——globset 的默认让 `*` 吃掉分隔符，于是 `src/*.rs` 答的是整棵子树，`**` 也就不再表示任何单星没表示过的东西，模型没有办法只要某一层目录自己的文件；② 输出预算保留前缀——第一条塞不下的行就封闭正文，否则跳过它去收后面更短的那条，等于把一个「按长度筛出来的子集」交给模型，而两个工具都在告诉它这是前 `returned` 条，且长度不是收窄 `path` 或 pattern 能翻页的维度；总数在正文停止增长后继续累加，所以 summary 仍然精确。<br>验收：`tests/it-tools/tests/read_file.rs` 34 条；`tests/it-tools/tests/search.rs` 13 条（含上述两条判据各 1 条）；`tests/it-coding/tests/{coding_host,prompt_dump,tool_profile,tool_schema_dump}.rs` 覆盖 capability、工具面与三家 provider wire 快照。 |
| R2-9 | 工具 schema **字节级稳定** | **DONE** | `ToolSchema` 在构造与反序列化时递归排序 JSON object key，并以 compact JSON 计算/验证 SHA-256；`ModelToolDefinition::new` 再在 provider-neutral 投影边界规范化一次，因而自定义投影也不依赖 `serde_json` map feature 的迭代顺序。字段按声明顺序序列化，schema 不接受时间戳或随机 ID。`ModelHandoffDefinition::new` 同样规范化——handoff 与 tool 汇进同一个 `tools` 数组、同一段缓存前缀，只规范化其中一个等于只堵半条路。字段按声明顺序序列化，schema 不接受时间戳或随机 ID。<br>**门禁形态是快照而不是「跑一遍测试」**：`cargo xtask schema-stability` 对账入库的 `api/tool-schemas.txt`，里面是三个 protocol **各自真正上线的形状**（Responses 的 `parameters`、Chat 的 `function.parameters`、Anthropic Messages 的 `input_schema`），由 `it-coding` 把真实的 `read_file` / `exec_command` / `apply_patch` 三个工具外加一个 handoff 灌进真实 codec 抓下来。这样 schema 一改就在 review 里显示成 diff，而不是只把门禁染红；改动确认无误后 `cargo xtask schema-stability --bless` 重写快照。同一份 payload 连渲 100 次，任一字节差异即 FAIL（99 次重渲含 198 个 wiremock server，实测 0.24s）。宏集成测试对泛型 derive 的完整 model-facing 投影做同样的 100 次比对。<br>**投影不实现 `Serialize`**：provider 各自用 `json!` 从 getter 组装，给 `ModelToolDefinition` 加 derive 只会凭空多出一种没人上线的形状，还会绕过 `api/*.txt` 基线悄悄进公开面（基线只记签名与 struct 声明，不记 trait impl）。实证基线：Codex 16 个工具 schema 共 19,786 B，在 82 个请求里**一字节不差**。 |
| R2-10 | 工具 schema token 预算 | **DONE** | `CodingProfile` 的三档 surface 均声明 `max_advertised_bytes = 20 * 1024`，`ToolRegistry::assemble` 以实际 `ModelToolDefinition` 投影（schema + name + description，排除 provider envelope）累加并拒绝越界；因此重命名或 provider-facing schema 改动也按真正下发的内容计费。`tests/it-coding/tests/tool_profile.rs` 另把已实现的干活工具按**份额**约束：上限不是另写一个绝对值，而是由 20 KiB 工具面上限除以默认档声明的 15 项得到每项份额，再乘以今天已实现的项数（当前 2155/4095 字节）。写死一个总量只会由撞上它的人抬高——和各档用区间而非精确项数是同一个理由；按份额写，它随工具落地自然放宽，只有「相对自己那格偏胖」的 schema 才会红。另有一条 provider wire 侧的互补契约（`tests/it-coding/tests/tool_schema_dump.rs`）量的是**含各家 envelope 后真正上线的字节数**，同样从 profile 读上限，当前最大 openai_chat 2718/20480 字节。`cargo xtask token-budget` 跑这两条并**转述它们打印的实测值**：只报「测试通过」的门禁不比 test 那条多说什么；更要紧的是退出码证明不了断言还在——用例被删掉、或按名字过滤一条都没匹配上，`cargo test` 都照样退出 0，所以两条契约各自把实测值打在一行标记上，门禁找不到那行即判红。完整 15 项工具面尚在分阶段实现，但每项接入 profile 时都会经过同一装配上限。 |
| R2-11 | 工具行为契约文本 | **进行中（3 项已落地）** | 行为契约规则与当前裁决见 [`Tool_Behavior_Contracts.md`](Tool_Behavior_Contracts.md)：description 写成**行为契约**（何时用、失败怎么分流、危险边界、外部内容信任边界），而不是参数说明。但注意 Codex 的反向证据：干活工具描述**极短**（`exec_command` 只有一句），长描述留给协作/编排类工具。**默认倾向短描述**，长契约仅用于高危或语义模糊的工具。当前 `read_file` / `exec_command` 为短描述；`apply_patch` 明示“后续失败可能保留先前动作”的非事务语义，并有文字与行为测试。<br>**措辞要盯着实现的全部失败分支，不是最容易想到的那一个**：初稿写的是“后续 **conflict** 可能保留先前动作”，而 `call()` 对文件系统错误、非 UTF-8、不支持的 action 同样提前返回并保留已提交的 delta——描述比实现窄，且模型收到的结果文本里根本没有 conflict 这个词，两头对不上。改成 failure 才覆盖 `render_stop` 的全部入口。<br>**改 description 要跑的门禁是三条不是一条**：`api/tool-surface.txt` 的指纹覆盖全部 model-facing 字段（R4-6 的设计目的正是让“只改描述”瞒不过去），所以本条把 `TOOL_SCHEMA_REVISION` 抬到 2 并重出两份快照——**缓存前缀是被有意作废的**。只跑 `schema-stability` + `token-budget` 会让 `prompt-dump` 在 CI 上红，实测就是这么发现的。 |
| R2-12 | **`ra-tools` 通用工具库拆分** | **DONE** | 落地形态：新建 `ra-tools`（可复用件层，依赖 `ra-core` / `ra-exec` / `ra-mcp`），`ra-coding/src/tools/` 下 12 个模块里的 11 个搬过去——`agent_ns` / `ask_user` / `exec_command` / `glob` / `grep` / `read_file` / `skill` / `update_plan` / `view_image` / `web` / `write_stdin`；`ra-coding` 只留 `apply_patch`。<br>**比计划更早做，因为成本曲线在这里触底**：条目原本排在第 7 步「R2 收尾时立刻做」，但此刻 12 个模块**全是一行 `//!` 的桩子**，搬运就是 `git mv` 加两处 `mod` 声明；等 R8 把它们实现完再拆，要连带动一遍所有 guard 与 profile 的引用。真正促成提前的是另一件事：目的地不定的话，下一个写 `read_file` 的人会打开 `ra-coding/src/tools/read_file.rs` 直接往里填——**留着那个文件本身就是在指错路**。<br>**判据是「换个产品一行不用改，只是要再写一遍」**，不是「看起来通用」。11 个都过这一关；`apply_patch` 当时判为不过——V4A 封装加编辑纪律正是一个第二产品会写得不一样的东西。**这一判定后来被推翻，见本条末尾的补记二。**<br>**门禁早就留好了位**：`xtask` 的 `LAYERS` 与 `ALLOWED_INTERNAL_DEPS` 从 R0 起就预登记了 `ra-tools`（可复用件 / `ra-core`+`ra-exec`+`ra-mcp`），所以这次是填位而不是给分层规则开口子——crate 一出现就被绑住。同时进 `public-api` 的 TRACKED 名单（`api/ra-tools.txt`，13 个 crate）与测试 workspace（`tests/it-tools`）。<br>**模块仍是桩子**：本条交付的是「文件归属定了」，工具本体属于 R2-8 与 R8-1..R8-6。<br>**补记（R2-8 首件时）**：搬过来的 11 个模块少了 advertise 集里的 `mcp.*` 与 `tool_search`，已补成 13 个——理由与本条同一条：模块不存在，下一个实现的人就会把它填到别处去。`ra-tools` 的模块表因此等于「15 个 advertise 入口减 `apply_patch`」，缺的只是实现进度。<br>**补记二（R6-8 之后）：`apply_patch` 也搬进 `ra-tools`，本条的判定改为 12 个全搬。**原来的理由把「V4A 封装」和「围绕它的编辑纪律」当成一件事，实际是两件，而只有第二件是产品内容——**把 diff 应用到文件上，一个改报告的研究 agent 同样需要**。搬过去的 280 行里一句提示词、一个产品分支都没有，只有 schema、文件系统 capability 和 `ra-patch`；编辑纪律（`prompt/editing.rs` 的唯一写入口、`profile.rs` 的档位、`dangerous_action` 的审批事实）原地未动，**这恰好证明了两者本来可分**。连带：`ra-coding` 现在不拥有任何工具，`ra-tools` 依赖加 `ra-patch`、`ra-coding` 依赖去掉 `ra-macros`（它当初进来就只为这个工具的 schema derive），`ALLOWED_INTERNAL_DEPS` 两处同步，`api/ra-tools.txt` 新增 3 项公开面（门禁先拦下再 `--bless`）。验收随之拆开：工具契约归 `it-tools/apply_patch` 6 条（含新增的 capability 越界与默认 options），宿主装配归 `it-coding/apply_patch` 2 条 |

### R2 实现状态修订（2026-08-11）

- `Deferred` 仍是 R2-5c 的声明形状，不是已可用的能力：`tool_search` 及跨轮 promotion state 尚未实现。为避免 enabled 工具“注册了但永远不可达”，当前 `ra-runtime::turn::prepare` 会明确拒绝 `ToolExposure::Deferred`；只能使用 `Advertised` 或 `Hidden`。R2-5c 完成时必须把本轮发现的 snapshot、promotion 结果和下一轮的 action surface 一起设计，不能恢复为只在 `PreparedTurn` 暂存后由 `into_call()` 丢弃。
- R2-7 / R3-4b 已落地执行保护：`RunConfig::max_function_tool_concurrency` 默认 8，零值在模型调用前拒绝；`turn::batch` 用 cancellation-aware semaphore 限住整条 dispatch chain，并保留 `RwLock` 的 `Parallel` 读锁 / `Exclusive` 写锁语义。`FunctionToolResult` 保留完整的函数调用结算结果；完成的流式 tool-call item 会立即进入同一条受监督的 dispatch chain，嵌套 agent 的结果填充归 R12-2。
- `read_file::rooted()` 不再对模型路径做 `canonicalize` 或词法折叠再重开；workspace root 仅在构造时转成 `cap_std::fs::Dir` capability，之后由该目录描述符相对打开并用同一 fd 做 metadata/read。root 内相对符号链接可用，越界链接被 capability resolver 拒绝；含 `..` 的 rooted 请求明确拒绝，不能静默把 `link/../target` 改写成另一条路径。文本和二进制读取均采用小的初始预分配并按实际字节增长，避免极大 limits 在打开小文件时直接申请巨型内存。

> **不拆的后果是具体的**：写第二个 agent 时，`ra-analysis` 要么复制这 14 个工具，要么依赖 `ra-coding`——后者直接破坏「产品 → 框架」的单向依赖，而这条依赖方向是本项目相对 AgentForge 的核心改进。

### R2-8 融合工具集（Codex × Claude Code）

> 设计原则：**执行面取 Codex**（一个 `exec_command` 统一前台/后台并收编 shell 长尾；`apply_patch` 唯一编辑入口且是 freeform 不包 JSON）；**观察面取 Claude Code**（`Read`/`Grep`/`Glob` 有结构化元数据，能进入模型上下文、Session 与离线 eval；裸 `rg` 输出做不到）；**编排面两家都很重，一律折叠或 defer**（Codex `collaboration` 4592 B、CC `Workflow` 21,088 B 占 30% schema 但只占 0.2% 调用）。`grep` / `glob` 是 rusty-agent 的 Rust-native 一等工具，不是 `exec_command` 或 shell alias；底层可复用 ripgrep 生态 crate，但输出契约、截断、统计、权限与排序由 rusty-agent 控制。

#### 完整并集映射（按能力族，覆盖两家全部工具）

| 能力族 | Codex | Claude Code | rusty-agent 决定 | 取谁 · 为什么 |
| --- | --- | --- | --- | --- |
| Shell 执行 | `exec_command`（PTY，前台返输出/后台返 session_id） | `Bash` | ✅ `exec_command` | **Codex**：一个工具统一前台+后台+交互，CC 的 `Bash` 没有会话概念 |
| 交互式 stdin | `write_stdin` | — | ✅ `write_stdin` | **Codex 独有**，CC 缺这个能力 |
| 轮内并行 | `multi_tool_use.parallel` | 一条 assistant 发多个 `tool_use` | ✅ 框架层实现（R3-4b） | 做成循环形态而非工具；显式包装工具留 v2 |
| 文件编辑 | `apply_patch`（freeform V4A，含 `*** Move to:`） | `Edit` + `Write` | ✅ `apply_patch` | **Codex**：1 个 schema 覆盖增/改/删/移，CC 要两个 |
| 文件读取 | 靠 `exec cat/sed` | `Read`（多模态 + 行号 + token 审计） | ✅ `read_file` | **CC**：结构化元数据可进入上下文、会话事实与离线 eval；裸 `cat` 不行 |
| 正则检索 | 靠 `exec rg` | `Grep`（匹配统计） | ✅ `grep`（Rust-native） | **CC 的工具形态 + Rust 实现**：不是让模型自己 `exec rg`，而是提供稳定结构化 observation |
| 文件模式查找 | 靠 `exec rg --files` | `Glob` | ✅ `glob`（Rust-native） | **CC 的工具形态 + Rust 实现**：确定排序、可审计跳过原因 |
| Notebook 编辑 | — | `NotebookEdit` | ⏸ defer | 低频 |
| 计划板 | `update_plan{plan[].{step,status}}` | `TodoWrite` | ✅ `update_plan` | **Codex 的结构**（带 explanation）+ CC 的 delta reminder 投影 |
| 计划模式 | — | `EnterPlanMode` / `ExitPlanMode`（4,324 B） | ❌ 不做工具 | 做成 **permission mode 的一档**，省 4KB schema |
| 目标与预算 | `get_goal` / `create_goal` / `update_goal` | — | ⏸ defer | 归自治 GoalRunner（Deferred） |
| 用户提问 | `request_user_input`（1-3 问 + `autoResolutionMs`） | `AskUserQuestion`（5,028 B） | ✅ `ask_user` | **融合**：CC 的选项结构 + **Codex 的超时自动决议**（CC 没有） |
| 子 agent | `collaboration` ns：spawn / followup / interrupt | `Agent` + `TaskCreate/Get/List/Output/Stop/Update` + `SendMessage` | ✅ `agent.*` ns | **Codex 的折叠形式** + **CC 的 `outputFile` 上下文隔离语义** |
| 图片 | `view_image` | `Read`（多模态入口） | ✅ `view_image` + `read_file` 双入口 | 两家各取；`view_image` 显式表达"要看图" |
| Web 搜索 | `web_search` | `WebSearch` | ✅ `web_search` | 两家一致 |
| Web 抓取 | 靠 `exec curl` | `WebFetch` | ✅ `web_fetch` | **CC**：独立工具才能挂 URL 信任边界与注入防护 |
| 工具发现 | `tool_search`（BM25，2,040 B） | `ToolSearch` | ✅ `tool_search` | **两家都有**；是工具面能压到 15 的前提 |
| MCP 资源 | `list_mcp_resources` / `read_mcp_resource` / `list_mcp_resource_templates` | MCP 工具直接注入 | ✅ `mcp.*` ns | **Codex 的折叠**：3 个压成 1 个入口 |
| 技能 | `skills/` 目录（SKILL.md 渐进披露） | `Skill` | ✅ `skill` | 两家形式一致 |
| 结构化输出 | `output_schema_file` | `StructuredOutput` | ❌ 不做工具 | 做成**请求参数**（`text.format` / `response_format`），比工具省 schema |
| 定时与唤醒 | `codex_app:automation_update`（cron + heartbeat 二合一） | `CronCreate/Delete/List` + `ScheduleWakeup`（合计 ~8KB） | ⏸ defer | 若做，取 **Codex 的二合一形式**折叠成 `automation.*`，不是 CC 的 4 个独立工具 |
| Worktree 隔离 | `codex_app:handoff_thread` | `EnterWorktree` / `ExitWorktree`（4,034 B） | ⏸ defer | |
| 工作流编排 | — | `Workflow`（**21,088 B = CC schema 的 30%**，仅占 0.2% 调用） | ❌ 不采用 | 性价比最差的单项 |
| 线程/会话管理 | `codex_app` ns 的 17 个子工具 | `TaskList` / `TaskGet` | ❌ 不进 agent 工具面 | 宿主专有，走控制协议（R13）而非工具 |
| GUI / computer-use | `click` / `press_key` / `set_value` / `list_apps` / `get_app_state` | `mcp__Claude_Browser__*` | ⏸ defer，走 MCP | 两家都是外挂形态，不进内置面 |
| 代码审查产出 | — | `ReportFindings` | ⏸ defer | 产品化功能 |
| 依赖装载 | `codex_app:load_workspace_dependencies` | — | ❌ | 宿主专有 |
| 文档/表格/PDF | plugins：documents / spreadsheets / presentations / pdf / latex | Skill 形态 | ⏸ defer | 走 skill 或 MCP |

**统计**：✅ 采用 15（其中取 Codex 6、取 CC 5、融合 2、两家一致 2）· ❌ 明确不做 5 · ⏸ defer 8。
**结论**：**Codex 赢在执行面与折叠手法，Claude Code 赢在观察面的结构化元数据。** 两家在编排面都偏重，这部分一律 defer。

**Advertise 集（15 个，schema 预算 ≤ 20 KB）**

| # | 工具 | 取自 | 为什么这样选 |
| ---: | --- | --- | --- |
| 1 | **`exec_command`** **DONE** | Codex | PTY 里跑命令，**返回输出或 `session_id` 转后台**——一个工具覆盖前台+后台+交互式。收编 rg/sed/jq/git/nl/find。参数：`cmd, workdir, shell, tty, login, timeout_ms, max_output_tokens, prefix_rule, justification` |
| 2 | `write_stdin` **DONE** | Codex | 向运行中会话写输入并返回最近输出；`tty=true` 时可发 Ctrl-C(``) 中断。与 1 配对，替代 AF 的 9 个 `background_shell_*` |
| 3 | **`apply_patch`** **DONE** | Codex（**custom freeform**） | 唯一编辑入口，V4A 格式，**明写 "do not wrap the patch in JSON"** 比 JSON 参数省 token。`*** Add File:` 覆盖新建，因此不需要 `write_file`；`*** Update File:` 覆盖编辑，因此不需要 `edit_file` |
| 4 | `read_file` **DONE** | **CC `Read`** | 多模态读取**统一入口**（文本/图片/PDF/notebook），带窗口与预算截断元数据。这是 CC 相对 Codex（靠 `cat`/`sed`）的真实优势：结构化元数据能稳定进入模型上下文、Session 与离线 eval。**已落地 615 B**（预估 ≈1 KB，与 Codex `view_image` 554 B / `write_stdin` 819 B 同量级）；**notebook 仍按纯文本读**——`.ipynb` 的 cell 分解跟着 `NotebookEdit` 一起 defer |
| 5 | `grep` **DONE** | **CC `Grep`** | Rust-native 一等工具：返回匹配数、扫描/跳过文件统计、截断原因、**收窄建议**。**已落地**：依赖只取了 `regex` + `globset`，**没有用 `ignore` / `grep-searcher`**——遍历必须走 workspace 的 descriptor capability（`RootedFileSystem::walk_files_below`），而 `ignore` 按路径名走目录，用它就等于把 R8-5 定的边界让回给路径字符串。跳过表（`.git` / `target` / `node_modules` / `.venv` / `__pycache__`）与 5 万文件上限因此是自己的策略，不是 `ignore` 的 gitignore 语义 |
| 6 | `glob` **DONE** | **CC `Glob`** | Rust-native 一等工具：文件模式查找，结果确定排序；**已落地**，与 `grep` 共用同一遍历与 `globset` 编译（`/` 为组件边界），不靠 `rg --files` 字符串输出当公共契约 |
| 7 | `update_plan` **DONE** | Codex `update_plan` + CC `TodoWrite` | 计划板。取 Codex 的 `plan[].{step, status}` 结构；**922 B**。<br>**板子就是那次调用本身**：工具不存计划，整块替换是唯一写法，权威副本是历史里的 tool call——resume、replay 与宿主 UI 读的是同一份字节。工具里留副本会得到一份「跨 run 串台、resume 后为空、最终还得跟历史对账」的第二真相。<br>**没有 `explanation` 参数**：Codex 有，是因为它没有旁白通道；本框架有（R3-10），再开一个字段就是同一句话的第二个落点，还要每轮付 schema 字节。<br>**delta reminder 投影未做**：Codex 记完不再提，CC 每轮重投——这是产品策略不是入口属性，且渲染要用 `ra-prompt` 的 `RuntimeReminder`，而 `ra-tools` 依赖白名单里没有它。归 R4-3 在 `ra-coding` 接线 |
| 8 | `view_image` **DONE** | 两家都有 | 本地图片进多模态上下文；**358 B**（Codex 554 B）。<br>**它和 `read_file` 的真正区别是对非图片的处理**：`read_file` 把 `chart.svg` 当文本读回来，模型得自己从结果里发现请求被改写了；这个直接拒绝并点名让它改用 `read_file`。空文件也拒——0 字节的 image block 会被 provider 整个请求打回，那时失败的是下一次请求而不是这次调用 |
| 9 | `web_search` **DONE** | 两家都有 | 结果列表满了**停在第一条塞不下的**（同 grep/glob 判据）；连第一条都塞不下时不硬塞，报「首条超预算」并留一个非空占位块。`Truncation` 记的是渲染字节两侧，不是条数 |
| 10 | `web_fetch` **DONE** | **CC `WebFetch`** | Codex 走 `exec curl`，但独立工具能挂 URL 信任边界与 prompt-injection 处理，值得单列。<br>**不可信边界走 guidance 而不是正文里的围栏**：`ObservationMetadata` 渲染成正文**之前的独立块**，页面关不掉一个它不在里面的定界符；写进同一个字符串的标记，页面自己也能写。更强的边界（provenance 挂在 block 上、并穿过压缩与 replay）归 R7-11。<br>报的是后端**实际应答的地址**而非请求地址——引用要指向读到的那份文档 |
| 11 | `ask_user` | Codex `request_user_input` + CC `AskUserQuestion` | 结构化提问 1-3 问；取 Codex 的 `autoResolutionMs`（超时自动决议）——这个 CC 没有 |
| 12 | `skill` **DONE** | CC `Skill` + Codex skills | 渐进式披露：清单进缓存前缀、正文进工具结果，两半读同一个 `SkillCatalog`。<br>**清单进前缀是合法的，且不是 memory 那条规则**：memory 在 agent 干活时就在变，catalog 只在有人装技能时变，所以它跟安装一样稳定。<br>**超出前缀份额的技能不会因此消失**：`skill` 传 `skill=null` + `offset` 就是分页列表入口，前缀里被截掉的那些照样能被发现和加载 |
| 13 | **`tool_search`** | Codex + **CC `ToolSearch`** | 长尾 deferred 工具的发现入口（R2-5c）。**两家都有**，是能把工具面压到 15 还不损失能力的前提 |
| 14 | `agent.*`（namespace） | Codex `collaboration` + CC `Agent`/`TaskOutput`/`SendMessage`/`TaskStop` | 折叠成 1 个入口：`spawn` / `output` / `followup` / `interrupt` / `stop` |
| 15 | `mcp.*`（namespace） | Codex 3 个 MCP 工具 | 折叠成 1 个入口：`list_resources` / `read_resource` / `list_templates` |

**Deferred 集（不 advertise，靠 `tool_search` 发现）**

`browser.*`、`computer_use.*`、`image_compare` / `image_crop`、`memory.*`、`notebook_edit`、`cron.*`、`schedule_wakeup`、`worktree.*`、`workflow`、`git_status` / `git_diff`（走 `exec_command`）。

**明确不采用**

| 项 | 来源 | 不采用的理由 |
| --- | --- | --- |
| `Workflow` | CC | 21,088 B **占 CC 全部 schema 的 30%**，但实测只占 6,234 次调用中的 12 次（0.2%）。性价比最差的一个 |
| `EnterPlanMode` / `ExitPlanMode` | CC | 4,324 B。plan mode 做成**运行模式**（permission mode 的一档）而非工具 |
| `Cron` / `ScheduleWakeup` / `EnterWorktree` | CC | 合计 12 KB。defer，有真实需求再 advertise |
| `write_file` / `edit_file` | AgentForge | `apply_patch` 已覆盖；两套编辑入口是 AF 工具面膨胀的典型来源 |
| 9 个 `background_shell_*` | AgentForge | `exec_command` + `write_stdin` 两个覆盖。**注意**：AF 的教训是"合并要改 2881 行 + 12 个 guard 的 180 处硬编码判定"——rusty-agent 从第一天就用统一 exec，没有这个迁移债 |
| `shell_command` 与 `exec_command` 并存 | AgentForge | 只留 `exec_command` |

**规模核算**：15 个中干活的 4 个（exec/write_stdin/apply_patch/read_file）≈ 3.9 KB，观察 2 个（grep/glob）≈ 1.5 KB，其余 9 个（含 2 个 namespace）≈ 12 KB → **总计约 17-18 KB，落在 ≤20 KB 预算内**，与 Codex 的 19.8 KB 同量级。

> **注意口径**：15 是**每轮 advertise 的入口数**，不是能力总数。namespace 折叠 + `tool_search` 之后，实际可达能力可到 40+（Codex 就是 16 入口 / ~44 能力，`codex_app` 一个入口后面挂 17 个子工具）。**不要把"15 个入口"读成"只做 15 件事"。** 见 [A.0 计数口径](#a0-️-工具计数口径不对齐会得出相反结论)。

### R2 非目标

| 项目 | 处理 |
| --- | --- |
| 40+ 工具的宽工具面 | 不做；硬上限 24（CC 量级），默认 14-16（Codex 量级）。AF 的 43-44 是反面教材 |
| 把 read/list/search/grep 各拆一个工具 | 不做；长尾能力收进 `exec_command`，只保留有独立价值的（read_file 的窗口预算、grep 的结构化统计） |
| 同时暴露 Codex 与 Claude Code 两套工具名 | 不做；一次请求只暴露一个 profile 的命名（AF `Codex_ClaudeCode_工具能力兼容开发方案` 的结论） |
| 工具描述靠外部文档 | 不做；模型只读 schema |
| 用工具名字符串做控制流判断 | 不做；控制流只读 `ToolOrigin` 与结构化 `CapabilityFamily`（AF 去词表化结论） |

### R2 验收标准

| 能力 | 标准 |
| --- | --- |
| 数量达标 | 默认 profile 工具数 ∈ [14,16]，`full` ≤ 24；总 schema ≤ 20 KB |
| 字节稳定 | 同配置连续渲染 100 次 schema 字节全同（CI 断言） |
| 身份无歧义 | 两个 MCP server 提供同名 `search` 工具时，调用与 RunState 恢复都能正确路由 |
| 观察可自纠 | 截断/空结果/超大输出场景下，工具结果里能看到原因与下一步建议 |

---

## R3 Agent Loop 内核

### R3 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R3-0 | **turn 准备阶段的固定顺序** | **DONE** | `ra-runtime::turn::prepare` 已落地 `prepare_turn` / `TurnPreparationRequest` / `PreparedTurn`，六个阶段固定：`resolve_enabled_tools`（跑动态 `is_enabled`）→ `resolve_handoffs` → `resolve_output_schema` → `resolve_model` → `resolve_settings` + **按工具面收敛** → `apply_model_input_filters`。`PreparedTurn` 同时持有可执行工具与模型面投影，**两者出自同一份 enabled 快照**，R3-2 的结算不会拿到与请求不一致的工具集。<br>**顺序真的承重，不只是排版**：新增 `ResolvedModelSettings::reconcile_tool_surface`，第 5 阶段拿第 1、2 阶段产出的广播名（tools + handoffs）过一遍——空工具面丢掉 `tool_choice` 与 `parallel_tool_calls`，`ToolChoice::Tool(name)` 指向本轮没广播的工具也丢掉。**不满足的选择器降级而不是报错**：一个工具这轮被 `is_enabled` 关掉正是动态可用性的用途，为它终止 run 等于让这个特性没法用。两个例外保留——`ToolChoice::None` 在空工具面下依然成立，`Mcp` 指的是 hosted 工具、本就不出现在中立面里。这一步之前，对调第 1 与第 5 阶段**不会掉任何测试**。<br>**新增 `ra-core::model::ModelResolver`**：`ModelProvider` 只在**已选定的 provider 内**解析名字，而准备阶段还需要 provider 注册身份、wire 协议和两个注册方设置层。契约放 core，`ProviderRegistry` 实现它，loop 内核因此能注入解析器而不依赖 `ra-model`。`ModelSelector` / `ResolvedModel` 一并落在 `ra-core::model::resolution`。<br>**审核修正**：① 第 5 阶段原本不接收工具面，顺序名存实亡——空工具面 + `tool_choice=Required` 会带着空工具表发出去，然后每轮栽在 provider 的 `tool_choice requires at least one tool` 上；② 第三方 `is_enabled` 是**裸 `.await`**，违反[取消契约](Cancellation_Contract.md) R1，`CancelScope` 已改成构造期必填参数（能省略就等于允许存在一个中断不了的 run），第 4 阶段前加检查点；③ `ModelSelector` / `ResolvedModel` 在 core 与 `ra-model` 各有一份**同名公开类型**靠私有转换桥接，且 `ProviderRegistry` 的固有方法与 trait 方法同名却返回不同类型——已删掉 `ra-model` 那份；④ `PreparedTurn` 只能借出 `ModelRequest` 而 `Model::get_response` 按值拿，每轮要克隆整轮输入历史，加 `into_request()`；⑤ tracing 恒为 `Disabled` 且无设置入口，R1-3 冻结的三态走不通，改为 `with_tracing`；⑥ 指令投射用 `and_then(as_static)`，R4-11 加动态源后会**静默发出空 system instructions**，改成显式失败。<br>**`turn` 模块以 `#[doc(hidden)] pub` 暴露**是显式取舍不是疏忽：测试在独立 workspace（`crates/` 下不许有内联测试），没有 `pub` 路径的模块根本无法被测。已在 `ra-runtime` 的 crate 文档里标注 `Internal`、进公开面基线让它的变动出现在 review 里、**无兼容承诺**。<br>**刻意留的桩**：`resolve_handoffs` / `resolve_output_schema` / `apply_model_input_filters` 都是独立阶段函数而非内联空值，R17 / R1-16 / R10-6b 各有**一个**插入点而不是几处调用点要重排（**第 7 阶段那个插入点已由 R10-6b 删除**：filter 必须排在 context processor 之后，而那已经在本函数之外；理由见 R10-6b）；`advertised_names` 已经把 handoff 名字算进去，R17 落地不用回头改这里。<br>**验收**（`tests/it-runtime/tests/turn_preparation.rs` 10 条 + `it-core` 工具面收敛 1 条）：动态 `is_enabled` 先于模型解析、失败即止；已取消的作用域一个 `is_enabled` 都不跑，父作用域取消能传播进来（断言走 `scope.reason()` 而非解析错误文本）；工具面被清空时 `Required` 不会带着空工具表发出去；指名未广播工具时降级而非终止 run；run override 与 agent 选择器、`None` 走注册表默认三条路径；tracing 三态；`into_request()` 转移所有权 |
| R3-1 | `NextStep` 四态状态机 | **DONE** | `ra-core::step::NextStep` 四态落地：`RunAgain` / `Handoff{new_agent}` / `FinalOutput{reason}` / `Interruption{items}`，**不加 `#[non_exhaustive]`**，已列在 `xtask` 扩展安全门禁的 `EXHAUSTIVE_ALLOWED` 例外表里并附理由。<br>**刻意不提供 `is_terminal()` / `should_continue()`**：布尔投影正是这个类型要取代的 early-return 捷径——它让调用点在不说明「拿 handoff 和 interruption 怎么办」的情况下就分支，而且**加第五个状态时它照样编译**。R3 的验收标准「`match next_step` 无 `_ =>` 兜底」是同一条纪律的另一面说法。<br>**`Handoff` 携带 `Arc<AgentSpec>` 而不是 `AgentId`**：目标解析是结算阶段（R3-2 / R3-4）的事，`NextStep` 是它的产物而不是它的输入；R3-12 在它周围绑 public / execution 身份，这里存的是 public 那个，事件与结果归属才不会跑偏。<br>**`FinalOutput` 携带 `FinishReason` 而不是输出值**：R1-16 的 `OutputValue` 尚不存在，拿 `String` / `Value` 占位等于提前冻结错误形状（与 R3-1c 同一条纪律）；最终输出项归 R3-3 的 `SingleStepResult`。<br>**补掉一个类型允许的无效状态**：`Interruption{items}` 若按 `Vec<RunItem>` 直接构造，混进一条非审批项就会让 run **永远等一个没人被问到的决定**，而症状（挂住）离病因很远。新增校验构造器 `NextStep::interruption()` 与 `RunItemKind::is_interruption()`；后者的 match 穷尽，新增 item kind 必须显式分类，不能默认落到「不是中断」——那个默认是会静默放行的方向。<br>**验收**（`tests/it-core/tests/next_step.rs` 6 条）：`收口()` 函数四臂无 `_` 兜底本身就是编译期断言；`Handoff` 用 `Arc::ptr_eq` 断言是同一实例而非等价副本；构造器拒绝非审批项与空列表 |
| R3-1b | **`FinishReason` 结构化终止原因** | **DONE** | `ra-core::finish::FinishReason` 七变体全部落地（`Final` / `ToolStop` / `MaxTurns` / `BudgetExhausted` / `Cancelled` / `ErrorHandled` / `GuardrailTripped`）+ `#[non_exhaustive]`，并已挂到 `NextStep::FinalOutput`——原因产生在结算处，不带上去等于让 runner 回头再猜一遍，正是本条要消灭的东西。`RunResult` 的字段随 R3-7 落地。<br>**单独一个模块而不是并进 `step`**：`step` 整个是 `Internal`（结算中间体，R1/R3 要能随时重构），而 `FinishReason` 在 Stable API 清单上——宿主、图边、closeout 三处都读它，**不能继承产出它的那个中间体的稳定性分级**。已写进 `ra-core` 的 crate 文档。<br>**两个刻意没有的形状**：① **变体不带载荷**——哪个 guard、哪个预算维度、什么取消原因分别由 `Error::Guardrail` / `Error::Budget` / `CancelReason::code` 记录，复制过来就是第二个可以和第一个不一致的真相源，而[取消契约](Cancellation_Contract.md)已明令「不要解析错误文本恢复原因，机器归因走 `code()`」；② **没有 `Custom(Cow<'static, str>)`**——开放标签适用于第三方必须能扩展的分类轴（扩展安全第 5 条列的 `CapabilityFamily` / `ToolNamespace` / `PromptRole`），而 R17-3 的边在这个值上路由，自由文本等于把控制流放回字符串，`#[non_exhaustive]` 才是它的生长方式。<br>**三个投影，每个都有指名的消费者**（沿用 `CancelReason` 的规矩：投影要能说出谁在用）：`code()` 给 trace 字段 / R9-0 rollout 行 / R13 协议，**与 serde 线格式逐字相同**，否则同一个值在两处记法不同、replay 与指标对不上；`is_complete()` 给 R15 判定还欠不欠 closeout、给 R17-3 选成功边还是回退边，区分的是**谁结束了这个 run** 而不是答得好不好；`is_resumable()` 给 R6-6 resume 与 R13 的「继续」，**严格窄于 `!is_complete()`**——guard 拦下与 error handler 收尾，再跑一遍是同样的停止点。<br>**`from_budget_kind()` 把 4 → 2 的映射钉在一处**：`BudgetKind` 说哪份额度没了，`FinishReason` 说 run 怎么结束的，两者不是一一对应。`MaxTurns` 单独保留一个原因（宿主的反应通常是「agent 在打转」而不是「活儿太大」），其余三种收敛到 `BudgetExhausted`。R3-8 软结束正要做这个判断，不钉住就会各写各的。<br>**未知线值显式失败**而不是落到某个默认变体——否则新版本写的「被 guard 拦下」在旧版本里会显示成「正常完成」。<br>**验收**（`tests/it-core/tests/finish_reason.rs` 7 条）：code 全局唯一且只含小写与下划线（它会做指标维度值）；`Display` 就是 `code`；线格式与 code 一致且可往返；未知值报错；`is_complete` / `is_resumable` 的划分与互斥；预算映射的 4→2 |
| R3-1c | **`AgentSpec` builder 与不可变契约** | **DONE** | `ra-core::agent` 已落地 `AgentSpec` / `AgentSpecBuilder` / `AgentInstructions`：字段全私有，**`AgentSpec` 刻意不实现 `Clone`**——共享走 `Arc<AgentSpec>`（`build()` 直接返回 `Arc`），变体只能经 `to_builder()` 显式派生，运行期没有任何可变入口。身份键是 `id`，`name` 只是展示元数据且允许重名。`model` 存**未解析选择器**，`model_settings` 是四层合并里的 agent 层。<br>**字段面与本条初版的一处偏差**：初版写的是存 `ModelSelector`，实际存 `Option<String>`。`ModelSelector` 是 R1-3a **解析的产物**（带 provider 注册身份与 wire 协议），`AgentSpec` 手里没有注册表，能存进去的只能是待解析的字符串；provider 解析归 R3-0 的第 4 阶段。<br>**刻意缺席并写进类型文档**：capabilities / guardrails / hooks / `output_schema` / `tool_use_behavior` / handoffs 全部等各自的协议中立契约（R3-5 / R1-16 / R7 / R17）先存在。私有字段 + `#[non_exhaustive]` 保证后补不是破坏性变更，而现在拿占位字符串顶上等于**提前冻结错误的身份与回调形状**。<br>**`AgentInstructions` 的私有 `InstructionSource` 枚举**是给 R4-11 留的：加动态源不动 `AgentSpec::instructions` 签名，也不用现在就把运行期上下文塞进 core 类型。<br>**Debug 脱敏**：指令只渲染字节数，`model_settings` 整个不打印——`extra_headers` 里会有 `authorization`。<br>**审核修正**：builder 原先只按 `ToolOrigin::lookup_key` 查重（带命名空间），而模型面广播的是裸名——两个 MCP server 各出一个 `search` 能 build 成功，然后**每轮**栽在 provider 的 duplicate tool name 上。现在查找键与模型面名字**两个身份都查**，且测试先断言"查找键确实不同、模型面名字确实相同"再断言拒绝，把这个洞本身钉住。<br>**验收**（`tests/it-core/tests/agent_spec.rs` 9 条）：`Arc<AgentSpec>` 满足 `Send + Sync + 'static`；`to_builder()` 复用工具的 `Arc` 而非深拷贝，派生不动原件；缺 `id` / `name` 返回明确 `Error::Config` 而非 panic，未 trim 与控制字符一并拒绝；两个同名 agent 靠 `id` 区分且能反查；Debug 不泄漏指令与凭据 |
| R3-2 | `ProcessedResponse` 分类 | **DONE** | 类型落在 `ra-core::step::processed`（`ProcessedResponse` + `ToolRunFunction` / `ToolRunHandoff` / `ToolRunApproval` / `ToolNotFound` / `ToolUse`），分类逻辑落在 `ra-runtime::turn::process::process_model_response`——core 只有类型与不变量，runtime 才做解析，符合两个 crate 各自的边界声明。<br>**四个类别按「结算要做的事不同」划分，而不是按 provider payload 类型**：跑它 / 转移控制权 / 问人 / 回一条答不上的失败。参考实现要拆 `shell_calls` / `apply_patch_calls` / `computer_actions` 是因为那三样在它那儿是三种不同的 payload 与执行路径；这里只有一条本地执行契约（`Tool` + `ToolLookupKey` 派发），shell 与 apply-patch 是产品 crate 里对它的普通实现。**给每个产品工具开一个类别 = 把产品词表塞进内核，并把 `if name == "shell"` 放回 runner**，正是 R7-10 要禁的那种文本驱动控制流。<br>**`interruptions` 与 `tools_used` 是投影不是字段**：前者只认 `RunItemKind::is_interruption`，因此与 `NextStep::interruption()` 的校验不可能各说各话；后者按 `ToolLookupKey` / `AgentId` 统计而不是按名字，`mcp.github.search` 与 `mcp.gitlab.search` 不会被并成一个——那正是 R3-6b 点名禁止的「按可重名的 tool name 统计」。<br>**`has_tools_or_approvals_to_run()` 把 not-found 算进去**（与参考实现相反且是刻意的）：未解析的调用不用「跑」，但它照样欠一份配对输出，漏掉它下一次请求就带着一个没有结果的 tool call 出去，provider 一律拒收。把它排除等于让 `false` 在「明明还有事」的场合读成「没事可做」——静默的那个方向。<br>**`ProcessedResponseBuilder` 只收整条 `RunItem` 再自己取 call**：同时接收 item 与 call 的签名允许造出 `call_id` 与自身记录对不上的动作，而那个错要到输出配错调用时才现形。`build()` 另查四件事：item id 唯一、跨类别 `call_id` 唯一（两个动作认领同一个 call 就会回两份输出）、MCP `request_id` 唯一，以及**每条欠答复的 item 都必须被某个动作认领**——走 `item()` 混进来的 `ToolCall` 会让 `has_tools_or_approvals_to_run()` 答「没事可做」，而响应里还压着一个没人会回的 call。新增 `RunItemKind::requires_action_binding()` 承载这条判据，match 与 `is_interruption` 一样穷尽：新 item kind 必须显式表态自己欠不欠答复。`ToolApproval` 刻意为 `false`——它是模型产不出来的控制面记录，由 `is_interruption` 那条线接管。<br>**分类不改写记录**：handoff 以普通 `ToolCall` 的线上形态到达时，`new_items` 保留 provider 原样，类型化视图挂在动作上（`HandoffCall::with_tool_name` 记下模型实际用的名字），会话与 replay 因此保持精确。<br>**新增 `ra-runtime::turn::prepare::TurnActionSurface`**：从 agent 重新推一份等于用**声明的**工具而不是**本轮启用的**快照解析名字，被 `is_enabled` 关掉的工具会重新变得可调用。快照**在准备阶段第 2 步就封好并存进 `PreparedTurn`**，不是等调用方来取时才建——重名校验因此发生在模型调用之前而不是结算之后，R17 落地后先 `into_request()` 的 runner 不会把一个有歧义的动作面发出去再花钱。拒绝「一个名字既是工具又是交接」：handoff 与 tool 共用线上命名空间，留着它就得在结算里随便挑一边。第 5 步的 `reconcile_tool_surface` 也改读 `surface.advertised_names()`，「本轮广播了什么」从此只有一个定义。结算走 `into_call() -> (TurnActionSurface, ModelRequest)` 一次接走两样，`into_request()` 保留给不做结算的调用方。<br>**已类型化的 `HandoffCall` 也要过本轮动作面授权**：指向没广播过的 agent 直接报错，未经授权的控制权转移跑起来比拒绝糟得多。<br>**验收**（`tests/it-core/tests/processed_response.rs` 9 条 + `tests/it-runtime/tests/response_classification.rs` 8 条）：动作与记录同进同出且保响应原序；wire 形态 handoff 保留 tool_name 且不改写记录；绑定错工具/错 item kind/目标不一致当场拒绝；重复 call_id 与重复 item id 拒绝；三类欠答复 item 走 `item()` 混进来被拒、已答完的输出与控制面记录放行；not-found 计入待办判据；MCP 审批同时进两个类别；`tools_used` 按查找键去重；动态关闭的工具结算阶段解析不出来；动作面在准备阶段就建好；工具与交接重名的动作面拒绝构造 |
| R3-3 | `SingleStepResult` | **DONE** | `ra-core::step::single` 落地 `SingleStepResult` + `SingleStepResultBuilder`：`original_input` / `model_response` / `pre_step_items` / `new_step_items` / `session_step_items` / `nested_history_owned_items` / `processed_response` / `next_step`。流式与非流式消费同一个值——两种结果形状就是两个 loop，第二个总会漂。<br>**`session_step_items` 没有默认值**：默认成 `new_step_items` 的那一刻，一个过滤了模型面记录的轮次就会静默把过滤后的集合当完整历史存进去，损失要到下一次会话才看得见。builder 强制显式给。<br>**`build()` 的十条不变量**（拆成六个命名函数，每个函数的名字就是它守的那条）：<br>① **`processed_response` 必须是 `model_response` 这一条的分类**——别的检查都拦不住这种错配，用量记的是一次调用、绑定的动作来自另一次、resume 重放的又是第三个故事。**按整条 `RunItem` 比对而不是只比 `ItemId`**：分类是逐条 clone 的，整体相等本来就成立，而「ID 对得上、内容对不上」正是只比 ID 会放行却错配照旧的那一格；分类哪天不再是非破坏性的，这一行就是报信的地方。<br>② **三个历史列表各自不许出现重复 `ItemId`**（`index_unique_items`）。用 `BTreeSet<&ItemId>` 建索引会把重复项**静默折叠**，而 R9-12 的对账正是按 id 认记录；`ProcessedResponse::build()` 早就查了同一条，这里不查就是两处不一致。<br>③ **模型产出的每条记录都必须进 `session_step_items`**。只查 `new_step_items` 会漏掉最危险的一格——那个列表允许被过滤，过滤成空时它自己的子集检查恒成立，而这一轮把模型说过的话一条没存。<br>④ **凡是「同一条记录的两份」都按 `ItemId` + `RunItemKind` 比对，不按整条 `RunItem`**（`check_stored_payload`，用在③⑤⑧三处）。`kind` 是模型可见的载荷，必须一致——否则一条同 ID 的 message 可以冒充模型真正发出的那个 tool call；而 `provenance`（R3-12）、宿主专用 `session_data`、原始 provider payload、`unknown` 恰恰是**为了让存下来那份更厚**才存在的字段，全在比较之外。这条切法同时封住了「只比 ID 放行冒充」和「整条相等误伤 R3-12」两边。<br>⑤ 送模型的项必须是存会话那份的子集（反过来允许）；⑥ `pre` 与 `new` 不许重叠（历史对账会重复计数）；⑦ **`session_step_items` 不许复用 `pre_step_items` 里的 id**——本轮产出与往轮产出按定义不相交，重叠意味着重复持久化或跨轮复用 id，两样都会打坏 R9-12 的游标式幂等追加。它在逻辑上**包含**⑥（因为⑤已保证 new ⊆ session），两条都留是为了报错措辞更具体，更具体的那条先跑。<br>⑧ `nested_history_owned_items` 只记 `ItemId` 且必须指向真实存在的记录（记副本就是第二份要同步的东西）；⑨ `Interruption` 携带的项必须在会话里（resume 读会话，否则永远答不上）、**必须真的是审批形状、且不许为空**——`NextStep::Interruption` 刻意可直接构造，那个校验构造器是约定不是闸门，闸门放在结算这一步，因为混进非审批项在这里才真正变成一个永远等下去的 run；同时必须**问全 `processed_response.interruptions()` 里的每一条**——停下来却只问一部分，剩下那条要等一个永远不会来的轮次；⑩ **待审批未清时不允许 settle 成 `RunAgain` / `Handoff`**——继续跑等于走过一个没人做过的决定，而 `FinalOutput` 允许（run 已结束，没人还欠决定）。<br>**中断项的包含关系是单向的，反向要求是错的**：`Tool::needs_approval` 触发时，审批项由**执行阶段**产生，模型响应里根本没有它。要求两边集合相等会把最常见的本地工具审批流直接判死。<br>**三条约束边界，给后续里程碑**：<br>· ③会挡住 R3-8「error handler 决定不把模型输出写进历史」的路径。刻意的取舍——静默丢历史要到下一次会话才看得见，这里失败则是响亮的。R3-8 若确实需要，应当加一个**有名字、有理由的显式出口**，而不是删掉这条不变量。<br>· ④用在⑤上等于宣布**模型面列表只能「筛选」不能「改写」**。今天成立：截断/裁剪属于 R10-6b 的 filter 链那条投影路径（跑在 loop 里、context processor 之后），操作的是 `ModelInputItem` 而非 `RunItem`；R5-8 裁的是往轮输出，落在 `pre_step_items`。若将来要「给模型截断版、会话存完整版」，会先撞在这条上。<br>· ⑨要求中断项在 **`session_step_items`** 里，比它自己给的理由（resume 读会话）严格一格——往轮持久化的项也在会话里，只是不在本轮新存的那批。今天不成问题（R3-1 的契约是「每一项都有决定才恢复」，往轮待审批不可能还挂着），R6-5 若出现「恢复后重提同一条待决项」的形态，这里是第一个撞上的地方。<br>**四类 guardrail 结果刻意缺席**：`InputGuardrailResult` / `OutputGuardrailResult` / 工具护栏三态归 R7-1 与 R7-3，用 `Vec<Value>` 或 `bool` 占位等于在契约写出来之前冻结错误形状——与 R3-1 给 `FinalOutput` 塞 `FinishReason` 而不是占位输出值是同一条纪律。类型 `#[non_exhaustive]` + 私有字段 + builder，R7 补上去不是破坏性变更。<br>**验收**（`tests/it-core/tests/single_step_result.rs` 13 条）：四份必需事实缺一即报错且报错点名它；会话项无默认值；分类与响应错配三种（换一条响应、同批但乱序、ID 相同内容被改写）都拒绝；模型产出未进会话被拒、带 provenance / session_data 的更厚会话记录放行、同 ID 换一份 payload 被拒；三个列表各自的重复 id 与 `session` 复用 `pre` 的 id 四种都点名报错；子集与重叠两条对账；待审批时三种 `next_step` 的允许/拒绝分界；漏问一条响应级待决项被拒、执行阶段生成的审批项放行；绕过 `NextStep::interruption()` 直接构造的非审批项与空列表在结算时仍被拒；中断项必须在会话里；嵌套归属 id 必须存在；`generated_items()` 前序在前 |
| R3-4 | turn 结算主流程 | **DONE** | `ra-runtime::turn::settle_turn` 四阶段固定顺序：分类（`process`）→ 回答每个动作（`batch`）→ 决定 NextStep（`resolve`）→ 记账成 `SingleStepResult`。每个阶段一个模块，后续要替换其中一个的里程碑各有**一个**插入点。流式路径（R3-7）消费同一个函数——两条结算路径就是两个 loop。<br>**工具执行链落在 `ra-runtime::tool::dispatch::dispatch_tool`**：调用方准入 → 重复准入（R3-6 插入点）→ 审批 → 入参护栏（R7-3 插入点）→ invoke（带 `timeout`）→ 出参护栏（R7-3 插入点）→ `ToolCallOutput`。**每个会拒绝的阶段都靠返回值拒绝，不靠抛文案**。审批的静态策略从声明直接作答、不进第三方代码，与准备阶段处理动态可用性同一套做法。<br>**框架错误文案绝不能进模型上下文**（实现时发现的硬约束）：`Error` 的 `Display` 是 ``工具 `{tool}` 失败（{kind:?}）：{message}``、`user_message()` 也是中文，两者都是写给日志与人看的。把任何一个拼进 tool result，等于把框架散文塞进模型上下文并让下一轮依赖它。模型可见的失败观察因此**只有 `{code, tool}` 两项**，没有任何散文；工具想对模型解释失败，走 `ToolFailureHandling::Custom` + `Tool::handle_failure` 自己写——那是唯一一条把文本送进上下文的路径，且写它的是工具自己。<br>**取消永不降级成观察**：`error.is_cancelled()` 在失败成型的最前面短路。把取消报成一条工具失败，会让 loop 越过那个叫它停下来的东西继续花钱。超时则按 `timeout_behavior` 分流，其余按 `failure_handling` 分流。<br>**取消检查点在 `execute_actions` 的入口，不只在执行循环里**（初版的缺口，审核时发现）：唯一的检查点原本在 `for action in processed.functions()` 的循环体内，`functions()` 为空时循环体一次都不进，整个函数**没有任何可达的检查点**。四种响应形态里三种漏掉——只有 not-found 的会 settle 成 `RunAgain`，只有 MCP 审批的会 settle 成 `Interruption`（向宿主要一个此刻没人该回答的决定），而**纯消息无动作的会 settle 成 `FinalOutput{Final}`**：`is_complete()` 为真意味着 R15 认为不欠收尾、R17-3 走成功边，取消被报成了「agent 自己做完了」，正是 `FinishReason` 拆出来要防的误判。一轮是否报成取消，**不能取决于模型这次恰好点了什么名字**。not-found 循环前另有一次检查，不是冗余：上面的循环有 `await`，取消若在最后一次 dispatch 完成时才到达会输掉这个竞争。<br>**每一个动作都必须被回答**：跑成功的、要人批的、和解析不到工具的，都留下一条配对到 `call_id` 的记录。悬空 tool call 会让下一次请求非法，provider 一律拒收。not-found 用与工具失败**同一个结构**（`{code, tool}`），模型只需要认一种错误格式。<br>**`resolve_next_step` 的分支顺序就是优先级规则**，写死一处：待决 > 交接 > 工具结果被提升为最终答案（R3-5 插入点）> 还有东西欠答复 → `RunAgain` > 收尾。最后一条**刻意不看消息内容也不看 `OutputPhase`**：「没要任何动作」是每个 provider 都一样表达的结构事实，从文本里读意图正是 R7-10 要禁的。<br>**item id 由 `call_id` 派生**（`{call_id}.output` / `{call_id}.approval`）而不是随机生成：回答的身份**就是**「这个调用的输出」，replay 稳定，R9-12 不用侧表就能幂等追加。两个函数集中在 `batch.rs`，R9-12 若决定改由会话拥有身份，只改这一处。<br>**刻意留的桩与边界**：执行是**顺序**的（R3-4b 换成读并行/写串行的真实批形态，R3-4c 管并发怎么收与怎么取消）；`check_for_final_output_from_tools` 恒返回 `None`（R3-5 的 `tool_use_behavior`），返回 `Option<FinishReason>` 而不是 bool，这样 run 结果自己说得清为什么停；`execute_handoffs` 遇到交接**明确报错**（R17 才有 `AgentId` → `AgentSpec` 的注册表），今天不可达——准备阶段不广播 handoff，分类就产不出来——但它响亮地失败，而不是当成模型什么都没要。三个 guardrail/去重桩保留 `Result<()>` 并带 `#[allow(clippy::unnecessary_wraps)]`：收窄返回类型会把调用点的 `?` 摘掉，而一个读起来像 no-op 的阶段正是会被人顺手删掉的那种。<br>**验收**（`tests/it-runtime/tests/turn_settlement.rs` 18 条）：无动作响应直接 `FinalOutput{Final}`；工具跑完 → `RunAgain` 且输出按 `call_id` 配对、本轮记录按发生顺序排列；not-found 拿到配对失败观察并逼出下一轮；需要审批时工具**一次都没跑**、干净停下且审批项进会话；MCP 审批与工具审批一起问全；失败三态（ModelVisible / Propagate / Custom）分流；超时两态分流；取消不降级成观察、已取消的作用域一个工具都不跑，且**已取消时不看模型这轮要了什么都报成取消**（表驱动覆盖 not-found / MCP 审批 / 纯消息三格，注释写明每格原本会 settle 成哪一个谎）；不接受该调用方的工具从模型侧看就是不存在；交接明确报错；一轮三个调用各自拿到自己的观察；结算结果带原始输入与前序项并通过 `SingleStepResult` 全部对账；另有一条**反向断言**：`Error` 的 Display 确实是中文，所以它绝不能出现在观察里 |
| R3-4b | **循环形态：一思 → 流式派发 → 批量观察** | **DONE** | **Codex 不是严格「一思一动」**。288 轮/11,215 次 `exec_command` 的实测切片：`reasoning → ACTION(exec) ACTION(exec) ACTION(exec) → observation×3 → reasoning → [报幕] → ACTION…`（一次 3 个 `sed` 读不同行段）。**这是它读代码快的结构性原因**；做成严格串行，长任务轮数与墙钟会数倍于它。<br>**本条的形态按本机 Codex 源码（`070a26a1f0`）改写过一次，原来写的「收集全部 tool_call 再批量执行」不是它的做法，而且更慢也更复杂。**三件事：<br>① **不收集，边流边派发**（`core/src/session/turn.rs:2356`）：响应流里每来一个 `OutputItemDone` 就地 `tokio::spawn` 派发，句柄 push 进 `FuturesOrdered` 保序，流结束后统一 `drain_in_flight`。**工具 #1 在模型还在吐 #2 的 token 时就已经在跑了**，省下的是整段生成时间，而「先收集再批量」把这段时间白等掉。<br>② **并行资格是每个工具自己声明的，不是调用点按类别猜**（`ToolExecutor::supports_parallel_tool_calls()`）。rusty-agent 的落点是 `ToolOptions::concurrency`（`ToolConcurrency::{Exclusive, Parallel}`，默认 `Exclusive`，R2-1 已落地）。<br>③ **门是一个 `RwLock<()>`**（`core/src/tools/parallel.rs:153`）：声明 `Parallel` 的拿 `read()`，`Exclusive` 的拿 `write()`。十行代码，写类天然对所有其它调用互斥，不需要按文件加锁的第二套机制。<br>**一条与原计划相反的实测**：Codex 的 `exec_command` 与 `shell_command` **都声明 `supports_parallel_tool_calls = true`**，只有 `apply_patch` 不声明（默认 false → 独占）。它**没有**把 exec 拆成「只读命令 / 写命令」再分别决定——那需要先解析命令语义，成本远高于收益。原计划里「`exec_command` 只读命令」这个限定因此删掉：并行与否由工具整体声明，命令级的危险性归 R7 的审批与沙箱管。<br>**`multi_tool_use.parallel`**（模型主动把多个调用打包成一次）仍可作为 v2 增强，但有了 ①③ 之后它只省 schema 不省时间，优先级下调 |
| R3-4c | **并发工具的结算与取消语义** | **DONE** | `turn::batch` 以 `JoinSet` 监督每一个已派发调用；为使任务可安全拥有到终态，`TurnSettlementRequest` 与 `ToolDispatchRequest` 都持有 `Arc` 上下文、调用身份、参数与取消域，而非把借用塞进后台任务。每个调用都有 `ScopeKind::Tool` 子作用域；任何可传播错误会以 `PeerFailure` 取消其余子作用域，却不改写父 turn 的根因。收集器在同一调度轮内先取尽已就绪任务，再按 `UserError` > `GuardrailTripwire` > `ToolTimeout` > 其它（取消为独立终态）择优；取消后的非取消迟到失败继续合并，落选错误写入 trace，绝不静默丢弃。取消或同伴失败后，`DRAIN_GRACE` 内持续 `join` 全部任务；到期才 `abort_all` 并再次 join 到空，panic / 异常 join 均记录。`settle_dispatches()` 是唯一写入 `TurnExecution` 的点，因此收集或 drain 中断不会留下半批 `new_step_items`。<br>**两处审核时补上的口子**：① 仲裁表的 `ToolTimeout` 那一格原本**一次都没被测到**——测试工具只声明了 `failure_handling: Propagate`，而 dispatch 对任何 `ToolErrorKind::Timeout` 一律先走 `timeout_behavior`（默认 `ModelVisible`），于是第四个工具变成了一条观察，压根没进仲裁，「四个类别同时完成」这句验收是假的；② `drain` 那条路上的迟到失败合并同样零覆盖——四个工具卡同一个 barrier 会在同一调度轮被取尽，全走 `direct`，`cancelled_teardown` 从未发生。第二个口子难测但**不是不可测**（第一版审核里判成了不可测，随后推翻）：`CancelScope::run` 用 `futures::future::select`，而它**确定性地先 poll 调用、Pending 时才看取消分支**，所以「结果与取消落在同一次唤醒」时调用赢——这正是 drain 里那个非取消分支存在的原因。确定性构造：让工具停在一个**读标志位就绪、但从不注册 waker** 的 future 上，置位不唤醒任何东西，于是它拿到的下一次 poll 真的就是 `cancel_tool_scopes` 触发的那次。于是迟到失败有两条各自独立的入口，各有一条测试：工具自己返回的失败（`Ok` 分支，按错误类别排名）与清理阶段任务死亡（`JoinError` 分支，按 `RankedFailure::task_failure` 排名，即 `_background_cleanup_task_exception_message` 那一格）。<br>**运行时前提写进模块文档**：drain 用 `tokio::time::timeout` 计时，宿主 runtime 没开 `enable_time` 会 panic，且这条路在**结算主路径**上（不同于 R3-7 那个游离 reaper，丢的只是 abort 兜底）；`abort_all` 之后的 join 刻意不设第二道 deadline——放弃就等于 drop `JoinSet`，那是 detach 而不是取消。<br>**验收**（`turn_settlement` 26 条）：四个失败类别按表逐行择优（每行的赢家都**声明在最后**，让「先完成的」和「模型顺序靠前的」两种解释同时失效）；同类失败由模型顺序决胜；工具自己的迟到失败在 drain 里被合并——它在模型顺序与完成顺序上**都是后一个**，只能靠错误类别赢，反向那行则证明「来得晚」本身不构成对批次结果的主张；清理阶段异常单独一条，既能凭模型顺序改写批次结果、又不会因为借用了 `Error::Caller` 这个容器就压过 guardrail；父取消后两个在途调用均完成清理；连同既有并发、上限与门锁回归。`exclusive` 门锁那条注明必须留在单线程测试 runtime——派发顺序等于 spawn 顺序，只在单线程 FIFO 下才等于模型顺序，批次承诺的是两种许可互斥，**不是**模型顺序决定谁先拿到（资源级准入是 R3-4d）。R17-4 复用本语义。 |
| R3-4d | **资源 / effect 级并发准入** | **DONE** | `ra-core` 提供可序列化的 `ResourceId` / `ResourceClaim{resource, access: Shared\|Exclusive}`；资源 ID 在构造和反序列化时均校验，`ToolOptions` 反序列化也拒绝 `Exclusive + resource_claims` 的无效组合。`ra-runtime` 保留 v1 的全局 `Exclusive` / `Parallel` 门，并为 Parallel 调用按稳定资源顺序取得共享或独占锁；动态 claim 在审批后、调用前求值，锁只持有到 handler 结束。`ra-exec` 仅构造 workspace / process 身份，runtime 仍只依赖 `ra-core`。`read_file::rooted` 声明共享 workspace claim。准入等待与 handler 执行分别记录。<br>验收覆盖同资源读写互斥、不相交资源并行、倒序多资源无死锁、取消释放、审批不持锁、动态 claim、旧工具兼容和并发上限；`cargo tree` 确认 `ra-runtime` 依赖面未扩大。 |
| R3-5 | `tool_use_behavior` | **DONE** | `ra-core::agent::ToolUseBehavior` 已成为不可变 `AgentSpec` 的一部分，默认 `RunLlmAgain`，并由 `to_builder()` 原样继承。另有 `StopOnFirstTool`、`StopAtTools{names}`（bare 模型名或 `ToolOrigin::qualified_name`）与 `Custom(Arc<dyn ToolUseBehaviorHandler>)`；custom 接收本轮 `ToolUseResult` 的只读、模型顺序切片，异步返回 stop / continue，错误原样传播（不折成 stop 也不折成 continue——拿不定主意的策略就是没决定）。**custom 是第三方 async 代码，走 `CancelScope::run` 而不是裸 `.await`**（审核时改的）：初版裸等，实测一个挂住的 handler 能让整个 run 在 `scope.cancel()` 之后不返回，正是 `Cancellation_Contract.md` 第 47 行那条硬规则和第 164 行反例表点名的形态；**取消检查点在 await 之后而不是之前**（第二轮审核补的，初版放在前面）：入口检查是 `CancelScope::run` 自己的职责，重复一遍只是复述它的契约；真正会漏的是出口——`run` 内部的 `select` 先 poll handler，handler 若在「送达中断的那一次唤醒」里恰好就绪就直接返回，取消分支根本不会被看到。下游没有任何补救：结算之后不再 await，runner 拿到 `FinalOutput` 直接 break，于是一个被用户叫停的 run 结算成 `ToolStop`——`is_complete()` 为真、走成功边、不欠收尾。这与 R3-4c drain 里那个非取消分支是同一个 `select` 顺序，只是从另一侧读。策略是否被这个运行时认识，**在看结果之前**判——否则同一份配置报不报错取决于模型这轮恰好点没点工具。`ra-runtime::turn::batch` 在唯一结算点生成这份结构化投影；`resolve_next_step` 仍按 `interruption → handoff → tool stop → run again → final` 的固定优先级裁决。<br>**「观察」不等于「结果」，分界线是 `is_error`**（审核时改的，初版按「凡 `Observed` 皆是结果」）。每条观察都要回答给模型，但只有**跑成了并产出了值**的那些才进 `tool_results`。这是三个策略共同的不变量：`StopOnFirstTool` / `StopAtTools` 无从检视自己停在什么上面，而 `ToolStop` 的 `is_complete()` 为真——R15 认为不欠收尾、R17-3 走成功边。初版漏掉的两格实测都成立：① 准入被拒的调用（`allowed_callers` 不含 `Direct`）答 `tool.not_found`，工具**一次没跑**，却让 `StopOnFirstTool` 报出 `Completed{ToolStop}`——而同一语义的另一半、turn 从没广播过的名字（`tools_not_found`）本来就被排除，两条路自相矛盾；② 工具跑了并失败（`ModelVisible`）同样收尾，模型连那条失败都没读到。`ToolFailureHandling::Custom` 自己写出的输出**仍然算结果**——那是工具主动交给模型去用的值。与 OpenAI Agents SDK 的差异就在这一条：SDK 把失败结果也当可提升的结果（它有 `final_output` 可以承载错误文本，本项目在 R1-16 之前没有）。想对齐 SDK 是 `settle_dispatches` 里去掉一个 `if`。命中策略时终态是 `FinishReason::ToolStop`，但**不是 assistant 交付**：本轮报幕仍为 Commentary，`final_message()` 为 `None`，工具观察仍在 `RunResult::new_items()` 中；R1-16 才定义将任意工具值提升为宿主输出的统一形状。`StopAtTools` 的名字**不校验是否存在于工具面**：动态可用性逐轮收窄、R13 的 MCP 工具运行时才进面，build 时校验会误杀即将成真的名字；代价是打错的名字永远不匹配，已写进类型文档。验收（`runner_loop` 29 条）：默认两轮循环不变；`StopOnFirstTool` 执行完整并发批次但只调模型一次；名字匹配和限定名匹配各覆盖；Custom 可看到有序完整结果并分别覆盖 stop / continue；审批中断优先于策略；**准入被拒与工具失败两格都逼出下一轮且观察仍配对留存**；**取消赢过 custom 策略无论它有没有作答**——两行：handler 永不就绪（断言「它会 resolve」本身），以及 handler 在取消那次 poll 里就绪并回答 `Ok(true)`（用「读标志位就绪但从不注册 waker」的 future 把这个 race 变确定性）；custom 的错误原样传播。后两条已用回退实现验证过会红。 |
| R3-6 | `reset_tool_choice` 与工具循环熔断 | **DONE** | **复位落在 `turn::prepare` 第 5 阶段、作用于 resolved 值**（`ResolvedModelSettings::reset_tool_choice()`），不写任何输入层。写进 run override 层看着更省事，但那是四层里优先级最高的一层：一旦写入就永久盖住 agent 自己声明的 `tool_choice`，交接之后对新 agent 依然生效，而且它不随 `RunState` 持久化，续跑时强制选择会复活。复位是「刚结算的这一轮」的事实，只能作用在设置定型的地方。<br>**只释放 `Required` / `Tool` / `Mcp` 三种强制选择**：`ToolChoice::None` 是 host 关掉工具调用的意图，不能因为模型违规调用了一次工具就被重新打开（`reconcile_tool_surface` 早就把 `None` 列为要守住的意图，两处必须一致）；`Auto` 本就不强制；未知变体保留不猜。判据仍是计划指定的 `used_any_this_turn`，`TurnPreparationRequest::new` 因此多一个**必填** `&ToolUseTracker`——可省略等于允许存在一条永远不复位、每轮强制同一个调用直到 turn 上限的 run。<br>**熔断按返回值拒绝**：`ra-runtime::circuit::admit_repeat` 返回 `Option<Error>` 而不是 `Result<()>`，签名上就不可能被 `?` 冒泡；dispatch 以 `tool.repeated_call` 观测投给模型（新增 `ToolErrorKind::RepeatedCall`，与 `ExecutionFailed` 分开——熔断发生在调用之前，工具根本没跑）。若走 `Err`，整条 run 已生成的 items 全部丢失、同批其它无关调用被连带取消，而唯一需要改变行为的模型什么都收不到。<br>**阈值默认关闭、按工具 opt-in**（`ToolOptions::with_max_repeat_streak` / `without_repeat_limit`）。被拒的调用照样记 attempt，streak 只增不减，触顶即单向闩锁；`DEFAULT_MAX_REPEAT_STREAK`(3) 保留为推荐值而非默认。`read_file` 明确**不**开：读可变状态时参数相同不等于证据相同，改完回读验证正是最先被误伤的调用，理由写在它的 `options()` 上，`tests/it-tools/tests/read_file.rs` 有一条断言钉住「当前决定是不开」。真正的判据由 R3-6c 的 evidence fingerprint 提供。<br>**验收**：`tests/it-runtime/tests/runner_loop.rs`（强制选择在模型用过工具后释放且第二轮不带任何选择、host 的 `ToolChoice::None` 两轮都保持、重复调用被拒但 run 正常收尾且被拒调用带 `tool.repeated_call`）+ `tests/it-runtime/tests/turn_preparation.rs` 第 13 条（没用过工具时不动、本 agent 用过则释放、**别的 agent 用过时本 agent 仍被强制**——这条是跨 agent 污染的回归）+ `tests/it-core/tests/tool_contract.rs`（默认关闭、opt-in 往返）。 |
| R3-6b | `AgentToolUseTracker` | **DONE** | 落地形态：`ra-core::state::tool_use`（`ToolUseTracker` / `AgentToolUse` / `ToolUseEntry` / `ToolUseRecord` / `ToolUseAttempt` / `ArgumentFingerprint`）+ `ra-core::step::processed::ProcessedResponse::attempts()` 投影 + `ra-runtime::turn::settle_turn` 的记账点，再由 `turn::batch` 把当前 (agent, 工具) 的 `repeat_streak` 投给 `dispatch::admit_repeat`。<br>**身份只有一处推导**：`ToolRunFunction` / `ToolRunHandoff` / `ToolRunApproval` / `ToolNotFound` 各有一个 `identity()`，`ProcessedResponse` 的投影与 batch 的查表都调它。两处各自拼一遍的话，分歧会让熔断器去查一个从没被记过的身份——永远读到 0，环还在转，而且没有任何断言会挂。<br>**两把键都不是名字**：外层键是稳定 `AgentId`（`AgentSpec` 的 `name` 是**刻意允许重名**的展示元数据），内层键是 `ToolUse`——它带的是 `ToolLookupKey` 而不是模型可见的工具名，`mcp.github.search` / `mcp.gitlab.search` / 裸 `search` 是三个身份。按名字合并会让熔断器同时做两件错事：在两个毫不相干的工具上误报，又漏掉真正的重复。<br>**类型放 `state` 而不是 `step`，是因为持久化会把 wire 形态钉死**。`step` 整个模块的稳定性等级是 `Internal`（「随时可以重构」），而一个存进 checkpoint、恢复时还要读得回来的值不能带这个承诺。`ToolUse` 因此**从 `step::processed` 移到 `state::tool_use`**，再从原处 `pub use` 回去，`ProcessedResponse::tools_used()` 的签名不变。计划表里写的挂载点是 `ra-runtime::ToolUseTracker`，这里改成 `ra-core`：`RunState` 属于 `ra-core`（分层第 1 条），而 `ra-core` 不能反向依赖 `ra-runtime`——挂载点写在 runtime 就等于 R6-6 落地时无处安放。逻辑仍然在 runtime，与 R3-2「类型在 core、分类在 runtime」是同一套划法。<br>**参数只留摘要，不留原文**：`ArgumentFingerprint` 是归一化 JSON 的 SHA-256。① 工具参数是模型往里塞什么就有什么（路径、查询、用户粘进来的凭据），而这个结构会进**每一次** checkpoint 写盘；摘要回答了消费方唯一真正在问的问题（「和上次是同一个调用吗」），同时把载荷整个拿出快照。② 归一化不是锦上添花：provider 不承诺 JSON 键序，按原始字节做指纹会让「模型正在原地打转」被读成「换了个调用」。③ canonical 化**显式排序**而不是靠 `serde_json` 默认的 `BTreeMap` 背书——`preserve_order` 是 additive feature，依赖图里任何一个 crate 打开它都会让全量指纹随链接的 feature 集合而不是随数据变化。<br>**连续段按「这个身份自己的调用序列」算**，不是按 agent 的整条调用序列。严格的「中间插进别的工具就归零」读法会漏掉最常见的一种环：卡在 `read(f)` / `grep(x)` 交替里的 agent，两条段各长 1，等于完全看不见。`repeat_streak` 是四个字段里**唯一存储而非投影**的，理由写在类型上：它是对完整序列的折叠，而完整序列被刻意丢弃了——`recent` 有 `TOOL_USE_RECENT_LIMIT`（8）的上限，无界轨迹会随 run 变长并在每次 checkpoint 整份重写。<br>**`record_turn` 本身就是轮边界**，没有单独的 `begin_turn`。一个能被忘记调用的重置方法，忘了之后 `turn_calls` 会永远等于 `run_calls`，而 `reset_tool_choice` 读到的就一直是「这轮用过工具」——静默的那个方向。同一次调用里先把该 agent 所有身份的本轮计数清零，再按顺序记账，这一轮没被点名的工具因此读成 0 而不是停在上一轮的值。<br>**重放同一轮不改变任何计数，靠两级判据**：恢复一个中断就是把同一条响应再结算一遍，重复计数会让模型看起来正好比实际多一倍爱打转，而熔断器正是按这个数字动手的。① **刚记过的那一轮，不限长度**——每个 agent 存一份整轮有序摘要（身份 + `call_id` + 参数摘要，逐段 length-prefix）；② **更早的调用，在本身份窗口内**——按 `call_id` 单独认。第一级不是冗余：一条响应对同一身份的调用数可以超过窗口，此时逐个检查会**级联**——每重记一个就挤掉下一个待检查的，整轮全部翻倍（实测 `run_calls` 18 vs 9）。本轮计数照常按 attempts 重算，重放后读到的值与第一次一致。两级都以 `call_id` 唯一为前提，这与框架里每一处按 `call_id` 配对的地方是同一个假设。<br>**记账点在结算的阶段 1 与阶段 2 之间，位置就是理由**：R3-6 的熔断器住在 `dispatch_tool` 里，也就是阶段 2 的下游，记在执行之后等于让熔断器看得见每一轮、唯独看不见正在问的这一轮。记的是**模型要了什么**而不是执行结果——被拒的、超时的、解析不到的，都是模型又要了一次同样的东西。`TurnSettlementRequest` 因此多了 agent 身份与 `&mut ToolUseTracker`，**两者都是必填**：可省略的 tracker 等于允许存在一条重复度永远为 0 的 run。（R3-12 落地后这个参数已从裸 `AgentId` 换成 `AgentBinding`，settle 自己取 `public_id()`，归属不跟着 sandbox clone 走这条约束因此由签名担保而不是靠注释。）<br>**跨版本**：四层结构各带 `schema_version` 与 `#[serde(flatten)] Unknown`，未知字段原样回写；`AgentToolUse` 用手写 `Deserialize` 拒绝同一身份出现在两条 entry——计数被劈成两半时每个消费方读到先撞上的那一份，熔断器要两倍重复才会响，而记录本身看起来完全正常。<br>**验收**（`tests/it-core/tests/tool_use_tracker.rs` 13 条 + `tests/it-runtime/tests/tool_use_tracking.rs` 6 条）：两个 agent 各记各的账、没跑过的 agent 读连续段是 0；同名不同来源的三个 `search` 是三个身份且按首次使用排序；指纹归一化键序（含嵌套）但区分取值与数组顺序；连续段只被**本工具**的参数变化打断、交替调用不打断；本轮计数每轮重置而累计继续、空轮的 agent 依然在册；未解析名字/交接/托管审批都算「用过」；窗口有上限而累计与连续段不受限、挤掉的是最旧的；同一 `call_id` 记两次不翻倍且本轮计数不变；**超过窗口的单轮重放仍然完全幂等**（去掉整轮摘要这条会红）；动作自报的身份与投影记账用的是同一个值；序列化往返后继续记账且**参数原文不出现在快照里**；更高版本写下的三层未知字段原样回写；重复身份当场拒收；`ToolUse` 四变体往返且 deferred 与裸工具不相等。runtime 侧：四类动作按响应原序全部记下（纯消息不算）；跨轮连续段累加、参数一变归 1；失败与 not-found 照记；停下来要审批那一轮工具一次没跑但已记账；交接报错的那一轮记账仍在；同一条响应换 agent 身份分账且去重窗口不串台 |
| R3-6c | **结构化失败记录与无进展熔断** | **DONE** | 落地形态：`ra-core::state::tool_failure`（`ToolFailureTracker` / `AgentToolFailures` / `ToolFailureEntry` / `ToolFailureRecord` / `ToolOutcome` / `EvidenceFingerprint`）+ `ra-runtime::turn::settle_turn` 阶段 2b 的记账点 + `ra-runtime::circuit::admit_progress`。与 R3-6b 的调用轨迹**并列而非合并**：一个记「模型要了什么」（执行前，熔断器必须看得见本轮），一个记「回来的是什么」（执行后才存在），两个 `schema_version` 各自演进，都挂在 `RunState` 上（`trackers_mut()` 一次借出两个——两个 `&mut` 访问器会各借整个 struct，调用方拿不到第二个）。<br>**按 evidence 计数，不按 input**，这是与 R3-6 熔断的根本分工：改了路径又失败的模型换了调用却什么也没学到，重跑同一条测试命令拿到更短失败列表的模型重复了调用却学到很多。所以 streak 在**模型可见结果**逐字节相同的连续失败上累加。degenerate 情形是诚实的而不只是方便的：默认失败投影只给模型 code + 工具名，两个不相干的失败因此是同一份证据、照样累加——被要求换思路的那一方本来就分辨不出这两次失败；想被区分，工具得用 `ToolFailureHandling::Custom` 自己说话。<br>**触发即清零，不闩锁**：拒绝什么都没跑，说不出关于工具的任何事；而让 streak 挺过自己的拒绝，它就再也降不下来（只有结果能降，而现在产生不了结果）。真卡住的 run 因此每 `limit + 1` 次调用付一次被拒的代价，而不是丢掉这个工具。成功同样清零。<br>**派发多出第三态 `ToolDispatch::Refused`**：「工具跑了但失败」与「工具根本没跑」是两件事。按 `is_error` 分类会把熔断器自己的拒绝当成关于工具的证据（实测：拒绝的 code 与原失败不同 → 被判成新证据 → 计数清零 → 工具又跑起来），也会对 `Custom` 失败完全失明——而那恰恰是唯一能让两次失败被区分开的工具类别。`ToolObservation` 因此携带失败码，分类由派发链给出而不是从渲染结果反推。<br>**准入只跨 response 生效**：outcome 要等调用跑完才存在，所以一轮响应里的所有调用都按「响应到达时」的同一个计数准入，拦截落在下一轮首个调用上。同身份链式串行（让每个调用看见前一个的 outcome）**试过并已回退**：链按 identity 建、熔断按 evidence 触发，等于把 `ToolConcurrency::Parallel` 从所有不显式豁免的工具上收回，去执行那些调用通常够不到的阈值——实测三读三个不同文件的峰值并发从 3 降到 1。若将来要「批内立即熔断」，必须是显式 opt-in 的严格批次策略，并在声明处写明它让同 identity 调用不再并发。<br>**默认开启、阈值 3**（`DEFAULT_MAX_NO_PROGRESS_STREAK`，`without_no_progress_limit()` 豁免——为等待某物就绪、失败形态必然重复的探针留的口子）。与 R3-6 相反的选择，理由是判据强得多且不闩锁：误判成本是一次被拒的调用，不是一个工具。<br>**未做 Guardian 的定长窗口双计数**：它要补的盲区（交替调用打断连续段）在这里不存在——本计数按身份分桶，只有该身份**自己成功**才会打断，而偶尔能成功的工具本就不该熔断；加窗口反而会对「最近五次都正常、只因窗口里还压着三次旧失败」的身份误触发。理由写在 `ToolFailureEntry::no_progress_streak` 上。<br>**memoization 桥接契约以文档形式写死**（`tool_failure` 模块文档），不是测试：cache hit 通常是成功、会清零计数，所以启用 memoization 时必须以「无新证据」的 observation 进入本判定；该功能暂缓，无法为它写运行测试。<br>**验收**（`tests/it-core/tests/tool_failure_tracker.rs` 11 条 + `tests/it-runtime/tests/no_progress_tracking.rs` 11 条）：同样失败但证据变化不误熔断；参数变化而错误/证据不变照样计数；成功清零且 streak 从下一次失败重新起算；身份与 agent 分账；同一轮重放不翻倍、**超过窗口的单轮重放**仍幂等；**checkpoint 恢复后重放旧成功/旧拒绝不清掉新 streak**（三种结局共用一个有界 `call_id` 窗口）；快照里不出现参数原文与输出原文；指纹归一化键序但区分取值；未知字段原样回写；重复身份当场拒收。runtime 侧：三次相同失败拒第四次；证据变化则第四次照常执行；`Custom` 失败仍记为失败；拒绝重新上膛而不是退役工具；**一轮响应内三次调用全部执行、结算后 streak 为 3、下一轮首个调用被拒**；**默认 options 下同一 `Parallel` 工具三次调用同时进入**（gated 并发回归——现有重叠测试用的是两个不同工具，复用同一工具的那条把并发上限设成了 1，两条都覆盖不到）；豁免工具不受限；停下来审批那一轮不记 outcome；纯消息轮不动任何计数。 <br>**待决（2026-09-09）：`max_no_progress_streak` 默认值是否改 `None`。** `ra-core::tool::options` 现有 `DEFAULT_MAX_NO_PROGRESS_STREAK = 3`、`with_max_no_progress_streak`、`without_no_progress_limit`，默认是**开启**。「记录退出状态」与「据此强制熔断」是两件事，前者无争议，后者是策略，需要判断它该不该是框架默认。**决策依据是它自身的收益、误伤面与产品需求**，不以任何外部实现的默认值为前置条件：上游 `openai-agents-python` 无对应机制；codex 的 `GuardianRejectionCircuitBreaker` 熔的是**安全评审的连续拒绝**（见第 137 行的核查），触发对象与后果都与工具失败证据不同，只能作机制参考，不能当同类默认值来对照。 |
| R3-7 | `Runner::run` / `run_streamed` | **DONE** | `ra-runtime::runner` 落地 `Runner` / `RunRequest` / `RunConfig` / `RunResult` / `RunOutcome` / `ContinuationInput` / `RunStream` / `RunStreamEvent`。<br>**一个 loop，不是两个**：两个入口都调 `run_loop`，流式那条只多一样东西——一个用来报幕的 channel。参考实现把这份差异铺在两条代码路径上；两条 loop 会漂，流式那条长出一个修复而另一条永远拿不到，而 bug 只在宿主恰好订阅时复现。<br>**loop 自己不判断该不该继续**：它 match `NextStep`，**没有 `_` 分支**，第五个状态是这里的编译错误。`Handoff` 分支重新绑定成 `AgentBinding::direct(new_agent)`——新 agent 是以 public 声明的形态到达的，准备了**这一轮**执行实例的东西对下一轮由谁跑没有发言权。今天不可达（结算拒绝交接），但状态机必须表态。<br>**中断是一种结局，不是错误**：`RunOutcome::{Completed{reason}, Interrupted{items}}`。把待审批折进 `FinishReason` 会让「做完了」和「在等人」对每个只看 `Ok` 的调用方长得一样，而报成 `Err` 则让宿主没有任何办法回答它再接着跑。<br>**`max_turns` 在这里而不等 R3-8，是刻意的越界一小步**：其余三个预算维度（cost / tokens / 墙钟）与「告诉模型还剩多少」都留给 R3-8，但轮次上限不是预算，是 **loop 自己的终止条件**——交一个只能从外面叫停的 loop，等于让后面每个里程碑的测试都带上挂死风险。默认 32，文档写明它是用来止损不是用来定规模的；`0` 当场报错而不是被读成「不限」。<br>**三份历史三个名字**：`original_input`（问了什么）/ `new_items`（产出了什么）/ `continuation_input(policy)`（下一次该发什么）。第三份是**带显式策略的投影**而不是第四个数组——`PreserveAll` 原样，`Normalized` 过 R1-17 的 normalizer。存成字段就会和第二份讲不同的故事，而漂移要到一个 session 之后才现形。`usage()` 同理，从 `model_responses` 求和而不是一路累加。<br>**`RunStream` 不是一根用完就没的管子**：事件与终态是同一次 run 的两个视图——读干净事件照样能 `finish()`，只要终态也不必先把事件读完。丢掉流会取消这次 run（否则 provider 还在为没人等的结果花钱），但取消的是一个**子作用域**，不连累调用方自己的。<br>**初版这条承诺是假的，审核时修掉**：`finish()` 原本在 `await` **之前**就 disarm 了守卫，而 tokio 的 `JoinHandle` 在 drop 时是**分离**不是 abort——`finish()` 的 future 一旦在 `select!` 里被放弃，守卫已解除、句柄已分离，后台 run 会一直调 provider 而没有任何人在等结果，正是那句文档承诺不会发生的事。守卫现在保持武装直到任务到达终态，`RunStream` 自己实现 `Drop`：先发取消信号，再把 join handle 交给一个短命 reaper，按 `DRAIN_GRACE` 等它收敛、超时就 abort。这不是加固，是补上 `DRAIN_GRACE` 文档点名的那条契约——「dropping a `JoinHandle` after cancelling leaves a running child process behind」（R3-4c ③）。回归测试用一个只会被取消才结束的 model + drop 通知，把「run 真的停了」变成可观测事件而不依赖计时。<br>**两条已知留白**：① 宽限期用 `tokio::time::timeout` 实现，宿主 runtime 没开 time driver 时 reaper 会 panic 并被吞掉，**丢的是 abort 兜底而不是取消本身**（信号在此之前已发出），已在 `run_streamed` 的文档里写明这个前提；② 兜底那一段没有断言守着——要构造一个在检查点之间不理会取消的任务，当前 runner 里够不到。<br>**两处实现时补上的静默缺口**：① `RunConfig::with_model` 一开始存了却没传给 `prepare_turn`——一个被静默忽略的 run 级设置，症状是账单和延迟对不上而不是一条报错，现已每轮下发并有断言；② `ToolUseTracker` 原本只进不出，`with_tool_use` 因此是条单行道，`RunResult::tool_use()` 补上交回，否则「恢复后接着数」这条 R3-6b 的性质根本无法达成。<br>**R1-7 的插入点是模型调用那一行**：今天走 `get_response`，订阅者在每一轮结算时拿到该轮全部记录（多轮 run 因此已经是增量的）；换成真流式后转发 provider delta，结算两边都消费终态 `ModelResponse`，周围一行都不用动。run channel（`RunStreamEvent`）与 model channel（`ModelStreamEvent`）是两个枚举，前者包住后者而不是扩展它——adapter 说不出「交接后公共 agent 变了」这种事。<br>**验收**（`tests/it-runtime/tests/runner_loop.rs` 12 条）：工具与最终回答之间来回直到模型不再要东西（含第二轮输入必须带上配对输出、usage 求和）；到达上限软收尾成 `MaxTurns` 而不报错；上限为零当场拒绝；待审批停下来是 `Interrupted` 且工具一次没跑；已取消的作用域一个模型调用都不发；续跑输入两种口径且原始输入在最前；流式逐轮推记录且报幕用公共身份、读完事件仍能取终态；只要终态时不必读事件；两条路径产出同一个结果；run 级模型覆盖每轮生效；工具轨迹随结果交回并接着数；丢掉流不连累调用方作用域 |
| R3-8 | 预算与上限 | **DONE** | `ra-core::budget` 已提供协议中立的 `BudgetLimit` / `BudgetSnapshot`：turn、token 与 absolute deadline 三维分别建模。**不设金额维度**（2026-08-13 定）——没有任何 provider 在 usage 里报金额，框架侧的数字只能来自一张内置价格表，而那张表按 provider / 模型 / token 类别各不相同、改价不通知，错了还不出声；逐请求 token 明细在每个 `ModelResponse` 上原样保留，宿主拿自己的合同价换算即可。openai-agents-python 是同一个取舍（`Usage` 里没有金额字段，`request_usage_entries` 的注释写明保留明细就是给宿主算钱用）。**三个上限（含 deadline）全在 limit 上，snapshot 只有花掉的计数**——limit 是配置、从不持久化，snapshot 因此整体可序列化、干净记录往返相等，不需要 `#[serde(skip)]` 字段（那种字段会让 `RunState` 的 `PartialEq` 在落盘前后不一致）。snapshot 与 `RunState` / `Usage` 一样带自己的 `schema_version` 与 flatten `Unknown`：它是 checkpoint 记录，新版本写的预算字段必须能被旧版本原样回写，父级的 `Unknown` 捕获不到嵌套层的未知键。snapshot 作为 `RunState` 一部分跨续跑保留计数，续跑段用新 limit 重新度量。`RunConfig::with_budget` 收敛原有 `max_turns`；**它是唯一能把 turn 上限清空的入口，因此无 turn 上限的 budget 在 run 起点被拒绝**——turn 上限是循环自己的终止条件，只能从外部停的循环在模型不断调工具时就是一次挂起。<br>**墙钟走 `CancelScope`，不另开一条平行通道**：run 起点按 `config.budget.deadline()` 派生自己的子 scope（`with_deadline` 只收紧不放宽，调用方更早的 deadline 自动生效），并 arm 一个 timer 把 `Deadline` 这份纯数据变成真正的取消——`ra-core` 不持有 runtime、arm 不了 timer，这个义务在此履行。在途模型调用、settlement 下正在跑的工具、循环自己的 checkpoint 同时看到它；**只 arm 模型调用那一段等于工具执行不受墙钟约束**，一个慢工具就能把 run 拖过 deadline 任意久。取消由 timer 落到本 run 的子 scope，因此不会波及调用方的树；出口处按 `CancelReason::is_expiry()` + deadline 确实到期这两个结构化条件（**不解析错误文本**）把它读回软停止，全循环只有这一个翻译点。<br>**扣减在 settlement 之前记账、但停止判定不在那里做**：这一份 response 已经付过钱，它的条目属于历史、它的工具调用属于发起它的那一轮，所以先结算完这一轮，由下一轮循环顶部的检查收口——否则模型刚给出的最终答案会连同整轮一起被丢掉。取消检查排在预算检查之前：同时成立时宿主对「被中断」和「额度耗尽」的反应不同。超限统一收为可续跑的 `FinishReason::MaxTurns` / `BudgetExhausted`，不将额度耗尽冒泡成失败。<br>**token 预算的 model-facing 提示走 input 尾部的 system 消息，绝不进 `instructions`**：instructions 是稳定缓存前缀，每轮变化的数字会把它每次请求都打碎（[A.4](#a4-claude-code-执行事件流) 与 R4-3/R4-4 的同一条约束）；提示不进 `generated`，因此不落历史、每轮按当前额度重算。R4-3 的尾部增量通道落地后它并入那条通道。<br>`RunErrorData` / `RunErrorHandlerInput` / `RunErrorHandlerResult` 与 `RunErrorHandler` 已落地：closeout handler 读结构化 `Error`、只读 run snapshot 与 **public agent 声明**（closeout 是替它说话，得知道替的是谁；execution 实例是框架内部，署它的名等于报出一个用户没配过的 agent），必须产生 final assistant message，并可明确选择是否写入历史；**返回 `None` 表示弃权**，run 就按没装 handler 的样子收场——一个 handler 要服务全部终止条件，就必须能说「这个不归我管」，否则宿主为预算装的 closeout 会在 R1-12 / R1-16 的 refusal 和非法结构化输出到达时，替一个它从没考虑过的情况开口。弃权不是错误处理，返回 `Err` 仍然让 run 失败。turn 与其余预算耗尽走此入口。**closeout 跑在一个兄弟 scope 上**：墙钟取消的是 run scope，如果 closeout 也挂在它下面，最需要收尾的那种停止反而永远收不了尾；兄弟 scope 让调用方的中断照样能停下一个卡住的 handler。`RunResult::usage()` 由 `model_responses` 求和投影而不是存一个累加字段——存总数就是第二个真相源，丢一次或重试一次就和它声称汇总的那些调用对不上。provider refusal fallback 与 invalid structured output 的生产点仍由 R1-12 / R1-16 实现，但将复用同一 handler contract。验收：core budget 契约 5 条（含 limit 侧墙钟、干净往返相等与未知字段原样回写）；runtime 12 条，覆盖尾部提示不动前缀且以 user 角色投递、扣减后仍结算本轮、最终答案不被预算吞掉、墙钟同时停模型调用与工具且被打断的那次调用仍留在 `model_responses` 里、取消优先于 turn 上限、续跑按已花额度度量、无 turn 上限被拒、closeout 的写/不写历史、非 final 消息被拒与 pending closeout 可被调用方取消。<br>**（2026-09-04 修）尾部提示改成 user 消息**：原来发的是 `Message::system`，而 input history 里的 system 文本不跨 provider——Anthropic 只在顶层 instruction 字段收 system，消息列表里一律拒（`ra-model/src/anthropic/request.rs`），所以**配了 token 预算的 run 在该 provider 上每个请求都当场 caller 报错**。与 R10-5 的 deferred 投递记录同一个坑、同一个解法：user 是 input history 跨协议唯一都认的角色。角色改了之后还有第二条边——压缩逐字保留 user 消息（summary 指令里明写「the runtime preserves those verbatim」），而压缩器拿到的 `visible_input` 是带尾部项的，不处理的话某一轮的剩余额度会被当成「用户原话」钉进 summary，在一条比该轮活得更久的记录里，旁边还挂着下一轮新发的提醒；`ra-context::compaction::carried_user_messages` 因此在读取「要逐字保留的用户轮次」之前先把请求的临时尾部摘掉。验收新增一条：预算与压缩同时装上时，summary 保留用户自己那句、不保留循环自己的尾部项。 |
| R3-8b | **最小可观测性基线** | **DONE**（trace 骨架与时延口径落地；四项计数待其依赖） | 落地形态：`agent` / `turn` / `generation` / `function` 四类 span 接到 loop 的**真实执行边界**，字段只放身份、计数、时长与稳定 code——prompt、模型正文、工具入参与工具正文一律不进 span，这条让 R14-2 的「关掉敏感数据」不至于把拓扑一起关掉。<br>**generation**：规范化的 input / cached-input / cache-write / output / reasoning 五维 token 与本次调用墙钟；cached 单列的理由同 R0-3。<br>**turn**：这一层才答得了「这一轮花了多少钱」——一个 turn 不是一次模型调用，它还含随后整批工具（将来还有重试与回退）。**本轮 usage 在 settlement 之前记录**，与预算计数器同一条论证：已经付过钱的响应，不能被之后失败的工具抹掉。`turn.index` 从 0 起（stream 事件是 1-based，转换只在一处）。<br>**function**：准入等待 / handler 执行 / 总时长三段分开。**span 必须在调用侧创建再 move 进 spawn 出去的任务**——tokio 不跨 spawn 传递 tracing 上下文，写在任务体里创建时看到的是空 span 栈，工具调用会变成游离根 span、脱离它所属的 run。这条已实测复现，是本条最容易悄悄写错的地方。<br>**agent**：聚合 usage、`finish.reason`，以及**哪个预算维度耗尽**（`FinishReason` 把 tokens/cost/wall_clock 压成同一个 `budget_exhausted`，不单列就分不出「贵」还是「慢」）。**失败与取消路径同样保留已消耗的 usage**：那些 token 是真花掉的，按终态过滤掉失败 run 会系统性少算成本。<br>**取消是归因，不是压平**：新增 `CancelScope::cancelled_scope()`——被 run 的 deadline 干掉的 tool scope，`kind()` 说 `Tool`、它说 `Run`，只有后者答得出「是哪一层的天花板先响」；另加 `BudgetKind::code()`。<br>usage / duration 这些字段名跨 span 类型复用而**尺度不同**，因此**聚合必须先按 `span.kind` 分组**——这条写进 `field` 模块文档，而不是另起一套 `usage.total_*` 把词表翻倍。词表新增 6 个常量，`api/ra-core.txt` 基线已更新。<br>验收对照：同一 mock trace 重放统计一致 ✓；失败 / 超时 / 取消 / 预算终止都被分类且不静默丢失 ✓（取消记 `cancelled` 而非 `error`，并带根因与**发起层级**）；等待段 + 执行段 ≤ 总时长、两段非负 ✓（用并发上限 1、两个调用、handler 40ms 的真实排队验证，不是只断言字段存在）；采集本身不改变模型输入与工具执行顺序 ✓（span 只读，不进 `next_input`，不改派发顺序）。9 条断言在 `tests/it-runtime/tests/runner_trace.rs`，**单独一个测试二进制**：`tracing` 的 callsite interest 缓存是**进程全局且惰性计算**的，线程局部 subscriber 隔离不了同 binary 里并行跑的兄弟用例，混在一起时全量跑约一半概率捕获为空。<br>**未做的四项，各自带挡着它的任务号**：重试次数（R1-9b，loop 里还没有重试）；compaction 次数与「压缩前后控制面一致」的确定性 fixture（R5-3 / R5-3b）；claim 冲突次数 / 拒绝次数 / 队列长度分布（要等 R3-4d 的资源准入上线才有对象）；memoization 命中率与陈旧命中数（memoization 仍暂缓）。**这四项一律不伪造数值**：字段词表已为它们预留，但没测过的数字不写进 span——占位数字会被当成真指标，比缺一个字段更糟。按任务 / agent / 图聚合、建基线与设 CI gate 仍归 R14-5。 |
| R3-9a | **`RunContext` / `ToolContext` 与运行身份契约** | **DONE** | **落地形态**：`ra-core::context::RunContext`——非序列化、宿主拥有，持 `RunId`、`RunAgent`、类型擦除应用上下文与 `BudgetSnapshot` 读视图；应用上下文的唯一读取门是 `app_context<T>()`（类型不匹配与没挂都返回 `None`）。**agent 视图是投影而不是 `AgentSpec` 本身**：`RunAgent` 只有 `id()` / `name()`，`RunContext::new` 从 spec 里取这两样。把整份 spec 递给工具会同时开两个洞——`model_settings().extra_headers()` 是 API key 所在处，工具读到就能写进模型可见输出；`tools()` 直接交出兄弟工具的 `Arc<dyn Tool>`，工具可绕开 caller 准入、重复/无进展断路器、审批、guardrail、超时与全部派发记录去调它。以后要加字段就继续加投影字段。`ra-core::tool::ToolContext<'_>`——由 `ToolInvocation` **直接改名演进**，持 `&RunContext` + 权威 `&ToolOrigin` + `CallId` + 参数 + `ToolCaller` + `&ToolServices`，工具抵达应用上下文只有 `ToolContext::run()` 这一条路。`ToolContext::new` 收 `&dyn Tool` 并**自己推导 origin**（不收裸 `&ToolOrigin`，否则调 A 记成 B 的身份会一路流进记录、trace 与无进展身份），但只存推导出的 `&ToolOrigin`——存 `&dyn Tool` 等于把上面那条绕行洞重新开回来。`ra-core::tool::ToolServices`——端口袋，具名访问器，今天只有 `work_state()`。`RunId` 落在 `ra-core::state::run`（与将持有它的 `RunState` 同处），只有 `new()` / `generate()`，没有 `Default`。`Tool::call` / `needs_approval` / `handle_failure` 收 `ToolContext`，`is_enabled` 收 `&RunContext`；`ToolRuntimeContext` 与 `ToolInvocation` 已从公开 API 消失（见 `api/ra-core.txt`）。四层链改为 `RunRequest{run_id, app_context, services}` → `TurnSettlementRequest{run, services}` → `TurnExecutionRequest{run, services}` → `ToolDispatchRequest{run, services}` → `ToolContext`；`RunRequest::with_work_state` 删除，改由 `with_services(ToolServices::new().with_work_state(..))` 进入。`RunContext` 由 `runner::live_context` 在准备与结算两处各自从 `RunState` 投影重建，所以结算侧的预算读视图含本轮已付 usage，而累加仍只发生在 `RunState` 上。<br>**本条按计划刻意未做**：`RunState` 还没有 `run_id` 与 `next_host_event_seq` 字段，跨恢复段的身份/序号连续性归**最小 R6-6a identity slice**（下一条），R8-0 仍以它为前置。<br>**验收落点**：`tests/it-core/tests/tool_contract.rs`（12/13/14/18/19：Debug 不泄参数与宿主态、服务袋端口取回宿主类型、没挂端口是 `None`、应用上下文是受检读取、预算是读视图且两次投影互不影响）、`tests/it-runtime/tests/agent_binding.rs::test_agent_binding_07`（执行实例跑的轮次，工具看到的仍是 public agent）、`tests/it-runtime/tests/runner_loop.rs`（`one_run_context_reaches_dynamic_availability_and_every_tool_call`：一次 run 里 `is_enabled` 与 `call` 跨两轮读到同一 run 身份；`a_run_without_host_state_reads_none_rather_than_another_hosts_object`；`work_state_handle_is_propagated_all_the_way_to_tools` 改走服务袋）、`tests/it-runtime/tests/turn_preparation.rs`（动态可用性经 `app_context::<HostContext>()` 读宿主态）。<br>**原始约束（保留）**：**执行顺序是 R3-9a → 最小 R6-6a identity slice → R8-0。** 对齐 openai `RunContextWrapper`：定义 `RunId` 及非序列化、宿主拥有的 `RunContext`，统一承载应用上下文、当前 public agent 视图与运行期事实的**读视图**；从它派生带 `CallId`、已解析工具身份和参数的唯一 `ToolContext`。它同时服务工具调用、R4-11 动态 instruction、R7 guard/hook 与 R12 handoff，绝不作为模型输入或持久化 `RunState` 的替代品。**必须演进现有 `ToolRuntimeContext`，不允许两个可 downcast 的 context trait 并存。** `ra-core` 不得依赖 `ra-coding` / `ra-exec`；R8 的 `CodingHost` 仅为一个具体实现。<br>**六条在动手前先定死**（2026-08-11 审查补，前三条其实是同一个问题的三个面：`ToolContext` 到底是什么形状）：<br>① **`ToolContext` 是 `ToolInvocation` 的直接改名与演进，不是新类型。** `CallId`、参数、caller 三样今天已经挂在 `ra-core::tool::invocation::ToolInvocation` 上；再造一个带同样三样东西的类型，和本条自己禁止的「两个 context 并存」是同一个错误，只是低一层。**不再二选一：公共类型直接改为 `ToolContext`，`Tool::call` 同次改签名；不采用 `ToolInvocation` 内持 `&ToolContext` 的 wrapper 方案。** 若发布节奏必须提供兼容期，只能给无运行时形状的 deprecated type alias，不能保留第二个对象或重复访问器。<br>② **与 R12-B 的 `ToolServices` 是同一次重构，一次做完**。两者都要重切 `RunRequest → TurnSettlementRequest → TurnExecutionRequest → ToolDispatchRequest → ToolContext` 这条四层链，而 R3-13 已经论证过这条链的改动代价包含**框架并不拥有其签名的第三方 `Tool::call`**。分两次改等于把最贵的动作做两遍：本条落地时 `ToolContext` 里就含 `ToolServices` 袋子，`work_state` 同一次迁完——今天袋子里只有它一个，是最便宜的时刻。<br>③ **应用上下文保持类型擦除，不引入泛型 `TContext`**。openai 的 `RunContextWrapper[TContext]` 会把泛型参数传染给 `Agent` / `Tool` / registry / `ra-tools`，异构工具从此进不了同一个 registry；R12-B 又已明确否决 `TypeId` 键的通用 service lookup。结论：应用上下文只暴露 `RunContext::app_context<T>()` 这一扇类型化读取门，框架端口一律具名访问器，`Tool` 与 registry 不加泛型参数。`WorkStateHandle::as_any()` 属任务态端口，不能充当或统计为应用上下文入口。**工具侧抵达那扇门的路径也只有一条**：`ToolContext::run() -> &RunContext`；今天的 `ToolInvocation::context() -> &dyn ToolRuntimeContext` 同次删除，不保留返回类型擦除对象的旧访问器——③ 定的是门，这句定的是工具怎么走到门口，缺了它「只有一扇门」在调用侧仍会有第二条路。<br>④ **usage / approval 的所有权在 `RunState`，`RunContext` 只提供读视图**。openai 的 `wrapper.usage` 是活累加器，照搬会让 resume 丢账——待审批/控制请求由 `RunState::pending_control_requests` 持有；逐请求 `ModelResponse::usage` 是不可变事实，`RunState::usage_totals` 只是由同一结算点原子更新、可由响应重建并校验的物化投影，预算只读该投影。不得存在第二个可独立递增的 usage counter。非序列化的 live context 不得成为任何需要跨段恢复的事实的唯一持有者。<br>⑤ **`RunId` 与事件序号都要能落盘，且跨段稳定性写死**。R8-0 的 `HostEvent.run_id`、R9 落盘、R14 replay 都按它归属，但本条的 `RunContext` 不序列化——所以 `RunId` 必须在 run 起点由显式构造参数注入 `RunState`（`RunState::start(run_id)`，不由 `new()` / `Default` 铸造，理由见 R6-6a）并持久化，resume 的第二段保留同一个 `RunId`；同一 run 的 `HostEvent.seq` 由 run 持有的 `Arc<AtomicU64>` 分配候选号、以「下一候选号」落进 `RunState`，恢复时取 `max(checkpoint_next, persisted_run_max_seq + 1)`，序号空间 per-`RunId`，不能在恢复后从零重置（R3-4b 的批并行工具是并发发射方，普通 `u64` 字段的 read-modify-write 不成立）。完整口径与「允许空洞、跨 run 顺序只信 `timeline_seq`」见 R6-6a 与 R8-0。R8-0 的 event 信封只能在最小 R6-6a identity slice 已落地后开工。<br>⑥ **工具身份不是字符串，agent 视图只给 public**。R2-1 已经把工具身份去词表化，`ToolContext` 只持有一个权威 `ToolOrigin`（其 `lookup_key()` 是路由/持久化键），不重复保存字符串名或第二份 key；R3-12（DONE）刻意分开 public / execution 且不提供 `execution_id()`，hook、guard、动态 instruction 都是宿主面，只能见 `public()`，execution 实例仅供框架内部。<br>验收：同一 context 在一次 run 的 tool / prompt / guard 路径保持身份一致；`RunState` serde 不触及 host context；`RunId` 与 event seq 跨恢复段连续且不重复；`usage_totals` 等于已持久化 `ModelResponse` usage 之和；待审批/控制请求恢复后仍可读、context 只能读不能写；core 依赖图不新增产品或执行层依赖。**外加机械门禁**：`ToolRuntimeContext` 不再出现在任何公开 API；应用上下文只能由 `RunContext::app_context<T>()` 读取；`as_any` 的检查只允许并限定在 `state::work`，不得用全仓字符串次数冒充 API 门禁。 |
| R3-9 | 生命周期 hook 点位 | **DONE** | **落地形态**：契约在 `ra-core::lifecycle`（`LifecycleEvent` / `LifecycleScope` / `LifecycleHook` ＋ `AgentStartInput` / `AgentEndInput` / `LlmStartInput` / `LlmEndInput` / `ToolStartInput` / `ToolEndInput` / `HandoffInput`），派发在 `ra-runtime::lifecycle`（`LifecycleHooks` ＋ `lifecycle::dispatch`）。安装口两条：`AgentSpecBuilder::lifecycle_hook(s)` / `clear_lifecycle_hooks` 与 `RunConfig::with_lifecycle_hook`；接线在 `runner`（agent_start / agent_end / llm_start / llm_end / handoff）与 `tool::dispatch`（tool_start / tool_end），四层链照旧透传。默认一个都不装。<br>**事件集照 `lifecycle.py` 取七个**：agent_start / agent_end / llm_start / llm_end / tool_start / tool_end / handoff。撤回版（备份分支 `r7-withdrawn-2026-09-09` 的 `68b8405`）的八个事件里，`run_start` / `run_end` 是把 agent 一对改名成 run 一对，`turn_start` 是发明——上游两样都没有，而每轮一次的 `turn_start` 与同样每轮一次的 `llm_start` 重合，留着就是两个名字一件事。<br>**一个 trait、两个安装点**（偏离，理由写进模块文档）：上游 `RunHooksBase` 与 `AgentHooksBase` 声明的就是同样这七个，差别只有 `on_agent_start`/`on_start`、`on_agent_end`/`on_end` 两对方法名。写两个 trait 等于一份契约两个名字、七处可漂；scope 无论如何都得是个值——同一个对象合法地既装在 run 上又装在 agent 上，分不清就把两条流当一条报。所以 `LifecycleScope` 作为回调参数传进去。<br>**这一族不做决定**：回调返回 `Result<()>`，错误照上游语义往外传（带 hook 名 / 事件 / scope 的上下文），与 `UserHook`「报告失败后继续」相反——决定型 hook 失败等于没有裁决、继续是有定义的；观察者失败等于叙述静默中断，只想记录的回调自己吞掉错误就行，那是它一行能做的局部决定。Stop 的续跑仍归 R7-4，被 stop hook 收回的那次交付不发 agent_end。<br>**身份是展示名、允许重名**：与运行级护栏、`UserHook` 同一口径，没有查找键，因此既无注册可拒、也无集合可校验——撤回版的 `validate_lifecycle_ids` 与 `GuardId` 依赖一并删除。<br>**每一对只夹住它自己命名的那件事**：tool 一对在准入闸之下、invoke 两侧，被链上任何一级拒掉的调用两个都不发；llm 一对一次逻辑调用一次，retry 与 provider fallback 在里面；`agent_start` 每次激活都发（段起点算一次，`is_resumed` 就是为分辨这个存在）；`agent_end` 与 output guardrail 共用 `FinishReason::is_complete` 这一个谓词，所以可恢复的预算软停、挂起的审批、失败与取消全都不发，两处不可能再漂；tool-stop 的交付没有 assistant message，由 `AgentEndInput::tool_outputs()` 带出来。<br>**handoff 告知接收方**：先把 agent 半边 `rebound` 到到达的 agent，再经那个集合发 handoff（对齐 `AgentHooksBase.on_handoff`），紧接着对到达方发 agent_start；`HandoffInput` 从尚未换绑的 run context 读 `from`，两侧都只给 `RunAgent` 投影。结算仍拒绝 handoff，所以这个点端到端不可达——`lifecycle::dispatch` 因此是 `#[doc(hidden)] pub`，照 `turn` 模块已有的那条理由：测试在独立 workspace，没有 `pub` 路径的模块根本无法被测，而这个 moment 没有别的可达调用点；其余六个都走 runner 覆盖。<br>**不外泄凭证与可调用对象**：`llm_start` 给的是解析后的模型、system instructions 与 input items，不是 `ModelRequest`（它通到 `extra_headers`，API key 在那里）；tool 一对给 `ToolOrigin` 而不是 `&dyn Tool`；handoff 给两个 `RunAgent` 而不是任一 `AgentSpec`。<br>**trace 照 R7-4 的做法**：自带 `lifecycle_hook` span 记 hook 名 / 事件 / scope，不进 `trace::field::ALL` 与 `SpanKind`——撤回版「`lifecycle.id` 必须与 `hook.id` 分开计数」的论据已失效，当前 `trace.rs` 里 `HOOK_ID` 与 `SpanKind::Hook` 根本不存在。<br>**不进 `RunState`**：观察者没有跨段需要知道的裁决，checkpoint schema 不变。<br>**验收落点**：`tests/it-core/tests/lifecycle_hooks.rs`（16 条：七个 moment 的词表与两个 scope 的 code、agent 上的声明与派生/清空、重名两个都装、默认实现每个 moment 都成功、scope 到得了回调、每种 input 的端口默认与注入、handoff 两侧都不给 spec）、`tests/it-runtime/tests/lifecycle_hooks.rs`（24 条：正常执行的完整 bracket、工具失败仍闭合并报 code、被拒 / 待审批的调用两个都不发、决定型 hook 排在 tool_start 之前、回调失败带出 hook＋moment＋scope、派发层与 run 层各一条取消路径、恢复段标记 continuation、失败 run 不报结束、软预算停不报结束、stop hook 收回的交付不报结束、tool-stop 的答案带 tool_outputs、两个 scope 各发一次且能分辨、`rebound` 的值语义、handoff 只告知 run 与接收方）。<br>**原始约束（保留）**：SDK 的 `RunHooks` / `AgentHooks` 生命周期契约；迁回前重新对照 `openai-agents-python/src/agents/lifecycle.py` 的事件、作用域、参数与异常语义，并去除对已撤销 guard 类型及登记表的依赖。生命周期观察与 R7-4 的决定型 hook 分开；Stop hook 的续跑能力不由生命周期观察者承担。验收覆盖正常执行、失败、取消、恢复与 handoff 的实际调用路径；旧测试数量不代表当前验收结果。 |
| R3-10 | **双通道输出（commentary / final）** | **DONE** | 落地形态：`ra-core::item::OutputPhase` 与 `Message::phase` 在 R1 就有；本条落的是**谁来定这个字段**——`ra-runtime::turn::resolve::step_items` 在 `resolve_next_step` 之后按结算结果给每条 assistant 消息盖章，`RunItem::with_output_phase` 只动 assistant 消息且保留整个包络（id / provenance / raw / session_data）。<br>**provider 说了不算**：模型完全可以一边要工具一边把消息标成 `final_answer`，也可能整轮不带 phase。它无从知道工具会不会失败、guard 会不会拦、宿主会不会被叫去审批——这些恰恰决定了这一轮是过程还是收尾。所以非终态轮（`RunAgain` / `Handoff` / `Interruption`）一律 Commentary。<br>**终态轮只有最后一条 assistant 消息是 Final**，同轮其余的仍是 Commentary。一条响应里「先报幕、再交付」正是本条对标的形态，整轮盖 Final 要付两次代价：UI 渲染出两次收尾，而这条记录下一轮还会作为输入回灌给模型（Responses 适配器会把 phase 下发成 `final_answer`），等于用 few-shot 教它把工具调用前的过程话写成交付。<br>**盖章位置在结算而不是 runner**：runner 事后改会让 `SingleStepResult` 留着 provider 那版、run 记录留着改过的那版，同一个 `ItemId` 两份 phase，而直接读 `SingleStepResult` 的 R9 落盘与 R12 归属拿到的是前者。R3-12 的归属放在 `step_items` 就是因为「那是唯一同时看见模型说了什么和回答产生了什么的地方」，通道归一是同一类事后归一化，同一个理由。<br>**规则只有一处推导**：`ra-core::step::phase`（`resolve_output_phases` / `delivery_index` / `phase_at`）。`ra-runtime` 的 `step_items` 调它，`SingleStepResult` 的闸门也读它——一个自己把被检查的规则再推导一遍的闸门，等两份说法分歧那天检查的是它自己那份。规则放 `ra-core` 而不是产出它的 runtime，与 R3-6b 把 `ToolUseTracker` 挪进 core 是同一条：闸门在 core，规则不能在它够不到的地方。<br>**闸门在 builder 上，不靠 runtime 恰好这么做了**：`check_resolved_output_phases` 要求 `session_step_items` 里每条 assistant 消息的通道都等于这一轮结算算出来的那个，**「没标 phase」也过不去**（两个通道是必选一个，UI 与 `final_message()` 都没有第三种状态的位置）。这样 R12 的嵌套 run、R17 的新路径、以及任何手搓 `SingleStepResult` 的地方都被同一条不变量管住，而不是只有今天这一条 `step_items` 路径。<br>**为此放宽的只有一格**：`check_response_reaches_the_session`（provider 原始回包 ↔ 会话存档）走 `model_payload_matches`，允许且只允许 assistant 消息的 phase 不同（`Message::matches_ignoring_phase` 用「对齐该字段再整体比较」实现，以后往 `Message` 加字段不会从这里漏过去）。**`check_carried_items` 与中断项那两处仍然逐字相等**——它们比的是两份都已结算的记录，通道不一致等于「给模型的那份」和「会话里的那份」对同一条消息讲两个故事。不放宽那一格，R3-10 就只能改在 runner 里，也就必然产生两份真相；那条不变量的文档原话是「如果分类哪天不再是非破坏性的，这一行就是说明它的地方」，这就是那一天。<br>**`final_message()` 改成按通道取**：最后一条被结算判为 `Final` 的 assistant 消息。**`None` 是一个真答案而不是「模型没说话」**——停在审批上的 run 还没交付，撞上轮次上限的也没有（上限在轮与轮之间生效，模型最后那句仍是干到一半的话），R3-8 的 error handler 才负责把这两种收成真正的结果。<br>**prompt 层那一半推迟到 R4**：本条的机制是「协议层两个通道 + prompt 层约束」，而 prompt 装配是 R4，今天没有可挂的地方。协议层已双向可用（Responses 适配器 R1 起就解析 `phase` 并下发 `final_answer`）。<br>**验收**（`tests/it-runtime/tests/runner_loop.rs` 6 条 + `tests/it-core` 5 条）：provider 标错方向的两种情形都被纠正且改过的通道进了下一轮输入；一轮里报幕与交付并存时只有最后一条算交付；撞上限的 run 整轮 Commentary 且没有交付消息；`final_message` 不把非 assistant 消息当交付；停在审批那一轮是 Commentary 且没有交付；流事件推的是结算后的通道。core 侧：改写通道保留 `RunItem` 包络并忽略非 assistant；存下来的记录只许按结算结果改模型回包的通道、正文一变照样拒收；已存通道与结算结果不符（两个方向 + 完全没标）当场拒收；终态轮只有最后一条 assistant 消息是 Final、且同一 ID 在 carried 与 session 里不能各有一份 phase；**生产者的产物必须原样通过闸门**——这条是「规则只有一处推导」的可执行形式，两边分头演化就在这里红 |
| R3-11 | loop 骨架快照测试 | **DONE** | 落地形态：`tests/it-runtime/tests/runner_loop.rs` 的 `loop_skeleton_keeps_its_per_turn_decisions_and_record_sequence` 用固定 mock 脚本跑三轮（narration + 工具 → narration + 工具 → 终局消息），**一张快照同时锁住每轮的 NextStep、该轮产出的记录序列（含 call/output 配对与 id）和每条消息的 phase**。<br>**快照必须读运行结果，不能靠推算**：初版把「第 n 轮之后还有第 n+1 轮 ⇒ 这轮判了 `run_again`」写在测试里自己算，那一列其实是常量，真正被断言的只有轮数和终态；而 R17 handoff 落地后它会把 handoff 原样打印成 `run_again`——回归地基恰恰该在那一刻报警。为此新增 `ra-runtime::runner::TurnRecord`（`turn` / `agent` / `next_step_code` / `finish_reason`）与 `RunResult::turn_records()` / `turn_items()`，由 loop 在 `match next_step` **之前**逐轮登记：之后登记会把这一轮记到 handoff 目标名下。<br>**对外只出 code，不出枚举**：`ra-core::step` 标的是 `Internal`，且 `NextStep` 是全框架唯一不带 `#[non_exhaustive]` 的公开枚举——宿主一旦 match 它，既冻结了产出它的结算管线，又把「加一个控制流状态」变成第三方的破坏性变更，正好与 R3-1 的穷尽性用意相反。新增 `NextStep::code()` 返回稳定字符串（同 `FinishReason::code`），载荷各归各位：终态给 `FinishReason`，中断项给 `RunOutcome::Interrupted`，handoff 目标就是下一条记录的 `agent`。<br>**不是第四份历史**：`TurnRecord` 只用 `Range<usize>` 借 `new_items` 的切片，不另存一份记录。**下标借来的切片必须能认出主人**：初版只用 `get` 兜住越界，而两个 run 的下标区间天然会重合——把 A 的记录递给 B 的结果，`get` 会成功，返回的是 B 在同一段下标上的记录，静默给出另一次运行的内容比 panic 更难查。记录与结果因此共享一个私有的 `Arc<TurnRecordOwner>` 身份，`turn_items()` 先 `Arc::ptr_eq` 认主再取切片，认不出就返回空。<br>**边界一并进了断言**：中断轮记 `interruption` 且 `finish_reason()` 为 `None`（停下来问人不是"结束"）；轮次上限在两轮之间触发，两条记录都仍是 `run_again`，终态只由 `RunResult::outcome()` 说了算——所以 `turn_records()` 的最后一条不一定是结束 run 的那一条 |
| R3-12 | Public agent / execution agent 绑定 | **DONE** | `ra-runtime::agent::AgentBinding` 同时持有 public 与 execution 两个 `Arc<AgentSpec>`，`prepare_turn` 与 `settle_turn` 都改成收它而不是收裸 spec / 裸 `AgentId`。<br>**要防的是「悄悄」，不是「不同」**：把一个 `Arc<AgentSpec>` 放在名叫 `agent` 的变量里传下去，R10 装配或 R11 sandbox 产出的 clone 会顺着同一条路走完全程——从此工具轨迹按它记、记录按它归属、hook 报它，而用户配置的是 `coder`，看到的却是一个他从没写过的东西。clone 什么都没声明，它只是变成了那个身份。<br>**做法是把「可能是任一个」的那个变量删掉**：只有两个名字不同的访问器，每个调用点必须表态要哪一个——「跑什么」是 `execution()`，「记在谁头上」是 `public_id()` / `public()`。**刻意不提供 `execution_id()`**：取一个用于归属的 id 只有一条短路径，而想拿执行实例自己的 id 得写 `execution().id()`，读起来就是「往跑起来的那个东西里看一眼」，不像一个可以拿去归档的身份。<br>**签名即约束**：`prepare_turn` 内部第一行就是 `let agent = request.agent.execution()`，六个阶段全读它——按 public 解析会把 sandbox 刚拿掉的工具重新递给模型，而错要到模型真的调它时才现形；`settle_turn` 只取 `public_id()`，调用方没有机会递错。`resolve_instructions` 是唯一的混合体：**源读 execution，报错点名 public**——配置错误必须指向用户写下的那个 agent，点名一个 clone 会让人去找一个根本不在配置里的东西。<br>**归属落在 `step_items`**，因为那是唯一同时看见「模型说了什么」和「回答产生了什么」的地方。**只填空、不覆盖**：已经声明了产出者的记录是从更清楚的地方来的（R12 的嵌套子 run 会标它自己的项），改写它等于把子 agent 的活记成父 agent 的。<br>**`is_prepared()` 比的是对象不是 ID**：一个 sandbox clone 完全可以沿用公共 agent 的 ID——它还是同一个 agent，只是装配方式不同；「两个 ID 不一样吗」会对这种情形答 false，而被换掉的恰恰是要跑的那套工具。<br>**`prepared()` 刻意不校验派生关系**：`AgentSpec` 里没有任何字段记录它，靠命名约定去推等于让身份依赖字符串形状——正是 `ToolOrigin` 把 `qualified_name` 挡在派发之外要避免的那件事。类型保证的是另一件：一个准备过的实例**无法**在不点名它所代表的 public agent 的情况下进入框架。<br>**验收**（`tests/it-runtime/tests/agent_binding.rs` 6 条）：无准备步骤时两个身份是同一个对象；准备与否看对象而不是 ID（同 ID 但换了工具面依然算准备过）；准备阶段广播的是 execution 的工具面、模型选择器也是 execution 选的；execution 带另一个 ID 时工具轨迹与每一条记录仍然记在 public 名下（含 `agent_name`）；已有归属的记录不被改写成父 agent；直接绑定时归属就是用户那个 agent 且空轮的 agent 依然在册 |
| R3-13 | **运行态与任务态分离（`WorkState` 挂载点）** | **DONE** | 落地形态：`ra-core::state::work::WorkStateHandle`（trait），以 `Option<Arc<dyn WorkStateHandle>>` 挂在 `RunRequest::with_work_state`，经 `TurnSettlementRequest` → `TurnExecutionRequest` → `ToolDispatchRequest` → `ToolInvocation::work_state()` **四层透传到每一个工具**。<br>**这四层就是本条存在的全部理由**：现在留位是每层加一个字段；R17 再补要改运行上下文的全部构造点**和所有 `Tool::call` 的调用方**——包括框架并不拥有其签名的第三方工具。一个不破坏别人代码就加不进来的里程碑，必须在需要它之前把缝留好。<br>**刻意不给 channel 操作**：trait 上今天只有 `as_any`。没有 reducer 的 `get` / `set` 对不是 R17-1 channel 的正确生长形状——channel 的要害正是并发写按声明好的规则合并，而不是后写覆盖先写。宿主今天能做的是 downcast 回自己的类型，等 R17-1 在其上加类型化操作时这条访问路径一行都不用改。<br>**两个名字必须分开，这是本条的第一句话**：`RunState`（`ra-core::state::run`，R6-6 的骨架）是**单次 run 的可恢复运行态**，R3-6b 的 `ToolUseTracker` 挂在它上面，带 `schema_version` 与 `Unknown` 回写，R6-6 往它加字段即可；`WorkState` 是**跨 run / 跨节点的任务态**，只以 handle 出现。把任务态复制进 `RunState` 会让每次 checkpoint 带一份私有快照，另一个节点一写就过期——那正是「两者不能混」要防的事。<br>**续接入口只留一个**：`RunRequest::with_state(RunState)`，`with_tool_use` 删除。两个入口顺序敏感（先 `with_tool_use` 再 `with_state` 会静默丢掉 tracker），而「按字段续接」这个形状本身有毛病——等 R3-8 与 R6-6 往 `RunState` 加东西，只认识旧字段的调用方会悄悄把新的抹掉。只要一个字段时用 `RunState::with_tool_use` 建好再整份传进去。<br>**guard 侧留白**：R7-0 只落了预算与登记表，guard 本身仍不存在，句柄到 guard 那一段等 R7-1/R7-2 的护栏层，插入点与工具是同一条链。<br>**验收**（`tests/it-core/tests/run_state.rs` 4 条 + `tests/it-core/tests/tool_contract.rs` 1 条 + `tests/it-runtime/tests/runner_loop.rs` 3 条）：默认状态是当前版本且没有轨迹；轨迹随整份状态序列化往返后接着数；换掉轨迹不动同一份状态里的其它字段（用更高版本写下的未知字段验）；更高版本的未知字段原样回写；`ToolInvocation` 上挂了句柄能 downcast 回宿主类型、没挂时是 `None` 而不是空壳。runtime 侧：整份运行态随结果交回、第二段接着数；句柄从 run 一路到达工具；没挂任务态时工具读到 `None`。<br>**补记（2026-08-11，R12-B 定案时）**：`WorkStateHandle` 是这条四层链上的**第一个**端口，`AgentControlPort` 是第二个，`WorkspaceLeaseManager` 与 budget reservation 是看得见的第三、第四个。本条「留位便宜、后补昂贵」的论证对第二个之后同样成立，但结论要再进一步：端口收进一个 `ToolServices` 袋子，四层只传袋子，之后加端口只改一个结构体而不是重走一遍构造链与第三方 `Tool::call`。**迁移趁早——今天袋子里只有 `work_state` 一个**，形状见 [R12-B](#r12-b-codex-multi-agent-集成分层冻结建议)「必须采用」第 1 条。**执行点是 R3-9a 第 ② 条，不单独排一次**：`ToolServices` 与 `ToolContext` 切的是同一条四层链，分两次改就是把「连第三方 `Tool::call` 签名一起动」这件最贵的事做两遍 |

### R3 实现状态修订（2026-08-11，2026-08-28 订正）

`R3-4b` 已完成流式派发：每次模型调用都读取流；适配器发出一个完成的 `RunItem::ToolCall` 时，runtime 立刻按该轮 `ToolConcurrency`、resource admission 与 semaphore 上限启动该调用。早启动的任务与结算批次共用同一个 semaphore 与 admission gate，重叠不会抬高峰值并发。终态 `ModelResponse` 仍是唯一的历史、usage、attempt 与 settlement authority：它会校验每个已启动调用的 tool identity 与参数，然后按模型顺序收集观察。**终态响应与自己的流不一致就让这一轮失败**——被改写、被漏掉、或同一 `call_id` 在流里出现两次，runtime 都无法判断 provider 究竟跑了哪一个；这个失败直接返回而不进 R3-4c 的失败仲裁表，否则同一调度轮里完成的普通工具错误会盖过它，报出后果而不是起因。<br>**重放边界的判据是 runtime 自己的事实，不是适配器的意见。** `CallConsumption` 记录这次调用放出去了什么——原始帧到达订阅者、或工具已按某个流式 item 启动——并在询问 `get_retry_advice` **之前**否决重试。适配器对中途断流报 `Safe` 并没有撒谎：它知道请求没被接受，但不知道某个工具已经写过文件。反过来，什么都没放出去的失败**保持适配器原本的分类**（通常是 `ReplaySafety::Unknown`），由重试闸门再问一次 continuation 是不是 server-managed——在这里替 provider 断言 `Safe` 正是 `Unknown` 这个状态要防的事。取消、流错误、分类失败、并发上限非法等每一条放弃流的路径，都先取消 tool scope 再 join 任务，而不是丢掉 `JoinSet`（它 drop 时的 abort 没有人等待）。<br>**声明了 `max_repeat_streak` 的工具不走早期派发。** R3-6 的 `admit_repeat` 是按「streak 已含本次调用及同响应的兄弟调用」定义的——这才使它能一次拒掉 N 个相同调用而不是按调度顺序拒——而流式派发时响应尚不存在，也无从知道后面还有几个。这类工具因此让出重叠、保住保证，由 settlement 按原路径回答；阈值本就是逐工具 opt-in，代价只落在明确要了熔断的地方。`no_progress_streak` 不需要这个处理：outcomes 在上一轮 2b 就已归档，两条路径读到同一个数。<br>`partial_messages` 现在只决定是否把原始 provider narration 转发给宿主，不再决定 loop 是否流式执行；转发出去的叙述收不回来，所以它同时关掉这次调用的重试窗口，而一个什么都不渲染的 run 不该付这笔钱。早启动的工具读到的是调用前的 `RunContext`（流还开着时响应用量不存在），settlement 仍构造调用后的那一份，叙述过的一轮因此两份并存——合并成一份只能是对其中一组工具说谎。并行资格由 `ToolConcurrency` 声明决定，`RwLock` 保持读读并行与写者独占，外层 semaphore 提供每轮总并发上限并接入 `RunConfig`。R3-4c 的失败选择、迟到失败合并和取消 drain 继续复用同一批次监督路径。

### R3 非目标

| 项目 | 处理 |
| --- | --- |
| 在 loop 里写业务判断 | 不做；一切判断以"返回哪个 NextStep"表达，判据来自结构化状态而非文本匹配 |
| auto-continuation 的启发式续跑 | R3 只做 `max_turns` 软结束；R15 不新增基于验证状态的自动续跑。若未来发现真实空转问题，必须先做 A/B eval 再单独立项 |
| handoff 作为主编排形态 | 不做；R3 保留 `NextStep::Handoff` 分支但优先实现 R12 的 `as_tool` 子 agent |

### R3 验收标准

| 能力 | 标准 |
| --- | --- |
| 状态穷尽 | `match next_step` 无 `_ =>` 兜底分支；新增状态编译期报错 |
| 中断是一等态 | 有待审批工具时 loop 正常返回 `Interruption`，不 block、不 panic |
| 可回放 | mock provider 脚本 + NextStep 序列快照稳定 |

---

## R4 Prompt 装配与缓存治理

> **本阶段的目标是把缓存命中率做到与两个参考同档。** **注意：下面这组数字曾挂在 AgentForge 的分析文档名下，2026-09-09 起不作为论据**——要用必须先回到 `codex-rs` 源码或第一手 provider 日志重新核实。机制本身（稳定前缀、缓存断点、动态段位置约束、Anthropic 侧 `cache_control` 分块）与数字无关，照做。

### R4 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R4-0 | **稳定前缀正文（Codex 骨架落成八段）** | TODO | 本条是 R4-8/8a/8b/8c/9 反复提到的「正文仍待写」的**唯一收口**——那几条此前把它指向 `R4-0..R4-5`，而 R4 表里从来没有 R4-0，R4-1..R4-5 又各有其义，读者按图索骥只会找到别的任务。<br>**缺的只是文本**：装配链路（`ra-coding::prompt::assemble_stable_prefix` → `AgentSpec::instructions`）与 `cargo xtask prompt-dump` 门禁已在 R4-8 那轮接通；当前 `identity` / `engineering` / `editing` / `autonomy` / `formatting` / `channels` / `personality` / `role` 已有正文，`tool_use` 与 `frontend` 仍是注册位空桩。<br>**为什么拆成八条而不是一条**：段序是缓存产物的一部分（`assemble_stable_prefix` 的文档已写死这点），每加一段都要重跑 `api/prompt-dump.txt` 快照并复查缓存链路；八段一次写完会压成一个不可 review 的 diff，而它改的正是每轮都要重付的那段字节。<br>**共同验收**：每段落地后 `api/prompt-dump.txt` 有可 diff 的增量；前缀已跨过 `MIN_CACHEABLE_PREFIX_TOKENS = 1024`，因此 `it-coding` 锁定 cache plan 指向真实 prefix，adapter 仍须按完整 wire 请求决定是否发送具体缓存字段。<br>**正文一律原创**，只对齐结构，见 R4 非目标第三条 |
| R4-0a | 身份与协作契约段（`identity`） | DONE | `ra-coding::prompt::identity` 将 Rusty、共享工作区、用户授权边界和「真正解决才算完成」写成稳定前缀的首段；prompt-dump 快照与集成测试锁定其首段位置及协作/完成语义。 |
| R4-0b | 工程判断段（`engineering`） | DONE | `ra-coding::prompt::engineering` 以稳定 `core_behavior` 段写入工程判断：优先复用仓库既有 public API / helper / 机制，选择解决当前问题的最小改动，不在缺少具体需求时增设并行抽象或通用机制，权衡显著时讲明白、验证与风险相称；prompt-dump 快照与集成测试锁定其在 identity 之后、tool_surface 之前的位置。**代码风格匹配的措辞不放这里**（归 personality）：同一条指令占两个缓存段，改了一边模型就同时拿到两个版本。**工具选用的具体措辞也不放这里**（归 R4-0g）：那部分随 advertise 集变化，混进本段会让工具面一动就打掉这一段前缀。五个角色共用本段——角色差异只归 role 段，稳定前缀骨架保持一致。<br>顺带补掉一个之前不安全的前提：`engineering` 占用的是通用槽名 `core_behavior`，而 `PromptAssembler::add_section` 撞名是静默替换，后续 `autonomy` 等模块再来抢同一个槽会让先注册的那段被造出来、校验过、却从不发送，只在下一次 dump 里少一行。现在 `with_sections` 拒绝任何已注册的段名（`add_section` 单调用的显式覆盖保留，只读角色换 role 段仍靠它） |
| R4-0c | 编辑约束与 Git 安全段（`editing`） | DONE | `ra-coding::prompt::editing` 将 `apply_patch` 定为直接编辑工作区文件的专用工具，而不把它误述为唯一可写机制；这与 Codex 的模型工具面一致，后者同时保留 shell / exec 作为通用执行入口。**照 Codex 原文补齐三句里的后两句**：禁止用 `cat` / heredoc 等 shell 写文件花招绕开编辑入口（没有这句，拿着执行入口的 agent 写一个 heredoc 就什么都没违反），同时豁免格式化命令与批量机械改写（没有这句，规则读起来仍是绝对的，日常改一百个文件就成了违规，会把整段的可信度一起赔掉）。<br>**这一段分两半装配，点名工具的那半要过两道门**：worktree 保全与破坏性 Git 禁令与角色、与工具面都无关，凡是够得着 shell 的都适用，五个角色一字不差共享（417 字节）；点名 `apply_patch` 的两条要求①该工具**确实在本次 advertised 工具面里**（`ToolSurfacePromptBuilder::contains_advertised_tool`，与稳定清单共用同一个 `is_advertised_to_model` 判据），②角色不是自称没有编辑工具的那几个（`!is_read_only() && !is_one_off()`）。第①道是 R10-4 的正题——提示不能点名 provider 请求里根本没有的入口，所以不带工具装配出来的前缀（`build_agent`）也不再提 `apply_patch`；第②道是因为本段 rank 5 排在 role 段 rank 9 之前，只读角色的 role 文本写着「你没有编辑工具，编辑会失败」，无条件的「用 `apply_patch` 编辑」会让它**先**读到指令、后读到否认。目前只有 host-backed main 拿到完整 702 字节的变体。破坏性 Git 清单与本产品 `dangerous_action::is_destructive_git` 实际检测的那组对齐（补上 `git clean`），提示与运行时拦截指向同一批命令。装配与集成测试锁定该段位于真实 advertised `tool_surface` 之后；`test_read_only_roles_keep_git_safety_without_the_editing_entry` 断言只读前缀整体不出现 `apply_patch`、安全那半跨角色逐字节相同；prompt-dump 快照记录两个变体的字节级增量。<br>**工具侧那一半已于 R4-5 补上**：`build_agent_with_host` 现在按角色装工具（`host_backed_tools`，只读与 one-off 拿空表），宿主背书的只读 agent 不再在 `tool_surface` 段里读到 `apply_patch`——提示说「你没有编辑工具」时，请求里也确实没有。`test_host_backed_read_only_and_one_off_agents_carry_no_tools` 同时断言工具表为空与前缀不出现该名字。 |
| R4-0d | 自主推进与止损段（`autonomy`） | DONE | `ra-coding::prompt::autonomy` 以稳定 `autonomy` 段回答「什么时候该停」：改动在权限内时提案不算交付，要在本轮内 inspect / change / verify / report 做完，同时显式尊重只要求计划、回答、暂停或改向的请求；失败的工具调用是下一次尝试的证据，不是可以原样重试的东西；运行时可在工具的无进展阈值后拒绝调用，默认连续三次无新证据失败。<br>**文本与 identity 划界**：角色权限、请求交付物、以及说明角色跨不过去的限制，这三件事 identity 段（R4-0a）已经写了，本段不复述——同一条指令进两个缓存段，改其中一个就会让模型同时持有两个版本，与 R4-0b 对 personality 的处理同理。这条划界连同合并重复 bullet，把段体积从 287 token 压到 **188 token**（751 字节，−35%）；host-backed 前缀 815 token，距 1024 缓存地板还剩 209，剩下四段要挤在这个余量里。<br>**拒绝的措辞是刻意的**：R3-6c 的断路器触发时会清零 streak，工具不会在本 run 里丢失，所以文本写 `refuse a tool call` 并补一句 `the tool is not lost and the task is not done`——写成「运行时可能拒绝某个工具」恰好会诱发断路器要防的那个推论（放弃该工具）。阈值只作为默认值陈述（工具可覆盖或关闭，边界仍由 R3-6c 实现负责），`prompt_dump` 用 `DEFAULT_MAX_NO_PROGRESS_STREAK` 断言把提示词里那个英文数词钉住，常量一改就红在文案旁边。段位于 `editing_verification` 之后，五个角色共享同一字节序列，角色段继续决定具体可做动作；完整段序断言按本文件既有约定移到本段的测试里（最近加入的段做主语）。 |
| R4-0e | 输出格式规则段（`formatting`） | DONE | `ra-coding::prompt::formatting` 以稳定 `final_answer` 段写入 UI 的 GitHub-flavored Markdown 契约：仅在确有助益时使用 1-3 词的 `**…**` 标题；列表保持扁平、有序项一律 `1.`；默认禁 emoji 与 em dash；真实本地文件引用采用 `[label](/abs/path:12)`。规则明确禁止 Markdown heading 语法、嵌套 bullet、`1)`、file URI、代码跨度包裹链接或路径、以及行范围，避免渲染器无法点击或版面失控。该段位于 `autonomy` 后、`personality` 前，五个角色逐字节共享；集成测试与 `api/prompt-dump.txt` 同时锁定文字和位置。 |
| R4-0f | 双通道文本段（`channels`） | DONE | `ra-coding::prompt::channels` 以稳定 `channels` 段写明双通道的文字契约：`commentary` 是工具工作期间短、可扫读的进度与部分发现，第一次调用工具前先报幕、之后每一批工具工作前再报一次；`final` 是完成后自包含的交付，结论、支撑证据、以及交回给用户的阻塞或决定都落在那里，宁可重述早先的更新也不要让用户回溯。该段不干预 R3-10 的 phase 归属机制。<br>**本段只管归位，不重述义务**：「必须报告阻塞」归 `autonomy`、「如实报告」归 `personality`，同一条指令占两个缓存段，改了一边模型就同时拿到两个版本。**节奏锚在工具批次而不是秒数**：模型没有时钟，被要求约束的那段时间又花在它写完消息之后的工具执行上，运行时也不测量它——这与 `autonomy` 里那个有测试钉死在 `DEFAULT_MAX_NO_PROGRESS_STREAK` 上的真实默认值不是一回事。两个通道标识符取 `OutputPhase` 自己的 label 并由测试钉住；**已知偏差**：OpenAI Responses codec 下行写的是 `final_answer`（回收时两种拼写都认），回放历史因此可能让模型看到本段不用的那个词，对齐它属于 codec 侧改动，把某一家的 wire 词表写进产品文案会让它进入每个角色的缓存前缀。<br>它位于 `autonomy` 后、`final_answer` 前，五个角色逐字节共享；完整段序断言随「最新加入的段」移到本条的测试里，且改用会在缺段时 panic 的 `position_of`（`Option<usize>` 比较里 `None < Some`，缺段反而能让顺序断言通过）。`ra-prompt` 的 canonical order 从 12 条 if 分支改为 `CANONICAL_SECTION_ORDER` 静态表，新增段是插入一行而不是重编号，`it-prompt` 的顺序测试同步补上 `channels`。<br>加入后 host-backed 前缀越过 1024 token 缓存门槛（1180）。**门槛是线上行为开关，所以断言下沉到 wire**：`CachePlan::for_prefix` 从不看 token 数，对它做的任何断言在百来 token 时同样成立；改为把真实 prefix 过一遍生产 Anthropic codec，断言 `system[0].cache_control` 确为 `ephemeral`，并配一条门槛以下的对照断言它不发断点。`preview_request_payload` 不适用——那个兼容预览根本不打断点。 |
| R4-0g | 工具使用契约段（`tool_use`） | DONE | `ra-coding::prompt::tool_use` 新增稳定行为段：只能从当次 advertised surface 选择入口，按能力与 schema 选择，优先能返回所需结构化观察的专用入口；当 surface 提供命令执行入口时，它承接长尾命令行工作，不再为每个命令或 utility 寻找细分工具。工具名称和 schema 继续只归 R4-6 的 `tool_surface`，本段不点名当前未广播的入口，因此不会让无 `exec_command` 的请求误以为它可用。段位于 `core_behavior` 后、`tool_surface` 前；`it-coding` 锁定文字、能力边界、段序、prompt dump 与 insta 快照。为保持 R4-7 的 2048 token 总 ceiling，行为段与由 coding profile 限定数量的 tool surface 各占 128 token。 |
| R4-0h | 前端设计段（`frontend`，懒加载） | TODO | 附录 B 裁决 7：取 Codex 但**不进常驻前缀**——它与多数任务无关，常驻等于每轮为它付 token。**R10-5 已落地，前置解除**：它不带工具，因此走 `RunConfig::with_capability` 装一个 `frontend` 族的 capability、由 `deferred_instructions` 出正文即可，投递一次进历史、不进常驻前缀；触发信号要覆盖 `wants_deferred_instructions`（没有工具就没有默认信号），且只能读结构化事实，不能拿用户文本做词表匹配。剩下的是正文本身 |
| R4-1 | `PromptSection` 分层模型 | DONE | 每段带 `{name, purpose, source, stability: Stable\|Volatile, position: Prefix\|TailMessage, content_hash, token_estimate}`。**类型层面禁止 `Volatile` 段落进 `Prefix` 位置** |
| R4-2 | 稳定前缀装配 | DONE | 基础段（core_behavior / tool_use / safety / editing_verification / final_answer / context_durability）跨轮 byte-stable；`stable_prefix_hash` 每轮断言不变，变了必须有显式 invalidation 理由 |
| R4-3 | 尾部**增量（delta）**提醒通道 | DONE | `RuntimeReminder` 只能产出 messages 尾部的 system/user 消息，**API 上不提供"改 system prompt"的能力**。**实现形态照 CC 的 `attachment`：只注入变化的增量，不重发整表**——`agent_listing_delta` 只发 `addedTypes`/`addedLines`，`date_change` 只发 `{newDate}`，压缩后大文件降级为 `compact_file_reference` 路径引用。见[附录 A.4](#a4-claude-code-执行事件流)。Anthropic 侧对齐 mid-conversation system 语义 |
| R4-4 | 缓存断点治理 | 部分 | Anthropic：易变头隔离在 cache_control 之外（CC block[0] 的做法）；OpenAI：稳定前缀走 `instructions`，动态段走 input 尾部。每 provider 一个 `CachePlan` 结构。<br>**计划构造只有一处**：`CachePlan::for_protocol(protocol, prefix, cache_scope)` 在 `ra-core`，内容全部由 `ApiProtocol::capabilities().prompt_cache()` 推出——`requires_explicit_breakpoints()` 才发断点、`explicit_cache_key()` 才带 key，新协议声明能力即得到计划，不用再加一条分支。`ProviderCacheStrategy::for_protocol` 的 match **故意穷举**：`ApiProtocol` 对下游 non-exhaustive、对 `ra-core` 不是，所以加协议会在这里编译失败、逼人给出缓存形状，而不是静默不缓存到账单上才发现。<br>**长度下限**：`MIN_CACHEABLE_PREFIX_TOKENS = 1024`。低于它两家都不缓存，而 Anthropic 上还要白占四个 `cache_control` 断点之一。精确下限是 per-model 的（Haiku 要 2048），细化归模型能力矩阵。<br>**cache_scope 目前取 `RunId`**，等 R9-2a 的 `SessionId` 落地后换成会话级——key 每轮变化比不带 key 更糟，那是在切分缓存而不是共享。<br>**降级而非报错**：`insert_cache_plan` 遇到别的协议的计划、或 `extra_body` 已显式设了 key，都是记一条 debug 日志后跳过/让位。缓存计划是省钱的优化，让它打挂整个请求等于拿可用性换折扣；model fallback 会把备好的请求换协议重发，正是这条的现实场景。<br>**校验点在 runner 的唯一分发处**（`ModelRequest::validate_cache_plan`），不靠每个 adapter 自觉——计划用 hash 指认前缀，忘了校验的 adapter 会发出一份指着别的文本的计划，只表现为缓存永不命中。<br>**尚未完成**：Anthropic 与 Chat Completions 的 adapter 还是空桩（`crates/ra-model/src/anthropic/request.rs`、`openai/chat/request.rs` 各一行），所以这两家的计划目前只是被带着走，`cache_control` 一个都还没真正发出去。OpenAI Responses 这条是通的（`instructions` + `prompt_cache_key`）|
| R4-5 | `prompt dump` 与 doctor | DONE（`prompt dump` 部分；doctor 归 R16） | `ra prompt dump [--role R] [--workspace P 或 --no-tools] [--provider X] [--model Y] [--cache-scope S] [--json] [--baseline F]` 打印分段、provenance、hash、token 与所声明的额度、缓存计划与门槛判定，`--baseline` 则对上一次 `--json` 记录做比对并点名触发源。**`Source` 与 `Budget` 两列是 R4-7 加的**：段的 provenance 与额度都是从正文里看不出来的事实，而快照要锁的正是它们；额度在线格式上可缺省为 `None`，否则 `--baseline` 读不了加这两列之前记的 dump。<br>**组装放在 `ra-coding::prompt::dump`，不放二进制**：dump 覆盖哪条前缀、host-backed agent 装哪些工具、没有 run 时 cache scope 写什么，都是产品决定；`ra-prompt` 只管渲染、一个都不知道，而分层门禁本来也不允许 `ra-cli` 越过 ra-coding 去够 ra-core/ra-prompt。更实际的理由是快照门禁组装的是同一份报告——放进二进制就会有两份，第一次改动就会分叉。现在 `test_stable_prefix_matches_the_committed_snapshot` 与 CLI 走同一个 `render_prompt_dump`，**`ra prompt dump --no-tools --role main` 的输出与 `api/prompt-dump.txt` 的 `### role: main` 段逐字节相同**，「核验入口」这句话第一次是可执行的。没有 run 产生报告时 cache scope 写 `PLACEHOLDER_CACHE_SCOPE`（`<run>`），否则每次调用都不一样、快照永远对不上。<br>**未知角色一律拒绝**，不落到 `PromptRole::Custom`：custom 没有自己的角色正文，接受一个拼写错误等于用一份长得完全像真的、角色段却是占位文本的报告去回答它——核验入口唯独不能干这件事。<br>**`--baseline` 回答的是「谁把 hash 打掉了」**：逐段配对成 added / removed / rewritten / moved，prefix hash 变了就退出码 1，可以直接拿去卡 CI。`moved` 按**两份 dump 共有段之间的相对次序**判定而不是绝对下标——插入一段会把它下面所有段的下标推一格，若照实报告，真正的成因会被七条「后果」淹掉；它单独成一类是因为两段对调时每段 content hash 都不变，拼接后的前缀却整个改写，没有别的信号看得见这件事。<br>`--provider` / `--model` 只是报告上的标签：今天没有任何段按 provider 组装，测试断言加标签不移动 prefix hash。<br>`ra-cli` 拆出 `[lib]`，`main.rs` 只剩解析与打印，`execute` 返回 `CommandOutput`（stdout + `CommandOutcome`）而不是直接写 stdout / 返回 `ExitCode`——`ExitCode` 既不可比较也变不回数字，只返回它的命令没法对脚本真正读的那个值做断言。新增 `tests/it-cli` 覆盖 clap 自检、角色取值、`--no-tools`/`--workspace` 与 `--json`/`--baseline` 互斥、退出码两个方向、以及坏 baseline 的两种拒绝。<br>顺带收掉 R4-0c 记的那条遗留：`build_agent_with_host` 改为按角色装工具（`host_backed_tools`，只读与 one-off 拿空表），dump 与 agent 因此报告同一份工具面。<br>**doctor 不在本条**：`config / sandbox / mcp / provider` 自检分别归 R0-5 验收行、R8-9 与 R16，这里只补掉本条真正缺的那个子命令 |
| R4-6 | 工具 schema 稳定性 | DONE | `ra-coding::prompt::assemble_stable_prefix_for_tools` 从与 `AgentSpec` 同一路径安装的真实工具构造 `tool_surface` 稳定段；仅列可广播工具，按模型可见名确定排序，**一份工具清单产出两件东西，且刻意分开**：prompt 段只列名字——它是模型可见的、进缓存前缀的，64 位十六进制摘要放这里既让模型拿不到任何可用信息，又会在每次 schema 微调时白白作废 instructions 段（真正变的是 provider 的工具表，那是请求的另一部分，自己会失效自己的缓存）；摘要（`TOOL_SCHEMA_REVISION` + 覆盖全部 model-facing 字段的 SHA-256 指纹）只落在 `api/tool-surface.txt`，只改描述或只改 schema 这类「名字列表看不出来」的编辑照样瞒不过去。实测：指纹移出后该段 56 token 降到 30。「广播」这条判据不在提示词侧另写一遍，而是读 `ToolOptions::is_advertised_to_model`——工具表预算、重名校验、提示词清单问的是同一个问题，各写一份迟早会对 `Hidden` / `Disabled` 各说各话。`TOOL_SCHEMA_REVISION` 是产品拥有的显式 invalidation 版本：工具面快照变更时，`BLESS_PROMPT_DUMP=1` 若未提高版本会拒绝覆盖，**且该检查跑在两份快照的任何一次落盘之前**——工具增删会同时动两份，只守其中一份会留下半 bless 的工作区（名字清单改了、指纹没改），下一次运行报在没被改写的那份上，而不是报在真正变了的东西上；只改描述或只改 schema 则只动 tool-surface，prompt 段永远不含指纹。两处落盘另外共用一把锁：它们由并行运行的不同测试写出，不同步就会截断对方正在读的快照，把一次合法 bless 变成「找不到 revision 标记」的 panic。`api/prompt-dump.txt` 同时收录 host-backed main 的真实稳定前缀；`cargo xtask prompt-dump` 对账这两份可 review 的产物。R2-9 继续锁 provider 的完整 wire payload 与连续 100 次渲染。 |
| R4-7 | 提示片段回归锁 | DONE | `it-coding/tests/prompt_regression.rs` 以 insta 快照锁住五个 shipped role 的 tool-free 前缀，以及实际 `build_agent_with_host` 生成的 main / coordinator host-backed 前缀；因此段顺序、段数、条件化编辑文字和工具面都必须经过快照 review。每段在定义处声明 token allowance，装配器拒绝超额段，回归测试同时要求每个 shipped 段都声明额度、各额度合计为 2048、各实际前缀落在 1024 缓存地板与该上限之间。默认 prefix 的 provenance 必须是 `Agent`，并扫描名称、purpose 和正文中第三方来源/归属标记；植入 marker 的对照测试避免门禁退化为永远匹配不到。 |
| R4-8 | **角色化 prompt 变体** | 部分 | **（2026-08-17 复审：接线已补，正文仍待写）**本条的文字此前到不了模型：全仓只有 `ra-runtime::turn::prepare` 的一处写 `system_instructions`，而它读的是 `AgentInstructions::as_static()` 的裸字符串，不经过 `PromptAssembler`。`ra-prompt` 的装配器、`PrefixStabilityTracker`、reminder、role、personality、dump、metrics 在本 crate 之外零引用。按 layering 门禁 `ra-runtime` 只能依赖 `ra-core`，装配必须由装配层注入，而该注入点（`ra-coding/src/prompt/*`）当时十个模块全是空 doc。接线已补：`ra-coding::prompt::assemble_stable_prefix` 走 `PromptAssembler` 装出稳定前缀，`ra-coding::build_agent` 把它写进 `AgentSpec::instructions`，于是 `system_instructions` 从此是装配产物而非裸字符串；`api/prompt-dump.txt` 落了五个角色的可 diff 快照，`cargo xtask prompt-dump` 从 SKIP 变成真门禁（实测改 personality 一个词即 FAIL，改回即 PASS）。**仍未完成的是正文本身**：`ra-coding/src/prompt/` 下**八个**按主题划分的注册位仍是空的，Codex 骨架的正文已单列为 **R4-0a..R4-0h**（原文写作 `R4-0..R4-5`，那是个指不到任何任务的编号）；今天前缀里只有 personality 与 role 两段。<br>两家都不是"一套 prompt 打天下"。**主 prompt 取 Codex 骨架，角色 prompt 全部取 CC**（Codex 无对应物），冲突逐条裁决见[附录 B](#附录-b-codex-与-claude-code-系统提示词的冲突裁决)。`PromptRole` 五档：`Main` / `ReadOnlySpecialist` / `Planner` / `OneOffAnswer` / `Coordinator`。共享同一份稳定前缀骨架，只换角色段 |
| R4-8a | **READ-ONLY 角色的"能力不可用"表达** | 部分 | **（2026-08-17 复审：接线已补，正文仍待写）**本条的文字此前到不了模型：全仓只有 `ra-runtime::turn::prepare` 的一处写 `system_instructions`，而它读的是 `AgentInstructions::as_static()` 的裸字符串，不经过 `PromptAssembler`。`ra-prompt` 的装配器、`PrefixStabilityTracker`、reminder、role、personality、dump、metrics 在本 crate 之外零引用。按 layering 门禁 `ra-runtime` 只能依赖 `ra-core`，装配必须由装配层注入，而该注入点（`ra-coding/src/prompt/*`）当时十个模块全是空 doc。接线已补：`ra-coding::prompt::assemble_stable_prefix` 走 `PromptAssembler` 装出稳定前缀，`ra-coding::build_agent` 把它写进 `AgentSpec::instructions`，于是 `system_instructions` 从此是装配产物而非裸字符串；`api/prompt-dump.txt` 落了五个角色的可 diff 快照，`cargo xtask prompt-dump` 从 SKIP 变成真门禁（实测改 personality 一个词即 FAIL，改回即 PASS）。**仍未完成的是正文本身**：`ra-coding/src/prompt/` 下**八个**按主题划分的注册位仍是空的，Codex 骨架的正文已单列为 **R4-0a..R4-0h**（原文写作 `R4-0..R4-5`，那是个指不到任何任务的编号）；今天前缀里只有 personality 与 role 两段。<br>CC 的写法值得照抄，它不是"请你别写"而是**"你没有这个工具，试了会失败"**：`You do NOT have access to file editing tools - attempting to edit files will fail.` 配合工具面真的把写工具摘掉。**这是 prompt 与工具面一致性的正确方向**——R10-4 讲的是"砍了工具别在提示里说用它"，这条是反向：砍了工具要明说"没有、试了会失败" |
| R4-8b | **只读模式的 shell 逃逸封堵** | 部分 | **（2026-08-17 复审：接线已补，正文仍待写）**本条的文字此前到不了模型：全仓只有 `ra-runtime::turn::prepare` 的一处写 `system_instructions`，而它读的是 `AgentInstructions::as_static()` 的裸字符串，不经过 `PromptAssembler`。`ra-prompt` 的装配器、`PrefixStabilityTracker`、reminder、role、personality、dump、metrics 在本 crate 之外零引用。按 layering 门禁 `ra-runtime` 只能依赖 `ra-core`，装配必须由装配层注入，而该注入点（`ra-coding/src/prompt/*`）当时十个模块全是空 doc。接线已补：`ra-coding::prompt::assemble_stable_prefix` 走 `PromptAssembler` 装出稳定前缀，`ra-coding::build_agent` 把它写进 `AgentSpec::instructions`，于是 `system_instructions` 从此是装配产物而非裸字符串；`api/prompt-dump.txt` 落了五个角色的可 diff 快照，`cargo xtask prompt-dump` 从 SKIP 变成真门禁（实测改 personality 一个词即 FAIL，改回即 PASS）。**仍未完成的是正文本身**：`ra-coding/src/prompt/` 下**八个**按主题划分的注册位仍是空的，Codex 骨架的正文已单列为 **R4-0a..R4-0h**（原文写作 `R4-0..R4-5`，那是个指不到任何任务的编号）；今天前缀里只有 personality 与 role 两段。<br>**对 rusty-agent 尤其关键**：我们用 `exec_command` 收编了一切，只摘掉编辑工具挡不住写操作。CC 的只读提示逐条列出了逃逸路径，全部要在 prompt 与 guard 两侧封死：`touch` / `rm` / `mv` / `cp` / **重定向 `>` `>>`** / **管道写** / **heredoc** / **`/tmp` 下建临时文件** / 任何改变系统状态的命令。提示只是第一道；第二道是 `ra-coding/guards` 里的只读模式命令 AST 校验 |
| R4-8c | `OneOffAnswer` 角色的三条硬约束 | 部分 | **（2026-08-17 复审：接线已补，正文仍待写）**本条的文字此前到不了模型：全仓只有 `ra-runtime::turn::prepare` 的一处写 `system_instructions`，而它读的是 `AgentInstructions::as_static()` 的裸字符串，不经过 `PromptAssembler`。`ra-prompt` 的装配器、`PrefixStabilityTracker`、reminder、role、personality、dump、metrics 在本 crate 之外零引用。按 layering 门禁 `ra-runtime` 只能依赖 `ra-core`，装配必须由装配层注入，而该注入点（`ra-coding/src/prompt/*`）当时十个模块全是空 doc。接线已补：`ra-coding::prompt::assemble_stable_prefix` 走 `PromptAssembler` 装出稳定前缀，`ra-coding::build_agent` 把它写进 `AgentSpec::instructions`，于是 `system_instructions` 从此是装配产物而非裸字符串；`api/prompt-dump.txt` 落了五个角色的可 diff 快照，`cargo xtask prompt-dump` 从 SKIP 变成真门禁（实测改 personality 一个词即 FAIL，改回即 PASS）。**仍未完成的是正文本身**：`ra-coding/src/prompt/` 下**八个**按主题划分的注册位仍是空的，Codex 骨架的正文已单列为 **R4-0a..R4-0h**（原文写作 `R4-0..R4-5`，那是个指不到任何任务的编号）；今天前缀里只有 personality 与 role 两段。<br>主线程忙时用户插话 → 派生无工具轻量 agent 秒回，**不打断主任务**。CC 的三条约束写得很准，直接用：① 不要提及"被打断"或"我刚才在做什么"——那个 framing 本身就是错的；② 禁止说 `Let me try...` / `I'll now...` / `Let me check...` 这类承诺动作的话（它没有工具，承诺必然落空）；③ 不知道就说不知道，**不要提议去查** |
| R4-9 | 人格与语气段 | 部分 | **（2026-08-17 复审：接线已补，正文仍待写）**本条的文字此前到不了模型：全仓只有 `ra-runtime::turn::prepare` 的一处写 `system_instructions`，而它读的是 `AgentInstructions::as_static()` 的裸字符串，不经过 `PromptAssembler`。`ra-prompt` 的装配器、`PrefixStabilityTracker`、reminder、role、personality、dump、metrics 在本 crate 之外零引用。按 layering 门禁 `ra-runtime` 只能依赖 `ra-core`，装配必须由装配层注入，而该注入点（`ra-coding/src/prompt/*`）当时十个模块全是空 doc。接线已补：`ra-coding::prompt::assemble_stable_prefix` 走 `PromptAssembler` 装出稳定前缀，`ra-coding::build_agent` 把它写进 `AgentSpec::instructions`，于是 `system_instructions` 从此是装配产物而非裸字符串；`api/prompt-dump.txt` 落了五个角色的可 diff 快照，`cargo xtask prompt-dump` 从 SKIP 变成真门禁（实测改 personality 一个词即 FAIL，改回即 PASS）。**仍未完成的是正文本身**：`ra-coding/src/prompt/` 下**八个**按主题划分的注册位仍是空的，Codex 骨架的正文已单列为 **R4-0a..R4-0h**（原文写作 `R4-0..R4-5`，那是个指不到任何任务的编号）；今天前缀里只有 personality 与 role 两段。<br>Codex 的三大价值观 `Clarity / Pragmatism / Rigor` + 明确禁令："no cheerleading, motivational language, artificial reassurance, and general fluffiness"。CC 对应段是"写码风格 match surrounding code"+"如实报告"。**这类段落是稳定前缀的一部分，必须 byte-stable** |
| R4-10 | 缓存命中率回归指标 | 部分 | eval 报告输出每 run 的 `cache_hit_rate`；设基线门槛（目标 ≥70%，对标 CC/Codex 的 ~89%）。**只落地了纯函数** `ra_prompt::metrics`（`calculate_cache_hit_rate` / `meets_cache_hit_target`，口径已对齐 `Usage` 的「cached 是 input 的子集」归一化）。`ra-eval` 的 `assert/cost.rs` 与 `report.rs` 仍是空桩，eval 报告没有输出 `cache_hit_rate`，门槛也没有真正设起来。<br>**第二轮复审推翻了缓存计划的形状**（2026-08-17）：初版把 `CachePlan` 做成「按 `ApiProtocol` 派生出 `ProviderCacheStrategy` + breakpoint + `prompt_cache_key`」，三条都错在同一处——**把 provider 事实当成了协议事实**。`protocol.rs` 自己的模块文档写得很清楚：「`prompt_cache_key` 是否被接受取决于是不是第一方 OpenAI，不取决于用哪个协议；协议层记录机制存在，**provider 层决定要不要发**」，而 `PromptCacheSupport` 又补了一句「第三方 gateway 讲 Chat Completions 也可能拒绝它」。按协议发意味着**任何自定义 `base_url` 的兼容端点都会收到一个可能被拒的未知字段**。同时 `AnthropicEphemeral` / `OpenAiInstructions` / `OpenAiChatMessages` 这三个名字、以及 `CacheBreakpoint` 的 `is_ephemeral` / 逐 mark `ttl_seconds`（Anthropic `cache_control` 的形状）全都进了 provider-neutral 的 `ra-core`；改动还顺手把 `ModelRequest` 上那段准确的警告（「`extra_body` 是静态 provider 注册数据，不是 session cache key 该待的地方」）删掉，换成一句与新代码矛盾的「本类型只携带 provider-neutral 计划」。<br>**已改成**：`ra-core::CachePlan` 只剩 `{prefix_hash, cache_scope}` 两个意图字段，`ProviderCacheStrategy` 与 `CacheBreakpoint` 从 core 删除，`for_protocol` 换成 `for_prefix`；Responses adapter **不再下放 `prompt_cache_key`**，只保留 `validate_cache_plan`（计划若在，必须指向真正发出去的 instructions）。**第三轮已把它接上**：新增 `ra-model::provider::quirks::ProviderQuirks`（默认全关）挂在 `ProviderRegistration` 上，`ProviderFactory::create` 收一个参数把它交给具体 provider；Responses adapter 只在端点显式声明 `with_prompt_cache_key(true)` 时才把 `CachePlan::cache_scope` 下放成 `prompt_cache_key`，没声明的端点照常走隐式前缀匹配，不会收到可能被拒的未知字段。注意它放在 `provider/` 而不是 `compat/`：`compat` 是 feature-gated 且只管 codec 层解析差异，而这是每个协议都要问的请求字段能力。**静态 `extra_body.prompt_cache_key` 现在直接报错**——它是每 provider 一份、所有 run 共用的注册数据，顶替一个 session 生命周期的 scope 等于让「配了缓存」看起来成立而实际把作用域治理废掉；报错是在唯一能改的时刻点名冲突。**另外记一笔**：静态 `extra_body.prompt_cache_key` 是**每个 provider 一份、所有 run 共用**的，它当不了 run scope；run 级 typed override 归 R1-13，别用 `extra_body` 顶。<br>**复审补两条缺口**（2026-08-17）：① **用动态 instruction 的 agent 拿不到任何 cache plan**——`prepare_turn` 只在 `system_instructions` 存在时构造 `CachePlan`，而动态源永远把它留空。Responses 的 `prompt_cache_key` 是与 `instructions` 无关的路由提示，工具表与历史那段前缀本来能受益；要修得让 `CachePlan` 能表达「有 scope 但不指向 prefix」，属类型变更，归本条与 R1-13。② `MIN_CACHEABLE_PREFIX_TOKENS` 只按 instructions 的长度卡，没把工具 schema 算进前缀长度——**第五轮改成由 adapter 判定**：第四轮先把度量拆成两个入参、由 `prepare_turn` 算工具表，但那仍然算不全——Responses 的 `merge_tools` 会把 `extra_body.tools` 里的 **hosted tools**（web search / file search / hosted MCP）与 handoff 合进同一张表，而 hosted tools 没有协议中立表示，运行时根本看不见。于是「值不值得缓存」整个下沉到 adapter：`CachePlan::for_prefix` 不再返回 `Option`、不再收 token 数，`prepare_turn` 只要有稳定 instructions 就无条件附上计划；Responses adapter 在 `merge_tools` **之后**按 instructions + 最终 wire 工具表（序列化形态）对门槛判定，只有过槛且端点声明支持才发 key。判定点只有一个，且在唯一看得见全表的地方。工具表常常是缓存前缀里更大的一半，只按 instructions 判会把「真实前缀好几倍于门槛」的请求排除在缓存之外，而那正是缓存最该生效的场景。<br>**同轮记一条现状**：当前产品前缀只有 personality + role 两段、167–236 tokens，远低于 1024，所以 `ra-coding` 造出来的 agent **今天拿不到任何 cache plan**，provider 声明支持也不会发 key。这是正文没写完（R4-0..R4-5），不是门槛错了——不会为了越过门槛去把提示词写长。已用 `it-coding` 的 `test_the_product_prefix_is_still_below_the_caching_floor` 把这个事实钉住（正文写到跨过门槛时该测试会失败，逼人回来复查缓存链路），并让 `api/prompt-dump.txt` 直接印出 `Cache Plan: none`，而不是让读者拿 token 数去对一个要另行查找的常量 |
| R4-11 | 动态 prompt 解析与 provenance | DONE | 参考 `Prompt` / `GenerateDynamicPromptData` / `PromptUtil`：静态 prompt ref、托管 prompt 参数和动态生成函数统一解析成 `ResolvedPrompt`；动态函数可读 **R3-9a 的** `RunContext` 与 public agent，但输出只能进入 `Volatile + TailMessage`，记录 source/hash/version/provenance 后再由 provider lowering。动态生成失败要结构化报错，不能静默回退并改变稳定前缀。<br>**放置规则由 `ResolvedPrompt::volatile_tail_sections` / `lower_to_tail_items` 在 `ra-core` 内强制**，不是各调用点自觉遵守：`ra-runtime` 只能依赖 `ra-core`（见 layering 门禁），所以规则若留在 `ra-prompt`，真正守着模型请求的那一份必然是第二份实现。动态段要 prefix 位置或 stable 稳定性都**报错而不是丢弃**——丢弃等于生成器以为自己贡献了文本、而那段文本从未到达模型，正是本条禁止的静默回退。<br>**provenance 落在 `PreparedTurn::instruction_provenance()`**（`PromptProvenance` = source + hash + version + provenance）。文本进请求之后，这条记录是事后回答「第 7 轮生成器写了什么」的唯一凭据。<br>**审核修正**（本条初版实现有，已修）：① 动态段中 `Prefix + Stable` 的部分被直接拼进 `ModelRequest::system_instructions`，即写入缓存前缀本身——生成器读 run context，产出逐轮变化，实测三轮三个不同前缀，前缀缓存命中率归零；而当时的运行时测试把这个行为断言成了期望。② 生成器错误与取消都被 `Error::config(format!("{{err}}"))` 重建：用户中断变成 `needs_intervention` 的配置错误、`is_cancelled()` 返回 false，违反[取消契约](Cancellation_Contract.md)「判断取消只用 `is_cancelled()`」；一次可重试的 provider 超时同样被降级。新增 `Error::with_context` 只改消息、保留 variant 与 source，`Cancelled` 原样透传。③ `ResolvedPrompt` 的 source/hash/version/provenance 在运行时被整个丢弃，本条要求的「记录」那一半没做。④ `PromptSection` / `ResolvedPrompt` 直接 derive `Deserialize`，绕开构造期校验——实测可反序列化出 `volatile + prefix` 的段，且 `content_hash` 能与 `content` 完全对不上，而缓存治理全建立在这个 hash 可信之上；改为 `serde(try_from)` 复跑校验并核对 hash。<br>**第二轮审核修正**（2026-08-17）：⑤ **放置规则只强制了一个方向。**`ResolvedPrompt` 只按 section 的 position/stability 判，从不看来源，而 `AgentInstructions::resolve_instructions()` 对静态源返回的正是一个没有 sections 的 `ResolvedPrompt`——实测 `lower_to_tail_items()` 把整段系统宪章包成 **user message**，无报错、无告警，而值本身不记得自己曾是前缀文本。改为返回 `ResolvedInstructions::{Prefix, Generated}`，放置随值一起走；该枚举**刻意不加 `#[non_exhaustive]`**（已登记进 `EXHAUSTIVE_ALLOWED`）：第三种放置不是可忽略的增量，`_` 分支把它塞进碰巧选中的槽位正是本类型要杜绝的。⑥ **hash 与 items 曾是两次独立推导。**`provenance_record()` 与 `lower_to_tail_items()` 各自跑一遍校验并克隆整个 section 表；「hash 覆盖真正发出去的文本」靠两次调用碰巧看到同一个值成立。新增 `ResolvedPrompt::lower()` 一次出两者，每轮少一份拷贝，不变式由构造保证而非靠目视。⑦ **provenance 在 filter 之前算完就脱手。**`apply_model_input_filters` 改为同时收发 `Option<PromptProvenance>`——R10-6b 真正实现 filter 链时，改了文本却不更新记录会直接摆在实现者手里，而不是等着被重新发现。**R10-6b 的结论是不把两者合并**：`content_hash` 的文档改为明说它覆盖的是准备阶段下放的字节，filter 之后的改动由 `ContextFilterReport` 逐条记录在旁边——各记各的事实，好过给 provenance 加一个要跟着同步的状态位。<br>**验收**（`it-core/tests/prompt_contracts.rs` 14 条 + `it-runtime/tests/turn_preparation.rs` 6 条 + `it-prompt/tests/dynamic.rs` 4 条）：动态输出只进 tail、要 prefix 即报错且错误里点名 agent 与段名；多个 tail 段按序全部下放；生成器失败保住 `Recoverability::Retryable` 且原消息不丢；生成期间取消仍是 `is_cancelled()` 且 `scope.reason()` 读得到 `UserInterrupt`；provenance 的 hash 覆盖真正发出去的文本；serde 拒绝 volatile-prefix 与 hash 不符，且外部分词器的精确 token 数原样往返 |

### R4 非目标

| 项目 | 处理 |
| --- | --- |
| 每轮重算整个 system prompt 的便利 API | 不做；这正是 AF 缓存病灶的来源。`instructions` 支持函数形式，但函数结果进的是尾部段不是前缀 |
| 靠加长系统提示提升遵守率 | 不做；对照 codex：完整系统提示 6.6 KB（`core/gpt_5_codex_prompt.md`），工具纪律全是软倾向 |
| 逐字复制 Claude Code / Codex 的 prompt | 不做；只对齐**结构**（稳定前缀 + 尾部增量 + schema 内行为契约），内容原创 |

### R4 验收标准

| 能力 | 标准 |
| --- | --- |
| 前缀稳定 | 一次 9 轮 run 里 `stable_prefix_hash` 9 轮全同 |
| 命中达标 | 真实 provider 上单 run cache hit rate ≥ 70% |
| 可核验 | `prompt dump` 能定位任意一次 hash 变化的触发源 |

---

## R5 上下文管理

### R5 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R5-1 | 工具结果预算 | **DONE** | 落地形态：`ra-core::tool::output` 加 `ModelExcerpt{blocks, artifact_ref}` + `ArtifactRef` + `ToolOutputProjection` / `ToolOutputProjector` 端口；`ra-context::budget::ToolResultBudget` 是唯一实现，默认 64 KiB + 8192 token；编码宿主经 `ToolServices::with_output_projector` 默认装上。**不新建 `ra-artifact` crate**，检索口留给 R5-5。<br>**入场判定与裁剪必须是同一次度量**，这是整条的骨架。先按序列化 JSON 判定、再按原文裁剪的写法会产出「通不过刚刚拒了它那道闸」的摘录——两者差一层 `{"type":"text","text":…}` 外框加逐字符转义，投影器的输出不满足自己的准入条件就没有不变量可测了。所以 `measure` 量的是 `model_blocks()`（provider 真正收到的东西，含元数据注记），摘录的额度就是同一上限减去同一函数算出的 overhead。文本按自身字节 + 共享 estimator 计价，不透明块按序列化载荷计价——那才是它真花的钱，而字符数对它没有意义。判定改量渲染后的投影还顺带退掉一个 guard：摘录不可能在结果被判超限的同时什么都没丢，所以记下的 `Truncation` 不会描述一次没发生的裁剪。<br>**双上限按相反区间生效**：默认值下 ASCII 先撞 token 线（8192 token 折 32,769 字节，是字节线的一半），字节线管的是字符数定不了价的东西——多字节文字（64 KiB 中文约 5,500 token）和根本没有字符的 base64 载荷。**装得下的不透明块留在摘录里**（图片常常就是答案），但不透明块最多占一半额度，否则一张截图能把正文挤成零。<br>**下限是真检查不是非零测试**：一次投影还包括截断行、guidance 句、artifact 注记和省略标记，低于这些固定开销的上限对任何结果都产不出合规摘录，在构造时拒一次，而不是每次调用都失败。下限也明说了它保不了的部分——引用里标识符的长度、工具自己挂的元数据只有拿到结果才知道，所以 `ensure_fits` 量成品再拒。元数据只计价、从不裁剪：截断和 guidance 是记录下来的事实，砍掉半句解释比不给解释更糟。<br>**投影器回传 `ToolOutputProjection` 而不是 `ToolOutput`**：收发都是结果的 trait 等于允许上下文策略替换掉它本该在概括的那条记录（上下文优化变成不可逆数据丢失），而 runtime 只能靠每次调用整份克隆去事后检查。现在投影只带可选摘录、截断和 guidance，由 runtime 应用到它仍然持有的 output 上——误用不可表达，克隆没了，策略是往工具记下的东西后面追加而不是覆盖。<br>**`ArtifactRef` 按 run + call 双重定名**：provider 只保证 call id 在自己那轮对话里唯一，两个会话都用 `call-1` 就会指向同一件 artifact。**它是身份不是取回承诺**——给模型的那句话说的是「已保存在会话记录中」而不是「可获取」，因为还没有工具答得了取回请求，凭空许一个不存在的能力只会换来一轮浪费。<br>**`ArtifactRef` / `ModelExcerpt` 的反序列化走构造函数同一套规则**：校验只写在构造函数里在这里就等于没写，checkpoint 与 rollout 正是靠反序列化拿到这些值的；派生实现会放进空引用或零块摘录，而零块摘录要到下一轮才以 provider 拒绝的形式暴露。`TOOL_OUTPUT_SCHEMA_VERSION` 升到 2，旧记录没有 excerpt 照常读，本版本写的 excerpt 经旧版本的 unknown 袋子原样回写。<br>**验收**（`tests/it-context/tests/tool_result_budget.rs` 10 条 + `tests/it-core/tests/tool_output.rs` 第 17/18 条 + `tests/it-runtime/tests/typed_dispatch.rs` 3 条）：装得下的结果原样返回且不记截断；超限保留 head/tail 且完整正文进不了 `model_blocks()`；**产出的摘录必须落在产生它的预算内**（三组上限各验一次，这条是前一版没人守而出过错的地方）；token 线在字节线有余量时照样报；元数据先计价再分配正文额度；装得下的图片留下、装不下的只留一行说明且 `retained_bytes` 为 0；低于下限的上限构造时拒收；引用太长时投影带着实测值拒绝；反序列化挡空块与四种坏引用；dispatch 侧证明投影器只能追加——工具的 `TruncationStage::Tool` 和 guidance 都排在策略那条前面 |
| R5-2 | 模型上下文窗口表 | **DONE** | 落地形态：`ra-context::window` 提供 `ContextWindowTable`（内建表 + 覆盖）、`ContextWindowThresholdRatio`（基点整数，默认 6000 = 60%）、`ContextWindowConfig`（宿主配置）。内建 69 条与 openai `capabilities/compaction.py` 的 `_MODEL_CONTEXT_WINDOWS` 逐条比对一致（1,047,576 / 400,000 / 200,000 / 128,000 四档），归一化同构：trim → lowercase → 剥 `openai/` → 去 `.` 和 `-`。<br>**未知模型返回 `None`，不猜**：编出来的窗口会把 provider 拒绝伪装成上下文管理的决策；宿主知道自己的端点，用显式覆盖项报窗口。<br>**阈值按比例、用整数算**：AF 的 256K 绝对阈值在峰值 171K 时从未触发。比例存基点（`u16`），`apply` 先商后余（`whole * ratio + remainder * ratio / scale`），大窗口不会在算阈值的路上溢出。<br>**配置是覆盖而非替换表**：内建项随框架演进，端点只拥有自己真正服务的模型；两个归一化后撞车的配置名直接拒收，不让 map 顺序决定用哪个窗口。<br>**查表只有一条路径**：`ContextWindowConfig` 持有构造时解析好的 table（`#[serde(skip)]`，反序列化时重建），内建表是 `LazyLock` 静态量、整个进程只建一次。曾经的写法是 config 自己扫覆盖项再查一张新建的表——每次未命中重建 69 条 map，而且**归一化两次**：`model_lookup_key` 不幂等，`open-ai/gpt-5` 只有在去掉分隔符之后才长得像 `openai/` 前缀，第二遍会把它剥掉并答出一个表里根本没有的窗口。<br>**比例反序列化的整数性检查不能用 `f64::EPSILON`**：`0.56 * 10_000` 是 5600.000000000001，而 EPSILON 是 1.0 处的 ULP、在这个量级上小了约四千倍——精确整数判定会拒掉 10001 个合法四位小数里的 1149 个，并让这个类型读不回自己刚序列化出去的东西。表示误差上界实测 9.1e-13（在 `0.5005`），第五位小数至少造成 0.1 的偏移，容差取 `1e-9` 卡在两者之间。<br>**验收**（`tests/it-context/tests/context_window.rs` 7 条）：内建表四档经归一化后各命中一次且 `other/gpt-5.4` 不命中；默认 60% 触发值与未知模型返回 `None`；覆盖项既能替换内建项也能新增未知模型；配置序列化往返（含 `#[serde(skip)]` 的 table 确实重建）；两位小数比例 0.56/0.57/0.68/0.69/0.81 全部通过而 0.60001 类仍拒；config 与其 table 对同一批名字形态逐个给出相同答案（含 `open-ai/gpt-5` 两侧都是 `None`）；空模型名、零窗口、归一化撞车、越界比例、未知字段五种坏配置全拒 |
| R5-3 | Compaction | **DONE** | 落地形态：`ra-context::compaction` 提供纯 `ContextUsage` / `CompactionLimits` / `CompactionAssessment` 三维触发器。总 token 阈值可直接由 `ContextWindowConfig` 为已知模型解析（60% 窗口）；未知模型返回 `None`，不能凭名字猜容量。任一阈值到线即触发，并保留所有同时命中的 reason；provider 有精确 tokenizer 时直接交 `ContextUsage`，本地 fallback 才用 provider-neutral item 表示的共享估算。<br>**估算只算内容，不算 wire framing**：走 `serde_json::to_value` 后遍历，字符串值 / 数字 / 布尔按模型读到的形态各算一次，字段名、分隔符与 JSON 转义一律不计。按序列化文本计量会把 JSON 形状的工具结果抬高约五分之一，两个后果都不能接受——60% 窗口阈值实际会在 50% 附近触发，而 `ToolResultBudget` 刚裁到单结果上限的 excerpt 会在这里被判超限，那是压缩再多轮也清不掉的 `SingleItemTokens`。`budget` 当初正是为同一理由弃用了序列化 JSON 口径，所以 `CHARS_PER_TOKEN` 上移到 `ra-core::prompt` 与 `estimate_tokens` 并列，两边反演同一个常数。<br>**触发必须有够得着的补救**：压缩会原样留下 head/anchor/tail，因此一个仅靠保留区就能满足的阈值会在下一轮再次命中，run 会永远在出摘要。`CompactionLimits::ensure_converges_with` 拒绝「保留上限 + 1 条摘要仍够到 `max_items`」的策略配对（配套 `AnchorRetention::max_retained_items`）。token 那一半没有静态答案——取决于超标项落在哪——所以写进 `CompactionReason::SingleItemTokens` 的文档：压缩只清得掉它丢弃的那条，留在保留区里的要靠 `ToolResultBudget` 这类逐项投影，而不是再压一轮。<br>**度量与配置都校验而非信任**：`ContextUsage` 要求单项不超过总量、且各项能凑得出所报总量（provider 从已下线的响应字段读 `largest_item_tokens` 会得到一个看着自洽的 0，单项触发就此静默失效）；单项上限 ≥ 总量上限被拒（它只会与本该提前预警的那条同时命中）；阈值比例 0 在写下的地方就被拒，非零比例把小窗口整除成 0 时报错点名模型与比例。<br>摘要用 `CompactionSummaryBuilder` 锁定 CC 的九段：`1 Primary Request and Intent / 2 Key Technical Concepts / 3 Files and Code Sections / 4 Errors and Fixes / 5 Problem Solving / 6 All User Messages / 7 Pending Tasks / 8 Current Work / 9 Optional Next Step`。所有 slot 都必须显式填入，且**空白内容被拒**——没什么可报的槽位要用文字说出来（`None.`），空槽与「摘要请求被截断或拒绝」在结果上无法区分；第 6 段是按时间顺序的 `Vec<String>`，逐条 fenced 渲染（fence 比消息内最长反引号连跑多一个，下限三个），用户正文不能伪造摘要标题或 fence 而吞掉后续消息，空白条目同样被拒。`AnchorRetention` 产出不重复的 `preserved_segment { head, anchor, tail }`：head/tail 先保留，剩余 anchor 取 caller 标出的最新 N 条，最终仍保持原历史顺序。<br>本条只决定投影与摘要形状，不发模型请求、不改 session / `RunState`：后续 capability 接入需复用原 stable prefix、构造无工具 schema 的 summary request，并由 session owner 原子安装结果；这使 compaction 本身不能清空熔断器、claim 或 usage。<br>**验收**（`tests/it-context/tests/compaction.rs` 11 条）：三维精确触发与全部 reason、disabled 维度不误触发、已知 / 未知模型阈值解析与舍零拒绝、估算只计内容（断言严格低于同一 item 的序列化文本估算）、坏度量与坏配置拒绝、保留策略与 item 阈值的收敛检查、九段完整性与逐条用户消息（含 0 / 3 / 4 个反引号连跑对应 3 / 4 / 5 个反引号的 fence）、摘要 slot 误用与空白内容拒绝、**R15-1 欠的那条：「改了但没验证」经第 4、8 段跨压缩保留**、anchor 的最近选择 / 去重 / 越界拒绝。<br>提交切分：R5-2 的 `window::*` 公开面基线由前一条独立提交 `chore(api): bless the context-window public surface` 补上，不与本条混在同一份 baseline diff 里。 |
| R5-3b | **压缩 / resume / replay 不得破坏控制面状态** | **DONE** | 长时程 agent 最典型的静默失效：**压缩之后熔断器失忆，同一个死循环再跑一遍**。R3-6c 的失败计数、R3-4d 的 claim 持有关系、R3-8 的预算累计都是 runtime 权威状态，而压缩、resume、replay 都在重建上下文——三者中任何一条把权威状态一起"摘要"掉，症状都不是报错，而是 agent 看起来更有耐心了。<br>**规则一句话：压缩只能改模型可见投影，不得改变 runtime 权威计数器。** `ra-context::project_compacted_model_input` 只借用 `RunItem` 权威历史并返回 `ModelInputItem` 投影，不接收或变更 `RunState`；非模型输入的本地审批不会被列入摘要覆盖项。R3-6c 的验收里已有"resume/replay 不重复计数"，本条是 R5 侧的对称条款。**R3-6c 落地时这条风险已被实测证实不是理论**：它的失败记录起初让成功与拒绝绕过重放去重，于是 checkpoint 恢复后重放一轮更早的成功，就把之后累积的 streak 抹掉了——修法是三种结局共用一个有界 `call_id` 窗口（`ToolFailureEntry::already_recorded`），与整轮指纹一起构成两层去重。R5 侧做压缩 / resume 时直接复用这套判据，不要另起一套。三条不变量：① compaction 前后，同一 `ToolFailureRecord` 的 attempt 计数与证据指纹不变；② 任何 claim 在压缩过程中不得被隐式释放或重复获取；③ resume 后重放同一段历史，预算累计值与压缩前一致，不重复扣减。<br>验收：`tests/it-runtime/tests/compaction_control_state.rs` 构造"压缩恰好发生在熔断阈值前一次失败之后"的 fixture：checkpoint 恢复后重放旧 `call_id` 不改失败记录/预算，下一次失败仍抵达阈值，随后调用被 `tool.no_progress` 拒绝；动态 claim 的求值次数在压缩期间不变。确定性 trace 见 R3-8b |
| R5-4 | 老工具结果选择性淘汰 | **DONE** | 落地形态：`ra-core::state::ToolOutputReferenceTracker` 是按 `RunId + CallId` 隔离的最近引用账本。**位置在 ra-core 而不是 ra-context**——它是要随 checkpoint 恢复的持久化状态，和 `RunState` 同层；它现在就是 `RunState` 的一个字段，`RUN_STATE_SCHEMA_VERSION` 因此 2→3，v2 旧 checkpoint 经 `Option` + `TryFrom` 补一个空账本而不是被拒，恢复时校验账本的 run 与 state 的 run 一致。账本自带 `schema_version`（当前 2）与 `Unknown`（记录级也有），新版本写下的字段在旧版本上原样回写。宿主在每轮结算后经 `record_turn` 写入新输出与其**结构化**引用，重放不会把较新的时间戳倒退；错 run、未知输出、引用早于产生轮、跨轮复用 call ID、**以及在账本已走过的轮次里首次登记新输出**一律拒绝——最后一条挡的是乱序结算，它会给刚产生的结果盖上过去的轮号，下一次投影就把活数据摘掉。框架不从 assistant 文本里猜 call ID——自然语言里的一个字符串既不是通用 provider 语义，也不够成为丢弃结果的证据。<br>**接线**：`ra-core::tool` 新增两个 port——`ModelInputProjector`（投影请求输入）与 `ToolOutputReferenceExtractor`（产品自己的结构化引用契约）。runner 只认这两个 trait，所以 ra-runtime 不依赖 ra-context；`ra-context::eviction::ToolOutputReferenceTrimmer` 实现前者，`CodingHost::build_run_config()` 默认装上。**轮次轴是整个 run 的**：`progress.turns` 是段内计数（`RunResult` / `TurnRecord` / trace / 错误 item id 都按它算），账本那两处改用 `reference_turn()` = 段开始时从 `last_completed_turn()` 取一次的基数 + 段内轮次；基数只取一次，中途重算会把本段已记的轮次再加一遍。没有这条，resume 后第一次工具调用就撞上乱序守卫直接报错，且第一段的结果永远不再可淘汰（`current - last` 借位为空）。**高水位是存下来的，不是从记录里派生的**（账本 schema 因此 1→2）：一轮可以既没有工具输出也没有结构化引用，派生法看不见这种轮次，会把轴压缩；旧 checkpoint 没有这个字段，回退到「取最新一条保留引用的轮次」这个保守下界，而写着的水位若低于某条记录的引用轮次则直接拒绝反序列化。<br>**淘汰口径**：已完成的未引用轮次 = `current_turn - last_referenced_turn - 1`（正在准备的那一轮响应还没到，不计入），达到 N 即可裁；未登记结果保守保留，已被后续引用的结果无论离尾部多远均保持完整。替换复用 R5-8 的摘要渲染与 allowlist、metadata 保留、字符上限不变量；本条额外以 R5-1 唯一的 run+call 编码铸 `ArtifactRef`，即使该结果此前尚未撞上单结果预算，摘要也能指向 session 内的权威完整记录。artifact 注记与 metadata 会先计价；若固定开销本身装不下，退回 R5-8 同样的裸摘要，而不是越过上限——**真实开销要按这个数算**：hex 编码把两个标识符各翻一倍，UUID run 加约 30 字符的 provider call id 合起来是 143 字符引用、213 字符固定开销，`max_output_chars` 低于约 222 就恒定丢定位符。这个下限刻意不在构造期校验（构造期拿不到标识符，理由同 R5-1 的 `floor`），只写进文档并由测试钉住 221/222 的临界。<br>**本条刻意未做**：账本整个 run 只增不删。裁剪是每轮从未裁历史重算的纯投影，而未登记 call ID 是被保守保留的，所以剪掉一条已摘要结果的记录会让它退回「未知」、下一轮又把完整结果发回模型；真要加界得有个比记录活得更久的 tombstone，那是 retention 契约，不归本条。<br>**验收**（`tests/it-context/tests/tool_output_reference_eviction.rs` 11 条 + `tests/it-runtime/tests/runner_loop.rs` 2 条 + `tests/it-coding/tests/coding_host.rs` 1 条）：两个位置相同的老结果中，第 5 轮仍显式引用的在第 9 轮保持完整、从未引用的变为带 artifact 的摘要；未登记输出不裁；完整轮次计数的五个边界（含产生轮本身、正在准备的那一轮、current 落在 last 之前）；真实 UUID/call id 下默认上限仍留得下预览，且 221 退回裸摘要、222 恰好装下标记与定位符；legacy 字符串结果被换成带定位符的结构化结果而权威记录形状不变；三条错误路径且被拒的一轮不改账本；账本 JSON 往返、未知字段两级原样回写、旧 replay 不倒退、call ID 不能跨轮改绑、跨 run tracker 被拒；空的已完成轮次照样推进高水位并在往返后存活，无该字段的旧 checkpoint 回退到记录高水位、而水位低于已保留引用的 checkpoint 被拒；`Default` 等于用模块默认值构造的 `new`；零窗口拒绝。runner 侧：projector 每次请求前跑、结算后记录产出（轮次 1、2），resume 段续轴到 4、5 且第一段的保留事实原样存活；`CodingHost::build_run_config()` 装了 projector。 |
| R5-5 | 压缩历史外部可寻址归档 | **DONE** | 落地形态：`ra-core::item::ArchiveRef` 用 session + compaction item 双重定名，`Compaction::with_archive_ref` 把可寻址引用带入摘要；`ra-context::archive::read_archived_history` 从权威 `Session` 按 compaction 覆盖的 item ID 回读完整 `RunItem`，不复制第二份历史。<br>**引用逐字带走，读取时永不否决**。本版本拆不开的地址——新版本引入的格式、别的宿主按不同保留字符集编码出来的——照样原样往返、照样按原文命中自己那条记录，只有 `session_id()` / `compaction_item_id()` 报不可用。读取时报错等于让整条 `Compaction`（进而是承载它的 session 记录）读不出来，而 rollout 那侧 `payload()` 失败的记录是被静默跳过的，正好是 `compat` 三条策略要防的静默数据丢失。<br>**按记录携带的引用定位，而不是引用里写的 session**。那个 session 是出处：子 agent transcript graft 进根、fork、导入的日志都保留写入时的地址，拿它当闸门就会把明明躺在眼前的历史报成不可用；而这个函数本来就只够得着传进来的那个 session，取地址的字面值不扩大任何读取面。地址能解析时，它写的 compaction item ID 仍必须与存储它的记录一致。<br>**模型被告知什么由宿主决定**。`with_archive_ref` 把取回提示做成参数：只有宿主知道自己到底有没有接检索工具、叫什么名字、用哪种语言，`None` 就是只记地址、什么都不说——给模型一句取不到的取回指令，换来的要么是必然失败的工具调用，要么是「历史已恢复」的假话。提示存在记录上而不是下沉请求时现拼，还顺带让模型真正看到的字数被 `ContextUsage::estimate_model_input` 计价、被 resume 原样回放。<br>schema 版本保持 1：两个字段都是扩展规则下的可选新增，升版只会让所有存量 compaction 对着一个不存在也不需要的迁移报 `needs_migration`。<br>缺失 / 重复 / 落在摘要之后的覆盖项按 session 损坏拒绝，避免把不完整材料伪装成完整历史。<br>**验收**（`tests/it-context/tests/archive.rs` 8 条）：完整回读且不改动权威历史；跨 session 搬家后仍解析；错配与陈旧引用返回不可用而非报错；三条损坏分支（缺失、覆盖项重名、历史内 ID 重复）各验一次；未知格式地址逐字往返并仍能命中；无提示时模型只看到摘要，有提示时只多出宿主写的那一段；投影带上引用而不展开归档。 |
| R5-6 | Oversized input preflight | **DONE** | **Codex 在这里有实锤缺陷，不要复刻**：两个 39 MB 会话各只有 10 行，体积来自一条 **19.9 MB 的用户输入**，`task_started=0` / `function_call=0`——模型那轮根本没跑起来。根因是压缩触发条件挂在 **messages 数量门槛**上，单条巨型输入不满足。三维触发 (a) messages 数量 (b) 单条体量 (c) 总 token 已由 R5-3 的 `CompactionLimits` 落地，本条做的是新输入这一侧。<br>落地形态：`ra-context::preflight` 提供 `InputPreflightConfig` / `InputPreflight` / `InputSummarizer` / `PreprocessedInput`。超限的单条 user text 保留首尾原文、中段分块 map-reduce，`InputSummarizer` 是宿主端口（`#[async_trait] + Send + Sync + 'static`，可 `Arc<dyn>` 装配，与 `ToolOutputProjector` / `ModelProvider` 同形）——`ra-context` 自己不发模型请求，宿主才知道用哪个模型、怎么记账、怎么重试。<br>**配置按它自己渲染出的输出校验，而不是只看首尾额度**：preamble、三个 heading、两道 fence 加一 token 摘要，用空占位跑一遍真实模板量出来（与 R5-8 量固定开销同法），装不下的上限在构造期就拒，而不是等一整轮 map-reduce 付完钱才发现无解。这条预留同时让首尾边界不可能交叉——够得上预处理的输入必然比两道边加起来长，中段恒非空，`split_edges` 的切片算术因此不再有 panic 面（仍加 clamp 兜底：推理错了的代价该是一条错误信息）。<br>**`for_model` 与 `for_compaction` 分工**：前者按计划的 0.6×context_window 取额度，文档**明说**它不为保留历史留余量；跑压缩的宿主该用后者，它停在单条上限**下方一 token**——`CompactionLimits::assess` 是 `>= limit` 到线即触发，而最新那条 item 触发的 `SingleItemTokens` 压缩清不掉（清它等于丢掉用户刚发的消息）。未知模型 / 无单条上限的策略返回 `None`，不猜容量。<br>**首尾原文进 individually sized fence**，复用 `compaction::summary::code_fence`（提为 `pub(crate)`，两边都注明不许有第二份拷贝）：边上是调用方自己的文字，能关掉自己那段的文字就能把原文冒充成「被丢弃部分的摘要」。<br>**map 有界并发**（`futures::stream::buffered` 保序，`DEFAULT_MAP_CONCURRENCY = 4`，比 runtime 那条 8 窄因为每单位是模型往返）；**map 产出在发 reduce 请求之前先计量**，几十条摘要拼起来本身超限时拒绝而不是原样再发一次。<br>**验收**（`tests/it-context/tests/preflight.rs` 14 条）：1 MB 单条输入确实走预处理（多次 map + 一次 reduce、首尾逐字保留、不原样透传）；分块逐块可辨地按源顺序抵达 reduce；装得下的输入零调用原样返回；fence 挡住伪造 heading（含四反引号）；并发峰值 >1 且 ≤ 配置宽度，设 1 时恰为 1；CJK 首尾不被切碎；三种输入形状拒绝；map / reduce 空白摘要各自报出所在阶段；超长摘要报错带上摘要自身 token 数；map 摘要超限时 `reduce_calls == 0`；构造期可行性拒绝；`for_compaction` 取 199,999 且边界输入确实会触发 `SingleItemTokens`，一 token 上限点名报错，无单条上限返回 `None`。 |
| R5-7 | 上下文用量 API | **DONE** | `ra-context::usage::ContextUsageBreakdown` 从即将发送的 `ModelRequest` 按 system / tools / messages / tool_results / reasoning 五类估算。replay item 用 content-only 走值；定义（工具、handoff、结构化输出）按渲染文本计价并整表只取整一次 —— JSON Schema 的载荷在键上，走值会把参数名算成零。`ModelToolDefinition::advertised_chars` 与 `advertised_bytes` 共用一个渲染器，字节预算与 token 估算不会对「广告了哪几部分」有分歧。重放的 MCP 工具目录同理归入 tools。同一趟遍历顺带产出 compaction 读的 `ContextUsage`（`model_input()`），宿主要两个数只走一遍历史。`for_model` 仅在 `ContextWindowConfig` 有明确模型窗口时返回容量与 basis-point 占用，未知模型不猜。它是 UI/诊断 API，不与 provider 事后计费 usage 混用。 |
| R5-8 | 旧轮次工具输出裁剪器 | **DONE** | 落地形态：`ra-context::eviction::ToolOutputTrimmer` 是 `&[ModelInputItem] -> Vec<ModelInputItem>` 的纯投影，只借用不改写，session/archive 保留原文。保护窗口 = 最近 `recent_turns` 条 user 消息及其之后的一切；更老的 `ToolCallOutput` 超过 `max_output_chars` 才有资格，配了 `trimmable_tools` 时还要求配对的 call 名字在白名单里（空集合＝整体关闭，是宿主停用一个已配置裁剪器的方式）。文本 preview、structured text preview、opaque image/file 只计入说明不进预览，均已覆盖。<br>**保护窗口加了 item 数兜底,这是对上游的有意偏离**：上游只数 user 消息,于是「一次请求 + 五十轮工具调用」——每个编码会话的形状,也正是本条存在的理由——永远一条都不裁。user 消息不足 `recent_turns` 时,边界退化为尾部 `recent_items` 条（默认 20,经 `with_recent_items` 设定）。**是兜底不是下限**：尾部 item 数很容易比 user 轮窗口伸得更远,做成下限会把短会话整个保护住。<br>**结构化结果裁完仍是 `ToolOutput`,不是裸字符串**。裸字符串会丢掉 R5-1 刚记下的 `ObservationMetadata`（截断事实 + 写给模型的那句话）和 `ArtifactRef`——内容没了之后,这些恰恰是最该留下的。所以 metadata 整份带过去,原来有 `ModelExcerpt` 的复用它的 artifact 引用；**新引用铸不出来**,一个引用要按 run + call 定名,而这个投影拿不到 `RunId`。`structured_summary_budget` 用空 summary 占位渲染一次,量出保留字段的固定开销、把余额交给 summary，占位块保证 join 分隔符数量与真实 summary 完全一致；只在 `固定开销 + [Trimmed] ≤ 上限` 时才返回预算,所以 summary 兜底恒装得下。**元数据本身撑爆上限时退回裸摘要**——这与 R5-1 的 `ensure_fits` 报错相反,是有意的：R5-1 在派发期,配置错误当场可改；本条是请求期投影,为一条老结果让整个请求失败更糟。<br>**计量按 `model_blocks()`、预览取内容块**,两者必须分开。预览若也走 `model_blocks()`,元数据注记排在最前面,一条被 R5-1 摘录过的结果会把整个预览花在 `[truncated by context budget: …]` 上,模型拿到框架的话、拿不到自己要的输出。计量则必须含注记与 artifact 注记,那才是 provider 真收到的东西。<br>**每个替换体都受 `max_output_chars` 约束**,于是「有没有真的变小」从运行时检查变成算术不变量（替换体 ≤ 上限 < 原尺寸）；`new()` 拒绝装不下 `[Trimmed]` 的上限,理由与 R5-1 的 `floor()` 相同。header 里写着 preview 长度、于是自身宽度依赖那个数,渲染两次即收敛：更短的预览只会让 header 更短,拿不回预算。`source_text` / `opaque_block_bytes` 与 `budget` 共用,两个裁剪阶段读同一批块、用同一种拼法——两个分隔符不一致的 join 会让从中取的每个计数都不一致。<br>`ra-core::item::ToolCallOutput::with_output` 换掉模型可见载荷,保留 `call_id`、error 标志与 unknown 字段。<br>**只处理 provider-neutral 的 `ToolCallOutput`**：上游那条 `tool_search_output` 是 OpenAI 专有 replay item,`ra-core` 有意不设对应类型,所以「tool-search schema prose 裁剪」不在本条范围内。**「报告节省量」也未做**：header 报的是原始规模,节省量目前只能由调用方相减得出,没有typed 的报告通道——要 typed 报告需要一个 `TruncationStage` 新变体（动 ra-core 公开枚举与 wire 值）,归后续。<br>**验收**（`tests/it-context/tests/tool_output_eviction.rs` 16 条 + `tests/it-core/tests/item_model.rs` 1 条）：保护窗口内外的裁与不裁、输入不被改写；**预览与 header 之间是真换行**（`\\n` 写成转义反斜杠会把两个字符送进 prompt,这条就是为它加的）；四组配置下替换体不超上限（含 `preview_chars` 4000 撞 9 字上限）；白名单只裁配对工具、空白名单整体关闭、无白名单时未配对结果按 `unknown_tool` 裁、有白名单时不裁；单 user 轮次的 run 经 item 兜底仍能裁,历史短于兜底窗口则不动；opaque 块只报数不泄露 base64；metadata 与 artifact 引用跨投影存活且整个模型视图不超上限；元数据撑爆上限时退回裸摘要；预览取的是工具输出而非框架 guidance 句；`call_id` 与 error 标志存活；配置下限与 `recent_items = 0` 均拒；`Default` 等于四个公开常量。 |

> **R5 的前置顺序门（2026-08-11 补）**：R5-3 compaction、R5-4 老结果淘汰以及任何会替换/丢弃 `ModelResponse` 工作副本的路径之前，必须先完成 **R9-0a 最小 rollout append writer**。它在每次模型结算时追加不可裁剪的逐请求 usage，并为同一 root thread 的已持久化事件给出稳定 append 顺序；R5 只可裁剪模型投影和 `RunState` 工作副本，不能裁掉该对账基线。R9-0a 不是验证账本、不是 SQLite 查询层，也不提前实现完整 session 系统；它只是 R9-0 的最小 append-only 落盘切片。执行顺序明确为 **R9-2a → R9-0a → R5-3/R5-4/R5-8**（其余 R5 可按依赖独立推进）。

### R5 非目标

| 项目 | 处理 |
| --- | --- |
| 降低单工具结果上限来省钱 | 不做；AF 已验证会伤 `read_file`（正要编辑的行被砍） |
| 把压缩做成 runner 里的一段 if | 不做；压缩是一个 Capability 的 `process_context`（R10） |

### R5 验收标准

| 能力 | 标准 |
| --- | --- |
| 阈值合理 | 长任务在窗口 60% 处触发压缩，不会"到 171K 还没触发" |
| 保真 | 压缩后仍能回答"改了哪些文件、哪些验证过、哪些没有" |
| 可回查 | 压缩掉的原始消息可通过 archive_ref 取回 |

---

## R6 权限、审批与中断恢复

### R6 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R6-1 | `PermissionMode` | **DONE** | **落地形态**：`ra-core::permission::PermissionMode` 五变体 + `#[non_exhaustive]` + `Default` 档为 `Default::default()`；`ALL` 常量按声明序列出当前全部档位。线格式用 camelCase 对齐既有公开词表，`label()` 与 serde 表示逐字相同，`Display` 也渲染同一个串——同一个值不能在日志里叫 `AcceptEdits`、在线上叫 `acceptEdits`。**未知线值显式失败**（沿用 R3-1b 的口径）：新版本写的档在旧版本上读成「按常规审批」是最不该有的静默降级；**不留 `Custom` 开放标签**，这个轴上路由的是执行边界而不是第三方分类（扩展安全第 5 条的判据）。<br>**两个刻意的形状**：① **不派生 `PartialOrd` / `Ord`**——声明序不是宽松度序（`BypassPermissions` 最宽松却夹在中间），`mode >= BypassPermissions`、`max()` 取「更严的那个」这类写法会编译通过并且在一个安全策略上是错的，而 `#[non_exhaustive]` 下往中间插变体会静默改变所有这类比较，且没有任何测试会红；档位只能 match，不能比大小。② **`Plan` 是只读而不是禁工具**——计划期仍要能查看工作区，这才和附录 B 裁决 3 的「计划期强制只读」、`PromptRole::Planner`（`is_read_only`）是同一个姿态；写成「禁止一切工具执行」会让这一档实现出来就不可用。<br>**验收**：`tests/it-core/tests/permission_mode.rs` 3 条（默认档；`ALL` 顺序 + 每档 `label` / `Display` / 线格式往返 + 标签是稳定 ASCII 标识符；未知值与非规范拼写均被拒）。<br>**本条刻意未做**：规则匹配、审批采集、沙箱实施与 UI 文案各有归属层，core 只持有它们要达成一致的那个选择；`PermissionRule` 归 R6-2、`PermissionDecision` 归 R6-3、运行中切换归 R13、配置矛盾预警归 R6-9。<br>**原始约束（保留）**：`Default \| AcceptEdits \| BypassPermissions \| Plan \| DontAsk`（claude `types.py:25`）；运行中可通过控制协议切换（R13）。`PermissionMode`、规则身份和决策值对象属于 `ra-core`；UI 文案、沙箱实现和具体持久化不进入 core。R8-1 之前先实现一个最小静态 allow/deny 策略，完整交互审批仍归本阶段后续任务。<br>**给 R6-6 留的一条**：这个值落进 `RunState` 时那个字段**不要**加 `#[serde(default)]`——字段缺失静默变成 `Default` 档，比 `Plan` / `DontAsk` 都宽松，与 `run_id` / `next_host_event_seq` 被定成必填 serde 字段是同一条理由。 |
| R6-2 | 权限规则引擎 | **DONE** | **落地形态**：core 出三个值类型——`PermissionDecision{Allow \| Deny \| Ask}`（三值、无字段）、`PermissionScope{Read \| Edit \| Execute}`、`PermissionRule{decision, tool_name?, namespace?}`；`ToolOptions` 增 `permission_scope`（`read_file` 声明 `Read`、`apply_patch` 声明 `Edit`）。运行时出 `ra-runtime::permission::PermissionEngine{mode, rules}`：规则**逆序取最后一条命中**，`Plan` 档对非 `Read` 直接拒且规则打不开它，`DontAsk` 把一切 `Ask` 归一成 `Deny`，`BypassPermissions` 与 `AcceptEdits`+`Edit` 在无规则命中时放行。派发链在「重复准入」与「审批」之间插入权限阶段：先算固定裁决（不进第三方代码），只有落到默认路径才去问工具的 `needs_approval`；`Deny` 走 `ToolErrorKind::PermissionDenied` 的 refusal（工具没跑，因而不进无进展熔断的证据），`Ask` 走既有的 `AwaitingApproval` 中断。`exec_command` 与 `apply_patch` 同时补上 `ToolApprovalPolicy::Always`——否则 `Default` 与 `AcceptEdits` 两档对树上唯一的 Edit 工具毫无区别，档位语义实现出来就是空转。<br>**四个刻意的形状**：① **engine 是构造函数的必填参数**而不是 builder 可选项——`TurnSettlementRequest::new` / `TurnExecutionRequest::new` / `ToolDispatchRequest::new` 三处都是公开 API，漏传会静默得到「`Default` 档 + 空规则集」，比 `Plan` / `DontAsk` 都宽松；这和 R6-1 给 R6-6 留的「那个字段不要 `#[serde(default)]`」是同一条理由，也和 `ToolDispatchRequest` 里 `cancel` 做成必填而非可选同形。② **`PermissionRule` 用 `deny_unknown_fields`**：规则是要落盘的，新版本写的**收窄**字段（比如日后的前缀匹配）被旧版本静默丢掉之后，剩下的是一条更宽的规则——沿用 R6-1「未知线值显式失败」的口径。③ **`PermissionScope` 默认 `Execute` 而不是 `Read`**：工具作者忘了声明，最坏结果是计划模式下多拦一个工具，不是多放一个。④ **求值只有一份实现**：`evaluate` 就是 `fixed_decision(..).unwrap_or_else(\|\| normalize(fallback))` 一行；两份会漂移的安全判定比一份慢的更贵。<br>**验收**：`tests/it-runtime/tests/permission_engine.rs` 8 条（显式 deny 压过 bypass 档；后置规则覆盖广谱规则；三档按声明 scope 生效，且 `Plan` 档下带 `Allow` 规则的执行类工具仍被拒；`DontAsk` 把待审批变成拒绝；`RunConfig` 换档不丢规则；带命名空间的规则经派发命中裸模型名；`AcceptEdits` 对非 Edit scope 仍走工具自己的策略；`Ask` 规则压过 bypass 档）+ `tests/it-core/tests/permission_mode.rs` 新增 2 条（规则匹配与序列化往返，未知字段被拒；scope 默认值与线格式）。<br>**转交出去的四件事**（不是欠账，各自已有主）：`PermissionUpdate{destination: Session \| LocalSettings \| ProjectSettings \| UserSettings}` 归 **R6-3**——那行的 `Allow{updated_input, updated_permissions}` 本来就含「顺带写规则」；落盘与撤销归 **R6-7**——`state.approve(item, always)` 的 `always` 就是「这条存成规则」；`prefix_rule(pattern)` 命令前缀白名单归 **R6-8 / R6-8b**——命令 AST 判据与四级自动审批分类是它真正的判断点；按目录 `trust_level` 归 **R6-10**——沙箱与审批的边界声明。<br>**一处主动收窄**：原文 `PermissionRule{tool, matcher, behavior}` 里的 `matcher` 收窄成「工具名 + 命名空间」，**不含参数匹配**。`exec_command` 的 `command: Vec<String>` 在 core 这一层没有 schema，把前缀匹配写进一个 provider 中立的值类型，等于把某个工具的参数形状硬编进公共词表。减打断这个目标不放弃：`ToolApprovalPolicy::Dynamic` + `Tool::needs_approval` 现在就通，`exec_command` 可以自己实现前缀白名单，且派发顺序保证它只在没有固定裁决时才被问到。<br>**给 R6-3 留的一条**：`PermissionDecision` 这个名字已经被用作**规则词表**（三值、无字段、可进配置），R6-1 原文说它归 R6-3，实际提前落在这里。R6-3 原文要在同名类型上挂 payload（`Allow{updated_input, updated_permissions}` / `Deny{message, interrupt}`），那是改公开面加改线格式（`"allow"` 变成 `{"allow":{...}}`），`#[non_exhaustive]` 对带字段的变体救不了。**宿主答复应当是另一个类型**：规则说什么（可序列化、进配置）和宿主答什么（改写入参、追加规则、中断整个 run）本来就是两件事，claude 把它们合成一个是它的历史包袱——core 概念照 upstream，形状不连线上类型一起照搬。<br>**原始约束（保留）**：`PermissionRule{tool, matcher, behavior}` + `PermissionUpdate{destination: Session\|LocalSettings\|ProjectSettings\|UserSettings}`，可落盘可撤销。**叠加 Codex 的两个减打断机制**：`prefix_rule(pattern, decision=allow)` 命令前缀白名单（`exec_command` 参数里就有这个字段）+ 按目录的 `trust_level` 分级——高频可信命令免重复审批，安全边界不变 |
| R6-3 | 审批决策类型 | **DONE** | **落地形态**：`ra-core::permission` 新增 `ToolApprovalDecision::{Allow(ToolApprovalAllow), Deny(ToolApprovalDeny)}`、`PermissionUpdate`（`AddRules` / `ReplaceRules` / `RemoveRules` / `SetMode`）与 `PermissionUpdateDestination`（`Session` / `LocalSettings` / `ProjectSettings` / `UserSettings`）。`ToolApprovalHandler::decide(&ToolApproval, &RunContext)` 是唯一的异步宿主答复契约：输入携带原始待审批记录和 live `RunContext`，输出是 core 值类型，UI 只呈现与采集，不能由文案反推语义。`PermissionDecision` 保持 R6-2 的三值、可持久化规则词表，绝不混入一次性审批 payload，避免把既有 `"allow"` 线格式改成对象。更新与决策均使用带标签的严格 serde，未知字段显式拒绝；更新目标必须显式给出，core 不可安全猜测批准应只留在 session 还是写进用户配置。<br>**四个刻意的形状**：① **两个分支各自持有 payload 结构体**（私有字段 + builder），而不是一个枚举挂两组具名字段——初版把 `with_updated_permissions` / `with_interrupt` 挂在枚举上，`deny(..).with_updated_permissions(..)` 编译通过且**静默丢掉**那条规则更新（实测 `updated_permissions().len() == 0`、线上只剩 `{"behavior":"deny","message":..}`），丢的正好是用户点"不要再问了"时唯一要落的东西；现在它是编译错误。**因此拒绝分支目前带不了规则更新**：R6-7 的 `state.reject(item, always)` 要落 deny 规则时，得往 `ToolApprovalDeny` 上加字段，而不是去复用批准分支的——这也是这两个结构体做成私有字段 + `#[non_exhaustive]` 的原因，加字段不破坏下游。② **`PermissionUpdate` 四个变体逐个 `#[non_exhaustive]` + 四个构造函数**：枚举的具名字段变体可以被下游字面构造，加一个限定词（规则作用域、有效期）就是破坏性变更；而 `xtask` 的扩展安全规则②只 syntax-check `Item::Struct`，看不见枚举变体的字段，机器过了不等于规则意图满足。③ **`updated_input` 用私有的 `UpdatedToolInput{Original, Replacement(Value)}` 而不是 `Option<Value>`**：`Option<Value>` 下"字段缺失"与"显式 `null`"都读回 `None`，`Some(Null)` 往返后会变成另一个值（初版实测 `equal=false`）。**方向是保住改写而不是抹掉它**——宿主要求换掉入参却被静默还原成原始参数，等于拿没人审过的 arguments 去执行；所以显式 `null` 和任何 JSON 值一样是一次改写，缺字段才是"保持原样"，公开面仍是 `Option<&Value>`，代价只有一个私有枚举加一对 ser/de 函数。④ **`decide` 的 `Err` 不是答复**：文档写死它既不能当批准，也不能被运行时悄悄改写成模型可见的拒绝——模型看到"被拒"会换条路走，而宿主还以为自己被问过；要拒绝就返回 `deny`。另加 `label()` / `Display`（渲染 `behavior` 标签，不渲染拒绝文案），沿用本模块"一个值只有一种拼写"的纪律。<br>**线格式大小写的分界规则写进了模块文档**：字段名是本框架自己的，一律 snake_case（`updated_input` / `tool_name`）；**复刻既有公开权限词表的标签值**保持上游逐字拼写（`acceptEdits` / `localSettings` / `addRules`）——那些值是宿主配置里已经存在的字符串，重拼等于静默拒掉用户写了很久的策略。所以一份 payload 里两种大小写并存是规则而不是疏忽。另一条路（整体 camelCase 跟上游的 `updatedInput`）同样自洽，但要重拼 R6-2 已发布的 `PermissionRule` 字段，换来的一致性只有人看得出、程序看不出。<br>**与 claude 的一处有意偏差**：上游允许省略 `destination` 并由进程推断；本框架要求必填，因为不同目标的权限生命周期与作用域不同，静默默认会扩大批准范围。参数级 matcher 继续不进入 core，更新复用 R6-2 的 `PermissionRule`（工具名 + namespace）；命令前缀与目录信任分别留给 R6-8 和 R6-10。<br>**验收**：`tests/it-core/tests/permission_mode.rs` 新增 4 条——① allow 输入改写 + 规则更新的 JSON 往返、deny 的 message/interrupt 往返、`label` / `Display`；② 四类更新经构造函数建立、`destination()` 取回、往返，缺 `destination` 与多出限定词字段两种都被拒；③ handler **把读到的 `RunContext` 与待审批记录经返回值送出来再断言**（断言写在回调里，回调没跑时会静默全过）；④ 线格式边界：`{"behavior":"allow"}` 最小形态双向、`interrupt` 默认不上线、`null` 输入是一次真实改写且与"缺字段"可区分并往返、allow/deny 两侧未知字段与 `updated_permissions` 混入拒绝分支均被拒。决策应用、暂停/恢复与落盘仍分别归 R6-5、R6-6、R6-7。 |
| R6-4 | 审批上下文 UI 字段 | **DONE** | `ra_core::permission::ToolPermissionContext` 已提供 `suggestions`、`tool_use_id`、`agent_id`、`blocked_path`、`decision_reason`、`title`、`display_name`、`description` 八个私有字段及只读访问器；`tool_use_id` 只能由 `for_approval(&ToolApproval)` 从 `call_id` 派生，并可由 `matches_approval` 在恢复前校验，`agent_id` 则只表示可选的子 agent 身份——它与 `RunContext::agent_id()` 是两件事，后者是**当前发言的公开 agent**，handoff 会把它换掉，不是全程稳定的 root 身份。它保留强类型 `PermissionUpdate` 建议和未知 UI 字段，UI 可直接渲染而不从工具名或 JSON 参数猜文案。`ToolApprovalHandler::decide` 以待审批记录、上下文与 live `RunContext` 为唯一回调契约；实际暂停、持久化与恢复仍归 R6-5 / R6-6 / R6-7。<br>**未知字段策略跟 `ToolApproval` 对齐**：`#[serde(flatten)] Unknown` 保留并原样回写，不用 `deny_unknown_fields`——这条记录会随待审批项进 `RunState`，旧 runtime 读到新版渲染提示必须能原样写回去，否则 R6-6 冻结 schema v1 之后就是整份 checkpoint 加载失败。**`tool_use_id` 是必填 serde 字段**（不带 `#[serde(default)]`，与 R6-6a 的 `run_id` 同一条口径）：缺字段、显式 null、重复键一律拒绝，不补一个匹配不到任何调用的空 `CallId`。<br>**验收落点**：`tests/it-core/tests/permission_mode.rs` 的 `test_tool_approval_handler_08`（回调三参数齐全）、`test_tool_permission_context_10`（八字段往返 + `matches_approval` 正反例 + 未知字段回写）、`test_tool_permission_context_11`（必填 ID 三种拒绝 + 仅带 ID 可加载，已做 mutation 验证：给 `tool_use_id` 加 `#[serde(default = ...)]` 必失败）。<br>**本条刻意未做**：`with_suggestions` 是覆盖语义而兄弟方法 `ToolApprovalAllow::with_updated_permissions` 是追加语义、`blocked_path` 用 `String` 而非 `PathBuf`、`title`/`display_name`/`description` 没有来源标记（第三方 MCP 工具描述与宿主自撰文案不可区分）——三条都已记录，等 R6-5 有真实生产者后再定。 |
| R6-5 | `NextStep::Interruption` 落地 | **DONE** | `ToolDispatch::AwaitingApproval` 生成带稳定 `call_id.approval` 身份的 `RunItemKind::ToolApproval`，同时进入 `TurnExecution`、`NextStep::Interruption` 和 `SingleStepResult::session_step_items`；`SingleStepResult` 的闸门保证待决项已存入 session、均为可审批项且不遗漏响应自身的待决项。runner 将它映射成 `RunOutcome::Interrupted` 并结束本段，不等待宿主回调，也不把它写成 `FinishReason` 或最终消息。<br>**待决项的归属在结算内部完成，不由 runner 事后回查**：`resolve_next_step` 必须早于 `step_items`（R3-10 的输出通道规则要读决策，决策因此读不到记录），而归属发生在 `step_items` 里，所以决策携带的是尚未归属的副本——同一轮于是握着同一个待决项的两份不等副本，存下来那份记着生产者、递给宿主那份不记。`rebind_interruption` 在 `step_items` 之后、`build()` 之前把决策重指向本轮存下的记录，`SingleStepResult` 因此在任何人读它之前就是自洽的：turn record、以及后续 R6-6 的 checkpoint 与 R6-7 的 resume 拿到的都是同一份带 provenance 的权威记录，而不是各自去回查一次。放在 runner 里只能修好一个消费者，其余只是「传递」决策的地方会继续携带落败的那份。<br>**保留提问顺序而非记录顺序**：两者今天一致，但承诺给宿主的是它被问的次序——托管审批按模型产出序，其后才是执行中才发现需要审批的调用——按记录位置重推会在两者分叉那天悄悄改变问题编号。<br>**「记录缺失」分支按构造不可达**：每条待决项要么是 `processed` 自己的项，要么是批次同时压进 `execution.new_items` 的审批项，而 `step_items` 是这两者的并集；仍然返回 `Result` 而不 unwrap，是因为那条包含关系是另一个模块的不变量，panic 在 runner 里报出来离成因更远，不如由结算点名它弄丢的那一项。流式入口随后发出 `Finished(Interrupted)`。批准、拒绝、状态落盘和从中断点继续仍分别归 R6-7 与 R6-6。<br>**验收**：`tests/it-runtime/tests/turn_settlement.rs` 覆盖本地审批项的生成和顺序，其中「托管 MCP 审批 + 工具审批」那条同时走过 `rebind_interruption` 的多项查找并断言提问顺序；`tests/it-runtime/tests/runner_loop.rs` 覆盖非流式的停止语义、tool-stop 不越过审批，以及流式路径发出 `ToolApproval` 记录后立即以 `Interrupted` 结束、没有第二次模型调用或工具执行，并断言结果中的待决项与流事件相同、**且直接断言其 provenance 指向公开 agent**——只断言两份相等的话，归属整体消失时两份会一起错而测试照样绿；已把 `rebind_interruption` 临时改成空转反证该断言确实拦得住（`left: None, right: Some("coder")`）。<br>**留了一格没做**：多条待决项从 `RunOutcome::Interrupted` 那一侧再看一遍。多项查找与排序本身已由上面那条结算级用例覆盖，缺的只是 runner 出口的视角。 |
| R6-6 | `RunState` 序列化 | **DONE**（列出的字段落地六项，另五项与两条对齐项刻意未做，归属见末段）| **落地形态**：在 R3-13 那个 `#[non_exhaustive]` 结构上加字段，没有新起容器。新增 `starting_agent` / `current_agent` / `original_input` / `generated_items` / `model_responses` / `pending_interruptions` 六格，`RUN_STATE_SCHEMA_VERSION_SUMMARIES` 开出「版本号旁边写清这版改了什么」的表——一次 bump 而没有摘要，等于让后来的人无从判断旧运行时能不能安全续跑这份 checkpoint。<br>**待决中断存 ID 不存副本**：每条待决项本来就在 `generated_items` 里，checkpoint 同时存两份就得让两份保持一致；哪天 session 给权威那份补上 `session_data` 或 provenance，靠整体相等去配对的续跑就会因为一个不改变问题内容的差异拒绝继续。`pending_interruption_items()` 负责按 ID 回解，`RunOutcome::Interrupted` 仍然把记录本身递给宿主。<br>**反序列化改成会拒绝，而不是强行凑合**（`from` → `try_from`，错误类型跟 `prompt.rs` / `tool/profile.rs` 一样是 `Error`）：checkpoint 是这套框架唯一从「写它的那个进程之外」进来的输入——磁盘、旧版本、编辑器——所以运行时事后依赖的每条不变量都在这里检查而不是假定。拦住的两种形状都是静默失败：**有历史却没有 `current_agent`** 会被 `begin_segment` 当成第一段，用续跑请求带的 input 覆盖 `original_input` 而保留按旧 input 生成的记录，拼出一份事后无从追溯的 transcript；**待决 ID 指向不存在或指向普通消息**，前者让这个 run 永远续不下去，后者让它一直等一个没人问过的问题的答案。<br>**续跑基线由请求选，且第一档是单向门**：带 input 的请求保持调用方自管续跑——这条路径是宿主追加一轮新用户消息的唯一去处；空 input 则请运行时按 checkpoint 自己的历史投影。调用方给的 input 是**基线不是记录**，checkpoint 学不到它捎带的那一轮，所以 `input_history_is_complete` 记下这件事，之后再要求投影会被拒——两档混用而不报错的后果正是「悄悄少一轮」。`RunResult::original_input` 的语义随之从「run 的开场输入」收敛成「本段的续跑基线」，`original_input + new_items` 因此在两档下都拼得出下一次输入，第三段不会丢掉第一段。<br>**loop 不再在 checkpoint 旁边留第二份历史**：`TurnLoopProgress` 只存本段在 run 历史里的起点下标，段级视图靠开窗读回，于是 `RunResult` 报本段、`RunState` 报整个 run，两者不可能对不上。`RunErrorData` 拿到的基线与段窗口就是模型看到的那一对。<br>**验收**：`tests/it-core/tests/run_state.rs` 27 条（新增七条：schema 摘要表必须覆盖当前版本且严格升序；带历史的 checkpoint 往返；待决项的线形态必须是纯 ID 数组；指向不存在 / 指向普通消息两种待决项在 setter 与反序列化两侧都被拒；有历史无 agent 被拒（删 `current_agent` 和删 `starting_agent` 两个方向各验一次）；带自备 input 的续跑被允许且把 `input_history_is_complete` 落成 false、重新载入后仍然拒绝投影；未答中断挡住下一段、清空后放行）。`tests/it-runtime/tests/runner_loop.rs` 65 条（续跑用例延长到三段，断言第三段的输入等于 `second.continuation_input(PreserveAll)`——这一段才是「从段而不是从 run 投影」会露馅的地方；自备基线续跑用例断言追加的新用户消息真的到达 provider；审批中断用例断言 checkpoint 存的是 ID 且能回解到权威记录、未答就续跑会报错且零模型调用）。另有一条 `requires_eq::<RunState>()` 把 `Eq` 这个公开契约钉住。<br>**本条刻意未做（归属待定，别当成漏了）**：任务描述里列出的 `session_items`、`last_processed_response`、guardrail 结果、工具 `lookup_key`、sandbox 会话引用五格，以及「对齐 `RunState` 的 context serializer/deserializer」与「strict context 模式」两条对齐项，都没有实现。guardrail 结果的类型全树还不存在（等 R7）；session 历史的权威落点在 R9；其余三格归哪一条**尚未定**，需要在动它们之前先把归属写进本表，否则下次读这行的人会以为是漏做。 |
| R6-6a | **`RunState` 扩展位预留与 identity slice** | **DONE**（identity slice 6 条验收全过；依赖日志的 2 条归 R9-0a；七个占位槽的**行为**仍按原里程碑展开） | **落地形态**：九类字段全部就位（**本条锁定的是这九类；R5-4 之后 `RunState` 上还多了第十个字段 `tool_output_references`，那次正是本条预告的「加字段就是一次迁移」——schema 2→3、旧记录用 `Option` + `TryFrom` 兜，详见 R5-4 那一行**）。`run_id: RunId` 与 `next_host_event_seq: u64` 是**必填 serde 字段**（不带 `#[serde(default)]`），缺任一的 checkpoint 直接反序列化失败而不是补一个；其余七个（`finish_reason`、`nested_runs`、`workspace_lease`、`work_state_ref`、`graph_cursor`、`usage_totals`、`pending_control_requests`）是带 `#[serde(default)]` 的占位槽，只有值类型与读写访问器，**行为归 R3-1b / R12-2·R12-3 / R8-11a·R12-5 / R17-1 / R17-5 / R3-8·R1-8 / R6**。入口是 `RunState::start(run_id)`，`new()` 与 `Default` 都已删除。`RunRequest::with_state` **接管运行身份**（用 state 的 `run_id` 覆盖 `new()` 收到的那个），因为分别传入的两个 id 一旦不一致，已落盘事件用的是哪个就说不清了。<br>**序号口径**：`EventSeqAllocator` 持 `Arc<AtomicU64>`，`next_host_event_seq` 是**已分配号的排他上界（下一候选号）**；恢复取 `max(checkpoint_next, persisted_run_max_seq + 1)`。四道门都用 API 形状焊死而不是靠约定——① `allocate()` 到 `u64::MAX` 报错不回绕，且**只在这一处**报耗尽（所以 `restore` 改成饱和而不是第二次报错）；② allocator 只能由 `RunState::restore_event_seq_allocator` 造，`EventSeqAllocator::new` / `restore` 是 `pub(crate)`，`RunRequest` 也不给 setter——两个可独立替换的身份载体会让一次 run 对工具报一个 id、往 checkpoint 写另一个；③ `snapshot_event_seq` 取 `max` 且断言 allocator 属于本 run（debug 下 panic），赋值会让陈旧 allocator 把上界推回去，静默跳过则留下一份上界永不推进的 checkpoint；④ `with_next_host_event_seq` 只上调，`set_next_host_event_seq` 已删除。**候选号在可失败的写之前分配，所以取消与崩溃必然留洞——验收只要求无重复与 per-run 可排序，不要求无空洞**，跨 run 顺序不由 `(run_id, seq)` 推出（归 R9-0a 的 `timeline_seq`）。<br>**验收落点**：`tests/it-core/tests/run_state.rs`（17 条：`test_run_state_01..04` 保留原编号与断言；身份相等性、缺 `run_id` 拒绝、缺 `next_host_event_seq` 拒绝、九槽往返、8×250 并发分配无重复、耗尽拒绝两条、恢复三档、`restore_event_seq_allocator` 往返、上界防回退与跨 run 绑定、外来 allocator 触发 `debug_assert`、上界只增、pending request 经 context 只读）、`tests/it-runtime/tests/runner_loop.rs`（`resuming_with_state_carries_the_states_run_id_into_runner_and_context` 用 placeholder id 与 state id 故意不同来真正区分身份来源；`runner_projects_pending_control_requests_into_live_context_seen_by_tools`；`runner_automatically_snapshots_event_seq_allocator_advancement_into_run_state`；`runner_restores_event_seq_allocator_with_persisted_max_seq`）。以上不变量逐条做过 mutation 验证（改回旧行为必有测试失败）。<br>**本条刻意未做**：`usage_totals` 只是留槽，预算仍由 `BudgetSnapshot` 自行累加，「预算只读该投影、不存在第二个可独立递增的 counter」归 R3-8 / R1-8；跨 run replay 的 `timeline_seq` 与 usage 对账基线归 R9-0a。<br>**原始约束（保留）**：schema v1 一旦发布，加字段就是一次迁移。**第一版锁定九类字段/槽位**：可先以 `Option` / 空集合 + `#[serde(default)]` 占位的 `finish_reason`（R3-1b）、`nested_runs: Vec<NestedRunRef>`（R12-2/R12-3）、`workspace_lease: Option<WorkspaceLeaseRef>`（R8-11a/R12-5）、`work_state_ref`（R17-1）、`graph_cursor`（R17-5）、`usage_totals`（R3-8/R1-8）、`pending_control_requests`（R6）；以及**不能是 optional placeholder 的** `run_id: RunId`（R3-9a）和 `next_host_event_seq: u64`（R8-0）。**身份由构造点显式注入，不由 `new()` / `Default` 铸造**（2026-08-11 二审补）：入口是 `RunState::start(run_id: RunId)`。已落地的 `impl Default for RunState { fn default() -> Self { Self::new() } }`（`ra-core::state::run`）在 `new()` 里铸 id 会一次性带来三个后果——`Default` 变成有身份副作用的构造、`RunState::new() == RunState::new()` 从此为假（这个类型 derive 了 `PartialEq`）、别处字段上的 `#[serde(default)]` 会在反序列化时静默铸出一个新 id，最后一条正是本条自己禁止的「deserialize 时悄悄生成另一个 id」，只是从另一个门进来。落地时 `Default` 要么删除，要么明确它产出的是**尚未绑定身份**的状态且不可直接进 run。`uuid` 已是 `ra-core` 依赖，能力从来不是问题，位置才是。缺少 `run_id` 的输入一律拒绝——`RUN_STATE_SCHEMA_VERSION = 1` 至今只存在于仓内、没有对外写出过 checkpoint，**没有 legacy 要照顾**，不留那条不会有人验证的迁移分支。<br>**event seq 的口径**（2026-08-11 二审 + 三审定稿，原文「`next_host_event_seq` 或等价 sink 分配」是二选一，「high-water」「无空洞」也含糊，这里一次定死）：① `next_host_event_seq` 是**已分配序号的排他上界（下一候选号）**，不是含糊的 high-water mark；② 运行期由 run 持有的 `Arc<AtomicU64>::fetch_add(1)` 分配同一 `RunId` 下不重复、单调的候选号——`RunState` 上一个普通 `u64` 字段是 read-modify-write，而 R3-4b（已落地）的一批工具就是并发发射 `HostEvent` 的；③ checkpoint 存 atomic 当前值；④ 恢复以 `max(checkpoint_next, persisted_run_max_seq + 1)` 初始化，**日志尚不存在时（R9-0a 之前）退化为 `checkpoint_next`**；`persisted_run_max_seq` 由 R9-0a writer 按 run 维护（**2026-08-19 修正**：原计划让它落在 sidecar 里以避免 resume 全文件扫描，R9-0a 实测这条不成立——放在日志旁边的汇总值无法自证由该日志算出，详见 R9-0a ⑥。现在 sidecar 只是只写产物，`open()` 一律扫描重建；真正的快路径要等 rollout 内部的 checkpoint 记录，已拆为 R9-0b）；⑤ **候选号分配在可失败的写之前，所以取消、写入失败、崩溃都会留下洞**——验收只要求无重复与每 run 已持久化事件按 seq 可排序，**不得要求无空洞**（要求无洞等于要求分配与落盘是一个原子操作）；⑥ 序号空间是 **per-`RunId`** 单调，子 run 有自己的空间，跨 run 顺序不能由 `(run_id, seq)` 推出，见 R8-0 与 R9-0a 的 `timeline_seq`。<br>**最小 identity slice（`run_id` + event seq）在 R3-9a 后、R8-0 前先落地；其余字段的行为仍按原里程碑展开。** 这不是把 R6 全前移，只是让 R8 的可落盘 event 信封不依赖一个尚不存在的恢复身份。usage 的单一事实来源也定死：逐请求 `ModelResponse::usage` 是原始事实，`usage_totals` 仅由同一结算点原子更新、可从已持久化响应重建并与其校验，预算读取该投影；不提供第二个独立递增入口。**重建基线必须指名**（2026-08-11 二审补）：R5-4 的老结果淘汰与 R5-8 的裁剪都会动掉旧 `ModelResponse`，「totals 等于 `RunState` 里现存响应之和」压缩后必然不成立，而那时错的不是 totals。基线只能是 **R9-0a / R9-0 rollout 日志里永不裁剪的那份逐请求 usage**；`RunState` 内的响应集合是可裁剪的工作副本，不作为对账依据。这与 R5-3b「压缩 / resume / replay 不得破坏预算累计」是同一条不变量的两种写法，也是 R5 前置顺序门（R9-2a → R9-0a → R5-3/R5-4/R5-8）存在的理由。待审批/控制请求同样只由 `RunState` 持有，`RunContext` 只能读。未知字段策略保持：**保留并原样回写，不报错**——否则新版本写下的 state 在旧版本上会直接丢数据。<br>**验收分两批，本条只验它自己完成时跑得动的那批**：identity slice 阶段——新 run 必有稳定 id 且该 id 来自显式构造参数（`RunState::new()` / `Default` 不得铸 id，往返两次构造仍相等）；缺 `run_id` 的 checkpoint 被拒绝而不是补一个；resume 保持同一 `RunId`；一批并发工具分配的候选 seq **无重复**（允许洞）；恢复后新分配的 seq 严格大于该 run 的 `checkpoint_next` 与已持久化最大值，含「已发事件未落 checkpoint」后重启一例；pending control request 往返后仍可恢复且经 context 只读不可写。**依赖日志的两条归 R9-0a 验收**：同一 root thread 的跨 run replay 使用稳定 `timeline_seq`；`usage_totals` 等于 rollout 日志逐请求 usage 之和且压缩后仍成立。 |
| R6-7 | approve / reject 与续跑 | **DONE** | **落地形态**：`RunState::approve(item, always)` / `reject(item, always)` 把宿主答复存进 checkpoint 的 `pending_interruption_resolutions`，**但不清除 pending 项**；`Runner::run` 在下一次模型调用之前跑 `resolve_interrupted_turn`，执行工具或写入拒绝输出，**追加完 output 才**清 pending ID。点击与执行之间崩一次就丢掉这次调用，是把删除放在点击时的代价。审批记录存 `ToolLookupKey`（不是裸工具名），恢复时按原工具身份重建调用；`RunState` schema 升至 v2，加载时**把不高于当前版本的 checkpoint 向上补齐**，避免带 v2 字段的载荷挂着 v1 标签写回去，让只认 v1 的 peer 静默丢掉答复。<br>**四个刻意的形状**：① **`always` 规则钉死在具体可执行体上**。`PermissionRule` 新增可选 `lookup_key` 与 `matches_origin`：写在配置里的名字规则保持 R6-2 的「缺 namespace = 匹配所有 namespace」语义（那是给人写策略用的），但一次点击生成的会话规则不能继承这个通配 —— 否则批准裸 `write_file` 会连带放行某个 MCP server 的同名工具，那是用户从没见过的东西。`matches`（只按名字）对已钉死的规则一律返回 false，防止它被悄悄放宽回名字规则。② **重新作答要撤销旧规则**。`answer_interruption` 先撤掉指向同一动作的旧规则再决定是否写新的：先点「始终允许」再改成普通拒绝，只追加不撤销会把用户已经收回的授权永久留在 `permission_rules` 里，而且没有任何 API 能删掉它。③ **恢复不是越权的通行证**。`approval_granted` 只让固定裁决里的 `Ask` 塌缩成 `Allow` —— 那正是宿主刚回答过的那个问题；deny 规则、`Plan` 档、`DontAsk` 是从来没被拿去点击的策略，照旧拒绝。顺序和结论同样重要：**先算固定裁决再兑现答复**，才是那些拒绝还够得着的原因。④ **拒绝与被拒同形**。宿主拒绝按 `ToolOutcome::refused` 入账，和权限阶段拒掉的调用一样 —— 两者都是「答复了但没跑工具」。不记的话，早先累积的失败 streak 会跨过一个根本没执行的调用留下来，下一次真实尝试就要拿这次没产生的证据去受审。<br>**恢复出来的调用要和正常轮次同形**：真实 streak 从 `ToolUseTracker` / `ToolFailureTracker` 读出交给熔断器（attempt 在被中断那一轮已记过，所以这里只补 outcome，两级幂等保证不重复计数），outcome 整批记一次而不是每个答案记一次（`record_turn` 用整轮指纹识别重复结算，拆成多次一元素调用会让那道防线描述一个再也不会被结算的轮次），output item 继承审批记录的 provenance，`RunContext` 补上 `pending_control_requests`。<br>**一处顺带的收窄**：`ToolApproval` 只存 lookup key，不再存 `qualified_name` —— 后者是前者的纯函数（`ToolOrigin::from_lookup_key` 推导、`validate` 强制相等），第二份副本只可能冗余或错误，拿两者互校证明不了任何读者不知道的事。它和把 key 装箱一起，避免一个只有审批记录才带的路由身份撑宽 session 里每一条 message 和 reasoning 记录（`RunItem` 曾因此从 416 涨到 488 字节，触发 `ra-session` 的 `large_enum_variant`）。<br>**验收**：`tests/it-core/tests/run_state.rs` 4 条（答复留存 + 精确 always 规则、同名异 namespace 不被覆盖、重新作答撤销旧规则、拒绝侧的 Deny 规则往返、无路由身份时 `approve` 当场拒绝而 `reject` 放行并退回名字规则）+ `tests/it-core/tests/permission_mode.rs` 1 条（钉死规则只匹配自己的可执行体，且 `matches` 不放宽它）+ `tests/it-runtime/tests/runner_loop.rs` 4 条（批准只跑一次并续跑、结算输出确实进入下一次模型请求、provenance 与 tracker 入账；拒绝零调用 + 错误输出；`Ask` 规则被答复满足而 `Deny` 规则仍拒绝；**被拒调用清掉它没参与制造的 streak** —— 这条已反证过，去掉记录后 `left: 2, right: 0`）。<br>**留了四格没做**（各自独立，不阻断）：run 后续失败时 state 随之丢失，重试会重跑非幂等工具（要真正的 at-most-once 得让 state 在错误路径上也能回到宿主）；拒绝输出自造了 `{"code":..}` 形状而没复用 `dispatch::failure_output` 的 `{"error":{...}}`；`PendingInterruptionResolution` 缺 `Unknown` 前向兼容槽，`InterruptionResolution` 也没有未知变体降级；`.output` item id 与 `batch::output_item_id` 重复了一份约定 |
| R6-8 | 危险动作检测 | **DONE** | **落地形态**：`ra_coding::dangerous_action::DangerousActionDetector` 绑定一个 canonical workspace root，输出按源序排列的 `DangerousActionReport`，只产生结构化事实、不自行批准、拒绝或解释用户意图。shell 路径先由 `tree-sitter-bash` 解析；检测器只遍历 `command` / `file_redirect` AST 节点，所以 `printf 'rm -rf /'` 中的文字是数据，不会被当成命令。已覆盖 `sudo` / `doas`、`mkfs*`、写 raw device 的 `dd`、进程/电源控制、`rm` / `rmdir` / `unlink`、破坏性的 `git clean` / `git reset --hard`；`sudo rm ...` 这类语法 wrapper 会同时显露 wrapper 与被包裹的真实程序。解析残缺时给出 `UnparseableShell`，不把无法结构化检查的命令报告为安全。<br>**参数要当参数读，不能用前缀判断糊过去**：`CommandWrapper` 表为每个 wrapper 声明「哪些选项吃掉下一个参数」与「被包装程序前有几个操作数」，wrapper 还会**反复**解包，于是 `timeout 5 rm -rf /`、`nice -n 10 rm -rf /`、`sudo -u root rm -rf /`、`stdbuf -o 0 rm -rf /`、`xargs rm -rf /`、`sudo timeout 5 rm -rf /` 都能走到真正的 `rm`。**表里漏一个带值选项，它的值就会被当成程序名，整条命令消失**——这条写进了类型文档。同理 `has_short_flag` 扫短选项簇里的字符（`git clean -fdx`），`git_subcommand` 跳过子命令前的全局选项（`git -C . reset --hard`）；`sh -c "..."` 的脚本操作数会递归解析（上限 3 层），读不出来或超限时给 `UnparseableShell`。<br>**安全输入必须静默**：`> /dev/null` 等 stream device、`>>` 追加、`tee --append`、无 `-f` 的 `ln`、`cp -n`、`git clean -n` 都不产生 finding。检测器喂的是审批提示，它对安全输入的沉默和对危险输入的响亮同等重要，因此验收里有一条专门断言「普通命令零 finding」。<br>**路径与影响面**：`>` / `>\|` / `&>` 重定向以及 `cp` / `mv` / `install` / `ln`、非 append 的 `tee`、`dd of=` 的静态输出目标会检查既有文件覆盖与越界；路径**逐组件解析、每个已存在的组件当场 canonicalize**，因此 workspace 内已有 symlink 指向外部时，写入不存在的子路径会报 `WriteOutsideWorkspace`，而 `linked-outside/../x` 里的 `..` 是从链接目标往上爬、不是从词法父目录爬（这也是它与 `RootedFileSystem` 的 cap-std 走法不再分歧的地方）。`rm` 目标含 glob、**读不出来的 expansion**、命中 workspace root、命中系统 root、或达到可配置阈值（默认 3 个）时为 `BroadDeletion`——读不出来的目标是影响面最不受限的那种，不能因为拿不到路径就丢掉；`find ... -delete`、递归 `git clean` 与 patch 中达到该阈值的 `DeleteFile` 同样入账。V4A plan 另检查 Add / **Update** / Move 目标覆盖及全部 action 的边界——`UpdateFile` 按定义就作用在既有文件上，它就是一次覆盖。<br>**工作目录是一等状态,因为语法树不是执行轨迹**：新增内部 `ShellCwd` 候选集与公开变体 `DangerousAction::UnknownWorkingDirectory`。`execution_reach` 按**白名单**走祖先链——只有顶层语句、`&&`/`\|\|` 列表的首个操作数、`redirected_statement` 的 body 算「必然执行」，循环体算「次数未知」，其余（分支体、pipeline、subshell、函数体、不认识的构造）一律算「可能执行」。必然执行且目标是已存在目录才替换 CWD；可能执行则新旧目录**并存**，任一候选越界就报。**`cd` 失败不等于 `cd` 未知**：`cd nowhere` 与 `cd 某文件` 都让 shell 原地不动，所以原 CWD 必须留着——丢掉它会让 `cd nowhere; touch ../out` 这条真实越界完全无声，而那恰恰是 shell 行为最确定的一种。目标存在但不是目录 → 原样保留 CWD 且零 finding；目标不存在 → 可能被脚本前面的 `mkdir` 创建，两种读法并存。只有 `popd`、`cd -`、动态操作数、循环体才真的报 `UnknownWorkingDirectory` 并停止解析相对路径。<br>**遍历必须迭代**：树深只受输入长度约束，递归遍历会把一段构造出来的深嵌套变成**进程 abort 而不是可捕获错误**，因此 `walk_shell_tree` 与 `contains_dynamic_shell_syntax` 都是带层级计数的 `TreeCursor` 走法，验收里有一条 5 万层嵌套的回归。<br>**边界**：这是 coding 产品语义，不能塞进 provider-neutral core；它不是 sandbox，symlink/路径检查与真正使用间的竞态仍由 R8 沙箱负责。没有从用户文本猜“是否授权”，也尚未把 `allow` / `soft_deny` / `hard_deny` 混入发现结果；R6-8b 消费这些事实决定自动审批，R6-7 决定中断后的恢复。<br>**验收**：`tests/it-coding/tests/dangerous_action.rs` 18 条——AST 对 quoted data 的区分、既有文件覆盖、`..` 与 symlink 越界、symlink 后的 `..` 不回到工作区、解析失败 fail-closed、**普通命令零 finding**、`dd of=` 自解析路径、wrapper 选项值不被当成程序、`sh -c` 嵌套解析与不可读时的降级、git 全局选项与短选项簇、读不出的删除目标算 broad、条件 `cd` 保留双读法、**失败的 `cd` 保留原目录**、5 万层嵌套不爆栈、阈值可配置、patch 的 Update 覆盖与大范围删除。 |
| R6-8b | **四级自动审批分类** | TODO | 照 CC 的 Auto Mode Classifier 分四级，**关键判据是"用户意图能否解除"**：`allow`（自动批准）/ `soft_deny`（破坏性、不可逆；**除非有明确用户意图授权，否则拦**）/ `hard_deny`（安全边界；**用户意图也解除不了，无条件拦**）/ `environment`（用户环境上下文，不是动作，只喂给判定器）。`soft_deny` 与 `hard_deny` 的分界不是"多危险"，而是**用户说了能不能算数**——这条区分比四个标签本身更重要。CC 用 LLM 分类器判定；**rusty-agent 先用确定性规则（命令 AST + 路径 + 影响面）覆盖 `hard_deny` 与 `allow` 两端，中间灰区才可选调用分类器**，避免把每次审批都变成一次模型往返 |
| R6-9 | 配置矛盾预警 | TODO | 对齐 claude 的 `CanUseToolShadowedWarning`：`allowed_tools` 整工具放行或 `BypassPermissions` 时，审批回调永不触发 → 连接时主动告警 |
| R6-10 | 沙箱与审批的边界声明 | TODO | 借鉴 Codex：把沙箱模式与审批策略作为**独立的高权限提示通道**（`developer` 角色或等价物）声明给模型，明写"越界命令会被拒绝" |

### R6 非目标

| 项目 | 处理 |
| --- | --- |
| 用自然语言判断"用户是否授权" | 不做；授权只来自结构化 `PermissionRule` 与显式审批 |
| 把审批做成阻塞 await | 不做；审批是可序列化中断，支持跨进程/跨会话恢复 |

### R6 验收标准

| 能力 | 标准 |
| --- | --- |
| 可中断可恢复 | 进程 A 产生中断 → 存盘 → 进程 B 加载 → approve → 继续完成任务 |
| 版本兼容 | 旧 schema 快照能加载或给出明确的不兼容说明 |
| 决策完整 | Allow 改写入参、Deny 携带中断，两条路径都有测试 |

---

## R7 护栏与 hook 扩展点

> **定位**：rusty-agent 优先复用 `openai-agents-python` 的通用契约与 `codex` 的成熟工具、执行和产品机制。新增抽象必须有实际产品需求或 Rust 实现约束，并记录上游无法满足的缺口与偏离理由。R7 只含宿主可装的输入/输出护栏、工具两端护栏与 hook 扩展点；不为预设的行为纪律另建全局治理体系。
>
> **框架自身不带纪律。** 早先的版本在这里长出过一套内建 guard 家族（RuntimeGuard 三态、guard 预算与登记表、内建纪律与用户 hook 的分离、read-before-edit / 后台收尾 / 改后验证三条纪律），2026-09-09 整体撤销并 reset 到 `f4593f5`。撤销理由是**前提从未成立**：codex 作为目前最好的编码 agent 产品，6.6 KB 系统提示里关于验证只有一句软话（`core/gpt_5_codex_prompt.md:43`），一条纪律 guard 都没有；`openai-agents-python` 更没有。而当时登记表三行的实证栏写的是「尚无真实 run 数据，而这一行正是取得数据的前提」——自证循环。代码和测试写完不能反过来证明需求成立。
>
> **交付前请求续跑由可选 hook 提供。** Codex 的 Stop hook 在携带有效续跑提示时可阻断交付，循环将提示写入历史后继续运行。`stop_hook_active` 会传给 hook，表示当前已处于 Stop hook 引起的续跑中；它本身不保证第二次阻断被忽略。缺少有效续跑提示的阻断会发 Warning 并忽略。参考 `core/src/session/turn.rs` 的 `run_turn_stop_hooks` 调用路径与 `hooks/src/events/stop.rs`。若产品需要额外的一次上限，应作为显式策略并记录偏离，不能称为 Codex 的默认契约。

### R7 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R7-1 | `Guardrail` 输入/输出两层 | **DONE** | 对齐 `openai-agents-python` 的 `guardrail.py`：输入护栏支持上游的并行默认值与串行配置，tripwire 中止运行；输出护栏检查最终输出。结论的持久化及恢复复用按 SDK 运行状态契约核对。<br>**身份表示待按契约确定**：上游使用 `get_name()` 返回字符串，不预先要求 guardrail、tool guardrail 与 hook 共用受限语法或全局唯一性。若实际持久化或归因需求需要 newtype，可考虑 `CheckId`，但须说明作用域与必要性，并保留上游允许的名称语义；该名称不是本任务的既定公共 API。 <br>**落地形态**：契约在 `ra-core::guardrail`（`GuardrailFunctionOutput` / 两个结果类型 / `GuardrailFinalOutput` / `GuardrailEvidence`），派发在 `ra-runtime::guardrail`，接线在 `runner`。身份用 `name() -> &str`，**未引入 newtype**，也不要求唯一——同名两个都跑、都记。<br>**四处偏离都写在模块文档里**：① 结果存名字而非活对象（持久化所迫，不赋予名字身份）；② 拒绝携带 `GuardrailEvidence`（对齐上游异常的 `guardrail_result` + `run_data`，因为被拒的 run 没有 result 可读，证据只记进 state 等于宿主拿不到）；③ 输入阶段的阻塞半场与 turns 共用同一条 wall-clock 翻译，输出阶段**刻意不共用**（那里答案已存在且未被检查，翻译成完成结果等于把未检查的输出交出去）；④ 恢复用显式标记而非比对判决（名字可重复、被取消的检查没有判决），标记在**阻塞半场通过后**置位——阻塞检查被 deadline 打断时什么都没发出去，续跑重试拿得回保证；赛跑检查没返回则不重试，它的模型调用已经发生。 |
| R7-3 | 工具入参 / 结果护栏 | **DONE** | 对齐 `tool_guardrails.py`：三态 `allow` / `reject_content{message}` / `raise_exception` 与上游逐字对应。入参拒绝时工具一次不跑、出参拒绝只换模型那份视图。`pre_approval_tool_input_guardrails` 可在审批前预检，但批准后执行前**必须**再检查一次。宿主装、按名字挂在工具上、不占任何预算——预算这个概念随 guard 家族一起撤销了。 <br>**落地形态**：契约在 `ra-core::guardrail::tool`，派发在 `ra-runtime::tool::guardrail`，安装表与 preflight 在 `runner`。身份用基线上已有的 `ToolGuardrailId`（注册表引用，与运行级的展示名称是两回事，不统一）。四个安装入口是 `with_tool_input_guardrail(s)` / `with_tool_output_guardrail(s)`，开关是 `with_pre_approval_tool_input_guardrails`。<br>**链序**：护栏解析排在解码与权限之前，它回答的是配置问题不是这次调用的问题——没人装的声明要在宿主被打断之前、也在解码失败能走到 `handle_failure` 之前就把调用挡下来；preflight 在能力装配之后、首个模型调用之前扫一遍全部工具。<br>**四处偏离都写在模块文档里**：① 同名两个安装直接拒（跟运行级护栏相反——那边名字只是展示名，同名两个都跑；这里 ID 是查找键，装两个的话跑两遍等于悄悄加倍、挑一个等于行为取决于安装顺序）；② 入参拒绝记 `Refused`、出参拒绝记 `Observed`+failed（工具真跑了，记成 `Refused` 会给 no-progress 断路器送一份「这工具没执行过」的假证据）；③ 出参拒绝把模型那条记录整个换掉而不是留原文加投影——这个框架里历史就是下一次请求的输入，被拒内容留在记录里等于照样发给模型；④ `SingleStepResult` 只加工具侧两条列表，运行级两条一次性属于整个 run、由 loop 直接写 `RunState`，抄一份到 turn 上会让「承载它的那一轮」看起来像「引发它的那一轮」。<br>**归约按上游逐个短路**：`reduce_tool_*_guardrails` 在第一个非 allow 处返回，后面的检查根本不调用；早先那版「全跑完再按 raise 压过 reject 归约」与 `tool_guardrails.py` 不符，已撤回。<br>新增 trace 字段 `guardrail.behavior`——`guardrail.triggered` 分不出 reject 和 raise。<br>**验收**（`tests/it-runtime/tests/tool_guardrail.rs` 12 条）：allow 也连 `output_info` 进 `RunState` 并 JSON 往返；入参拒绝时工具零次执行且另一端根本不问；出参拒绝时工具跑过一次、只有模型读到的那句变了；raise 带 `GuardrailEvidence::ToolInput` 终止；检查器不可达时失败整轮而不是拒绝调用；未安装的声明与同名两个安装都在首个模型调用前停下（`model.calls() == 0`）；审批前预检不免除批准后再检查（关掉开关则只检查一次）；两个边界 × reject/raise/error 的顺序短路，安装顺序故意与声明顺序打乱。 |
| R7-4 | `UserHook` 事件面 | **DONE** | 事件集对齐 codex `hooks/src/events/`（session_start / session_end / user_prompt_submit / pre_tool_use / post_tool_use / permission_request / stop / compact / interrupt）与 `protocol/src/protocol.rs:1585` 的 subagent_start / subagent_stop。<br>**能拒的有四个**：`pre_tool_use`、`permission_request` 决定一次调用；`stop`、`subagent_stop` 决定一次交付——拦截必须携带续跑提示，`stop_hook_active` 向 hook 标明已进入阻断后的续跑，不能据此假定框架自动挡住第二次阻断，无提示的拦截发 Warning 并忽略（照 codex `core/src/session/turn.rs:566-588`）。<br>hook 是宿主扩展点：覆盖不了权限策略的拒绝，也不承载框架纪律——框架已经没有纪律可承载。<br>**落地形态**：契约在 `ra-core::hook`（`HookEventName` / `HookEvent` / `HookDecision` / `UserHook` / `UserHookDispatcher` / `UserHookContext` / `HookReport` / `HookRunStatus` / `ToolHookData` / `StopHookData` / `CompactTrigger`），派发在 `ra-runtime::hook`（`UserHooks` / `UserHookRegistration` / `BoundUserHooks`），接线分三处：`runner`（stop / subagent_start / subagent_stop / interrupt，并把同一个 dispatcher 交给 context processor）、`tool::dispatch`（pre_tool_use / permission_request / post_tool_use）、`ra-context::compaction`（两个 compact 事件）。安装入口 `RunConfig::with_user_hook(s)`，子 run 身份 `RunRequest::with_parent_run_id`。回调报告走既有宿主事件通道，新增 `HostEventBody::Hook(HookReport)`（family `"hook"`，`Unknown` 前向兼容照旧），**不进模型历史**。默认一个都不装。<br>**身份不做唯一约束**：`name()` 是展示名，同名多个都跑都记——与运行级护栏同一口径，跟工具护栏的 `ToolGuardrailId` 相反（那边 ID 是查找键）。查找键在这里是 `HookEventName`，注册即订阅。<br>**八处偏离都写在模块文档里**：① **codex 的一个 `compact` 拆成 `PreCompact` / `PostCompact` 两个事件**——post 携带 `Compaction`（summary 与被覆盖的记录 id），pre 只有 record_id 与 trigger；合成一个就得让 compaction 字段可空，于是每个 hook 都要先分辨自己在哪半场。两个都**只在真的发生压缩时发**：处理器先过自己的阈值，没过一个都不发，产出失败也没有 post。`CompactTrigger::Manual` 框架自己不产出，它是 `ContextProcessorRequest::notify_pre_compact` 这个公开入口留给「用户显式要求压缩」的宿主处理器的。② **`Allow` 只对 `permission_request` 有效**：`pre_tool_use` 只能拒不能批。策略判 `Deny` 时 `pre_tool_use` 根本不问——它唯一可能的效果就是拒，而答案已经是拒；判 `Ask` 时 `permission_request` 的 `Allow` 只解这一次待决审批，解不了策略级拒绝。③ **session / prompt 三个事件不由 runner 发**：一个 run 不是一个 session，恢复出来的 model-input 投影也不是一次新提交的 prompt；由会话属主拿 `UserHooks::bind` 得到的同一个 dispatcher 显式发。同理父 run 身份必须宿主显式给，不从 agent 名字猜。④ **续跑提示按普通 user 角色进历史、花既有 turn 预算**，不另开重试预算；`stop_hook_active` 只是如实告知，第二次阻断照样生效。⑤ **`Interrupt` 跑在独立的 cleanup scope 上**——run 自己的 scope 已经取消，挂在它下面的回调会在开始前就被 drop；用入场检查点的读视图（取消不产出 `RunResult`，不能为没结算的一轮编造账目），deadline 是同一条 `DRAIN_GRACE`，判决丢弃，通知不复活 run。⑥ **错误 / 超时 / 不支持的判决一律记录后忽略**（照 codex），只有取消向上传播——那不是 hook 失败，是整个操作停了。⑦ **同事件的回调并发跑**，聚合顺序是：任一 `Deny` 压过所有 `Allow`（取注册顺序第一条消息），多个 `Block` 按注册顺序空行拼接，都没有才 `Allow` / `Continue`。⑧ **超时默认照 codex**：`session_end` 1 秒，其余 600 秒，可逐条覆盖；run 自己的取消与 deadline 永远优先。<br>**归因码**：`hook.pre_tool_use` / `hook.permission_request`，与 `guardrail.tool_input` / `guardrail.tool_output` / `tool.permission_denied` 并列进 function span 的 `error.code`——被拒调用给模型看的形状是刻意一样的，谁拦的只能从这里读。<br>**验收**（`tests/it-runtime/tests/user_hook.rs` 15 条 + `runner_trace.rs` 1 条）：pre 拒绝时工具零次执行且审批那端根本不问；`Allow` 只解待决审批、策略拒绝赢、`DontAsk` 不被覆盖；同事件里拒绝压过同伴的批准；流式路径上 pre/post 各一次且 post 的判决被忽略；续跑进历史、连阻两次、`stop_hook_active` 序列 `[false,true,true]`、`RunState` JSON 往返后仍为真、`TurnRecord` 变成 `run_again`、两条续跑记录 id 互不相同；空提示阻断报 `Ignored` 带 warning 且原交付照常，宿主事件 JSON 往返一致；连阻不额外加预算（`max_turns` 照样停）；`stop_on_first_tool` 的交付也能续跑、且输出护栏只看真正交付的那份；被 Stop 阻断的候选保留历史，但不再作为交付：Stop、输出护栏与 RunResult 统一选择仍为 FinalOutput 的结算轮消息，续跑后工具结束或耗尽预算都不会重新交付旧候选，显式预算 closeout 优先；子 run 跨审批恢复不重复 announce、根 `Stop` 不被误用、`parent_run_id` 随检查点回来；失败与超时分别记 `Failed` / `TimedOut` 且都不授权；取消丢弃在跑的回调（记 `Cancelled`）、`Interrupt` 收到 `user_interrupt` 且它的 `Block` 被忽略；两个 compact 事件只在真尝试时发、共用同一条 record 身份、覆盖数为 4；宿主处理器发 `manual` 且拿不到控制效果；run 一遍 session/prompt 三个事件计数为 0，宿主自己 bind 后发这三个才收得到。 |

### R7 已撤销（2026-09-09）

保留决策痕迹，避免以后重新论证同一件事。撤销时的实现在分支 `r7-withdrawn-2026-09-09`（未推送）。

| 原编号 | 原任务 | 撤销理由 |
| --- | --- | --- |
| R7-0 | Guard 预算与登记表 | 它为约束 R7-2/5/6/7/8 而存在，被约束方撤销后自身没有对象 |
| R7-2 | `RuntimeGuard` 三态 + 提醒 | 两个参考都没有独立于护栏与 hook 的第四套拦截家族。产品要装策略走 R7-3 与 R6 权限链，对应 codex 的 `execpolicy` / `core-plugins/git_policy.rs` |
| R7-5 | 内建纪律与用户 hook 的分离 | 没有内建纪律可分 |
| R7-6 | read-before-edit 状态机 | codex 无对应机制，收益无实证 |
| R7-7 | 后台任务 closeout gate | 同上。**原语 `ra-tools::background_shell_wait` 仍要做**——它对应 codex 的 `unified_exec` + `write_stdin`，归 R8 |
| R7-8 | 改后验证提醒 | codex 只有一句软提示；`pre_final` 上一次误报花的是一次往返而不是一句话，误伤成本与收益不成比例 |
| R7-9 | 失败归因分流 | 记录退出状态有价值，归 R3-6c；据此强制熔断属于额外策略，默认值另行裁决 |
| R7-10 | 去词表化审计 lint | 两个参考都没有这条 lint；codex 用类型达到同样效果（`ToolName` 结构体 + `flat_tool_name` 单一出口 + 一条注释），不用 lint。降为评审原则，不占任务 |
| R7-11 | 不可信内容与敏感数据策略 | 整套 `ContentTrust` 三值枚举 + 全链路 label 传播是过度抽象：trust 与 secrecy 是两根正交的轴。codex 用 `AdditionalContextKind::{Untrusted, Application}` 共 35 行做到载荷部分——**label 就是 role**，压缩、summary、replay 因此免费保留。缩成「外部内容只能以 user / tool 角色落地」的构造路径不变量后归 `ra-prompt`，不属于 R7 |

### R7 非目标

| 项目 | 处理 |
| --- | --- |
| 框架自带任何 guard | **不做**。`openai-agents-python` 没有，codex 也没有。产品要装策略走 R7-3 / R6 / R7-4 三条既有路径 |
| 工具选择做硬 gate | 不做；对齐 codex 的软倾向表达（`prefer rg`、`without fuss`），不做 `maybe_defer_unverified_exploration` 这类硬门 |
| 用外部 hook 承载框架纪律 | 不适用——框架不再有纪律可承载 |
| 默认在交互流里挂重型 verifier | 不做；自治 / eval 场景才 opt-in |

### R7 验收标准

| 能力 | 标准 |
| --- | --- |
| 两层护栏 | **已达成（R7-1）**：`InputGuardrail` 默认与首个模型调用并跑、`run_in_parallel() == false` 的改为发出请求前先跑完；tripwire 抛 `Error::Guardrail` 并 cancel 该轮，**拒绝携带 `GuardrailEvidence`** 让宿主在没有 result 的情况下仍读得到判决与 `output_info`。`OutputGuardrail` 只在 loop 自己走到结论时问，且工具结果成为答案时也看得到实际内容。结论进 `RunState`、随检查点往返，续接由显式标记决定是否重跑 |
| 工具边界 | **已达成（R7-3）**：三态与 `tool_guardrails.py` 逐字对应，链条逐个短路、第一个非 allow 之后的检查不调用；入参拒绝时工具一次不跑（另一端也不问，它不是一次通过的检查）、出参拒绝只换模型那份视图；审批前预检不免除批准后的再检查；声明了没人装的检查、以及同名装了两个，都在首个模型调用前就让 run 停下；每条完成的检查连 `output_info` 进 `RunState` 并随检查点往返，raise 的证据改挂在 `Error` 上——被它终止的 run 没有 result 可读 |
| hook 面对齐 codex | **已达成（R7-4）**：事件集与 codex 一致，**唯一一处形状偏离是把 `compact` 拆成 `PreCompact` / `PostCompact`**，理由与其余七处一并记在 R7-4 行里；`stop` / `subagent_stop` 可携带有效提示请求续跑，提示按普通 user 角色进历史、花既有 turn 预算；`stop_hook_active` 如实传给 hook，不作为自动拒绝第二次阻断的上限（测试里连阻两次都生效）；无续跑提示的阻断记 `Ignored` 带 warning 并忽略，原交付照常。额外次数限制须作为显式产品策略 |
| 分层清楚 | **已达成（R7-4）**：hook 的 `Allow` 只解一次待决审批，策略判 `Deny` 的路径上 `pre_tool_use` 根本不问、`permission_request` 不被询问；归因码在 function span 的 `error.code` 上分得开——`hook.pre_tool_use` / `hook.permission_request` / `guardrail.tool_input` / `guardrail.tool_output` / `tool.permission_denied`，`runner_trace.rs` 有一条专门守这三类边界 |
| 无框架内建纪律 | **已达成**：`ra-runtime` 导出的是 hook 注册面（`ra_runtime::hook`），没有任何 guard 注册入口；`ra-coding` 一个 hook 一个 guard 都不装；仓库里没有 `guard-registry.md` 这类登记表 |

---

## R8 OpenAI Agents Python 执行环境移植

> **目标修正（2026-09-16）**：Rusty 是 `openai-agents-python` 的 Rust 移植，R8 首先复刻上游的执行环境、能力装配与运行生命周期；Codex 只作为局部实现和 coding 产品扩展的参考，不再与 OpenAI 竞争框架规格。默认值、字段含义、合并顺序、错误、所有权与恢复语义均以固定上游为准，不能以“更严格”或已有 Rusty 实现为理由默改。Rust 所有权、异步、类型系统或既有持久化兼容确需改变表达时，逐项记录理由及行为影响。
>
> **基线**：本地 `/Users/moses/workspace/custom-app/openai-agents-python`，commit `89c02c828ee8510fe9a84ee6675608193aa13b02`（`v0.22.0-70-g89c02c82`，2026-08-28）；定位使用 commit ＋文件＋符号名。本次已核对 session/client、run config、runtime ownership、capabilities、unix_local/Docker、manifest/snapshot 及相关测试目录；完整字段与测试映射是 R8-P0 的实现前交付，不把本次计划重排冒充完整源码移植审计。
>
> **状态口径**：旧 R8 的 DONE 只表示旧规格下已有实现，不表示上游对齐。旧编号不复用，新主线使用 `R8-P*`；所有旧编号在下表保留去向，历史正文及原测试记录完整归档到 [附录 C：R8 重排前记录](#r8-legacy-record)。本表主线目前均未完成；归档中的开发顺序与硬性要求不再生效。

### R8 契约边界与本次撤销的前提

- **三种 session 分开**：R9 的对话历史 `Session`、上游 `SandboxSession` 工作区执行环境、现有 Rusty 由 `ProcessManager` 持有的 exec 会话（对外只有 `ExecSessionId` 与 `ExecSessionState`，没有一个叫 `ExecSession` 的类型）覆盖单条命令／交互进程，三者不是同一个对象，不能复用身份、状态或清理责任。
- **先移植公共协议，再复用内部机制**：`BaseSandboxClient` / options、`BaseSandboxSession` / `SandboxSession` / state、`SandboxAgent` / `SandboxRunConfig` 定义对外行为；`ProcessManager`、capability 文件系统和 patch 算法是候选实现组件，不反向决定上游 API。Rust 可用组合替代继承，但保留职责、调用顺序和可替换能力。具体 crate 归属在 R8-P0 按现有依赖图确定，不先发明通用执行平台，也不向 `ra-core` 塞 Docker 参数。
- **所有权按上游而非按 run 猜测**：`runtime_session_manager.py::_create_resources` 区分外部 live session（`owns_session=False`）与 client 创建／恢复的 session；还维护按 agent 的资源和恢复身份。撤销“一个 run 必定一个容器”和“run 结束一律删除容器”。清理按上游 result/runtime 边界、所有权和失败顺序移植；不得将任意对象 drop 解释为有权删除调用方资源。
- **恢复属于移植主线**：上游已有的 PTY、state 序列化、resume、端口和 snapshot 不再整体划成不影响完成判定的远期增强。可以分阶段交付，但未实现必须列为缺口，不能宣称 Unix-local／Docker 完整对齐；重连工作区也不能冒充恢复在途命令。
- **额外限制不污染默认行为**：R8-7 的凭据过滤默认值、四条 rlimit，R8-8/9 的 `SandboxLevel` 梯子及全部 socket 禁止，旧 R8-10 的强制本地 bind mount、固定 image、Linux-only 首期、强制容器整体终止等，均不再作为 OpenAI 移植前提。需要的增强保留在显式选用的产品配置或适配器扩展中，另验收、另报告。既有使用这些策略的调用方不在本次文档修改中自动放宽，代码迁移须显式区分兼容配置和增强配置。
- **通用 lease 不阻塞移植**：撤销 R8-11a / R8-13 先完成才能实现 sandbox session 或 Docker 的依赖。上游自带的并发保护、所有权及清理必须移植；跨 run writer admission、git worktree、`RUSTY_AGENT_TMPDIR` 是 coding 扩展，需要时由产品装配。已有持久化字段的去留需兼容审查，不能直接删掉旧记录。
- **工具按能力装配**：基线 `Capabilities.default()` 是 `Filesystem`、`Shell`、`Compaction`；Filesystem 默认提供 `view_image` 与 `apply_patch`，Shell 根据 `supports_pty()` 决定是否装 `write_stdin`。Rusty 当前六工具 coding profile 不能代替上游默认 surface。命名相同也要核对参数与行为。

### R8 开发任务与执行顺序

以下源码路径均相对基线的 `src/agents/`，测试路径相对基线 `tests/`。每条实现时建立“上游测试 → Rust 测试 → 未覆盖原因”映射；Python-specific 的表示可适配，行为测试不能仅因难以移植而删除。

| 顺序／任务 | 交付范围与源码锚点 | 已有实现去向与验收 | 状态 |
| --- | --- | --- | --- |
| 0 / **R8-P0 契约与差异清单** | 枚举 `sandbox/__init__.py`、`sandbox/session/__init__.py` 的公共面及实际消费者；记录类型／方法／默认值／合并顺序／错误／序列化／生命周期，建立基线源码与测试映射。核对 `run_config.py::SandboxRunConfig`、`sandbox/sandbox_agent.py::SandboxAgent`、`sandbox/runtime*.py` 的运行集成；确定 Rust 模块与依赖落点。 | 对已有 `ra-exec` / `ra-tools` / `ra-patch` / `CodingHost` 逐项判定直接复用、适配、产品扩展或无消费者待删；每个行为差异必须注明必要性或移出主线，不能只写“Rust 惯例”。记录依赖和复制代码的许可证／归属。未审计项明确标缺口，不以整章 DONE 代替。 | **部分完成**；见 [受版本管理的契约与差异清单](R8-P0_上游沙箱契约与差异清单.md)。生命周期／工具语义已勘误，核心协议注入与依赖方向已裁决，P1/P4 优先测试映射已补；完整字段与跨章审计仍待清单 §11 收口 |
| 1 / **R8-P1 Client、Session、State 与错误契约** | `sandbox/session/{sandbox_client,base_sandbox_session,sandbox_session,sandbox_session_state}.py`、`sandbox/{types,errors}.py`。覆盖 client create/resume/delete、options 类型判别；session start/stop/shutdown/running、文件／exec／PTY／归档／端口能力及 state；保留基础实现与 instrumentation wrapper 的职责差别。 | 不以 `SandboxBackend::confine` 充当此协议；复用 `ProcessManager` 持有的 exec 会话（`ExecSessionId` / `ExecSessionState`）只作内部命令句柄。先移植 `test_client_options.py`、`test_types.py`、`test_errors.py`、`test_session_state_roundtrip.py`、`test_compatibility_guards.py` 的相关测试。序列化后的 path grants／mount authority 必须按上游重绑定，不能把磁盘状态当作新授权。 | TODO；旧契约待适配 |
| 2 / **R8-P2 Manifest、路径与物化** | `sandbox/manifest.py`、`entries/`、`workspace_paths.py` 的 `SandboxPathGrant`、`materialization.py`、session 的 manifest application / mount lifecycle。移植 entries、环境值解析、workspace root、路径授权、ephemeral、用户、挂载与物化顺序。 | 现有 `Workspace` capability 仅在不改变上游路径授权时复用；不把所有读取强制收窄到旧 coding 根。验收 `test_manifest.py`、`test_entries.py`、`test_manifest_application.py`、`test_workspace_paths.py`、`test_mount_security.py` / `test_mount_lifecycle.py`。挂载策略可分批，但公共模型、支持矩阵与缺口必须完整可见。 | **部分完成**（声明面已交付，见 [R8-P0 清单 §3.8](R8-P0_上游沙箱契约与差异清单.md)）。`workspace_paths`／`entries`（含六家挂载 provider、两种 strategy、四种 pattern 与支持矩阵）／`environment`／`manifest`（条目、授权、环境、挂载目标、持久化排除、凭据暴露确认）／`manifest_render`／`materialization` 已落地，线格式与树形渲染逐项对过上游实测。**挂载生命周期执行、激活时凭据边界、`_mount_security` 错误脱敏、宿主符号链接解析那一半 `WorkspacePathPolicy` 未移植**——都需要活 session 或宿主文件系统，随 P3/P4/P7/P8 与服务 crate |
| 3 / **R8-P3 Unix-local client/session** | `sandbox/sandboxes/unix_local.py::{UnixLocalSandboxClient,UnixLocalSandboxClientOptions,UnixLocalSandboxSession,UnixLocalSandboxSessionState}`。移植 create/resume/delete、workspace 生命周期、环境构造、exec、文件与 archive 操作；PTY 的具体实现与 P5 联调。 | 复用 `ProcessManager`、流读取、文件实现前先核对 exec 输出、超时、用户与错误语义。基线 `inherit_host_environment=True`，allowlist 与 manifest environment 的规则照上游；旧默认凭据清洗／rlimit 不隐式套用。真实本地运行移植 `test_unix_local.py` 与 session 共用测试；本后端不宣称 OS 隔离。 | TODO；旧 R8-7 仅是底层组件 |
| 4 / **R8-P4 SandboxAgent 与 Runner 集成** | `run_config.py::SandboxRunConfig`、`sandbox/{sandbox_agent,runtime,runtime_agent_preparation,runtime_session_manager}.py`。client/options、live session、explicit state、RunState 恢复来源及优先级、manifest/snapshot/cwd、capability preparation 与 agent 恢复身份逐项移植。 | 区分 owned/borrowed session；按源码保留 pre-stop、stop、shutdown、delete、dependency close 的条件与失败处理。移植 `test_runtime.py`、`test_runtime_agent_preparation.py`、`test_run_cwd.py`；普通／流式结果、异常、取消、handoff、多 agent、外部 session 不被误删都须测试。先接本地最小执行，snapshot/resume 全场景在 P7 收口，未收口不标本条完成。 | TODO |
| 5 / **R8-P5 Shell、PTY 与交互工具** | `sandbox/capabilities/{capability,shell}.py`、`capabilities/tools/` 的 exec / write_stdin、`session/{pty_types,pty_output}.py` 及两种后端 PTY 实现。保留 capability 绑定、工具自定义回调、参数默认值、yield、进程 id、截断、stdin、终止与 capability detection。 | 旧 R8-1/2/3/3a 的监督器、缓冲与后台视图可适配；旧 `until/match_text/control` 不自动进入上游工具 schema。恢复 PTY 为主线任务，不把管道 stdin 当作终端。非 TTY 与 TTY 的输出语义各自对齐；移植 `capabilities/test_shell_capability.py`、`test_pty_types.py`、`test_pty_output.py`，并验证真实交互／取消／输出收尾。 | TODO；已有管道工具不等于对齐 |
| 6 / **R8-P6 Filesystem、patch、view_image 与能力默认集** | `sandbox/capabilities/{filesystem,capabilities}.py`、相关 tools、`sandbox/{files,apply_patch}.py`。文件操作经同一 sandbox session；保留 workspace scope / run cwd、run_as、工具装配与 configure_tools 行为。 | 对 `ra-patch` 的语法、匹配、部分失败、返回值逐项比较后复用；旧“多个候选一律拒绝”等不能仅凭既有测试成为默认规格。补 `view_image`，验证文件内容与路径校验。移植 `test_apply_patch.py`、`test_view_image_content_validation.py`、`test_posix_tool_paths.py` 和 capabilities 下 filesystem／patch／image 测试。`read_file/grep/glob` 留 coding 扩展。默认 Compaction 与 Skills／Memory 的 R9/R10 分工在 P9 对齐，不能静默漏掉默认能力。 | TODO；已有 patch 为候选组件 |
| 7 / **R8-P7 Snapshot、归档与恢复** | `sandbox/{snapshot,snapshot_defaults,materialization}.py`、session 的 tar_workspace / archive_extraction / snapshot_lifecycle / workspace_payloads、state 与 runtime resume。覆盖 Local/Noop/Remote snapshot 及 specs、persist/restore/restorable、ephemeral 排除与恢复后重物化。 | 分批实现并逐种公布支持状态；不要把 snapshot 扩张成模型历史、外部副作用或整个 loop 的事务回滚。移植 `test_snapshot.py`、`test_snapshot_defaults.py`、`test_tar_workspace.py`、`test_tar_utils.py`、`test_extract.py`、`test_workspace_payloads.py` 及 `integration_tests/test_runner_pause_resume.py`；路径穿越、归档 limits 默认值、授权重绑定及损坏状态必须覆盖。 | TODO |
| 8 / **R8-P8 Docker client/session** | `sandbox/sandboxes/docker.py` 四个 client/options/session/state 类型，依照 P1 协议实现 create/resume/delete、start/stop/shutdown、exec／PTY、文件、workspace archive、挂载、端口与 snapshot 接入。运行配置更换 client，而不是改 agent 定义。 | 旧 Docker 模块为空，不宣称复用完成。模块按 session 设计落位，不实现宿主 `SandboxBackend`。不强制 bind mount 作为唯一文件通道，不硬编码一 run 一容器；环境／默认网络及 `network_mode=none`、端口冲突、labels、容器复用／重建按基线。`none` 不等于 Rusty 的禁全部 socket；rlimit 与平台围栏是 opt-in 扩展。先基本执行，再 PTY／恢复／挂载／端口，全部目标能力完成后才标对齐。移植 `test_docker.py`、`test_docker_network_mode.py`、`test_exposed_ports.py`、`test_mounts.py` 并配真实 daemon 验收；非 TTY 分流可由 SDK 解帧，TTY 按上游。 | TODO |
| 9 / **R8-P9 Instrumentation、扩展与跨章集成** | `session/{events,sinks,manager,dependencies}.py`、capabilities 的绑定与默认集、runtime instructions；核对 Compaction、Skills、Memory 与 R9/R10 的接口，核对顶层 `tool.py` 的 shell／apply-patch 工具与 sandbox capability 工具各自消费者，避免同名混同。 | 旧 `HostEvent` 可作适配出口，不代替上游事件载荷策略、sink 顺序与 flush。移植 `test_session_sinks.py`、`test_session_manager.py`、`test_dependencies.py`、`test_compaction.py` 及能力测试；跨章未接通列为缺口，不新建第二套 compaction/memory 引擎。外部云 client 协议可替换，但七家厂商适配不默认纳入本轮；单独列支持矩阵。 | TODO |
| 10 / **R8-P10 上游对齐验收与迁移** | 汇总 P0 的契约／测试映射，对 Unix-local 和 Docker 执行同一组 client/session 行为场景；覆盖 Runner 同步／流式、handoff、暂停恢复及 caller-owned session。发布默认行为与扩展的迁移说明。 | 上游 mock 测试移植与真实运行测试互补；正式 Linux/Docker job 缺前置条件必须失败，本地 skip 显式计数且不算验收通过。macOS 本地与 Docker Desktop、Windows 的实际支持按上游适用范围分别验证，未验平台明确标未验证。旧 coding E2E 留作回归，但不能替代 parity 验收。升级序列化/API 时按现有兼容政策处理，不自动改变旧用户的安全配置。 | TODO |

**依赖顺序**：P0 → P1 → P2 → P3 的基础实现 → P4 的本地运行接线 → P5/P6 → P7 与 P4 恢复收口 → P8 → P9 → P10。共享协议涉及的细节可以在首次真实消费者出现时完善；不能等所有后端完成才第一次接 Runner。P9 的跨章契约在 P0 就登记，完整集成不晚于 P10。R8-11a、seatbelt/bwrap、doctor 与更严格网络策略均不阻塞本顺序。

### R8 旧编号去向与既有代码复用

| 旧任务 | 旧实现状态（历史事实） | 当前归属与处理 |
| --- | --- | --- |
| R8-0 CodingHost / 事件契约 | 已实现 | 对应 P0/P1/P4/P9；保留产品宿主和兼容记录，不拿旧公开类型冻结阻止上游契约适配。 |
| R8-1 exec_command | 已实现 | P3/P5 的候选执行组件，按上游参数／返回／超时行为适配。 |
| R8-2 管道 write_stdin，PTY 删除 | 管道已实现 | P5；撤销“PTY 不在主线”裁决，旧管道支持不能代替上游 PTY。 |
| R8-3 后台 job | 已实现 | 内部消费视图可复用；公共生命周期按 P1/P5，不额外建立第二个进程所有者。 |
| R8-3a until/match/control | 已实现 | Rusty coding 扩展；默认移植工具面按 P5，不强行把扩展参数并入。 |
| R8-4 V4A apply_patch | 已实现 | P6 逐行为核对后复用，现有拒绝／匹配规则不自动算上游规则。 |
| R8-5 Workspace / 文件工具 | 已实现，曾有移交项 | P2/P6 复用实现，不以 coding capability 根替代上游 grants 与 session 文件 API。 |
| R8-5a Read Ledger | 不实现 | 维持不实现；不作为移植或验收依赖。 |
| R8-5b 文件事件 | 已实现 | P9 适配候选／coding 事件扩展，不冒充上游 audit 全覆盖。 |
| R8-6 grep/glob | 已实现 | coding profile 扩展，不改变上游默认 capability surface。 |
| R8-7 unix_local 基线 | 环境／rlimit 已实现 | P3 新建上游 client/session；旧 EnvPolicy/ResourceLimits 保留为显式增强，默认行为分开。 |
| R8-8 seatbelt | 已实现并有 macOS 验收记录 | 可选宿主围栏扩展；保留代码与测试，不作为 OpenAI session 协议或默认安全阶梯。 |
| R8-9 bwrap/seccomp | 已实现；历史验收状态表述不一致 | 可选宿主围栏扩展；原生验收状态需按 CI 证据另行核实，不能用本次重排宣布完成。 |
| R8-10 Docker | 未实现 | 由 P8 接管；重排前的首期限制不再定义移植完成条件。 |
| R8-11 manifest/snapshot/物化 | 未实现 | 拆至 P2/P7，移除未定义的“loop 回滚”承诺。 |
| R8-11a WorkspaceLease | 未实现 | 移出移植主线，保留为 R12-5 coding 并发扩展的原编号；重新论证最小实际消费者后实施。产品若依赖该保护，完成前仍不能默认开启相应可写并发。 |
| R8-12 统一网络策略 | 未实现 | 上游 backend 选项在 P3/P8 对齐；跨 web/MCP 出站统一策略及严禁 socket 是可选产品扩展，既有 R11-2b 等引用仍指向此扩展。 |
| R8-13 run 临时目录 | 底层机制已有，接线／跨进程恢复未完成 | 移植所需 workspace 临时资源按 P3/P4/P7 生命周期处理；`RunTempDir` 可内部复用，`RUSTY_AGENT_TMPDIR` 与独立 doctor 回收为 coding 扩展，不强制所有 session 每 run 新建目录。 |

### R8 参考、范围与验收规则

**OpenAI 决定语义，Codex 帮助实现。** 原 R8-A 的缓冲、进程组、patch 解析、路径处理等经验留在归档中作为候选；“直接照抄依赖选型”“先冻结 Rusty 类型”不再是移植原则。实际复制或改写第三方源码须核对固定版本 LICENSE/NOTICE 并保留要求的归属；不能把算法复用扩张成搬入整个产品控制面。

**本轮范围**：上游通用 sandbox/client/session 契约、Unix-local 与 Docker 内置后端、相关 capability、状态／snapshot 与 Runner 集成。已有上游能力可以分批，不可以用“非目标”隐藏差距；云厂商专有 client 实现明确留后续，本轮保留可实现它们的上游 client/session 扩展点，不能用宿主命令包装 trait 替代。不自研 diff 格式，不增加默认 read ledger、强制 git worktree、全局 writer lease 或更严格安全政策。

| 验收维度 | 完成标准 |
| --- | --- |
| 契约 parity | P0 每项都有源码／上游测试／Rust 实现与测试的映射；默认值、优先级、错误和生命周期一致，必要偏离有具体 Rust 原因；未覆盖项显式列出。不能只凭类型名相似判对齐。 |
| 生命周期与恢复 | owned/borrowed、每 agent 资源、start/stop/shutdown/delete 顺序、普通／流式结果清理及失败、state 往返、授权重绑定和暂停恢复均通过；Drop、CLI 退出或输出 EOF 不替代实际清理事实。 |
| 工具与工作区 | 默认 capabilities、工具 schema、PTY 条件、run cwd/run_as、exec 与文件在同一 session 中工作；snapshot/ephemeral 和目录授权按上游，不把工作区恢复称为外部副作用回滚。 |
| 真实后端 | Unix-local 与 Docker 各有真实创建、执行、交互、文件、持久化／恢复及清理测试；目标 CI 不能靠 skip 通过。平台支持与未验证项单列，不以一个 Linux job 宣称跨平台完成。 |
| 兼容与可选增强 | 旧 coding 工具／持久化回归保持可追溯，迁移显式；seatbelt/bwrap、rlimit、环境过滤、lease 等仅在选择的扩展路径生效，并继续单独验收。 |

---

## R9 会话与持久化

### R9 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R9-0a | **最小 rollout append writer（R5 前置；不含快恢复，见 R9-0b）** | **DONE** | **落地形态**：`ra-session::rollout` 包含 `writer.rs`（`RolloutWriter`、`RolloutRecord`、`RolloutPayload`、`RolloutSessionMeta`、`RolloutSidecar`、`RolloutTurnContext`、`RolloutModelUsage`、`RolloutChildAnchor`）与 `reader.rs`（`RolloutReader`、`RolloutSummary`、`graft_child_transcripts`）。usage 累加收口在 `ra_core::usage::Usage::accumulate`（含未知字段合并），**不从 ra-session 开放 `Unknown` 的写入口**——中途曾把 `Unknown::extend_from` 与 `Usage::with_unknown` 改成 `pub` 来实现它，那等于给「未知字段只由反序列化产生」开一道任意写的后门，已还原。<br>① **文件与序号不变量**：以 `SessionId` 为 root thread 文件划分（`rollout-<session_id>.jsonl`），行级 append-only。`timeline_seq` 为严格落盘时分配字段，内存中的 `HostEvent` 不预先赋值；重开文件从已有记录的最大 `timeline_seq + 1` 续号。恢复时校验**严格递增**：重复或倒退直接报 `Corrupted` 拒绝打开（那意味着两个 writer 交错或外部改写），**空洞不算损坏**——截断残尾本来就会留洞，且 R6-6a 已定死序号空间允许空洞。<br>② **不可裁剪的逐请求 usage 基线**：每次模型调用结算以 `RolloutModelUsage` 追加；writer 维护 `usage_totals` 累加值，与 `RolloutReader::scan_summary` 扫盘求和严格一致，会话历史压缩（`Compaction`）后依然保持全量 token 消耗账本真相。<br>③ **崩溃安全与尾行容错**：写入逐行 flush。**判据是「行尾有没有换行符」而不是「能不能解析」**——`read_until` 只在 EOF 处才返回不带分隔符的缓冲区，所以缺换行是区分「进程死在 `write_all` 中途」与「这行写全了但本版本读不懂」的唯一信号。无换行结尾的残片（含 CJK 多字节 UTF-8 断裂）由 reader 忽略、由 `RolloutWriter::open` 截断至上一条完整记录；**带换行结尾的完整记录一律保留**，即使 payload 解析不了。反过来做过一版：把 payload 解析失败的末行也判成残尾并截掉，结果是新版本写的记录在旧版本上被**永久删除**且 `timeline_seq` 回退重号——正是 `compat.rs` 开篇要防的静默降级删数据。另有一个撕裂写可能停在恰好能解析的字节边界上，`open()` 补一个换行符封行，否则下一条会拼到它尾巴上。<br>④ **信封与反序列化严密性**：`RolloutRecord` 采用显式 `type` 与 `payload` 字段，杜绝重复 flatten key（两个 `#[serde(flatten)]` 会让 `unknown` 把 payload 一起吞掉，再序列化产出重复 key 且体积翻倍）；`payload()` 严格校验已知类型（损坏时报 `Corrupted`），仅未知 `type` 降级为 `Unknown`。<br>⑤ **子 run 锚点与 Replay 拼接**：子 agent 完整轨迹写独立 transcript（subkey），root thread 写入携带 `AgentOperationId`、`child_agent_id` 与 `transcript_ref` 的 `RolloutChildAnchor`（`Spawned` / `Completed` 等）；`graft_child_transcripts` 在回放时依据锚点将独立子轨迹稳定缝合回主时间线。<br>⑥ **Sidecar 是只写产物，`open()` 一律全量扫描重建**（原计划的「sidecar 直接恢复、避免全文件扫描」已**从本任务移出**，由 R9-0b 以 rollout 内 checkpoint 记录实现，已 DONE）。`rollout-*.sidecar.json` 仍然落盘（`persisted_run_max_seq`、`next_timeline_seq`、`usage_totals`），但**恢复路径永不读它**，写失败也只置 `sidecar_is_stale()` 并打 warning，不让派生缓存的失败改变主写入的成败。**为什么不能快恢复**：这些都是从日志派生的汇总值，而**放在日志旁边的任何校验都证明不了汇总值由这条日志算出**。中途实现过两版都不成立——先按「文件长度相同即内容相同」判定，可 `flock` 是建议锁，别人原地改写长度不变；改成对日志字节做 SHA-256 后仍然不成立，摘要回答的是「这些字节变没变」，而 `next_timeline_seq` 与 usage **根本不在它覆盖的字节里**，sidecar 自身一次仍能解析的损坏（seq 3 变 1）照样通过校验，续写立刻写出重复序号、日志从此打不开。**要真正跳过扫描，汇总必须以 checkpoint 记录的形式写进 rollout 内部**，受 reader 的损坏契约覆盖，sidecar 退化为「最后一个 checkpoint 在哪个偏移」的索引（索引错也无害，checkpoint 本人从日志读并校验），恢复只扫 checkpoint 之后的尾巴。这已拆成独立任务 R9-0b。**注意 sidecar 的公开定位是 advisory cache**：可以喂状态栏或看板，不得用于恢复、预算、计费或任何权威判断——它可能任意错、可能滞后（写失败只置 writer 侧的 `sidecar_is_stale()`，外部读者看不到）、也可能被非原子覆盖写撕裂；权威 usage 账本是 rollout 里的 `model_usage` 记录序列。<br>⑦ **单 writer 由 `flock` 强制**：`open()` 先开文件、**先取 `LOCK_EX`+`LOCK_NB` 再扫描**，让「扫描 / 分配 seq / 追加」整体落在锁内；第二个 writer 立刻失败并指名冲突。已有文件分支同时恢复 `O_APPEND` 作为第二道防线——此前用 `read + write` 加 `seek`，两个 writer 会互相覆盖记录并让整个日志读不出来。锁挂在 open file description 上，进程崩溃由内核回收，没有陈旧锁文件。**依赖**：`rustix`（本就在锁文件里的传递依赖，API 安全，不触碰 `unsafe_code = "forbid"`）。**非 Unix 平台直接拒绝打开 writer**，不做无锁降级——无锁运行会损坏日志而不是降低服务质量，把失败留在 `open()` 才可读；解除限制的方式是实现 `LockFileEx`，不是删掉这个检查。**明确非目标**：不遵守 flock 的外部写入者。advisory 锁只约束参与者，扫描与首次 append 之间的窗口任何 advisory 方案都关不上，扫描后重查只会缩小窗口却暗示一个并不存在的保证。支持模型是「每个 rollout 一个 writer，打开期间无外部改动」，违反由 ① 的单调性检查事后抓出。<br>⑧ **写失败后 writer 中毒**：`write_all` / `flush` / `sync_all` 任一失败即置 `is_poisoned()`，后续 `append` 一律拒绝，错误文案写明「提交结果未知，需 drop 后重开」。内核可能已接受部分或全部字节，而调用方两种自然反应都是错的——重试会在残片后再写一条、两行融成一条不可解析的记录，换个 payload 继续则会复用一个可能已落盘的 `timeline_seq`。重开即恢复路径（③ 修残尾、① 重算序号）。<br>**本条刻意未做**：resume 的快路径（见⑥，已拆为 R9-0b）；sidecar 的原子替换与每次 append 重写它的写放大（它已不参与恢复，后果仅限外部读者可能读到半截 JSON）；非 Unix 平台的文件锁。<br>**验收落点**：`tests/it-session/tests/rollout_writer.rs` 20 条（全套 `it-session` 27 条通过）。以上不变量逐条做过 mutation 验证——改回旧行为必有测试失败，其中「恢复重新采信 sidecar 汇总」一改会同时打挂 6 条。**未执行的一条**：⑧ 的错误注入测试走 Linux `/dev/full`（唯一无需 root 的必失败写目标），`#[cfg(target_os = "linux")]` 门起来，**在 macOS 开发机上不会运行**；`RLIMIT_FSIZE` 会发 `SIGXFSZ` 打死进程，稀疏文件撑上限的边界随文件系统而变，都不能作为可移植替代。中毒逻辑目前只有 Linux CI 才真正跑到。 |
| R9-0b | **rollout 内 checkpoint 记录与快恢复** | **DONE** | 从 R9-0a ⑥ 拆出。<br>① **`RolloutPayload::Checkpoint` 写进 rollout 本体**：`RolloutCheckpoint` 携带 `session_id`、`usage_totals`、`persisted_run_max_seq`，每 `DEFAULT_CHECKPOINT_INTERVAL`（256，`set_checkpoint_interval` 可调）条记录后追加一条，**排在它所统计的那批记录之后**，所以它总结的前缀已经落盘。它是一条普通 rollout 行,因此自动受 reader 的解析契约与 `timeline_seq` 单调性检查覆盖——**这正是放在日志旁边的汇总做不到的事**,后者被改坏了没有任何东西能发现。checkpoint 自身的 `timeline_seq` 就是边界,恢复以它为种子再叠加其后的记录。<br>② **sidecar 退化为索引**：只多一个 `last_checkpoint_offset`,**恢复只读这一个字段**,而且当提示而非事实用——按偏移读出那一行后要求它解析得出、是 checkpoint、且 `session_id` 与本次打开的一致,任一不满足就回落全扫。因此索引错误、滞后、缺失三种情况**只花一次全扫,不会给出错误答案**;这也是索引可以住在日志外面、而汇总不行的原因——偏移能拿它指向的东西验,汇总没有东西可验。**索引会自我修正**：尾扫途中遇到更新的 checkpoint 就采用它,否则一次 sidecar 写失败会让此后每次 resume 都从那个旧 checkpoint 重扫越来越长的尾巴。**采用前先验它本会话可用**——只改 checkpoint 的 `session_id`、汇总数值仍正确这种篡改,会让全扫把一个快路径必然拒绝的 offset 写回索引,于是此后永久全扫且什么都不报;现在这种 offset 不进索引,写入方在下一个间隔补出新 checkpoint 后索引自行恢复。<br>③ **边界单调性单独校验**：尾部扫描看不到它前面那条记录,所以「尾部第一条没有越过 checkpoint」这种倒退它发现不了,必须在拼接处单独判。漏掉这条,快路径会静默接受一条倒流的序号。<br>④ **checkpoint 间隔跨重启保持**：`records_since_checkpoint` 由恢复时的「最后一个 checkpoint 之后有多少条记录」重建,而不是归零。归零会让每次都差一点到间隔的进程永远触发不了 checkpoint,尾巴无限增长,快恢复退化成长尾扫描。<br>⑤ **sidecar 原子替换与写放大**：改为写临时文件再 `rename`（同目录内 rename 是原子的,而截断原文件再写会留下一个读者可能看到空文档或半截 JSON 的窗口）;不再每条 append 重写,改为 `open()` / 每次 checkpoint / `flush()` / `sync_all()` 时写。**代价**：writer 若不 flush 就 drop,sidecar 会停留在上一个 checkpoint 的状态——对索引无害（回落全扫）,但把它当摘要看的外部读者会看到旧数据,这与它本就是 advisory cache 的定位一致。<br>⑥ **全扫时校验 checkpoint**：从头扫描手里正好有 checkpoint 声称要总结的那段前缀,于是顺路把它的 `usage_totals` 与 `persisted_run_max_seq` 跟实际累加值对一遍,不符报 `Corrupted`;日志若有 `session_meta`,checkpoint 的 `session_id` 与之矛盾同样报错(没有 `session_meta` 的日志无从对照,那一档由上面的「不进索引」兜住)。**这是必要的补丁而不是锦上添花**：审查指出「记录放进日志不会自动证明它由此前缀推导而来」——把 `usage_totals.input_tokens` 原地改成另一个合法数字,session 与 `timeline_seq` 都不动,快路径会照单全收。这一点属实,先前的提交信息把「进了日志」说得能解决信任问题,是**过度声称**。进日志真正买到的比那窄:它不会像旁边的文件那样在正常运行中与日志走散(不会陈旧、不会跨会话、随日志一起被复制或截断),而且**它是一堆原始事实中唯一的派生值,所以唯一可被交叉校验**。<br>**明确的取舍**：能校验它的前提是读过那段前缀,而那正是快恢复要省掉的事,所以**快路径必然信任 checkpoint 的数字,原地篡改在那里发现不了**;任何跳过全量读取的方案都如此。`open()` 因此不再是全库体检,**`scan_summary` 才是权威完整校验**——它读全部记录、解析全部 payload、并对账每个 checkpoint;`read_all` 只校验行级信封,payload 不解析、checkpoint 不对账,不能当完整校验用。<br>**验收落点**：`tests/it-session/tests/rollout_writer.rs` 30 条（全套 `it-session` 37 条通过）。快路径由「把 checkpoint 之前的头部整段改成等长垃圾」证明——全扫必失败,恢复却成功且 usage 与全扫一致,说明它确实没读那段。回落一致性由五种索引状态（正常 / 缺失 / 截断 / 越界 / 指向非 checkpoint 记录）逐个与全扫结果比对。八条 mutation 验证：关掉快路径、去掉 checkpoint 身份校验、去掉边界单调性检查、关掉全扫校验、间隔计数归零、索引不前进、索引采用不验可用性、去掉身份矛盾检查,各自只打挂对应的测试——中间有一版前两条 mutation 存活,因为测试构造的场景在「信」与「不信」下答案恰好相同,已改成两者会得出不同结果的构造。 |
| R9-0 | **双通道事件日志（rollout 格式）** | TODO | **照 Codex 的 `rollout-*.jsonl`**：一个文件 = 一个 thread，行级 append-only（崩溃不破坏已写内容），同一时间轴上交织两个通道——**`response_item`（送往/来自模型的 wire 协议）** 与 **`event_msg`（驱动 UI 的事件流）**。顶层还有 `session_meta`（首行：id/cwd/cli_version/model_provider）与 `turn_context`（每轮的 cwd/approval_policy/sandbox_policy/model/effort）。**`function_call` ↔ `function_call_output` 靠 `call_id` 配对而非顺序对齐**（CC 同理：`tool_use.id == tool_result.tool_use_id`，71 对完全配平）。收益：天然支持回放、rollback（实测 `thread_rolled_back` 37 次）、断点续跑；UI 拿 `event_msg` 做流式渲染。**实现注释**：这里也参考 openai `RunResultStreaming.stream_events()` 的三类事件分层，但落盘要同时保存 raw provider event、semantic run event 与 session item，避免 UI 事件丢失后无法 replay。**这是 R13 控制协议与 R14 replay 的共同地基，必须先于两者** |
| R9-2a | **最小 `Session` port 与 `SessionId`** | **DONE**（三条验收全过，落点见下）| **落地形态**：`ra-core::session` 只有 `session.rs` + `session/{id,port}.rs` 两样东西——port 与身份；`ProviderConversationId` 在 `ra-core::model::request`；参考内存实现 `InMemorySession` 在 **`ra-session::memory`**。<br>**① 实现放哪、以及它的连带代价**：`ra-core` 里本来有 `InMemoryHostEventSink`（`event/sink.rs`）这个同类先例，把内存实现留在 `ra-core` 本来说得通；最终按「零实现」的字面口径挪进 `ra-session`。**这不是免费的**：`ra-core` 侧从此没有任何可跑的实现，测试必须分工，而分工一旦做错，验收就会静默失效（见 ②）。锁中毒沿用 sink 的口径——`PoisonError::into_inner` 继续跑，不报 `SessionErrorKind::Corrupted`（那个 kind 是 `NeedsIntervention` + 「会话记录已损坏」文案，对一个内存测试替身过重）。<br>**② 测试分工是硬约束，不是风格偏好**。`tests/it-core/tests/session_port.rs`（6 条）只验 kernel 自己拥有的东西：`SessionId` 的形状与集合行为（`Borrow<str>` 与 derive `Hash` 自洽，所以 `HashMap<SessionId, _>::get("...")` 成立）、object safety、可在 `ra-core` 之外实现、trait object 跨 task 边界（钉住 `Send + Sync + 'static`）——它跑在测试内 stub 上，因为 `ra-core` 没有别的可跑。**三条验收断言必须跑在 `InMemorySession` 上**，落在 `tests/it-session/tests/session_contract.rs`（7 条）。这条是踩出来的：中间一版把验收断言留在 it-core 的 stub 上，于是 `clear()` 与 `new_with_items` 覆盖为零、`get_items` 的 `n >= len` 分支也没走到——把尾切片改成头切片、`clear()` 改成空实现，全套测试照样绿。**验收断言打在测试自己写的 stub 上，只证明 trait 可实现，不证明发出去的实现是对的**。搬回真实实现后，同样两个 mutation 立刻打挂 4 条。<br>**③ `it-session` 对 `ra-session` 用 `default-features = false`**：验收第二条「任一产品无需 SQLite/JSONL 也能实现与消费此 port」是一句**依赖图断言**，唯一可验证的形式就是 `cargo tree -p it-session` 里没有 rusqlite。`ra-session` 的默认 feature 是 `sqlite`，不关掉这条验收就只存在于文档里（改之前确实带着 rusqlite 0.32 + libsqlite3-sys 0.30）。**将来加 SQLite store 的测试请另开测试目标，不要在这里打开 feature**，`tests/it-session/Cargo.toml` 里已写了这条注释。<br>**④ `ProviderConversationId` 必须真的接进 `ConversationContinuation`，只定义类型不接等于没做**。审查里出现过「类型建好、`ConversationId(String)` 一字未改」的中间态：那时三分身份只活在 doc 注释里，本地 `SessionId` 照样能塞进 provider 槽位。现在是 `ConversationId(ProviderConversationId)`、`conversation_id() -> Option<&ProviderConversationId>`、`with_conversation_id(impl Into<ProviderConversationId>)`。newtype 是 `#[serde(transparent)]`，而 `ConversationContinuation` 用的是 `tag = "type", content = "id"` 的邻接标签，所以 **wire payload 一字未变**；生产调用点只有 `ra-model/src/openai/responses/request.rs` 一处，由 `it-model` 的 `body["conversation"] == "conv_123"` 兜住。<br>**⑤ `SessionId::generate()` = `sess-` + UUID v7**，与 `ExecSessionId` 的 `exec-` + v7 同口径，而不是 `RunId` 的裸 v4。两个理由：R9-0a 是「一个文件 = 一个 thread」、thread 身份就是它，v7 的时间可排序性在文件名上是白拿的；三种 id 会同时出现在 `HostEvent` / `RunState` 记录里，裸 v4 在日志里彼此无法区分。**注意 `RunId` 至今没有前缀**（`state/run.rs` 仍是 `Uuid::new_v4()`），补不补归 R6，别照着这条的注释以为已经有了。<br>**本条刻意未做**：compaction 的 optional capability（只保证它不进必需接口）；`SessionStore`（R9-2）；`Session` 还没接进 runner 与 `RunContext`（R6-6）。<br>**原始约束（保留）**：先在 `ra-core::session` 定义不绑定存储布局的最小异步契约：`get_items(limit)` / `add_items(items)` / `pop_item()` / `clear()`，以及不透明 `SessionId`。它只管理本地、可移植的对话历史，和 R3-9a 的 live `RunContext`、R6 的可恢复 `RunState` 严格分离。compaction 通过单独 optional capability 暴露，不能加入必需接口。<br>**三条 2026-08-11 审查补**：<br>① **收发的是 R1-1 的 `RunItem`（session 权威记录），不是 `ModelInputItem`**——那两层已经在 R1-1 冻结，port 只说 "items" 会让每个实现自己挑一层。`limit` 是**读投影参数、不做物理删除**（对照表 §`Session` 一行已写明，这里同样约束到接口上）；模型输入只能由 `RunItem` 经 R1-17 normalizer 投影产生。<br>② **本地、exec 与 provider 的会话 id 必须三分**：本条的本地权威历史使用 `SessionId`；R8-2 的进程/PTY 使用 `ExecSessionId`；`ModelRequest` 的 provider 管理远端 conversation 使用 `ProviderConversationId`（wire 字段仍可叫 `conversation_id`）。当前 `ConversationContinuation::ConversationId(String)` 已是 provider 语义，不能复用为本地 session 身份；R9-13 也禁止它进入可移植 `RunState`。<br>③ **排序约束不是「R9-2 之前」**（按编号本来就是，等于没约束）。真正等它的是更早三条：R5-3 compaction、R6-6 的 `session_items`、R9-12 对账。**执行顺序必须排在 R5-3 与 R6-6 之前**，否则那几条会各自长出一份历史访问方式，再收口就是同时改三处调用面——这与 R3-9a 排在 R8-0 之前是同一个理由。<br>验收：内存实现与 `ra-session` 实现可互换；任一产品无需 SQLite/JSONL 也能实现与消费此 port；`RunItem` 经该 port 往返不丢 `ItemProvenance`、隔离的 `RawProviderItem` 与未知字段（原验收第三条「模型输入只能来自显式投影而非直接序列化 host context」是 R3-9a 的约束，与本 port 无关，已移除）。 |
| R9-1 | 会话存储后端 | TODO | **每个后端各自可作权威，不预设主从，也不强制双写**（2026-09-09 修正：原文把「SQLite + rollout JSONL 双写」写成任务本身；上一轮改写又把 SQLite 留成「查询索引」，仍然不是独立后端）。框架侧只要 R9-2a 的 `Session` port。本条提供的实现各自完整：**JSONL** 可移植、便于外部工具消费；**SQLite** 同样承担权威存储，不只是索引，要能独立满足 append / resume / fork 的全部读写。要同时拿两者的产品可以自己组合两个后端，**组合形态的公开类型现在不定死**——等第一个真实需求出现再决定它长什么样。<br>**恢复所需信息由后端自己提供**：port 只要求「resume 前能拿到该会话的 items 与恢复所需的元数据」，具体是文件偏移、行号、还是主键游标，属于实现细节 |
| R9-2 | `SessionStore` trait | TODO | **它是 R9-2a 那个 port 的后端，不是它的子类型**（2026-08-11 审查补）：port 是**单个会话实例**的接口（`add_items(items)`），本条是**按 key 的多会话仓库**（`append(key, entries)`）——「在 port 之上做扩展」的说法会让两者都成为写历史的合法入口。关系定死为：`SessionStore` + `SessionId` 打开一个 `Session` 视图；runner、agent 与产品代码只能经 `Session` 读写历史。store 的实现、wrapper、迁移与运维代码可调用 backend，但不得绕过 `Session` 另建第二套历史语义。在此之上补持久化/查询扩展：**必需** `append(key, entries)` / `load(key)`；**可选** `list_sessions` / `list_session_summaries` / `delete` / `list_subkeys`（Rust 用独立 trait + `Option<&dyn ...>` 探测，对齐 claude 的 duck-typed 可选方法）。`append` 在本地写成功**之后**调；带 `uuid` 的 entry 当幂等键。compaction-aware 能力单独做 optional trait，不把压缩强耦合到所有 session。**加一条 `SessionWrapper` 组合链**（借鉴 `extensions/memory/encrypt_session.py`）：加密 / 压缩 / 审计 / 脱敏都是**包在 store 外面的一层**，不是每个 store 各自实现——否则第三方写一个 Redis store 还要自己实现加密 |
| R9-3 | 镜像批处理 | TODO | `batched`（~100ms 合并 / 500 条 / 1MiB 触发，**每轮 result 前强制 flush**）与 `eager`（每帧后台刷，不阻塞读循环）两档；失败重试 3 次后降级为 `MirrorError` 消息，不阻塞主流程 |
| R9-4 | 增量摘要 fold | TODO | 纯函数 `fold_session_summary(prev, entry) -> summary`，store 在 append 里维护 sidecar；`list_session_summaries` 一次返回全部，避免"列会话要 load 每个会话" |
| R9-5 | lite 列表读取 | TODO | 只读 head + tail + stat 提取标题/时间/大小（claude `_read_session_lite`）；列表页不解析整个 JSONL |
| R9-6 | 会话链重建 | TODO | 按 `parent_uuid` 重建对话链，分叉时选最优分支（claude `_build_conversation_chain` + `_pick_best`） |
| R9-7 | resume 与物化 | TODO | 本地缺失时从外部 store 物化后 resume；`load_timeout_ms` 防挂死；subkey（subagent transcript）一并物化；路径穿越校验。**「物化到临时 JSONL」是 `JsonlStore` 这一个后端的实现方式**（2026-09-09 修正）——port 层只要求「resume 前能拿到该会话的 items」，后端怎么落地由它自己定，内存后端与远端后端不需要落盘 |
| R9-8 | 会话变更族 | TODO | `rename` / `tag` / `delete` / **`fork`**（按 uuid 链重建父子关系生成新会话），每个都有 store 版本 |
| R9-9 | 崩溃安全 checkpoint | TODO | 增量持久化 todo / file_references / 部分产物；cancel / fail / max-turns 时写 partial snapshot。<br>**出处按裁决删除**（原引 AF P24.5）：条目本身保留，它是 R12-A 已采纳的关闭顺序「…→ 终止子进程/释放 lease → **flush checkpoint** → 释放 registry/reservation」在单 run 上的落点，站在本文自己的不变量上，不需要外部出处 |
| R9-10 | 文件检查点与 rewind | TODO | 编辑前备份，`rewind_files(user_message_id)` 回滚到某条用户消息时的状态（claude `enable_file_checkpointing`） |
| R9-11 | 长跑性能治理 | TODO | 连接池、批量写、索引；长 run（1000+ 事件）下写入不成为瓶颈的基准测试 |
| R9-12 | Session input / persistence 对账 | TODO | 参考 `run_internal/session_persistence.py`：生成 `SessionInputPlan{prepared_for_model, append_for_turn, history_refs}`；session callback 重排/过滤/复制时仍只追加真正的新项。用 `PersistenceCursor{current_turn_persisted_item_count}` 支持 streaming/resume 幂等追加；retry 只 rewind fingerprint 精确匹配的尾部 suffix，失败时恢复已 pop 项并等待 cleanup 可见；guardrail trip、resumed turn、nested-history ownership 和 provider conversation sanitization 都走同一入口 |
| R9-13 | 服务端 conversation session 边界 | TODO | OpenAI Conversations 这类服务端 session 作为 provider-specific backend：lazy 创建远端 `ProviderConversationId`、add/list/delete items、limit 读取和远端 pop；wire 上的 `conversation_id`、服务端 item id、不可持久 reasoning item 等只在 adapter/session policy 内处理。`ProviderConversationId` 不等于 R9-2a 的本地 `SessionId`。<br>**不强制本地权威日志**（2026-09-09 修正：原文的「可移植 rollout/SQLite 仍是本地权威，不能因为有服务端 conversation 就跳过本地事件日志」与 R9-1 的后端可选直接矛盾）。要不要在服务端 session 之外再留一份本地日志，是**产品的可用性与可移植性权衡**——留一份能离线 replay、能换 provider；不留则省一次写入。框架两种都支持，由宿主选 |
| R9-14 | Provider-specific compaction session | TODO | 把 Responses `run_compaction(args)` 做成 optional capability：支持 `previous_response_id` / `input` / `auto`，`store=false` 时禁止错误使用 previous_response_id，本地 tool output 后 defer 到下一轮，force compaction 要写 trace。通用 R5 压缩仍由 rusty-agent 自己实现，provider compaction 只能是额外优化路径 |
| R9-15 | **`WorkState` 持久化** | **DEFERRED（随 R17）** | `WorkState`（R17-1）与 `RunState` 共用 checkpoint 通道：每次 channel 写入进 R9-0 的 `event_msg`（带 `node_id` / `channel` / `reducer` / `version`），快照按图节点边界落盘。**不新建 store**——复用 R9-9 的崩溃安全 checkpoint。图级恢复 = `WorkState` 快照 + 各在跑节点的 `RunState` + 调度器游标（R17-5） <br>**暂缓理由**：`WorkState` 是 R17-1 的产物，图引擎不做则本条无对象。R17 恢复时一并恢复。 |

### R9 非目标

| 项目 | 处理 |
| --- | --- |
| 外部 store 承担持久性保证 | 不做；本地是权威，store 是镜像（at-most-once + 幂等键） |
| 内置多种远程 store 实现 | 不做；提供 trait + 一个参考实现（文件系统），Redis/S3/Postgres 由使用方实现 |

### R9 验收标准

| 能力 | 标准 |
| --- | --- |
| 断电可续 | kill -9 后 resume 能恢复消息上下文、todo 终态、文件引用与部分产物 |
| 镜像不阻塞 | 故意做一个慢 store，消息流延迟不受影响，失败降级为 MirrorError 消息 |
| 列表够快 | 1000 个会话的列表页 < 200ms（lite 读取 + fold 摘要） |

---

## R10 Capability 装配层

> **定位（2026-09-09 审查后收窄）**：`Capability` 对齐上游 `openai-agents-python/src/agents/sandbox/capabilities/capability.py`——上游确有这个概念，含工具、提示、上下文处理与依赖声明，所以 R10 不是凭空设计。但**上游那套服务于 sandbox，我们把它推广成通用装配单元，这一步是自行论证的扩展**，因此它是一种**可选的装配方式**：普通工具与 agent 无须经过 capability 才能装上。<br>**这个复杂度信号已于 2026-09-09 结清**，结论见下一段。
>
> **两条装配入口的职责（2026-09-09 定，原为待解决的复杂度信号）**：两条路**携带的贡献不同**，应按 capability 所需的全部贡献选择入口。<br>**① 在构造 agent 处装**（`ra-coding::host_backed_surface` → `build_agent_with_profile` 这一类）：带 `tools` 与 `static_instructions`。静态文本正是为了进缓存前缀才在 run 之前解析，而前缀只在这里装配一次，背后有段序与已提交的 prompt dump。<br>**② 在 `RunConfig::with_capability` 装**：带 `bind`、`tools`、`instructions`、`deferred_instructions`、`sampling_params`、`context_processor`——**唯独不带 `static_instructions`**。这条路在 agent 已经存在之后才跑，只能追加，落在段序与 dump 的下游，静态片段无处安放。<br>**判据**：只有工具 → 两条路均可；有静态文本 → 由 ① 承载；需要 `bind`、per-run / deferred 提示、采样参数或上下文处理 → 必须由 ② 承载，与有无工具无关（compaction 需要运行时入口）。同时贡献静态文本和运行时行为的 capability，目前没有能完整承载它的单一入口，宿主须显式调整组合，例如将静态文本改为 per-run 提示后走 ②。**两处声明相同工具会按 lookup key 冲突而失败，不会自动合并；无工具的 capability 不触发该冲突，但静态文本在 ② 仍会被拒绝。**<br>**已落地的强制**：`CapabilityPlan::assemble` 现在会**拒绝**携带静态前缀文本的 capability——此前是静默丢弃：工具进得去、声明它的那段话没了，模型收到一个前缀里查无此物的入口。权威表述在 `ra-core::capability` 的模块文档，产品侧的具体取舍在 `ra-coding::capabilities`。

> Capability 把工具 + 提示片段 + 采样参数 + 上下文变换 + 依赖声明**捆在一起装**，这一点对齐上游 `sandbox/capabilities/capability.py` 的同名概念。**但它是一种可选的装配方式，不是一等强制单元**（2026-09-09 收窄，见上）——普通工具与 agent 不经过它照样能装。

### R10 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R10-1 | `Capability` trait | **DONE** | `ra_core::capability::Capability` 以 `CapabilityFamily` 身份收拢工具、异步提示片段、按安装顺序折叠的采样参数、上下文变换和依赖声明；`CompactionCapability` 已作为第一个消费者同时实现它与既有的 `ContextProcessor`。对齐 openai `sandbox/capabilities/capability.py` 时作了三项有意适配：①上下文变换继续用已有的异步 `ContextProcessor` 契约，capability 只暴露该 processor，避免第二套较弱的处理接口；②尚不存在归 capability 所有的 sandbox manifest，故不预置 `process_manifest` 或 `instructions(manifest)`；③不可变的 `Arc` capability 经 `bind(&RunContext)` 返回该 run 的替身，取代 Python 为可变绑定所需的 `clone_for_run()`——`bind` 收 `RunContext` 是因为它是本框架唯一的 live per-run 值，另造一个 capability 专用的绑定上下文正是第 31 行警告的「第二个竞争上下文」。<br>**`CapabilityFamily` 落成开放 newtype 而不是带 `Custom` 的枚举**，扩展安全第 5 条据此细化（见该条）：这个类型唯一的操作就是相等比较，枚举形态会让 `Custom("shell")` 与 `Shell` 不相等；构造器同时拒绝 `Shell` / `SHELL`，一个能力族只能有一个拼法。十个内置族按 R10-3 清单预置为常量。<br>**采样参数是折叠不是「返回一层」**：`ModelSettings::resolve` 是固定四层且没有两层 merge，返回增量层等于把合并语义留给下一条去发明；折叠照搬上游 `deep_merge` 的顺序语义，后一个 capability 看得见前一个的结果。<br>依赖拓扑校验和实际装配仍归 R10-2；本条不含 `RunConfig` 的安装入口。验收：`tests/it-core/tests/capability.rs` 8 条（一族一拼法含 10 个被拒拼法、线格式校验、只声明 `kind` 时其余七项全空、折叠顺序与「各自看得见前一个」、提示片段归属等于 `kind()`、绑定产出 per-run 副本且不写回已装值、上下文变换走 processor 契约）与 `tests/it-context/tests/compaction.rs` 新增 1 条（压缩只贡献变换：无工具、无提示片段、不动采样参数）。 |
| R10-2 | 依赖校验与装配顺序 | **DONE** | `ra_runtime::capability` 负责装配：`CapabilityPlan::resolve` 校验已安装集合并保留显式安装顺序，`assemble(&RunContext)` 检查静态提示通道后整批绑定并收集运行时贡献，`AssembledCapabilities::prepare_agent` 产出执行实例；`RunConfig::with_capability` 提供安装入口。<br>**依赖只要求在场，不形成排序边**，对齐上游 `required_capability_types()` 的集合语义。缺依赖一次报告全集；互相依赖只要所需族均已安装就接受。采样折叠、提示片段和 context processor 链均按宿主显式安装顺序消费。<br>**一族只能安装一个 capability**，否则依赖无法唯一指认对象；自依赖仍拒绝，因为它只能由自身满足，无法校验其他族。<br>**绑定先整批做完再读运行时贡献**，改变族的绑定副本被拒。工具、提示、采样进入执行实例并经 `AgentBinding::prepared` 配对，归属仍是用户配置的 agent；上下文处理器追加在宿主直接安装的 processor 之后。<br>**运行时提示片段接在执行实例的静态指令之后**；要求进入 tail 的片段、抢占其他族 section 的片段、重复 section 名，以及动态指令与前缀文本混装均拒绝。`static_instructions` 由 agent 构造入口处理，运行时装配拒绝该通道，详见本节两条入口职责。<br>集合校验在 `begin_segment` 之前；运行时装配在 run scope 与 deadline 建立之后。`CodingHost` 用 `with_capability` 安装压缩，使 `compaction` 族进入装配集合。<br>验收：`tests/it-runtime/tests/capability_assembly.rs` 覆盖缺依赖、一族一装、自依赖拒绝、互相依赖接受、依赖不改变安装顺序、按安装顺序折叠、绑定及贡献收集、提示通道校验、工具冲突、processor 顺序和完整运行路径。<br>**2026-09-09 收窄**：删除无真实依赖生产者支撑的拓扑排序，保留存在性校验。后续只有实际 capability 组合证明需要额外排序机制时才重新评估。 |
| R10-3 | 内置 capability 集 | **DONE（10/10：`Shell` / `Filesystem` / `ApplyPatch` / `Search` / `Memory` / `Todo` / `ViewImage` / `Web` / `Skills` 九族落在 `ra-tools/src/capability.rs`，`Compaction` 由 R10-6 落在 `ra-context`）** | 十族清单：`Shell`、`Filesystem`、`ApplyPatch`、`Search`、`Todo`、`Compaction`、`Memory`、`ViewImage`、`Web`、`Skills`。<br>**shell 那一对合成一个 capability 是有实质内容的，不是分组**：构造器收一个 `ProcessManager` 交给两边，「`write_stdin` 只能寻址 `exec_command` 起的会话」从此是结构性的，而不是每个装配点都得自己知道再重新建立的事实。`ra-coding/src/capabilities.rs` 由空壳变成真正的组合层，`host_backed_tools` 完全由它派生；**只读角色的筛选从「按工具的 `PermissionScope` 过滤」上移成「按 capability 过滤」**——capability 是原子的，留半个会产出没有任何已装 capability 描述的面，而这正是 capability 要消灭的形态（判据仍读工具自己的声明，不是第二份名单）。`CodingHost` 的六个单工具工厂改为经四个 `*_capability()` 取工具，工作区与 process manager 只在一处决定。<br>**五个都不声明依赖**：缺依赖是硬装配错误，声明只检查在场、不形成排序边，不是习惯的记录——`apply_patch` 不读文件也能建文件，用 shell 读再打补丁是能跑的配置，声明 `apply_patch -> filesystem` 等于拿它换一条注释；上游 `Filesystem` / `Shell` / `Compaction` 同样什么都不声明。**「第一条真实的边在 `Memory`（它读不了自己的库）」这句已由 R10-8 撤销**：那只在「memory 只出提示词、模型用 `read_file` 去读」的形态下成立，而 `MemoryCapability` 自带三个入口、经 `MemoryStore` 取数，边随依赖一起消失了。**内置集目前仍然零依赖边**，第一条真实的边还在前面。<br>**四个的提示片段已由 R10-4b 补上**（`static_instructions`，见该条）：本条产出的 plan 被 agent 构造路径读**两样**——工具经注册表与 profile 装配成 `ToolSurface`，片段经 `CapabilityPlan::static_prompt_sections()` 与该 surface 对账后进同一个稳定前缀。**没有再装到 `RunConfig` 上**——两边都装会把同一个工具声明两遍。<br>**剩下四族已补齐（2026-09-06）**，原文「缺席而不是空壳」的理由兑现了而不是被推翻：`Todo` / `ViewImage` / `Web` / `Skills` 的工具写完（见 R2-8，11/15），族才进已装集合。**三族要后端才能造**——`Web` 收 `WebAccess`、`Skills` 收 `SkillCatalog`（两个契约新落在 `ra-core::web` / `ra-core::skill`），`Memory` 收 `MemoryStore`；`Todo` 什么都不收（计划板不碰工作区、进程与网络），其余五族收 `Workspace`。所以**宿主没有后端就没有那一族**，这正是要的结果：给缺的那一半配一个答「未配置」的桩，等于让依赖校验通过一个什么都不做的东西。<br>**`ra-coding` 只装了 `Todo` 与 `ViewImage`**，`Web` / `Skills` 不装——它没有后端。档位清单照旧点名这三个入口，于是缺件仍然是**按名字报的装配失败**，「这个部署没有联网」和「这个角色不给」两件事没有被混成一件。`codex_like` 缺的工具从 9 个降到 7 个（`agent` / `ask_user` / `mcp` / `skill` / `tool_search` / `web_fetch` / `web_search`）。<br>**`Web` 是 `Execute` 不是 `Read`**：`Read` 的定义排除「触发外部动作」，而一次检索把查询发出去、把第三方写的文本带回来。只读角色因此不拿它。<br>**`Skills` 是唯一片段不是常量的族**，这与 `Memory` 的规则相反且不冲突：memory 在 agent 干活时就在变，catalog 只在有人装技能时变。声明份额随清单份额一起动，否则装第 12 个技能会变成一条没人碰过的 capability 里的装配失败。验收：`tests/it-tools/tests/capability.rs` 14 条（原 8 条 + 新 6 条：skills 片段随 catalog 变、空 catalog 明说、份额随清单动、web 片段声明结果性质、plan 片段声明整块替换、image 片段区分看与读）与 `tests/it-coding/tests/coding_host.rs` 9 条（新增「宿主不装自己没有后端的族」）；四个新工具各有自己的测试文件，合计 35 条。 |
| R10-4 | 工具面 profile | **DONE** | `core(6-8)` / **`codex_like(14-16, 默认)`** / `full(≤24)`（见 R2-4 与[附录 A](#附录-a-codex--claude-code-实证基线)）。落地形态：`ra-coding::host_backed_tool_surface(role, host, profile)` 由角色装的 capability 建 `ToolRegistry`、按档装配出 `ToolSurface`；`build_agent_with_profile` 把 `into_tools()` 交给 `AgentSpec`、把 advertised 名单交给前缀。**工具与提示从同一个对象读出来，没有哪种排法能只切一边。**<br>**此前 profile 在 agent 构造路径上一次都没被读过**：工具直接来自 capability plan，清单又从同一份列表生成——两边确实一致，但三档形同虚设，预算从没为真正上线的那个面计过费。<br>**先对账再渲染，且渲染用的就是对过的那一份**：注册表记下的是它计费的那批模型面名字，清单渲染的是该 surface 携带的工具，两者是对同一个投影的两次读取，而 `model_definition()` 允许由集成方提供——对完再读一次就可能「对账通过的是一个名字、前缀里写的是另一个」。不一致时错误两个方向都点名。<br>**档位与角色是两个问题，`to_tool_profile_for_role` 是它们相遇的地方**：档位的区间是照「什么都装的 agent」量的，只读角色天生少三个入口，直接套档会撞 floor——而 floor 存在的理由恰恰是抓「掉了工具」。因此把角色扣下的 key 从 selection 里减掉、把扣下的**已广播**那些从区间里减掉（`Hidden` 从来不占预算槽，减它会把区间收窄错）。**减法从「没装的 capability」推出来，不是第二份名单**，capability 里加一个工具，当天就跟着被扣。**它不放松档位**：没人写的工具不在任何角色的扣除集里，照样响亮失败。<br>**收窄后的档报自己叫 `core-read_only_specialist` 而不是 `core`**：这个身份会进装配错误和 surface 自己的记录，`core` 却只点名三个入口，读起来就成了「这一档悄悄缩了」。<br>`build_agent_with_host` 签名不变，走 `HOST_BACKED_PROFILE`（= `core`）——**刻意不是默认档**：`codex_like` 十五个里九个还没写，装不出来，而这正是它该有的行为；这个常量就是「哪天不再如此」要改的那一处。<br>**两点连带后果**：①工具按 lookup key 序到达 agent，而不是 capability 的贡献序——这是 profile 自己的保证（两个宿主装同样的 capability、顺序不同不该产生不同的工具表字节），清单本来就按名排序，所以**所有已提交的前缀快照逐字节不变**；②prompt dump 走同一个入口、同一个常量，报告仍然覆盖产品实际发的那个 agent。<br>**本条不含 capability 自己的提示片段**——R10-3 曾把那一半记在本条名下，与「档位切换切什么」是两件事，另立 R10-4b 跟踪（已完成；`host_backed_tool_surface` 在那里改名 `host_backed_surface` 并同时返回片段，本条描述的对账与档位逻辑不变）。<br>验收：`tests/it-coding/tests/tool_profile.rs` 22 条（新增 8 条：一个注册表切两档、工具与清单逐项同步、被丢掉的九个不出现在整段前缀里 / 提示与 advertised 名单不符被拒且两方向点名 / **对账后的快照直接用于渲染，整条提示路径只读一次投影** / 只读角色收窄而不是撞 floor、且档名带角色 / one-off 装出空面 / agent 声明的与它自己前缀清单的逐项相等 / 装不出来的档在 agent 存在之前就失败 / 一个 surface 同时喂前缀与 agent），`tests/it-coding/tests/prompt_dump.rs` 一条断言随之改为 lookup key 序 <br>**已核实（2026-09-09）：这三个区间本来就在 `ra-coding::profile` 里，框架侧零个硬编码数字**——`ToolSurfaceBudget` 只校验 profile 自己声明的band。原文的措辞读起来像通用约束，是文档问题不是代码问题；两处文档已写明「数字属于声明它的 profile，不约束别的产品」。 命名档位（profile）保留为通用装配方式，数量上下限移到 `ra-coding` 的自有检查里——一个检索产品或对话产品没有理由受这三个数字约束。 |
| R10-4b | Capability 提示片段与工具同路到达 | **DONE** | 内置 capability 各出一段前缀片段（R10-3 补齐四族后为九段）（`ra-tools/src/capability.rs`：`filesystem` 96 / `search` 128 / `apply_patch` 128 / `shell` 128 token 额度，实测合计 ~280），说的是自己那几个入口的机制——`write_stdin` 只能答 `exec_command` 起的会话、补丁的 context 行按盘上现状匹配、`grep`/`glob` 返回的是结果集而 shell 返回的是裸文本、越界路径是被拒不是空。**「什么时候用」不在里面**，那是产品的 `editing_verification` / `tool_use`，一条规则只占缓存前缀的一个跨度。<br>**`Capability::static_instructions` 与 `instructions` 是两个贡献点，不是一个方法两处读**：缓存前缀是一个 agent 所有 run 共用的那一段，所以它的文本不能是某次 run 的函数；`CapabilityPlan::static_prompt_sections()` 在任何 run 存在之前读前者（agent 构造、prompt dump 只能在这里读——`bind` 要 `RunContext`，那时还没有 run），`assemble` 绑定之后读后者。两条路共用同一套结构校验（片段必须认领**自己族名**的 section、source 必须是自己族、必须落在 prefix）。**名字也校验而不只是 source**：`CapabilityFamily::prompt_section_name()` 把「一族一段」变成结构性的，否则一个 capability 可以占掉 `tool_use` 这种产品段的槽，而撞名错误不会说这两个本来就该是不同的东西。<br>**片段在 agent 构造处到达，而不是 run 装配处——与本条原计划相反，理由是清单段搬不过去**：清单是**由已装配 surface 渲染出来的产品文本**，`ra-runtime` 既写不了产品文案、也够不到给它排序的 assembler；而 run 装配收集到的贡献是接在 agent 指令后面的，落在 canonical rank 表与已提交 prompt dump 的下游——一半在快照里、一半不在的前缀，等于门禁不再覆盖真正发出去的那段。于是方向反过来：**已经把 capability 变成工具面的那一处，同时把它们变成提示段**。`host_backed_tool_surface` 改名 `host_backed_surface`，返回 `HostBackedSurface { tool_surface, capability_sections }`，两半由同一次装配产出。<br>**片段与工具面对账**：一个片段只在它能代言的每个 advertised 入口都还在面上时保留；**整族被档位丢掉就静默丢片段**（那是档位在说话，surface 自己的 floor 才是抓「掉了工具」的那个），**只丢一半则报错**（capability 是原子的，它的片段把这一族当一件事描述，半个族会留下点名请求里没有的入口的文本）。只读角色因此天然不带 `apply_patch` / `shell` 两段——这比 R4-0c 那种「手写 `contains_advertised_tool` 门」强的地方在于：门是每加一个工具都得有人记得写的，而片段根本不可能脱离它的入口到达。<br>**`ra-prompt` 的 rank 表把四段排在清单与策略段之间**：讲 `apply_patch` 干什么的段落放在「你有这个工具」的清单之前读不通，而编辑规则放在解释了编辑代价之后更好用；四段之间按角色装配序（读 / 找 / 写 / 跑）。表里写的是字面量（`CapabilityFamily` 进不了 const 上下文），`it-prompt` 那条测试是拴住两处拼写的东西。dump 的 Source 列拓宽到 24 以容下 `capability(apply_patch)`。<br>**解析片段是异步的，所以 `build_agent_with_host` / `build_agent_with_profile` / `PromptDumpRequest::build` / `render_prompt_dump*` / `compare_prompt_dump` / `ra_cli::execute` 一并变 async**，`main` 加 `#[tokio::main]`。这是 R10-1 把 `instructions` 定为 async 的直接后果，不是这里新增的代价。<br>**代价**：host-backed main 前缀 ~1299 → ~1579 token，声明上限 2432 → 2912（四段的 480 计入产品 ceiling，即使文本不在 `ra-coding` 里——只盖住本 crate 自己写的段的 ceiling，装上一个 capability 就不再描述这个前缀了）。<br>**没做的一半**：这四个 capability 的采样参数与上下文变换仍未在产品路径上折叠。四个都不贡献这两样，而 `RoleCapabilities` 从 crate 外注入不了，所以现在加折叠是一段没有测试能撑住的代码；等第一个真出采样参数的 capability 落地时一起补（见 R10 验收表）。<br>验收：`tests/it-tools/tests/capability.rs` 6 条（新增 1 条：每个内置片段的名字/归属/位置/稳定性、额度已声明且没超、**且它贡献的每个入口都在自己文本里被点名**——这半只有 capability 自己知道，产品对不出来），`tests/it-runtime/tests/capability_assembly.rs` 21 条（新增 3 条：run 之前只解析 static 半边且按装配序、static 片段不跨绑定边界被二次读取、run-free 那条读同一套结构规则；原「两个 capability 抢同一 section 名」改写为「per-run 片段必须认领自己族的段」——族名派生之后撞名已不可表达），`tests/it-coding/tests/tool_profile.rs` 24 条（新增 2 条：每个 advertised 入口都被贡献它的 capability 解释、只读角色连同 capability 一起交出它的片段且保留读那半），`tests/it-prompt/tests/stable_prefix.rs` 15 条（新增 1 条：四段的 rank 与注册顺序无关），`tests/it-coding/tests/prompt_regression.rs` 新增只读 host-backed 快照与「已装内置族才能出现在 shipped 前缀里」的 provenance 判据 |
| R10-5 | 按需/懒加载 | **DONE** | `Capability::deferred_instructions` 是第三条提示通道：与另两条一样在装配期解析一次，但**只在信号触发后才投递**，且永不进缓存前缀。三条通道的分工写进 `ra-core::capability` 模块文档：`static_instructions`（run 之前解析→前缀，每轮按缓存价）/ `instructions`（每 run 解析→前缀，该 run 每轮按缓存价）/ `deferred_instructions`（每 run 解析→触发后进历史，一次全价且只在触发的 run）。<br>**一次投递进 run 历史，不是每轮重发尾部——这半是整条任务的成立条件**：尾部在缓存跨度之后，一段 400 token 的片段粘在尾部每轮全价，而常驻在前缀里每轮只付缓存价（≈1/10），也就是说「粘尾部」的懒加载在长 run 里**比常驻更贵**，正好把机制做反。写进历史则付一次，模型一直留着，之后每轮都落在缓存读已覆盖的那一段里。这也是 CC 的 system-reminder 实际形态（[附录 A.4](#a4-claude-code-执行事件流)）。<br>**投递记录是 user 消息不是 system 消息**：system 文本在 input history 里不可移植——Anthropic 适配器只接受它出现在顶层 instruction 字段，出现在历史里当场报 caller 错误（`ra-model/src/anthropic/request.rs`），用 system 会让首次投递之后该 provider 的每个请求都失败。`it-model/anthropic_messages.rs` 新增一条钉住这条 lowering（user 尾巴与 tool_result 合进同一个 user block）。<br>**信号只读结构化状态**：`LoadSignal` 携带 turn 序号与该 agent 的 `AgentToolUse`，**不带任何会话文本**——「任务看起来跟浏览器有关就加载」是自然语言词表匹配，换一种语言或换个说法就失效（AF 去词表化结论，审计 lint 见 R7-10）。**默认信号 = 模型调了这个 capability 自己的入口**，由 `tools()` 推出而不是第二份名单：工具 schema 小且本来就常驻，模型光看清单就能发起第一次调用，解释机制的那段随第一个结果到达，而那也是它最早能被执行的一轮。按 `ToolLookupKey` 而非模型面名字判定，否则两个 server 同名工具会让 A 的调用触发 B 的文本。没有工具的、或由别的结构化事实触发的，覆盖 `wants_deferred_instructions`。<br>**不记「已投递」标志位**：record id 由族名派生（`capability-prompt.{family}`），历史里有它就是投递发生过的记录。标志位是同一事实的第二份拷贝，第一个分歧点就是续跑——历史过 checkpoint，内存标志不过，run 会重发一份它在自己 transcript 里看得见的文本。<br>**结构校验两条**：片段必须认领 `{family}.deferred`（与常驻段分名，因为一个 capability 完全可以两边都出——常驻只留通用策略，机制走 deferred；一个名字两段文本会让 dump 把其中一个报成另一个），且必须要 `TailMessage`（要前缀的 deferred 片段就是「带延迟的常驻文本」，被拒）。<br>**没做的三样**：①**`ra-coding` 里带工具的 capability 拿不到这条通道**——那四个装在 agent 上而不是 run config 上（装两处会把工具声明两遍，见 R10-4b）。今天不花代价：四个都不出 deferred 文本；而**不带工具的重型段（正是 R4-0h 那种）走 `RunConfig::with_capability` 今天就完整可用**，`compaction` 就是这个形态。真正成为限制是第一个「既有工具又有重型片段」的 capability（多半是 browser），那时的解法要求装配层能把「agent 已经装了这个 capability 的工具」与「agent 自己声明了撞名工具」分开，而那正是现在这条撞名检查存在的理由，留给它一起定；②五个未写的族里还没有真正的重型片段，所以内置集暂无消费者；③逐 profile 的 deferred token 快照本该归 R10-7，那里的裁决是**不出这一列**：装在 agent 上的 capability 结构性地拿不到这条通道，一列恒零读起来像个测量值；改成 `it-coding/coding_host.rs` 一条断言——四个内置 capability 都不出 deferred 文本，哪天有人写了就红，而那正是该决定「这段文本怎么到达 run」的时刻。<br>验收：`tests/it-core/tests/capability.rs` 10 条（新增 2 条：一族的常驻段与 deferred 段分名、默认信号读自己的 tools 且按路由身份判定所以同名异源工具不触发；裸 capability 那条补断言「不出 deferred 文本、也不会自动触发」），`tests/it-runtime/tests/capability_assembly.rs` 30 条（新增 8 条：不触发的 run 前缀与尾部都没有它但仍只解析一次、调用自己入口的下一轮以 user 记录到达且始终不进前缀、之后不再重发、别的 capability 的调用不触发它、自定义信号自己说了算、两条结构拒绝、续跑不重发自己历史里已有的那份），`tests/it-model/tests/anthropic_messages.rs` 新增 1 条（settled tool call 之后的 user 尾巴与 tool_result 合进同一个 user block） |
| R10-6 | Compaction 作为 capability | **DONE** | `ContextProcessor` 把策略与 runner 摘要回调隔开；`CompactionCapability` 依据 R5 的窗口/保留/摘要投影生成 `Compaction` 权威记录，runner 接在每次普通模型调用之前。摘要请求复用该轮解析后的模型与稳定前缀、无工具或 handoff 面，使用严格 JSON 输出；CodingHost 默认启用。<br>**判定量的是模型可见视图，不是会话存的历史**：摘要代表它替换掉的记录，但那些记录是权威的、投影永远不删，所以按存储量会永远超线——每轮买一份新摘要，而被量的历史只增不减。`compacted_history_view` 把已被现有摘要覆盖的部分剔掉，这也是一次压缩能跨轮生效、而不是每轮从头重算的原因。<br>**摘要拿不到或读不懂不再终止 run**：压缩恰恰发生在上下文最大的时候，那里失败丢掉的工作最多；该轮改用未压缩视图继续，响应照样计费，取消仍然是取消。无正文的用户消息标注而非拒绝，一张粘贴的图片不能让整个 run 永久无法压缩。<br>**逐项投影排在上下文处理之后**：processor 重投影的是整段历史，投影器若先跑，只会裁掉那些随后被原样替换回去的项——两者同装（CodingHost 就是）时保留区会以全尺寸回来。<br>花费由付账的响应推导而非另行给出；`ModelRequest::with_input` 取代对 `#[non_exhaustive]` 结构的逐字段重建；loop 读 `RunState::input_history_is_complete` 而不是自己再推一遍。<br>验收 `tests/it-runtime/tests/runner_loop.rs` 7 条：压缩后下一轮不再出摘要、第二次压缩量的是当前投影视图、窗口解析不出时不动请求、摘要读不懂/拿不到都继续跑、投影器在上下文处理之后运行 |
| R10-6b | Model-call input filter 链 | **DONE** | `ra-core::filter` 落成契约：`ModelInputData`（input + instructions）/ `ContextFilterRequest`（run id + 全 run turn 序号 + 输出引用账本）/ `ContextFilter` / `ContextFilterReport` / `ContextFilterChain`；`RunConfig::with_context_filter` 按安装序装，chain 跑在每次普通模型调用前、**所有 context processor 之后**。<br>**报告由 chain 量出来，filter 没有自报的入口**：`{changed, chars_saved, token_estimate_delta}` 全部从「交给它的 input」与「它返回的 input」两侧算。自报等于同一事实的第二份拷贝，两份一分歧，R14 replay 断言的就成了 filter 对自己的说法而不是真正发出去的请求——与 R10-6「花费由付账的响应推导」是同一条。量测是**惰性**的：全链无人改动时一次 item 渲染都不走；有人改了，它产出的那份量测就是下一个 filter 的起点，n 个 filter 至多 n+1 次遍历。两个省量**同号**，正数=请求变小，所以把短密钥换成长标记的 redaction 报负数而不是绕回一个巨大的节省。<br>**instructions 可读不可改，改了报错点名**：前缀是缓存跨度，而 filter 每轮跑一次，改它等于每轮挪动缓存键——这正是 R4-11 在上一阶段拒绝 per-run 生成器写前缀的同一条规则，往后挪一个阶段。仍然把它交出去，是因为要给整轮请求计费的策略必须能量到它。这是相对上游 `ModelInputData`（instructions 可写）的有意偏离。<br>**R3-0 在 `prepare_turn` 预留的第 7 阶段是删掉而不是填上**：filter 必须排在 context processor 之后，而 processor 需要 `PreparedTurn` 才建得出 summarizer，所以 filter 最早能跑的点已经在这个函数之外；更硬的一条是 `HistorySpan::split` 按**位置**切 prefix/history/tail，在准备阶段删掉一项会让每个长度检查照样通过而边界全错，随后 compaction 重建的就是错的区段。R4-11 ⑦ 把 `PromptProvenance` 穿过那个阶段，是为了让「改了文本却不更新记录」摆在实现者手里；现在改由 `content_hash` 自己的文档说清它覆盖的是**准备阶段下放的字节**，filter 之后的改动由旁边的 `ContextFilterReport` 逐条记录——各记各的事实，不给 provenance 加第二个状态位。<br>**`ModelInputProjector` 删除，两个 trimmer 改实现 `ContextFilter`**：它本来就是「模型调用前的纯投影」，留着就是第二套同义机制，两套里任何一套先改就会分家。连带收益是 `ToolOutputTrimmer`（R5-8 的位置窗口那个）**第一次有了装配路径**——此前只能被直接调用，没有任何组装请求的代码路径会跑到它。`CodingHost` 的 `ToolOutputReferenceTrimmer` 改由 `with_context_filter` 装。输出引用账本的维护门从「装了 projector」改成「chain 非空」：账本每轮只花一串 call id，而按 filter 逐个问「你读不读 retention」，答错的代价是某个 filter 静默看到一本空账。<br>**字符基线从 `ra-context::estimate` 搬进 `ra-core::item::estimate`**：`ra-runtime` 够不到 `ra-context`（layering 门禁），而一个省量只有跟催生它的那条上限同口径才有意义，所以压缩触发器与 filter 报告必须共用一份；逐项取整也保留，改成先求和再取整会给同一份输入产出第二个数。<br>**刻意没做的两样**：①`Capability` 不加第五个贡献点（`context_filter()`）——今天没有任何内置 capability 出 filter，压缩走的是 processor，加了就是没有消费者的抽象；②报告没有进 span 聚合，`trace::field` 词表不为它扩容，按 profile/按 run 的聚合与 CI 基线归 R14-5。<br>验收：`tests/it-core/tests/context_filter.rs` 10 条（空链原样返回且无报告；无改动也留报告，「装了没事干」与「没装」因此可分；省量按自己那一步量，字符与 token 数写死；增长报负值；装配序 + 每条报告只覆盖自己那步 + 后一个 filter 看到前一个的产出；instructions 可读、改则报错且错误点名 filter 与「缓存前缀」；失败即停、其后的 filter 不跑；turn facts 三样都到；同名两个按序而非按名定位；报告 wire 往返），`tests/it-runtime/tests/runner_loop.rs` 87 条（新增 1：两个 filter 在真实 run 里的逐步量测与链内可见性；改写 2：原 projector 两条改名并补断言 instructions 到达、无改动报告成对出现），`tests/it-context/tests/tool_output_eviction.rs` 17 条与 `tool_output_reference_eviction.rs` 12 条（各新增 1：位置 trimmer 经 filter 抵达请求且与直调同结果；引用 trimmer 从 request 读 run/turn/账本，三轮不动、九轮替换、跨 run 账本被拒），`tests/it-coding/tests/coding_host.rs` 改断言产品策略以具名 filter 到达 <br>**已撤销（2026-09-09）：instructions 只读限制已解除。** 缓存优化不能限制宿主的合法定制——宿主改前缀的后果应当**量出来**，而不是由框架禁止。改法：`ModelInputData.instructions` 恢复可写（回到上游语义），chain 报告增加「本轮是否改动了前缀」一项。<br>**验收**：改后的 instructions 确实进入最终请求；该请求的内容哈希、token 估算与诊断输出与实际发出的请求一致，不继续报告旧值。**缓存字段遵循 provider 语义**——会话级 `prompt_cache_key` 可以保持稳定，不要求它随指令正文变化 |
| R10-7 | Capability 装配快照测试 | **DONE** | `api/capability-assembly.txt` 覆盖每个 shipped tier × role：可装配项记录 capability 归属、模型可见工具、schema bytes、提示段 token 与完整前缀 token；尚无实现的 profile 记录其声明工具清单、预算及完整拒绝原因，不能伪造的 schema/prompt/token 量明确标为 unavailable。这样补齐一个工具或改变任一已装配面的成本都会产生需审核的快照 diff，而不会把未实现工具静默当作零成本。 |
| R10-8 | **记忆的三层结构（只有第一层进内核）** | **DONE** | **先澄清命名**：openai-agents-python 的 `memory/` 是**会话存储**（`session.py` 只有 `get_items`/`add_items`/`pop_item`/`clear_session`，`extensions/memory/` 是 8 个 session 后端，合计 5,246 行），对应 `ra-session`，与检索无关。唯一像检索的 `advanced_sqlite_session.py:1570` 的 `find_turns_by_content` 是对会话轮次做子串匹配、服务于分支与用量统计，仍是会话存储。**真正的记忆参照系是 codex**：`codex-rs/memories/{read,write}`（5,135 行，③）、`codex-rs/ext/memories`（2,506 行，②）、`codex-rs/state` 的 `memories_1.sqlite`（`state/src/sqlite.rs:31`，③的 job 队列）。三层在此对齐：<br>① **契约层 → `ra-core::memory`（本条落地）**：`MemoryStore{list, read, search}` + 独立的 `MemoryUsageSink`。**与原计划的 `{put, search(query,k,filter), forget}` 三处不同**：（a）**没有 `put`/`forget`** —— codex 里模型永远写不了记忆，唯一能碰的写是往 `extensions/ad_hoc/notes/` 丢一份待审提案且需用户显式要求，真正改记忆的是 Phase 2 的离线子 agent；删除是保留窗口 + `usage_count` 排序的副产品，不是 run 能调的动作。留 `put` 等于把那套系统特意不给的写权限发出去。（b）**`search` 不带 `k`/分数** —— codex 的 `ext/memories/src/local/search.rs` 是字面子串匹配、按 (path, line) 排序、全仓没有 embedding；整个系统唯一的排序在 Phase 2 的 SQL 里离线做。（c）**三个读方法不带默认实现**，可选反馈通过独立 port 扩展 —— 全默认会允许存在一个「三个入口全拒绝」的 store，而 capability 照样报告出一个能用的记忆面，正是 R10-3「空 capability 比缺席更糟」那条。<br>**契约里没有任何文件系统语义**：记录由 store 自己发的不透明 `MemoryRecordId` 指名，记录内位置是 `MemoryAnchor`，结果集位置是 `MemoryCursor`，没有 path、没有目录、没有行号。**这是评审改出来的**：初版把 codex 的 markdown 浏览形状（`path`/`Collection`/`first_line`/`matched_line`）直接抄进了内核，而 SQL / 文档库 / 向量库只能伪造目录树与行号来满足它，且每一处伪造都会变成调用方随后依赖的事实——与「形状不能连线上类型一起照搬」是同一条。store 想给人看的位置写进 `MemoryRecord::label` 与 `MemoryHit::location`（`MEMORY.md:12` / `row 4821` / `chunk 7 of 19`），**纯展示、无人解析**。<br>**②→③ 的引用反馈**：读取不写 usage。版本化结果通过 `ObservationMetadata.memory_exposures` 携带 record/revision/anchor 与绑定 excerpt 的 token；runner 只登记实际进入模型请求、未被裁剪或替换的本地工具证据，并持久化到 `RunState`。最终答案的 `[[memory:token]]` 只在匹配曝光账本时形成 `MemoryUsage`；未知 token、未曝光版本、重复引用不计。独立的 `MemoryUsageSink` 由宿主配置，100 ms 超时或失败不阻断答案，持久 outbox / 重试和幂等落在宿主。`MemoryExposure` / `MemoryUsage` 各带自己的 `schema_version` 与 flatten `Unknown`——它们是**嵌套**持久化记录（前者同时躺在 `RunState` 和 `ObservationMetadata` 里，紧挨着同样形状的 `Truncation`），父级的 `Unknown` 捕获不到嵌套层的未知键（R3-8 同一条），丢了就等于把「用过的记忆」记成没用过。**没装 sink 的 run 完全不推导也不持久化证据**：推导要把请求里每份工具输出反序列化再与权威记录逐项比对、每轮一次，产物还会写进 `RunState` 从而进每个 checkpoint——没有 sink 就没有任何东西读得到它，这笔开销和状态膨胀不该发生。list 不产出内容证据，无版本结果可读但不参与保留反馈。<br>② **装配层 → `ra-tools` 的 `Memory` capability（本条落地挂载点，后端实现归 R18-6）**：`memory_search` / `memory_read` / `memory_list` 三个入口 + `MemoryCapability`，收一个 `Arc<dyn MemoryStore>`。**自带专用工具而不是复用 `read_file`/`grep`**（对齐 codex 的 `dedicated_tools=true` 分支；它默认是 `false`，所以默认配置下要靠 `read/src/usage.rs` **解析 shell 命令**来统计记忆用量）。三个收益：store 不必是文件系统、这一族不欠别人依赖、用量是事实而非事后推断。<br>**⚠️ 原计划「第一条真实依赖边由 Memory 带来（它读不了自己的库）」不成立，已撤**（R10-2 / R10-3 / `ra-tools/src/capability.rs` 三处同步）：有依赖的是「memory 只出提示词、模型用 `read_file` 去读」那个形态，而那也正是没有任何结构性东西拦住模型读出记忆根目录的形态。**本框架内置集目前仍然零依赖边**，依赖校验机制留着给第三方 capability 用，为了演示机制去编一条边就是 `apply_patch -> filesystem` 那个错换个名字。<br>**上限归 store，不归工具**：每个 request 带 `MemoryBudget{items, bytes}`，由 store 决定在哪停并发权威 cursor / anchor——工具事后裁掉的东西是拿不回来的（续取只能靠 store 发的 cursor，调用方伪造不出）。预算的 bytes 统一为 `memory_response_json` 编码后的紧凑 JSON 正文字节，包含 ID、anchor、cursor、展示串与转义；store 测量与工具投影完全一致。工具兜底检查条数与字节，后端违约则整页拒绝且不登记证据，不切坏可恢复的分页。**拒绝语落在正文里而不是留一个空正文**——空 text block 是 provider 可以丢掉的块（Anthropic 直接拒），否则后端超预算那一轮不报错、下一次请求才当场失败；它也不受页预算约束，因为那条上限量的是后端产出的紧凑 JSON（`with_max_bytes` 自己就是这么写的），而拒绝语与引用 guidance 一样属于工具自己的控制信封。引用 guidance 是单独的最多 8 KiB 控制信封。**store 用自己的词汇报的错也必须变成一句话**：`MemoryStore` 是第三方扩展点，返回非 `MemoryStoreError` 是常态而不是框架 bug，而这三个入口走 `ToolFailureHandling::Custom`，那里 `handle_failure` 返回 `None` 会让错误上抛、终止整轮——一次记忆查询不该能拖垮整个 run。兜底句子刻意是通用的（store 的原始 message 是本 crate 没写过、也担保不了的开发者文案，可能带主机名或连接串）；**取消不走兜底**，它不是 store 失败而是调用方不问了，必须继续以取消的身份传播。`MemoryBudget::new` 把 0 规范化成 1，因为 `clamp(1, 0)` 会 panic，那会把宿主的一个配置笔误变成每次记忆调用当场崩。<br>**硬约束（比原文更严，原文会把 codex 唯一真在做的事禁掉）**：**随 query 变化的检索结果不得进稳定前缀**；query 无关、慢变、带硬 token 上限的摘要**可以**——codex 就是把 `memory_summary.md` 截到 2,500 token 嵌进 developer instruction（`ext/memories/src/lib.rs` 的 `MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT`），那块只由后台 Phase 2 重写、会话内恒定。本条的实现里检索结果结构性地只能走 tool result（即尾部），capability 片段是常量。<br>③ **管线层 → Deferred**：离线蒸馏 / 择优提炼、lease + watermark 的 jobs 队列（codex `memories_1.sqlite` 形态，`Stage1JobClaimOutcome` 五态、Phase 1 并发抽取 + Phase 2 全局锁与 git baseline diff）。它是独立后台系统，**不进主循环**。<br>**不建 `ra-memory` crate**：①在 `ra-core`，②在 `ra-tools`，③是外部系统，没有哪一层需要单独的 crate。<br>验收：`it-core/memory` 覆盖 opaque 句柄、预算、分页、typed refusal、版本证据持久化与 JSON 测量；`it-tools/memory` 覆盖精确预算、anchor/cursor 透传、超限拒绝（正文非空）、零预算、后端故障、**非 typed 后端错误不终止轮次**与 Unicode 全文多页重建；`it-runtime/runner_loop` 覆盖最终引用去重、未引用不写入、未知/错版本拒绝、filter 移除证据、恢复、sink 故障与**无 sink 时不积累证据**；capability 测试固定前缀与工具尾部投影。 <br>**已修正（2026-09-09）：那条政策表述已撤销，且未新增任何写接口。** codex 不让模型写记忆是它自己的产品选择，推不出所有 rusty-agent 产品都不该有可写记忆。**但现在不新增任何写接口**——保留只读的 `MemoryStore{list, read, search}` 并不妨碍宿主自己装写工具；写入是否需要版本冲突、覆盖、追加、删除、事务，没有真实消费者可供判断，现在定契约就是在清理过度设计的同时新增一个预设抽象。出现实际写入需求时再确定扩展形态。<br>曝光账本、使用反馈、保留排序同样降为**可选记忆组件**：没装 sink 的 run 今天已经完全不推导，把这条从「默认行为」写成「组件契约」即可 |

### R10 非目标

| 项目 | 处理 |
| --- | --- |
| 动态加载 .so/.dll 形式的 capability | 不做；capability 是编译期注册 + 配置期选择；第三方能力走 MCP |

### R10 验收标准

| 能力 | 标准 | 状态 |
| --- | --- | --- |
| 一次操作切一套 | 换 profile 后工具、提示、采样参数、上下文变换同步变化 | **部分（3/4）**（R10-4 + R10-4b）：工具、清单段、各 capability 的片段由同一次 `host_backed_surface` 产出，换档三样同步变；采样参数与上下文变换仍未在产品路径上折叠——四个内置 capability 都不贡献这两样，现在加折叠是没有测试能撑住的代码，等第一个真出采样参数的 capability 一起补 |
| 无错配 | 快照测试证明每个 profile 的提示片段只提到该 profile 存在的工具 | **完成**（R10-4 + R10-4b + R10-7）：不错配是装配期的硬失败，管到两层——清单段（R10-4 的对账）与 capability 自己的片段（R10-4b：代言的入口不全在面上就不进前缀，只在面上一半则报错）；**逐 profile 的判据由 R10-7 补上，且两个方向都判**：`it-coding/tool_profile.rs` 逐 tier × role 扫整段前缀，落在产品工具词表里的反引号名字必须在该行 advertised 名单上，反过来每个 advertised 入口也必须被提到——后者同时是前者的对照，否则「扫不到任何东西」和「前缀真的干净」会给出一样的绿。`api/capability-assembly.txt` 是同一件事的可 review 形态 |

---

## R11 MCP、Skills 与插件

### R11 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| **R11-0** | **MCP 最小可用链路** | TODO | **本条是 R11-1/2/4/12 的先导切片，做完就有一条端到端可用的 MCP 路径**，其余各条在它之上加厚。详见下方[「R11 复用 Codex MCP 栈的取舍」](#r11-复用-codex-mcp-栈的取舍)：① 依赖官方 `rmcp` crate 而不是手写 JSON-RPC；② 只做 stdio + streamable-http 两种传输；③ 抄 `codex-mcp/src/tools.rs` 的模型可见名冲突消解；④ `ToolInfo` → `impl ra_core::tool::Tool`，`concurrency` 由 `annotations.read_only_hint` 推导、`exposure` 由 `tool_search` 是否启用决定；⑤ `CallToolResult.content[]` 直接映射成 R2-3 的结构化 block。**不含**：OAuth、连接复用/懒启动、运行时控制、prompts/resources |
| R11-1 | MCP client：stdio | TODO | 子进程 + JSON-RPC；initialize / list_tools / call_tool / list_prompts / list_resources。抽象边界对齐 `MCPServer`：`connect` / `cleanup` / `list_tools` / `call_tool` / prompts / resources；server 连接状态、工具缓存和 `needs_approval` 由 server adapter 管理，不散落到 runner。stdio 只负责 spawn params、cwd、env、encoding 和 stream 创建，共享 session/request/cache/approval 逻辑 |
| R11-2 | MCP client：SSE / HTTP | TODO | streamable HTTP 落到 `McpTransportConfig`：url、headers、auth、HTTP client factory、request timeout、session id、terminate-on-close。transport 错误必须 credential-safe redaction；initialized notification 失败、HTTP auth 类型差异和 v1/v2 SDK 差异都封在 transport adapter 内。**SSE 降级为可选**：Codex（`070a26a1f0`）的 `codex-rmcp-client` 只启用 `transport-child-process` 与 `transport-streamable-http-client` 两个 feature，SSE 在 MCP 规范里已被 streamable-http 取代——`ra-mcp` 的 `sse` feature 若无真实 server 需要就删掉，留着等于给一条废弃传输开门 |
| R11-2b | MCP OAuth | TODO | 授权码 + PKCE（`rmcp` 的 `auth` feature + `oauth2` crate），refresh token 存储与并发刷新竞争。**依赖 R8-12**：refresh token 是凭据，必须先有一条不会把它带进 trace / 模型输入 / 外发工具参数的通路，再谈拿到它。原先写的依赖项 R7-11（`ContentTrust::Secret`）已撤销——凭据外发是**外发边界**的问题，归 R8-12 / R11 的显式 policy，不是输入侧的 provenance 类型；参考 codex 的 `secrets` crate（keyring 存储 + 输出端 `redact_secrets` 尽力脱敏）。**不照抄 Codex 的那 3.9k 行**——其中大部分绑着 `CodexAuth` / keyring 四平台 / 本地回环回调服务器，服务的是 ChatGPT 账号而不是通用 MCP。在此之前 `bearer_token_env_var` + 静态 header 覆盖绝大多数真实 server |
| R11-3 | **进程内工具服务器** | TODO | 宿主直接注册工具，零 IPC（claude `create_sdk_mcp_server` 的价值所在）。Rust 侧：`InProcessToolServer` + `#[derive(ToolInput)]`。**最小形态前移到 R11-0**：Codex 的 `in_process_transport.rs` 只有 14 行，而它是 MCP 测试的地基——没有它，每条 MCP 测试都要 spawn 一个真子进程 |
| R11-4 | 工具过滤与缓存 | TODO | 静态 allow/block 列表 + 动态过滤函数；`cached_tools` + `invalidate_tools_cache`。动态 filter 输入 `ToolFilterContext{run_context, agent, server_name}`，filter 报错 fail-closed 排除该工具并记录脱敏日志 |
| R11-5 | MCP 工具审批 | TODO | `needs_approval: bool \| {never:[...], always:[...]}`；对接 R6 的审批通道 |
| R11-6 | MCP 运行时控制 | TODO | `reconnect_mcp_server` / `toggle_mcp_server` / `get_mcp_status`（claude client 的三个方法）；状态进 UI。借鉴 `MCPServerManager` 的每 server worker：连接/清理分别有 timeout，服务器并行连接，失败 server 隔离并记录 phase，支持 `reconnect(failed_only=true)`；单个坏 server 不应拖垮整个 run |
| R11-6b | MCP lifecycle task affinity | TODO | 借鉴 openai `_ServerWorker`：每个 server 一个命令队列，connect/cleanup 在同一 worker task 内执行，timeout 用当前 task cancel/`tokio::select!` 表达，避免清理跑到另一个 task 破坏底层 transport/cancel-scope 约束。`cleanup_all` 反序，cancelled cleanup 可按配置 suppress 但必须记录到 `errors` |
| R11-7 | `strict_mcp_config` 隔离 | TODO | 只用显式传入的 server，忽略磁盘上的项目/用户/插件配置——SDK 隔离模式必需 |
| R11-8 | Skills：发现与白名单 | TODO | 从 `.rusty-agent/skills/` 与插件发现；白名单过滤。**明确文档化：白名单是上下文过滤器，不是沙箱**——未列出的 skill 文件仍在磁盘上、仍可被 Read/Bash 读到（claude 文档的诚实说明，照抄这个边界声明） |
| R11-9 | Skills：加载与规模硬化 | TODO | `load_skill` 工具；大 skill 的分块与 token 上限；skill 内容进上下文的位置遵守 R4（尾部而非前缀） |
| R11-10 | 插件系统 | TODO | 本地/内置/Git 插件安装与发现；插件可提供 commands / agents / skills / hooks / MCP servers；**权限风险推导与工具级风险覆盖** |
| R11-11 | 插件安全模型 | TODO | catalog 元数据、权限/风险摘要、checksum/签名状态、显式安装确认、启用/禁用/卸载（原引 AF P18，出处按裁决删除，条目本身保留）。安装记录必须锁定 canonical source（本地绝对路径或 Git URL + commit）、版本、内容 digest、签名验证结果与声明的工具/MCP/hook 权限；更新或来源漂移一律重新审批。R11-8 的白名单只是上下文过滤，**不构成**插件可执行代码或 MCP 的信任授权；其运行权限仍交 R6 与 R8-12 判定（原写 R6 / R7-11 / R8-12，R7-11 整条已在 R7 缩减裁决中撤销，属悬空引用）。 |
| R11-12 | MCP 工具元数据解析 | TODO | 对齐 openai `_mcp_tool_metadata.py`：title / description 有多个来源（`annotations.title` → `title` → `name`），按优先级解析并缓存。**关键区分**：`description_for_model`（进 schema，占 token 预算，受 R2-10 约束）与 `title` / `description_for_ui`（只给宿主渲染，不进请求）是两个字段。把 UI 文案塞进 schema 是白烧 token |

### R11 复用 Codex MCP 栈的取舍

> 依据：本机 Codex Rust 源码 `/Users/moses/workspace/custom-app/codex @ 070a26a1f0`。

**先看规模，再决定抄什么**——整体照搬的代价必须先量出来：

| crate | 行数 | 内容 | 与本项目的关系 |
| --- | --- | --- | --- |
| `codex-rmcp-client` | ~11.1k | 传输 + OAuth。其中 `oauth.rs` 1623 + `perform_oauth_login.rs` 1158 + `auth_status.rs` 890 = **约 35% 是 OAuth** | 传输那部分本来就是 `rmcp` 的薄封装；OAuth 见下 |
| `codex-mcp` | ~31k（含 ~12k 测试） | 连接集合、目录、名字归一化、binding、目录缓存、pagination | **只有 3 处值得逐行搬** |
| `core/src/mcp*.rs` + `tools/handlers/mcp.rs` | ~4.2k | exposure 决策、tool call 派发、Apps/plugin 策略 | 决策规则可抄，策略部分是产品面 |

**最关键的一条事实：Codex 不手写 MCP 协议，它依赖官方 `rmcp` crate**（`codex-rs/Cargo.toml:396`，`rmcp = "=3.0.0"`，features `client / transport-child-process / transport-streamable-http-client / transport-async-rw`）。`ra-mcp` 今天是 25 行空壳，`Cargo.toml` 里 `stdio` / `sse` / `http` 三个 feature 一个 MCP SDK 依赖都没有。**先定这一条，其余都是它的下游。**

#### 值得逐行搬的三处

| # | 位置 | 为什么 |
| --- | --- | --- |
| ① | `codex-mcp/src/tools.rs:113` `normalize_tools_for_model_with_prefix` | **纯函数、零依赖，整段可搬**。算法：sanitize → 检测 namespace 冲突 → sha1 前 12 位后缀 → 检测 tool 名冲突 → 再后缀 → 压进 64 字节。冲突判定用 `server\0namespace\0connector_id` 三元组而不是名字——两个 server 各有一个 `search` 时，名字相同但身份不同。<br>**本项目的落点已经就位**：R2-1 的 `ToolOrigin` / `ToolLookupKey`（namespace + name）就是同一个「原始身份 vs 模型可见名」双名字模型，`ToolNamespace` 是开放 newtype 且已声明「两个 MCP server 的同名工具可在同一 `BTreeMap` 里独立反查」。缺的正是这段消解算法 |
| ② | `core/src/tools/handlers/mcp.rs:114` `supports_parallel_tool_calls` | `self.tool_info.supports_parallel_tool_calls \|\| annotations.read_only_hint == Some(true)`。**这是 R2-1 刚落地的 `ToolConcurrency` 的第一个真实消费者**：MCP 工具的 `options().concurrency()` = `read_only_hint == Some(true) ? Parallel : Exclusive`。默认 `Exclusive` 在这里同样是对的——没声明 read-only 的第三方工具就是可能写 |
| ③ | `core/src/mcp_tool_exposure.rs:90` exposure 决策 | `tool_search` 启用 → 全部 `Deferred`，否则 `Direct`；再叠一层字节预算（单工具 8 KB / 总计 64 KB），超了降 `Hidden`。**MCP 是 `ToolExposure::Deferred` 的头号用户**——三个 server 就是几十份 schema，每轮都付。R2-1 的三态刚好完整表达这个决策，包括「注册可派发但既不广播也不索引」的第三态 |

#### 结构上本项目更干净的一处

Codex 的 `McpBinding`（`codex-mcp/src/binding.rs`）做的是「冻结目录」：一轮的工具列表与执行句柄是同一个快照，`PreparedMcpCall` 绑死 client / timeout / server metadata。**这跟 R3-0 的 `TurnActionSurface`「准备阶段冻结、跨模型调用存活、结算只对着它解析名字」是同一个不变量。** 所以 **不需要第二套 binding**：MCP 工具作为 `Arc<dyn Tool>` 直接进 `TurnActionSurface` 即可。Codex 需要独立的 binding，是因为它的 MCP 目录会在一轮之内被 Apps 刷新覆盖；本项目没有那条路径。

#### 明确不抄

| 项目 | 规模 | 理由 |
| --- | --- | --- |
| **OAuth 全家桶**（`oauth.rs` / `perform_oauth_login.rs` / `auth_status.rs` / `oauth_http_client.rs` / keyring 四平台 feature） | ~3.9k 行 | 见下方专条 |
| `connectors` / `codex_apps` / `plugins` / `code-mode` | `McpManager` 292 行里几乎全部 | ChatGPT 产品面：connector 策略、Apps 工具缓存共享、plugin 归属。本项目的对应位置是 R11-10/11 的插件系统，模型不同 |
| 六态 `ToolExposure` | — | R2-1 已写明理由：它把「可见性 × 面」两条轴交叉进一个枚举，而本项目第二条轴是 `allowed_callers` |
| `connection_manager.rs` 的启动复用 | 892 行 | 大半是 OAuth 凭据比对与 Apps 缓存共享。其中**懒启动**那一小块（`defer_startup` + `watch::channel`，`connection_manager.rs:491-509`：目录缓存里已有可见工具就先不连，等真被调用再连）是干净的好东西，留给 R11-6 单独取 |

#### OAuth：对 rusty-agent 到底有没有用

**框架层近期没用，产品层中期会用，但不该照抄 Codex 那一份。** 三条：

1. **Codex 那 3.9k 行里，大部分不是「OAuth」而是「ChatGPT 账号」。** `auth_status.rs`、`oauth_http_client.rs`、`perform_oauth_login.rs` 绑着 `codex-login` / `CodexAuth` / `codex-keyring-store` / `webbrowser` / 本地回环 `tiny_http` 回调服务器，服务的是「用 ChatGPT 订阅登录 Codex Apps」这个具体产品。**通用的那部分只有 `rmcp` 自带的 `auth` feature + `oauth2` crate 的授权码 + PKCE 流**，本身不大。
2. **覆盖面上，`bearer_token_env_var` 顶得住绝大多数场景。** Codex 自己在 `connection_manager.rs:306` 就把它当成和 OAuth 并列的一条路径（且对 Apps 而言它优先级更高）。今天主流的远程 MCP server（内部服务、CI、自建）都是 header token；需要交互式浏览器授权的是面向消费者的托管 server。**R11-0/R11-2 只做 `bearer_token_env_var` + 静态 header 是正确的取舍**，不是欠债。
3. **真做的时候，卡点不在协议在存储。** 授权码流本身 `oauth2` crate 几十行就跑通；难的是 refresh token 存哪、多进程并发刷新怎么不互相踩（Codex 为此有 `oauth_store_was_contended` 这种字段）、以及**凭据绝不能进 trace / 模型输入 / 工具参数**——最后这条正是 [R7-11](#r7-11) 的 `ContentTrust::Secret` 要管的东西。**所以 OAuth 的正确挂载点是 R7-11（不可信内容与敏感数据策略）落地之后，不是 R11-2 之内**；提前做会造出一份绕过 provenance 的凭据通路。

结论：**OAuth 记为 R11-2b（TODO，依赖 R8-12），不进 R11-0 的最小链路。**

#### 三个会咬人的接口点（实现前先看）

1. **MCP 的 `inputSchema` 基本都不满足 strict 不变量**（没有 `additionalProperties: false`、`required` 不全）。R2-1 复审后 `ToolSchema::new()` 会当场拒收，其 doc 已点名 MCP。**MCP 路径必须走 `ToolSchema::loose()`**，而不是把别人的 schema 改写成 strict 再发——改写会静默改变工具语义。
2. **`CallToolResult.is_error == true` 是模型可见的失败，不是框架错误。** 对应 `ToolFailureHandling::ModelVisible`，绝不能是 `Propagate`：MCP server 报错是日常，不是停轮的理由。
3. **结果转换要比 Codex 做得好。** Codex 是 `serde_json::to_value(content)` 一把塞成 JSON（`codex-mcp/src/binding.rs:291`），因为它没有结构化输出块。本项目有 R2-3 的 `ToolOutputBlock::{Text, Image, File}`，而 MCP 的 `content[]` 本来就是 text / image / resource 三态——**应当一一映射**，不要退化成 JSON 字符串再让模型自己解析。

#### R11-0 落地顺序与可抄的常量

1. `ra-mcp::client` —— `rmcp` 的 stdio + streamable-http；`initialize` / `tools/list`（分页上限抄 `MAX_MCP_CATALOG_ITEMS = 2_048`，`codex-mcp/src/pagination.rs:10`）/ `tools/call`（超时抄 `DEFAULT_TOOL_TIMEOUT = 300s`，`codex-mcp/src/rmcp_client.rs:92`，可被 server 配置覆盖）
2. `ra-mcp::catalog` —— `McpToolInfo` + 上面的 ①；`ToolFilter` 的 enabled/disabled 名单（`codex-mcp/src/tools.rs:66-96`，30 行，直接抄）；`tool_is_model_visible`（读 `_meta` 里的 UI visibility，`connection_manager/tool_catalog.rs:43`）
3. `ra-mcp::tool` —— `impl ra_core::tool::Tool for McpTool`；`options()` 按 ②③ 生成；`call()` 按上面第 3 点映射结果
4. `ra-mcp::in_process` —— Codex 的 `in_process_transport.rs` 只有 14 行。**进程内 server 极便宜且是测试的地基，应当在 R11-0 里就做**，而不是等 R11-3
5. 连接生命周期（启动事件、复用、懒启动、每 server worker）留给 R11-6 / R11-6b

### R11 非目标

| 项目 | 处理 |
| --- | --- |
| 远程 skill/plugin catalog 服务端 | DEFERRED；先做本地与 Git |
| MCP sampling / roots 全量支持 | 先做 tools/prompts/resources，sampling 后置 |
| MCP OAuth 授权码流 | 记为 R11-2b，依赖 R8-12（凭据外发边界）；R11-0/R11-2 只做 `bearer_token_env_var` + 静态 header |

### R11 验收标准

| 能力 | 标准 |
| --- | --- |
| 传输可用 | stdio / streamable-http 各有 e2e 测试（SSE 若保留则同样要有；不保留就删掉 feature，不留半条） |
| 进程内零 IPC | 宿主注册的工具调用不产生子进程或网络往返 |
| 隔离可信 | `strict_mcp_config` 下磁盘配置完全不生效 |
| 名字冲突可解 | 两个 server 各有一个同名工具时，两个都可调用且路由到正确的 server；模型可见名 ≤ 64 字节 |
| 声明面自动推导 | MCP 工具的 `ToolConcurrency` 来自 `annotations.read_only_hint`、`ToolExposure` 来自 `tool_search` 是否启用，两者都有断言而不是靠人工配置 |
| 结果不退化成字符串 | MCP 返回的 image / resource 落成 `ToolOutputBlock::{Image,File}`，不是被 JSON 序列化进文本块 |

---

## R12 子 Agent 内核机制

> **实证与优先级要分开处理（2026-08-06 定，本版修正）**：Codex 一个 288 轮 / 11,215 次 `exec_command` / 153 MB 的重度真实编码任务里，**结构化 spawn 事件为 0**——`spawn_edges` / `agent_jobs` 三张表全程零激活。CC 侧子代理确实在用（102 次 `Agent`），但只占 6,234 次调用的 1.6%。
>
> **这条实证管的是「编码产品默认 advertise 哪些工具」，不管「框架提供哪些机制」。** 上一版把它外推成 R12 整体后移，是一次错误外推。拆开之后：
>
> | 结论 | 是否成立 |
> | --- | --- |
> | 编码 profile 默认不 advertise 编排工具，省那 4.5 KB schema | ✅ 继续成立 |
> | 「先把单循环做到极致」优于「上多 agent 拓扑」 | ✅ 继续成立——拓扑归 R17，不在本阶段 |
> | 框架可以晚点再提供子 agent 机制 | ❌ **反转**，理由见下 |
>
> **反转的理由是时机，不是价值。** 子 agent 的四件事——审批中断序列化进父 `RunState`、取消传播到子 run 的子进程、父子 usage 合账并受父上限约束、transcript 作为 session subkey 落盘——**全部触碰内核不变量**，产品层做不了（做了就是反向依赖 runtime 内部）。而 R6-6 一旦冻结 `RunState` schema v1，再补这些字段就是一次迁移。
>
> **落地**：R12-1/2/3/5/6/7 前移到 R6 之前，至少 `RunState` 相关字段要在 R6-6 定型时就位（见 R6-6a）。拓扑编排（fan-out / supervisor / debate / handoff 边）全部归 [R17](#r17-编排与图引擎ra-flow)。

### R12 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R12-1 | `AgentDefinition`、注册与 `HandoffSpec` | DONE | 对齐 claude `agents` 选项：`{description, prompt, tools, model}`；可从配置文件、插件、API 三处注册。此处同时在 `ra-core` 冻结协议中立的 `HandoffSpec`：稳定 target `AgentId`、模型侧名称/描述/输入 schema、动态 enable 判定与 `HistoryProjection` 声明；回调读取 R3-9a `RunContext`，而模型输入投影与 Session 权威历史可不同。它只声明线性控制转移，不实现 graph、fan-out 或调度。<br>**已落地**：`HandoffSpec` 在 `ra-core::agent`，`HistoryProjection` 默认 `None`——把调用方的完整 transcript 悄悄交给另一个 agent 既是上下文成本意外也是权限扩张，要给必须写下来；动态可用性回调与动态工具同在准备阶段的取消作用域内解析，取消后不会跑完也不会在模型调用付费之后才生效。`AgentDefinition` 是 `AgentSpec` 的别名而非第二套声明模型：配置文件、插件、API 三条注册路径共用同一个不可变形状。`AgentRegistry` 按 `AgentId` 寻址并拒绝任何 handoff 目标缺失的声明集；直接 run 暂不查它，它是图运行时到来时目标解析的唯一已校验来源。<br>**审核修正**：① tool 名与 handoff 名分属两个校验集合，`build()` 放过了同名组合，冲突推迟到轮内的 `ToolNameCollisionPolicy`（默认 `Warn`）——handoff 赢，那个可执行 tool 每轮被静默丢掉。两者本就投影进同一个 provider 命名空间，且动态可用性只会删条目不会加条目，因此改为共用一个 `advertised_names` 集合在声明期就拒；轮内策略继续负责只有轮才知道的冲突。② `ToolProfile` 的预算只算 tool 选择，handoff 白占 provider 工具表的条目数与 schema 字节——新增 `ActionSurfaceBudget`（`RunConfig` / `TurnPreparationRequest` 可设），在动态可用性之后、模型解析之前对真正要发出去的表计量，两类条目共用 `advertised_definition_bytes` 一把尺。③ `batch.rs` 里「preparation advertises no handoffs 所以分支不可达」的注释已随本条失效：现在声明可达、执行未实现，注释改为说明这一点。<br>**验收**：`it-core/tests/agent_definition.rs` 3 条（strict 模型投影与默认无历史、别名共用声明形状、自转移/重名/空历史窗口三类拒绝）+ `it-runtime/tests/agent_registry.rs` 2 条（稳定身份序解析、重复 id 与未注册目标）+ `it-core/tests/agent_spec.rs` tool/handoff 同名拒绝 + `it-runtime/tests/turn_preparation.rs` 3 条（动态 handoff 进同一 action surface、取消在模型解析前打断、预算把 handoff 的条目数与字节都算进去）。clippy `-D warnings`、`cargo xtask public-api` 与 fmt 全绿。 |
| R12-2 | `Agent::as_tool()` | TODO | 把 agent 包成工具（openai `agent.py:576`）：独立上下文、结果回灌父级。**这是 Codex/CC 的主形态，优先于 handoff**。必须显式区分 `as_tool` 与 handoff：前者接收生成 input、子 agent 完成后父 agent 继续；后者传递/过滤历史并转移控制权。结构化输入可借鉴 `AgentAsToolInput` / `StructuredInputSchemaInfo`，但最终仍转换成协议中立 `InputItem` |
| R12-3 | 嵌套审批镜像 | TODO | 子 agent 内的审批中断要冒泡到父 run 的 `Interruption`，批准后镜像回子 agent（openai `_nested_approvals_status` / `_apply_mirrored_approval`） |
| R12-4 | 子 agent 事件转发 | TODO | 子 agent 的 RunItem 事件按需转发给订阅者（可折叠展示） |
| R12-5 | 并发、工作区隔离与取消传播 | TODO | **R8 重排后的范围（2026-09-16）**：本条涉及 `WorkspaceLease`、writer admission 与工作区隔离的要求属于 Rusty coding 并发扩展，不是上游 sandbox client/session 的先决条件；普通 Runner、sandbox runtime 的上游并发保护及 owned/borrowed 生命周期由 R8-P4 移植。下述扩展约束仅在宿主选择对应产品功能时适用。<br>`AgentPool` 并发上限；父 run cancel 传播到所有子 agent，恢复/取消时不得遗留工作区或进程。<br>**隔离策略由宿主选择，不写死 worktree**（2026-09-09 修正：原文要求「任何可能写入的 agent 必须拿独立 `ExclusiveWrite` worktree」，那是 git 专有策略）。框架提供 R8-11a 的 lease 原语与一个隔离选项，**选项的具体公开类型现在不定死**——先落地「共享 / 串行 / 独立工作区」三种语义，形态等第一个真实产品定。<br>**进 `RunState` 的是 lease 标识与清理责任**；恢复所需的其余信息由隔离实现自己提供并自描述——`base_revision` 只属于 git 实现，不是所有后端都有的字段。<br>**并写冲突不自动合并，且承诺按隔离策略分档**（2026-09-15 修正：原文写「框架不让最后完成者静默覆盖」，与同日修正的 R8-11a「选共享时并发与冲突责任由宿主承担，框架只如实记录谁在持有」直接冲突——共享模式下框架没有任何机制能兑现这句）。`ExclusiveWrite` 与串行策略下，lease 保证参与该租约协议的写入者不重叠；**不保证基于陈旧内容的后续写入不会覆盖已有修改**，也不约束未参与协议的外部写入者。版本冲突检测与发布策略由宿主决定；**共享策略下框架只记录持有者，覆盖与冲突责任在宿主**；独立工作区要到发布/合并阶段才谈得上冲突检测，且「怎样算冲突、要不要 merge node」仍由宿主决定 |
| R12-6 | 预算继承与累计用量 | TODO | **先做三件确定需要的**：跨父子 agent 的**累计** token / 费用 / wall-clock、显式深度上限与防递归、取消传播。<br>**统一 reservation 系统暂缓**（2026-09-09 修正）：把 token、费用、进程、网络调用提前收进一套预留/回收机制过重，且没有产品在驱动它。子 agent 启动前的可回收 reservation 按实际场景逐个增加，不预先建总账 |
| R12-7 | 子 agent 结果返回与轨迹引用 | TODO | **默认支持直接返回子 agent 的结论**（2026-09-09 修正：原文照 CC 的 `Agent.outputFile` 强制「主上下文只留 `agent_id` + 文件指针」——那是一种降低上下文成本的手段，不该固化成子 agent 契约；一个只回一行结论的子 agent 走文件指针纯属绕路）。<br>**完整轨迹的独立存储与按需取回是可选项**：子 agent 完整轨迹可写独立文件并作为主会话 subkey（R9-2 的 `list_subkeys`），父级用 `agent_output` 按需取回；探索型子 agent 开它，问答型子 agent 不开。由产品配置。<br>**独立存储不等于脱离主时间线**：无论选哪种，子 run 的 spawn / complete 与审批冒泡都必须在主时间线上可见 |
| R12-7b | 停滞子 agent 止损 | TODO | 父 agent 可检测子 agent 停滞并发"强制收敛"消息（CC 实测的 `SendMessage` 止损催收），而不是干等到超时 |
| R12-8 | `NextStep::Handoff` 实现 | DONE | **由 `ra-runtime` 在运行循环里独立完成，不依赖 R17 图引擎**（2026-09-09 修正：原先「移至 R17-8」把 SDK 的基础能力压在一个尚不存在的大型子系统之后）。对齐 `openai-agents-python`：handoff 是「换 agent 接着用同一条历史」，上游在 run loop 里就地完成。`HandoffInputData{input_history, pre_handoff_items, new_items}` 与 `HandoffInputFilter` 照上游语义实现——下一个 agent 的**模型输入**可以过滤，但 **session history 保留完整新项**，避免回放重复或丢失。R12-1 已冻结的 `HandoffSpec` 声明是它的输入<br>**已落地**：handoff 类型收进 `ra-core::agent::handoff`（`HistoryProjection` / `HandoffSpec` 平移，新增 `HandoffInputData` / `HandoffInputFilter`）。**目标解析放在准备阶段**：`RunConfig::with_agent_registry` 提供声明集，本轮真要广播的每条 handoff 都在模型调用之前绑定到目标 `Arc<AgentSpec>`，解析不到就拒绝——广播一条跑不了的转移等于花钱买一个死胡同；绑定后的 `PreparedHandoff` 随 `TurnActionSurface` 走到结算，理由与 `ToolRunFunction` 持有 `Arc<dyn Tool>` 相同：转移的必须是本轮广播过的那一条。<br>**两层收窄、一次存储**：声明的 `HistoryProjection` 是上限，`HandoffInputFilter` 在它的产物上再跑，所以宿主 transform 只能更窄——声明扣下的上下文根本不会递给它。两者都只产出模型输入，`session_step_items` 永远是完整集；收窄后的视图装进 `RunState::handoff_projection`（schema v3→v4），否则恢复后的第一轮会把刚扣下的 transcript 原样重发。`RunResult::continuation_input` 同样走投影——它本是被扣下历史唯一会悄悄漏出去的口子。<br>**控制权是单数**：一轮里多条转移按模型顺序取第一条，其余照样给 `handoff_not_performed` 的配对输出（无输出的调用会让下一次请求非法）；本轮若停下来问人则一条都不执行，否则历史会宣称控制权转移到一个还没开始跑的 agent。收尾时输出 guardrail 按**实际交付答案的 agent** 重新合并。<br>**审核修正**：① typed `HandoffCall` 原按目标身份做「最后一条胜出」查找——同一个目标可以有两条不同名、不同投影的声明，身份选不出哪一条，改为按模型实际调用的 wire name 解析，歧义直接拒绝；`advertises_handoff_to` 退回纯谓词，不再兼作查找。② `RunErrorData` 的 `original_input` / `new_items` 文档补上转移后的含义。<br>**未做**：`HistoryProjection::Summary` 在准备阶段拒绝。生成摘要是一次模型调用，结算这一层没有 summarizer；降级成 `Full` 是权限扩张、降成 `None` 是扣下承诺过的上下文，两个都比明说不支持差——要做得把 summarizer 接进结算，属独立一块。<br>**验收**：`it-core/tests/handoff_input.rs` 4 条（Full/None、`LastItems` 倒数且不吃掉转移记录、Summary 拒绝、拍平丢控制面记录并修断裂配对）+ `it-core/tests/run_state.rs` 2 条（投影过 checkpoint、边界记录缺失即拒）+ `it-runtime/tests/agent_handoff.rs` 7 条（控制权转移与会话留全量、Full、filter 再收窄、投影治下一个 segment 与 `continuation_input`、交付按收尾 agent 查、注册表缺口、Summary 拒绝）+ `it-runtime/tests/turn_settlement.rs` 3 条（转移结算、多条转移、中断压过转移）+ `it-runtime/tests/response_classification.rs` 2 条（同目标按名区分、歧义/缺名/错名拒绝）。`cargo xtask all` 九条门禁全绿。 |
| R12-9 | 批量 fan-out | **移至 R17-4/R17-7** | 并行 fan-out 与 join 是调度器的职责，归 `ra-flow::Scheduler`；R12 只负责「单个子 run 能被正确启动、审批、取消、计账」 |

### R12-A 本地 Codex 多 Agent 源码对照与采纳裁决（2026-08-11）

> 证据基线同 R8-A：`/Users/moses/workspace/custom-app/codex`，提交 `070a26a1f0`。Codex 的实现已经证明：multi-agent 不是一个 `spawn_agent()` 函数，而是以 root-scoped control plane 管理的、可恢复的 thread tree。Rusty 应借鉴其**控制面不变量**，但继续以 R12（单子 run 正确性）+ R17（拓扑调度）拆分实现。

| Codex 源码与行为 | Rusty 采纳位置 | 裁决与验收不变量 |
| --- | --- | --- |
| `core/src/agent/control.rs` 的 `AgentControl` 由同一 root session tree 共享；以 `Weak<ThreadManagerState>` 断开 `manager → session → control → manager` 引用环 | `ra-runtime` 的 `AgentControl` / `RunServices` | **采纳**。一个 root run tree 只有一个 control plane；父子共享注册表、预算和取消根，但不把全局 `ThreadManager` 强引用塞进每个工具上下文 |
| `core/src/agent/registry.rs` 维护 canonical `AgentPath`、thread metadata、spawn reservation、总数和最大深度 | R12-1/R12-5 | **采纳**。使用不透明 `AgentId` + 可解析路径，spawn 先 reservation 再启动，失败 RAII 归还；同时限制 total agents、depth、active execution，不能只限制并发数 |
| `agent/control/execution.rs` 以 guard 计活跃执行；`residency.rs` 以 LRU 卸载可安全退出的 idle thread | R12-5、R9 | **采纳原则，R12 初版简化**。先做并发 admission + 无泄漏 cancel/drain。不得通过直接 drop task 来“省内存”。<br>**淘汰的因果顺序按源码修正（2026-08-11 核对 `residency.rs:117-142`）**：不是「只有已 materialize 的才允许淘汰」，而是**先判可卸载、再主动 flush、然后卸载**——`is_unloadable()` 要求三件事同时成立（状态是 `Completed \| Errored \| Interrupted`、无活跃 turn、邮箱无待投递项），通过后调 `ensure_rollout_materialized()` 把它刷下去，失败则中止本次淘汰。差别不是措辞：前者是一个只读谓词，后者要求淘汰路径里有一次真实的 flush 调用和它的失败分支 |
| `agent/control/spawn.rs` fork parent history，保存 parent thread/source/depth/config/environment/policy 继承；可从 rollout 恢复 | R12-2、R9-7、R9-12 | **采纳**。`SpawnRequest` 必须显式选择 `HistoryProjection`（full / last-N / summary / none）、配置继承与 workspace lease；默认值不能隐式把完整父历史、权限或秘密扩大给子 agent |
| `protocol::InterAgentCommunication{author, recipient, other_recipients, content, trigger_turn}` 与 `AgentPath` 路由 | R12-3/R12-4、R9-0 | **采纳**。父子/同级消息是结构化 event，有 author/recipient/call_id/turn 归属；`trigger_turn=false` 只是投递消息，`true` 才申请一个子 turn 的预算和并发名额 |
| `CollabAgentSpawn*`、`CollabAgentInteraction*`、`SubAgentActivity*` 事件与 `AgentStatus` | R12-4、R13、R14 | **采纳**。定义稳定的 `AgentSpawned`、`AgentMessageSent`、`AgentStatusChanged`、`AgentCompleted`、`AgentClosed` 事件；UI 是事件消费者，父 agent 的模型上下文只接收明确投影，不能把所有活动日志塞回 prompt |
| `close_agent` / `shutdown_agent_tree` 先 materialize/flush，再递归关闭 live descendants、释放 registry/residency | R12-5、R9-9 | **采纳**。关闭顺序：停止接收新输入 → 向下取消 → 等待 drain → 终止子进程/释放 lease → flush checkpoint → 释放 registry/reservation；每步应幂等，可从 crash recovery 重试 |
| `session/multi_agents.rs` 根据版本、session source、effort 选择 explicit/proactive/custom mode，并默认禁止 subagent 再继承 multi-agent mode | R10 profile、R12-6 | **有选择采纳**。Rusty 默认 `ExplicitOnly`，只有宿主/产品显式启用的 profile 才允许自动委派；子 agent 的再委派需单独 capability 与深度/预算授权，不能因继承 prompt 自动无限递归 |

#### R12/R17 的实现顺序约束

1. **先 R12 机制，后 R17 拓扑。** 先证明单个子 run 的身份、审批、取消、预算、持久化与 workspace lease 正确，再让 `supervisor` / `fan_out_join` / `debate` 同时启动多个。
2. **工具接口只是控制面的投影。** `agent.spawn` / `send_message` / `wait` / `close` 可以在 `ra-tools` 以 namespace 暴露；真正的 `AgentControl`、`AgentRegistry`、预算 reservation 和生命周期必须在 runtime/service 层，不能由工具实现自管 HashMap。
3. **线程树与工作区树都要可恢复。** R9 要持久化 parent/child edge、agent path、状态、budget reservation、`WorkspaceLeaseRef` 和未完成控制请求；对依赖旧 R8-11a 保护的 coding 产品配置，该扩展未完成前不默认允许多个可写子 agent 并行；不作为上游 sandbox runtime 的统一限制。
4. **不要照搬 Codex 的 V1/V2 兼容分支与桌面事件协议。** Rusty 从一个版本化事件/RunState schema 起步；需要的是其状态机与资源清理纪律，不是历史兼容包袱。

### R12-B Codex multi-agent 集成分层（冻结建议）

> **原则：工具是控制面的薄投影，`ra-flow` 是调度器，只有 `ra-runtime` 持有 live agent tree。** 因此既不能让 `ra-tools` 自己维护 agent HashMap，也不能让 `ra-flow` 直接操作 session、worktree 或子进程。这样分层可覆盖 Codex 的 root-scoped `AgentControl` 能力，而不会把 Codex 的产品 `Session` / `ThreadManager` 复制进框架。

```text
模型 / 宿主 API
  │ agent.spawn / send_message / wait / close
  ▼
ra-tools::agent                 只做 schema、参数校验、模型可见 ToolOutput
  │ 调用受类型约束的 AgentControlPort（不可 as_any downcast）
  ▼
ra-runtime::agent::AgentControl root run tree 的 live registry、状态迁移、审批/取消/预算 admission
  ├── ra-session 的 AgentGraphStore  持久 edge、checkpoint、subkey、恢复所需状态
  ├── ra-exec 的 WorkspaceLeaseManager  分配/回收 ReadOnly 或 ExclusiveWrite 工作区（worktree 只是其一种实现）
  ├── ra-runtime::RunnerFactory  启动/恢复 child run，并挂到父 CancelScope
  └── HostEvent::Agent(..)      UI/trace 事件族，走 R8-0 的统一信封；不是模型上下文
  ▼
ra-flow::Scheduler              只决定拓扑、并发、join、edge；通过 AgentControl 启动 AgentNode
  ▼
ra-coding / ra-assistant         定义角色、profile、委派策略与是否 advertise agent namespace
```

| 层 / crate | 必须拥有 | 明确禁止拥有 |
| --- | --- | --- |
| `ra-core::agent` | `AgentId` / `AgentPath` / `AgentStatus` / `SpawnRequest` / `HistoryProjection` / `AgentTreeSnapshot` 等可序列化值对象；`AgentControlPort`、`AgentGraphStore`、`WorkspaceLeaseManager` 等**端口 trait**（经 `ToolServices` 递给工具，见下）；agent 事件族的**载荷**（信封是 R8-0 的 `HostEvent`） | Tokio task、HashMap live registry、文件/数据库 I/O、模型或产品策略 |
| `ra-runtime::agent` | root-scoped `AgentControl`、`AgentRegistry`、并发/深度 limiter、budget reservation、取消与审批镜像、幂等状态机、`RunnerFactory` 调用 | SQL/JSONL 实现、worktree/sandbox 实现、拓扑专属的 supervisor 规则 |
| `ra-session` | `AgentGraphStore` 实现：parent-child edge、operation id、状态、subkey 指针、checkpoint 与恢复查询 | 运行中的 task 句柄、预算或 lease 的最终决策 |
| `ra-exec` | `WorkspaceLeaseManager` 实现、工作区物化/清理（git worktree、容器卷、临时副本各是一种实现，框架不预设哪一种）、子进程归属与终止 | agent role、历史投影、图的路由判断 |
| `ra-tools::agent` | `agent.*` namespace 的输入/输出 schema 与薄适配；经 `ToolContext` 的 `ToolServices` 取显式 `AgentControlPort` 访问控制面 | 应用上下文 downcast、自行 spawn `Runner`、自管 registry/budget/lease |
| `ra-flow` | `AgentNode`、supervisor/fan-out/pipeline/debate 的图定义、调度、join/barrier、图级 budget admission | 直接读写 session store、自行物化工作区（含 worktree）、绕过 `AgentControl` 启动 child run |
| `ra-coding` / `ra-assistant` | AgentDefinition、角色 prompt、工具/profile allowlist、委派触发策略和产品验收 | 运行时状态机、跨产品的资源清理或持久化实现 |
| `ra-protocol` / UI | 控制请求与事件订阅的 transport/渲染 | 把 UI 事件当作 agent 的权威状态或模型输入 |

#### 必须采用的控制与持久化交界

1. agent 工具通过**显式端口**而不是应用上下文 downcast 访问控制面；未安装能力时返回明确的“profile 未启用”工具结果。这样 `ra-tools → ra-core ← ra-runtime` 不形成依赖环，`ra-tools` 也不必认识任何产品的具体类型。<br>**但落点是一个 `ToolServices` 袋子，不是 `ToolContext` 上并排的第 N 个字段**（2026-08-11 定）：`AgentControlPort` 是**第二个**这样的端口——R3-13 刚为 `WorkStateHandle` 走过一遍，而本节自己就已经能看见第三、第四个（`WorkspaceLeaseManager`、budget reservation）。R3-13 记下的代价是那条透传链有**四层**：`RunRequest → TurnSettlementRequest → TurnExecutionRequest → ToolDispatchRequest → ToolContext`，每加一个端口就要重走一遍，而且第三方工具的 `Tool::call` 签名也在这条链上。**第二个端口出现的时刻，正是判断第一个是模式还是特例的时刻**：把端口收进一个 `ToolServices`（内含各端口的具名访问器），四层只传它，之后加端口只改一个结构体。<br>**刻意不做 `TypeId` 键的通用 service lookup**：Rust 里从 `dyn Any` 拿回 `dyn Trait` 需要每个 trait 一份注册胶水，最后会退化成应用上下文 downcast 加几层壳——正是这条要消灭的东西。具名访问器 + 一个袋子是这门语言里的正解。<br>`work_state` 的迁移趁早：今天袋子里只有它一个。
2. `AgentRegistry` 是 live cache，`AgentGraphStore` 是恢复权威记录；两者不互相冒充。每个 spawn/message/close 都带幂等 `AgentOperationId`，崩溃恢复按持久状态重放或补偿。
3. spawn 使用 saga，而非假设跨预算、session、worktree、task 启动存在数据库事务：`Validate → PersistPending → ReserveBudget+Lease → CreateChildRun → PersistRunning → Start`；任一失败按反向顺序释放 reservation/lease、记录终态。恢复器负责收敛遗留 `Pending/Starting/Closing`。
4. child `RunState` 在启动前写入：`parent_agent_id`、`agent_path`、`depth`、history projection、budget reservation、`WorkspaceLeaseRef`、权限/模型的**收紧后快照**、未完成审批/控制请求。父取消只向下传播，child 不得扩大父权限、预算或可写工作区范围。
5. 事件、模型输入、持久化记录三分离：`HostEvent::Agent(..)` 供 UI/trace（信封归 R8-0）；`AgentGraphStore` 供恢复；父 prompt 默认只保留 `AgentRef + 摘要/显式消息`。不可把完整 child transcript 或 UI token 流回灌父上下文。

#### 本节哪些必须现在冻结，哪些是后续实现

R12 的定位是「只提供内核机制，拓扑归 R17」。上面这套东西按现在的写法读下来会让 R12 长成一个小号 R9，所以**划一条线**——判据与 R6-6a 同一条：**schema 冻结之后再补就是一次迁移的，现在就得定；其余是实现，可以排到后面。**

| 现在就要冻结（契约） | 理由 |
| --- | --- |
| child `RunState` 的那组字段（上面第 4 条） | R6-6 冻结 `RunState` schema v1 之后再加就是迁移，这正是 R12 前移到 R6 之前的全部理由 |
| 状态机的 enum 取值与「业务终态 / 资源终态」的划分 | 它会进 checkpoint 与 `AgentGraphStore`，是持久化形状 |
| `AgentControlPort` / `AgentGraphStore` / `WorkspaceLeaseManager` 三个 trait 的签名，以及 `ToolServices` 的形状 | 它们横穿四层透传链与第三方 `Tool::call`，晚了要动别人的代码 |
| `HistoryProjection` 的取值（full / last-N / summary / none）与默认值 | 默认值决定「子 agent 会不会隐式拿到父的完整历史与权限」，是安全边界不是调参 |
| `AgentOperationId` 的存在与幂等语义 | 恢复时按它去重；事后补等于所有已存记录都没有它 |

| 可以后置（实现） | 排到哪 |
| --- | --- |
| saga 执行器与反向补偿 | R12-5b；初版可以是「顺序执行 + 失败即刻反向释放」，不要求崩溃后可续 |
| 崩溃恢复收敛器（收敛遗留 `Pending/Starting/Closing`） | 跟 R9-12 的对账一起做，那时才有真实的持久化记录可收敛 |
| residency LRU 淘汰 | R12-A 已写明「初版简化」，先做并发 admission + 无泄漏 drain |

#### 与 Codex 对齐的最小状态机

`Created → PendingPersistence → Reserving → Starting → Running → {Completed | Failed | Cancelled | Rejected} → Closing → Closed`

- **标清楚哪些落盘**：`Created` 是持久化之前的内存态，**不可能出现在 checkpoint 里**，所以它不进持久化枚举——否则恢复路径上会多一条不可达的 `match` 臂，而且没人说得清它是「不可达」还是「漏了」。`PendingPersistence` 起的每一个状态都要落盘，`Reserving` 尤其不能省：崩溃后就是靠它知道有一笔预算/lease 需要回收。
- `Completed / Failed / Cancelled / Rejected` 是业务终态；`Closing / Closed` 是资源清理终态，二者不可混淆。
- `close`、父取消、超时、崩溃恢复都走同一 idempotent close path：停止新输入 → 向下取消 → drain → terminate process group → release lease/reservation → flush graph/session → `Closed`。
- R17 的 join 只接受业务终态结果；资源是否已经 `Closed` 由 closeout/恢复器单独断言，防止“报告完成但 worktree/进程泄漏”。

### R12 非目标

| 项目 | 处理 |
| --- | --- |
| 在 R12 做拓扑编排 | 不做；supervisor / fan-out / debate / handoff 边全部归 R17，R12 只提供内核机制 |
| 把子 agent 机制放进产品层 | **不做**；那四件事触碰内核不变量，放产品层只能反向依赖 runtime 内部 |
| 多 agent 自动分工/角色推断 | 不做；子 agent 由显式定义与显式调用 |
| 编码 profile 默认 advertise 编排工具 | 不做；机制在框架里，是否 advertise 由 profile 决定（R2-4 / R10-4） |

### R12 验收标准

| 能力 | 标准 |
| --- | --- |
| 子 agent 可审批 | 子 agent 内的危险操作能冒泡到宿主审批并正确恢复 |
| 取消干净 | 父 run 取消后无残留子 agent、无残留子进程 |
| 预算不穿透 | 子 agent 消耗计入父 run 总账并受总上限约束 |

---

## R13 控制协议与 App Server

### R13 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R13-1 | 帧协议定义 | TODO | `control_request` / `control_response` / `control_cancel_request` / 消息帧 / `transcript_mirror`（对齐 claude `_internal/query.py` 的路由）。`request_id` 关联 + 在途请求可取消 |
| R13-2 | transport：stdio | TODO | 行分帧（`LineFramer` + `flush`）+ `max_buffer_size` 上限（超限报错不无限吃内存）+ stderr 独立通道 |
| R13-3 | transport：WebSocket | TODO | 同一套帧；本地 token + client allowlist |
| R13-4 | 初始化握手 | TODO | `initialize` 请求携带 hooks 配置、agents 定义、skills 白名单；响应返回能力清单与版本 |
| R13-5 | 反向控制请求 | TODO | 宿主侧实现 `can_use_tool` / `hook_callback` / `in_process_tool_call`，harness 反向调用 |
| R13-6 | 运行中动态控制 | TODO | `interrupt` / `set_permission_mode` / `set_model` / `rewind_files` / `stop_task` / `mcp_reconnect` / `mcp_toggle` / `get_context_usage` / `get_mcp_status`（claude client 的完整方法集） |
| R13-7 | 生命周期正确性 | TODO | **`result` 帧 ≠ run 结束**：在途任务集合非空时不关输入通道；只有能可靠到终态的任务类型（子 agent / workflow）才允许延迟收尾——background shell / 长驻 monitor 不算（claude `DEFERRING_TASK_TYPES` 的血泪注释） |
| R13-8 | 事件订阅与拉取 | TODO | `run.subscribe` 流式订阅 + 断线后按 offset 补拉；订阅滞后（lagged）时的恢复语义 |
| R13-9 | 线程 / 运行时 API | TODO | thread CRUD、run 启动/暂停/恢复/取消、审批 API、trace 查询 |
| R13-10 | 连接生命周期硬化 | TODO | 心跳探活、自动重连、重连后刷新与重订阅、请求默认超时与长操作 override（原引 AF P38，出处按裁决删除；这几件是通用连接治理，条目本身保留） |
| R13-11 | 结构化输出 | TODO | `output_format: {type: json_schema, schema}`；最终结果按 schema 校验 |

### R13 非目标

| 项目 | 处理 |
| --- | --- |
| 远程多租户 / 设备配对 | DEFERRED |
| gRPC / protobuf | 不做；JSON 帧足够，且便于调试与跨语言 |

### R13 验收标准

| 能力 | 标准 |
| --- | --- |
| 双向可用 | 宿主能被 harness 反向调用做审批与 hook，且在途请求可取消 |
| 不早关 | 有后台任务/子 agent 时收到 result 不关通道；有回归测试 |
| 断线可恢复 | 拔网重连后事件不丢、不重复 |

---

## R14 Eval / Replay 飞轮

### R14 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R14-1 | 确定性 fixture 与 mock provider | TODO | 脚本化 provider 响应；同一 fixture 多次跑结果一致 |
| R14-2 | Trace 记录与回放 | TODO | 完整 run 的结构化 trace（请求/响应/工具/guard/NextStep 序列）可导出与回放。借鉴 `SpanData`（Agent / Task / Turn / Generation / Response / Function / Handoff / Guardrail / MCP）、嵌套 `Trace` / `Span`、`TraceState` 可恢复上下文，以及可替换 `TracingProcessor` 的 `on_*` / `force_flush` / `shutdown`；`trace_include_sensitive_data=false` 时保留 span 结构但不落输入输出 |
| R14-2b | **Run grouping** | TODO | 对齐 openai `run_internal/run_grouping.py`：多次 `Runner::run` 归到一个 `RunGroupId`（默认取 session id，可显式指定）。**R17 直接依赖这条**——一张图跑下来是 N 次独立 run，没有共同 group id 就在 trace 里聚合不成一张图，「实际路径 == 期望路径」的断言也无从做起。注意 `RunGroupId` 与 `trace_id` 是两个东西：单次 run 内可以有多个 trace |
| R14-3 | trace 结构断言 | TODO | 断言的对象是**契约正确性与任务结果**：`NextStep` 序列合法、每个 tool call 有配对输出、`FinishReason` 与实际收尾一致、结构化输出满足 schema。<br>**不断言工作习惯**（2026-09-09 修正）：原文要求 trace 里存在「报幕 → 工具 → 观察 → 自纠 → 验证 → 结论」的证据链——那是一种解题风格，不是正确性判据，一个直接答对的 run 不该因此判失败。具体工作路径若要看，归产品自己的评估集 |
| R14-4 | 拒绝与熔断指标 | TODO | 为保留的机制建立指标：输入/输出护栏统计检查与 tripwire，工具护栏统计 `allow` / `reject_content` / `raise_exception`，决定型 hook 按事件统计阻断与后续执行结果。护栏及 hook 当前尚待 R7 实现，不能计作现有拒绝路径。当前熔断码 `tool.no_progress` 可纳入统计；`tool.blocked` 随 R7-9 实现一并回退，不作为现有指标，是否恢复另行决定。<br>指标从实际记录的结构化运行、护栏、hook 与工具事件派生；工具事件本身不足以覆盖输入拒绝或 Stop hook 等没有工具调用的路径。不新增运行时纪律账本，也不把指标变成默认阻断。 |
| R14-4a | **交付质量观察指标** | TODO | 顶部的验证状态裁决把「未来 A/B 证明降低 false completion」列为复议条件，本条负责让那句话有数可依。<br>**只作为观察指标，不单独判定虚假完成**（2026-09-09 修正：原文把「改后没有验证类调用」「最后一次验证失败」机械算作 false completion——两者都不成立。如实披露「我改了但没跑测试」的 run 是诚实的，不是虚假完成；跑过命令也不证明任务做对了）。<br>**采集**：成功收尾的 run 里，改动后有无观察类调用、末次验证的退出状态、final 里有无对未验证的披露，三项分别统计。**判定虚假完成需要独立的正确性判据**（人工标注或任务自带的验收），上面三项只是与它做相关性分析的输入 |
| R14-5 | 成本与资源指标 | TODO | 每 run 的 total token / uncached / **cache_hit_rate** / 轮数 / 工具调用数；对子 agent / graph 另计 reservation、累计费用、wall-clock、峰值并发、存活子进程数、MCP/网络调用数与取消 drain 时间；设 gate 阈值，并能区分任务失败、模型行为失败与预算/资源治理失败。 |
| R14-6 | 真实 provider 门禁 | TODO | 分阶段：mock 全绿 → 单 provider pilot → confirmatory。**样本量按指标定，不设统一数字**（2026-09-09 修正）：原文的 `n≥3` / `n≥10` 对所有默认值变更一刀切，缺少针对具体指标方差的依据。改为实验指导——变更前写明要动的指标、预期效应量与判据，由该指标的观测方差决定样本量。「一次跑不能当结论」这条保留 |
| R14-7 | 对照任务集 | TODO | 覆盖：单文件修改、跨文件依赖闭环、超大文档定位、只读分析报告、多交付物产出、失败恢复、后台长跑、多 agent 汇总 |
| R14-7a | **对抗、安全与多 agent E2E 套件** | TODO | 在 R14-7 的固定任务集上追加：两个 writer 的 worktree 冲突与显式 merge、父图/子 run 总预算耗尽、kill/restart 后 lease 与子进程回收、恶意网页/MCP/skill/plugin 输出尝试改变控制指令、未经授权的外发、secret 进入 trace/UI/模型输入。fixture 同时断言功能结果与安全不变量（无越界写、无未授权外发、无 Secret 落盘）；对提醒或 guard 的默认启用采用 AgentForge P67 式的 shadow/control/treatment 真实 provider A/B 门禁，收益、额外 token、误伤和 replay 一项不达标则不升级默认策略。 |
| R14-8 | 失败沉淀为回归用例 | TODO | eval 失败自动生成回归用例草稿 + 归因标签（原引 AF P39，出处按裁决删除，条目本身保留） |
| R14-9 | 跨版本回归锁 | TODO | prompt 段、工具 schema、`NextStep` 序列进快照回归。**纪律指标已移除**（随 R7-6/7/8 撤销） |

### R14 非目标

| 项目 | 处理 |
| --- | --- |
| 复刻第三方 benchmark 的绝对分数 | 不做；目标是**自身可度量的收敛**与对照趋势 |
| 声称与 Codex / Claude Code 字节级等价 | 不做；第一方隐藏 prompt、服务端摘要、模型 snapshot 是有界未知 |

### R14 验收标准

| 能力 | 标准 |
| --- | --- |
| 可复现 | 同 fixture 连跑 5 次结果一致 |
| 可归因 | 一条失败 trace 能指出失败发生在哪个环节 |
| 可门禁 | 成本与纪律指标有阈值，回归时 CI 失败 |
| 安全与编排可信 | R14-7a 的对抗 fixture 证明：writer 不共享可写工作区、图总预算不可穿透、取消后无 lease/进程泄漏，且不可信内容不能越权外发或泄露 Secret |

---

## R15 输出成型与改后披露

> **分层（2026-09-09 修正）**：本段绝大部分是**产品的提示与呈现层**，不是框架机制。输出模式、验证披露、固定槽位属于某个产品对自己 final 的要求，随产品换而换，因此落在 `ra-coding` 一类的产品 crate，**框架不提供、也不要求任何产品提供**。<br>属于通用机制、必须在框架里的只有三样：`FinishReason`（run 为什么停）、结构化结果（`OutputValue` / output schema）、以及**显式续跑接口**（宿主拿到一个未完成的 run 能接着跑）。R15 的验收只对这三样负责。

### R15 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R15-1 | 工具历史驱动的验证摘要 | TODO | final 成型从当前 run 的 Session / 工具事件提取实际编辑、执行过的命令及结果。它是一次性呈现投影，不持久化、不绑定 criterion、不成为权威状态。<br>**已知限制：压缩之后这条投影的可靠性依赖 summary 质量，这是本次裁决换来的代价，写在这里不是为了推翻它。** 被删除的验证账本原本承担一件事——让「改了但没验证」熬过压缩。现在验证事实的权威在 `Session`，但**模型看的是上下文投影**，而 R5-3 会压缩它：压缩之后这个事实是否还在，取决于 9 段式 summary 的第 4 段（Errors and fixes）与第 8 段（Current Work）有没有抓住它。这是**摘要质量属性，不是机制保证**。<br>两条后果要认下来：① **R5-3 的摘要规格因此多背了一份责任**——它现在是"改后未验证"唯一的跨压缩载体，R5-3 的验收里应有一条针对该事实的保留断言（**已落地**：`an_edit_made_but_not_yet_verified_survives_into_the_summary`，断言该事实渲染后仍分别落在第 4 段与第 5 段之间、第 8 段与第 9 段之间，把两段并成一坨散文会挂）；② final 成型若发生在压缩之后，`Session` 仍是权威、可回查，所以 R15-1 应**直接读工具事件历史而不是读模型上下文**——这条不做，长 run 的 final 会随摘要一起失真。R14-4a 度量这个失真的实际发生率 |
| R15-2 | 改后验证提醒与诚实披露 | TODO | 最后一次持久化编辑后，可向模型发送一次软提醒以选择 targeted verification；未验证、验证失败或环境受阻必须在 final 中如实说明。提醒不阻止收尾、不触发自动续跑 |
| R15-3 | Closeout / auto-continuation 实验 | **DEFERRED（默认不做）** | 不移植 AgentForge P46 的 fast path，也不依据验证状态实现 closeout gate。只有 eval 先证实尾随只读工具造成真实空转，且 A/B 证实修复不增加 token、轮数或假完成时，才单独立项 |
| R15-4 | Final answer 模式 | TODO | `Minimal` / `Engineering` / `EvidenceBrief` / `AnalysisReport` / `ArtifactSummary`；按任务类型选择，只作用于呈现层，不改 loop/closeout/verification（原引 AF P60/P62，出处按裁决删除；五档的取舍属 Rusty 自己的设计选择，要不要这么分由 R14 的度量决定） |
| R15-5 | 事实型 final 槽位 | TODO | final 的“改了什么 / 实际怎么验证 / 剩余风险 / 未完成项”从工具历史与模型当前上下文组织；仅检查已声称的命令是否存在于历史，不推断 criterion 覆盖率，也不硬拦 |
| R15-6 | Bounded continuation | TODO | 预算/轮次耗尽时的有界继续：明确剩余工作、显式续跑而非无声中断 |
| R15-7 | 诚实呈现 | TODO | 部分完成、被阻塞、降级执行必须在 final 里明说；**测试失败要贴输出，跳过的步骤要说明** |

### R15 非目标

| 项目 | 处理 |
| --- | --- |
| 用词表判断"用户是否要详细回答" | 不做；用结构化 `OutputVerbosity` + 任务类型（AF 已把这条词表删掉） |
| `VerificationLedger` / criterion-evidence 映射 / 文件版本表 | 不做；Session 与工具事件是唯一事实来源，摘要只按需导出 |
| 默认自动验证一切或验证状态驱动自动续跑 | 不做；模型自选 targeted verification，框架只做一次软提醒与 final 诚实披露 |
| 直接移植 AgentForge P46 closeout fast path | 不做；它解决的是其 runner 的特定空转问题，Rusty 必须先有同类实测证据 |
| 逐字复刻第三方输出长度 | 不做 |

### R15 验收标准

| 能力 | 标准 |
| --- | --- |
| 不复制事实 | 不出现验证账本、criterion 映射或文件版本表；验证摘要可从同一段工具历史重建 |
| 不额外续跑 | 改后提醒不触发自动工具调用或 provider continuation；未验证只影响 final 披露 |
| 诚实 | final 中声称已运行的命令必须能在工具历史中找到；失败/部分完成/降级在 final 中可见，有回归用例 |

---

## R16 CLI / 桌面产品面（DEFERRED）

在能力主线（R0-R15）达标后推进。子任务清单：CLI REPL、`doctor`（config / sandbox / prompt / mcp / provider）、桌面 timeline（工具过程折叠、diff 卡、审批卡、todo 清单、context compacted 降噪）、命令面板、@mention 文件引用、历史回放。参考 AgentForge P16/P17/P20/P23/P29-P36 的成品结论，**不重走 UI 试错路径**。

---

## R17 编排与图引擎（`ra-flow`）

> **阶段状态（2026-09-09）：整段暂缓，作为未来的独立扩展。** channel/reducer、调度器、图级 checkpoint、资源预留与预置拓扑合起来是另一个大型框架，当前没有任何产品需求驱动它。**它不得成为基础 agent、多 agent 或 session 的前置条件**——原先压在这里的 `NextStep::Handoff` 已退回 R12-8 由 `ra-runtime` 独立完成。<br>**恢复条件**：真实产品出现**重复的**编排需求，用现有方式（`Runner` + 子 agent + 普通控制流）表达时产生**可验证的**维护或恢复成本（例如同一套失败恢复逻辑在多处重写、跨节点续跑要各自造轮子），且独立图组件相对这些成本有明确收益。<br>**恢复时不继承既有方案**：R17-4a 里的强制 worktree 与统一 reservation 系统随 R12-5 / R12-6 一起撤销，不作为恢复开发时的既定设计。
>
> **定位：可复用件，不是内核。** `ra-flow` 只消费 `Runner` 的公开 API，不碰 turn 结算内部；`ra-runtime` 反向零依赖，由 `cargo tree` 门禁（R0-6）保证。
> **与 R12 的关系**：R12 提供「跑一个子 agent」的内核机制（审批冒泡 / 取消 / 计账 / 隔离），R17 提供「按什么拓扑跑一堆」的调度。**R12 是 R17 的前置。**
> **口径提醒**：R2 的工具面预算（15 入口 / ≤20 KB）是 **coding profile 的策略**，不是框架上限。图编排类产品可以有自己的 profile 与预算，不受那两个数字约束。
> **ReAct 不在本阶段**：`ra-runtime` 的 loop 本身就是 ReAct，它不需要图引擎，也不应该被图引擎包一层。图里的 `AgentNode` 内部跑的就是原样的 ReAct 循环。

### R17 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R17-1 | **`WorkState` 通道模型** | TODO | typed channel + reducer（`Append` / `LastWriteWins` / `Merge(fn)`），每个 channel 带 `version` 与 `provenance{node_id, run_id, turn}`。**这是图与 plan-execute 的公共地基**——没有它，节点之间只能传自然语言，等于把控制流交还给文本（违反 R7-10）。**归属要分清**：`WorkState` 这个**可序列化值对象放 `ra-core`**（否则 `ra-session` 为了 R9-15 落盘就要依赖 `ra-flow`，破坏依赖方向）；**reducer 注册表与类型化读写放 `ra-flow`**。Rust 形态：`HashMap<ChannelId, ChannelValue>`，类型化访问走 `state.get::<T>(ch)` / `state.apply::<T>(ch, v)`。挂载点在 R3-13 已预留 |
| R17-2 | `Node` 与 `NodeOutcome` | TODO | `Node::run(ctx, &mut WorkState) -> NodeOutcome`。三类节点：`AgentNode`（一次 `Runner::run`）/ `FnNode`（纯函数，不调模型）/ `SubGraph`。`NodeOutcome::{Goto(NodeId), Fan(Vec<NodeId>), Halt(FinishReason), Interrupt(items)}`——**照 `NextStep` 的收口纪律：单一出口，禁止 early return**。`Node` 是第三方实现的 extension point，方法带默认实现 |
| R17-3 | `Edge` 条件路由 | TODO | 边的判据**只读结构化值**：`FinishReason`（R3-1b）、`OutputValue`（R1-16）、`WorkState` channel。**禁止文本匹配**，与 R7-10 去词表化同一条纪律，并进同一个 lint。路由函数返回 `Option<NodeId>`，无匹配走**显式 `default_edge`** 而不是隐式落空 |
| R17-4 | `Scheduler` | TODO | 并行分支、join/barrier（`all` / `any` / `quorum(n)`）、map-reduce fan-out（收编原 R12-9）、单节点超时、并发上限、取消传播（复用 R0-4 取消树）。**父图取消必须能杀到孙子 run 的子进程**——这条要有 e2e 测试，是最容易漏的一处泄漏 |
| R17-4a | **图级资源治理与可写调度** | **DEFERRED（随 R17，方案作废）** | **恢复时重新设计，不继承下面的历史方案。**<br>**恢复后要解决的问题**：图跑起来之后，谁来对整张图的用量设上限、谁来判断两个节点能不能同时写同一个工作区。<br>**历史方案（2026-09-09 作废，仅存档）**：曾定为 `GraphBudget` 作整张图唯一的 admission authority，对 token / 费用 / wall-clock / 峰值并发 / 子进程 / 网络调用设总上限并预留 R12-6 预算；调度器验证 `WorkspaceLease` 兼容性，共享 workspace 的 writer 一律拒绝、writer 只能在独立 worktree 上并行。作废原因：统一 reservation 系统随 R12-6 撤销，强制 worktree 随 R12-5 撤销，两者都是这份方案的地基 |
| R17-5 | `GraphCheckpoint` 与恢复 | TODO | 图级快照 = `WorkState` + 每个在跑节点的 `RunState` + 调度器游标 + 预算 reservation + `WorkspaceLease` 生命周期状态，落盘复用 R9-15。**图级 pause/resume 与 R6 审批中断是同一套机制**：子 run 的 `NextStep::Interruption` 冒泡成 `NodeOutcome::Interrupt`，批准后从断点续跑，**不重跑已完成节点**；若 worktree 已丢失或 base revision 漂移，恢复必须 fail-closed 并要求显式重新物化。 |
| R17-6 | **Plan-and-Execute 预置拓扑** | TODO | `Plan{steps[].{id, intent, status, depends_on, acceptance}}` 作为 `WorkState` 的一等 channel；planner 节点产出**结构化 plan**（走 R1-16 `OutputSchema`，不解析自然语言）→ executor 逐步消费 → 失败或偏离走 **replan 回边**。**与 R2-8 的 `update_plan` 工具是两回事**：那个是给模型看的板子（投影），这个是驱动执行的数据（控制流） |
| R17-7 | **Multi-agent 预置拓扑** | TODO | `supervisor`（路由到 worker 后回收结论）/ `fan_out_join`（只读任务并行 + 归并）/ `pipeline`（顺序交接）/ `debate`（多轮互评后仲裁）。每个拓扑就是**一份 `Graph` 定义 + 一组默认 reducer，不是新机制**——机制全在 R12 与 R17-1..5 |
| R17-8 | handoff 在图里的表达 | TODO | **不再收编 R12-8**：handoff 的运行行为由 R12-8 在 `ra-runtime` 完成，本条只负责让一条 handoff 在图拓扑里也能被表达为带 `HistoryProjection` 的边，使图引擎（若启用）能调度它。**基础 agent、多 agent 与 session 都不得依赖本条** |
| R17-9 | 图的可观测性与导出 | TODO | 每个节点进出、每条边的判据取值、`WorkState` 每次写入都进 R9-0 的 `event_msg` 通道；`trace_id` / `span_id` 与 R14-2 共用，**多次 run 靠 R14-2b 的 `RunGroupId` 聚合成一张图**。eval 能断言「**实际走过的路径 == 期望路径**」，这是图类 agent 唯一有意义的回归断言。<br>另外提供 **DOT 导出**（借鉴 openai `extensions/visualization.py`）：`graph.to_dot()` 输出 graphviz 文本，建完图先看一眼比读代码快。**只输出文本，不引入 graphviz 运行时依赖** |
| R17-10 | 图定义的静态校验 | TODO | 建图时校验：不可达节点、无出边的非终止节点、reducer 与 channel 类型不匹配、**环路无退出条件**（必须有 `max_iterations` 或显式收敛判据）。**建图期报错优于运行期烧 token** |

### R17 非目标

| 项目 | 处理 |
| --- | --- |
| 把图引擎塞进 `ra-runtime` | **不做**；`ra-flow` 只用 Runner 公开 API，反向零依赖由 CI 保证 |
| 用图重新实现 ReAct | 不做；ReAct 是 loop 本体，包一层图只增加开销与调试难度 |
| 图定义 DSL / YAML schema | 不做；先用 Rust builder，DSL 等真实需求（且 DSL 会削弱类型化 channel 的价值） |
| 分布式执行（跨进程调度节点） | 不做；单进程内并发，跨进程走 R13 控制协议 |
| 自动图生成（让模型画 DAG） | 不做；图由开发者显式定义，模型只在节点内部决策 |

### R17 验收标准

| 能力 | 标准 |
| --- | --- |
| 四种形态可跑 | ReAct（单 `AgentNode`）/ plan-and-execute / supervisor 多 agent / 条件分支图，各有一个 e2e 用例 |
| 断点可恢复 | 图跑到一半 `kill -9`，恢复后 `WorkState` 与调度器游标一致，已完成节点不重跑 |
| 取消干净 | 父图取消后无残留子 run、无残留子进程（含孙子层） |
| 路由无词表 | 所有边的判据来自结构化值；lint 证明路由代码里没有用户业务语言匹配 |
| 反向零依赖 | `ra-runtime` 的 `Cargo.toml` 里没有 `ra-flow`；`cargo tree` 校验进 CI |

---

## R18 通用助手参考产品（`ra-assistant`）

> **定位：第二个参考产品，与 `ra-coding` 构成对照组。** 它的价值不在产品本身，在于**证伪**——框架里任何为编码场景做的隐含假设，都会在这里暴露成编译错误或难看的 workaround。
>
> **一个消费者只会把抽象拟合到那一个业务上。** `ra-coding` 能跑通，只证明「框架跑得动编码 agent」；只有再加一个特征**刻意相反**的产品，才能证明抽象是通用的。

| 维度 | `ra-coding`（参考产品 A） | `ra-assistant`（参考产品 B） |
| --- | --- | --- |
| 主要动作 | **写**（`apply_patch`） | **读与检索** |
| 循环形态 | 单循环 ReAct | **图 / plan-and-execute / 多 agent 汇总** |
| 工具面 | exec + patch 为核心 | web / read / grep + memory，**无写工具** |
| 纪律 | read-before-edit、改后验证提醒、final 诚实披露 | 引用可溯源、结论与证据对齐、不编造 |
| 上下文重点 | 编辑历史与 diff | 检索片段与来源 |
| 记忆 | 不需要 | **`MemoryStore` 的第一个真实消费者** |
| 输出成型 | 代码变更 + 验证证据 | 结构化报告 + 引用 |
| 权限模式 | 需要审批与沙箱 | 基本只读，审批面极小 |

### R18 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R18-1 | 只读 profile 与工具面 | TODO | 从 `ra-tools` 装配：`read_file` / `grep` / `glob` / `web_search` / `web_fetch` / `ask_user` / `update_plan` / `skill` / `tool_search` / `memory.*`。**不含 `apply_patch`、不含 `exec_command` 的写权限**。工具数与 schema 预算**自定**，不受 R2 的 coding 口径（15 入口 / ≤20 KB）约束——这正是那条口径必须标注「仅 coding profile」的原因 |
| R18-2 | READ-ONLY 表达与 shell 逃逸封堵 | TODO | 复用 R4-8a 的「你没有这个工具，试了会失败」写法 + R4-8b 的逃逸清单（`touch` / `rm` / `mv` / 重定向 / heredoc / `/tmp`）。**这里是验证 R4-8a/b 是否真的是机制的地方**：如果这两条只能在 `ra-coding` 里用，说明它们被写死在编码语境里了 |
| R18-3 | 检索结论的溯源检查 | TODO | 产品要求引用可追溯到 `source_id`。检查分两个边界：① R7-3 工具结果护栏只检查检索结果本身的结构、来源标识与可用证据；此时最终报告尚未生成，不能检查最终结论。② 最终报告的引用完整性、来源对应关系与原文支持程度，由产品配置的 R7-1 输出护栏或离线评估检查，并提供可查阅的来源证据。输出护栏的 tripwire 不自动改写答案或续跑；产品若需要纠正后重答，应显式配置相应续跑流程。来源标识存在不等于原文支持结论。原检索 guard 与共享预算登记表已撤销。 |
| R18-4 | plan-and-execute 拓扑接线 | **DEFERRED（随 R17）** | 用 R17-6：planner 产出结构化 `Plan` → executor 逐步检索 → 证据不足走 replan 回边。**这是 `ra-flow` 的第一个非测试消费者** <br>**暂缓理由**：依赖 R17-6 的预置拓扑。 |
| R18-5 | 多 agent 汇总拓扑 | TODO | 用 R17-7 的 `fan_out_join`：并行子 agent 各查一个子问题 → 归并成一份报告。子 agent transcript 走 R12-7 的 outputFile 隔离，主上下文只留指针 |
| R18-6 | `Memory` capability 接线 | TODO | R10-8 第②层的第一个真实 store：三个入口与 `MemoryCapability` 已在 R10-8 落地并由 fixture store 驱动，这里补真正的后端（codex 的 `LocalMemoriesBackend` 约 844 行是参照）并接进 `CodingHost`。<br>**验收要卡住那条硬约束，且按 R10-8 修正后的表述**——*随 query 变化的*检索结果只能进 R4-3 尾部 delta 通道，`stable_prefix_hash` 跨轮不变（用 R4-5 的 prompt dump 核验）；但 query 无关、慢变、带硬 token 上限的摘要**允许**进前缀（codex 就是这么做的），所以验收判据要能把这两种分开，不能一刀切「任何记忆都不得进前缀」。<br>**另一半是 citation 接线**：R10-8 已提供版本化曝光账本、最终 citation 验证和独立 `MemoryUsageSink`，这里为真实后端发稳定 ID / revision、接 durable outbox 并按 `(run_id, final_item_id, token)` 幂等消费，随后驱动 `usage_count` / `last_usage` 与离线保留排序；不把 search/read 曝光直接当成使用次数。 |
| R18-7 | 报告成型 | TODO | 复用 R15-4 的模式机制（`Minimal` / `EvidenceBrief` / `AnalysisReport`），**内容自有**。验证「final answer 模式」是框架机制而非编码专属 |
| R18-8 | **★ 对照组回归** | TODO | 一份 CI 报告并排列出两个产品的：profile 工具数、schema 字节、prompt 段数与 token、平均轮数、cache hit rate。<br>**「零按产品名分支」由三道互补手段共同约束，任何一道都不单独保证完全消除**（2026-09-09 更正：先前写的「依赖不到产品 crate，产品词汇就进不去」是错的——一个框架 crate 完全可以不依赖任何产品，却写 `if product_name == "coding"`）：① `cargo xtask layering` 的依赖方向检查（规则 1-4，含传递边）保证**依赖方向**；② 同一门禁的规则 5 已有源码扫描（`scan_product_references`，带 `layering-allow:` 例外标记），它是**辅助**手段，识别不了改名与间接表达；③ 代码评审与跨产品用例。不再声称任一手段是充分条件 |

### R18 非目标

| 项目 | 处理 |
| --- | --- |
| 做成完整产品（UI / 账号 / 多租户） | 不做；它是**验证载体**，规模控制在能跑通四种形态即可 |
| 与 `ra-coding` 共享 prompt 内容或工具实现 | **不做**；需要共享的东西说明它是机制，应该先上移到 `ra-tools` / `ra-prompt` |
| 追求领域效果（RAG 召回率、报告质量） | 不做；那是产品课题，本阶段只验证框架边界 |

### R18 验收标准

| 能力 | 标准 |
| --- | --- |
| **抽象无后门** | 框架 crate 里没有任何按产品名分支的代码；两个产品的全部差异由 profile + capability + prompt + guard 注册表达 |
| 图有真实消费者 | `ra-flow` 的 plan-execute 与 fan_out_join 在 `ra-assistant` 里被真实使用，不是只存在于测试用例 |
| 记忆有真实消费者 | `MemoryStore` 至少一个实现被用起来，且检索注入不破坏稳定前缀（`stable_prefix_hash` 跨轮不变） |
| 工具零复制 | `ra-assistant` 里没有任何与 `ra-coding` 重复的工具实现；两者共用 `ra-tools` |
| 产品间零依赖 | `cargo tree` 证明 `ra-coding` ↮ `ra-assistant` |

---

## 推荐开发顺序

> **要动手请先看[当前执行序](#当前执行序2026-08-19-快照)。** 本节三张梯队表是原始规划，记录的是当初为什么这么排；实际执行已按契约优先的理由提前了几条，分歧与理由逐条写在那一节。

### 第一梯队：能跑（R0 → R3）

| 序 | 任务组 | 说明 |
| --- | --- | --- |
| 1 | R0-1..R0-8 | 骨架、错误、取消、配置、CI、**扩展面契约（七条 + 稳定性分级）** |
| 2 | R1-1, R1-2, **R1-2b**, R1-3, R1-3a, R1-5b, R1-17, R1-4 | 项模型 + **`ModelSettings` 四层 resolve** + Model trait + Provider 注册 + 输入归一化 + **ApiProtocol 矩阵** + 一条 provider 路径（先 OpenAI Responses）。**R1-2b 必须在 R1-3 之前**——`get_response` 的签名要用它 |
| 3 | R2-1..R2-3, R2-7(部分) | Tool trait + 身份键 + schema 宏 + `read_file` / `exec_command` / `apply_patch` 三件套 |
| 4 | **R3-1c**, R3-0, R3-1, **R3-1b**, R3-2..R3-4, R3-6b, R3-7, R3-10, R3-12, **R3-13** | **`AgentSpec` → turn 准备顺序 → NextStep 状态机 + `FinishReason` + turn 结算 + Runner + agent 身份绑定 + `WorkState` 挂载点** —— 到这里有一个能改文件的最小 agent。<br>**R3-1c 虽然编号靠后，执行顺序排第一**：R3-0 与 R3-7 都以 `AgentSpec` 为入参 |

**里程碑 M1**：CLI 能接受一句话任务，读文件、改文件、跑命令，产出最终回答。**同时 `examples/minimal_agent`（不依赖 `ra-coding`）能编译并跑通**——这是扩展面第一次被真实验证。

### 第二梯队：像 Codex（R4 → R8）

| 序 | 任务组 | 说明 |
| --- | --- | --- |
| 5 | R4 全部 | **优先级极高**：稳定前缀 + 尾部增量 + 缓存断点。晚做的代价是后面每个 prompt 改动都在破坏缓存 |
| 6 | R1-6, R1-6b, R1-5, R1-8..R1-15（含 R1-9b）, R1-18(可选) | **OpenAI Chat（工作量最大的单项）** + compat + Anthropic（含 cache_control）+ usage 明细 + 重试 + 模型回退 + 可选聚合桥 |
| 7 | R2-4..R2-12 | 工具 profile（默认 codex_like 14-16 个）+ 单 exec 收编 + schema 字节稳定 + 观察反馈 + **`ra-tools` 拆分**（趁工具还没被 guard/profile 绑死） |
| 8 | R5 全部 | 上下文预算 + 压缩 + 淘汰 + preflight |
| 9 | R8-1..R8-6 | exec_command / write_stdin / apply_patch / 文件 / 检索 |
| 9b | **R12-1/2/3/5/6/7（契约先行）** | **子 agent 内核机制前移**：`as_tool` + 嵌套审批冒泡 + 取消传播 + 预算继承 + outputFile 隔离，并先冻结 `WorkspaceLeaseRef` / budget reservation 的 `RunState` 形状。**必须在 R6 之前**——这些字段等 schema v1 冻结后再补就是一次迁移；真正的独立 worktree 创建与 writer admission 依赖 R8-11a，放到第 12 步完成。 <br>**R12-8 `NextStep::Handoff` 归入本步**（2026-09-09 解绑：它是 SDK 基础能力，不再等 R17 图引擎）——**已完成** |
| 10 | R6 全部（含 **R6-6a**） | 权限 + 审批 + **RunState 序列化**（schema v1 从这里起版本，**扩展位一次留够**） |
| 11 | R7 + **R3-9** | Guardrail 两层 + 工具两端护栏 + UserHook 事件面；**R3-9 生命周期 hook 一并迁回**（两套注册路径各自独立，观察者不承担 stop 的续跑能力）——**已完成**：最终落地为一个 `LifecycleHook` trait ＋ agent / run 两个安装点，scope 作为回调参数 |
| 12 | **R8-P0..P10：OpenAI 执行环境移植** | 按 R8 新顺序完成契约、Unix-local、Runner、PTY／文件、恢复、Docker 与对齐验收。旧 R8-11a / R12-5 的 coding 并发扩展独立排期，不阻塞移植；该扩展完成前不默认启用依赖其保护的可写子 agent 并发。 |

**里程碑 M2**：在真实 provider 上完成一个跨文件修改任务，cache hit ≥ 70%，read-before-edit / 后台收尾 / 改后验证三项纪律指标可测；`RunState` schema v1 冻结且扩展位齐备。

### 第三梯队：像产品（R9 → R15）

| 序 | 任务组 | 说明 |
| --- | --- | --- |
| 13 | **R9-0** | **双通道事件日志（rollout 格式）—— 提前做**，它是 R13 控制协议与 R14 replay 的共同地基 |
| 14 | R9-1..R9-15 | 会话、镜像、fork、resume、checkpoint、session 对账、服务端 conversation、provider compaction、`WorkState` 持久化 |
| 15 | R10 全部 | Capability 装配层（把 R2/R5/R8 已有的能力收编进来） |
| 16 | R13-1..R13-7 | 控制协议核心（帧、stdio、握手、反向请求、生命周期） |
| 17 | R11 全部 | MCP + 进程内工具 + skills + 插件 |
| 18 | R14 全部 | Eval 飞轮 —— **必须在 R15 之前**，因为是否需要改后提醒或收尾策略必须先由度量决定 |
| 19 | R15 全部 | 工具历史投影、改后提醒、final 成型；不实现验证账本或 closeout gate |
| 20 | R13-8..R13-11 | 事件订阅、连接硬化、结构化输出 |
| 21 | **R17-1..R17-5, R17-9, R17-10** | **编排与图引擎地基**：`WorkState` + Node/Edge/Scheduler + 图 checkpoint + 可观测性 + 静态校验。排在 R14 之后是因为图类 agent 的回归断言（「实际路径 == 期望路径」）依赖 eval 飞轮 |
| 22 | **R17-6, R17-7, R17-8** | **预置拓扑**（整段随 R17 暂缓）：plan-and-execute、supervisor / fan-out / pipeline / debate，以及 handoff 在图里的边表达——**handoff 本身早在第 9b 步由 R12-8 跑起来了，不等这里** |
| 23 | **R18 全部** | **第二个参考产品 `ra-assistant`**：只读工具面 + 检索纪律 + 图拓扑接线 + 记忆消费 + **对照组回归**。排在最后是因为它要消费 R17 的成果，但**它才是「框架是否通用」的终审** |

**里程碑 M3**：桌面/IDE 宿主能通过控制协议驱动，支持审批、中断恢复、会话 fork、rollout 回放，eval 报告有成本与纪律双指标。
**里程碑 M4**：ReAct / plan-and-execute / graph / multi-agent 四种形态各有一个 e2e 用例跑通，且**四者共用同一套 `Runner` 与 `RunState`**——不存在为某一形态开的后门。
**里程碑 M5**：`ra-coding` 与 `ra-assistant` 两个特征相反的产品跑在同一套框架上，**框架 crate 里没有一行按产品名分支的代码**，两者的全部差异由 profile + capability + prompt + guard 注册表达。

### 第四梯队（DEFERRED）

R16 产品面、远程 catalog、多租户、图定义 DSL、分布式节点调度。

---

## 当前执行序（2026-08-19 快照）

> 上面三张梯队表是**原始规划**，本节是**当下按什么顺序动手**。两者分歧时以本节为准，且分歧理由必须写在这里；梯队表不改，它记录的是当初为什么这么排。每完成一条就把状态回写到对应阶段的任务表与[近期待办清单](#近期待办清单)两处。

### 已发生的偏离（在案，不是漏做）

R6-6a、R8-0、R9-2a、R9-0a、R9-0b 五条按编号分别属于第 10、9、14 步，实际已提前做完。共同理由是**它们冻结的是 schema 与不变量，不是功能**——`RunState` 的扩展位、`HostEvent` 的信封、`Session` port 的收发类型、rollout 的序号空间与 usage 基线，任何一条等到本来的位置再补都是一次带迁移的破坏性变更。连带结果：R5 的前置门（R9-0a 的不可裁剪 usage 基线与 `timeline_seq`）与 R5-3 / R6-6 的前置（R9-2a）**都已通过**。

**R2-4（批次 C 第 13）提前到批次 A 中间做完**，理由同上一类但有一处不同：注册表与 profile 冻结的同样是形状而不是功能，可它交付时**装配不出任何一档**——15 个 advertise 入口当时只有 `read_file` 与 `exec_command` 是真工具。这不是缺陷而是本条的交付物之一：三档清单先写下来，`assemble` 就会在缺件时点名失败，批次 A 剩下的 R8-4 与后续 R8-5/8-6 是**对着一份已存在的规格**填空，而不是各自决定自己叫什么、算不算 advertise。代价是 `build_agent` 这一轮仍不接 profile；`apply_patch` 已随 R8-4 落地，接上去还要等 `write_stdin` / `grep` / `glob`。

### 当前缺口：M1 已达成（2026-08-28 更新）

批次 A 四条（R8-1 / R8-4 / R2-7 / R3-4b）全部 DONE，`examples/minimal_agent` 也已落地并进入 `cargo xtask layering` 的白名单，M1 的两半都到齐了。R2-8 现在计 3/15（`read_file` / `exec_command` / `apply_patch`），其余 12 个 advertise 入口归批次 E。

> **本段原文已过期，保留改写而不是删除**：它当初写的是「三件套只有 `read_file` 落地……R2-8 计 1/15」，那是排出批次 A 的理由。理由已经兑现，下面的 A 在 B 之前不再是分歧而是既成事实。

**当初把 M1 缺口排在 R4 正文之前的两条理由（存档）**：① R4 的**机制**（R4-1/2/3/11）已经 DONE，「晚做 R4 的代价是此后每个 prompt 改动都在破坏缓存」这条风险已经退役，剩下的是文本；② R4-0c 与 R4-0g 直接点名 `apply_patch` 与 advertise 集，在工具不存在时写它们，等于给一个干不了活的 agent 写它没有的工具的行为契约，而且写完也没有真实 run 能验。

**`examples/minimal_agent` 落地时暴露的两件事**，各自独立记在下面：

| 发现 | 处理 |
| --- | --- |
| **`Model::get_response` 在 `ra-runtime` 里已无调用点** —— R3-4b 之后每次模型调用都走 `stream_response`（`runner.rs::call_model` 注释写明「so every call streams」，`partial_messages` 只决定旁白是否离开 runtime）。`Model` trait 仍把两个方法并列为必填,第三方据此以为非流式 run 会走 `get_response`,实际不会 | **已裁定（2026-08-28）**：`stream_response` 改为带默认实现（由 `get_response` 合成单 `Completed` 流），`get_response` 成为唯一必填方法。理由与连带改动见 R1-3 行 |
| **取 run 的最终文本没有出口** —— 宿主要么走 `final_message()` 再 `text_content()`，要么自己遍历 `new_items()` 找 `OutputPhase::Final`（后者会重复 `RunResult::new` 里已有的 `find_final_message`，两份定义迟早分叉） | **已补** `RunResult::final_text()`：只读 `final_message` 一个字段，空串与 `final_message()` 的 `None` 同义。挑选/裁剪/按受众渲染仍归 R15 |

### 批次 A — 补齐 M1（能改文件的最小 agent）

| # | 任务 | 名称 | 排这里的理由 |
| ---: | --- | --- | --- |
| 1 | R8-1 | `exec_command` | 执行脊柱，占 Codex 全部调用的绝对主力。类型与责任边界已由 R8-A 冻结，`ra-exec` 的 command / output / session 底座已在 |
| 2 | R8-4 | V4A `apply_patch` | 唯一编辑入口。`ra-patch` 的 parse 已有，apply 只有骨架、render 与 fuzz 是桩；R2-3 留的 `custom_tool_call_output` 缺口也在这条补 |
| 3 | R2-7 | 并发、超时与失败成型 | `FunctionToolResult` 收口，别让 runner 只拿到裸字符串而丢掉审批与嵌套 run。**要有两个真工具才验得了**，所以排在 1、2 之后 |
| 4 | R3-4b | 循环形态：一思 → 流式派发 → 批量观察 | 轮内并行是默认形态不是优化项。读 R2-1 的 `ToolConcurrency` 与 R2-7 的并发上限，R3-4c/4d 的结算与准入已就位 |

**到这里 M1 达成**：CLI 能接一句话任务，读文件、改文件、跑命令，产出最终回答。

**四条均 DONE，并且 `examples/minimal_agent` 已交付**：只依赖 `ra-core` + `ra-runtime`，自带一个 `Tool`、一个 `Model` 与一个 `ModelResolver`，无 API key 无网络即可跑完两轮（工具调用 → 最终回答）。它的约束不靠自觉——`xtask/src/layering.rs` 的 `ALLOWED_INTERNAL_DEPS` 给它钉了一行 `&["ra-core", "ra-runtime"]`，加任何第三个内部依赖 `cargo xtask layering` 当场失败并点名。已反证该门禁确实拦得住（临时加 `ra-coding` → FAIL）。

### 批次 B — R4 正文与核验入口（原第 5 步）

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 5 | R4-0a | 身份与协作契约段（`identity`） | 前缀第一段 |
| 6 | R4-0b | 工程判断段（`engineering`） | |
| 7 | R4-0c | 编辑约束与 Git 安全段（`editing`） | 依赖批次 A 的 R8-4 |
| 8 | R4-0d | 自主推进与止损段（`autonomy`） | **DONE**；措辞与 R3-6c 的按工具配置阈值对齐 |
| 9 | R4-0e | 输出格式规则段（`formatting`） | **DONE**；认 `final_answer` 槽，host-backed 前缀 993 token，距 1024 地板只剩 31 |
| 11 | R4-5 | `prompt dump` 与 doctor | **DONE**（`prompt dump`）；组装落在 `ra-coding::prompt::dump`，快照门禁与 CLI 共用一个渲染入口，`--baseline` 点名触发源并以退出码报告漂移 |
| 11 | R4-5 | `prompt dump` 与 doctor | 补 `ra-cli` 的 `prompt dump` 子命令——现在这个「核验入口」只有测试在用，对使用者不存在 |
| 12 | R4-7 | 提示片段回归锁 | **DONE**；三样齐：insta 锁装配后正文（七份快照，五个 role + main/coordinator host-backed）、每段在定义处声明 token 额度且由装配器强制（合计 2048 = 2×缓存地板）、no-copy 走结构性检查（provenance 必须是 `Agent` + 三字段扫第三方来源标记，带植入 marker 的对照） |

前缀已跨过 1024 token 的缓存门槛；`test_the_product_prefix_reaches_the_caching_floor_and_has_a_plan` 锁定这项事实以及 cache plan 的 prefix 对齐。adapter 仍以完整 wire 工具表决定是否下发缓存字段。

### 批次 C — 工具面收口（原第 7 步前移一半）

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 13 | R2-4 | 工具注册表与 profile | **DONE（提前到批次 A 中间）**；默认 `codex_like(14-16)`，三档清单已落，缺的工具由 assemble 点名 |
| 14 | R2-9 | 工具 schema 字节级稳定 | **DONE**；`api/tool-schemas.txt` 落盘对账，R4-6 的落盘门禁可照抄这个形态 |
| 15 | R4-6 | 工具 schema 稳定性 | **DONE**；工具清单进稳定前缀，指纹留在 `api/tool-surface.txt`，`prompt-dump` 对账两份产物 |
| 16 | R4-0g | 工具使用契约段（`tool_use`） | 必须与 15 对着同一批工具 |
| 17 | R2-5 | 手法一：单 exec 收编长尾 | |
| 18 | R2-5b | 手法二：namespace 折叠 | |
| 19 | R2-5c | 手法三：deferred tool + `tool_search` | 声明面 `ToolExposure::Deferred` 已就位，BM25 用 `bm25` crate |
| 20 | R2-6 | 动态启用与冲突策略 | |
| 21 | R2-10 | 工具 schema token 预算 | ≤ 20 KB |
| 22 | R2-11 | 工具行为契约文本 | **进行中（3 项已落地）**；默认短描述 |

### 批次 D — Provider 面（原第 6 步）

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 23 | R1-6 | OpenAI Chat Completions provider | **DONE**；R1 最大单项，convert / stream / reasoning 三件齐 |
| 24 | R1-7 | 流式与部分消息 | **DONE**；终态 backfill 在模型通道、run 通道只转发旁白、Responses 换成真 SSE |
| 25 | R1-8 | Usage 逐请求明细 | **DONE**；`RequestUsage` 逐条 + `Usage` 账本，预算改读该账本，rollout 对账加 `requests` 维度；review 后补了旧 checkpoint 迁移、默认响应记一次请求、条目未知字段提升三处 |
| 26 | R1-9 | 错误归一化与重试 | **DONE**；事实挂 source 链、重放判据由发出层盖章、`RetryBackoff` 注入随机数；review 后补 stateful 请求不声称 replay-safe、`Retry-After` 日期形式、409 归 `Conflict` |
| 27 | R1-9b | Retry policy 与流式重试边界 | **DONE**；`ModelRetryPolicy` / `RetryPolicyContext` / `RetryDecision` 把可执行的宿主策略与可序列化的 retry/backoff 设置分开，`max_retries` 是初始请求之外的硬上限。runner 每次物理请求各开 generation span，成功响应在 usage ledger 前补失败请求的零用量条目；退避等待走 turn cancel scope，取消或 deadline 不会在等待后再发一枪。adapter 的 `Unsafe` replay verdict 是硬拒；client-owned 非流式请求可安全重放，server-managed 的 `Unknown` 只能由策略显式批准。流式只要已向 run subscriber 转发过一个 raw provider event，随后的失败立即标 `Unsafe`，绝不重放并重复用户已见帧。`retry.*` trace 字段记录尝试、上限、等待和原因；6 条 `it-runtime/model_retry` 契约测试覆盖安全非流重试与 usage、adapter veto、公开流事件边界、显式零预算、退避等待取消、无 adapter advice 时错误链 `retry_after` 回落。 |
| 28 | R1-6b | OpenAI 兼容端点 compat 层 | **DONE**（提前到 R1-7 之前做）；`CompatEndpoint` 一处描述一个端点，`sse_done_marker` 补齐 Quirks 七项 |
| 29 | R1-5 | Anthropic Messages provider | **DONE**；真 adapter 落地（传输 / lowering / lifting / SSE / 错误分类），R1-11 的三条耦合搬了过来，`cache_control` 打在单块 system 上并按 tools+system 合计计门槛。审核当场修掉 redacted 回放、流式 panic、`output_format` 废弃形状、`tool_choice:none`、流内 error 分类、失败流不终止六处；smoke preview 退休归 R1-14 |
| 30 | R1-14 | Anthropic 请求形态对齐 | |
| 31 | R1-15 | thinking 块与签名 | |
| 32 | R1-11 | Thinking / effort 透传 | **DONE**；三协议下发 + Anthropic 三条字段耦合校验，落点在 compat preview，真 adapter 归 R1-5 / R1-14 |
| 33 | R1-13 | `prompt_cache_key` 稳定下发与显式断点 | run 级 typed override 归这条，不许用 `extra_body` 顶 |
| 34 | R4-4 | 缓存断点治理（收尾） | 补 Anthropic 与 Chat 两个 adapter 的空桩，`cache_control` 到此才真的发得出去 |
| 35 | R1-16 | 结构化输出 schema | DOING；声明链（`OutputSchema` + build 时校验 + 投影到请求）已提交，解析 / `OutputValue` / closeout 校验仍欠 |
| 36 | R1-12 | 模型分层与拒答回退 | |
| 37 | R1-10 | Provider 兼容矩阵 smoke | 四协议 payload 内联快照 + Responses↔Chat reasoning/tool-pair 往返 smoke |

### 批次 E — 执行面补齐（原第 9 步剩余）

| # | 任务 | 名称 |
| ---: | --- | --- |
| 38 | R8-2 | `write_stdin` + PTY |
| 39 | R8-3 | 后台 job 生命周期 |
| 40 | R8-5 | 文件工具边界 |
| 41 | R8-6 | 检索工具 |
| 42 | R2-8 | 内置工具集 v1 收尾（15 个 advertise 入口）；**11/15**，剩 `ask_user` / `tool_search` / `agent.*` / `mcp.*` |

### 批次 F — 上下文管理（原第 8 步，前置门已过）

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 43 | R5-2 | 模型上下文窗口表 | **DONE**；内建 69 条对齐上游、未知模型不猜，阈值按窗口比例（默认 60%）以基点整数计算，config 与 table 只有一条查表路径 |
| 44 | R5-1 | 工具结果预算 | **DONE**；入场判定与裁剪同一次度量，投影器只能追加（`ToolOutputProjection`），`ArtifactRef` 按 run + call 定名 |
| 45 | R5-3 | Compaction | **DONE**；`ContextUsage` / `CompactionLimits` / `CompactionAssessment` 三维触发器，估算只计内容不计 wire framing；`CompactionSummaryBuilder` 锁 CC 九段、空白 slot 被拒、第 6 段逐条 fenced 渲染防伪造；`AnchorRetention` 产出不重叠的 head/anchor/tail 并保持原序；`ensure_converges_with` 拒绝清不掉自身阈值的策略配对。**本条只定投影与摘要形状**，发请求与装回 session 归 R10-6 / R9-14。验收 `tests/it-context/tests/compaction.rs` 19 条 |
| 46 | R5-3b | 压缩 / resume / replay 不得破坏控制面状态 | **DONE**；`project_compacted_model_input` 只借 `RunItem` 权威历史、不接收也不改 `RunState`，本地审批不进摘要覆盖项；复用 R3-6c 的 `ToolFailureEntry::already_recorded` 有界 `call_id` 窗口 + 整轮指纹两层去重，不另起一套。验收 `tests/it-runtime/tests/compaction_control_state.rs`：checkpoint 恢复后重放旧 `call_id` 不改失败记录与预算，下一次失败仍抵达阈值并被 `tool.no_progress` 拒绝，动态 claim 求值次数不变 |
| 47 | R5-4 | 老工具结果选择性淘汰 | **DONE**；显式引用账本落在 `ra-core::state` 并成为 `RunState` 字段（schema 2→3），未引用 N 个完整轮次的大结果才会转成有界 artifact 摘要；runner 经 `ModelInputProjector` / `ToolOutputReferenceExtractor` 两个 port 接线，轮次轴跨 resume 连续；R5-8 仍是互补的位置窗口策略 |
| 48 | R5-8 | 旧轮次工具输出裁剪器 | **DONE**；`ToolOutputTrimmer` 纯投影，user turn 窗口 + item 兜底，裁完仍是带 metadata 与 artifact 引用的 `ToolOutput`，替换体恒不超上限 |
| 49 | R5-6 | Oversized input preflight | **DONE**；超限单条 user text 保留首尾原文、中段 map-reduce，`InputSummarizer` 是宿主端口，配置按自己渲染出的输出校验可行性 |
| 50 | R5-7 | 上下文用量 API | **DONE**；五类分档存在 `[usize; COUNT]` 里按 `index()` 索引，UI 遍历的 `ALL` 与加总出 total 的是同一份数据（const 断言保证位置完备）；定义按渲染文本计价（键算进去）、整表只取整一次，MCP 工具目录归 tools；`model_input()` 同遍历产出 compaction 的 `ContextUsage`，两个总数不会打架；共享字符基准落在私有 `crate::estimate`，compaction 与 usage 平级消费 |
| 51 | R5-5 | 压缩历史外部可寻址归档 | **DONE**；`ArchiveRef` 逐字带走、读取不否决（未知格式仍按原文命中记录），按记录携带的引用定位而非引用内的 session（graft / fork 过来的历史照样解析），取回提示由宿主提供、默认什么都不告诉模型 |

### 批次 G — 子 agent 内核契约（原第 9b 步，**必须在 R6 之前**）

等 `RunState` schema v1 冻结后再补这些字段就是一次迁移，所以契约先行。

| # | 任务 | 名称 |
| ---: | --- | --- |
| 52 | R12-1 | `AgentDefinition`、注册与 `HandoffSpec` |
| 53 | R12-2 | `Agent::as_tool()` |
| 54 | R12-3 | 嵌套审批镜像 |
| 55 | R12-6 | 预算继承、预留与总账 |
| 56 | R12-7 | 子 agent 上下文文件级隔离 |
| 57 | R12-5 | 并发、工作区 lease 与取消传播（**只做契约与 `RunState` 形状**；真正的工作区物化与 writer admission 依赖 R8-11a，归第 12 步。2026-09-15：原文此处写「worktree 创建」，与 R8-11a / R12-5 已取消强制 worktree 的裁决不符，改为通用措辞） |
| 58 | R12-4 | 子 agent 事件转发 |
| 59 | R12-7b | 停滞子 agent 止损 |

### 批次 H — 权限与恢复（原第 10 步）

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 60 | R6-1 | `PermissionMode` | 计划模式作为其中一档（附录 B 裁决 3） |
| 61 | R6-2 | 权限规则引擎 | 只做规则匹配与档位求值；落盘/撤销归 R6-7，参数级判据归 R6-8，`PermissionUpdate` 归 R6-3 |
| 62 | R6-3 | 审批决策类型 | `PermissionDecision` 已被 R6-2 用作三值规则词表，宿主答复要另起一个类型 |
| 63 | R6-8 | 危险动作检测 | |
| 64 | R6-8b | 四级自动审批分类 | 分界是「用户意图能否解除」而非危险程度 |
| 65 | R6-4 | 审批上下文 UI 字段 | |
| 66 | R6-5 | `NextStep::Interruption` 落地 | |
| 67 | R6-6 | `RunState` 序列化 | **schema v1 从这里冻结**，扩展位由 R6-6a 一次留够 |
| 68 | R6-7 | approve / reject 与续跑 | **DONE**；答复存进 checkpoint、追加完 output 才清 pending，`always` 规则钉死 lookup key，schema 升 v2 |
| 69 | R6-9 | 配置矛盾预警 | |
| 70 | R6-10 | 沙箱与审批的边界声明 | |

**到这里 M2 达成**（真实 provider 上跨文件修改、cache hit ≥ 70%、三项纪律指标可测、`RunState` schema v1 冻结）。

### 批次 I — 后台执行闭环与文件事实（插队，排在第 12 步之前）

**插队理由**：批次 E 的 R8-3 交付了 `BackgroundJob`，但它至今**除自己的集成测试外零调用方**——模型够不着的子系统，R8 验收「后台可收尾」那一条因此不通。第 12 步的沙箱后端改的是执行路径，不依赖这个入口；先补它不会被后面推翻，反过来先做沙箱则让这块代码继续挂着。两条各自独立交付，**不因为共用一次 `TOOL_SCHEMA_REVISION` 就捆成一次修改**。

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 71 | R8-3a | `write_stdin` 的等待与控制 | **DONE**；后台执行闭环：默认 poll、有界 closeout、匹配、显式控制、增量输出与并发语义作为一个交付单元落地 |
| 72 | R8-5b | 文件事实进 Session 工具事件 | **DONE**；R8-5 移交项②，独立交付，未改模型工具 schema |

**批次 I 已完成。** 收尾按计划一次做完：`TOOL_SCHEMA_REVISION` 5→6、三份基线重刷、九条门禁全绿。下一件回到第 12 步。

### 批次 J — 旧 R8 宿主基线与临时目录（历史实现记录）

> 2026-09-16 起不再是当前开发顺序；以下 DONE 与裁决只记录旧规格交付。新任务按 R8-P0..P10 执行；涉及强制 lease、默认策略和 Docker 前置的旧结论由 R8 新边界取代。

**为什么这两条一起**：临时目录是每个沙箱后端写策略的输入，`RUSTY_AGENT_TMPDIR` 的注入还要与环境清理定先后（先洗再注）。放到执行后端之后做，会让各条路径重复接线；Docker 另按 R8-10 定义目录映射与容器退出确认。

| # | 任务 | 名称 | 备注 |
| ---: | --- | --- | --- |
| 73 | R8-7 | 沙箱后端：unix_local | **DONE**；环境派生 + 四条 rlimit（软硬同设）+ cwd 诚实化，落在 `ExecEnvironment` 并由 `ProcessManager` 持有 |
| 74 | R8-13 | 运行期临时目录 | **机制 DONE，产品接线未做**；计数句柄、幂等清理、`RecoveryRequired` 与已核验回收入口齐了，但没有 run 真的拿到目录 |
| 75 | R8-8 | 沙箱后端：macOS seatbelt | **DONE**；三档梯子 + 独立网络轴 + `SandboxBackend` trait + 实际生效档位随结果返回，macOS 真 spawn 验收 |
| 76 | R8-9 | 沙箱后端：Linux bwrap + seccomp | **实现 DONE，原生验收在 CI**；namespace 做边界、seccomp 补 netns 管不到的路径型 Unix socket，`ra doctor sandbox` 自证 |

**两处裁决记在这里，正文各自的行里有完整版**：① 不定义 `SandboxBackend` trait——一个实现看不出接口哪部分通用，而两个参考的形状本就不同（codex 是单次 spawn 的围栏，openai 是有 start/stop/state 的 session），现在写就是照着第一个实现描边；trait 等 R8-8 / R8-9 到齐再写，`ExecEnvironment` 是它将来的入参。**已兑现（2026-09-15）**：两个后端到齐后 trait 照这条裁决写了，形状是它们真正共有的那部分——`available_level()` 先说这台机器最多能给哪一档，`confine()` 再把命令包起来，**宿主围栏后端一律不 spawn**（宿主执行路径仍由本 crate 的统一启动点拉起，统一实施环境、rlimit 与进程组；2026-09-16 明确：这个契约只约束 `SandboxBackend`，R8-10 的容器执行环境另行实施并验证容器内策略和生命周期，不伪装成同一条 spawn 链）。`ExecEnvironment` 确实成了它的入参。② 工作区 `unsafe_code` 保持 `forbid`，`ra-exec` 自带一份 lint 表降为 `deny`，因为 `forbid` 连需要它的那一行上的 `#[allow]` 都压不住；一条测试断言两张表除这一项外逐字一致。

**历史遗留的归属已调整**：旧 R8-13 的产品接线未完成；上游 session 生命周期由 R8-P3/P4/P7 处理，不等待 R8-11a。独立 run 临时目录与 lease 联动仅在 coding 扩展需要时实现。

### 之后

按梯队表第 11 步起：R7 与 R3-9 全部已完成（生命周期 hook 走 `ra_runtime::lifecycle` 这条独立注册路径，不复用 `ra_runtime::hook` 的决定型注册面），**当前第 12 步改为 R8-P0..P10 上游执行环境移植；R8-11a / R12-5 coding 并发扩展独立排期** → 第三梯队 R9-0 起。**R4-0h 前端设计段**的前置（R10-5 按需加载）已就位，可随时落地，仍不进常驻前缀。

#### 第 12 步的历史措辞更正（2026-09-15；现行顺序以 R8-P0..P10 为准）

以下保留历史裁决；2026-09-16 已同步改写梯队表。lease 与 writer admission 仅约束选用它们的 coding 扩展，不再定义上游移植主线的完成条件：

1. **「实际 worktree 生命周期」作废，改为通用工作区生命周期。** 梯队表写的是「完成 `WorkspaceLease` 的实际 worktree 生命周期」，而 R8-11a 与 R12-5 在 2026-09-09 已取消强制 worktree，这一处漏改。git worktree 只是隔离实现之一，**基础 `Runner`、handoff 与只调 API 的 agent 不因此需要 git**。同一处措辞在 R12-B 的分层图与分层表、批次 G 第 57 条也一并改掉。
2. **R12-5 的「框架不让最后完成者静默覆盖」按隔离策略分档。** 它与 R8-11a 同日修正的「共享模式下框架只如实记录持有者」正面冲突，共享模式下没有任何机制能兑现那句承诺。独占与串行也只保证参与租约协议的写入者不重叠，不保证基于陈旧内容的后续写入不会覆盖已有修改。
3. **R8-7 / R8-8 / R8-13 的 AF 出处删除**（AF P41-1 / P14.1 / P21），依「不以 AF 为设计依据」的裁决；事情本身保留：环境构造与沙箱策略组合参考 codex 的 `unified_exec` 与 `sandboxing`；rlimit、具名配置及降级关系属于 Rusty 的设计选择，不归因为上游已有契约。**R8-12 的 `ContentTrust` / `Secret` / `UntrustedData` 词表删除**——R7-11 整条早已撤销，R11-2b 也已把凭据外发重新归到 R8-12，只有 R8-12 自己那行漏改。
4. **R8-13 的宿主机制与 R8-7 同批，不排在执行后端之后。** 临时目录是写策略的输入，`RUSTY_AGENT_TMPDIR` 的注入还要与环境清理定先后；此处原定的 run 级持有接口先于 Docker 的依赖已撤销，现按 R8-P4 的上游所有权与清理契约接入 R8-P8。

**随后补上的裁决（2026-09-15，见 R8-11a）**：lease 与 `ResourceClaim` 如何协作已经定了——**按工作区协调作用域组织长期准入状态，键是 `ResourceId` 而不是 lease id**，lease 继续只管资格、模式与清理责任。定它的过程里核出一件事值得单独记住：**当前并不是两把锁在争，而是一个缺口**——`ResourceAdmissionGate` 的锁表每轮新建、不同 run 互不可见，所以工作区独占实际上只在同一批调用内成立，跨轮、跨 run 和后台执行期间都是敞开的。裁决连带给出五条约束、两条补充（全局 gate 留在轮内；claims 阶段要能看到已解码输入）和一条必须覆盖的死锁验收。**只做 run 级 lease 的中间形态可以先交付，但不得据以宣布 coding 并发扩展完成**：它挡不住单 run 里「起后台命令 → 下一轮 `apply_patch`」的并写。

#### AF 出处清理（2026-09-15，全文一次做完）

依「不以 AF 为设计依据」的裁决，把**拿 AF 当正面依据**的引用一次清干净，共八条：第 12 步内的 R8-7（P41-1）、R8-8（P14.1）、R8-13（P21），以及 R9-9（P24.5）、R11-11（P18）、R13-10（P38）、R14-8（P39）、R15-4（P60/P62）。

**做法是删出处、留条目，不是顺手补一个新出处。** 清理时发现两类错误，记在这里免得再犯：① R8-7 原把资源上限一并算到 codex `unified_exec` 头上，可 codex 全仓只有 `process-hardening` 给**自己**设 `RLIMIT_CORE=0`、`utils/pty` 读一次 `RLIMIT_NOFILE`，对子进程没有任何资源上限策略；② R8-8 原称 codex 有"可枚举的降级档"，而 `Minimal` 只是 `FileSystemSpecialPath` 的一个路径记号，由 `include_platform_defaults()` 一个布尔决定拼不拼进策略，不是按强弱排序的梯子。**换出处必须回源码核过，核不动就写成 Rusty 自己的设计选择**，这比挂一个错的上游更诚实。

**保留不动的是拿 AF 当反面教材的那些行**（"全局非目标"表的 P68 / P43-2、R2 工具面里 43-44 个工具与双 profile 并存那几条）：它们记的是"不做什么"以及当初为什么排除，属于历史观察而不是设计依据。

顺带修掉同类的一处悬空引用：R11-11 的"运行权限交 R6 / R7-11 / R8-12 判定"——R7-11 早已撤销，与 R8-12 是同一处漏改。

---

## MVP 验收标准

| # | 能力 | 标准 |
| --- | --- | --- |
| 1 | 能干活 | 在真实仓库上完成"定位 → 修改 → 验证"的跨文件任务，不需要人工干预中间步骤 |
| 2 | 成本可比 | 同任务同模型下，total token 与 cache hit rate 与 Codex CLI 差距 ≤ 20%（目标：cache hit ≥ 70%，对标 Codex 89.5% / CC 79.7%） |
| 2b | 工具面达标（**仅 coding profile**） | 默认 profile 工具数 ∈ [14,16]、硬上限 24；schema 总量 ≤ 20 KB 且逐轮字节全同。**这是 `ra-coding` 的产品指标，不是框架上限**——第三方产品与图编排类 profile 各有自己的预算 |
| 2c | 输出克制（**仅 coding profile**） | 平均每轮 output ≤ 1.5k token（对标 CC 1,033 / Codex 1,418） |
| 3 | 纪律确定 | read-before-edit、后台任务收尾、改后验证三项在 eval 中 applicable 场景命中率 ≥ 90%；**硬阻断 guard ≤ 8 个**，行为塑造靠双通道而非拦截 |
| 4 | 可中断可恢复 | 审批中断、进程崩溃、断网三种场景都能从结构化状态恢复继续 |
| 5 | 可治理 | 宿主能通过控制协议改模型、改权限模式、中断、回滚文件、观察上下文用量 |
| 6 | 可扩展 | 第三方能通过 MCP 或进程内工具服务器加能力，不改框架代码 |
| 6b | **框架真的通用** | `examples/minimal_agent` **不依赖任何参考产品**也能定义自有 `Tool` / `Guard` / `Capability` 并跑通 run；`cargo public-api` 基线无未标注破坏性变更 |
| 6c | **四形态同源** | ReAct / plan-and-execute / graph / multi-agent 四种形态**共用同一套 `Runner` 与 `RunState`**，没有为任一形态开的特例分支或后门 API |
| 6d | **双产品对照** | `ra-coding`（写为主 / 单循环）与 `ra-assistant`（只读为主 / 图编排）跑在同一套框架上；**框架 crate 里零按产品名分支**；`cargo tree` 证明两个产品 crate 互不依赖；两者差异 100% 由 profile + capability + prompt + guard 表达（R18-8 的 CI 报告） |
| 7 | 可度量 | 每次改动都能通过 eval 看到成本、纪律、行为三类指标的变化 |
| 8 | 不吹牛 | 文档明确声明：不宣称与 Codex / Claude Code 等价；第一方隐藏 prompt、服务端摘要、模型 snapshot 是有界未知 |

---

## 全局非目标（贯穿所有阶段）

| 项目 | 处理 | 依据 |
| --- | --- | --- |
| effort / 模型自动 router | 不做，只透传用户设置 | 上游 `openai-agents-python` 无自动 router；codex 的 effort 也是用户设置 |
| 用自然语言词表做控制流判断 | 不做；只读结构化状态 | AF `AF_词表式判断审计` |
| 靠加长系统提示提升遵守率 | 不做；少而精 + 就近放置 + 带正例 | codex 6.6 KB 系统提示，工具纪律全是软倾向 |
| 每轮重算 system prompt | 不做；破坏前缀缓存 | AF `三方Agent运行机制对照` |
| 把框架纪律放进用户可配 hook | 不适用；框架不带纪律，hook 是宿主扩展点 | codex `hooks/` 全部是宿主/项目可配 |
| **靠堆 guard 逼近 Codex 效果** | **不做，而且框架自带 guard 数量为零**（2026-09-09 R7 撤销后不再有「≤8」这个上限，因为没有可数的东西）。codex 用的是前置生成引导（双通道 + 系统提示散文），不是事后硬阻断 | `codex-rs/core/gpt_5_codex_prompt.md`（6.6 KB，无一条硬 gate） |
| **宽工具面（40+）** | 不做；对齐单次 advertise 口径的 Codex 16 / CC 24，长尾能力收进单个 `exec_command`。**这条约束的对象是 `ra-coding` 的 profile，不是框架** | 附录 A |
| 把产品指标当框架上限 | 不做；工具数、schema 预算、output 长度都是 **coding profile 的策略**，`ra-assistant` 与第三方产品各自定 | 本版新增 |
| 只用一个参考产品验证框架 | 不做；单一消费者会把抽象拟合到那一个业务上，必须有 `ra-coding` / `ra-assistant` 两个特征相反的对照组 | R18 |
| 在框架里按产品名分支 | **不做**；出现即 CI 失败。差异只能由 profile / capability / prompt / guard 注册表达 | R18-8 |
| 让产品层承载内核不变量 | 不做；子 agent 审批/取消/计账/隔离必须在框架内，否则每个产品重写一遍或反向依赖 runtime 内部 | R12 优先级说明 |
| 公开 API 无稳定性承诺 | 不做；Stable / Evolving / Internal 三级 + `cargo public-api` 门禁 | 框架契约与扩展面 |
| 默认在交互流挂重型 verifier | 不做；自治 / eval 才 opt-in | 两个参考默认都不挂 |
| 一次实测当统计结论 | 不做；pilot n≥3、confirmatory n≥10 才改默认 | AF P68 |
| 逐字复制第三方 prompt | 不做；只对齐结构，内容原创并标注 provenance | AF P43-2 |
| Tracing 外部上报 / voice / realtime | 不做 | 分析文档 §3.12 |
| 云沙箱适配（七家） | 不做；留 trait | 分析文档 §3.12 |

---

## Deferred Backlog

- ~~`NextStep::Handoff`、批量 fan-out、Workflow DAG 引擎~~ —— **本版移出 Deferred，归 [R17](#r17-编排与图引擎ra-flow)**。原因：「288 轮重度编码任务里零激活」这条实证管的是**编码产品默认 advertise 什么**，不管**框架提供什么机制**；而 graph / plan-execute / multi-agent 是本项目的一等交付目标
- 图定义 DSL / YAML schema、分布式跨进程节点调度（R17 非目标）
- CSV 行级批处理（Codex `agent_jobs` / `agent_job_items`：instruction + output_schema + input/output csv + attempt_count + max_runtime）—— 同上，零激活
- **记忆异步管线**：stage1（每会话蒸馏 raw_memory + rollout_summary）→ stage2（择优提炼），用带**租约 lease + 水位 watermark + 重试**的 jobs 队列做幂等 backfill。主循环零额外延迟、可崩溃恢复。设计形态照 Codex `memories_1.sqlite`
- **自治 GoalRunner**：goal 绑定 `token_budget + time_budget`，到顶进 `usage_limited` / `budget_limited` 态而非无限烧；状态落库（Codex `goals_1.sqlite/thread_goals`）
- 远程 Skill / Plugin / MCP Catalog（含签名与受管安装）；Plugins marketplace（`cache/<market>/<name>/<version>/` 版本化）
- 多端产品面：远程通道、设备配对、多租户 policy plane
- MCP sampling / roots 完整支持
- 浏览器工具、computer-use 与视觉链路（ffprobe → frame → view_image）——走 deferred tool + `tool_search` 发现

---

## 附录 A：Codex / Claude Code 实证基线

> 全部为**真实抓包 / 真实会话日志**，非推测。原始文件：
> - Codex：`/Users/moses/workspace/custom-app/openai-rust-server/codex_tools_16_gpt55.json`（19,786 B，82 个请求逐字节不变）、`codex_instructions_gpt55.txt`（16,299 字符）；`~/.codex/logs_2.sqlite`（334 次完整请求体）；`~/.codex/sessions/**/rollout-*.jsonl`；`~/.codex/{state_5,goals_1,memories_1}.sqlite`
> - Claude Code：`~/.claude/projects/` 下 140 个会话 `.jsonl`，75,247 个 assistant 轮次；透明代理截获的 45 个 `/v1/messages` 完整请求体（含 system 数组与 24 个工具 schema —— 这些**永远不落盘**，只有出网那一跳能拿到）
> - 参考源码：`/Users/moses/workspace/custom-app/openai-agents-python`（`src/agents/` 全部）、`/Users/moses/workspace/custom-app/codex`（`codex-rs/` 全部，含 `core/gpt_5_codex_prompt.md`、`hooks/`、`execpolicy/`、`memories/`）。引用一律带文件路径与行号，便于复核。
> - `~/Downloads/` 下 `AF_vs_Codex_工具集对齐分析.md`（Codex 三手法 + guard 影响判定矩阵）、`CC实证_系统提示词全文.md`、`CC实证_内部机制分析_对照AF.md`（3-block system + 24 工具尺寸 + mid-conversation-system）、`Codex对话记录输出结构与Agent架构分析.md`（双通道日志 + 288 轮长任务实测）
> - 同目录 `Codex_Tools_and_System_Prompts_Analysis.md` / `Claude_Code_Tools_and_System_Prompts_Analysis.md`（全量能力扫描，口径 ③；提供 `codex_app` 17 子工具、`multi_tool_use.parallel`、CC 五类角色 prompt、Auto Mode Classifier 四级）

### A.0 ⚠️ 工具计数口径（不对齐会得出相反结论）

同一个 agent 会被数出完全不同的工具数，取决于口径。**本计划的所有预算与上限都用口径 ①**：

| 口径 | Codex | Claude Code | 说明 |
| --- | ---: | ---: | --- |
| **① 单次请求 advertise（本计划采用）** | **16** | **24** | 从出网请求体的 `tools[]` 直接数。**这是模型每轮真正看到、真正付 token 的数量** |
| ② 常规档 advertise | 7 | ~15 | 非满配会话 |
| ③ 全量可达（含 namespace 子工具 + MCP + 插件 + 动态注入） | **~44** | **29+** | 跨会话/配置/二进制扫描出的全部能力 |

口径 ③ 的数字（见同目录 `Codex_Tools_and_System_Prompts_Analysis.md`、`Claude_Code_Tools_and_System_Prompts_Analysis.md`）**不与口径 ① 矛盾，反而是 deferred/namespace 机制的直接证据**：Codex 的 `codex_app` 命名空间下有 **17 个子工具**（create_thread / fork_thread / handoff_thread / list_threads / read_thread_terminal / send_message_to_thread / wait_threads / automation_update / …），但在请求里**只占 1 个入口、1,303 B**。

> **这条对 rusty-agent 的意义**：能力多 ≠ schema 大。目标不是"只做 16 个能力"，而是"**每轮只 advertise 15 个入口**，其余靠 namespace 折叠与 `tool_search` 按需发现"。

### A.1 Codex 完整工具面（16 个 advertise，gpt-5.5 档）

| # | type | name | schema 字节 | 参数 | rusty-agent 对应 |
| ---: | --- | --- | ---: | --- | --- |
| 0 | function | **`exec_command`** | 1635 | cmd, workdir, shell, tty, login, timeout/`yield_time_ms`, `max_output_tokens`, `prefix_rule`, `sandbox_permissions`, justification | R8-1 |
| 1 | function | `write_stdin` | 819 | session_id, chars, `max_output_tokens`, `yield_time_ms` | R8-2 |
| 2 | function | `list_mcp_resources` | 651 | server, cursor | R11-2 |
| 3 | function | `list_mcp_resource_templates` | 740 | server, cursor | R11-2 |
| 4 | function | `read_mcp_resource` | 552 | server, uri | R11-2 |
| 5 | function | `update_plan` | 781 | explanation, plan[] | R2-8 (todo) |
| 6 | function | `request_user_input` | 2097 | questions[1-3], autoResolutionMs | R13 (宿主交互) |
| 7 | **custom** | **`apply_patch`** | 850 | **freeform**（明写 "do not wrap the patch in JSON"） | R8-4 |
| 8 | function | `view_image` | 554 | path, detail | R2-8 |
| 9 | namespace | `collaboration` | 4592 | 子工具：`spawn_agent` / `followup_task` / `interrupt_agent` / … | R12 |
| 10-12 | function | `get_goal` / `create_goal` / `update_goal` | 297/785/1963 | objective, token_budget, status | R3-8 (task_budget) |
| 13 | namespace | `codex_app` | 1303 | **17 个子工具**：create_thread / fork_thread / handoff_thread / get_handoff_status / list_threads / read_thread / read_thread_terminal / send_message_to_thread / wait_threads / set_thread_{archived,pinned,title} / list_projects / navigate_to_codex_page / open_in_codex / automation_update / load_workspace_dependencies | 宿主专有不抄，但**"17 个能力压进 1,303 B"是 namespace 折叠价值的最强证据** |
| 14 | tool_search | — | 2040 | query, limit（BM25 检索延迟加载的工具元数据） | **R2-5c**（advertise 集第 13 个，是工具面能压到 15 的前提；不是可选项） |
| 15 | web_search | — | 95 | — | R2-8 |

**关键读数**：
- 合计 **19,786 B ≈ 4,946 token**，instructions 16.3 KB ≈ 4,074 token → **前缀总量 ~9k token 且全程不变**，100% 走缓存。
- 常规档只开 **7 个**工具；16 是满配。
- **干活工具极轻**：`exec_command` + `apply_patch` 合计仅 2,485 B。重的是协作/编排（`collaboration` 4592、`tool_search` 2040、`request_user_input` 2097、`update_goal` 1963）——**工具预算花在编排，不花在把文件操作拆成 N 个原子工具**。
- 调用分布：`exec_command` 4,992 + `CUSTOM:exec` 4,386 占绝对主体，`apply_patch` 514。**rg/sed/jq/git/nl 全经 exec_command**。

**Codex 省 schema 的三个手法**（对应 R2-5 / R2-5b / R2-5c）：
1. **单 exec 收编**：`exec_command` + `write_stdin` 一套 PTY 覆盖 shell + 后台会话 + 交互式，不给每个动作独立工具；
2. **namespace 折叠**：`collaboration` / `codex_app` 是 namespace（1 入口代表一组），子工具不 advertise；
3. **deferred + `tool_search`**：长尾工具不 advertise，靠 BM25 检索发现并按需加载。落库形态：`thread_dynamic_tools(thread_id, position, name, description, input_schema, defer_loading, namespace)`。

> **advertise ≠ 拥有**：registry 里工具一个没删，suppressed 的仍可执行；但模型看不见 schema 就不会主动调用 → 行为层等效于"用不上"。

### A.2 Claude Code 工具面（24 个，≈69,650 B ≈17.4k token）

**按 schema 字节排序（截获的真实 `tools[]` 数组）**：

| 工具 | 字节 | 占比 | rusty-agent 处理 |
| --- | ---: | ---: | --- |
| **`Workflow`** | 21,088 | **30%** | ❌ 不采用（只占 0.2% 调用） |
| `AskUserQuestion` | 5,028 | 7% | ✅ 融合进 `ask_user` |
| `EnterPlanMode` | 4,324 | 6% | ❌ 做成运行模式不是工具 |
| `ScheduleWakeup` | 4,096 | 6% | ⏸ defer |
| `CronCreate` | 4,063 | 6% | ⏸ defer |
| `EnterWorktree` | 4,034 | 6% | ⏸ defer |
| `Agent` | 2,981 | 4% | ✅ 折叠进 `agent.*` namespace |
| `Bash` | 2,698 | 4% | ✅ 由 `exec_command` 取代 |
| 其余 16 个（Read/Edit/Write/Skill/Web*/Todo…） | 21,338 | 31% | 选择性采用 |

**反直觉的关键读数**：干实事的 `Bash+Read+Edit+Write` 合计才 **6,039 B（9%）**；token 大头在编排/调度/交互类（Workflow+Cron+Worktree+Agent+Ask+Wakeup ≈45 KB ≈**65%**）。Codex 同构（大头在 `collaboration`/`tool_search`/`request_user_input`）。**→ 两家的工具预算都花在编排，不花在把文件操作拆成 N 个原子工具。**

**单会话 6,234 次调用分布**（最大会话）：

| 工具 | 次数 | 占比 | 类别 |
| --- | ---: | ---: | --- |
| `Bash` | 3,212 | 51.5% | 执行（收编 grep/sed/git/jq） |
| `Read` | 1,295 | 20.8% | 多模态读取入口 |
| `Edit` | 1,007 | 16.2% | 精确改文件 |
| `TodoWrite` | 194 | 3.1% | 任务清单 |
| `TaskOutput` | 191 | 3.1% | 取子代理输出 |
| `Agent` | 102 | 1.6% | 派生子代理 |
| `Write` | 82 | 1.3% | 写文件 |
| `SendMessage` | 57 | 0.9% | 代理间通信 |
| `AskUserQuestion` | 43 | 0.7% | 向用户提问 |
| `ScheduleWakeup` / `TaskStop` / `Workflow` | 15 / 15 / 12 | 0.7% | 自唤醒 / 止损 / 编排 |
| `EnterPlanMode` / `ExitPlanMode` | 5 / 4 | 0.1% | 计划模式 |

`Bash + Read + Edit` = **88.5%**。→ **CC 的 24 是"编排能力多"，不是"干活工具多"。**

### A.2b Claude Code 请求形态（透明代理截获的 `/v1/messages`）

**system 是 3 个 block 的数组，不是单条字符串**：

| block | 字节 | `cache_control` | 内容 |
| ---: | ---: | --- | --- |
| [0] | 80 | **无** | `x-anthropic-billing-header: cc_version=2.1.211.3c8; cc_entrypoint=claude-vscode;`（**每次可能变**） |
| [1] | 94 | `ephemeral` | 身份句 `You are Claude Code, Anthropic's official CLI…` |
| [2] | 7,782 | `ephemeral` | 行为正文 |

**block[2] 的章节结构（"行为宪法"，rusty-agent 稳定前缀的直接参照）**：安全边界 → `# Harness`（输出是 markdown / 工具在权限模式后 / `<system-reminder>` 是 harness 注入非用户 / 优先专用工具 / 独立调用可并行 / `file:line` 可点击）→ 写码风格（match surrounding code）→ 代词中立 → 可逆性与如实报告 → `# Session-specific guidance` → `# Memory` → `# Environment` → `# Context management` → IDE 专属段。

> **缓存断点摆位就是 79.7% 命中的机制本体**：易变计费头放最前但**不设** cache_control（不进缓存前缀），稳定的身份+正文各设 ephemeral 断点。**24 个工具无一带 `cache_control`**——tools 由 system[2] 之后的整体前缀缓存覆盖。

**顶层控制字段**：`max_tokens=64000`、`thinking={type:adaptive}`、`output_config={effort:xhigh}`、`stream=true`、`metadata.user_id`（含 device_id/session_id）。
**beta 头**：`claude-code-20250219, interleaved-thinking-2025-05-14, mid-conversation-system-2026-04-07, effort-2025-11-24`。

**消息结构**（181 条 messages 的样本）：role 分布 `user 83 / assistant 82 / system 16`；block 分布 `text 94 / thinking 45 / tool_use 71 / tool_result 71`（**71 对完全配平**）。
- 工具调用三段式：assistant 出 `tool_use{name, id, input}` → 紧跟 user 的 `tool_result{tool_use_id, is_error, content}`，**靠 id 配对不靠顺序**；**失败走同一通道**（`is_error:true`），不另开机制。
- `thinking{type, thinking, signature}` 独立块，signature 约 1200 字符加密签名。
- **16 条 `role:system` 消息插在 messages 流里**（beta `mid-conversation-system`），两种用途：(a) 某些工具结果被重写成 system 叙述；(b) **动态提醒**（"The TodoWrite tool hasn't been used recently…"）——这就是会话 jsonl 里 `todo_reminder` attachment 在网络层的真身。**→ 直接印证 R4-3：提醒不进前缀，走 messages 尾部。**

### A.3 Codex 执行事件流

**落盘载体**：`~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<thread_uuid>.jsonl`，一文件 = 一 thread，行级 append-only（崩溃不破坏已写内容），663 B ~ 153 MB。

**四类顶层记录**：

| `type` | 作用 |
| --- | --- |
| `session_meta` | 会话头（首行）：id / cwd / originator / cli_version / model_provider |
| `turn_context` | 每轮运行上下文：cwd / approval_policy / sandbox_policy / model / effort |
| **`response_item`** | **送往/来自模型的 wire 协议**：`message` / `reasoning` / `function_call{name, arguments, call_id}` / `function_call_output{call_id, output}` |
| **`event_msg`** | **驱动 UI 的事件流**（数量最多） |

**`event_msg` 全量频次**（最能反映真实运行画像）：

```
token_count 21785 | agent_message 14680 | agent_reasoning 5770
patch_apply_end 5069 | user_message 857 | task_started 840
task_complete 795 | web_search_end 424 | context_compacted 291
mcp_tool_call_end 114 | turn_aborted 47 | thread_rolled_back 37
```

- `agent_reasoning` / `agent_message` 是 `reasoning` / `message` 的 **UI 镜像**（同内容，给前端流式渲染）——所以同一段思维链会出现两次。
- `task_started` 带 `model_context_window`；`task_complete` 带 `last_agent_message`（最终答案全文）。
- `patch_apply_end` 带 `changes` 全 diff + `success` + stdout/stderr。
- `thread_rolled_back` / `turn_aborted` → 记录支持**时间旅行/撤销**。

**一轮的典型时序**：

```
turn_context（本轮 cwd/sandbox/model/effort）
└─ event_msg: task_started (model_context_window)
   ├─ response_item: reasoning      ┐ 同步镜像
   ├─ event_msg: agent_reasoning    ┘
   ├─ event_msg: agent_message（报幕，phase=commentary）
   ├─ response_item: function_call (name=exec_command, call_id=X)
   ├─ event_msg: token_count
   ├─ response_item: function_call_output (call_id=X, output=…)
   │   …（多轮 action/observation）…
   ├─ event_msg: patch_apply_end (changes diff)
   └─ event_msg: task_complete (last_agent_message=最终答案)
```

**系统提示词结构**（4 个顶层节，全文 16.3 KB）：`# Personality`(Writing style / Technical communication) → `# Working with the user`(Intermediate commentary / Final answer) → `# Rules for getting work done`(File editing constraints / Autonomy and persistence) → `# Using skills`。整套工具纪律 + 自主性纪律**不到 2 KB**，且全是软倾向（`prefer rg`、`prefer parallelization`、`without fuss`），**没有一条硬 gate**。桌面版另有 Engineering Judgment 与 Frontend & Design 段。

**协议层双通道**（`payload.phase`）：

```
turn 开始
  ├─ response_item{type:message, role:assistant, phase:"commentary"}   ← 报幕：1-2 句说要做什么
  ├─ response_item{type:custom_tool_call, name:"exec_command", input}  ← 动作
  ├─ response_item{type:function_call_output, output}                  ← 观察
  ├─ (重复 commentary → tool_call → output …)
  └─ response_item{type:message, role:assistant, phase:"final"}        ← 收尾交付，一 turn 只发一次
```

**请求层生命周期**（同一 thread 25 次请求的实测时间线）：

| kind | n_input | n_tools | 说明 |
| --- | ---: | ---: | --- |
| `turn` | 9 → 85 | 16 | 正常推进，input items 逐步累积 |
| `compaction` | 122 | **0** | 压缩请求**不带工具 schema** |
| `turn`（压缩后首轮） | **9** | 16 | 122 项 → 9 项 |

**缓存机制**：`prompt_cache_key == thread_id`，**每会话唯一值**（21/21 会话验证）；`instructions` 长度每会话单值；工具面大小每会话单值。→ 命中 89.5%，21 轮里 14 轮在 90%+。

**行为轨迹**：explore（`rg`/`sed`/`nl`/`git status`）→ 用真实数据纠偏（`jq` 核对、读源码确认，不轻信文档旧数字）→ plan（`update_plan`）→ edit（`apply_patch`，克制、只动该动的）→ verify（`git diff --check` / `--stat` 自检）。平均每轮 output **1,418 token**（reasoning 占 39%）。

### A.3b Codex 长任务实测：三条修正性结论

> 方法：扫描全量会话按 `function_call` 次数排序，取最重的真实任务逐条解析。

**① 循环形态是「一思 → 批量并行动作 → 批量观察」，不是「一思一动」**

```
reasoning → ACTION(exec) ACTION(exec) ACTION(exec) → observation×3 → reasoning → [报幕] → ACTION …
```
一次 `reasoning` 后常跟多个并排 `exec_command`（例如一次 3 个 `sed` 读不同行段）。**这是它读代码快的结构性原因。** → R3-4b

**② 重度真实任务里多 agent 零激活**

最重会话（改 compaction 逻辑）实测：

| 指标 | 值 |
| --- | ---: |
| task 轮次 | 288 起 / 271 完成 / 17 中断（自洽） |
| `exec_command` | **11,215** |
| `update_plan` | 384（全程维护计划板） |
| **`context_compacted`** | **128**（长任务里压缩是常态不是例外） |
| `thread_rolled_back` | 6 |
| **结构化 spawn 事件** | **0** |

「spawn」关键词命中 3,057 行，逐条甄别**全是文本噪声**（源码文件名 `spawn.rs`、shell fork 日志、JS `execFile,spawn`、用户 markdown 里的"多 agent | TODO"）。`spawn_edges` / `agent_jobs` / `thread_goals` 三张表在静态文件里为空。→ **R12 整体后移**

**③ 「39 MB 大文件 ≠ 长任务」——首轮超大输入卡死的真实样本**

两个 39 MB 会话各只有 **10 行**，体积来自其中**一条 19.9 MB 的用户输入**（大量代码上下文一次性粘入），被双通道镜像成 `response_item.message` + `event_msg.user_message` 两份 ≈ 40 MB。且 `task_started=0`、`function_call=0`——**模型那轮根本没跑起来**。根因：压缩触发条件挂在 messages 数量门槛上，单条巨型输入不满足。**Codex 也有此缺陷，rusty-agent 不要复刻。** → R5-6

### A.3c Codex 的其它子系统（多数进 Deferred）

- **双运行器**：`AgentRunner`（交互，`task_started → ReAct → task_complete`）+ `GoalRunner`（自治，由 `thread_goals` 驱动）。自治侧有 **token/时间双闸**：`token_budget + tokens_used + time_used_seconds`，到顶进 `usage_limited` / `budget_limited` 态而非无限烧。→ R3-8 的 `task_budget` 参照
- **四库分工**：`state_5`（threads / agent_jobs / spawn_edges / **thread_dynamic_tools** / remote_control）、`goals_1`（自治目标+预算）、`memories_1`（记忆管线）、`logs_2`（结构化日志）。均用 sqlx 迁移 → 印证内核是 Rust
- **记忆异步管线**：`stage1_outputs`（每会话蒸馏 raw_memory + rollout_summary）→ `selected_for_phase2`（择优提炼）；`jobs` 是带**租约 lease + 水位 watermark + 重试**的后台队列（幂等 backfill）。**主循环零额外延迟、可崩溃恢复** → Deferred，但设计形态照抄
- **CSV 行级批处理**：`agent_jobs`（instruction / output_schema_json / input_csv_path / max_runtime_seconds）+ `agent_job_items`（row_json / attempt_count / result_json）→ Deferred（实测零激活）
- **Skills 渐进式披露**：目录形态 `SKILL.md`(YAML frontmatter: name/description + Workflow) + `agents/*.yaml` + `scripts/` + `references/`；**description 决定何时加载，正文才进上下文** → R11-8
- **Plugins marketplace**：`[marketplaces.*]` + `[plugins."name@marketplace"]`，按 `cache/<market>/<name>/<version>/` 版本化缓存 → R11-10

### A.4 Claude Code 执行事件流

**会话记录类型**（append-only JSONL，最大单会话 81 MB / 21,397 条）：

| 类型 | 条数 | 作用 |
| --- | ---: | --- |
| `assistant` | 11,520 | 模型输出（text / thinking / tool_use / fallback 块） |
| `user` | 6,958 | 用户输入 + 工具结果回填 |
| **`attachment`** | 1,380 | **动态上下文增量注入**（见下） |
| `last-prompt` | 436 | 断点续跑的叶子指针 |
| `mode` | 435 | 权限/计划模式切换 |
| `queue-operation` | 366 | 运行中排队指令 |
| `file-history-snapshot` | 207 | 文件编辑前快照（可回滚） |
| `system` | 95 | `compact_boundary` / 模型回退 / API 错误 |

**`attachment` 增量注入机制（CC 最独特的一环，delta 而非 full）**：

| 类型 | 次数 | 机制 |
| --- | ---: | --- |
| `todo_reminder` | 898 | 待办状态投影，随进度增量刷新 |
| `queued_command` | 144 | 生成期间用户排队的指令 |
| `file` / `edited_text_file` | 83 / 45 | 文件内容 / 外部编辑注入 |
| `compact_file_reference` | 66 | 压缩后只留**路径引用**，不留全文 |
| `ultra_effort_enter` | 63 | 高强度推理模式开关 |
| `agent_listing_delta` | 37 | 字段就叫 `addedTypes`/`addedLines`——**只注入新增，不重发整表** |
| `date_change` | 24 | 跨天只发 `{newDate}` |
| `skill_listing` | 8 | 可用 skill 清单 |

→ 净效果：**变化的才注入，且以最小 delta 注入**，前缀（系统提示 + 工具定义 + 早期历史）长期字节级不变 → 缓存持续命中 79.7%。
→ **对 rusty-agent 的直接指令**：R4-3 的 `RuntimeReminder` 必须做成 **delta attachment**，不是每轮重渲染的 full 段。这是 R4 最具体的落地形态。

**压缩机制**（`system.subtype = compact_boundary`，100 次带数据的样本）：

```json
{ "trigger":"manual", "preTokens":274396, "postTokens":9254, "durationMs":95451,
  "preservedSegment": {"headUuid":"…","anchorUuid":"…","tailUuid":"…"},
  "cumulativeDroppedTokens": 265142 }
```

- preTokens 中位 **323,791**；postTokens 中位 **9,254**；压缩比中位 **37.1×**。
- **锚点保留**：head/anchor/tail 三个 UUID 锚住必须保留的尾段，非无差别截断。
- **固定 9 段式 summary** 作为新起点：`1 Primary Request and Intent / 2 Key Technical Concepts / 3 Files and Code Sections / 4 Errors and fixes / 5 Problem Solving / 6 All user messages / 7 Pending Tasks / 8 Current Work / 9 Optional Next Step`。**第 6 段逐条保留所有用户消息**，避免丢意图。→ 直接作为 R5-3 的实现规格。
- **文件引用降级**：大文件全文换成 `compact_file_reference` 路径引用，需要时重读。

**子代理机制（控制主上下文膨胀最狠的一招）**：

```json
{ "isAsync": true, "status": "async_launched", "agentId": "ac9621da3a1d3d45a",
  "resolvedModel": "claude-fable-5", "outputFile": "…/tasks/<id>.output",
  "canReadOutputFile": true }
```

- 异步启动不阻塞主线程，可并行多个；
- **子代理完整轨迹写独立 `outputFile`，主上下文只留 agentId + 文件指针**，几万 token 的探索过程完全不进主上下文；
- 主线程用 `TaskOutput` 按需取回结论（只取需要的部分）；
- `SendMessage` 可对停滞子代理发"止损催收"，强制其立即收敛交付。
- → 这是 **R12-2/R12-7 的实现规格**：子 agent transcript 落文件/subkey，父级只持有指针。

**模型分层与回退**：单会话 `opus-4-8[1m] 6379 / opus-4-8 3653 / fable-5 1479`；`system.subtype = model_refusal_fallback` 出现 38 次（`fable-5` 拒答 → 自动回退 `opus-4-8`）。**便宜模型打头、拒答/出错自动升级，是运行时硬机制不是提示词请求。** → 补入 R1-4/R1-5 的 `fallback_model` 语义。

**缓存窗口感知**：`ScheduleWakeup{delaySeconds: 270}`，注明"保持缓存窗口内的兜底唤醒"——**270 < 300 秒缓存 TTL 是刻意的**。连"等待"都在为缓存命中服务。

### A.5 三方成本对照（同 provider / 同模型）

> **下表的第三列是历史对照，2026-09-09 起不作为设计论据；前两列的数字同样需要回到源码或第一手日志核实。**

| 指标 | Claude Code | Codex | AgentForge (pre-P69) |
| --- | --- | --- | --- |
| 每请求上下文 | ~413,000 tok | ~168,000-200,000 tok | 440,000-790,000 tok |
| **缓存命中率** | **79.7-89.8%** | **89.5%** | **13-46%** |
| 每请求付全价的新 token | **4 tok** | 低 | 高（前缀抖动，每轮重付） |
| 工具数 | 24 advertise（活跃 ~15；全量 29+） | **16 advertise**（常规 7；全量 ~44） | 43 |
| 工具 schema 总量 | ~69.6 KB / 17.4k tok | **~19.8 KB / 4.9k tok** | 47.6 KB / 11.9k tok |
| 系统提示 | 3-block 数组 + cache_control | 16.3 KB 单值全程不变 | 8 段 stable + 动态段（抖动） |
| 平均 output / 轮 | **1,033 tok** | 1,418 tok | 偏高 |
| 同任务 total token | — | **202,758** | 437,818-701,600（3.4×） |

**rusty-agent 的目标线**：工具数 14-16、schema ≤ 20 KB、缓存命中 ≥ 70%（对标 89%）、平均 output ≤ 1.5k token、同任务 total token 与 Codex 差距 ≤ 20%。

---

## 附录 B：Codex 与 Claude Code 系统提示词的冲突裁决

> 依据：`Codex_Tools_and_System_Prompts_Analysis.md`（Codex 主 prompt 16.3 KB 全文）、`CC实证_系统提示词全文.md`（CC 主 prompt block[2] 7,782 B）、`Claude_Code_Tools_and_System_Prompts_Analysis.md`（CC 5 个角色 prompt）。

### B.0 先分清层级：大部分"冲突"是错觉

| 层 | Codex | Claude Code | 关系 |
| --- | --- | --- | --- |
| **主 agent 行为宪法** | 16.3 KB，4 顶层节 | 7,782 B block[2] | **同层，逐条会冲突** |
| **角色/子 agent prompt** | 未捕获（走 `collaboration` namespace，prompt 不出网） | 5 个：Coordinator / ReadOnlySpecialist / Planner / OneOffAnswer / AutoModeClassifier | **Codex 无对应物，纯补充，零冲突** |

**结论：Codex 给主 prompt 骨架，CC 给角色 prompt。** 只有主 prompt 那一层需要裁决。

### B.1 真冲突逐条裁决

| # | 议题 | Codex | Claude Code | 裁决 | 理由 |
| ---: | --- | --- | --- | --- | --- |
| 1 | 编辑机制 | `apply_patch` 唯一入口，**明令禁止** `cat`/shell 写文件/Python 读写 | `Edit`（串替换）+ `Write`（覆盖） | **取 Codex** | 已在 R2-8 选了 `apply_patch`。CC 里所有涉及 `Edit`/`Write` 的措辞**必须整段丢弃**，否则提示会指向不存在的工具 |
| 2 | 并行形态 | 工具级：`multi_tool_use.parallel`，且原文强调 *"and only that"* | Agent 级：Coordinator 用 `TaskCreate` 并行派 worker | **取 Codex** | R3-4b 已定循环形态为"一思→批量并行动作"；R12 多 agent 后移。CC 的 Coordinator 段整体挂起到 R12 |
| 3 | 计划模式 | 散文式 implementation plan，无专用工具 | `EnterPlanMode`/`ExitPlanMode` + 只读 Architect 子 agent | **形式取 Codex，语义取 CC** | 不做工具（省 4,324 B），但**保留"计划期强制只读"这个语义**，做成 permission mode 的一档 + `PromptRole::Planner` |
| 4 | 格式规则 | 极细：header 用 `**…**` 且 1-3 词 Title Case、只用 `1. 2. 3.` 不用 `1)`、禁嵌套 bullet、禁 emoji/em dash、file link 用 `[label](/abs/path:12)` | 只有一句 "displayed as GitHub-flavored markdown" | **取 Codex** | 具体度完胜；CC 那一句可作为兜底 |
| 5 | 报幕机制 | `commentary` / `final` 双通道 + **每 30s 一次更新** | 无 phase 概念，text block 直接输出 | **取 Codex** | 已定 R3-10 双通道 |
| 6 | 人格表达 | `pragmatic`(默认)/`friendly` 两档 + Clarity/Pragmatism/Rigor + 反客套禁令 | "match surrounding code" + "report outcomes faithfully" | **合并** | 方向一致不冲突：Codex 给人格骨架，CC 的"如实报告失败/跳过"补进去 |
| 7 | 前端设计指令 | 有大段（图标用 lucide、卡片圆角 ≤8px、禁 orb、Three.js full-bleed…） | 无 | **取 Codex 但懒加载** | 常驻会污染前缀且与多数任务无关；按信号加载（R10-5 通道已落地，正文见 R4-0h） |

### B.2 CC 独有、可直接用上的四件

| 件 | 价值 | 落到 |
| --- | --- | --- |
| **READ-ONLY 的"能力不可用"表达** | 不是"请你别写"，是 *"You do NOT have access to file editing tools - attempting to edit files will fail."* —— **提示与工具面一致**的正确写法 | R4-8a |
| **只读模式的 shell 逃逸清单** | 逐条列出 `touch`/`rm`/`mv`/`cp`/`>`/`>>`/管道/heredoc/`/tmp` 临时文件。**对我们尤其要命**——`exec_command` 收编一切之后，只摘编辑工具挡不住写 | R4-8b |
| **`OneOffAnswer` 的三条硬约束** | 不提"被打断"、禁说 `Let me check...`、不知道就说不知道且不提议去查。**Codex 完全没有这个模式** | R4-8c |
| **`soft_deny` vs `hard_deny` 的"用户意图能否解除"** | 分界不是"多危险"，是**用户说了算不算数**。比四个标签本身更重要 | R6-8b |

### B.3 计数口径说明

`Claude_Code_Tools_and_System_Prompts_Analysis.md` 的 **29** 是全量可达口径：24 项内置 advertise 工具 + 5 项 MCP 外挂接口（`mcp__Claude_Browser__computer` / `navigate` / `get_page_text` / `preview_start`、`mcp__ccd_session__mark_chapter`）。预算与开发计划扣掉 MCP 外挂后，与抓包口径的 **24** 一致（见 [A.0 计数口径](#a0-️-工具计数口径不对齐会得出相反结论)）。

**规划一律以 24 为准**，避免按 29 去对齐而虚高工具预算。

---

## 近期待办清单

- [x] R0-1 Cargo workspace 与 crate 边界（14 crate facade + 内部 `pub(crate)` 收口 + 直接依赖白名单 + `feature-matrix` 门禁逐 crate 验全关/全开 + rustc 负向夹具）
- [x] R0-2 错误与结果基线（子系统 × 可恢复性两维，投影而非存储；18 条断言）
- [x] R0-3 日志与 span 分类（八分类词表 + 字段常量 + 级别投影；23 条断言；零 subscriber、零外部后端）
- [x] R0-4 取消与超时基线（作用域树 + 根因先到先得 + 时限只收紧；31 条断言；`Docs/Cancellation_Contract.md`）
- [x] R0-5 配置模型（六层 + 显式来源选择 + 隔离开关 + 来源可诊断；25 条断言。**机制已备，文件发现与解析待 `ra-cli`**）
- [x] R0-6 CI 与质量门（CI 三 job + 九条门禁的三态运行器；`layering` / `no-inline-tests` / `public-api` / `feature-matrix` / `test` 真执行，其余四条报 SKIP 并计数；`cargo deny` 四项全绿）
- [x] R0-7 独立测试 workspace（进版本库；宿主清单从 `crates/` 推导 + `it-e2e`；空测试拒绝门禁；136 条有效断言；零内联测试）
- [x] **R0-8 扩展面契约落地（`#[non_exhaustive]` / builder / sealed / 开放标签 / schema_version / public-api 基线 / 稳定性分级）**
- [x] **`examples/minimal_agent`：不依赖 `ra-coding` 的最小自定义 agent（框架通用性的唯一硬证据）**：只依赖 `ra-core` + `ra-runtime`，自带 `Tool` / `Model` / `ModelResolver`，离线跑完两轮。**约束由 `xtask` 的 `ALLOWED_INTERNAL_DEPS` 钉死**（`&["ra-core", "ra-runtime"]`），加第三个内部依赖即 FAIL 并点名，已反证。落地时暴露两件事，均已处理：`Model::get_response` 在 runtime 已无调用点（已裁定——`stream_response` 改带默认实现，见 R1-3）、最终文本无出口（已补 `RunResult::final_text()`）
- [x] R1-1 项模型 `RunItem` / `ModelResponse`
- [x] R1-2 内容块与多模态（七种 `ContentBlock` + base64 / 本地路径图片）
- [x] **R1-2b `ModelSettings` 与四层 resolve 语义（四层不可变 resolve + per-provider `extra_body` 深合并）**
- [x] R1-3 `Model` / `ModelProvider` trait
- [x] R1-3a Provider 注册与模型名前缀解析
- [x] R1-5b `ApiProtocol` 能力矩阵（协议差异显式建模）
- [x] R1-4 OpenAI Responses provider
- [x] **R1-6 OpenAI Chat Completions provider（convert / stream / reasoning 三件，R1 最大单项；§2 七条 + §3 五条各一条测试，28 条契约测试）**
- [x] **R1-6b OpenAI 兼容端点 compat 层（`CompatEndpoint` 一处描述一个端点；Quirks 七项齐；keyless 本地端点；两处静默失败改响亮失败；14 条契约测试）**
- [x] **R1-7 流式与部分消息（`ModelStreamEvent::Completed` 终态 backfill、`with_partial_messages` 只转发旁白、Responses 真实 SSE；12 条契约测试）**
- [x] **R1-9 错误归一化与重试（`NormalizedProviderError` 挂 source 链、`RetryBackoff` 注入抖动、重放判据由发出层盖章且 stateful 请求只给 `Unknown`；26 条契约测试）**
- [x] **R1-9b retry policy 与流式重试边界（策略与持久配置分离；硬上限 / 可取消退避 / 失败请求 usage；公开流事件后绝不 replay）**
- [x] **R1-10 Provider 兼容矩阵 smoke（同一份中性请求内联快照四种 payload；Anthropic 只出 `#[doc(hidden)]` preview 且宁可报错也不产出错形状；Responses↔Chat reasoning / tool-pair 双向往返）**
- [x] **R1-5 Anthropic Messages provider（真 adapter：传输 / lowering / lifting / SSE / 错误分类；`cache_control` 打在单块 system 上、门槛按 tools+system 合计；`output_config.format` 与 `tool_choice:none` 按当代 API 下发；`redacted_thinking` 逐字回放；坏帧报错不 panic、失败流即终止；tool_result 保持在 user 轮开头；9 条契约测试。preview 退休归 R1-14）**
- [x] R1-17 输入项规范化与 reasoning replay
- [ ] R1-18 可选 LiteLLM / any-llm bridge
- [x] R2-1 `Tool` trait 与 `ToolOrigin` 身份键
- [x] R2-3 工具结果结构化（结构给宿主、散文给模型；截断是列表不是字段）
- [x] **R2-4 工具注册表与 profile（内核只出机制：注册表按 lookup key 唯一、profile = 选择 + 必填预算、`assemble` 单向出 `ToolSurface`；`core(6-8)` / `codex_like(14-16)` / `full(≤24)` 与 20 KB 都落在 `ra-coding`，因为它们是 coding profile 的策略不是框架上限。同名双 server 可注册、不可同面，但名字唯一性只管「进得了 tool list 的那些」（`ToolOptions::can_reach_model_surface()`：`Hidden`/`Disabled` 不占名字，`Deferred`/`Dynamic` 占）；`Hidden`/`Deferred`/`Disabled` 不计费而 `Dynamic` 计费；字节口径统一到 `ModelToolDefinition::advertised_bytes()`。**core 今天仍装配不出来且必须响亮失败**——清单即规格）**
- [x] **R2-12 `ra-tools` 通用工具库拆分（比计划更早——趁 12 个工具还都是模块桩子，搬运是零成本）**
- [x] **R3-0 turn 准备阶段的固定顺序（tools → handoffs → schema → model → settings → filter）**
- [x] R3-1 `NextStep` 四态状态机
- [x] **R3-1b `FinishReason` 结构化终止原因（R17 图边 / R15 final 成型 / R13 UI 三处依赖）**
- [x] R3-2 `ProcessedResponse` 分类（类型在 `ra-core::step`，解析在 `ra-runtime::turn::process`）
- [x] R3-3 `SingleStepResult`（四类 guardrail 结果留给 R7 补，不占位）
- [x] R3-4 turn 结算主流程（流式批派发 R3-4b、并发收口 R3-4c、tool-stop R3-5、交接 R17 各留一个插入点）
- [x] **R3-1c `AgentSpec` builder 与不可变契约（编号在后，执行顺序排 R3 第一——R3-0 与 R3-7 都以它为入参）**
- [x] **R3-4c 并发工具的结算与取消语义（失败选择 / 迟到合并 / 取消排空，R17-4 复用同一套）**
- [x] **R3-4d 资源 / effect 级并发准入（保留 v1 工具级锁；resource claim 由 runtime admission 收口）**
- [x] R3-6b `AgentToolUseTracker`
- [x] **R3-6c 结构化失败记录与无进展熔断（不用独立 Reflector 长文）**
- [x] R3-7 `Runner::run` / `run_streamed`（两个入口共用一个 loop；`max_turns` 是 loop 的终止条件，其余预算留 R3-8）
- [x] **R3-8b 最小可观测性基线（tokens/cache、wall time、准入/执行分段、budget outcome）**：`agent` / `turn` / `generation` / `function` 四类 span 已接到 runtime 的真实执行边界。generation 记规范化的 input / cached-input / cache-write / output / reasoning token 与调用墙钟；turn 在 settlement **之前**记本轮 usage，所以随后失败或被取消的工具抹不掉已付费的调用；function 分开记准入等待、handler 执行与总时长，且 span 在调用侧创建后再交给 spawn 出去的任务（tokio 不跨 spawn 传 tracing 上下文，写在任务体里会变成游离根 span）；agent 记聚合 usage、墙钟、`finish.reason` 与 `budget.kind`，失败与取消路径同样保留已消耗的 usage。取消带根因与**发起层级**（新增 `CancelScope::cancelled_scope()`）。字段只含身份、计数、时长与稳定 code，不含 prompt、模型正文、工具参数或输出。当前 runner 尚无自动 retry，compaction 也未进入 loop，资源准入计数要等 R3-4d，因此**不伪造** retry / compaction / claim 数值；词表已为后续真实执行路径预留，R14 负责聚合与设门。`it-runtime` 用独立测试二进制捕获真实 span，9 条断言覆盖嵌套关系、usage、时延分段、错误与取消分类、重放确定性与敏感内容隔离。
- [x] **R3-9a `RunContext` / `ToolContext` 与 `RunId` 契约（直接演进 `ToolInvocation`，与 R12-B 的 `ToolServices` 一次改完；`RunState` 的 `run_id` / event seq 仍归下一条最小 R6-6a identity slice，之后才进 R8-0）**
- [x] R3-12 public agent / execution agent 绑定
- [x] **R3-13 `WorkState` 挂载点（只留位不实现；当前槽位落在 `RunRequest` → 结算 → 派发 → `ToolInvocation` 四层；R3-9a 与 `ToolServices` 同次直接演进末端为 `ToolContext`，避免 R17 改全部构造点与所有 `Tool::call` 调用方）**
- [x] **R4-11 动态 prompt 解析与 provenance（放置规则在 `ra-core` 内强制、且两个方向都强制——`ResolvedInstructions::{Prefix, Generated}` 让静态宪章无法被降成 user message；`ResolvedPrompt::lower()` 一次出 items 与 provenance；provenance 随 filter 链一起传递；取消与生成器错误保住分类）**
- [ ] **R4-0a..R4-0h 稳定前缀正文八段（identity / engineering / editing / autonomy / formatting / channels / tool_use / frontend；装配链路与 prompt-dump 门禁已通，缺的只是文本。原先被写作不存在的编号 `R4-0..R4-5`）**（R4-0a identity、R4-0b engineering、R4-0c editing、R4-0d autonomy、R4-0e formatting 已 DONE；editing 认的槽名是 `editing_verification`，核验那一半的文本未写，落地时只能改同一个 builder。autonomy 认独立槽名 `autonomy`，默认连续三次无新证据失败的止损边界与 R3-6c 对齐。formatting 认 `final_answer` 槽作为它在前缀里的规范位置，其余提示词主题在引入时各自决定段边界；余下三段待写。装配器已补上撞名拒绝，后续几段可以安全地各认一个槽名。**注意 token 余量**：host-backed 前缀已到 993 token，距 1024 缓存地板只剩 31，下一段落地大概率越过地板，`test_the_product_prefix_is_still_below_the_caching_floor` 会按设计转红，届时要连缓存计划链路一起复核）
- [x] **R9-0a 最小 rollout append writer（R5-3/R5-4/R5-8 的前置：不可裁剪 usage 基线与 root-thread `timeline_seq`，两项均已交付，R5 的门可以过；单 writer 由 `flock` 强制、非 Unix 不支持 writer；**快恢复不在本条范围内，已拆为 R9-0b**）**
- [x] **R9-0b rollout 内 checkpoint 记录与快恢复（checkpoint 作为普通记录写进 rollout 本体，是一堆原始事实里唯一的派生值，因而可被全扫交叉校验——`scan_summary` 会把它的汇总与实际累加值对账，原地篡改在这里报 `Corrupted`；sidecar 退化为 `last_checkpoint_offset` 索引且会自我修正。取舍：快路径不读 checkpoint 之前的字节，因此**必然信任**它的数字，篡改只在全扫时可见，完整校验由 `RolloutReader::read_all` / `scan_summary` 承担）**
- [x] **R5-3b 压缩 / resume / replay 不得破坏控制面状态（熔断计数、claim、预算累计三条不变量）**：压缩只借用权威 `RunItem` 历史并返回模型投影，不接收或修改 `RunState`；恢复并重放旧 `call_id` 后，失败记录、动态 claim 求值次数与 usage ledger 保持不变，阈值前的下一次失败仍会让后续调用触发 `tool.no_progress`。
- [ ] R5-8 旧轮次工具输出裁剪器
- [x] **R6-6a `RunState` 扩展位预留与 identity slice（九类字段就位；`run_id` / `next_host_event_seq` 为必填 serde 字段，缺任一即拒绝；allocator 只能由 `RunState` 造且四道门焊死序号空间。依赖日志的两条——跨 run `timeline_seq`、`usage_totals` 与 rollout 对账——归 R9-0a；七个占位槽的行为仍归各自里程碑）**
- [x] **R6-6 `RunState` 序列化（schema v1 从这里冻结；checkpoint 自带 run 历史——agent 身份、开场输入、逐段记录、模型响应、待决中断 ID；反序列化会拒绝「有历史无 agent」和「待决 ID 无所指」两种静默失败；续跑基线由请求选，带 input 是调用方自管且为单向门，空 input 才由运行时投影；loop 不再另存一份历史）**：列出的字段里 `session_items` / `last_processed_response` / guardrail 结果 / 工具 `lookup_key` / sandbox 会话引用五格，以及 context serializer/deserializer 与 strict context 两条对齐项，均刻意未做且归属待定，见 R6-6 行末段
- [x] **R8-0 `CodingHost` 与宿主事件信封冻结（`HostEvent` 单信封 + exec / agent 两族；`seq` 只能由 `EventSeqAllocator` 分配、`run_id` 从分配器取，工具经 `ToolContext::event_emitter` 抵达；未知族 / 未知 kind / 未知字段三层降级读都不丢数据；分类标签开放、`ExecStreamKind` 保持封闭；`ra-patch` 改依赖 `ra_core::compat`）**
- [x] **R8-1 `exec_command`（执行脊柱：`ra-tools` 出 schema 与模型可读结果，`ra-exec` 出进程与会话，失败经 `ExecError` 跨层；子进程只有 supervisor 一个所有者，终止是 `SIGTERM` → `DRAIN_GRACE` → `SIGKILL` 打进程组并在 leader 回收后补扫尾；idle/total 两个 deadline 长在 supervisor 里不靠外部清扫；容量算活着的进程组、腾不出就 `AtCapacity` 拒绝；死因 first-reason-wins 且一个会话一条 `ExecEvicted`；manager limits 是 ceiling；schema 里每个参数都有消费者，`tty` 明确拒绝。46 条断言）**
- [ ] **R12-1/2/3/5/6/7 子 agent 内核机制前移到 R6 之前**（R12-1 DONE：`HandoffSpec` 声明、`AgentDefinition` 别名与 `AgentRegistry`、`ActionSurfaceBudget`；**R12-8 DONE**：`NextStep::Handoff` 在 `ra-runtime` 运行循环里落地，含目标解析、历史投影与输入过滤器、`RunState` 投影持久化）
- [x] **R8-3a `write_stdin` 的等待与控制（后台执行闭环）**：`until: output|done|match` + `match_text` + `control: interrupt|cancel` 加在既有工具上而不是开第三个入口；等待时限一律走 `yield_time_ms`，`return_reason` 与 closeout 如实回报；匹配搜全部保留历史、投递只给未投递字节；按 session 的有界投递锁与「被等待的 session 不算空闲」两条并发/存活语义同批做
- [x] **R8-7 沙箱后端 unix_local（基线：环境派生 + 资源上限 + cwd 诚实化）**：`EnvPolicy` 照 codex `ShellEnvironmentPolicy` 六步实现，三处偏离写明（凭据过滤按作用命名且默认开、`set` 在 `include_only` 之后、软硬上限同设）；不做 `RLIMIT_AS` / `RLIMIT_NPROC` 并各记理由；策略挂 `ProcessManager` 不挂 `ExecRequest`；`ra-exec` 局部 lint 表换取一处经审计的 `pre_exec`，工作区仍 `forbid`
- [ ] **R8-P0..P10 OpenAI 执行环境移植**：先契约与差异清单，再 Unix-local、Runner 所有权、Shell/PTY、Filesystem/view_image、snapshot/resume、Docker、instrumentation 与兼容验收；旧 R8 DONE 不表示 parity DONE。
- [ ] **旧 R8-13 coding 临时目录扩展**：底层机制已有，产品接线与跨进程恢复未完成；不阻塞上游 session 移植，按真实产品消费者与 R8-11a 扩展决定后续接线。
- [x] **R8-5b 文件事实进 Session 工具事件**（R8-5 移交项②：`read_file` / `apply_patch` 的读写事实目前根本没有事实来源）
- [ ] R9-12 Session input / persistence 对账
- [x] **R9-2a 最小 `Session` port 与 `SessionId`（执行顺序在 R5-3 / R6-6 之前；`SessionStore` 是它的后端不是上位接口。port 与身份在 `ra-core::session`，参考实现 `InMemorySession` 在 `ra-session::memory`；**验收断言跑在真实实现上、不跑在测试 stub 上**，it-core 只验 port 形状与 object safety；`it-session` 用 `default-features = false`，让「无需 SQLite/JSONL」成为依赖图里可验证的事实；`ConversationContinuation::ConversationId` 已换成 `ProviderConversationId` 且 wire 不变；`SessionId::generate` = `sess-` + UUID v7）**
- [ ] R9-13 服务端 conversation session 边界
- [ ] R9-14 Provider-specific compaction session
- [ ] **R9-15 `WorkState` 持久化（复用 R9-9 checkpoint，不新建 store）**
- [x] **R10-8 记忆三层结构（契约在 `ra-core`、装配在 `ra-tools`、管线 Deferred；不建 `ra-memory` crate）**
- [x] **R10-3 内置 capability 集补齐 10/10（`Todo` / `ViewImage` / `Web` / `Skills` 四族连同 `update_plan` / `view_image` / `web_search` / `web_fetch` / `skill` 五个工具一起落地；`WebAccess` 与 `SkillCatalog` 两个契约新落在 `ra-core`，框架自己不开 socket、不读技能目录；`ra-coding` 只装前两族，后两族没有后端就不装）**
- [ ] R11-6b MCP lifecycle task affinity
- [ ] **R11-12 MCP 工具元数据解析（`description_for_model` 与 UI 文案分开）**
- [ ] **R14-2b Run grouping（R17 把 N 次 run 聚合成一张图的前提）**
- [ ] **R17-1 `WorkState` 通道模型与 reducer（值对象在 `ra-core`，reducer 在 `ra-flow`）**
- [ ] **R17-2/3 `Node` / `NodeOutcome` 与结构化 `Edge` 路由（禁止文本匹配）**
- [ ] **R18 第二参考产品 `ra-assistant` + 对照组回归（框架零按产品名分支的 CI lint）**

---

<a id="r8-legacy-record"></a>

## 附录 C：R8 重排前的设计与完成记录（2026-09-16）

> 以下完整保留本次重排前 R8，含最近的 Docker 修订，仅供追溯代码、测试和历史决策。任务顺序、默认策略、非目标和 DONE 不再定义移植验收；现行要求以正文 R8-P0..P10 为准。

<details>
<summary>展开旧 R8 全文（历史记录，不作为当前实施指令）</summary>

### 旧 R8 执行面

### R8 开发顺序

| 顺序 | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| R8-0 | **CodingHost 与核心契约冻结** | DONE | 已冻结 `HostEvent` 信封、`HostEventEmitter` 与分配器绑定、`ExecEvent` / `AgentEvent` 族与前向兼容性（`HostEventBody::Unknown`、`ExecEvent::Unknown`、`AgentEvent::Unknown`、`unknown: Unknown` 扁平化反序列化字段保留）、UUID v7 会话标识、`ToolOutput` 与宿主事件双通道严格隔离；在 `ra-exec` / `ra-patch` / `ra-coding` 冻结 `ExecSessionState`、`ExecLimits`、`ExecOutputSummary`、`PatchPlan`（含 `MoveFile` 源与目标路径追踪及纯派生 target_files）、`CommittedPatchDelta` 与 `CodingHost`。全量门禁与集成测试 100% 通过。 |
| R8-1 | `exec_command` | **DONE** | 落地形态：`ra-tools::exec_command`（schema、参数解码、模型可读结果）+ `ra-exec::session::ProcessManager`（进程生命周期、信号、捕获、会话）。会话在 spawn 之前登记，所以 yield 出去的 id 一定已可寻址；状态机 `Reserved → Starting → Running → Exited\|Failed\|Cancelled\|Expired` 沿用 R8-A 冻结的形状。<br>**失败跨层用 `ExecError` 而不是 `Error::tool`**：`ra-exec` 为任何调用方跑进程，它无从知道工具叫什么，而 `write_stdin` 将来驱动的是同一个 manager；工具名与模型可读句子只在 `ra-tools` 写一次，且经 `Error::with_source` 按类型下取，不从 message 里 `contains` 回捞（去词表化；早期版本正是这样把同一句诊断嵌套输出了两遍）。<br>**子进程只有一个所有者**：`supervise` 任务独占 `Child`，取消 / 驱逐 / 空闲 / 总超时一律只往 channel 发终止请求。`Child` 放共享锁后面必须锁住整个 `wait()`，于是每个想杀它的调用方都要等它自己先死——`cat` 这种不会退出的命令直接把测试挂死。<br>**终止就是取消契约的 drain**：`SIGTERM` 打**进程组** → `DRAIN_GRACE` → `SIGKILL` 仍打进程组；leader 被回收后若曾请求过终止再补一次组扫尾，否则 leader 先退出时忽略 TERM 的后台子进程会永久存活。经 `rustix::process::kill_process_group`（workspace `unsafe_code = "forbid"`，rustix 是既定路线，新开 `process` feature）。中断是 `SIGINT` 不是 kill。<br>**两个 deadline 长在 supervisor 里**：idle 与 total 都由它自己的 select 兑现并会按活动重新 arm，不依赖任何宿主调度的清扫器——要别人记得调用才会触发的超时不算超时。因此 `prune_inactive` 一并删除，它做的三件事已分别由 supervisor 与 `execute` 覆盖。<br>**容量算的是活着的进程组**：会话在进程真正被回收后才离开 active 集，不是在收到终止请求时，否则连续新建会不断"腾出"容量而进程还在跑。腾不出时返回 `ExecError::AtCapacity` 而非放行——每次超时放行一个，会让计数在最扛不住的机器上无界增长。<br>**死因单一**：`finish()` first-reason-wins，`ExecEvicted` 由 supervisor 从**记录下来的状态**发出（不是从它自己观察到的 deadline），一个会话一条。<br>**每个 schema 参数都有消费者**：`cmd` / `workdir` / `shell` / `tty` / `login` / `yield_time_ms` / `timeout_ms`。`tty=true` 明确拒绝而不是静默给管道（事件里写 `pty: true` 是谎）；`max_output_tokens` 按 `read_file` 已写明的理由不进 schema（模型不知道自己在花谁的预算）；`prefix_rule` / `sandbox_permissions` / `justification` 跟读它们的审批里程碑一起回来——无类型 JSON 袋子正是 R2-3 明确否决的形态。<br>**manager limits 是 ceiling 不是 default**：`ExecLimits::tightened_by` 逐字段取更严，请求只能收紧不能放宽（与取消契约 R5「时限只能收紧」同一条规则）。<br>**结构化事实按 R2-3 的口径**：耗时、退出码、stdout/stderr 原始字节数与 `retained_bytes` 都是具名字段；`Truncation` 两侧都是源字节，省略标记不计入保留量。<br>**验收**（`tests/it-exec/tests/process_execution.rs` 33 条 + `tests/it-tools/tests/exec_command.rs` 13 条）：死锁；快退出竞态（多线程 runtime 跑 20 次）；进程组扫尾（抗 TERM 的后台 shell）；stdin 阻塞不挡取消；非法 UTF-8 之后的输出不丢 + EOF flush + 跨读边界不拆字符；idle / total 超时与"有输出就不算空闲"；容量跨 drain 不被绕过；ceiling 只收紧；截断账目按源字节；失败句子只出现一次。**未做**：PTY（`tty` 拒绝）、沙箱与审批（R6 / R8-11）、`ToolContext` 上没有 `CancelScope`，工具调用被取消时运行时只 drop future、外部仍叫不停进程（`ra-core` / `ra-runtime` 接缝）；宿主直接 drop runtime 时抗 TERM 的孙子进程仍会漏，正确出口是关闭时走一遍驱逐 |
| R8-2 | `write_stdin`（管道 stdin） | **DONE** | **原计划是 `write_stdin` + PTY，PTY 半边已裁决删除**：`portable-pty` 是一条从未被任何源文件引用的依赖，`ra-exec::pty` 的 `StdinCommand` / `TerminalMode` 除自己的契约测试外没有消费者，而它们声明的规则（非 TTY 拒绝字符输入）与真实路径相反——`write_stdin` 就是往管道 stdin 写字节，且有测试依赖这一点。会话用的是标准输入管道，管道接受普通写入，这是现在代码里唯一的说法。同时移除 `ExecRequest::pty` 与 `ExecStartedEvent::pty`（后者的唯一生产者就是前者，删后它只会是每条 exec-start 事件上恒为 `false` 的字段加一个无调用方的 setter）；两处的前向兼容都由 `Unknown` 捕获保证，旧记录里的 `"pty"` 原样读入并写回，有断言守着。**TTY 分配与模型可发起的中断不是丢了，是没有声明**——两者都还没有端到端契约，将来要回来就得先写那份契约，而不是恢复这些类型。<br>**已落地的部分**：`ra-tools::write_stdin` 工具本体、与 `exec_command` 共享的 `ProcessManager`、按字节原样写入不补换行、空 `chars` 作 poll（对已结束但仍保留的 session 返回退出状态）；session 自带**跨调用存活的投递游标**（`interactive_output_cursors` / `mark_interactive_output_delivered`），由 `exec_command` 的 yield 快照播种、每次 `write_stdin` 读完后推进，只进不退且夹在已产出字节内，因此两次调用之间产生的输出会在下一次 poll 回传，且投递过的不再重复。记账失败不改变工具返回值——唯一的失败原因是 session 已不在注册表，而 `session_id` 是模型收取输出与取消命令的唯一句柄。<br>**移交给后续任务的三项**：`\x03` 中断入口（`ProcessManager::write_stdin` 的 `is_interrupt` 保留，但 schema 里没有中断参数，模型走不到——归 R8-3 的 `cancel` 一并设计）、同一 session 的 poll/read/write 互斥（当前只有工具级 `ToolConcurrency::Exclusive` 这一粗粒度保护，归 R8-3 的 `read`/`wait` 一并做）、TTY 分配（无消费者，等真有交互式 REPL 场景再立任务）。<br>**一处措辞待定**：`write_stdin` 的 schema 描述仍写 "returns output produced afterward"，实际返回的是尚未投递的输出（可能早于本次调用产生）；改它要一次 `TOOL_SCHEMA_REVISION` 与三份基线重刷，可与 R8-2 的 schema 变更一并做 |
| R8-3 | 后台 job 生命周期 | **DONE** | 落地形态：`ra-exec::job::BackgroundJob` —— 一个执行 session 的**消费侧视图**，不是第二个进程属主；child、进程组、捕获缓冲、deadline 与终止仍然只归启动它的 `ProcessManager`。三个入口：`snapshot()`、`cancel()`、`wait(JobWaitUntil)`。<br>**`Done` 等的是 closeout，不是终态**——两者相差最多一个 `DRAIN_GRACE`：`stop` 在被要求的当下就记下 `Cancelled` 并返回，此时进程组还在 drain，用终态实现 `Done` 会把还活着的进程报成已结束，并交回一份缺了「命令退场时打印的全部输出」的快照。`ExecSession` 因此新增 `closed`（只在 child 被回收、两个 output reader 结束或触到 bounded drain 之后置位）与独立通知，`JobSnapshot::is_closed()` 对外暴露它。<br>**`Match` 自带 timeout**（`Match { text, timeout }`，不是从 `Timeout` 变体借）：一个只能由「命令也许永远不会产生的输出」满足的条件，没有理由无限期阻塞调用方。匹配是增量的——每条流一个游标加一段 `needle.len() - 1` 的进位后缀跨读取边界，因此一次唤醒的代价是刚到达的字节，而不是重新拷贝并重扫两条捕获缓冲。deadline 只在「本轮已扫完所有既有输出」之后才检查，因为输出与到期可能同一瞬间就绪，按唤醒原因分支会在存在未检视的匹配时报 timeout。<br>**截断标记不参与匹配**：`read_from` 会在丢字节处渲染 `... [omitted N bytes] ...`，这对「要把输出展示给人看」的读者是对的，对「要搜索输出」的读者是错的——搜 `bytes` 会命中标记本身，跨缺口的 needle 会匹配上命令从未连续写出的文本。新增 `HeadTailBuffer::read_retained_from` 返回 `RetainedRead`（缺口两侧分开承载），matcher 在两段之间清掉进位前缀。<br>**每次 wait 交回的快照都取自产生它的那一次持锁**：状态与输出分开读会让调用方看到「退出前的输出配退出后的状态」；wait 成功之后再进一次 registry，则会让 retention 把一个已完成的 job 变成查找失败。扫描以闭包形式在同一把锁内执行，因此只有在真的要返回一个观察结果时才构建完整 summary，而不是每个输出 chunk 一次。<br>**测试**：`tests/it-exec/tests/job.rs` 11 条——取消后等到真回收（`trap '' TERM` + 断言已离开 active 集）、stdout/stderr 分别命中、跨读取边界匹配、match 命中优先于 closeout、match deadline、截断标记不误匹配、空 match 拒绝、未知 job 的 snapshot 与 cancel。<br>**移交给后续任务的四项**（①④ 已由 R8-3a 兑现，②③ 仍在）：① **模型可见的工具入口**——当时 `BackgroundJob` 除自己的测试外没有任何调用方；**R8-3a 已接上**，形态是 `write_stdin{until, match_text, control}` 而不是独立的 job 工具；② **`list`**——`ProcessManager::active_sessions()` 已有，但没有 job 级别的列举入口；③ **`read_tail_bytes`**——`wait` 结果目前回传完整 summary，尾部字节裁剪未做；④ **`\x03` 中断入口**（R8-2 移交过来的一项，`ProcessManager::write_stdin` 的 `is_interrupt` 仍在，schema 里没有中断参数，模型走不到）。<br>**一处已知未修**：`snapshot()` / `wait` 的轮询不刷新 `last_active_at`（`read_output` 会刷），所以一个**沉默**的 job 在默认 5 分钟 idle TTL 下仍会被 idle sweep 带走，`wait` 随后返回 `Done(Expired { IdleTimeout })`。`Match` 有 deadline 之后后果已从「永久挂起」降为「拿到一个已定义的结果」，故未在本任务内修。**R8-3a 已修**：改为登记式的 `SessionWatch`——被等待的 session 不算空闲，也不被 retention 挤掉，但不豁免 total deadline 与显式取消 |
| R8-3a | **后台执行闭环：`write_stdin` 的等待与控制** | **DONE** | **形状裁决：不开新工具入口**，`until` / `match_text` / `control` 三个参数加在 `write_stdin` 上。R2-8 的 advertise 集是 15 个且明写「AF 的 9 个 `background_shell_*` 不采用，由 `exec_command` + `write_stdin` 覆盖」；codex 也没有独立的等待工具，`unified_exec` 就是再调一次带 `yield_time_ms`。**原先写在 R8 验收标准里的名字 `background_shell_wait` 与这份工具词表冲突，随本条作废**。<br>**等待条件与等待时限分开**：`until: output（默认）\| done \| match`，时限一律是 `yield_time_ms` 并受宿主 yield ceiling 收紧。**不保留 `until: timeout`**——超时是每种等待的上限，不是第三种业务条件，`done` 也不许把一次工具调用挂成无限期；到期只结束本次等待，不终止进程。底层已有的 `JobWaitUntil::Timeout` 正好承接有界的 `done`，内部契约不动。`match` 配非空 `match_text`，其他模式带它即参数错误。<br>**返回必须同时说明「为何返回」和「进程是否收完」**：`return_reason: output \| matched \| done \| timeout \| contended`，`done` 严格等于 closeout 完成；取消已登记但仍在 drain 时如实报告尚未收完。现有渲染在有输出时会掩盖退出状态，一并修；`exec_command` 的 yield 结果与本工具的结果用同一套字段。<br>**匹配范围与投递游标分别定义**：匹配搜索保留输出的全部历史——调用前已经打印的 `READY` 不能漏——stdout / stderr 分开、大小写敏感、字面匹配、不跨截断缺口；返回正文仍只投递尚未投递的字节，游标只在真正进入本次正文的字节上推进。命中附带所属流与命中片段，让「命中了但没有新输出」也可理解。以后有重复交互式握手的实际需求，再加显式匹配起点。<br>**取消可同入口，中断必须另有语义**：`control: interrupt \| cancel`，指定时要求 `chars` 为空，执行后仍按 `until` 等待收取输出。**不把管道里的 `"\u0003"` 偷换成中断信号**——当前没有 PTY，它就是输入字节，偷换等于毁掉原样写入契约。<br>**两个生命周期问题在本条内解决**：① 同 session 的「读游标 → 写入 → 等待 → 取一致快照 → 推进游标」并发契约（不声明 session 的动态 `ResourceClaim`，避免 runtime 在工具 deadline 之前重复排队；manager 内的投递锁统一保护直接调用与控制调用的游标，跨 session 并行。取消/中断先发送信号再有界等待投递锁；输入写入也受时限约束，超时明确提示可能已写入部分字节。**`yield_time_ms` 是 handler 内的整次交互预算而不是每段各一份**——拿锁、写入、等待三段共用一条 deadline，否则要 1 秒的调用可能花 3 秒；拿不到投递锁时如实报「没看成」而不是报「等过了没动静」，争锁失败只承诺本次未检查/未收取输出，不承诺输出仍保留；即使 session 已 closed 也保留重试收取指引）；② 等待期间的存活保障——被等待的 session 不算空闲、也不被 retention 挤掉，**但不豁免 total deadline 与显式取消**。<br>**不纳入**：`read_tail_bytes`（它只控制返回展示、不限制搜索范围，且要先定义被裁掉部分的遗漏报告与游标推进规则）、`list`（跨 session 操作，不靠「省略 `session_id`」塞进本工具；先补宿主侧列举能力，模型可发现的入口另作 deferred 工具裁决，不能把宿主 API 完成写成模型入口完成）。`status` 由零等待 poll 覆盖。<br>**边界声明**：后台进程跨调用持有 workspace claim 的问题**不在本条闭环内**，交给第 12 步 R8-11a 的 lease 实现。本条闭环的是「模型可等待、可匹配、可中断、可取消」，不是资源互斥<br>**落地形态**：`ra-exec::job` 补 `JobWaitUntil::Output{delivered_stdout, delivered_stderr, timeout}` 与 `JobWaitResult::OutputReady`，`Matched` 多带一段命中片段；`ra-exec::session` 新增 `ExecRead`（正文 + 下一个游标 + **本段**被采集上限丢掉的字节）、`SessionWatch`、`InteractionGuard`。工具侧 `ra-tools::write_stdin` 加 `until` / `match_text` / `control` 三个参数，`ra-tools::session_return` 是 `write_stdin` 与 `exec_command` yield 共用的结果形状（`return_reason` / `state` / `closed` / `exit_code` / 两条流 / 截断账目）。`TOOL_SCHEMA_REVISION` 5→6，三份基线随之重刷。<br>**四处偏离都写在模块文档里**：① **不声明动态 `ResourceClaim`**——claim 排的队在工具 deadline *之外*，admission 等多久 `yield_time_ms` 管不着；改由 manager 的 per-session 投递锁在 deadline 之内串行，它还多覆盖了根本不过 admission 的直接调用方。② **`yield_time_ms` 是整通调用的预算**，拿锁 / 写入 / 等待三段共用一条 deadline——每段各给一份会让要 1 秒的调用花 3 秒。③ **争锁失败是第五种 `return_reason`（`Contended`）而不是 `Timeout`**：前者是「没看成，输出还在那儿」，后者是「看了，没动静」，对「再调一次值不值」给的答案相反；而且它即使遇到已关闭 session 也保留收取指引，**不承诺「输出完整」也不承诺「没有丢失」**——没看过就不能担保，另一个持有者可能已经取走、采集上限也可能丢字节。④ **投递正文不做 trim**：没有换行的提示符尾空格是交互式会话的真信号，增量投递里裁掉就没了（`exec_command` 的 completed 分支仍按旧行为 trim）。<br>**移交未做**：`read_tail_bytes` 与 `list` 按本条开头的理由不纳入；后台进程跨调用持有的 workspace claim 归第 12 步 R8-11a。<br>**验收**：`tests/it-tools/tests/write_stdin.rs` 21 条 + `tests/it-exec/tests/job.rs` 16 条——closeout 压过最后一行输出、匹配到调用前就打印的历史、取消插到阻塞写前面、有界写如实交代可能写了一半、被等待的静默 session 熬过六个 idle 窗口而没人等的照样被扫、retention 绕开被等待的 job、以及整通调用预算（按每段一份算会超 650 ms，已用变异验证旧逻辑确实失败） |
| R8-4 | V4A `apply_patch` | **DONE** | `ra-patch` 负责完整 V4A 解析与纯 hunk 应用，支持 `*** Begin Patch` / Update / Add / Delete / Move / `@@ context` / `*** End of File`；匹配按 exact → trim-end → trim → Unicode 标点/空白归一化降级，多个候选一律拒绝，不静默选第一个。工具本体 `ra_tools::apply_patch::ApplyPatchTool` 住在可复用件层（归属见 R2-12 补记二），`ra-coding` 通过 `CodingHost` 的根目录 capability 把它装成唯一编辑入口——**capability 是调用方的事，工具在被交给的文件系统里能写到哪就写到哪，不自己解析或校验根目录**；逐动作 best-effort，后续失败会返回已提交的 `CommittedPatchDelta` 摘要，绝不声称未实现的跨文件原子事务。更新保持既有 CRLF，移动通过同一 capability 的 descriptor-relative rename。陈旧上下文由 hunk 失败和重新读取处理，不建立产品级 freshness 状态。当前通用调用面先兼容 `{ "patch": ... }` 与裸 patch 字符串，后者为 custom/freeform wire 接入预留；provider 的 custom-tool wire 映射仍归 R1-5b。`@@` 头是**搜索锚点而不是紧邻的上下文行**（连续 `@@` 逐层收窄；锚点自身重复时由 hunk 上下文消歧，不因锚点不唯一就拒绝），一个 hunk 内被上下文分隔的多个变更块拆成独立 hunk 分别定位，文件原本没有末尾换行的就不补。验收：`it-patch` 14 条覆盖 V4A 语法、四级模糊匹配、歧义、CRLF、末尾换行保持、锚点定位与嵌套锚点、多变更块拆分；`it-tools/apply_patch` 6 条覆盖工具契约（多动作 delta、best-effort 部分提交、裸字符串调用、capability 越界、默认 options、描述如实披露部分应用）；`it-coding/apply_patch` 2 条覆盖产品装配（advertise 唯一编辑入口 + 宿主确实交出工作区 capability）。 |
| R8-5 | 文件工具边界 | **DONE（路径策略与工具事件两项移交）** | advertise 工具只保留 `read_file`（窗口/预算截断元数据）与 `apply_patch`（R8-4）；不单独暴露 `write_file` / `edit_file` / `list_files`。<br>**落地形态：`ra-exec::fs::Workspace`** —— 规范化根 + descriptor capability + `ResourceId` 三者作为一个值，产品开一次、每个工具拿借用。`CodingHost` 把同一个 workspace 交给 `read_file` / `apply_patch` / `exec_command`，`read_file` 同时首次进入 host-backed 工具面（此前编排里根本没有读入口，测试里建的那个能读进程所及的任何地方）。**改之前三个工具各自解析根、各自开 descriptor、各自从手头那个路径派生身份**，于是「同一个工作区」是三个靠构造巧合达成一致的句柄，第一次有人换个写法传同一个目录就散了。<br>**身份长在 workspace 上而不是每个工具里**：一个值被三方共享，才是「读的 shared claim 与写的 exclusive claim 描述同一把锁」成立的原因；在 `open()` 处派生也把失败挪到了开工作区那一处，而不是每个工具构造时各失败一次。三个工具里原本逐字复制三遍的 9 行身份推导随之塌成一行，`for_workspace` 收 `&Workspace`。<br>**claim 口径**：读 shared，补丁与命令 exclusive。`apply_patch` 交出全局 `Exclusive` 换成工作区 exclusive —— 这是**收窄不是放宽**：它不再串行化跟本工作区无关的调用，同时开始排除它原先能并行的读。两个构造函数各自把并发声明写在明面上（无身份的 capability 命不出锁，所以 ambient 形态仍然全局串行）。<br>**只读角色保留只观察的入口**：此前 host-backed 装配对 read-only / one-off 一律返回空，这在宿主只提供写入口时是对的，在它开始提供一个不写的入口时就错了 —— 一个读不了东西的 read-only specialist 执行不了它自己的角色文案。过滤按每个工具声明的 `PermissionScope` 走，不是在旁边另记一份名单。<br>**工具面 revision 升到 4**（`read_file` 进 advertised table）。<br>**验收**：`tests/it-coding/tests/coding_host.rs` 2 条（三者共享同一 resource 且 kind/value 指向本 host 的 canonical root；`apply_patch` 写、`read_file` 读 —— 只比 `ResourceId` 的话三个工具各开各的 `Dir` 也能过）、`tests/it-coding/tests/prompt_dump.rs` 与 `tests/it-cli/tests/prompt_dump_command.rs` 各拆成「one-off 无工具面 / 只读角色有工具面但只含观察项」两条。<br>**移交给后续任务的两项**：① **完整路径策略** —— per-path denies 与 mount policy 仍未实现，本条只兑现了 descriptor 解析（cap-std 从根句柄逐段走，符号链接只跟到不出根为止），check-then-open 竞态已关闭，但那不是路径*策略*；② **文件读写事实进 Session 工具事件（已由 R8-5b 兑现）** —— 当时 `read_file` 与 `apply_patch` 都不碰 `context.event_emitter()`，只有 `exec_command` 发事件，所以 session log 记得下命令、记不下一次 run 读写了哪些文件。**这项必须补**：R8-5a 推迟版本表的理由正建立在「工具事件是可回放事实来源」上，而这个来源目前不存在。<br>**两处已知未修**：① 工作区互斥只在一次调用内成立 —— admission permit 在 `invoke` 之后即 drop，而 yield 到后台的命令还在跑，跨调用的互斥要改资源闸门本身（归后台 job 工具入口一并设计）；② `read_file` 的 shared claim 让同一批次的读串行在 `exec_command` / `apply_patch` 之后，批量读与命令不再重叠。要恢复重叠得把 exec 的 claim 从整个工作区收窄到进程 / cwd，这是独立的设计决定，未在本条内做 |
| R8-5a | 文件版本表 / Read Ledger | **DEFERRED（不实现）** | 不创建 `ReadRevisionSnapshot`、读前置条件或“未变更”缓存投影。当前 coding loop 依赖模型上下文、`apply_patch` 匹配结果和必要时重读来处理陈旧内容；工具事件是可回放事实来源。只有真实 A/B 能证明独立版本追踪的收益高于状态同步、恢复和 token 成本时，才可重新评估；在此之前不得为它设计 port、crate 依赖或验收门槛 |
| R8-5b | **文件事实进 Session 工具事件** | **DONE** | R8-5 移交项②。`read_file` 与 `apply_patch` 都不碰 `context.event_emitter()`，所以 session log 记得下命令、记不下一次 run 读写了哪些文件——而 R8-5a 推迟版本表的理由**正建立在「工具事件是可回放事实来源」上**，这个来源目前不存在。<br>事件走自己的族（不塞进 exec 族，它们不是进程事实），记录实际发生的读取与修改：关联 tool call、路径、读取范围或修改类型及结果；**`apply_patch` 部分成功后失败也必须留下已提交修改的事实**，日志持久化失败要可见，不能把已写盘的修改伪装成没有发生。<br>**承诺就到这里**：这是 `read_file` / `apply_patch` 的工具操作记录，不是 shell 任意文件访问的完整审计，也不自动等于可重建文件内容。<br>**与 R8-3a 同批但独立交付**：内部 Session 事件不改模型工具 schema，不能拿它当「必须合并修改」的理由<br>**落地形态**：`ra-core::event::file`（`FileEvent` / `FileReadEvent` / `FileChangeEvent` / `FileChangeKind`）自成一族，进 `HostEventBody::File`；`read_file` 与 `apply_patch` 经 `ToolContext::event_emitter` 发出。<br>**三处偏离**：① **不塞进 exec 族**——读文件不是进程事实，合进去等于让每个 exec 事件消费者去过滤根本不来自进程的记录；② **行数记「模型收到的」而不是「窗口选中的」**，被输出上限截短的窗口只报活下来那几行；③ **sink 拒收要 `tracing` 留痕**（`ra-tools` 因此新增 `tracing` 依赖）——已经写盘的修改不能因为记不下来就变成没发生。<br>**承诺边界**：这是 `read_file` / `apply_patch` 的工具操作记录，不是 shell 任意文件访问的完整审计，也不等于可重建文件内容。<br>**验收**：`tests/it-core/tests/host_event.rs` 信封与往返，两份工具测试覆盖部分应用仍逐条记账、move 带两个路径、截断窗口只计已投递行数、没有宿主在听时读取照样成功 |
| R8-6 | 检索工具 | **DONE** | `grep` / `glob` 已在 `ra-tools/src/{grep,glob}.rs` 实现为 Rust-native 一等工具：正则文本检索与确定性文件模式匹配均经 workspace capability 遍历，返回匹配数、扫描/跳过统计、工具阶段截断与收窄建议；文本工具跳过二进制、过大和不可读文件，且读取实际受上限约束，避免文件在 metadata 预检后增长时无界读入。目录 symlink 不递归跟随，防止循环或越界遍历。两项均已装配到 `CodingHost`，主角色拿到 6 项 core surface，只读角色保留 3 项观察工具。`rg --files` / `find` / `git grep` 等临时或长尾检索仍通过 `exec_command` 收编。 |
| R8-7 | 沙箱后端：unix_local | **DONE** | 基线后端：进程初始 `cwd`、环境变量清理、资源上限（rlimit）。<br>**它提供的不是隔离**（2026-09-15 修正：原文写「工作区根限制」，与文件工具那条同名而强度完全不同）。`ra-exec::fs::Workspace` 的 cap-std 根限制约束的是**走它的文件工具**，本条约束的是**一个被拉起的进程**——`cwd` 与环境变量拦不住子进程读写根外任意路径。宿主进程的路径限制由 R8-8 / R8-9 施加；R8-10 另建容器执行环境，其挂载与文件边界独立定义和验收；本条在 codex 的分类里对应 `SandboxType::None`，表示本次执行未使用平台沙箱，不代表宿主机器没有可用的平台沙箱。<br>**出处改写**：原文引 AF 的 shell 资源上限策略，按「不以 AF 为设计依据」的裁决删除。环境构造参考 codex `core/src/unified_exec` 与 R8-A 已采纳的条目（`NO_COLOR=1` / `TERM=dumb` / 禁 pager）；rlimit 是 Rusty 的设计选择，不宣称该目录已有对应实现，具体限制项与平台差异须在实现时明确。<br>**同名不同物在案**：openai 的 `sandbox/sandboxes/unix_local.py` 是一个有 start / stop / from_state 的长生命周期 session，不是围栏；本条只借了名字，没有借形状。<br>**已落地**：`ra-exec::sandbox::unix_local` 出 `EnvPolicy`（`EnvInherit` 三档 + `EnvPattern` 只认 `*` + `exclude` / `include_only` / `set`）与 `ResourceLimits`（cpu / fsize / nofile / core 四条），`ra-exec::sandbox::ExecEnvironment` 把它们与临时目录收成一份宿主配置，挂在 `ProcessManager` 上而不是 `ExecRequest` 上——请求由模型输出构成，能被命令点名的策略就能被命令放宽。<br>**三处偏离照 codex 的 `ShellEnvironmentPolicy` 写明**：① 凭据过滤按它做的事命名且默认开（上游 `ignore_default_excludes` 默认 `true`，即默认交出启动 shell 里的每把钥匙）；② `set` 在 `include_only` **之后**生效，上游的顺序让 allowlist 能删掉同一份策略刚设的值；③ 软硬上限一起设——只设软的，一句 `ulimit` 就能爬回去。不做 `RLIMIT_AS`（macOS 对 mmap 基本忽略，半数机器上是装饰却会被信任）与 `RLIMIT_NPROC`（按 uid 计数，低到能拦 fork bomb 的值会让用户自己开不了 shell）。<br>**unsafe 例外**：`pre_exec` 是 rlimit 在 Rust 里的唯一入口，而 `forbid` 连那一行上的 `#[allow]` 都压不住，所以本 crate 自带一份 lint 表把 `unsafe_code` 降为 `deny`，工作区保持 `forbid`；一条测试断言两张表除这一项外逐字一致。<br>**验收**：`it-exec/tests/sandbox_baseline.rs` 中环境派生与模式匹配 7 条、真实 spawn 只见派生环境 1 条、`ulimit` 证明钩子真的跑了且子进程爬不回去 3 条、lint 表一致性 1 条。 |
| R8-8 | 沙箱后端：macOS seatbelt | **DONE** | `sandbox-exec` profile。<br>**读侧写成具名档位，不写「透明化」**（2026-09-15 修正：原依据取自 AF 实测结论，按「不以 AF 为设计依据」的裁决删除；且「透明化」是态度不是契约）。参考 codex `codex-rs/sandboxing/src/seatbelt.rs` 的策略组合方式：文件读写策略、平台默认集与拒绝规则分别组合；`seatbelt_read_only_platform_defaults.sbpl` 提供 `:minimal` 请求的平台默认集，基础规则见 `seatbelt_base_policy.sbpl`。**具名配置及允许的降级关系由 Rusty 定义**，这些源码不证明上游已有按强弱排序的降级档位。Rusty 必须明确各配置的实际权限，并在结果里返回**实际生效的配置**。<br>**降级不是默认行为**：请求的保证兑现不了就拒绝；只有宿主显式允许较弱档位时才降级，且降到哪一档必须随结果返回，不能只写进日志。后端整体不可用时按 `crates/ra-exec/src/sandbox.rs` 已定的规则构造失败，不静默退回 R8-7<br>**已落地**：`ra-exec::sandbox` 出 `SandboxLevel` 三档梯子（`unconfined` / `workspace-write` / `isolated`，按强弱可比较）、`SandboxPolicy`、`SandboxBackend` trait、`Confinement` 与随结果返回的 `SandboxReport`；`sandbox/seatbelt.rs` 按四段拼 profile（base / 读 / 写 / 网络），**路径一律走 `-D` 参数，不拼进策略正文**——带引号或反斜杠的路径能结束它被贴进去的那个字符串，一条路径能改写的策略不是策略。`ProcessManager` 在 spawn 前把命令包一层，未包时也如实报 `unconfined`。<br>**三条实测发现记在这里，省得再摸一遍**：① 少了 `(allow file-read* (literal "/"))`，**每个二进制都在 `main` 之前 SIGABRT**，把所有顶层目录逐个放行也不行——loader 要穿过根目录本身；② seatbelt 匹配的是内核解析后的路径，所以 `/tmp/x` 这样的可写根一条也不生效（内核看到的是 `/private/tmp/x`），**只解析顶层别名、深层 symlink 一律拒绝**，跟 codex 划在同一处：深层组件能被沙箱内的进程换掉，跟着走就等于把宿主写的路径换成命令自己选的路径；③ `/tmp` `/etc` `/var` `/private` 四个顶层链接要单独给 metadata 读，否则命令按别名写的路径穿不过去。<br>**网络是独立的一根轴，不上梯子**：最常见的真实配置就是「能写工作区、碰不到网络」，两者融在一档里这格就没了。它也**永不降级**——「能不能开 socket」的梯子只有一级，没有可供宿主提前授权的中间档；`unconfined` ＋ 拒网是自相矛盾，直接拒。<br>**降级只在宿主提前写下时发生**：`accepting_down_to` 给出可接受的下限，实际生效的档位与 `downgraded_from` 一起挂在 `ExecOutputSummary.sandbox` 上，而不是只进日志——降级不是事件，是「这个答案有多可信」的属性，留在日志里就要靠人按时间戳去对，而没有人会去对。<br>**平台门从 `mod` 声明移到后端选择上（偏离原注释）**：原来 `#[cfg(all(feature, target_os))]` 意味着 macOS 上编译不出 bwrap 的策略翻译、Linux 上编译不出 seatbelt 的，而每个后端其实是两样东西——一份纯翻译，和几个只在某个内核上有意义的调用。整块门掉的是前者：**Linux 宿主将要跑的策略，在写它的机器上连编译都过不去，更别说测**。现在翻译在哪儿都编译、哪儿都测，内核调用各自留 `cfg`，选择仍然不会把 Linux 后端交给 macOS 宿主。<br>**未做，因此不含在交付里**：① 产品没有请求沙箱——`CodingHost` 仍是 `ProcessManager::default()`，coding run 如实报 `unconfined`，接线与 R8-13 的 run 级持有者同批；② confinement 没进宿主事件族，只随结果返回，否则同一套词表要在 `ra-core` 再写一遍。<br>**验收**：`it-exec/tests/sandbox_backends.rs` 28 条（18 条与平台无关、7 条 macOS 真 spawn、3 条 Linux 过滤器），macOS 上真 spawn 覆盖：写进根内成功／写到根外被拒且文件不存在、`isolated` 与 `workspace-write` 对根外读取给出相反结果、**对着本机自己开的 listener** 证明拒网与放行两个方向都成立（内网地址才分得清「沙箱拒了」和「这台机器没网」）、结果如实报出后端与档位、`RUSTY_AGENT_TMPDIR` 在沙箱里可写、相对路径根作为 `ExecError::Sandbox` 而不是启动失败被拒 |
| R8-9 | 沙箱后端：Linux bwrap + seccomp | **部分（实现与 Linux 验收测试已补，待原生验收）** | 严格 allowlist、网络隔离、fail-closed；`ra-cli doctor sandbox` 自检。<br>**审核修复（2026-09-15）**：bwrap 通过 `--seccomp` 接收封印的匿名 memfd；拒绝网络时禁止 socket 创建和通信，补上 netns 无法隔离路径型 Unix socket 的缺口；始终拒绝 ptrace / process_vm / io_uring，x86_64 拒绝 x32 syscall 编号。编译、传递或安装失败不启动目标命令。**当前 syscall 策略是 denylist，不据此宣称严格 syscall allowlist 已交付**。<br>doctor 使用随机、排他创建的 `0o700` 临时目录，由持有句柄清理；网络拒绝必须有允许网络的同探针对照，启动错误、未完成和跳过不能返回健康。Linux 集成测试覆盖宿主 Unix socket、文件边界及 x32，CI 安装 bubblewrap 后原生执行；macOS 上的交叉编译不替代 Linux 内核验收。<br>**第二轮审核补的四件事（2026-09-15）**：① 过滤器描述符**避开 0/1/2**——子进程的 stdio 在 `pre_exec` 之前就 dup2 上去了，落在那三个号上的策略会被管道顶掉，bwrap 就从错误的描述符读策略（结果是启动失败而非放行，但这里把这种情况直接消掉）；② `ra-exec` 开一扇 `test-api` 窄门，**把过滤器变成可断言的东西**：此前 `compile` 是 `pub(super)`、CI 又没有把 `confine()` 走到，**删掉挂载过滤器那一行，所有平台的测试照样全绿**；现在断言拒网比放行多出规则、x32 前置三条指令逐字节正确、以及一条不需要装 bwrap 的 `confine_with` 把 `--seccomp <fd≥3>` 真的送进了 argv；③ **denylist 比上游更严的代价写在代码注释里**：codex 用 arg-0 条件放行 `AF_UNIX` 与 `recvfrom`，因为本地 IPC 是相当多工具管自己子进程的方式（它点名 `cargo clippy`；Python `multiprocessing`、`syslog()`、任何 D-Bus 客户端同类）。本实现下它们会在工具链深处拿到 `EPERM`，很难归因。保的是更简单那个承诺：**`Denied` 就是没有 socket，不是「没有 IP socket」**；真需要断网下的本地 IPC，才是把 arg-0 例外加回来的场合，而那要在有 Linux 机器可测时做；④ `doctor sandbox` 的判决改三态，见下。<br>**`doctor` 判决是三态而不是两态**：`confined` / `not-confined` / `unverified`。「没有被限制」和「没能检查」是关于一台机器的两个不同事实，压成一个，等于对一个镜像里根本没有 bash 的宿主说「你的沙箱不管用」——证据支持不了这句话。探针**启动失败**（壳不存在）记 `skipped` 并给出 `unverified`，探针跑过却给错答案才记 `failed` 并给出 `not-confined`；两种都不算健康，退出码都是 2，区别在给人读的那份报告里。`live_checks` 保留目录时也不再闷着，报告里有一行 notes 说明留在哪儿、为什么不敢删。<br>**第三轮审核修的两处（2026-09-15）**：① **相对 `PATH` 条目能让探测对象与实际启动的程序是两个文件**——探测在宿主当前目录下跑，启动却带着**请求指定的 cwd**（就是模型一直在写的那个工作区）；`PATH` 里有个 `bin`，探到的是可信的 `bwrap`，执行的是 `<工作区>/bin/bwrap`，**施加沙箱的程序变成被沙箱的一侧写的那个**。已复现为真实序列。修法是**相对条目直接丢掉而不是解析**（拿当前目录解析只是换个位置摔——没人保证两个时刻的当前目录一样），并且 `located_program` 改成 `OnceLock` 解析一次，探测与启动共用同一个绝对路径；回归测试**把当前目录切进一个真的放着 `bin/bwrap` 的树里**再查，去掉过滤即失败（变异验证过）。② **真沙箱下缺失 shell 不是启动失败**：实际启动的是包装程序（`sandbox-exec` / bwrap），它存在、能起来，在里面 exec 不到 shell 才非零退出——从外面看与「跑过并被拒」一模一样，于是镜像里没有 bash 的宿主被报成沙箱不管用（**实测 exit 71**）。改由每个探针先打印的那个标记来回答「探针到底跑没跑」：没有标记就是 `Skipped` / `unverified`，有标记才轮得到 `Failed`。 原测试用的是**没有沙箱**的 manager，恰好绕开了生产路径，已补一条带真实后端的；同样变异验证过。 |
| R8-10 | **Docker 容器执行环境与工作区 session** | **TODO（定位与分批范围已定，未实现）** | **出处与边界（2026-09-16 裁决）**：参考 openai-agents-python `src/agents/sandbox/sandboxes/docker.py` 里 `DockerSandboxClient` / `DockerSandboxClientOptions` / `DockerSandboxSession` 三者的生命周期与工作区语义，以及 `docs/sandbox/clients.md` 的 run config 选择方式；它是跨命令的执行环境，不是宿主进程围栏。**锚点是 commit ＋ 符号名，不是行号**：本条对照 `89c02c82`（`v0.22.0-70-g89c02c82`，2026-08-28），行号会浮动、全仓关键词命中数不构成契约，复核时按符号名重新定位。Codex 的平台沙箱管理不提供本条的实现依据；落地时记录支持子集与语义偏离。**容器隔离强度和可复现范围由具体配置与验收证明**，固定 image 身份不等于工作区、外部依赖与网络输入全部可复现。<br>**分层**：保留 `SandboxBackend` 的宿主命令包装契约；Docker 不实现这个 trait，不进入 `SandboxLevel` 强弱阶梯，也不在失败后自动回退宿主执行。执行环境在宿主配置层选择，模型请求不能改变 daemon、image、挂载或安全策略。先以具体类型或内部枚举接线，实际公共部分稳定后再提接口；不预建通用远程执行框架，不向 provider-neutral `ra-core` 添加 Docker 字段。**模块要先搬出 `sandbox::`**：现有占位 `ra-exec::sandbox::docker` 连同它那行 doc 里的「backend / strong isolation / reproducible」三个词都属于旧定位——放在一个它明确不实现的 trait 的模块里，读者只会按围栏去读它，而那三个词恰是本条不肯无证据承诺的。实现的第一步是移出 `sandbox::`（`ra-exec::container` 或同级命名）并重写 doc，不是在原地长出实现再回头改名。<br>**不能只包一层 `docker run`**：当前 `env_clear/envs` 与 `pre_exec` 的 rlimit 只直接作用于宿主子进程，包装后将落在 CLI 上；宿主进程组消失也不能证明容器使用者退出。Docker 路径必须单独实施工作负载策略与生命周期监督，不能据宿主启动成功报告容器内保证已兑现。<br>**首期范围**：受控的本地 Linux Docker 环境，一个 run 持有一个容器，多条命令共享工作区；固定并报告实际 image 身份，完成命令输入输出、退出码、取消、run 关闭与幂等回收。先确定 R8-11a / R8-13 的 run 级持有与 closeout 接口，再交付本闭环；具体容器操作放执行层，产品选择与 run 接线留宿主层。**「输入输出」的含义钉死为 stdout / stderr 仍是两条**：`ExecStreamKind` 在宿主执行路径上全程分流，而容器 attach 给的是带帧头的复用流、TTY attach 更是直接合并两者，所以本条对外必须保持两条独立流；所选 SDK 已提供解帧后的流时直接消费，否则由适配层解帧，不要求重复实现协议解析。交一条合并流不算完成，首期不做 PTY 也不构成合并的理由。<br>**工作区必须一致**：首期使用可信宿主指定的工作区 bind mount 与固定容器路径映射，文件工具和 shell 必须操作同一份工作区，并共用 lease / admission 约束；临时目录也要映射，容器内 `RUSTY_AGENT_TMPDIR` 指向容器路径。宿主路径与容器 POSIX 路径独立表示，明确宿主文件工具只能访问映射的工作区，不据此宣称已提供容器全文件系统工具。远程 daemon 不沿用「bind mount 指向 Rusty 所在机器」的假设，首期拒绝。<br>**策略逐项兑现**：daemon 客户端环境与工作负载环境分开配置，定义 image `ENV`、宿主环境过滤与请求变量的合并顺序；cpu / fsize / nofile / core 的软硬 rlimit 必须在容器内生效，不支持就拒绝，CPU 配额和容器总量限制不能冒充单进程 rlimit。**这四条上游没有对应物**——`docker.py` 全文不涉及 rlimit，它们是 R8-7 带过来的 Rusty 要求，别去上游找参考实现。**配置位置与计量范围分开**：首期在容器创建时统一配置四条 rlimit，作为容器内各进程的资源限制，不将其解释为整个容器共享的累计额度。R8-7 把 `ResourceLimits` 挂在 `ProcessManager` 而不是 `ExecRequest` 上，证明的是策略由宿主持有，不决定限制的计量范围。实现时核实所选 API 与运行时如何向 exec 进程应用限制，逐次 exec 及其子进程必须实测符合请求的软硬上限；即使 API 支持 per-exec 覆盖，也不改变「请求不能放宽策略」。可安装的 hard limit 受运行时权限与内核约束，不能一概以 daemon 自身上限判定；运行时无法安装请求的软硬上限时拒绝启动，不静默降级。当前 `NetworkAccess::Denied` 承诺禁止 socket，Docker `network_mode=none` 不禁止 Unix socket，不能当成等价实现；复用该策略必须补容器内 syscall 限制并实测，兑现不了则拒绝。若另设仅禁外网策略，必须独立命名和审议，不改旧词含义。报告实际执行环境、image、挂载及生效策略，不复用未经证明的 `Isolated` 标签。<br>**信任边界**：daemon 控制权只属于可信宿主，socket、控制凭据与管理接口不得暴露给工作负载；rootful daemon、rootless 与 VM 的边界不同，不按 Docker 名称判定强弱。首期只接受可验证的受控配置，不开放模型传入任意 Docker 参数。<br>**取消与清理**：分别记录「命令结束」「全部容器工作负载停止」「工作区可清理」。关闭 exec 连接、输出 EOF 或杀 CLI 都不是工作负载退出证明。取消后若无法确认命令后代退出，首期终止整个容器并把 session 标为不可继续使用；仍无法确认容器停止则保留待恢复记录与目录，不能释放清理保护并报告成功。run 关闭先确认使用者停止，再回收自身拥有的目录和容器；外部工作区不随容器删除。异常退出须保留可供 doctor 核验的容器身份、所有权与清理记录，这不等于支持继续执行。<br>**后续批次**：PTY、端口暴露、远程 daemon / 高级挂载、snapshot 与跨进程 resume 分项交付；未支持能力显式拒绝。多命令 session 不等于重启恢复，重连容器不等于恢复在途命令；恢复需核验容器身份、所有权、image 与策略，snapshot 与物化依赖 R8-11。Windows / Docker Desktop 另做路径、文件和生命周期验收，不能因 Docker 可连接就宣布支持。<br>**首期验收**：真实 daemon 上验证文件工具与 shell 双向可见、多命令工作区连续性、stdout / stderr 分流还原、容器内环境与四条 rlimit、文件边界与请求的网络限制、取消后无后代继续写工作区、正常关闭及重复回收、daemon 失联保留待恢复状态、image / 策略不满足时 fail-closed；仅测 argv 或 mock 不算交付。**正式验收与开发自测分开**：首期正式集成验收在 Linux CI 的真实 daemon 上执行，前置条件须在承担 Docker 验收的 job 中显式声明；该 job 缺少 daemon、无法连接或缺少其他必需条件时必须失败，不能以全数 skip 得到绿色验收。普通开发 checkout 允许显式跳过缺少前置条件的集成测试并计数，跳过不计为通过。macOS 可运行协议解帧、策略翻译等平台无关测试，也可用 Docker Desktop 做探索性测试；这些不替代 Linux 正式验收，也不据此宣布首期支持 Docker Desktop。首期通过只能标「首期 DONE」，不能宣称已完整对齐上游 session 能力。 |
| R8-11 | Manifest / Snapshot / 物化 | TODO | 工作区清单、快照与还原（openai `sandbox/manifest.py` / `snapshot.py` / `materialization.py`）；供 worktree 隔离与 loop 回滚 |
| R8-11a | **`WorkspaceLease` 生命周期** | TODO | 提供工作区租约原语：申请、校验、回收、crash-safe cleanup。<br>**与 Docker 的开发顺序（2026-09-16）**：本条与 R8-13 先确定 run 级持有者及 closeout 接口，供 R8-10 的容器 session 接入；Docker 的停止确认与清理记录不能用宿主 CLI 的退出替代。接口先行不等于本条的 admission 与完整执行已交付。<br>**互斥只在宿主选了独占或串行策略时承诺**（2026-09-09 修正：原先无条件写「同一可写工作区同一时刻只有一个持有者」，与 R12-5 允许的共享策略直接冲突——不能一边允许共享，一边承诺框架必然阻止覆盖）。选 `ExclusiveWrite` 时框架保证同一工作区同时只有一个持有者，选串行时保证不重叠；**选共享时并发与冲突责任由宿主承担**，框架只如实记录谁在持有。<br>**lease 不围绕 worktree 定义**：`mode: ReadOnly \| ExclusiveWrite` 与生命周期是通用的；具体隔离手段与它的恢复信息由实现提供——git worktree 实现自然带 `worktree_ref` 与 `base_revision`，容器实现带镜像与卷标识，内存实现可能什么都不带。框架不要求每种实现都产出 git 概念。<br>何时申请、能否并发、如何 join 由 R12-5 决定。<br>**lease 与 `ResourceClaim` 的关系已裁决（2026-09-15）**。先厘清现状：**今天不存在两把锁相争，只有一个缺口。** `ResourceAdmissionGate` 的锁表**每轮新建**（`ra-runtime::turn::batch`），流式执行与本轮结算共用同一个 gate，下一轮重新创建，不同 run 的锁表互不可见；permit 在 `invoke` 返回时释放（`ra-runtime::tool::dispatch`）。所以 `exec_command` 声明的 `ResourceClaim::exclusive(workspace)` **只在同一轮的这批调用之间有效**——跨轮、跨 run 与后台执行期间都没有保护。<br>**裁决：按工作区协调作用域组织长期准入状态，键是 `ResourceId` 而不是 lease id。** 多个 lease 可以指向同一工作区，按 lease 分表会把跨 lease 的冲突漏回去。两层职责继续分离：**lease 管谁有资格使用工作区、使用模式与清理责任；协调作用域管参与协调的调用经 `ResourceId` 找到同一份 admission 状态**。共享策略下是否参与协调仍由宿主决定——否则选「共享」反而比以前阻塞更多，是个反直觉的退化。<br>**五条约束**：① 同一协调作用域内，跨轮、跨 run 的相同 `ResourceId` 共用资源锁——只延长旧 permit 的寿命不够（owned guard 确实能活过创建它的 gate），下一轮会为同一个 `ResourceId` 造一把新锁，旧锁还被握着而新调用在另一把锁上排队，互斥静默失效；② 后台 job 在**启动前**接管所需的资源持有凭证并持续到 closeout，不能等 yield 之后再补交接；③ **不把整个 `AdmissionPermits` 交给后台 job**——它还含全局 gate，长期持有会扩大无关工具的阻塞范围，两者生命周期分开定义；④ `write_stdin`、取消与收尾路径不得被该 job 自己持有的工作区写权限挡住（`write_stdin` 目前未声明任何 claim，故这条对它是潜在风险；取消与收尾则已经在路径上）；⑤ **内存锁不持久化**，恢复时按 lease、存活进程与清理记录重建准入状态。<br>**两条补充**：⑥ **全局 gate 留在轮内**，不随资源锁一起提到协调作用域，否则 `ToolConcurrency::Exclusive` 会从「本批独占」悄悄变成「跨 run 独占」，那是没人要过的产品级串行化；相应地，后台 job 持资源锁期间一个 `Exclusive` 工具仍可运行——**独占约束的是工具调用，不是进程**。⑦ **claims 阶段要能看到已解码输入**：`exec_command` 的 claim 是构造期静态的 `exclusive(workspace)`，而 dispatch 第 5 阶段传的是不带解码输入的 context，于是无法按「这次是否转后台」给出更窄的凭证；照约束 ② 直接实施会让**任何**转后台的命令把整张工作区表独占到收尾，连 `read_file` / `grep` 都挡掉——一个十分钟的测试就是十分钟只读封锁。解码本就在第 5 阶段之前完成，把它传下去即可。<br>**验收必须覆盖的死锁场景**：同一批里既有转入后台的交互进程，又有被它挡住的 `apply_patch`，而解锁它的 `write_stdin` 要等这批结束才轮得到模型发出，两边互等。**这是死锁不是慢**，因此**资源准入必须是有界等待 + 结构化拒绝**——把「工作区被会话 X 持有」作为工具结果回给模型，让它能继续交互或取消，不能是无限期 await。地基已有一半：`acquire_permits` 收 `&cancel`、等待时长记进 `TOOL_ADMISSION_WAIT_MS`，缺的是超时后那条拒绝路径。<br>**分阶段允许，据以宣布完成不允许**：只做 run 级 lease、不动 admission 作用域的中间形态可以先交付，但它挡不住本 run「起后台命令 → 下一轮 `apply_patch`」的并写——**这是单 run 最普通的工作流，不是共享场景的边角**，因此不得据以宣布第 12 步完成 |
| R8-12 | 网络策略 | TODO | 默认放行/deny 策略、域名 allowlist；`web_fetch` / `web_search` 走同一策略层。<br>**悬空引用已删**（2026-09-15 修正）：原文写「作为 R7-11 的数据外发执行点……按 source trust 判定，默认拒绝把 `Secret` 或未获授权的 `UntrustedData` 发到外部」。**R7-11 整条已在 R7 缩减裁决中撤销**，`ContentTrust` 三值枚举不存在也不会有（代码里零命中），R11-2b 早已把凭据外发重新归到本条，只有本行漏改。<br>**凭据外发按 R11-2b 的裁决办**：显式 policy ＋ 输出端尽力脱敏（参考 codex `secrets` crate 的 `redact_secrets`），**不做输入侧 provenance 类型**。<br>**两个执行点，能力不同，谁也替不了谁**：① 工具层——`web_fetch` / `web_search` / MCP 出站按目标域与工具身份准入，这一层知道自己在发什么；② 沙箱后端层——shell 拉起的任意进程只能由后端管（codex 的 `NetworkProxy` / `ManagedNetworkSandboxContext` 与 `seatbelt_network_policy.sbpl`，Linux 走 netns），这一层管得住连接但看不见载荷。**域名 allowlist 单独不构成防泄露承诺**。两处的决策与拒绝原因都以脱敏形式进 trace |
| R8-13 | 运行期临时目录 | **部分（机制 DONE，接线未做）** | 每个 run 一个系统临时目录，注入 `RUSTY_AGENT_TMPDIR` 给 shell / tool / hook（原文引 AF P21，按「不以 AF 为设计依据」的裁决删除出处，保留这件事本身）。<br>**清理有前置条件，不是「调用结束就删」**（2026-09-15 修正：原文只写「自动清理」，没说等谁）：必须等使用者全部退出——后台进程仍存活时不删，否则删的是一个正在被写的目录；可恢复的 run 在暂停期间保留目录，恢复后仍指向同一个；异常退出保留并由 `doctor` 回收，不指望进程退出钩子；重复清理必须幂等，且只删自己创建的那一层，不碰宿主给定的父目录。<br>**顺序上它是后端的输入，不是后端之上的验收项**：每个沙箱后端的写策略都要包含这个目录，`RUSTY_AGENT_TMPDIR` 的注入还要与 R8-7 的环境清理定先后（先洗再注），所以宿主机制与 R8-7 同批，不排在执行后端之后。**这两件在宿主侧都已兑现，不是待办**：`TMPDIR_ENV_VAR` 由 `ra-exec::session` 那个唯一启动点在 `env_clear` 之后注入，先洗再注是既成事实；scratch 目录由 `resolve_confinement` 自己并进可写集而不是指望宿主记得加（理由写在 `ra-exec::sandbox` 那个方法上：命令被告知有这个目录却写不进去，是比没有更坏的状态），所以「后端写策略包含它」对已有的两个后端自动成立。Docker 的目录映射、容器使用者确认与清理保护由 R8-10 在 run 级持有接口确定后接入，宿主侧这两处就是它要照的先例。<br>**已落地的是机制**：`ra-exec::tmpdir::RunTempDir` 以 `key` 派生目录名、`0o700` 创建、按路径在进程内注册表里共享同一份状态（两次 `open_in` 同 key 必须共用一个计数器，否则一边能删掉另一边正在用的目录）；`TempDirUse` 句柄在**进程存在之前**取得并持到被回收，`cleanup` 在句柄未清零时返回 `InUse`，经已打开的 `Dir` 句柄移除以关掉校验与删除之间的 TOCTOU，并核对 dev/ino 身份。<br>**回收 shell 不等于后代退出**：supervisor 在排空后探一次进程组，确认不了就把目录标成 `RecoveryRequired` 并保留——不杀后台活计也不无限等。标记是**黏的**，因为事后再探一个裸 pgid 分不清"退了"和"号被复用了"；宿主自行核验后走 `cleanup_after_recovery`，它只放行这一道闸，身份校验与句柄移除照走。覆盖范围是**被跟踪的进程组**：`cmd &` 与被 init 收养的孤儿都在组内，自己 `setsid` / `setpgid` 跑掉的不在；宿主 Linux 路径由 R8-9 的 PID namespace 处理，Docker 路径由 R8-10 独立确认容器使用者退出，不能复用宿主 pgid 判据。**Linux 侧已由 R8-9 关掉**：bwrap 的 PID namespace 里没有「跑到组外」这回事——`--unshare-pid` ＋ `--die-with-parent` 让 namespace 内的后代随 init 一起结束；macOS 侧这个口子仍然开着，seatbelt 不提供 PID 隔离。<br>**未做，因此不标完整交付**：① 没有任何 run 真的拿到目录——`CodingHost` 是工作区作用域的，需要一个释放点在 closeout 的 run 级持有者，那属于 R8-11a 那批；② 跨进程恢复未做，重启后对同一 key `open_in` 会以 `Create{AlreadyExists}` 失败（拒绝认领来历不明的旧目录是有意的，但要留恢复口子得先给它一个独立的错误变体）。<br>**验收**：`it-exec/tests/sandbox_baseline.rs` 临时目录 12 条，含清理与登记互斥的并发用例、清理后重开拿到可用目录、符号链接替换不跟随、真实后代存活触发 `RecoveryRequired` 且不被后续成功命令洗掉、已核验宿主可回收但仍挡在 `InUse` 之前。注入与写策略这两件另在 `it-exec/tests/sandbox_backends.rs`：`Isolated` 档下受限命令写 `$RUSTY_AGENT_TMPDIR` 再由宿主读回，证明目录确实落在沙箱可写集里，而不只是一个被传下去的环境变量 |

### R8-A 本地 Codex Rust 源码对照与采纳裁决（2026-08-11）

> 本节的实现证据基线为本机 `/Users/moses/workspace/custom-app/codex`，提交 `070a26a1f0`。目的不是复制 Codex 的产品 `core` 单体，而是在保持 `ra-core` / `ra-runtime` / `ra-tools` / `ra-coding` 分层的前提下，沉淀后续实现可逐文件对照的契约。Codex 的 `read_file` 不是其当前默认原生工具；Rusty 的 `read_file` 是有意保留的能力，须与 exec/patch 使用同一工作区边界。

#### Exec：可直接借鉴的执行脊柱

| Codex 源码与行为 | Rusty 采纳位置 | 裁决与验收不变量 |
| --- | --- | --- |
| `core/src/tools/handlers/unified_exec*.rs`、`tools/runtimes/unified_exec.rs` 把输入处理、策略/沙箱与运行时分开 | `ra-tools` schema/handler；`ra-exec` executor、sandbox 与 manager；`ra-coding` profile/policy | **采纳**。工具实现不得直接 `spawn`；未来本地/远程 executor 只能替换 `ra-exec` 内部实现，不能改变公开工具契约 |
| `core/src/unified_exec/process_manager.rs` 在首次 yield 前保存进程所有权 | `ra-exec::ProcessManager` | **采纳**。工具调用栈返回或被取消后，后台进程仍由 session manager 持有；不能让最后一个 `Child`/`Arc` 随初始调用取消而掉落 |
| 每个 live process 有独立交互锁；不同 process 可并发 | `ExecSessionId` 与 session lock | **采纳**。同会话 read/poll/write 串行、跨会话并行；所有输出读取都有 cursor/消费语义，测试覆盖并发 poll+stdin |
| `unified_exec/head_tail_buffer.rs` 原始收集限制为 1 MiB，头尾各半；再做模型响应 token 截断 | `ra-exec` 输出采集层、`ra-tools` 结果渲染层 | **采纳原则**。分离 capture byte budget、模型可见 token budget、增量 cursor；截断标记应报告省略字节/原因，不能把三者混作一个 `max_output_chars` |
| 运行时流式发 UI 输出，同时生成独立工具响应快照，并记录 terminal interaction | CodingHost event sink | **采纳**。最小宿主事件：`ExecStarted`、`ExecOutput`、`ExecYielded`、`TerminalInteraction`、`ExecExited`、`ExecEvicted`；模型 `ToolOutput` 不充当 UI/持久化事件载体。R9 再把同一事件模型落盘 |
| 默认初始 yield 10 s，并限定 yield/poll 区间；session 数量上限 64，清理 inactive entry | 生命周期配置 | **采纳语义、参数可不同**。明确 initial yield、poll wait、最大会话数、idle TTL、总存活 TTL、取消与 eviction 的行为；容量/TTL 终止整个进程组并写 `ExecEvicted{reason}` |
| `write_stdin` 无隐式换行，空输入为 poll；非 TTY 特判 Ctrl-C | `ra-tools/src/write_stdin.rs` | **采纳**。见 R8-2；参数校验与错误文本必须让模型能区分“会话不存在 / 已退出 / stdin 不可写 / 非 TTY 输入被拒” |
| 通过 `NO_COLOR=1`、`TERM=dumb`、禁 pager 等降低终端噪声，而非简单 regex 清 ANSI | `ra-exec` 环境构造与输出净化 | **采纳**。环境优先；如需 sanitizer，必须安全处理 CSI/OSC/CR，且不改原始采集统计 |

#### Apply patch：预览、冲突与提交事实

| Codex 源码与行为 | Rusty 采纳位置 | 裁决与验收不变量 |
| --- | --- | --- |
| `apply-patch/src/lib.rs` 先解析成 action/变化，再由 `core/src/tools/handlers/apply_patch.rs` 做权限与预览，runtime 执行 | `ra-patch` → `ra-coding::apply_patch` → workspace filesystem capability | **采纳**。`PatchPlan` 先于写入生成，包含规范化目标路径、预期 revision/snapshot、提议内容/删除/移动、匹配等级、源位置及冲突/歧义信息；审批展示计划而非原始 patch 文本 |
| Codex 即便后续失败也返回已提交的 `AppliedPatchDelta`；并非跨文件 all-or-nothing | `PatchResult` | **采纳真实语义**。R8 初版合同是“best effort + authoritative committed delta”；若以后要事务化，另行实现同目录临时文件、fsync、rename 与 rollback journal，未实现前不得写“原子” |
| `seek_sequence.rs`：exact → trim end → trim → 部分 Unicode 标点/空格归一化，从 offset 向 EOF 查找，取第一个匹配 | `ra-patch` 匹配器 | **有选择采纳**。保留分级匹配与每级可观测性；Rusty 对多个候选应拒绝并报告候选位置，不能静默取第一个。明确 line-ending policy：默认不静默将 CRLF 重写为 LF |
| `StreamingPatchParser` 可随 freeform 参数增量产生 patch 进度 | `ra-patch` parser API | **延后实现，预留接口**。R8 可先完整输入后 parse，但 AST/parser 不应被设计为只能一次性消费 |
| Codex 可截获 shell 里的 `apply_patch` 并转交补丁工具 | 工具路由 | **不采纳（当前）**。Rusty 已把 `apply_patch` 定义为 freeform 一等工具；不把 shell 文本重写成工具调用 |

#### R8 开工前必须冻结的类型与责任边界

1. `ExecRequest`、不透明 `ExecSessionId`、`ExecSessionState`、输出 cursor、精确 stdin 语义和生命周期/资源配置；状态迁移必须是可审计的单向终态迁移。**进程会话 id 必须叫 `ExecSessionId`，不能复用 `SessionId`**（2026-08-11 审查补）：R9-2a 的 `SessionId` 是本地权威历史，provider 的远端 conversation 另用 `ProviderConversationId`；三者所有者、生命周期、是否持久化全不同，还会同时出现在 `HostEvent` 与 `RunState` 相关记录里。两个类型今天都尚未存在，分名零成本。
2. `PatchPlan`、`PatchMatchLevel`、`PatchConflict`、`CommittedPatchDelta`；陈旧上下文由 patch 匹配失败与模型重读处理，不引入 read revision snapshot 或写前 freshness gate，`ra-patch` 保持纯函数。
3. `ToolOutput` 只服务模型；宿主事件用具名 enum/struct 服务 UI、审计与 R9 replay。不要向 `ObservationMetadata` 塞 `serde_json::Value` 形式的万能字段。
4. 工作区文件能力是内部受策略约束的接口；`read_file`、patch 写入、exec cwd 三条路径复用它。当前 `read_file` 还应补充文件元数据/流式读取上限，以及 descriptor-relative 或 no-follow 打开方式以缩小 canonicalize-then-open 的符号链接 TOCTOU 窗口。

#### 明确不照搬的部分

| 项目 | 原因与决策 |
| --- | --- |
| Codex `core` 产品单体、桌面/UI/云端耦合 | 不复制；Rusty 保持 framework 与 product 分层，补的是稳定的宿主事件和 workspace capability |
| remote executor / `exec-server` 全套平台能力 | R8 不做；仅让 `ExecRequest` 面向 executor 抽象，避免未来接口破坏 |
| 未经实现验证的跨文件原子 patch | 不宣称；R8 先提交 authoritative delta，事务语义单列后续工作 |
| 建立 Read Ledger / 文件 freshness 表 | 不做；Session 工具事件已经保留读写事实，额外状态表没有已证实收益 |
| 以任意 JSON 承载 exec/patch 事实 | 不做；以强类型结果和事件保证 R9 持久化、R14 replay 的稳定性 |

#### 源码复用政策：借鉴接口，不直接搬运产品结构体

Codex 本地仓库的 `LICENSE` 为 Apache-2.0；若确有必要复制其实现，必须保留许可证/NOTICE、标明修改，并先补 Rusty 自己的边界与回归测试。该许可结论不是“架构上应该直接复制”的结论。

**判据是「有没有把外部世界的假设一起带进来」，不是「是不是叶子 crate」。** 纯的东西签名自己会说：`seek_sequence` 是 `&[String]` 进、`Option<usize>` 出，`head_tail_buffer` 是字节进、字节出，两者都不知道文件系统、进程、provider 或 session 的存在。反例是 `utils/pty`（2850 行）——它是叶子，却携带进程组语义、信号处理与 Windows console 假设，而这些恰恰是 R8-2 必须自己拥有、自己测的部分。**按「叶子」筛会把它筛进来，按「假设」筛不会。**

**合规的落点必须是具体的，否则这条政策活不过第三次移植**：① 顶层 `NOTICE` 记录被移植的上游、许可证与提交号；② 每个移植文件头一行 `// Adapted from openai/codex (Apache-2.0) @ <commit>, modified.`；③ `xtask` 加一条很便宜的检查——带该注释的文件必须在 `NOTICE` 里有对应条目，反之亦然。三样缺一样，半年后就没人说得清哪些代码是搬来的。

| Codex 类型 | 结论 | Rusty 对应做法 |
| --- | --- | --- |
| `codex_tools::ToolExecutor<Invocation>` + `ToolExposure`（`codex-rs/tools/src/tool_executor.rs`） | **不直接搬** | Rusty 已有 provider-neutral `ra_core::Tool`、`ToolSchema`、`ToolOptions`；只吸收「spec/runtime 分离、工具 exposure 独立于注册、并行能力声明」三个概念。不要把 Codex 的 Code Mode / deferred-search surface、`codex_protocol` 类型带进框架核心 |
| `core::tools::registry::CoreToolRuntime` / `ToolRegistry` | **不直接搬** | 它把 hooks、telemetry、MCP、streamed argument diff、session 等 Codex 产品服务混在核心 registry。Rusty 在 `ra-runtime` 增加 product-agnostic dispatch/event extension point；产品功能留 `ra-coding` / capability 层 |
| `ToolInvocation` / `ToolOutput`（`core/src/tools/context.rs`、`tools/src/tool_output.rs`） | **结构可参考，不能同名移植** | Rusty 现有 invocation/output 已处理 provider-neutral schema、稳定 identity 与持久化兼容；R8 只补具名 exec/patch 事实和 host event，避免引入 `Arc<Session>` 让所有通用工具耦合 Codex session |
| `unified_exec/head_tail_buffer.rs`、`apply-patch/seek_sequence.rs` | **可受控改写/小范围移植** | 它们接近纯算法。先写 Rusty 测试与本地 API，再以 Apache-2.0 合规方式改写或带归属移植；不可同时抄入 Codex 的 runtime、错误模型和 filesystem 假设 |
| `utils/pty`（2850 行，含 `process_group.rs` / `windows_input.rs`） | **不移植，当踩坑清单读** | 它是叶子但**不纯**：进程组归属、信号与终止顺序、Windows console 输入编码全在里面，而这三样正是 R8-2 的核心职责，抄进来等于把「谁负责杀干净子进程」这个问题连同答案一起外包。读它是为了知道有哪些坑，实现要走 `portable-pty` + Rusty 自己的 `ExecSessionId` 与生命周期 |
| `apply-patch/src/parser.rs` + `text_file.rs`（V4A 语法） | **可受控改写/小范围移植** | 与上面同一档：输入是补丁文本、输出是结构化 action。**不发明新格式**这条已经定了，那么解析器就没有第二种写法值得自己再摸一遍 |
| 第三方依赖选型（`bm25` / `nucleo` / `ignore` / `globset` / `tree-sitter-bash` / `similar`） | **直接照抄选型** | 这不是移植而是省调研：`tool_search` 的 BM25、`file-search` 的 `nucleo`+`ignore`、`execpolicy` 用 tree-sitter 解析 bash AST 做命令判定，都是已被 Codex 生产验证过的组合 |

### R8 非目标

| 项目 | 处理 |
| --- | --- |
| 七家云沙箱适配（e2b / modal / daytona / …） | 不做；留 `SandboxBackend` trait 供第三方实现 |
| 自研 diff 格式 | 不做；用 V4A |
| 把 shell 收编成唯一工具 | 不做；AF 实测证据类工具保留是必要的 |

### R8 验收标准

| 能力 | 标准 |
| --- | --- |
| Codex 语义对齐 | **已达成**：`exec_command` + `write_stdin` + `apply_patch` 能完成一个真实多文件修改任务。<br>**这条此前既没标达成、也没人说过为什么不做**——三个工具各自的测试都在 `it-tools`，那些证明的是「工具能用」，而本条唯一要证的是「三个合起来能干成一件事」，没有任何测试覆盖。现补 `it-e2e/tests/coding_task.rs`：同一个版本号散在两个文件里，**一次 patch 同时改两处**；项目自带的检查脚本必须同意这次修改，所以得真跑它；而脚本是交互式的（打印 `READY` 后等 stdin），命令因此越过 yield，答案只能写进一个已经在跑的 session。<br>**模型不是固定脚本**：它读被交给的工具输出再决定下一步，因为第三步没有别的走法——session id 是运行时才有的，固定脚本叫不出它的名字。这顺带把一件值得单独断言的事变成断言：**模型要续接 session 所需要的那个标识，确实出现在工具给它的正文里**，而不是只躺在某条它永远看不到的记录的字段上；测试也按 provider 的做法只读文本块，不去 JSON 信封里捞。<br>**两次变异验证过它不是装饰**：只改一个文件时被挡下；两个文件都改但值不一致时，**是项目自己的检查脚本报出 `MISMATCH file=2.0.0 code=9.9.9`**，也就是 exec + write_stdin 这半边真的承重。<br>**顺带发现的产品事实**：默认权限模式下这个 run 会停在第一次 `apply_patch` 上等审批，一步都走不下去——这是对的，且各有归属测试（`it-runtime/tests/permission_engine.rs` 管引擎、`it-coding/tests/apply_patch.rs` 管审批看到什么），所以本条像一个执行已批准任务的宿主那样显式答掉这道闸，量的才是三个工具把活干完，不是闸门本身 |
| 隔离可信 | 宿主平台围栏与容器执行环境分开验收：越界写被拒、请求的网络策略生效、fail-closed。**宿主两项已有验收，容器未做（R8-8 / R8-9 / R8-10）**：seatbelt 在 macOS 上真 spawn 验过三项，bwrap 在 Linux CI 上原生验过三项（`it-exec/tests/sandbox_linux.rs`：宿主路径型 Unix socket、x32 syscall 编号、两档各自的文件边界），Docker 工作区 session（R8-10）未做，还须通过该条的工作区一致性、容器内策略、取消与回收及 daemon 失联验收。宿主路径的 fail-closed 是这条里**不靠平台**的部分，用显式传入后端的 `resolve_confinement_with` 在任何机器上都测得到：没有后端、后端不可用、够不到请求档位、降级会把网络还回来，四种都拒 |
| 后台可收尾 | **已达成（R8-3a）**：模型可见的等待与控制入口，形态是 `write_stdin{until, match_text, control}` 而不是第三个工具；名字 `background_shell_wait` 与 R2-8 的工具词表冲突，已作废。对应 codex 的 `unified_exec` + `write_stdin`：等待就是再调一次并带上时限。**原先挂在这里的 `background_job_closed` 交付点提醒随 R7-7 撤销**——收尾是产品提示词的事，不是框架 gate；宿主若要在交付点拦一次，走 R7-4 的 `stop` hook |

---


</details>
