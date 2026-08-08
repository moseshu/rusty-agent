//! `ra-core`: behavioral assertions for cross-version compatibility (R0-8 rule 6).
//!
//! What is locked down here is that **a downgrade read loses nothing**: when a record written by a
//! newer build is read by an older one and written back, the extra fields must still be there
//! untouched. serde's default is to drop them silently — which shows up to the user as "some state
//! vanished after a resume", with no way to trace it.

use ra_core::compat::{Compatibility, SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

/// The record as "older code" sees it: it knows only two fields.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct 旧版记录 {
    schema_version: SchemaVersion,
    #[serde(default)]
    max_turns: u32,
    /// flatten has to come last: it claims every key the preceding fields did not.
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

/// The JSON a newer build writes: two fields the older one does not know.
const 新版写出的: &str = r#"{
    "schema_version": 2,
    "max_turns": 12,
    "新增的标量": "值",
    "新增的对象": { "嵌套": [1, 2, 3] }
}"#;

// ---------------------------------------------------------------------------
// unknown-field retention
// ---------------------------------------------------------------------------

#[test]
fn 未知字段被兜住而不是丢弃() {
    let 记录: 旧版记录 = serde_json::from_str(新版写出的).expect("旧版应当能读新版记录");

    assert_eq!(记录.max_turns, 12, "已知字段照常解析");
    assert_eq!(记录.unknown.len(), 2, "两个不认识的字段应当被兜住");
    assert!(记录.unknown.get("新增的标量").is_some());
    assert!(记录.unknown.get("新增的对象").is_some());
}

#[test]
fn 已知字段不会跑进未知集() {
    let 记录: 旧版记录 = serde_json::from_str(新版写出的).expect("应能解析");

    assert!(记录.unknown.get("max_turns").is_none());
    assert!(记录.unknown.get("schema_version").is_none());
}

#[test]
fn 回写时未知字段原样带回() {
    // The "new writes, old reads, old writes again" path must not lose data.
    let 记录: 旧版记录 = serde_json::from_str(新版写出的).expect("应能解析");
    let 回写 = serde_json::to_string(&记录).expect("应能序列化");
    let 解析: serde_json::Value = serde_json::from_str(&回写).expect("回写结果应是合法 JSON");

    assert_eq!(解析["新增的标量"], "值", "标量字段丢了：{回写}");
    assert_eq!(解析["新增的对象"]["嵌套"][2], 3, "嵌套结构丢了：{回写}");
    assert_eq!(解析["max_turns"], 12);
    assert_eq!(解析["schema_version"], 2);
}

#[test]
fn 干净的记录不写出空对象() {
    let 记录 = 旧版记录 {
        schema_version: SchemaVersion::new(2),
        max_turns: 5,
        unknown: Unknown::new(),
    };
    let 回写 = serde_json::to_string(&记录).expect("应能序列化");

    assert!(记录.unknown.is_empty());
    assert!(
        !回写.contains("unknown"),
        "skip_serializing_if 没生效，记录里多了噪音：{回写}"
    );
}

#[test]
fn 未知字段回写顺序确定() {
    // BTreeMap rather than HashMap: an unstable order makes the same data serialize to different
    // bytes each time, which breaks snapshot tests and content addressing.
    let json = r#"{"schema_version":1,"max_turns":1,"z":1,"a":2,"m":3}"#;
    let 记录: 旧版记录 = serde_json::from_str(json).expect("应能解析");

    let 第一次 = serde_json::to_string(&记录).expect("应能序列化");
    let 第二次 = serde_json::to_string(&记录).expect("应能序列化");
    assert_eq!(第一次, 第二次);

    let 键序: Vec<&str> = 记录.unknown.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(键序, vec!["a", "m", "z"], "未知字段应按键的字典序排列");
}

#[test]
fn 缺字段的旧记录能被新代码读出() {
    // Rule 2: every added field is #[serde(default)].
    let 旧数据 = r#"{"schema_version":1}"#;
    let 记录: 旧版记录 = serde_json::from_str(旧数据).expect("缺字段应走默认值而不是报错");

    assert_eq!(记录.max_turns, 0);
    assert!(记录.unknown.is_empty());
}

// ---------------------------------------------------------------------------
// schema versions
// ---------------------------------------------------------------------------

#[test]
fn 版本号序列化成裸整数() {
    // The host may not be written in Rust, so the version has to be obvious at a glance.
    let json = serde_json::to_string(&SchemaVersion::new(7)).expect("应能序列化");
    assert_eq!(json, "7");

    let 回读: SchemaVersion = serde_json::from_str("7").expect("应能解析");
    assert_eq!(回读, SchemaVersion::new(7));
}

#[test]
fn 兼容性判定三档() {
    let 本地 = SchemaVersion::new(3);

    assert_eq!(
        SchemaVersion::new(3).compatibility(本地),
        Compatibility::Same
    );
    assert_eq!(
        SchemaVersion::new(2).compatibility(本地),
        Compatibility::Older
    );
    assert_eq!(
        SchemaVersion::new(4).compatibility(本地),
        Compatibility::Newer
    );
}

#[test]
fn 只有旧记录需要迁移() {
    // A newer record needs no migration: fields only grow and unknown ones are retained, which
    // already makes a downgrade read safe.
    assert!(Compatibility::Older.needs_migration());
    assert!(!Compatibility::Same.needs_migration());
    assert!(
        !Compatibility::Newer.needs_migration(),
        "对更新的记录做迁移是没有意义的——迁移链只往前走"
    );
}

#[test]
fn 版本更新的记录仍然能读() {
    // This is the policy itself: failing would mean "an older client cannot open the session".
    let 记录: 旧版记录 = serde_json::from_str(新版写出的).expect("更新的 schema 版本不该阻止读取");
    assert_eq!(记录.schema_version, SchemaVersion::new(2));
}

#[test]
fn 兼容性标签稳定() {
    assert_eq!(Compatibility::Same.label(), "same");
    assert_eq!(Compatibility::Older.label(), "older");
    assert_eq!(Compatibility::Newer.label(), "newer");
    for c in [
        Compatibility::Same,
        Compatibility::Older,
        Compatibility::Newer,
    ] {
        assert_eq!(c.to_string(), c.label());
    }
}

#[test]
fn 版本_display_带_v_前缀() {
    assert_eq!(SchemaVersion::new(1).to_string(), "v1");
}
