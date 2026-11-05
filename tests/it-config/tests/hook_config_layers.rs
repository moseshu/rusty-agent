// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Hook-related project layer and requirement loading, ported from Codex's
//! `core/src/config/config_loader_tests.rs`. The project folder is `.rusty-agent` here.

use it_config::fixture_fs::TestFileSystem;
use ra_config::loader::*;
use ra_config::*;
use ra_paths::AbsolutePathBuf;
use std::path::Path;
use tempfile::tempdir;

const PROJECT_FOLDER: &str = ".rusty-agent";

fn trusted_project_config(codex_home: &Path, project: &Path, root_markers: Option<&[&str]>) {
    let markers = root_markers
        .map(|markers| {
            format!(
                "project_root_markers = [{}]\n",
                markers
                    .iter()
                    .map(|marker| format!("'{marker}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .unwrap_or_default();
    let key = TomlValue::String(project_trust_key(project)).to_string();
    std::fs::create_dir_all(codex_home).unwrap();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        format!("{markers}[projects.{key}]\ntrust_level = 'trusted'\n"),
    )
    .unwrap();
}

fn write_linked_worktree_pointer(repo_root: &Path, worktree_root: &Path) {
    let worktree_git_dir = repo_root.join(".git/worktrees/feature-x");
    std::fs::create_dir_all(&worktree_git_dir).unwrap();
    std::fs::write(
        worktree_root.join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    std::fs::write(
        worktree_git_dir.join("gitdir"),
        format!("{}\n", worktree_root.join(".git").display()),
    )
    .unwrap();
    std::fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
}

/// Writes a project `PreToolUse` hook; `marker` becomes the layer's `foo` value, which tells the
/// layer that keeps its own configuration apart from the folder its hooks come from.
fn write_project_hook_config(folder: &Path, marker: Option<&str>, command: &str) {
    std::fs::create_dir_all(folder).unwrap();
    let marker = marker
        .map(|value| format!("foo = \"{value}\"\n\n"))
        .unwrap_or_default();
    std::fs::write(
        folder.join(CONFIG_TOML_FILE),
        format!(
            "{marker}[hooks]\n\n[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"{command}\"\n"
        ),
    )
    .unwrap();
}

fn project_hook_command(layer: &ConfigLayerEntry) -> Option<&str> {
    layer
        .config
        .get("hooks")?
        .get("PreToolUse")?
        .as_array()?
        .first()?
        .get("hooks")?
        .as_array()?
        .first()?
        .get("command")?
        .as_str()
}

async fn load(codex_home: &Path, cwd: &Path, options: ConfigLoadOptions) -> ConfigLayerStack {
    load_config_layers_state(
        &TestFileSystem,
        codex_home,
        Some(AbsolutePathBuf::from_absolute_path(cwd).unwrap()),
        &[],
        options,
        &NoopThreadConfigLoader,
    )
    .await
    .expect("load config layers")
}

fn plain_options() -> ConfigLoadOptions {
    ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests())
}

fn project_layers(stack: &ConfigLayerStack) -> Vec<&ConfigLayerEntry> {
    stack
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect()
}

fn folder(path: &Path) -> Option<AbsolutePathBuf> {
    Some(AbsolutePathBuf::from_absolute_path(path.join(PROJECT_FOLDER)).unwrap())
}

#[tokio::test]
async fn top_level_allow_managed_hooks_only_in_user_config_does_not_enable_requirements_policy() {
    let tmp = tempdir().unwrap();
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        "allow_managed_hooks_only = true",
    )
    .unwrap();
    let layers = load(tmp.path(), tmp.path(), plain_options()).await;
    assert_eq!(layers.requirements_toml().allow_managed_hooks_only, None);
    assert!(layers.requirements().allow_managed_hooks_only.is_none());
}

#[tokio::test]
async fn hooks_allow_managed_hooks_only_in_user_config_does_not_enable_requirements_policy() {
    let tmp = tempdir().unwrap();
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        "[hooks]\nallow_managed_hooks_only = true\n\n[[hooks.PreToolUse]]\nmatcher = \"^Bash$\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"python3 /tmp/user-hook.py\"\n",
    )
    .unwrap();
    let layers = load(tmp.path(), tmp.path(), plain_options()).await;
    assert!(
        layers
            .get_active_user_layer()
            .and_then(|layer| layer.config.get("hooks"))
            .is_some(),
        "hooks should still deserialize from config.toml"
    );
    assert_eq!(layers.requirements_toml().allow_managed_hooks_only, None);
    assert!(layers.requirements().allow_managed_hooks_only.is_none());
}

#[tokio::test]
async fn load_config_layers_includes_cloud_hook_requirements() {
    let tmp = tempdir().unwrap();
    let codex_home = tmp.path().join("home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let managed_dir = tmp.path().join("managed-hooks");
    std::fs::create_dir_all(&managed_dir).unwrap();
    let requirements = format!(
        "\n[hooks]\nmanaged_dir = '{0}'\n\n[[hooks.PreToolUse]]\nmatcher = \"^Bash$\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = 'python3 {0}/pre.py'\ntimeout = 10\nstatusMessage = \"checking\"\n",
        managed_dir.display()
    );
    let expected: ConfigRequirementsToml = toml::from_str(&requirements).unwrap();
    let options = ConfigLoadOptions {
        cloud_config_bundle:
            it_config::support::CloudConfigBundleFixture::loader_with_enterprise_requirement(
                requirements,
            ),
        ..plain_options()
    };
    let layers = load(&codex_home, tmp.path(), options).await;
    assert_eq!(layers.requirements_toml().hooks, expected.hooks);
    assert_eq!(
        layers
            .requirements()
            .managed_hooks
            .as_ref()
            .map(|hooks| hooks.source.clone()),
        Some(Some(RequirementSource::EnterpriseManaged {
            id: "req_1".to_string(),
            name: "Base requirements".to_string(),
        }))
    );
}

#[tokio::test]
async fn linked_worktree_project_layers_keep_worktree_config_but_use_root_repo_hooks() {
    let tmp = tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo_root = tmp_path.join("repo");
    let repo_child = repo_root.join("child");
    let worktree_root = tmp_path.join("worktree");
    let worktree_child = worktree_root.join("child");
    std::fs::create_dir_all(worktree_root.join(PROJECT_FOLDER)).unwrap();
    std::fs::create_dir_all(worktree_child.join(PROJECT_FOLDER)).unwrap();
    write_linked_worktree_pointer(&repo_root, &worktree_root);
    for (dir, foo, command) in [
        (&repo_root, "repo-root", "echo repo root hook"),
        (&repo_child, "repo-child", "echo repo child hook"),
        (&worktree_root, "worktree-root", "echo worktree root hook"),
        (
            &worktree_child,
            "worktree-child",
            "echo worktree child hook",
        ),
    ] {
        write_project_hook_config(&dir.join(PROJECT_FOLDER), Some(foo), command);
    }
    let codex_home = tmp_path.join("home");
    trusted_project_config(&codex_home, &repo_root, None);

    let stack = load(&codex_home, &worktree_child, plain_options()).await;
    let layers = project_layers(&stack);
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].hooks_config_folder(), folder(&repo_child));
    assert_eq!(layers[1].hooks_config_folder(), folder(&repo_root));
    assert_eq!(
        layers[0].config.get("foo").and_then(TomlValue::as_str),
        Some("worktree-child")
    );
    assert_eq!(
        project_hook_command(layers[0]),
        Some("echo repo child hook")
    );
    assert_eq!(
        layers[1].config.get("foo").and_then(TomlValue::as_str),
        Some("worktree-root")
    );
    assert_eq!(project_hook_command(layers[1]), Some("echo repo root hook"));
}

#[tokio::test]
async fn linked_worktree_project_layers_use_root_repo_hooks_without_worktree_config_toml() {
    let tmp = tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo_root = tmp_path.join("repo");
    let worktree_root = tmp_path.join("worktree");
    std::fs::create_dir_all(worktree_root.join(PROJECT_FOLDER)).unwrap();
    write_linked_worktree_pointer(&repo_root, &worktree_root);
    write_project_hook_config(&repo_root.join(PROJECT_FOLDER), None, "echo repo root hook");
    let codex_home = tmp_path.join("home");
    trusted_project_config(&codex_home, &repo_root, None);

    let stack = load(&codex_home, &worktree_root, plain_options()).await;
    let layers = project_layers(&stack);
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].hooks_config_folder(), folder(&repo_root));
    assert_eq!(project_hook_command(layers[0]), Some("echo repo root hook"));
}

#[tokio::test]
async fn nested_project_root_markers_do_not_redirect_regular_repo_hooks() {
    let tmp = tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo_root = tmp_path.join("repo");
    let project_root = repo_root.join("project");
    let nested = project_root.join("child");
    std::fs::create_dir_all(repo_root.join(".git")).unwrap();
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join(".hg"), "hg").unwrap();
    for (dir, command) in [
        (&repo_root, "echo repo root hook"),
        (&project_root, "echo project root hook"),
        (&nested, "echo nested hook"),
    ] {
        write_project_hook_config(&dir.join(PROJECT_FOLDER), None, command);
    }
    let codex_home = tmp_path.join("home");
    trusted_project_config(&codex_home, &project_root, Some(&[".hg"]));

    let stack = load(&codex_home, &nested, plain_options()).await;
    let layers = project_layers(&stack);
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].hooks_config_folder(), folder(&nested));
    assert_eq!(layers[1].hooks_config_folder(), folder(&project_root));
    assert_eq!(project_hook_command(layers[0]), Some("echo nested hook"));
    assert_eq!(
        project_hook_command(layers[1]),
        Some("echo project root hook")
    );
}

/// An untrusted malformed worktree must not read or decode the repository's hook configuration.
#[tokio::test]
async fn malformed_untrusted_linked_worktree_does_not_read_root_hooks() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(worktree.join(PROJECT_FOLDER)).unwrap();
    std::fs::create_dir_all(repo.join(PROJECT_FOLDER)).unwrap();
    write_linked_worktree_pointer(&repo, &worktree);
    std::fs::write(
        worktree.join(PROJECT_FOLDER).join(CONFIG_TOML_FILE),
        "foo =",
    )
    .unwrap();
    std::fs::write(repo.join(PROJECT_FOLDER).join(CONFIG_TOML_FILE), [0xff]).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let key = TomlValue::String(project_trust_key(&repo)).to_string();
    std::fs::write(
        home.join(CONFIG_TOML_FILE),
        format!("[projects.{key}]\ntrust_level = 'untrusted'\n"),
    )
    .unwrap();
    let layers = load(&home, &worktree, plain_options()).await;
    let layers: Vec<_> = layers
        .all_layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    assert_eq!(layers.len(), 1);
    assert!(layers[0].disabled_reason.is_some());
    assert_eq!(layers[0].config, TomlValue::Table(Default::default()));
}
