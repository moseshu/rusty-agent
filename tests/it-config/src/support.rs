// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Test-only helpers exposed for cross-crate integration tests.
//!
//! Production code should not depend on this module.

use ra_config::CloudConfigBundle;
use ra_config::CloudConfigBundleLoader;
use ra_config::CloudConfigFragment;
use ra_config::CloudRequirementsFragment;

#[derive(Debug, Clone, Default)]
pub struct CloudConfigBundleFixture {
    bundle: CloudConfigBundle,
}

impl CloudConfigBundleFixture {
    pub fn enterprise_requirement(contents: impl Into<String>) -> Self {
        Self::default().add_enterprise_requirement(contents)
    }

    pub fn enterprise_config(contents: impl Into<String>) -> Self {
        Self::default().add_enterprise_config(contents)
    }

    pub fn loader_with_enterprise_requirement(
        contents: impl Into<String>,
    ) -> CloudConfigBundleLoader {
        Self::enterprise_requirement(contents).into_loader()
    }

    pub fn loader_with_enterprise_config(contents: impl Into<String>) -> CloudConfigBundleLoader {
        Self::enterprise_config(contents).into_loader()
    }

    pub fn add_enterprise_requirement(mut self, contents: impl Into<String>) -> Self {
        let index = self.bundle.requirements_toml.enterprise_managed.len() + 1;
        self.bundle
            .requirements_toml
            .enterprise_managed
            .push(CloudRequirementsFragment {
                id: format!("req_{index}"),
                name: if index == 1 {
                    "Base requirements".to_string()
                } else {
                    format!("Requirements {index}")
                },
                contents: contents.into(),
            });
        self
    }

    pub fn add_enterprise_config(mut self, contents: impl Into<String>) -> Self {
        let index = self.bundle.config_toml.enterprise_managed.len() + 1;
        self.bundle
            .config_toml
            .enterprise_managed
            .push(CloudConfigFragment {
                id: format!("cfg_{index}"),
                name: if index == 1 {
                    "Base config".to_string()
                } else {
                    format!("Config {index}")
                },
                contents: contents.into(),
            });
        self
    }

    pub fn into_bundle(self) -> CloudConfigBundle {
        self.bundle
    }

    pub fn into_loader(self) -> CloudConfigBundleLoader {
        let bundle = self.into_bundle();
        CloudConfigBundleLoader::new(async move { Ok(Some(bundle)) })
    }
}
