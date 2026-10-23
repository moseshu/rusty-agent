// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Optional discovery policy from agents/openai.yaml. Presentation and dependency execution have
//! no consumer in the Host catalog adapter and are not installed by discovery.

use serde::Deserialize;

/// Upstream product labels used only when selecting skills with an explicit product restriction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SkillProduct {
    /// `ChatGPT` product skills.
    #[serde(alias = "CHATGPT")]
    Chatgpt,
    /// Coding product skills.
    #[serde(alias = "CODEX")]
    Codex,
    /// Atlas product skills.
    #[serde(alias = "ATLAS")]
    Atlas,
}

#[derive(Default, Deserialize)]
pub(super) struct SkillMetadataFile {
    #[serde(default)]
    pub(super) policy: Option<SkillPolicy>,
    // Validate optional metadata as upstream before using its policy. Discovery has no UI or
    // dependency-installation consumer, so these values are discarded after validation.
    #[serde(default, rename = "interface")]
    _interface: Option<SkillInterfaceFile>,
    #[serde(default, rename = "dependencies")]
    _dependencies: Option<Dependencies>,
}

#[derive(Deserialize)]
pub(super) struct SkillPolicy {
    #[serde(default)]
    pub(super) allow_implicit_invocation: Option<bool>,
    #[serde(default)]
    products: Vec<SkillProduct>,
}

impl SkillPolicy {
    pub(super) fn matches_product(&self, restriction_product: Option<SkillProduct>) -> bool {
        self.products.is_empty()
            || restriction_product.is_some_and(|product| self.products.contains(&product))
    }
}

#[derive(Deserialize)]
struct SkillInterfaceFile {
    #[serde(rename = "display_name")]
    _display_name: Option<String>,
    #[serde(rename = "short_description")]
    _short_description: Option<String>,
    #[serde(rename = "icon_small")]
    _icon_small: Option<String>,
    #[serde(rename = "icon_large")]
    _icon_large: Option<String>,
    #[serde(rename = "brand_color")]
    _brand_color: Option<String>,
    #[serde(rename = "default_prompt")]
    _default_prompt: Option<String>,
}
#[derive(Deserialize)]
struct Dependencies {
    #[serde(default, rename = "tools")]
    _tools: Vec<DependencyTool>,
}
#[derive(Deserialize)]
struct DependencyTool {
    #[serde(rename = "type")]
    _kind: Option<String>,
    #[serde(rename = "value")]
    _value: Option<String>,
    #[serde(rename = "description")]
    _description: Option<String>,
    #[serde(rename = "transport")]
    _transport: Option<String>,
    #[serde(rename = "command")]
    _command: Option<String>,
    #[serde(rename = "url")]
    _url: Option<String>,
    #[serde(rename = "oauth")]
    _oauth: Option<DependencyOAuth>,
}
#[derive(Deserialize)]
struct DependencyOAuth {
    #[serde(rename = "callbackPort", alias = "callback_port")]
    _callback_port: Option<u16>,
}
