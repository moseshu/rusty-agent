//! Open tool namespaces used for collision-free identity.

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::error::{Error, Result};

/// An application-defined namespace such as `mcp.github` or `plugin.issue_tracker`.
///
/// This is an open newtype rather than a closed enum: third-party integrations must be able to
/// introduce namespaces without changing `ra-core`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolNamespace(String);

impl ToolNamespace {
    /// Creates a non-empty namespace.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_namespace(&value)?;
        Ok(Self(value))
    }

    /// Stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolNamespace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub(super) fn validate_namespace(value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(Error::caller(
            "tool namespace must be non-empty, trimmed, and contain no control characters",
        ));
    }
    Ok(())
}
