// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Local secrets encrypted with age, keyed by a passphrase kept in the OS keyring.
//!
//! [`SecretsManager`] selects a backend; the only backend is [`LocalSecretsBackend`], which keeps
//! one encrypted file per [`LocalSecretsNamespace`] under `<home>/secrets/`. Each namespace's
//! passphrase is generated on first write and stored through a [`KeyringStore`], so secrets
//! never reach plaintext configuration and stay fully local.
//!
//! **Stability**: `Internal`. Consumed by gateway OAuth credential storage in this workspace.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use ra_keyring_store::DefaultKeyringStore;
use ra_keyring_store::KeyringStore;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

mod git;
mod local;
mod sanitizer;

pub use local::LocalSecretsBackend;
pub use local::LocalSecretsNamespace;
pub use sanitizer::redact_secrets;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use local::test_api;

// Codex stores its passphrases under the "codex" service. This product uses its own service so
// its keyring entries are never mistaken for, or overwrite, a Codex installation's entries.
const KEYRING_SERVICE: &str = "rusty-agent";

/// A validated secret name: uppercase ASCII letters, digits, and underscores.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

impl SecretName {
    /// Validates a secret name after trimming surrounding whitespace.
    pub fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        anyhow::ensure!(!trimmed.is_empty(), "secret name must not be empty");
        anyhow::ensure!(
            trimmed
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'),
            "secret name must contain only A-Z, 0-9, or _"
        );
        Ok(Self(trimmed.to_string()))
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a secret applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretScope {
    /// Available everywhere.
    Global,
    /// Available only in the named environment.
    Environment(String),
}

impl SecretScope {
    /// Creates an environment scope after trimming surrounding whitespace.
    pub fn environment(environment_id: impl Into<String>) -> Result<Self> {
        let env_id = environment_id.into();
        let trimmed = env_id.trim();
        anyhow::ensure!(!trimmed.is_empty(), "environment id must not be empty");
        Ok(Self::Environment(trimmed.to_string()))
    }

    /// Returns the stable, env-safe identifier used as the on-disk map key.
    #[must_use]
    pub fn canonical_key(&self, name: &SecretName) -> String {
        match self {
            Self::Global => format!("global/{}", name.as_str()),
            Self::Environment(environment_id) => {
                format!("env/{environment_id}/{}", name.as_str())
            }
        }
    }
}

/// A stored secret's scope and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretListEntry {
    /// The scope the secret was stored under.
    pub scope: SecretScope,
    /// The secret's name.
    pub name: SecretName,
}

/// The secrets backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecretsBackendKind {
    /// The age-encrypted local file backend.
    #[default]
    Local,
}

/// Storage for named secrets.
pub trait SecretsBackend: Send + Sync {
    /// Stores `value`, replacing any existing value.
    fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()>;
    /// Reads a stored value.
    fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>>;
    /// Removes a value, returning whether one existed.
    fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool>;
    /// Lists stored secrets, optionally restricted to one scope.
    fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>>;
}

/// Reads and writes secrets through the selected backend.
#[derive(Clone)]
pub struct SecretsManager {
    backend: Arc<dyn SecretsBackend>,
}

impl SecretsManager {
    /// Creates a manager whose local backend keeps its key in the platform keyring.
    #[must_use]
    pub fn new(home: PathBuf, backend_kind: SecretsBackendKind) -> Self {
        let backend: Arc<dyn SecretsBackend> = match backend_kind {
            SecretsBackendKind::Local => {
                let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
                Arc::new(LocalSecretsBackend::new(home, keyring_store))
            }
        };
        Self { backend }
    }

    /// Creates a manager whose local backend keeps its key in `keyring_store`.
    #[must_use]
    pub fn new_with_keyring_store(
        home: PathBuf,
        backend_kind: SecretsBackendKind,
        keyring_store: Arc<dyn KeyringStore>,
    ) -> Self {
        let backend: Arc<dyn SecretsBackend> = match backend_kind {
            SecretsBackendKind::Local => Arc::new(LocalSecretsBackend::new(home, keyring_store)),
        };
        Self { backend }
    }

    /// Creates a manager for one local namespace whose key lives in `keyring_store`.
    #[must_use]
    pub fn new_with_keyring_store_and_namespace(
        home: PathBuf,
        backend_kind: SecretsBackendKind,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: LocalSecretsNamespace,
    ) -> Self {
        let backend: Arc<dyn SecretsBackend> = match backend_kind {
            SecretsBackendKind::Local => Arc::new(LocalSecretsBackend::new_with_namespace(
                home,
                keyring_store,
                namespace,
            )),
        };
        Self { backend }
    }

    /// Stores `value`, replacing any existing value.
    pub fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        self.backend.set(scope, name, value)
    }

    /// Reads a stored value.
    pub fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        self.backend.get(scope, name)
    }

    /// Removes a value, returning whether one existed.
    pub fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        self.backend.delete(scope, name)
    }

    /// Lists stored secrets, optionally restricted to one scope.
    pub fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        self.backend.list(scope_filter)
    }
}

/// Names the environment for `cwd`: the enclosing Git repository's directory name, or a short
/// hash of the canonical path when there is no repository.
#[must_use]
pub fn environment_id_from_cwd(cwd: &Path) -> String {
    if let Some(repo_root) = git::get_git_repo_root(cwd)
        && let Some(name) = repo_root.file_name()
    {
        let name = name.to_string_lossy().trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }

    let canonical = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..12).unwrap_or(hex.as_str());
    format!("cwd-{short}")
}

/// Computes the OS keyring account name used to store a local namespace's passphrase.
/// Existing namespaces retain their shared key; gateway credentials use an independent key.
#[must_use]
pub fn compute_keyring_account(home: &Path, namespace: LocalSecretsNamespace) -> String {
    let canonical = home
        .canonicalize()
        .unwrap_or_else(|_| home.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..16).unwrap_or(hex.as_str());
    let home_account = format!("secrets|{short}");
    // Separate keys also prevent concurrent first writes to the gateway and primary
    // stores from overwriting each other's newly generated encryption key.
    match namespace {
        LocalSecretsNamespace::GatewayOAuth => format!("{home_account}|gateway-oauth"),
        LocalSecretsNamespace::ManagedSecrets | LocalSecretsNamespace::McpOAuth => home_account,
    }
}

pub(crate) fn keyring_service() -> &'static str {
    KEYRING_SERVICE
}
