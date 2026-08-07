# tests/ —— 独立测试 workspace

## 为什么单独一个 workspace

1. **主 workspace 的依赖图不被测试污染**：`insta` / `wiremock` / `rstest` 只出现在
   这里。主仓库 `cargo build --workspace` 不会为了跑一次构建去拉测试专用依赖，
   `cargo tree` 看到的也就是真实的产品依赖。
2. **内核与服务零测试代码**：`crates/` 下**禁止** `#[cfg(test)] mod tests`，
   由 `cargo xtask no-inline-tests` 在 CI 强制。

**本目录进版本库**（只有 `target/`、`Cargo.lock` 与个人实验 `test-gemini/` 被忽略）。
断言是 R0 各项"已完成"的唯一证据，必须能被 review、能在 CI 上跑；放在忽略目录里
等于把证据留在一台机器上。

## 布局

```
tests/
├── Cargo.toml        # 独立 workspace
├── it-core/          # 每个被测 crate 一个宿主 crate
│   ├── src/lib.rs    # 空，仅为让 cargo 认这是个 crate
│   └── tests/*.rs    # 真正的测试用例
├── it-model/
├── ...
├── it-eval/          # ra-eval 宿主（每个库 crate 都有一个）
└── it-e2e/           # 跨 crate 端到端
```

`it-e2e/fixtures/` 里的 crate 是编译期负向夹具，各自声明空 `[workspace]`，不会成为
测试 workspace 的 member。R0-1 用它们验证内部模块确实无法被下游引用；测试以
`cargo check --offline` 运行，避免把网络状态误判成边界失败。

不预建空测试源文件。能力实现时，测试文件必须和第一条真实断言一起创建；
`cargo xtask test` 会先**从 `crates/` 推导**宿主映射（每个库 crate 必须有 `it-<后缀>`
宿主并 path 依赖它），并拒绝没有测试函数的占位文件，再启动 Cargo。宿主表不写死在
任何地方——新建一个库 crate，门禁当天就会要求它的宿主。
`it-e2e/tests/workspace_contract.rs` 用同一套推导反向断言 workspace 隔离、零内联测试
与现代 `foo.rs + foo/` 模块布局。

## 运行

```bash
cargo test --manifest-path tests/Cargo.toml            # 全部
cargo test --manifest-path tests/Cargo.toml -p it-core # 单个
cargo xtask test                                        # 等价封装
```

## 测私有项怎么办

集成测试只能触达 `pub` API。确需覆盖内部实现时，给对应 crate 加一个
`testing` feature，在 `src/` 里暴露一个 `#[doc(hidden)] pub mod testing`
重导出内部项，测试侧用 `features = ["testing"]` 打开。

**这是例外不是常态**：先考虑该行为是不是本就应该出现在公开契约上。
