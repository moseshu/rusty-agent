// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bedrock credential state shared across independently configured threads in this process.

use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use ra_config::ModelProviderAwsAuthInfo;

use super::{auth_refresh::AwsAuthRecovery, credential_export::AwsCredentialExport};

#[derive(Default)]
pub(super) struct BedrockSharedState {
    aws_credential_exports: Mutex<Vec<(ModelProviderAwsAuthInfo, Weak<AwsCredentialExport>)>>,
    aws_auth_recoveries: Mutex<Vec<(ModelProviderAwsAuthInfo, Weak<AwsAuthRecovery>)>>,
}

pub(super) fn process_shared_state() -> &'static BedrockSharedState {
    static STATE: OnceLock<BedrockSharedState> = OnceLock::new();
    STATE.get_or_init(BedrockSharedState::default)
}

impl BedrockSharedState {
    pub(super) fn aws_credential_export(
        &self,
        aws: &ModelProviderAwsAuthInfo,
    ) -> Option<Arc<AwsCredentialExport>> {
        let config = aws.credential_export.as_ref()?;
        Some(shared(&self.aws_credential_exports, aws, || {
            AwsCredentialExport::new(config.clone())
        }))
    }

    pub(super) fn aws_auth_recovery(
        &self,
        aws: &ModelProviderAwsAuthInfo,
    ) -> Option<Arc<AwsAuthRecovery>> {
        let config = aws.auth_refresh.as_ref()?;
        Some(shared(&self.aws_auth_recoveries, aws, || {
            AwsAuthRecovery::new(config.clone())
        }))
    }
}

fn shared<T>(
    entries: &Mutex<Vec<(ModelProviderAwsAuthInfo, Weak<T>)>>,
    aws: &ModelProviderAwsAuthInfo,
    create: impl FnOnce() -> T,
) -> Arc<T> {
    let mut entries = entries.lock().unwrap_or_else(PoisonError::into_inner);
    entries.retain(|(_, entry)| entry.strong_count() != 0);
    if let Some(entry) = entries
        .iter()
        .find(|(cached, _)| cached == aws)
        .and_then(|(_, entry)| entry.upgrade())
    {
        return entry;
    }
    let entry = Arc::new(create());
    entries.push((aws.clone(), Arc::downgrade(&entry)));
    entry
}
