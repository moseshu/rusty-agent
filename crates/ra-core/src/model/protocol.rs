//! `ApiProtocol`：线上协议的显式建模。
//!
//! 三套协议的能力**不等价**，把差异藏起来会让 `ra-runtime` 悄悄依赖某一种的语义。
//! 因此协议是 `ra-core` 的一等类型，`ModelRequest` 保持协议中立，
//! 由 `ra-model` 的各适配器负责下降（lowering）。
//!
//! | 能力 | Responses | Chat Completions | Anthropic Messages |
//! | --- | --- | --- | --- |
//! | reasoning 项 | 一等，可回传 `encrypted_content` | 仅 `reasoning_content` 字符串，需回放 | 一等 `thinking` 块 + 签名 |
//! | 服务端会话 | `previous_response_id` / `conversation_id` | 无 | 无 |
//! | 稳定前缀位置 | 顶层 `instructions` | `messages[0]` 的 system | `system[]` 数组分块 |
//! | 提示缓存 | 显式 `prompt_cache_key` | 依赖前缀自动命中 | 显式 `cache_control` 断点 |
//! | 工具调用载体 | `function_call` item | message 里的 `tool_calls` | `tool_use` 内容块 |
//! | 工具结果载体 | `function_call_output` item | `role: "tool"` 消息 | `tool_result` 内容块 |
//! | 结构化输出 | `text.format` | `response_format` | `output_format` |
