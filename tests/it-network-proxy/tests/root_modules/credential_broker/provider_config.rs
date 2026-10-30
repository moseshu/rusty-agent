#[cfg(test)]
mod tests {
    use super::{CredentialProviderConfig, CredentialProviderConfigExt};

    #[test]
    fn shared_provider_validation_preserves_compiler_errors() {
        let valid = CredentialProviderConfig {
            env: vec!["CUSTOM_TOKEN".to_string()],
            patterns: vec![r"custom_[a-z]{20}".to_string()],
            url_prefixes: vec!["https://provider.example/v1".to_string()],
            ..Default::default()
        };
        valid.validate("custom").unwrap();
        assert_eq!(
            valid.validate("").unwrap_err().to_string(),
            "credential provider name must not be empty"
        );
        let mut invalid = valid;
        invalid.env.clear();
        assert_eq!(
            invalid.validate("custom").unwrap_err().to_string(),
            "credential provider `custom` has no environment keys"
        );
    }
}
