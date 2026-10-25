// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Both upstream Git materialization fixtures against real local repositories.

use ra_coding::plugins::{
    marketplace::MarketplacePluginSource,
    materialize::{
        materialize_marketplace_plugin_source,
        test_api::{PluginGitMode, run_git, run_git_output},
    },
};
use std::{fs, path::Path};

#[test]
fn materialize_git_subdir_uses_sparse_checkout() {
    let run_git = |args: &[&str], cwd| run_git(args, cwd, PluginGitMode::Manual);
    let run_git_output = |args: &[&str], cwd| run_git_output(args, cwd, PluginGitMode::Manual);
    let codex_home = tempfile::tempdir().expect("create codex home");
    let repo = tempfile::tempdir().expect("create git repo");
    let plugin_dir = repo.path().join("plugins/toolkit");
    fs::create_dir_all(&plugin_dir).expect("create plugin directory");
    fs::create_dir_all(repo.path().join("plugins/other")).expect("create other plugin");
    fs::write(plugin_dir.join("marker.txt"), "toolkit").expect("write plugin marker");
    fs::write(repo.path().join("plugins/other/marker.txt"), "other").expect("write other marker");
    fs::write(repo.path().join("root.txt"), "root").expect("write root marker");

    run_git(&["init"], Some(repo.path())).expect("init git repo");
    run_git(
        &["config", "user.email", "test@example.com"],
        Some(repo.path()),
    )
    .expect("configure git email");
    run_git(&["config", "user.name", "Test User"], Some(repo.path())).expect("configure git name");
    run_git(&["config", "commit.gpgsign", "false"], Some(repo.path()))
        .expect("disable fixture commit signing");
    run_git(&["add", "."], Some(repo.path())).expect("stage git repo");
    run_git(&["commit", "-m", "init"], Some(repo.path())).expect("commit git repo");
    let sha = run_git_output(&["rev-parse", "HEAD"], Some(repo.path())).expect("resolve commit");

    let materialized = materialize_marketplace_plugin_source(
        codex_home.path(),
        &MarketplacePluginSource::Git {
            url: repo.path().display().to_string(),
            path: Some("plugins/toolkit".to_string()),
            ref_name: None,
            sha: Some(sha),
        },
    )
    .expect("materialize git source");

    assert_eq!(
        plugin_dir.file_name(),
        materialized.path.as_path().file_name()
    );
    assert!(materialized.path.as_path().join("marker.txt").is_file());
    let checkout_root = materialized
        .path
        .as_path()
        .parent()
        .and_then(Path::parent)
        .expect("materialized path should be nested under checkout root");
    assert!(!checkout_root.join("root.txt").exists());
    assert!(!checkout_root.join("plugins/other/marker.txt").exists());
}

#[test]
fn materialize_git_source_rejects_sha_that_resolves_to_hostile_default_branch() {
    let run_git = |args: &[&str], cwd| run_git(args, cwd, PluginGitMode::Manual);
    let run_git_output = |args: &[&str], cwd| run_git_output(args, cwd, PluginGitMode::Manual);
    let codex_home = tempfile::tempdir().expect("create codex home");
    let repo = tempfile::tempdir().expect("create git repo");
    run_git(&["init"], Some(repo.path())).expect("init git repo");
    run_git(
        &["config", "user.email", "test@example.com"],
        Some(repo.path()),
    )
    .expect("configure git email");
    run_git(&["config", "user.name", "Test User"], Some(repo.path())).expect("configure git name");
    run_git(&["config", "commit.gpgsign", "false"], Some(repo.path()))
        .expect("disable fixture commit signing");

    fs::write(repo.path().join("marker.txt"), "benign").expect("write benign marker");
    run_git(&["add", "."], Some(repo.path())).expect("stage git repo");
    run_git(&["commit", "-m", "benign"], Some(repo.path())).expect("commit benign revision");
    let benign_sha =
        run_git_output(&["rev-parse", "HEAD"], Some(repo.path())).expect("resolve commit A");

    fs::write(repo.path().join("marker.txt"), "malicious").expect("write malicious marker");
    run_git(&["add", "."], Some(repo.path())).expect("stage malicious revision");
    run_git(&["commit", "-m", "malicious"], Some(repo.path())).expect("commit malicious revision");
    let malicious_sha =
        run_git_output(&["rev-parse", "HEAD"], Some(repo.path())).expect("resolve commit B");
    run_git(&["branch", "-m", &benign_sha], Some(repo.path()))
        .expect("name default branch after commit A");

    let err = materialize_marketplace_plugin_source(
        codex_home.path(),
        &MarketplacePluginSource::Git {
            url: repo.path().display().to_string(),
            path: None,
            ref_name: None,
            sha: Some(benign_sha.clone()),
        },
    )
    .expect_err("hostile default branch must not satisfy SHA pinning");

    assert_eq!(
        err,
        format!("checked out Git SHA {malicious_sha} does not match requested SHA {benign_sha}")
    );
}

fn git_fixture() -> (tempfile::TempDir, String, String) {
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| run_git(args, Some(repo.path()), PluginGitMode::Manual).unwrap();
    git(&["init"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.com"]);
    git(&["config", "commit.gpgsign", "false"]);
    fs::create_dir(repo.path().join(".codex-plugin")).unwrap();
    fs::write(
        repo.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"demo","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(repo.path().join("marker"), b"first").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "first"]);
    let first = run_git_output(
        &["rev-parse", "HEAD"],
        Some(repo.path()),
        PluginGitMode::Manual,
    )
    .unwrap();
    git(&["branch", "release"]);
    fs::write(repo.path().join("marker"), b"second").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "second"]);
    let second = run_git_output(
        &["rev-parse", "HEAD"],
        Some(repo.path()),
        PluginGitMode::Manual,
    )
    .unwrap();
    (repo, first, second)
}

#[test]
fn local_materialization_keeps_the_original_path_without_staging_or_existence_checks() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("not-created");
    let path =
        ra_coding::plugins::AbsolutePathBuf::try_from(root.path().join("missing-plugin")).unwrap();
    let materialized = materialize_marketplace_plugin_source(
        &home,
        &MarketplacePluginSource::Local { path: path.clone() },
    )
    .unwrap();
    assert_eq!(materialized.path, path);
    assert!(!home.exists());
    drop(materialized);
    assert!(root.path().exists());
}

#[test]
fn git_sha_precedes_ref_and_both_git_modes_own_their_checkouts() {
    let (repo, first, second) = git_fixture();
    let home = tempfile::tempdir().unwrap();
    for (mode, reference, sha, expected_sha) in [
        (
            PluginGitMode::Manual,
            Some("release".to_string()),
            None,
            first.clone(),
        ),
        (
            PluginGitMode::Manual,
            Some("missing-ref".to_string()),
            Some(first.clone()),
            first,
        ),
        (PluginGitMode::Automatic, None, None, second),
    ] {
        let materialized = ra_coding::plugins::materialize::test_api::materialize_with_mode(
            home.path(),
            &MarketplacePluginSource::Git {
                url: repo.path().display().to_string(),
                path: None,
                ref_name: reference,
                sha,
            },
            mode,
        )
        .unwrap();
        let path = materialized.path.to_path_buf();
        assert_eq!(
            run_git_output(&["rev-parse", "HEAD"], Some(&path), mode).unwrap(),
            expected_sha
        );
        assert!(path.join("marker").is_file());
        drop(materialized);
        assert!(!path.exists());
    }
    assert!(repo.path().join("marker").exists());
}

#[test]
fn git_checkout_failure_cleans_staging_and_preserves_source_diagnostics() {
    let (repo, _, _) = git_fixture();
    let home = tempfile::tempdir().unwrap();
    let err = materialize_marketplace_plugin_source(
        home.path(),
        &MarketplacePluginSource::Git {
            url: repo.path().display().to_string(),
            path: None,
            ref_name: Some("missing-ref".into()),
            sha: None,
        },
    )
    .unwrap_err();
    assert!(err.starts_with("git checkout missing-ref failed with status"));
    assert!(err.contains("\nstdout:\n"));
    assert!(err.contains("\nstderr:\n"));
    assert_eq!(
        fs::read_dir(
            home.path()
                .join("plugins/.marketplace-plugin-source-staging")
        )
        .unwrap()
        .count(),
        0
    );
    assert!(repo.path().join("marker").exists());
}

#[test]
fn a_materialized_checkout_can_be_installed_before_its_owner_is_dropped() {
    use ra_coding::plugins::{PluginId, store::PluginStore};
    let (repo, first, _) = git_fixture();
    let home = tempfile::tempdir().unwrap();
    let materialized = materialize_marketplace_plugin_source(
        home.path(),
        &MarketplacePluginSource::Git {
            url: repo.path().display().to_string(),
            path: None,
            ref_name: None,
            sha: Some(first),
        },
    )
    .unwrap();
    let temporary = materialized.path.to_path_buf();
    let store = PluginStore::new(home.path().to_path_buf());
    let id = PluginId::new("demo".into(), "fixture".into()).unwrap();
    store
        .install(materialized.path.clone(), id.clone())
        .unwrap();
    let installed = store.active_plugin_root(&id).unwrap();
    drop(materialized);
    assert!(!temporary.exists());
    assert_eq!(
        fs::read(installed.as_path().join("marker")).unwrap(),
        b"first"
    );
    assert_eq!(store.active_plugin_version(&id).as_deref(), Some("1.0.0"));
}

#[test]
fn automatic_clone_sanitizes_inherited_config_and_manual_cwd_discards_inherited_git_dir() {
    let (desired, _, _) = git_fixture();
    let (ambient, _, _) = git_fixture();
    let home = tempfile::tempdir().unwrap();
    let empty_global = home.path().join("empty-gitconfig");
    fs::write(&empty_global, "").unwrap();
    for case in ["automatic", "manual-cwd"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "inherited_git_environment_child",
                "--nocapture",
            ])
            .env("RA_PLUGIN_GIT_CHILD", case)
            .env("RA_PLUGIN_GIT_HOME", home.path())
            .env("RA_PLUGIN_GIT_SOURCE", desired.path())
            .env("GIT_DIR", ambient.path().join(".git"))
            .env("GIT_CONFIG_GLOBAL", &empty_global)
            .env("GIT_CONFIG_SYSTEM", &empty_global)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "clone.defaultRemoteName")
            .env("GIT_CONFIG_VALUE_0", "injected")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains(&format!("completed materialization fixture: {case}"))
        );
    }
}

#[cfg(unix)]
#[test]
fn npm_materializer_keeps_its_owner_through_store_installation() {
    use flate2::{Compression, write::GzEncoder};
    use std::{io::Cursor, os::unix::fs::PermissionsExt};
    let fixture = tempfile::tempdir().unwrap();
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (name, bytes) in [
        (
            "package/package.json",
            br#"{"name":"demo","version":"1.0.0"}"#.as_slice(),
        ),
        (
            "package/.codex-plugin/plugin.json",
            br#"{"name":"demo","version":"1.0.0"}"#.as_slice(),
        ),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, name, Cursor::new(bytes))
            .unwrap();
    }
    let bundle = fixture.path().join("fixture.tgz");
    fs::write(&bundle, archive.into_inner().unwrap().finish().unwrap()).unwrap();
    let bin = fixture.path().join("npm");
    let quoted = format!("'{}'", bundle.to_str().unwrap().replace('\'', "'\\''"));
    fs::write(
        &bin,
        format!("#!/bin/sh\ncp {quoted} package.tgz\nprintf '%s\\n' \"$@\" > args.txt\n"),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![fixture.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "inherited_git_environment_child",
            "--nocapture",
        ])
        .env("RA_PLUGIN_GIT_CHILD", "npm")
        .env("RA_PLUGIN_GIT_SOURCE", fixture.path())
        .env("RA_PLUGIN_GIT_HOME", fixture.path())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("completed materialization fixture: npm")
    );
}

#[test]
#[ignore = "runs only in the isolated environment subprocess launched by the parent test"]
fn inherited_git_environment_child() {
    let Some(case) = std::env::var_os("RA_PLUGIN_GIT_CHILD") else {
        return;
    };
    let case = case
        .to_str()
        .expect("child fixture mode must be valid UTF-8");
    let source = std::path::PathBuf::from(std::env::var_os("RA_PLUGIN_GIT_SOURCE").unwrap());
    match case {
        "manual-cwd" => {
            let actual = run_git_output(
                &["rev-parse", "--show-toplevel"],
                Some(&source),
                PluginGitMode::Manual,
            )
            .unwrap();
            assert_eq!(
                dunce::canonicalize(actual).unwrap(),
                dunce::canonicalize(source).unwrap()
            );
        }
        "automatic" => {
            let home = std::path::PathBuf::from(std::env::var_os("RA_PLUGIN_GIT_HOME").unwrap());
            let materialized = ra_coding::plugins::materialize::test_api::materialize_with_mode(
                &home,
                &MarketplacePluginSource::Git {
                    url: source.display().to_string(),
                    path: None,
                    ref_name: None,
                    sha: None,
                },
                PluginGitMode::Automatic,
            )
            .unwrap();
            let remote = run_git_output(
                &["config", "--get", "remote.origin.url"],
                Some(materialized.path.as_path()),
                PluginGitMode::Automatic,
            )
            .unwrap();
            assert_eq!(remote, source.display().to_string());
            assert!(materialized.path.as_path().join("marker").is_file());
        }
        "npm" => {
            use ra_coding::plugins::{PluginId, store::PluginStore};
            let home = std::path::PathBuf::from(std::env::var_os("RA_PLUGIN_GIT_HOME").unwrap());
            let materialized = materialize_marketplace_plugin_source(
                &home,
                &MarketplacePluginSource::Npm {
                    package: "demo".into(),
                    version: Some("^1.0.0".into()),
                    registry: Some("https://offline.invalid".into()),
                },
            )
            .unwrap();
            let temporary = materialized
                .path
                .as_path()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned();
            assert_eq!(
                fs::read_to_string(temporary.join("args.txt"))
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                [
                    "pack",
                    "--ignore-scripts",
                    "--pack-destination",
                    temporary.to_str().unwrap(),
                    "--registry",
                    "https://offline.invalid",
                    "--",
                    "demo@^1.0.0"
                ]
            );
            let store = PluginStore::new(home);
            let id = PluginId::new("demo".into(), "npm-fixture".into()).unwrap();
            store
                .install(materialized.path.clone(), id.clone())
                .unwrap();
            drop(materialized);
            assert!(!temporary.exists());
            assert!(
                store
                    .active_plugin_root(&id)
                    .unwrap()
                    .as_path()
                    .join("package.json")
                    .is_file()
            );
        }
        other => panic!("unknown fixture case: {other}"),
    }
    println!("completed materialization fixture: {case}");
}
