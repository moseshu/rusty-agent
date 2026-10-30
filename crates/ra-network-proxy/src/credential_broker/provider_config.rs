// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
pub use ra_config::network_values::{CredentialAuthMethod, CredentialProviderConfig};

/// Upstream provider validation for the shared configuration record.
/// Rust requires a local extension trait because the record is owned by ra-config.
pub trait CredentialProviderConfigExt {
    /// Validate a complete provider definition using the broker's compilation rules.
    ///
    /// # Errors
    /// Returns the upstream compiler diagnostic for an invalid provider.
    fn validate(&self, id: &str) -> anyhow::Result<()>;
}

impl CredentialProviderConfigExt for CredentialProviderConfig {
    fn validate(&self, id: &str) -> anyhow::Result<()> {
        super::configured::ConfiguredCredentialProvider::compile(id, self)?;
        Ok(())
    }
}
