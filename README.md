# rusty-agent

用 Rust 写的 agent 框架，以及用它构建的参考产品。

> **当前状态：R0 已完成。** crate 边界、错误、取消、日志、配置、独立测试 workspace
> 与扩展安全契约已落地；下一阶段开始实现 provider 抽象。
> 公共 API 处于 `0.0.x`，不提供任何稳定性承诺。

## 这是什么

两件事，不是一件：

1. **一套通用 agent 框架** —— 用于实现 ReAct、graph engineering、plan-and-execute、multi-agent 四类智能体；
2. **用这套框架写出的参考产品** —— 它们的作用是把框架抽象逼到正确的形状。

## 分层

判断一段代码的落点只问两句话：

| 提问 | 答「是」则 |
| --- | --- |
| 换成另一个领域的 agent，这段代码要改吗？ | 产品内容 |
| 不用改，但换个产品要再写一遍吗？ | 可复用件 |
| 两条都答「否」 | 内核机制 |

```
产品层        ra-coding（编码 agent）· 第三方产品
可复用件层     ra-flow（编排与图引擎）· ra-tools（通用工具）· ra-patch（V4A 补丁）
内核层        ra-runtime（loop 内核）· ra-core（类型与契约）
通用服务层     ra-model · ra-prompt · ra-context · ra-session · ra-exec · ra-mcp
```

依赖方向单向，由 CI 校验：内核不依赖可复用件与产品，可复用件不依赖产品，产品之间零依赖。

## Crate

| crate | 职责 |
| --- | --- |
| `ra-core` | 公共类型与契约：`RunItem` / `ModelRequest` / `Tool` / `Capability` / `Guard` / `Permission` / `RunState` |
| `ra-macros` | `#[derive(ToolInput)]` / `#[tool]` 过程宏 |
| `ra-model` | provider 实现：OpenAI Responses / OpenAI Chat / Anthropic Messages / OpenAI-compatible |
| `ra-prompt` | 提示词装配、稳定前缀、缓存计划、增量提醒 |
| `ra-context` | 上下文预算、压缩、淘汰、归档 |
| `ra-runtime` | loop 内核：turn 结算、工具分派、guard/hook、审批中断 |
| `ra-session` | 事件日志、会话存储、resume、fork、checkpoint |
| `ra-exec` | 进程、PTY、后台 job、沙箱后端 |
| `ra-mcp` | MCP client（stdio / SSE / HTTP）与进程内工具服务器 |
| `ra-protocol` | 控制协议帧、transport、app-server |
| `ra-eval` | fixture、replay、trace 断言、成本与纪律报告 |
| `ra-patch` | V4A `apply_patch` 解析与应用 |
| `ra-coding` | 参考产品：编码 agent |
| `ra-cli` | 命令行入口 |

## 构建

需要 Rust 1.97.1（`rust-toolchain.toml` 已固定）。

```bash
cargo check --workspace
```

## License

Apache-2.0，见 [LICENSE](LICENSE)。
