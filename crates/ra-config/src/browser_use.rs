// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain the source configuration records and constraints."
)]
use crate::AllowDenyRequirementToml;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BrowserUseConfigToml {
    pub allow_history_access: Option<bool>,
    pub default_origin_policy: Option<BrowserUseOriginPolicyConfigToml>,
    pub origins: Option<BTreeMap<String, BrowserUseOriginPolicyConfigToml>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BrowserUseOriginPolicyConfigToml {
    pub access: Option<AllowDenyRequirementToml>,
    pub downloads: Option<AllowDenyRequirementToml>,
    pub uploads: Option<AllowDenyRequirementToml>,
    pub full_cdp_access: Option<AllowDenyRequirementToml>,
}
