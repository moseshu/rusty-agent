//! 配置的分层与来源追踪。
//!
//! # 这里只有机制
//!
//! 本模块不定义任何**具体配置项**——那些字段依赖各阶段的能力（provider、沙箱、
//! 上下文预算……），在各自阶段定义，用 [`Layered<T>`] 承载。这里给的是「一个值
//! 从哪来、谁盖过谁、为什么没生效」这套机制。读文件、读环境变量同样不在这里：
//! `ra-core` 不做 I/O，本模块只定**路径与命名约定**（[`CONFIG_DIR_NAME`]、
//! [`env_key`]），发现与解析由 `ra-cli` 做。
//!
//! # 六个来源，优先级从低到高
//!
//! | 来源 | 谁写的 | 典型位置 |
//! | --- | --- | --- |
//! | [`SettingSource::Builtin`] | 框架 | 代码里的默认值 |
//! | [`SettingSource::UserFile`] | 用户 | `~/.rusty-agent/config.toml` |
//! | [`SettingSource::ProjectFile`] | 团队 | `<项目>/.rusty-agent/config.toml`，进版本库 |
//! | [`SettingSource::LocalFile`] | 个人 | `<项目>/.rusty-agent/config.local.toml`，**不进版本库** |
//! | [`SettingSource::Env`] | 运维 / CI | `RA_MODEL_NAME=...` |
//! | [`SettingSource::Explicit`] | 调用方 | CLI 参数、API 入参 |
//!
//! 「团队配置 < 个人配置 < 环境变量 < 命令行」这个顺序对应的是**离这次运行有多
//! 近**：越贴近当下这一次调用的，越有权决定。
//!
//! # 两个刻意的设计
//!
//! **① 默认什么外部来源都不读。** [`SourceSelection::default`] 是
//! [`SourceSelection::isolated`]，即只有 `Builtin` + `Explicit` 生效。框架被嵌进
//! 别人的进程时，偷偷去读 `~/.rusty-agent/config.toml` 是不可接受的——那是宿主
//! 应用的用户目录，不是我们的。`ra-cli` 显式调 [`SourceSelection::all`] 打开。
//! 这条对齐 claude 的 `setting_sources`：**来源是选出来的，不是发现出来的**。
//!
//! **② 被排除的来源在诊断里仍然看得见。** 隔离开关（对齐 `strict_mcp_config`）
//! 让磁盘配置完全不生效，但如果 `doctor` 只显示生效值，用户会困在「我明明写了
//! 配置为什么没用」里。所以 [`FieldReport`] 把每一层都列出来，并区分
//! [`LayerStatus::Shadowed`]（被更高层盖了）与 [`LayerStatus::Excluded`]（来源
//! 压根没启用）——**这两种「没生效」的修法完全不同**。

use core::fmt;

/// 用户主目录与项目根下的配置目录名（同名，靠位置区分）。
pub const CONFIG_DIR_NAME: &str = ".rusty-agent";

/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// 个人覆盖文件名。**应当进 `.gitignore`**：它是给「我本机就要用另一个模型」这
/// 类需求的，进版本库就会把个人偏好强加给整个团队。
pub const LOCAL_CONFIG_FILE_NAME: &str = "config.local.toml";

/// 环境变量前缀。
pub const ENV_PREFIX: &str = "RA_";

/// 配置路径到环境变量名的映射：`model.name` → `RA_MODEL_NAME`。
///
/// 点与连字符都折成下划线，因此**配置路径的字段名里不要用连字符**——`a-b` 与
/// `a.b` 会撞到同一个环境变量上。
#[must_use]
pub fn env_key(path: &str) -> String {
    let mut key = String::with_capacity(ENV_PREFIX.len() + path.len());
    key.push_str(ENV_PREFIX);
    for ch in path.chars() {
        match ch {
            '.' | '-' => key.push('_'),
            other => key.extend(other.to_uppercase()),
        }
    }
    key
}

// ---------------------------------------------------------------------------
// 来源
// ---------------------------------------------------------------------------

/// 一个配置值来自哪一层。
///
/// 优先级是从变体**投影**出来的（[`Self::precedence`]），不是存储的字段——与
/// `Recoverability` 同一个路子，两者不可能不一致。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingSource {
    /// 框架内置默认值。**永远启用**：关掉它就没有兜底值了。
    Builtin,
    /// 用户级配置文件。
    UserFile,
    /// 项目级配置文件（进版本库，团队共享）。
    ProjectFile,
    /// 项目内的个人覆盖文件（不进版本库）。
    LocalFile,
    /// 环境变量。
    Env,
    /// 调用方显式传入：CLI 参数、API 入参。**永远启用**：这是本次调用的意图。
    Explicit,
}

impl SettingSource {
    /// 全部来源，按优先级从低到高。`doctor` 与门禁用它遍历。
    pub const ALL: &'static [Self] = &[
        Self::Builtin,
        Self::UserFile,
        Self::ProjectFile,
        Self::LocalFile,
        Self::Env,
        Self::Explicit,
    ];

    /// 优先级，越大越优先。数值本身无意义，只有序关系是契约。
    ///
    /// 留出间隔是为了将来能往中间插层（比如「插件带来的默认值」）而不必改动
    /// 已有档位的相对关系。
    #[must_use]
    pub const fn precedence(self) -> u16 {
        match self {
            Self::Builtin => 0,
            Self::UserFile => 100,
            Self::ProjectFile => 200,
            Self::LocalFile => 300,
            Self::Env => 400,
            Self::Explicit => 500,
        }
    }

    /// 稳定的机器可读标识。进 `doctor` 输出与 trace。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::UserFile => "user",
            Self::ProjectFile => "project",
            Self::LocalFile => "local",
            Self::Env => "env",
            Self::Explicit => "explicit",
        }
    }

    /// 是否是**从磁盘或环境里发现**的来源。
    ///
    /// 隔离开关关掉的正是这一档：[`Self::Builtin`] 与 [`Self::Explicit`] 不是发现
    /// 来的，一个是代码写死的兜底，一个是调用方当场传的，都没有「意外生效」的风险。
    #[must_use]
    pub const fn is_discovered(self) -> bool {
        !matches!(self, Self::Builtin | Self::Explicit)
    }

    /// 是否来自配置文件。
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, Self::UserFile | Self::ProjectFile | Self::LocalFile)
    }
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// 来源选择
// ---------------------------------------------------------------------------

/// 哪些来源参与解析。对齐 claude 的 `setting_sources`。
///
/// 只管得着**发现来的**那四档（[`SettingSource::is_discovered`]）；`Builtin` 与
/// `Explicit` 恒定启用，因此不存在「什么都没配上」的荒谬状态。
///
/// ```ignore
/// let 隔离 = SourceSelection::isolated();          // 只认内置默认 + 调用方传入
/// let 常规 = SourceSelection::all();               // ra-cli 用这个
/// let 定制 = SourceSelection::isolated().with(SettingSource::Env);  // CI：只认环境变量
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSelection {
    /// 已启用的**发现来源**位集。恒定启用的两档不占位——见 [`bit`]。
    discovered: u8,
}

/// 来源在位集里的位。恒定启用的来源返回 0，于是开关它们自然成为无操作，不需要
/// 在每个方法里再写一次特判。
const fn bit(source: SettingSource) -> u8 {
    match source {
        SettingSource::UserFile => 1 << 0,
        SettingSource::ProjectFile => 1 << 1,
        SettingSource::LocalFile => 1 << 2,
        SettingSource::Env => 1 << 3,
        SettingSource::Builtin | SettingSource::Explicit => 0,
    }
}

impl SourceSelection {
    /// 只认 [`SettingSource::Builtin`] 与 [`SettingSource::Explicit`]，磁盘与环境
    /// 完全不生效。对齐 `strict_mcp_config` 的隔离语义。
    ///
    /// 这是[默认值](Self::default)。
    #[must_use]
    pub const fn isolated() -> Self {
        Self { discovered: 0 }
    }

    /// 启用全部来源。**`ra-cli` 显式用这个**，库调用方按需自选。
    #[must_use]
    pub const fn all() -> Self {
        Self {
            discovered: bit(SettingSource::UserFile)
                | bit(SettingSource::ProjectFile)
                | bit(SettingSource::LocalFile)
                | bit(SettingSource::Env),
        }
    }

    /// 启用一个来源。对恒定启用的来源是无操作。
    #[must_use]
    pub const fn with(mut self, source: SettingSource) -> Self {
        self.discovered |= bit(source);
        self
    }

    /// 关闭一个来源。对恒定启用的来源是无操作——[`SettingSource::Builtin`] 与
    /// [`SettingSource::Explicit`] **关不掉**。
    #[must_use]
    pub const fn without(mut self, source: SettingSource) -> Self {
        self.discovered &= !bit(source);
        self
    }

    /// 该来源是否参与解析。
    #[must_use]
    pub const fn allows(self, source: SettingSource) -> bool {
        !source.is_discovered() || self.discovered & bit(source) != 0
    }

    /// 是否处于隔离模式：没有任何发现来的来源生效。
    #[must_use]
    pub const fn is_isolated(self) -> bool {
        self.discovered == 0
    }
}

impl fmt::Debug for SourceSelection {
    /// 列出启用了哪些来源。派生出来的 `discovered: 5` 对诊断毫无帮助，而这个类型
    /// 出现的场合几乎都是在诊断。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceSelection(")?;
        if self.is_isolated() {
            f.write_str("isolated")?;
        } else {
            let mut first = true;
            for source in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
                if self.allows(*source) {
                    if !first {
                        f.write_str("+")?;
                    }
                    f.write_str(source.label())?;
                    first = false;
                }
            }
        }
        f.write_str(")")
    }
}

impl Default for SourceSelection {
    /// [`Self::isolated`]——**默认不读任何外部配置**。理由见模块文档。
    fn default() -> Self {
        Self::isolated()
    }
}

// ---------------------------------------------------------------------------
// 带来源的值
// ---------------------------------------------------------------------------

/// 一个值加上它来自哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sourced<T> {
    source: SettingSource,
    value: T,
}

impl<T> Sourced<T> {
    /// 构造。
    #[must_use]
    pub const fn new(source: SettingSource, value: T) -> Self {
        Self { source, value }
    }

    /// 来源层。
    #[must_use]
    pub const fn source(&self) -> SettingSource {
        self.source
    }

    /// 值。
    #[must_use]
    pub const fn value(&self) -> &T {
        &self.value
    }

    /// 丢掉来源，取出值。
    #[must_use]
    pub fn into_value(self) -> T {
        self.value
    }
}

// ---------------------------------------------------------------------------
// 分层值
// ---------------------------------------------------------------------------

/// 一个配置项在各层的候选值。
///
/// 每层最多一个值——同一来源再次 [`set`](Self::set) 会**覆盖**而不是追加（一个
/// 文件对一层，重复只可能来自解析同一层两次）。
#[derive(Debug, Clone)]
pub struct Layered<T> {
    entries: Vec<Sourced<T>>,
}

impl<T> Layered<T> {
    /// 空的分层值：没有任何层给过值。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 带内置默认值构造。绝大多数配置项都该有兜底。
    #[must_use]
    pub fn builtin(value: T) -> Self {
        let mut layered = Self::new();
        layered.set(SettingSource::Builtin, value);
        layered
    }

    /// 写入某一层的值，覆盖该层原有的值。
    pub fn set(&mut self, source: SettingSource, value: T) -> &mut Self {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.source == source) {
            existing.value = value;
        } else {
            self.entries.push(Sourced::new(source, value));
        }
        self
    }

    /// 是否任何一层都没给过值。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 取某一层的原始值，**不考虑优先级与来源选择**。诊断用。
    #[must_use]
    pub fn get(&self, source: SettingSource) -> Option<&T> {
        self.entries
            .iter()
            .find(|e| e.source == source)
            .map(Sourced::value)
    }

    /// 按优先级解析出生效值，跳过未启用的来源。
    #[must_use]
    pub fn resolve(&self, selection: SourceSelection) -> Option<Sourced<&T>> {
        self.entries
            .iter()
            .filter(|e| selection.allows(e.source))
            .max_by_key(|e| e.source.precedence())
            .map(|e| Sourced::new(e.source, &e.value))
    }

    /// 生效值本身。要知道它来自哪一层用 [`Self::resolve`]。
    #[must_use]
    pub fn value(&self, selection: SourceSelection) -> Option<&T> {
        self.resolve(selection).map(|s| *s.value())
    }

    /// 各层来源，按优先级从高到低。
    pub fn sources(&self) -> impl Iterator<Item = SettingSource> {
        let mut sources: Vec<SettingSource> = self.entries.iter().map(Sourced::source).collect();
        sources.sort_unstable_by_key(|s| core::cmp::Reverse(s.precedence()));
        sources.into_iter()
    }
}

impl<T: fmt::Display> Layered<T> {
    /// 生成诊断报告——`ra doctor config` 的数据来源。
    ///
    /// 值在这里被字符串化：报告要能把不同类型的配置项排在同一张表里。
    #[must_use]
    pub fn report(&self, key: impl Into<String>, selection: SourceSelection) -> FieldReport {
        let effective = self.resolve(selection).map(|s| s.source());

        let mut layers: Vec<LayerReport> = self
            .entries
            .iter()
            .map(|entry| LayerReport {
                source: entry.source,
                value: entry.value.to_string(),
                status: if !selection.allows(entry.source) {
                    LayerStatus::Excluded
                } else if Some(entry.source) == effective {
                    LayerStatus::Effective
                } else {
                    LayerStatus::Shadowed
                },
            })
            .collect();
        layers.sort_unstable_by_key(|l| core::cmp::Reverse(l.source.precedence()));

        FieldReport {
            key: key.into(),
            layers,
        }
    }
}

impl<T> Default for Layered<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 诊断
// ---------------------------------------------------------------------------

/// 某一层的值为什么生效、或为什么没生效。
///
/// 把「被盖了」与「没启用」分开是本模块存在的一半理由：前者要改更高层的配置，
/// 后者要改来源选择，**修法完全不同**。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerStatus {
    /// 这一层的值就是最终生效值。
    Effective,
    /// 被更高优先级的层盖住了。
    Shadowed,
    /// 该来源未启用，压根没参与解析。
    Excluded,
}

impl LayerStatus {
    /// 稳定的机器可读标识。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Effective => "effective",
            Self::Shadowed => "shadowed",
            Self::Excluded => "excluded",
        }
    }
}

impl fmt::Display for LayerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 一层在诊断报告里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerReport {
    source: SettingSource,
    value: String,
    status: LayerStatus,
}

impl LayerReport {
    /// 来源层。
    #[must_use]
    pub const fn source(&self) -> SettingSource {
        self.source
    }

    /// 该层的值（已字符串化）。
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// 生效情况。
    #[must_use]
    pub const fn status(&self) -> LayerStatus {
        self.status
    }
}

/// 一个配置项的完整来源报告。`ra doctor config` 每行一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReport {
    key: String,
    layers: Vec<LayerReport>,
}

impl FieldReport {
    /// 配置项路径，如 `model.name`。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 所有给过值的层，按优先级从高到低。含未启用的层——**看得见才诊断得了**。
    #[must_use]
    pub fn layers(&self) -> &[LayerReport] {
        &self.layers
    }

    /// 生效的那一层。所有层都没给值、或给了值的层都未启用时为 `None`。
    #[must_use]
    pub fn effective(&self) -> Option<&LayerReport> {
        self.layers
            .iter()
            .find(|l| l.status == LayerStatus::Effective)
    }

    /// 是否存在「配了但没生效」的层——`doctor` 该对这些配置项重点提示。
    #[must_use]
    pub fn has_ineffective_layers(&self) -> bool {
        self.layers
            .iter()
            .any(|l| l.status != LayerStatus::Effective)
    }
}

impl fmt::Display for FieldReport {
    /// 一行摘要：`model.name = gpt-5 (env)`。逐层明细走 [`FieldReport::layers`]，
    /// 排版是 `ra-cli` 的事。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.effective() {
            Some(layer) => write!(f, "{} = {} ({})", self.key, layer.value, layer.source),
            None => write!(f, "{} = <未设置>", self.key),
        }
    }
}
