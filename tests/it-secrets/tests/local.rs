// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The encrypted local backend: versioning, keyring failures, atomic writes, namespace isolation,
//! and the decrypted-plaintext caches.
//!
//! Ported from the unit tests in Codex `codex-rs/secrets/src/{lib,local}.rs`. Codex isolates its
//! account-credential namespace from the gateway namespace; this product has no account
//! namespace, so the isolation case uses the managed-secrets namespace, which shares the same
//! home-wide key.

use std::fs;
use std::sync::Arc;

use anyhow::{Context, Result};
use keyring::Error as KeyringError;
use ra_keyring_store::KeyringStore;
use ra_keyring_store::tests::MockKeyringStore;
use ra_secrets::test_api;
use ra_secrets::{
    LocalSecretsBackend, LocalSecretsNamespace, SecretName, SecretScope, SecretsBackendKind,
    SecretsManager, compute_keyring_account, environment_id_from_cwd,
};
use sha2::{Digest, Sha256};

fn backend(
    home: &std::path::Path,
    keyring: &Arc<MockKeyringStore>,
    namespace: LocalSecretsNamespace,
) -> LocalSecretsBackend {
    LocalSecretsBackend::new_with_namespace(home.to_path_buf(), keyring.clone(), namespace)
}

#[test]
fn environment_id_fallback_has_cwd_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env_id = environment_id_from_cwd(dir.path());
    let canonical = dir
        .path()
        .canonicalize()
        .expect("tempdir canonical path should exist")
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..12).expect("digest has at least 12 chars");
    assert_eq!(env_id, format!("cwd-{short}"));
}

#[test]
fn environment_id_uses_the_enclosing_repository_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("my-repo");
    fs::create_dir_all(repo.join(".git")).expect("git dir");
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let nested = repo.join("src/inner");
    fs::create_dir_all(&nested).expect("nested dir");
    assert_eq!(environment_id_from_cwd(&nested), "my-repo");
}

#[test]
fn manager_round_trips_local_backend() -> Result<()> {
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let manager = SecretsManager::new_with_keyring_store(
        home.path().to_path_buf(),
        SecretsBackendKind::Local,
        keyring,
    );
    let scope = SecretScope::Global;
    let name = SecretName::new("GITHUB_TOKEN")?;

    manager.set(&scope, &name, "token-1")?;
    assert_eq!(manager.get(&scope, &name)?, Some("token-1".to_string()));

    let listed = manager.list(None)?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, name);

    assert!(manager.delete(&scope, &name)?);
    assert_eq!(manager.get(&scope, &name)?, None);
    Ok(())
}

#[test]
fn load_file_rejects_newer_schema_versions() -> Result<()> {
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring);

    test_api::save_empty_file_with_version(&backend, test_api::SECRETS_VERSION + 1)?;

    let error = test_api::load_file(&backend).expect_err("must reject newer schema version");
    assert!(
        error.to_string().contains("newer than supported version"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[test]
fn set_fails_when_keyring_is_unavailable() -> Result<()> {
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let account = compute_keyring_account(home.path(), LocalSecretsNamespace::ManagedSecrets);
    keyring.set_error(
        &account,
        KeyringError::Invalid("error".into(), "load".into()),
    );

    let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring);
    let scope = SecretScope::Global;
    let name = SecretName::new("TEST_SECRET")?;
    let error = backend
        .set(&scope, &name, "secret-value")
        .expect_err("must fail when keyring load fails");
    assert!(
        error
            .to_string()
            .contains("failed to load secrets key from keyring"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[test]
fn save_file_does_not_leave_temp_files() -> Result<()> {
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring);

    let scope = SecretScope::Global;
    let name = SecretName::new("TEST_SECRET")?;
    backend.set(&scope, &name, "one")?;
    backend.set(&scope, &name, "two")?;

    let secrets_dir = test_api::secrets_dir(&backend);
    let entries = fs::read_dir(&secrets_dir)
        .with_context(|| format!("failed to read {}", secrets_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to enumerate {}", secrets_dir.display()))?;

    let filenames: Vec<String> = entries
        .into_iter()
        .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
        .collect();
    assert_eq!(
        filenames,
        vec![test_api::LOCAL_SECRETS_FILENAME.to_string()]
    );
    assert_eq!(backend.get(&scope, &name)?, Some("two".to_string()));
    assert!(test_api::mcp_oauth_cached(&test_api::secrets_path(&backend)).is_none());
    Ok(())
}

#[test]
fn local_namespaces_write_separate_files() -> Result<()> {
    let _cache_lock = test_api::mcp_oauth_cache_test_lock();
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let managed_backend = backend(home.path(), &keyring, LocalSecretsNamespace::ManagedSecrets);
    let mcp_backend = backend(home.path(), &keyring, LocalSecretsNamespace::McpOAuth);
    let gateway_backend = backend(home.path(), &keyring, LocalSecretsNamespace::GatewayOAuth);
    let scope = SecretScope::Global;
    let name = SecretName::new("TEST_SECRET")?;

    managed_backend.set(&scope, &name, "managed-value")?;
    mcp_backend.set(&scope, &name, "mcp-value")?;
    gateway_backend.set(&scope, &name, "gateway-value")?;

    assert_eq!(
        managed_backend.get(&scope, &name)?,
        Some("managed-value".to_string())
    );
    assert!(test_api::mcp_oauth_cached(&test_api::secrets_path(&managed_backend)).is_none());
    assert_eq!(
        mcp_backend.get(&scope, &name)?,
        Some("mcp-value".to_string())
    );
    let secrets = home.path().join("secrets");
    assert!(secrets.join("local.age").is_file());
    assert!(secrets.join("mcp_oauth.age").is_file());
    assert!(secrets.join("gateway_oauth.age").is_file());
    // Removing the home-wide key must not make the independent gateway file unreadable.
    keyring
        .delete(
            test_api::keyring_service(),
            &compute_keyring_account(home.path(), LocalSecretsNamespace::ManagedSecrets),
        )
        .expect("remove managed secrets key");
    assert_eq!(
        gateway_backend.get(&scope, &name)?,
        Some("gateway-value".to_string())
    );
    Ok(())
}

#[test]
fn mcp_oauth_cache_reuses_plaintext_and_invalidates_when_ciphertext_changes() -> Result<()> {
    let _cache_lock = test_api::mcp_oauth_cache_test_lock();
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let first = backend(home.path(), &keyring, LocalSecretsNamespace::McpOAuth);
    let second = backend(home.path(), &keyring, LocalSecretsNamespace::McpOAuth);
    let path = test_api::secrets_path(&first);
    let scope = SecretScope::Global;
    let name = SecretName::new("TEST_SECRET")?;
    let cached_file =
        || test_api::mcp_oauth_cached(&path).expect("MCP OAuth credentials should be cached");

    first.set(&scope, &name, "one")?;
    let (first_cached, second_cached) = std::thread::scope(|threads| {
        let first_reader = threads.spawn(|| {
            assert_eq!(first.get(&scope, &name)?, Some("one".to_string()));
            Ok::<_, anyhow::Error>(cached_file())
        });
        let second_reader = threads.spawn(|| {
            assert_eq!(second.get(&scope, &name)?, Some("one".to_string()));
            Ok::<_, anyhow::Error>(cached_file())
        });
        Ok::<_, anyhow::Error>((
            first_reader.join().expect("first credential reader")?,
            second_reader.join().expect("second credential reader")?,
        ))
    })?;
    assert!(first_cached.ptr_eq(&second_cached));

    assert_eq!(second.get(&scope, &name)?, Some("one".to_string()));
    assert!(first_cached.ptr_eq(&cached_file()));

    first.set(&scope, &name, "two")?;
    assert!(test_api::mcp_oauth_cached(&path).is_none());
    assert_eq!(second.get(&scope, &name)?, Some("two".to_string()));
    assert!(!first_cached.ptr_eq(&cached_file()));

    assert!(first.delete(&scope, &name)?);
    assert!(test_api::mcp_oauth_cached(&path).is_none());
    assert_eq!(second.get(&scope, &name)?, None);

    Ok(())
}

#[test]
fn gateway_cache_is_per_backend_and_invalidated_by_writes() -> Result<()> {
    let home = tempfile::tempdir().expect("tempdir");
    let keyring = Arc::new(MockKeyringStore::default());
    let first = backend(home.path(), &keyring, LocalSecretsNamespace::GatewayOAuth);
    let second = backend(home.path(), &keyring, LocalSecretsNamespace::GatewayOAuth);
    let scope = SecretScope::Global;
    let name = SecretName::new("PROVIDER_OAUTH_ABC")?;

    first.set(&scope, &name, "one")?;
    assert!(test_api::gateway_cached(&first).is_none());
    assert_eq!(first.get(&scope, &name)?, Some("one".to_string()));
    let cached = test_api::gateway_cached(&first).expect("first backend caches its plaintext");
    assert!(test_api::gateway_cached(&second).is_none());

    // Another backend's write changes the ciphertext, so the first backend decrypts again.
    second.set(&scope, &name, "two")?;
    assert_eq!(first.get(&scope, &name)?, Some("two".to_string()));
    let refreshed = test_api::gateway_cached(&first).expect("first backend recaches");
    assert!(!cached.ptr_eq(&refreshed));
    assert!(test_api::mcp_oauth_cached(&test_api::secrets_path(&first)).is_none());
    Ok(())
}
