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
cargo xtask test                                        # 全部（推荐入口）
cargo xtask test -p it-core                             # 单个宿主
```

默认用 `cargo test`。门禁输出会写明用的是哪个 runner，免得两次绿没有可比性。

直接用 cargo 也一样：

```bash
cargo test --manifest-path tests/Cargo.toml -p it-core
```

### 为什么慢，以及能做什么

这里是**五十多个独立测试二进制**，断言本身几乎不花时间（日志里满屏
`finished in 0.00s`），成本全在编译链接和一个个起进程上。

1. **`[profile.dev] debug = 0`**（已配在 `tests/Cargo.toml`）。调试信息是链接器要搬
   运的最大一块，关掉直接砍链接时间，顺带让 `tests/target` 不再动辄几十 GB。断言失败
   照样打印 `文件:行`（那来自 panic location），只是 backtrace 没有行号；要用调试器时
   临时 `--config profile.dev.debug=2`。

2. **每个新链接出来的二进制，首次执行要多花十几秒**。这是 macOS 对未签名 / 未公证可执行
   文件在首次运行时的在线校验，实测形态很清楚（数字来自一台开发机）：

   | 场景 | 耗时 |
   | --- | --- |
   | 刚链接出来、首次执行 | ~18s，全程 0% CPU |
   | 同一个二进制再跑 | 0.008s |
   | 同样的内容换个路径 | ~1s（按内容缓存，不按路径） |

   一次 `ra-core` 改动会重链五十多个二进制，于是 **≈16 分钟纯等待**，与测试本身无关；只改
   一个测试文件则只重链那一个。**系统设置 → 隐私与安全性 → 开发者工具**里给终端开豁免，
   实测**不能**消掉这一项（它管的是"允许运行不满足策略的软件"，不是那次在线查询）。真正
   有效的方向是让这台机器到 Apple 校验端点的网络通畅——代理 / VPN / 防火墙拦住时就是这种
   固定十几秒的超时形态。

3. **[cargo-nextest](https://nexte.st) 是 opt-in 的**（`cargo install cargo-nextest --locked`，
   配置在 `tests/.config/nextest.toml`）：

   ```bash
   RA_TEST_RUNNER=nextest cargo xtask test
   ```

   它跨二进制并行，正好治这里"进程启停占大头"的病。**默认不启用**：在这台机器上整仓跑它
   会停在 list 阶段——枚举用的子进程 0% CPU 无限期挂着，而只跑单个宿主时秒回。原因未查明，
   只在规模上来时复现。挂住的门禁比慢的门禁更糟，所以默认留给 `cargo test`。

4. **迭代时只跑受影响的宿主**，全量留给提交前那一次。注意 `-p it-core` 与全量构建的
   feature 合并结果不同，来回切会触发重编——同一轮里固定一种选择最省。

## 测私有项怎么办

集成测试只能触达 `pub` API。确需覆盖内部实现时，给对应 crate 加一个
`testing` feature，在 `src/` 里暴露一个 `#[doc(hidden)] pub mod testing`
重导出内部项，测试侧用 `features = ["testing"]` 打开。

**这是例外不是常态**：先考虑该行为是不是本就应该出现在公开契约上。
