// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Persists gateway credentials and serializes exchanges across all configurations in one store.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ra_keyring_store::KeyringStore;
use ra_secrets::LocalSecretsNamespace;
use ra_secrets::SecretName;
use ra_secrets::SecretScope;
use ra_secrets::SecretsBackendKind;
use ra_secrets::SecretsManager;

pub(super) async fn lock_credentials(home: &Path) -> io::Result<File> {
    let directory = home.join("secrets");
    std::fs::create_dir_all(&directory)?;
    // Configurations have separate credential entries but rewrite the same encrypted file.
    // Keep one stable sidecar locked from the initial read through token exchange and save.
    let path = directory.join("gateway_oauth.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    tokio::time::timeout(Duration::from_mins(/*mins*/ 1), async {
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(()),
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
                }
                Err(error) => return Err(io::Error::from(error)),
            }
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for provider OAuth credentials",
        )
    })??;
    Ok(file)
}

pub(super) struct GatewayAuthStorage(SecretsManager);

impl GatewayAuthStorage {
    pub(super) fn new(home: PathBuf, keyring: Arc<dyn KeyringStore>) -> Self {
        Self(SecretsManager::new_with_keyring_store_and_namespace(
            home,
            SecretsBackendKind::Local,
            keyring,
            LocalSecretsNamespace::GatewayOAuth,
        ))
    }

    pub(super) fn load(&self, credential_id: &str) -> io::Result<Option<String>> {
        self.0
            .get(&SecretScope::Global, &secret_name(credential_id)?)
            .map_err(|_| io::Error::other("failed to load provider OAuth credentials"))
    }

    pub(super) fn save(&self, credential_id: &str, value: &str) -> io::Result<()> {
        self.0
            .set(&SecretScope::Global, &secret_name(credential_id)?, value)
            .map_err(|_| io::Error::other("failed to save provider OAuth credentials"))
    }
}

fn secret_name(credential_id: &str) -> io::Result<SecretName> {
    let digest = credential_id
        .strip_prefix("provider-oauth|")
        .ok_or_else(|| io::Error::other("invalid provider OAuth credential account"))?;
    SecretName::new(&format!("PROVIDER_OAUTH_{}", digest.to_ascii_uppercase()))
        .map_err(io::Error::other)
}
