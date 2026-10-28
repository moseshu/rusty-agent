// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain the source configuration records and constraints."
)]
use serde::Deserialize;

/// Managed requirements for the interactive in-app browser, not agent Browser Use.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct InAppBrowserRequirementsToml {
    /// Whether external browser settings may be imported. Only `Some(false)`
    /// denies import; omission is effectively `true`. Keep omission unset while
    /// composing managed layers so it cannot replace an explicit requirement.
    pub allow_external_browser_settings_import: Option<bool>,
}
