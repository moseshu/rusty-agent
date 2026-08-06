# 示例与 Demo 目录 (Examples)

本目录用于存放供开发者快速熟悉和学习 **`rusty-agent`** 框架的可执行示例与样例代码。

---

## 📌 目录定位与使用说明

目前 `rusty-agent` 框架的核心模块（包含 `crates/` 内部组件与基础设施）正在按[开发计划](../Docs/Rusty_Agent_Framework_Development_Plan.md)有序推进开发中。

随着核心功能逐步完成，本目录将陆续补充以下标准的 Rust 示例程序：

### 规划中的示例列表

| 示例文件 | 演示主题 | 说明与核心概念 |
| :--- | :--- | :--- |
| **`01_basic_agent.rs`** | 基础 Agent 对话 | 演示如何初始化 Provider Client、配置 System Preamble 提示词，并发起多轮对话。 |
| **`02_custom_tools.rs`** | 自定义工具与 Function Calling | 演示如何使用 `#[tool]` 宏注册自定义 Rust 函数，并处理参数 Schema 与错误。 |
| **`03_react_loop.rs`** | ReAct 思考与行动循环 | 演示基于 `NextStep` 状态机的 ReAct (Reasoning + Acting) 双通道输出与自纠流程。 |
| **`04_subagent_as_tool.rs`** | 子 Agent 派生与上下文隔离 | 演示通过 `Agent::as_tool()` 将子 Agent 包装为工具，实现文件级的上下文隔离。 |
| **`05_mcp_integration.rs`** | MCP 协议集成 | 演示如何连接外部或进程内 MCP (Model Context Protocol) 工具与资源服务器。 |

---

## 🚀 运行示例（核心框架完成后）

后续核心模块开发完成后，开发者可直接在终端通过 Cargo 运行任意示例：

```bash
# 设置环境变量
export OPENAI_API_KEY="your-openai-api-key"
# 或
export GOOGLE_API_KEY="your-google-api-key"

# 运行指定示例
cargo run --example 01_basic_agent
cargo run --example 02_custom_tools
```
