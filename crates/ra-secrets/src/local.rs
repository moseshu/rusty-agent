// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The age-encrypted local secrets file and its keyring-held passphrase.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use age::decrypt;
use age::encrypt;
use age::scrypt::Identity as ScryptIdentity;
use age::scrypt::Recipient as ScryptRecipient;
use age::secrecy::ExposeSecret;
use age::secrecy::SecretString;
use anyhow::Context;
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ra_keyring_store::KeyringStore;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;
use zeroize::Zeroize;

use super::SecretListEntry;
use super::SecretName;
use super::SecretScope;
use super::SecretsBackend;
use super::compute_keyring_account;
use super::keyring_service;

const SECRETS_VERSION: u8 = 1;
const LOCAL_SECRETS_FILENAME: &str = "local.age";
const MCP_OAUTH_SECRETS_FILENAME: &str = "mcp_oauth.age";
const GATEWAY_OAUTH_SECRETS_FILENAME: &str = "gateway_oauth.age";
static MCP_OAUTH_CACHE: Mutex<Option<CachedSecrets>> = Mutex::new(None);

/// Selects the local encrypted file used by a `LocalSecretsBackend`.
///
/// Codex also has a namespace for its own account credentials. Account sign-in is not part of
/// this product, so that namespace and its file are not carried over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalSecretsNamespace {
    /// General managed secrets stored in `local.age`.
    #[default]
    ManagedSecrets,
    /// OAuth credentials for external MCP servers.
    McpOAuth,
    /// Gateway OAuth credentials, isolated from other secrets in file and encryption key.
    GatewayOAuth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SecretsFile {
    version: u8,
    secrets: BTreeMap<String, String>,
}

struct CachedSecrets {
    path: PathBuf,
    ciphertext_hash: [u8; 32],
    passphrase_hash: [u8; 32],
    file: Arc<SecretsFile>,
}

impl SecretsFile {
    fn new_empty() -> Self {
        Self {
            version: SECRETS_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

/// Secrets in one age-encrypted file per namespace, keyed by a passphrase held in the keyring.
#[derive(Clone)]
pub struct LocalSecretsBackend {
    home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    namespace: LocalSecretsNamespace,
    gateway_cache: Arc<Mutex<Option<CachedSecrets>>>,
}

impl std::fmt::Debug for LocalSecretsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSecretsBackend")
            .field("home", &self.home)
            .field("keyring_store", &self.keyring_store)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl LocalSecretsBackend {
    /// Creates a backend for the general managed-secrets namespace.
    #[must_use]
    pub fn new(home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self::new_with_namespace(home, keyring_store, LocalSecretsNamespace::ManagedSecrets)
    }

    /// Creates a backend for `namespace`.
    #[must_use]
    pub fn new_with_namespace(
        home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: LocalSecretsNamespace,
    ) -> Self {
        Self {
            home,
            keyring_store,
            namespace,
            gateway_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Stores a nonempty `value`, replacing any existing value.
    pub fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        anyhow::ensure!(!value.is_empty(), "secret value must not be empty");
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        file.secrets.insert(canonical_key, value.to_string());
        self.save_file(&file)
    }

    /// Reads a stored value.
    pub fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        let canonical_key = scope.canonical_key(name);
        let file = self.load_file()?;
        Ok(file.secrets.get(&canonical_key).cloned())
    }

    /// Removes a value, returning whether one existed.
    pub fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        let removed = file.secrets.remove(&canonical_key).is_some();
        if removed {
            self.save_file(&file)?;
        }
        Ok(removed)
    }

    /// Lists stored secrets, optionally restricted to one scope. Malformed keys are skipped.
    pub fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        let file = self.load_file()?;
        let mut entries = Vec::new();
        for canonical_key in file.secrets.keys() {
            let Some(entry) = parse_canonical_key(canonical_key) else {
                warn!("skipping invalid canonical secret key: {canonical_key}");
                continue;
            };
            if let Some(scope) = scope_filter
                && entry.scope != *scope
            {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    fn secrets_dir(&self) -> PathBuf {
        self.home.join("secrets")
    }

    fn secrets_path(&self) -> PathBuf {
        let filename = match self.namespace {
            LocalSecretsNamespace::ManagedSecrets => LOCAL_SECRETS_FILENAME,
            LocalSecretsNamespace::McpOAuth => MCP_OAUTH_SECRETS_FILENAME,
            LocalSecretsNamespace::GatewayOAuth => GATEWAY_OAUTH_SECRETS_FILENAME,
        };
        self.secrets_dir().join(filename)
    }

    /// Gateway readers share one bounded cache per backend; independent managers
    /// still reread the ciphertext and keyring before reusing decrypted credentials.
    fn decrypted_cache(&self) -> Option<&Mutex<Option<CachedSecrets>>> {
        match self.namespace {
            LocalSecretsNamespace::McpOAuth => Some(&MCP_OAUTH_CACHE),
            LocalSecretsNamespace::GatewayOAuth => Some(&self.gateway_cache),
            LocalSecretsNamespace::ManagedSecrets => None,
        }
    }

    fn load_file(&self) -> Result<SecretsFile> {
        let path = self.secrets_path();
        if !path.exists() {
            return Ok(SecretsFile::new_empty());
        }

        let ciphertext = fs::read(&path)
            .with_context(|| format!("failed to read secrets file at {}", path.display()))?;
        let passphrase = self.load_or_create_passphrase()?;
        let cache = self.decrypted_cache().map(|cache| {
            let ciphertext_hash: [u8; 32] = Sha256::digest(&ciphertext).into();
            let passphrase_hash: [u8; 32] =
                Sha256::digest(passphrase.expose_secret().as_bytes()).into();
            let cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
            (cache, ciphertext_hash, passphrase_hash)
        });
        if let Some((cache, ciphertext_hash, passphrase_hash)) = cache.as_ref()
            && let Some(cached) = cache.as_ref()
            && cached.path == path
            && cached.ciphertext_hash == *ciphertext_hash
            && cached.passphrase_hash == *passphrase_hash
        {
            return Ok(cached.file.as_ref().clone());
        }
        let plaintext = decrypt_with_passphrase(&ciphertext, &passphrase)?;
        let mut parsed: SecretsFile = serde_json::from_slice(&plaintext).with_context(|| {
            format!(
                "failed to deserialize decrypted secrets file at {}",
                path.display()
            )
        })?;
        if parsed.version == 0 {
            parsed.version = SECRETS_VERSION;
        }
        anyhow::ensure!(
            parsed.version <= SECRETS_VERSION,
            "secrets file version {} is newer than supported version {}",
            parsed.version,
            SECRETS_VERSION
        );
        if let Some((mut cache, ciphertext_hash, passphrase_hash)) = cache {
            *cache = Some(CachedSecrets {
                path,
                ciphertext_hash,
                passphrase_hash,
                file: Arc::new(parsed.clone()),
            });
        }
        Ok(parsed)
    }

    fn save_file(&self, file: &SecretsFile) -> Result<()> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;

        let passphrase = self.load_or_create_passphrase()?;
        let plaintext = serde_json::to_vec(file).context("failed to serialize secrets file")?;
        let ciphertext = encrypt_with_passphrase(&plaintext, &passphrase)?;
        let path = self.secrets_path();
        write_file_atomically(&path, &ciphertext)?;
        if let Some(cache) = self.decrypted_cache() {
            let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.as_ref().is_some_and(|cached| cached.path == path) {
                *cache = None;
            }
        }
        Ok(())
    }

    fn load_or_create_passphrase(&self) -> Result<SecretString> {
        let account = compute_keyring_account(&self.home, self.namespace);
        let loaded = self
            .keyring_store
            .load(keyring_service(), &account)
            .map_err(std::io::Error::from)
            .with_context(|| format!("failed to load secrets key from keyring for {account}"))?;
        if let Some(existing) = loaded {
            Ok(SecretString::from(existing))
        } else {
            // Generate a high-entropy key and persist it in the OS keyring. This keeps secrets
            // out of plaintext config while remaining fully local and offline.
            let generated = generate_passphrase()?;
            self.keyring_store
                .save(keyring_service(), &account, generated.expose_secret())
                .map_err(std::io::Error::from)
                .context("failed to persist secrets key in keyring")?;
            Ok(generated)
        }
    }
}

impl SecretsBackend for LocalSecretsBackend {
    fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        LocalSecretsBackend::set(self, scope, name, value)
    }

    fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        LocalSecretsBackend::get(self, scope, name)
    }

    fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        LocalSecretsBackend::delete(self, scope, name)
    }

    fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        LocalSecretsBackend::list(self, scope_filter)
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().with_context(|| {
        format!(
            "failed to compute parent directory for secrets file at {}",
            path.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let filename = path.file_name().with_context(|| {
        format!(
            "failed to compute filename for secrets file at {}",
            path.display()
        )
    })?;
    let tmp_path = dir.join(format!(
        ".{}.tmp-{}-{nonce}",
        filename.to_string_lossy(),
        std::process::id()
    ));

    {
        let mut tmp_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "failed to create temp secrets file at {}",
                    tmp_path.display()
                )
            })?;
        tmp_file.write_all(contents).with_context(|| {
            format!(
                "failed to write temp secrets file at {}",
                tmp_path.display()
            )
        })?;
        tmp_file.sync_all().with_context(|| {
            format!("failed to sync temp secrets file at {}", tmp_path.display())
        })?;
    }

    match fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            #[cfg(target_os = "windows")]
            {
                if path.exists() {
                    fs::remove_file(path).with_context(|| {
                        format!(
                            "failed to remove existing secrets file at {} before replace",
                            path.display()
                        )
                    })?;
                    fs::rename(&tmp_path, path).with_context(|| {
                        format!(
                            "failed to replace secrets file at {} with {}",
                            path.display(),
                            tmp_path.display()
                        )
                    })?;
                    return Ok(());
                }
            }

            let _ = fs::remove_file(&tmp_path);
            Err(initial_error).with_context(|| {
                format!(
                    "failed to atomically replace secrets file at {} with {}",
                    path.display(),
                    tmp_path.display()
                )
            })
        }
    }
}

fn generate_passphrase() -> Result<SecretString> {
    let mut bytes = [0_u8; 32];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("failed to generate random secrets key")?;
    // Base64 keeps the keyring payload ASCII-safe without reducing entropy.
    let encoded = BASE64_STANDARD.encode(bytes);
    // Codex wipes the raw key with volatile writes and a compiler fence; `zeroize` performs the
    // same non-elidable wipe without the crate opting into unsafe code.
    bytes.zeroize();
    Ok(SecretString::from(encoded))
}

fn encrypt_with_passphrase(plaintext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let recipient = ScryptRecipient::new(passphrase.clone());
    encrypt(&recipient, plaintext).context("failed to encrypt secrets file")
}

fn decrypt_with_passphrase(ciphertext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let identity = ScryptIdentity::new(passphrase.clone());
    decrypt(&identity, ciphertext).context("failed to decrypt secrets file")
}

fn parse_canonical_key(canonical_key: &str) -> Option<SecretListEntry> {
    let mut parts = canonical_key.split('/');
    let scope_kind = parts.next()?;
    match scope_kind {
        "global" => {
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            Some(SecretListEntry {
                scope: SecretScope::Global,
                name,
            })
        }
        "env" => {
            let environment_id = parts.next()?;
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            let scope = SecretScope::environment(environment_id.to_string()).ok()?;
            Some(SecretListEntry { scope, name })
        }
        _ => None,
    }
}

/// Internals the separate test workspace inspects; not part of the supported surface.
#[cfg(feature = "test-api")]
pub mod test_api {
    use super::{
        Arc, CachedSecrets, LocalSecretsBackend, MCP_OAUTH_CACHE, Mutex, Path, PathBuf,
        PoisonError, Result, SecretsFile,
    };
    use std::collections::BTreeMap;
    use std::sync::MutexGuard;

    /// The newest secrets file version this build reads.
    pub const SECRETS_VERSION: u8 = super::SECRETS_VERSION;
    /// The managed-secrets file name.
    pub const LOCAL_SECRETS_FILENAME: &str = super::LOCAL_SECRETS_FILENAME;

    /// The keyring service under which namespace passphrases are stored.
    #[must_use]
    pub fn keyring_service() -> &'static str {
        crate::keyring_service()
    }

    static MCP_OAUTH_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes tests that observe the process-wide MCP OAuth cache.
    pub fn mcp_oauth_cache_test_lock() -> MutexGuard<'static, ()> {
        MCP_OAUTH_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes an empty secrets file that declares `version`.
    pub fn save_empty_file_with_version(backend: &LocalSecretsBackend, version: u8) -> Result<()> {
        backend.save_file(&SecretsFile {
            version,
            secrets: BTreeMap::new(),
        })
    }

    /// Decrypts and validates the backend's file, discarding its contents.
    pub fn load_file(backend: &LocalSecretsBackend) -> Result<()> {
        backend.load_file().map(|_| ())
    }

    /// The directory holding the backend's encrypted files.
    #[must_use]
    pub fn secrets_dir(backend: &LocalSecretsBackend) -> PathBuf {
        backend.secrets_dir()
    }

    /// The backend's encrypted file.
    #[must_use]
    pub fn secrets_path(backend: &LocalSecretsBackend) -> PathBuf {
        backend.secrets_path()
    }

    /// An opaque handle on a decrypted file held by a cache.
    pub struct CachedFile(Arc<SecretsFile>);

    impl CachedFile {
        /// Whether both handles refer to the same decrypted allocation.
        #[must_use]
        pub fn ptr_eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.0, &other.0)
        }
    }

    fn cached(cache: &Mutex<Option<CachedSecrets>>, path: &Path) -> Option<CachedFile> {
        cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|cached| cached.path == path)
            .map(|cached| CachedFile(Arc::clone(&cached.file)))
    }

    /// The process-wide MCP OAuth cache entry for `path`, if it holds one.
    #[must_use]
    pub fn mcp_oauth_cached(path: &Path) -> Option<CachedFile> {
        cached(&MCP_OAUTH_CACHE, path)
    }

    /// The backend's own gateway cache entry for its file, if it holds one.
    #[must_use]
    pub fn gateway_cached(backend: &LocalSecretsBackend) -> Option<CachedFile> {
        cached(&backend.gateway_cache, &backend.secrets_path())
    }
}
