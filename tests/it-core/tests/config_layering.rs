//! `ra-core`：配置分层与来源追踪（R0-5）的行为断言。
//!
//! 这里锁住的是**契约**，不是实现细节：
//! - 优先级顺序：内置 < 用户 < 项目 < 个人 < 环境 < 显式传入
//! - 默认不读任何外部配置——库被嵌进别人的进程时不该偷读用户主目录
//! - 隔离开关下磁盘配置**完全不生效**，但在诊断里**仍然看得见**
//! - 「被盖了」与「没启用」是两种不同的没生效，修法不同，不能混成一种

use ra_core::config::{
    CONFIG_DIR_NAME, CONFIG_FILE_NAME, ENV_PREFIX, FieldReport, LOCAL_CONFIG_FILE_NAME,
    LayerStatus, Layered, SettingSource, SourceSelection, env_key,
};

/// 生效值。`Layered::value` 直接给 `&T`，比从 `resolve` 里剥两层引用可读。
fn 生效值(layered: &Layered<&'static str>, 选择: SourceSelection) -> Option<&'static str> {
    layered.value(选择).copied()
}

/// 生效值来自哪一层。
fn 生效来源(layered: &Layered<&'static str>, 选择: SourceSelection) -> Option<SettingSource> {
    layered.resolve(选择).map(|s| s.source())
}

/// 六层各给一个值的配置项。
fn 六层齐全() -> Layered<&'static str> {
    let mut layered = Layered::builtin("内置");
    layered
        .set(SettingSource::UserFile, "用户")
        .set(SettingSource::ProjectFile, "项目")
        .set(SettingSource::LocalFile, "个人")
        .set(SettingSource::Env, "环境")
        .set(SettingSource::Explicit, "显式");
    layered
}

// ---------------------------------------------------------------------------
// 优先级
// ---------------------------------------------------------------------------

#[test]
fn 优先级顺序是契约() {
    // 越贴近这一次调用的，越有权决定。
    let 期望 = [
        SettingSource::Builtin,
        SettingSource::UserFile,
        SettingSource::ProjectFile,
        SettingSource::LocalFile,
        SettingSource::Env,
        SettingSource::Explicit,
    ];

    for pair in 期望.windows(2) {
        assert!(
            pair[0].precedence() < pair[1].precedence(),
            "`{}` 应当低于 `{}`",
            pair[0],
            pair[1]
        );
    }

    assert_eq!(SettingSource::ALL, 期望, "ALL 应按优先级从低到高排列");
}

#[test]
fn 优先级两两不同() {
    let mut 优先级: Vec<u16> = SettingSource::ALL.iter().map(|s| s.precedence()).collect();
    优先级.sort_unstable();
    let total = 优先级.len();
    优先级.dedup();
    assert_eq!(优先级.len(), total, "两层同优先级会让解析结果不确定");
}

#[test]
fn 最高层胜出() {
    let layered = 六层齐全();

    assert_eq!(生效值(&layered, SourceSelection::all()), Some("显式"));
    assert_eq!(
        生效来源(&layered, SourceSelection::all()),
        Some(SettingSource::Explicit)
    );
}

#[test]
fn 缺层不影响解析() {
    let mut layered = Layered::builtin("内置");
    layered.set(SettingSource::ProjectFile, "项目");

    assert_eq!(生效值(&layered, SourceSelection::all()), Some("项目"));
    assert_eq!(
        生效来源(&layered, SourceSelection::all()),
        Some(SettingSource::ProjectFile)
    );
}

#[test]
fn 同层重复写入是覆盖不是追加() {
    // 一个文件对一层，重复只可能来自解析同一层两次。
    let mut layered: Layered<&str> = Layered::new();
    layered.set(SettingSource::Env, "先");
    layered.set(SettingSource::Env, "后");

    assert_eq!(layered.get(SettingSource::Env), Some(&"后"));
    assert_eq!(layered.sources().count(), 1, "同层不该留下两条记录");
}

#[test]
fn 无人给值时没有生效值() {
    let layered: Layered<&str> = Layered::new();
    assert!(layered.is_empty());
    assert!(layered.resolve(SourceSelection::all()).is_none());
    assert!(layered.value(SourceSelection::all()).is_none());
}

// ---------------------------------------------------------------------------
// 来源选择与隔离
// ---------------------------------------------------------------------------

#[test]
fn 默认不读任何外部配置() {
    // 框架被嵌进别人的进程时，偷读 ~/.rusty-agent/config.toml 是不可接受的。
    let 默认 = SourceSelection::default();
    assert!(默认.is_isolated(), "默认必须是隔离的");
    assert_eq!(默认, SourceSelection::isolated());

    for source in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
        assert!(!默认.allows(*source), "默认不该启用 `{source}`");
    }
}

#[test]
fn 隔离模式下磁盘与环境完全不生效() {
    // 对齐 strict_mcp_config：只用显式传入的，忽略发现来的。
    let layered = 六层齐全();
    assert_eq!(
        生效值(&layered, SourceSelection::isolated()),
        Some("显式"),
        "显式传入不受隔离影响"
    );

    let mut 无显式 = Layered::builtin("内置");
    无显式
        .set(SettingSource::ProjectFile, "项目")
        .set(SettingSource::Env, "环境");
    assert_eq!(
        生效值(&无显式, SourceSelection::isolated()),
        Some("内置"),
        "隔离模式下应退回内置默认值"
    );
}

#[test]
fn 内置与显式两档关不掉() {
    // 关掉内置就没有兜底值，关掉显式就等于无视本次调用的意图——两者都是荒谬状态。
    let 选择 = SourceSelection::all()
        .without(SettingSource::Builtin)
        .without(SettingSource::Explicit);

    assert!(选择.allows(SettingSource::Builtin));
    assert!(选择.allows(SettingSource::Explicit));
}

#[test]
fn 逐个来源可开可关() {
    for source in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
        let 只开这个 = SourceSelection::isolated().with(*source);
        assert!(只开这个.allows(*source), "`{source}` 打开失败");
        assert!(!只开这个.is_isolated());

        for 其它 in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
            if 其它 != source {
                assert!(
                    !只开这个.allows(*其它),
                    "打开 `{source}` 不该连带打开 `{其它}`"
                );
            }
        }

        let 关掉这个 = SourceSelection::all().without(*source);
        assert!(!关掉这个.allows(*source), "`{source}` 关闭失败");
    }
}

#[test]
fn ci_场景只认环境变量() {
    let 选择 = SourceSelection::isolated().with(SettingSource::Env);
    let layered = 六层齐全();

    let mut 无显式 = Layered::builtin("内置");
    无显式
        .set(SettingSource::ProjectFile, "项目")
        .set(SettingSource::Env, "环境");

    assert_eq!(生效值(&无显式, 选择), Some("环境"));
    assert_eq!(
        生效值(&layered, 选择),
        Some("显式"),
        "显式传入仍然压过环境变量"
    );
}

#[test]
fn 来源分类自洽() {
    for source in SettingSource::ALL {
        // 发现来的 ⟺ 可被来源选择关掉
        assert_eq!(
            source.is_discovered(),
            !SourceSelection::isolated().allows(*source),
            "`{source}` 的 is_discovered 与隔离行为不一致"
        );
        // 文件层一定是发现来的
        if source.is_file() {
            assert!(source.is_discovered(), "`{source}` 是文件却不算发现来的");
        }
    }
}

#[test]
fn 来源标签唯一且稳定() {
    let mut labels: Vec<&str> = SettingSource::ALL.iter().map(|s| s.label()).collect();
    labels.sort_unstable();
    let total = labels.len();
    labels.dedup();
    assert_eq!(labels.len(), total, "来源标签重复，doctor 输出会有歧义");

    assert_eq!(SettingSource::Builtin.label(), "builtin");
    assert_eq!(SettingSource::UserFile.label(), "user");
    assert_eq!(SettingSource::ProjectFile.label(), "project");
    assert_eq!(SettingSource::LocalFile.label(), "local");
    assert_eq!(SettingSource::Env.label(), "env");
    assert_eq!(SettingSource::Explicit.label(), "explicit");
}

#[test]
fn 来源选择的_debug_可读() {
    // 这个类型出现的场合几乎都是诊断，派生的 `discovered: 5` 帮不上忙。
    assert_eq!(
        format!("{:?}", SourceSelection::isolated()),
        "SourceSelection(isolated)"
    );
    assert_eq!(
        format!("{:?}", SourceSelection::all()),
        "SourceSelection(user+project+local+env)"
    );
    assert_eq!(
        format!("{:?}", SourceSelection::isolated().with(SettingSource::Env)),
        "SourceSelection(env)"
    );
}

// ---------------------------------------------------------------------------
// 诊断
// ---------------------------------------------------------------------------

#[test]
fn 报告按优先级从高到低列出所有层() {
    let report = 六层齐全().report("model.name", SourceSelection::all());

    let 顺序: Vec<SettingSource> = report.layers().iter().map(|l| l.source()).collect();
    assert_eq!(
        顺序,
        vec![
            SettingSource::Explicit,
            SettingSource::Env,
            SettingSource::LocalFile,
            SettingSource::ProjectFile,
            SettingSource::UserFile,
            SettingSource::Builtin,
        ]
    );
    assert_eq!(report.key(), "model.name");
}

#[test]
fn 报告区分被盖住与未启用() {
    // 这是本模块存在的一半理由：两种「没生效」的修法完全不同。
    // 被盖住 → 改更高层的配置；未启用 → 改来源选择。
    let 选择 = SourceSelection::all().without(SettingSource::UserFile);
    let report = 六层齐全().report("model.name", 选择);

    let 状态 = |source: SettingSource| {
        report
            .layers()
            .iter()
            .find(|l| l.source() == source)
            .map(|l| l.status())
    };

    assert_eq!(状态(SettingSource::Explicit), Some(LayerStatus::Effective));
    assert_eq!(状态(SettingSource::Env), Some(LayerStatus::Shadowed));
    assert_eq!(状态(SettingSource::UserFile), Some(LayerStatus::Excluded));
}

#[test]
fn 未启用的层在报告里仍然看得见() {
    // 只显示生效值的话，用户会困在「我明明写了配置为什么没用」里。
    let report = 六层齐全().report("model.name", SourceSelection::isolated());

    assert_eq!(report.layers().len(), 6, "隔离模式不该让配置从报告里消失");

    for layer in report.layers() {
        if layer.source().is_discovered() {
            assert_eq!(
                layer.status(),
                LayerStatus::Excluded,
                "`{}` 应标为未启用而不是被盖住",
                layer.source()
            );
        }
    }
}

#[test]
fn 报告能指出生效值与来源() {
    let report = 六层齐全().report("model.name", SourceSelection::all());
    let 生效 = report.effective().expect("应有生效层");

    assert_eq!(生效.source(), SettingSource::Explicit);
    assert_eq!(生效.value(), "显式");
    assert!(report.has_ineffective_layers(), "另外五层都没生效");
}

#[test]
fn 只有一层时没有未生效的层() {
    let report = Layered::builtin("内置").report("model.name", SourceSelection::all());

    assert!(!report.has_ineffective_layers());
    assert_eq!(
        report.effective().map(|l| l.source()),
        Some(SettingSource::Builtin)
    );
}

#[test]
fn 全被排除时没有生效值() {
    let mut layered: Layered<&str> = Layered::new();
    layered.set(SettingSource::Env, "环境");

    let report = layered.report("model.name", SourceSelection::isolated());
    assert!(report.effective().is_none(), "唯一给值的层被排除了");
    assert!(report.to_string().contains("未设置"), "{report}");
}

#[test]
fn 报告的一行摘要带来源() {
    let report = 六层齐全().report("model.name", SourceSelection::all());
    assert_eq!(report.to_string(), "model.name = 显式 (explicit)");
}

#[test]
fn 报告值被字符串化以便同表展示() {
    // doctor 要把不同类型的配置项排在同一张表里。
    let 数字: FieldReport = Layered::builtin(42_u32).report("turn.max", SourceSelection::all());
    assert_eq!(数字.effective().expect("应有生效层").value(), "42");

    let 布尔: FieldReport =
        Layered::builtin(true).report("sandbox.enabled", SourceSelection::all());
    assert_eq!(布尔.effective().expect("应有生效层").value(), "true");
}

// ---------------------------------------------------------------------------
// 路径与环境变量约定
// ---------------------------------------------------------------------------

#[test]
fn 环境变量名映射稳定() {
    assert_eq!(env_key("model.name"), "RA_MODEL_NAME");
    assert_eq!(env_key("sandbox.network.allow"), "RA_SANDBOX_NETWORK_ALLOW");
    assert_eq!(env_key("verbose"), "RA_VERBOSE");
}

#[test]
fn 环境变量名总是带前缀且全大写() {
    for path in ["model.name", "a", "x.y.z"] {
        let key = env_key(path);
        assert!(key.starts_with(ENV_PREFIX), "`{key}` 缺前缀");
        assert_eq!(key, key.to_uppercase(), "`{key}` 应全大写");
        assert!(!key.contains('.'), "`{key}` 不该残留点");
    }
}

#[test]
fn 配置文件名约定稳定() {
    assert_eq!(CONFIG_DIR_NAME, ".rusty-agent");
    assert_eq!(CONFIG_FILE_NAME, "config.toml");
    assert_eq!(LOCAL_CONFIG_FILE_NAME, "config.local.toml");
    assert_ne!(
        CONFIG_FILE_NAME, LOCAL_CONFIG_FILE_NAME,
        "个人覆盖文件必须与团队配置分开，否则没法只 gitignore 前者"
    );
}
