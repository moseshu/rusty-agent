//! 跨版本兼容：schema 版本与未知字段保留（扩展安全第 6 条）。
//!
//! # 要解决的事
//!
//! `RunState` / `WorkState` / rollout 行都要跨版本读写。**降级读取才是难的那一半**：
//! 新版本写出来的记录，旧版本读到时会遇见不认识的字段。默认的 serde 行为是**默默
//! 丢掉**，于是「新版写 → 旧版读 → 旧版再写」这条路径会**静默删数据**——用户只会
//! 看到 resume 之后某些状态没了，且无从追查。
//!
//! 所以三条策略在 R0 定死：
//!
//! | # | 策略 | 载体 |
//! | ---: | --- | --- |
//! | 1 | 每个可序列化结构带 `schema_version` | [`SchemaVersion`] |
//! | 2 | 新增字段一律 `#[serde(default)]` | serde 属性，无需类型支持 |
//! | 3 | **未知字段保留并原样回写，不报错** | [`Unknown`] |
//!
//! # 用法
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! struct RunState {
//!     schema_version: SchemaVersion,
//!     #[serde(default)]
//!     max_turns: u32,
//!     /// 必须放在最后：flatten 会吃掉所有没被前面字段认领的键。
//!     #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
//!     unknown: Unknown,
//! }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 可序列化结构的 schema 版本。
///
/// 序列化成裸整数（`"schema_version": 3`），不是对象——它要能被任何语言的读取方
/// 一眼看懂，包括不用 Rust 的宿主。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    /// 构造。
    #[must_use]
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    /// 版本号。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// 读到的记录相对**本地代码**的兼容性。
    #[must_use]
    pub const fn compatibility(self, local: Self) -> Compatibility {
        if self.0 == local.0 {
            Compatibility::Same
        } else if self.0 < local.0 {
            Compatibility::Older
        } else {
            Compatibility::Newer
        }
    }
}

impl core::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// 读到的 schema 版本与本地的关系。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compatibility {
    /// 同版本，直接读。
    Same,
    /// 记录比本地旧：本地新增的字段走 `#[serde(default)]` 补齐。
    Older,
    /// 记录比本地新：**仍然要读**，多出来的字段进 [`Unknown`] 并在回写时原样带回。
    ///
    /// 这一档不报错是刻意的——报错等于「装了旧版客户端就打不开会话」，而字段只增
    /// 不删加上未知字段保留，已经让降级读取是安全的。
    Newer,
}

impl Compatibility {
    /// 稳定的机器可读标识。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Older => "older",
            Self::Newer => "newer",
        }
    }

    /// 是否需要迁移后才能安全使用（R6-6 的迁移链入口）。
    #[must_use]
    pub const fn needs_migration(self) -> bool {
        matches!(self, Self::Older)
    }
}

impl core::fmt::Display for Compatibility {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// 未被任何已知字段认领的键值对。
///
/// 配 `#[serde(flatten)]` 使用：反序列化时兜住所有多余的键，序列化时原样写回。
/// 这样「新版写 → 旧版读 → 旧版再写」不会丢数据。
///
/// 用 [`BTreeMap`] 而不是 `HashMap`：**回写顺序必须确定**，否则同一份数据每次写出
/// 的字节都不同，diff、快照测试与内容寻址全部失效。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Unknown(BTreeMap<String, Value>);

impl Unknown {
    /// 空集。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否没有未知字段。用作 `skip_serializing_if`，让干净的记录写出来不带空对象。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 未知字段个数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 取一个未知字段。
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// 按键的字典序遍历。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    /// Folds another layer's unknown fields in, with the incoming layer winning on conflict.
    ///
    /// Crate-internal on purpose: unknown fields are produced by deserialization, not by callers.
    /// Layered resolution is the one place that legitimately combines two of these sets, and a
    /// merged value that silently carried fewer fields than the layer it came from would defeat
    /// the whole point of retaining them.
    pub(crate) fn extend_from(&mut self, other: &Self) {
        self.0.extend(
            other
                .0
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

impl<'a> IntoIterator for &'a Unknown {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::collections::btree_map::Iter<'a, String, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
