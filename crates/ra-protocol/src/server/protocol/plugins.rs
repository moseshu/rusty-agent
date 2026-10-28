// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Plugin policy and product labels from the upstream coding-product protocol.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[non_exhaustive]
/// Declared installation availability, independent of installed or enabled state.
pub enum PluginInstallPolicy {
    #[serde(rename = "NOT_AVAILABLE")]
    /// The package is unavailable for installation.
    NotAvailable,
    #[serde(rename = "AVAILABLE")]
    /// The package may be selected for installation.
    Available,
    #[serde(rename = "INSTALLED_BY_DEFAULT")]
    /// The package is declared installed by default by its source policy.
    InstalledByDefault,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[non_exhaustive]
/// Declared authentication timing; this metadata performs no authentication.
pub enum PluginAuthPolicy {
    #[serde(rename = "ON_INSTALL")]
    /// Authentication is declared at installation time.
    OnInstall,
    #[serde(rename = "ON_USE")]
    /// Authentication is declared when the package is used.
    OnUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
/// Upstream product restriction labels shared by plugin and skill metadata.
pub enum Product {
    #[serde(alias = "CHATGPT")]
    /// The `ChatGPT` metadata label.
    Chatgpt,
    #[serde(alias = "CODEX")]
    /// The coding-product metadata label.
    Codex,
    #[serde(alias = "ATLAS")]
    /// The Atlas metadata label.
    Atlas,
}
impl Product {
    /// Returns the upstream app-platform label without accessing an app service.
    #[must_use]
    pub fn to_app_platform(self) -> &'static str {
        match self {
            Self::Chatgpt => "chat",
            Self::Codex => "codex",
            Self::Atlas => "atlas",
        }
    }

    /// Recognizes a trimmed, case-insensitive product source name.
    #[must_use]
    pub fn from_session_source_name(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "chatgpt" => Some(Self::Chatgpt),
            "codex" => Some(Self::Codex),
            "atlas" => Some(Self::Atlas),
            _ => None,
        }
    }

    /// Matches a product list; the shared contract treats an empty list as unrestricted.
    #[must_use]
    pub fn matches_product_restriction(&self, products: &[Product]) -> bool {
        products.is_empty() || products.contains(self)
    }
}
