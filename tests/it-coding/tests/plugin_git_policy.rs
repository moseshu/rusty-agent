// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Ambient Git policy, trusted repository ownership, and canonical staging boundaries.

use ra_coding::{
    permissions::AbsolutePathBufGuard,
    plugins::materialize::test_api::{
        PluginGitMode, configure_trusted_repository, git_command, repository_local_git_variables,
    },
};
use std::{ffi::OsStr, fs, path::Path};

#[test]
fn automatic_git_removes_only_the_upstream_repository_variables() {
    let command = git_command(PluginGitMode::Automatic, Path::new("git"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-c"), OsStr::new("safe.bareRepository=explicit")]
    );
    for name in repository_local_git_variables() {
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| value),
            Some(None),
            "{name}"
        );
    }
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("GIT_OPTIONAL_LOCKS"))
            .map(|(_, value)| value),
        Some(Some(OsStr::new("0")))
    );
    for name in [
        "HOME",
        "PATH",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_SSH_COMMAND",
    ] {
        assert!(command.get_envs().all(|(key, _)| key != OsStr::new(name)));
    }
}

#[test]
fn manual_git_keeps_the_available_environment_and_safe_bare_setting() {
    let command = git_command(PluginGitMode::Manual, Path::new("git"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-c"), OsStr::new("safe.bareRepository=explicit")]
    );
    assert_eq!(command.get_envs().count(), 0);
}

#[test]
fn trusted_repository_guard_owns_the_explicit_bare_repository() {
    let home = tempfile::tempdir().unwrap();
    let mut command = git_command(PluginGitMode::Automatic, Path::new("git"));
    let guard = configure_trusted_repository(&mut command, home.path()).unwrap();
    let path = guard.path().to_owned();
    assert!(path.starts_with(dunce::canonicalize(home.path()).unwrap().join(".tmp")));
    assert_eq!(
        fs::read_to_string(path.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    assert!(path.join("objects").is_dir());
    assert!(path.join("refs").is_dir());
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("GIT_DIR"))
            .map(|(_, value)| value),
        Some(Some(path.as_os_str()))
    );
    drop(guard);
    assert!(!path.exists());
}

#[test]
fn trusted_repository_requires_an_existing_product_home() {
    let parent = tempfile::tempdir().unwrap();
    let mut command = git_command(PluginGitMode::Automatic, Path::new("git"));
    let err =
        configure_trusted_repository(&mut command, &parent.path().join("missing")).unwrap_err();
    assert!(err.starts_with("failed to resolve trusted product home:"));
    assert!(!parent.path().join("missing").exists());
}

#[test]
fn trusted_repository_home_expansion_uses_the_existing_thread_scope() {
    let home = tempfile::tempdir().unwrap();
    AbsolutePathBufGuard::with_home_directory(home.path(), || {
        let mut command = git_command(PluginGitMode::Automatic, Path::new("git"));
        let guard = configure_trusted_repository(&mut command, Path::new("~")).unwrap();
        assert!(
            guard
                .path()
                .starts_with(dunce::canonicalize(home.path()).unwrap())
        );
    });
}

#[cfg(unix)]
#[test]
fn trusted_staging_rejects_an_external_symlink_but_allows_an_internal_one() {
    let home = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let staging = home.path().join(".tmp");
    std::os::unix::fs::symlink(external.path(), &staging).unwrap();
    let mut command = git_command(PluginGitMode::Automatic, Path::new("git"));
    assert_eq!(
        configure_trusted_repository(&mut command, home.path()).unwrap_err(),
        "trusted Git repository root escapes product home"
    );
    assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
    fs::remove_file(&staging).unwrap();
    let internal = home.path().join("owned");
    fs::create_dir(&internal).unwrap();
    std::os::unix::fs::symlink(&internal, &staging).unwrap();
    let guard = configure_trusted_repository(&mut command, home.path()).unwrap();
    assert!(
        guard
            .path()
            .starts_with(dunce::canonicalize(internal).unwrap())
    );
}
