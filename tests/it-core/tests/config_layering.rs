//! `ra-core`: behavioral assertions for configuration layering and provenance (R0-5).
//!
//! What is locked down here is the **contract**, not implementation detail:
//! - the precedence order: builtin < user < project < local < environment < explicit
//! - nothing external is read by default — a library embedded in someone else's process has no
//!   business reading their home directory
//! - under isolation, on-disk configuration is **entirely inert** yet **still visible** in
//!   diagnostics
//! - "shadowed" and "not enabled" are two different kinds of inert with different fixes, and must
//!   not collapse into one

use ra_core::config::{
    CONFIG_DIR_NAME, CONFIG_FILE_NAME, ENV_PREFIX, FieldReport, LOCAL_CONFIG_FILE_NAME,
    LayerStatus, Layered, SettingSource, SourceSelection, env_key,
};

/// The effective value. `Layered::value` hands back `&T` directly, which reads better than
/// peeling two layers of reference off `resolve`.
fn effective_value(layered: &Layered<&'static str>, selection: SourceSelection) -> Option<&'static str> {
    layered.value(selection).copied()
}

/// Which layer the effective value came from.
fn effective_source(layered: &Layered<&'static str>, selection: SourceSelection) -> Option<SettingSource> {
    layered.resolve(selection).map(|s| s.source())
}

/// A configuration item with a value from each of the six layers.
fn all_six_layers() -> Layered<&'static str> {
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
// precedence
// ---------------------------------------------------------------------------

#[test]
fn test_config_layering_01() {
    // The closer to this particular call, the more say it gets.
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
fn test_config_layering_02() {
    let mut 优先级: Vec<u16> = SettingSource::ALL.iter().map(|s| s.precedence()).collect();
    优先级.sort_unstable();
    let total = 优先级.len();
    优先级.dedup();
    assert_eq!(优先级.len(), total, "两层同优先级会让解析结果不确定");
}

#[test]
fn test_config_layering_03() {
    let layered = all_six_layers();

    assert_eq!(effective_value(&layered, SourceSelection::all()), Some("显式"));
    assert_eq!(
        effective_source(&layered, SourceSelection::all()),
        Some(SettingSource::Explicit)
    );
}

#[test]
fn test_config_layering_04() {
    let mut layered = Layered::builtin("内置");
    layered.set(SettingSource::ProjectFile, "项目");

    assert_eq!(effective_value(&layered, SourceSelection::all()), Some("项目"));
    assert_eq!(
        effective_source(&layered, SourceSelection::all()),
        Some(SettingSource::ProjectFile)
    );
}

#[test]
fn test_config_layering_05() {
    // One file per layer, so a repeat can only mean the same layer was parsed twice.
    let mut layered: Layered<&str> = Layered::new();
    layered.set(SettingSource::Env, "先");
    layered.set(SettingSource::Env, "后");

    assert_eq!(layered.get(SettingSource::Env), Some(&"后"));
    assert_eq!(layered.sources().count(), 1, "同层不该留下两条记录");
}

#[test]
fn test_config_layering_06() {
    let layered: Layered<&str> = Layered::new();
    assert!(layered.is_empty());
    assert!(layered.resolve(SourceSelection::all()).is_none());
    assert!(layered.value(SourceSelection::all()).is_none());
}

// ---------------------------------------------------------------------------
// source selection and isolation
// ---------------------------------------------------------------------------

#[test]
fn test_config_layering_07() {
    // When the framework is embedded in someone else's process, quietly reading
    // ~/.rusty-agent/config.toml is unacceptable.
    let 默认 = SourceSelection::default();
    assert!(默认.is_isolated(), "默认必须是隔离的");
    assert_eq!(默认, SourceSelection::isolated());

    for source in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
        assert!(!默认.allows(*source), "默认不该启用 `{source}`");
    }
}

#[test]
fn test_config_layering_08() {
    // Mirrors strict_mcp_config: use only what was passed explicitly, ignore what was discovered.
    let layered = all_six_layers();
    assert_eq!(
        effective_value(&layered, SourceSelection::isolated()),
        Some("显式"),
        "显式传入不受隔离影响"
    );

    let mut 无显式 = Layered::builtin("内置");
    无显式
        .set(SettingSource::ProjectFile, "项目")
        .set(SettingSource::Env, "环境");
    assert_eq!(
        effective_value(&无显式, SourceSelection::isolated()),
        Some("内置"),
        "隔离模式下应退回内置默认值"
    );
}

#[test]
fn test_config_layering_09() {
    // Disabling builtin would leave no fallback and disabling explicit would ignore the intent of
    // this very call; both are absurd states.
    let 选择 = SourceSelection::all()
        .without(SettingSource::Builtin)
        .without(SettingSource::Explicit);

    assert!(选择.allows(SettingSource::Builtin));
    assert!(选择.allows(SettingSource::Explicit));
}

#[test]
fn test_config_layering_10() {
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
fn test_config_layering_11() {
    let 选择 = SourceSelection::isolated().with(SettingSource::Env);
    let layered = all_six_layers();

    let mut 无显式 = Layered::builtin("内置");
    无显式
        .set(SettingSource::ProjectFile, "项目")
        .set(SettingSource::Env, "环境");

    assert_eq!(effective_value(&无显式, 选择), Some("环境"));
    assert_eq!(
        effective_value(&layered, 选择),
        Some("显式"),
        "显式传入仍然压过环境变量"
    );
}

#[test]
fn test_config_layering_12() {
    for source in SettingSource::ALL {
        // discovered if and only if source selection can turn it off
        assert_eq!(
            source.is_discovered(),
            !SourceSelection::isolated().allows(*source),
            "`{source}` 的 is_discovered 与隔离行为不一致"
        );
        // a file layer is always discovered
        if source.is_file() {
            assert!(source.is_discovered(), "`{source}` 是文件却不算发现来的");
        }
    }
}

#[test]
fn test_config_layering_13() {
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
fn test_config_layering_14() {
    // This type shows up almost exclusively in diagnostics, where a derived `discovered: 5` helps
    // nobody.
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
// diagnostics
// ---------------------------------------------------------------------------

#[test]
fn test_config_layering_15() {
    let report = all_six_layers().report("model.name", SourceSelection::all());

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
fn test_config_layering_16() {
    // This is half the reason the module exists: the two kinds of inert have different fixes.
    // Shadowed -> change a higher layer's configuration; not enabled -> change source selection.
    let 选择 = SourceSelection::all().without(SettingSource::UserFile);
    let report = all_six_layers().report("model.name", 选择);

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
fn test_config_layering_17() {
    // Showing only the effective value strands the user on "I clearly wrote that, why is it
    // ignored".
    let report = all_six_layers().report("model.name", SourceSelection::isolated());

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
fn test_config_layering_18() {
    let report = all_six_layers().report("model.name", SourceSelection::all());
    let 生效 = report.effective().expect("应有生效层");

    assert_eq!(生效.source(), SettingSource::Explicit);
    assert_eq!(生效.value(), "显式");
    assert!(report.has_ineffective_layers(), "另外五层都没生效");
}

#[test]
fn test_config_layering_19() {
    let report = Layered::builtin("内置").report("model.name", SourceSelection::all());

    assert!(!report.has_ineffective_layers());
    assert_eq!(
        report.effective().map(|l| l.source()),
        Some(SettingSource::Builtin)
    );
}

#[test]
fn test_config_layering_20() {
    let mut layered: Layered<&str> = Layered::new();
    layered.set(SettingSource::Env, "环境");

    let report = layered.report("model.name", SourceSelection::isolated());
    assert!(report.effective().is_none(), "唯一给值的层被排除了");
    assert!(report.to_string().contains("未设置"), "{report}");
}

#[test]
fn test_config_layering_21() {
    let report = all_six_layers().report("model.name", SourceSelection::all());
    assert_eq!(report.to_string(), "model.name = 显式 (explicit)");
}

#[test]
fn test_config_layering_22() {
    // doctor has to line up items of different types in one table.
    let 数字: FieldReport = Layered::builtin(42_u32).report("turn.max", SourceSelection::all());
    assert_eq!(数字.effective().expect("应有生效层").value(), "42");

    let 布尔: FieldReport =
        Layered::builtin(true).report("sandbox.enabled", SourceSelection::all());
    assert_eq!(布尔.effective().expect("应有生效层").value(), "true");
}

// ---------------------------------------------------------------------------
// path and environment-variable conventions
// ---------------------------------------------------------------------------

#[test]
fn test_config_layering_23() {
    assert_eq!(env_key("model.name"), "RA_MODEL_NAME");
    assert_eq!(env_key("sandbox.network.allow"), "RA_SANDBOX_NETWORK_ALLOW");
    assert_eq!(env_key("verbose"), "RA_VERBOSE");
}

#[test]
fn test_config_layering_24() {
    for path in ["model.name", "a", "x.y.z"] {
        let key = env_key(path);
        assert!(key.starts_with(ENV_PREFIX), "`{key}` 缺前缀");
        assert_eq!(key, key.to_uppercase(), "`{key}` 应全大写");
        assert!(!key.contains('.'), "`{key}` 不该残留点");
    }
}

#[test]
fn test_config_layering_25() {
    assert_eq!(CONFIG_DIR_NAME, ".rusty-agent");
    assert_eq!(CONFIG_FILE_NAME, "config.toml");
    assert_eq!(LOCAL_CONFIG_FILE_NAME, "config.local.toml");
    assert_ne!(
        CONFIG_FILE_NAME, LOCAL_CONFIG_FILE_NAME,
        "个人覆盖文件必须与团队配置分开，否则没法只 gitignore 前者"
    );
}
