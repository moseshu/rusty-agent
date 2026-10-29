// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product metadata carveouts in canonical and legacy workspace policies.
use ra_filesystem::legacy::SandboxPolicy;
use ra_filesystem::permissions::{FileSystemSandboxPolicy, forbidden_agent_metadata_write};
use ra_paths::AbsolutePathBuf;
use tempfile::tempdir;

#[test]
fn workspace_profiles_and_legacy_policies_protect_both_product_config_names() {
    for metadata_exists in [false, true] {
        let workspace = tempdir().expect("workspace");
        let extra = tempdir().expect("extra writable root");
        for name in [".rusty-agent", ".codex"] {
            std::fs::create_dir(extra.path().join(name)).expect("extra metadata");
            if metadata_exists {
                std::fs::create_dir(workspace.path().join(name)).expect("workspace metadata");
            }
        }
        let extra_root =
            AbsolutePathBuf::from_absolute_path(extra.path()).expect("extra absolute path");
        let profile =
            FileSystemSandboxPolicy::workspace_write(std::slice::from_ref(&extra_root), true, true);
        let legacy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
            &SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![extra_root],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            },
            workspace.path(),
        );
        for policy in [profile, legacy] {
            for root in [workspace.path(), extra.path()] {
                assert!(
                    policy.can_write_local_path_with_cwd(&root.join("source.rs"), workspace.path())
                );
                for name in [".rusty-agent", ".codex"] {
                    for relative in [
                        name.to_string(),
                        format!("{name}/config.toml"),
                        format!("{name}/rules/allow.rules"),
                        format!("{name}/hooks.json"),
                    ] {
                        let path = root.join(relative);
                        assert!(
                            policy.can_read_local_path_with_cwd(&path, workspace.path()),
                            "{path:?}"
                        );
                        assert!(
                            !policy.can_write_local_path_with_cwd(&path, workspace.path()),
                            "{path:?}"
                        );
                        assert_eq!(
                            forbidden_agent_metadata_write(&path, workspace.path(), &policy),
                            Some(name)
                        );
                    }
                }
            }
        }
    }
}
