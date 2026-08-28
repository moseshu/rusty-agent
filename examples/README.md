# 示例与 Demo 目录 (Examples)

本目录用于存放供开发者快速熟悉和学习 **`rusty-agent`** 框架的可执行示例与样例代码。

---

## 📌 目录定位与使用说明

目前 `rusty-agent` 框架的核心模块（包含 `crates/` 内部组件与基础设施）正在按[开发计划](../Docs/Rusty_Agent_Framework_Development_Plan.md)有序推进开发中。

随着核心功能逐步完成，本目录将陆续补充以下标准的 Rust 示例程序。

### 已交付

| 示例 | 演示主题 | 说明 |
| :--- | :--- | :--- |
| **`minimal_agent/`** | 最小自定义 agent | 只依赖 `ra-core` + `ra-runtime`，自带一个 `Tool`、一个 `Model` 与一个 `ModelResolver`，离线跑完「工具调用 → 最终回答」两轮。 |

`minimal_agent` **不是教程，是门禁**。它存在的理由是证明第三方只用内核就能装出一个能跑的 agent，因此它的依赖被 `xtask/src/layering.rs` 的 `ALLOWED_INTERNAL_DEPS` 钉死为那两个 crate——想加第三个来把例子写完，说明某个第三方需要的能力躲进了参考产品，`cargo xtask layering` 会当场失败并点名。**放宽那一行不是修复**。对应里程碑 M1 与 MVP 验收第 6b 项。

它自带 `Model` 也是这个理由的一部分：真实 provider 在 `ra-model`，依赖它只能证明「你能跑本项目的 adapter」，证明不了「你能自带 adapter」。附带好处是无需 API key、无需联网，可以进 CI。

```bash
cargo run -p minimal_agent
```

### 规划中的示例列表

| 示例文件 | 演示主题 | 说明与核心概念 |
| :--- | :--- | :--- |
| **`01_basic_agent.rs`** | 基础 Agent 对话 | 演示如何初始化 Provider Client、配置 System Preamble 提示词，并发起多轮对话。 |
| **`02_custom_tools.rs`** | 自定义工具与 Function Calling | 演示如何使用 `#[tool]` 宏注册自定义 Rust 函数，并处理参数 Schema 与错误。 |
| **`03_react_loop.rs`** | ReAct 思考与行动循环 | 演示基于 `NextStep` 状态机的 ReAct (Reasoning + Acting) 双通道输出与自纠流程。 |
| **`04_subagent_as_tool.rs`** | 子 Agent 派生与上下文隔离 | 演示通过 `Agent::as_tool()` 将子 Agent 包装为工具，实现文件级的上下文隔离。 |
| **`05_mcp_integration.rs`** | MCP 协议集成 | 演示如何连接外部或进程内 MCP (Model Context Protocol) 工具与资源服务器。 |

---

## 🚀 运行示例

每个示例是 workspace 里独立的一个 crate（`examples/*` 已登记为 workspace member），因此用 `-p` 运行，不是 `--example`：

```bash
cargo run -p minimal_agent
```

`minimal_agent` 自带模型，不需要任何环境变量。规划中的其余示例接真实 provider，届时需要：

```bash
export OPENAI_API_KEY="your-openai-api-key"
# 或
export GOOGLE_API_KEY="your-google-api-key"
```
